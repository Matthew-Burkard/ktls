//! File diagnostics: syntax errors plus the semantic checks the core analysis supports.

use std::path::Path;

use super::resolve::{Resolved, TypeScope};
use super::source::Language;
use super::Snapshot;
use crate::symbols::{Modifiers, Origin};
use crate::syntax::ast::{self, AstNode};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: TextRange,
    pub severity: Severity,
    pub message: String,
    /// Kotlin compiler diagnostic id, e.g. `UNRESOLVED_REFERENCE`.
    pub code: &'static str,
}

/// Why semantic checks were skipped for a file (shown once, not per diagnostic).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Coverage {
    Full,
    SyntaxOnly(String),
}

pub fn file_diagnostics(snap: &Snapshot, path: &Path) -> (Vec<Diagnostic>, Coverage) {
    let Some(file) = snap.file(path) else { return (Vec::new(), Coverage::SyntaxOnly("unknown file".into())) };
    if file.language == Language::Java {
        return (Vec::new(), Coverage::SyntaxOnly("Java files are only indexed".into()));
    }
    let parse = file.parse();
    // One mistake often makes the parser report several errors in a row (e.g. a missing `)`
    // also yields "Expecting '}'"). Like the compiler's IDE mode, show the first per line.
    let mut out: Vec<Diagnostic> = Vec::new();
    let mut lines = std::collections::HashSet::new();
    let mut errors: Vec<_> = parse.errors().iter().collect();
    errors.sort_by_key(|e| e.range.start());
    for e in errors {
        let line = file.text[..usize::from(e.range.start()).min(file.text.len())].matches('\n').count();
        if !lines.insert(line) {
            continue;
        }
        out.push(Diagnostic { range: e.range, severity: Severity::Error, message: e.message.clone(), code: "SYNTAX" });
    }

    let ctx = snap.context_of(path);
    let coverage = if file.language == Language::KotlinScript {
        Coverage::SyntaxOnly("scripts are checked for syntax only".into())
    } else if ctx.is_none() {
        Coverage::SyntaxOnly("file is not part of any module's source roots".into())
    } else if snap.workspace.jdk.is_none() {
        Coverage::SyntaxOnly("no JDK found".into())
    } else if !snap.workspace.is_complete(ctx) {
        let m = snap.workspace.module(ctx.unwrap().module);
        Coverage::SyntaxOnly(format!("dependencies of module {} are not fully resolved", m.name))
    } else {
        Coverage::Full
    };
    if coverage != Coverage::Full {
        return (out, coverage);
    }
    let resolver = snap.resolver(path);
    if !resolver.classpath.has_builtins() {
        return (out, Coverage::SyntaxOnly("the Kotlin standard library is not on the classpath".into()));
    }
    let summary = file.summary();

    // Imports.
    for imp in &summary.imports {
        if let Err(i) = resolver.resolve_import(imp)
            && let Some((name, range)) = imp.segments.get(i) {
                out.push(unresolved(*range, name));
            }
    }

    // Type references anywhere in the file.
    let root = parse.syntax();
    let mut locals_cache = LocalsCache::default();
    for node in root.descendants().filter(|n| n.kind() == SyntaxKind::USER_TYPE) {
        if node.ancestors().any(|a| a.kind() == SyntaxKind::ERROR) {
            continue;
        }
        let ut = ast::UserType::cast(node.clone()).unwrap();
        let segs: Vec<(String, TextRange)> =
            ut.segments().iter().filter_map(|s| s.name_token()).map(|t| (ast::ident_text(&t), t.text_range())).collect();
        if segs.is_empty() || segs.iter().any(|(s, _)| s.is_empty()) {
            continue;
        }
        let scope = scope_at(&node, &summary.package, &mut locals_cache);
        let names: Vec<&str> = segs.iter().map(|(s, _)| s.as_str()).collect();
        match resolver.resolve_type_name(&names, &scope) {
            Ok(Resolved::Classifier(_) | Resolved::TypeParameter(_) | Resolved::Local(_)) => {}
            Ok(_) => out.push(unresolved(segs.last().unwrap().1, &segs.last().unwrap().0)),
            Err(i) => out.push(unresolved(segs[i].1, &segs[i].0)),
        }
    }

    // Redeclarations of classifiers within the same module.
    let visible = |c: Option<super::FileContext>| c.is_some_and(|c| Some(c.module) == ctx.map(|x| x.module));
    for d in summary.classifiers() {
        if d.symbol.modifiers.has(Modifiers::EXPECT) || d.symbol.modifiers.has(Modifiers::ACTUAL) {
            continue;
        }
        let others = snap
            .index
            .classifier(&d.symbol.fq_name, &visible)
            .filter(|l| *l.path != *path)
            .filter_map(|l| {
                let f = snap.file(&l.path)?;
                let s = f.summary();
                let decl = l.resolve(&s)?;
                let platform_specific = decl.symbol.modifiers.has(Modifiers::EXPECT) || decl.symbol.modifiers.has(Modifiers::ACTUAL);
                (!platform_specific).then_some(())
            })
            .count();
        if others > 0
            && let Origin::Source { name_range, .. } = &d.symbol.origin {
                out.push(Diagnostic {
                    range: *name_range,
                    severity: Severity::Error,
                    message: format!("Redeclaration: {}", d.symbol.name),
                    code: "REDECLARATION",
                });
            }
    }

    out.sort_by_key(|d| (d.range.start(), d.range.end()));
    out.dedup();
    (out, Coverage::Full)
}

fn unresolved(range: TextRange, name: &str) -> Diagnostic {
    Diagnostic { range, severity: Severity::Error, message: format!("Unresolved reference '{name}'."), code: "UNRESOLVED_REFERENCE" }
}

fn decl_name(n: &SyntaxNode) -> Option<String> {
    let name = ast::token(n, SyntaxKind::IDENT).map(|t| ast::ident_text(&t));
    match n.kind() {
        SyntaxKind::OBJECT_DECLARATION => {
            let companion = ast::child::<ast::ModifierList>(n).is_some_and(|m| m.has("companion"));
            name.or_else(|| companion.then(|| "Companion".to_string()))
        }
        _ => name,
    }
}

fn type_param_names(n: &SyntaxNode) -> Vec<String> {
    n.children()
        .filter(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST)
        .flat_map(|l| l.children().filter_map(|tp| ast::token(&tp, SyntaxKind::IDENT)).map(|t| ast::ident_text(&t)).collect::<Vec<_>>())
        .collect()
}

/// Supertype names (dotted segments) of a class/object node.
fn supertype_names(n: &SyntaxNode) -> Vec<Vec<String>> {
    n.children()
        .filter(|c| c.kind() == SyntaxKind::SUPER_TYPE_LIST)
        .flat_map(|l| l.children().collect::<Vec<_>>())
        .filter_map(|entry| ast::child::<ast::TypeRef>(&entry))
        .filter_map(|t| t.inner().and_then(ast::UserType::cast))
        .map(|ut| ut.segments().iter().filter_map(|s| s.name_token()).map(|t| ast::ident_text(&t)).collect())
        .collect()
}

/// Classifier names declared inside each body, keyed by the body's range.
#[derive(Default)]
pub struct LocalsCache(std::collections::HashMap<TextRange, Vec<String>>);

/// Compute the type scope at `node` from its ancestors.
pub fn scope_at(node: &SyntaxNode, package: &str, cache: &mut LocalsCache) -> TypeScope {
    let mut scope = TypeScope::default();
    let mut class_chain: Vec<Option<String>> = Vec::new();
    for anc in node.ancestors() {
        if matches!(anc.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL) && anc != *node {
            scope.supertypes.extend(supertype_names(&anc));
        }
        match anc.kind() {
            SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => {
                scope.type_params.extend(type_param_names(&anc));
                class_chain.push(decl_name(&anc));
            }
            SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::TYPEALIAS => scope.type_params.extend(type_param_names(&anc)),
            SyntaxKind::BLOCK | SyntaxKind::OBJECT_LITERAL => {
                // A body: classes declared in it are local; enclosing classes above are not
                // addressable by FQ name from here.
                let locals = cache.0.entry(anc.text_range()).or_insert_with(|| {
                    anc.descendants()
                        .filter(|c| matches!(c.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS))
                        .filter_map(|c| decl_name(&c))
                        .collect()
                });
                scope.locals.extend(locals.iter().cloned());
                class_chain.push(None);
            }
            _ => {}
        }
    }
    // Enclosing classes with a FQ name: the chain up to the first local scope boundary.
    class_chain.reverse();
    let mut fq = package.to_string();
    for c in class_chain {
        match c {
            Some(name) => {
                fq = if fq.is_empty() { name } else { format!("{fq}.{name}") };
                scope.enclosing.push(fq.clone());
            }
            None => break,
        }
    }
    scope
}

#[cfg(test)]
mod tests;
