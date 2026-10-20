//! Imports: candidates for unresolved names ("Import …" quick fixes) and organize imports.

use std::path::Path;

use super::completion::{import_edit, importable};
use super::infer::{Engine, Subst};
use crate::analysis::Snapshot;
use crate::analysis::resolve::KOTLIN_DEFAULT_IMPORTS;
use crate::symbols::{Origin, Symbol, Visibility};
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportFix {
    /// The unresolved name and where it is.
    pub name: String,
    pub range: TextRange,
    pub fq: String,
    pub edit: (TextRange, String),
}

/// How an unresolved name is used, which decides what may be imported for it.
enum Usage {
    /// `Foo` in a type position.
    Type,
    /// A simple name in an expression: classes and top-level functions/properties.
    Simple,
    /// `recv.name`: extensions applicable to the receiver (`None`: receiver type unknown).
    Selector(Option<crate::symbols::TypeRef>),
}

/// Unresolved names in `range` and the declarations that an import would bring into scope.
pub fn import_fixes(snap: &Snapshot, path: &Path, range: TextRange) -> Vec<ImportFix> {
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let Some(file) = snap.file(path) else { return Vec::new() };
    let mut out: Vec<ImportFix> = Vec::new();
    for tok in tokens_in(&e.root, range) {
        let Some((name, usage)) = unresolved(&e, &tok) else { continue };
        for fq in candidates(&e, &name, &usage) {
            let fix = ImportFix { name: name.clone(), range: tok.text_range(), edit: import_edit(file, &fq), fq };
            if !out.contains(&fix) {
                out.push(fix);
            }
        }
    }
    out
}

/// Identifiers in `range`, or the one touching an empty range.
fn tokens_in(root: &SyntaxNode, range: TextRange) -> Vec<SyntaxToken> {
    if range.is_empty() {
        return root.token_at_offset(range.start()).filter(|t| t.kind() == SyntaxKind::IDENT).take(1).collect();
    }
    root.descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|t| t.kind() == SyntaxKind::IDENT && t.text_range().intersect(range).is_some_and(|r| !r.is_empty() || range.is_empty()))
        .collect()
}

fn unresolved(e: &Engine, tok: &SyntaxToken) -> Option<(String, Usage)> {
    let parent = tok.parent()?;
    if parent.ancestors().any(|a| matches!(a.kind(), SyntaxKind::IMPORT_LIST | SyntaxKind::PACKAGE_DIRECTIVE)) {
        return None;
    }
    let name = ast::ident_text(tok);
    match parent.kind() {
        SyntaxKind::USER_TYPE_SEGMENT => {
            // Only the first segment of `a.b.C` can be imported.
            let ut = parent.parent()?;
            if ut.children().find(|c| c.kind() == SyntaxKind::USER_TYPE_SEGMENT).as_ref() != Some(&parent) || !super::type_segment(e, &parent).is_empty() {
                return None;
            }
            Some((name, Usage::Type))
        }
        SyntaxKind::NAME_REF => {
            if !e.resolve_name_ref(&parent).is_empty() {
                return None;
            }
            let expr = parent.parent().filter(|p| p.kind() == SyntaxKind::CALL_EXPR && p.first_child().as_ref() == Some(&parent)).unwrap_or_else(|| parent.clone());
            let outer = expr.parent()?;
            match outer.kind() {
                SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR if outer.first_child().as_ref() != Some(&expr) => {
                    let recv = outer.first_child()?;
                    Some((name, Usage::Selector(e.type_of(&recv))))
                }
                SyntaxKind::CALLABLE_REFERENCE if outer.first_child().as_ref() != Some(&expr) => None,
                _ => Some((name, Usage::Simple)),
            }
        }
        _ => None,
    }
}

/// FQ names to import for `name`: project declarations first, then libraries.
fn candidates(e: &Engine, name: &str, usage: &Usage) -> Vec<String> {
    let own = e.package();
    let mut project: Vec<String> = Vec::new();
    let mut library: Vec<String> = Vec::new();
    let classes = matches!(usage, Usage::Type | Usage::Simple);
    if classes {
        project.extend(e.res.source_classifiers_where(&|s| s == name).into_iter().map(|(_, fq)| fq));
        library.extend(e.res.classpath.top_level_classes_where(&|s| s == name));
    }
    if matches!(usage, Usage::Simple | Usage::Selector(_)) {
        let wants = |s: &Symbol| match usage {
            Usage::Selector(recv) => {
                s.receiver.is_some()
                    && recv.as_ref().is_none_or(|t| !e.applicable_to(vec![(s.clone(), Subst::new())], t).is_empty())
            }
            _ => s.receiver.is_none(),
        };
        for s in e.res.source_top_level_where(&|n| n == name) {
            if matches!(s.origin, Origin::Source { .. }) && s.visibility != Visibility::Private && wants(&s) {
                project.push(s.fq_name.clone());
            }
        }
        for pkg in e.res.classpath.packages_with_members() {
            for s in e.res.classpath.package_members_named(&pkg, name) {
                if s.kind.is_callable() && s.visibility == Visibility::Public && wants(&s) {
                    library.push(s.fq_name.clone());
                }
            }
        }
    }
    let package_of = |fq: &str| fq.rsplit_once('.').map(|(p, _)| p.to_string()).unwrap_or_default();
    let keep = |fq: &String| importable(fq) && package_of(fq) != own && !KOTLIN_DEFAULT_IMPORTS.contains(&package_of(fq).as_str());
    project.sort();
    library.sort_by_key(|fq| (fq.starts_with("java.") || fq.starts_with("javax.") || fq.starts_with("sun."), fq.clone()));
    let mut out: Vec<String> = Vec::new();
    for fq in project.into_iter().chain(library).filter(keep) {
        if !out.contains(&fq) {
            out.push(fq);
        }
    }
    out
}

/// The import list with unused imports removed and sorted by the `.editorconfig`
/// `ij_kotlin_imports_layout`, or the IntelliJ default (others, `java.*`, `javax.*`,
/// `kotlin.*`, then aliases). `None` if nothing changes.
pub fn organize_imports(snap: &Snapshot, path: &Path) -> Option<(TextRange, String)> {
    let file = snap.file(path)?;
    let root = file.parse().syntax();
    let list = root.descendants().find(|n| n.kind() == SyntaxKind::IMPORT_LIST)?;
    let directives: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::IMPORT_DIRECTIVE).collect();
    let (first, last) = (directives.first()?, directives.last()?);
    let span = TextRange::new(first.text_range().start(), last.text_range().end());
    // Comments between imports would be lost: leave such lists alone.
    let commented = list
        .descendants_with_tokens()
        .filter_map(|t| t.into_token())
        .any(|t| matches!(t.kind(), SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT | SyntaxKind::KDOC) && span.contains_range(t.text_range()));
    if commented || directives.iter().any(|d| d.descendants().any(|n| n.kind() == SyntaxKind::ERROR)) {
        return None;
    }
    let full = crate::analysis::diagnostics::coverage(snap, path) == crate::analysis::diagnostics::Coverage::Full;
    let unused = crate::analysis::lints::unused_imports(snap, path, full);
    let layout = ImportLayout::for_file(path);
    let mut kept: Vec<(usize, String)> = Vec::new();
    for d in &directives {
        if unused.contains(&d.text_range()) {
            continue;
        }
        let text = d.text().to_string().trim_end_matches(';').split_whitespace().collect::<Vec<_>>().join(" ");
        kept.push((layout.group(&text), text));
    }
    kept.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| import_order(&a.1, &b.1)));
    kept.dedup();
    let mut new_text = String::new();
    for (i, (group, text)) in kept.iter().enumerate() {
        if i > 0 {
            new_text.push('\n');
            if layout.blank_before(*group, kept[i - 1].0) {
                new_text.push('\n');
            }
        }
        new_text.push_str(text);
    }
    let mut range = span;
    // Removing every import also removes the blank line(s) after the list.
    if new_text.is_empty() {
        let rest = &file.text[usize::from(span.end())..];
        let ws = rest.len() - rest.trim_start_matches(['\n', '\r', ' ', '\t']).len();
        range = TextRange::new(span.start(), span.end() + TextSize::from(ws as u32));
    }
    let old = &file.text[usize::from(range.start())..usize::from(range.end())];
    (old != new_text).then_some((range, new_text))
}

/// Lexicographic by path segments, so `a.b.*` sorts with `a.b.C` and `a.b` before `a.bc`.
fn import_order(a: &str, b: &str) -> std::cmp::Ordering {
    let segs = |s: &str| s.trim_start_matches("import ").split('.').map(str::to_string).collect::<Vec<_>>();
    segs(a).cmp(&segs(b))
}

/// An `ij_kotlin_imports_layout`: `*` (anything else), `a.b.**` (a package and its
/// subpackages), `^` (alias imports, or `^a.**`) and `|` (a blank line).
struct ImportLayout {
    entries: Vec<String>,
}

impl ImportLayout {
    const DEFAULT: &str = "*,java.**,javax.**,kotlin.**,^";

    fn for_file(path: &Path) -> ImportLayout {
        let value = editorconfig_value(path, "ij_kotlin_imports_layout").unwrap_or_else(|| Self::DEFAULT.to_string());
        ImportLayout { entries: value.split(',').map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect() }
    }

    /// The index of the entry that `import` (a whole directive) belongs to: the most
    /// specific matching pattern.
    fn group(&self, import: &str) -> usize {
        let body = import.trim_start_matches("import ");
        let (path, alias) = match body.split_once(" as ") {
            Some((p, _)) => (p, true),
            None => (body, false),
        };
        let mut best: Option<(usize, usize)> = None;
        for (i, e) in self.entries.iter().enumerate() {
            let (pattern, for_alias) = match e.strip_prefix('^') {
                Some(rest) => (rest, true),
                None => (e.as_str(), false),
            };
            if for_alias != alias || pattern == "|" {
                continue;
            }
            let specificity = match pattern {
                "" | "*" => 1,
                p => match p.strip_suffix(".**").or_else(|| p.strip_suffix(".*")) {
                    Some(prefix) if path == prefix || path.starts_with(&format!("{prefix}.")) => 2 + prefix.len(),
                    _ => continue,
                },
            };
            if best.is_none_or(|(_, s)| specificity > s) {
                best = Some((i, specificity));
            }
        }
        // Aliases without a `^` entry go with the other imports.
        match best {
            Some((i, _)) => i,
            None if alias => self.group(&format!("import {path}")),
            None => self.entries.len(),
        }
    }

    /// Whether a `|` separates entries `prev` and `next`.
    fn blank_before(&self, next: usize, prev: usize) -> bool {
        next > prev && self.entries[prev.min(self.entries.len())..next.min(self.entries.len())].iter().any(|e| e == "|")
    }
}

/// The value of `key` for a Kotlin file from the `.editorconfig` files above `path`
/// (closest wins; sections are matched loosely: `*`, or any glob mentioning `kt`).
fn editorconfig_value(path: &Path, key: &str) -> Option<String> {
    for dir in path.ancestors().skip(1) {
        let Ok(text) = std::fs::read_to_string(dir.join(".editorconfig")) else { continue };
        let mut applies = false;
        let mut found = None;
        let mut root = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                applies = section == "*" || section.contains("kt");
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            if k == "root" && v.eq_ignore_ascii_case("true") {
                root = true;
            } else if applies && k == key {
                found = Some(v.to_string());
            }
        }
        if found.is_some() || root {
            return found;
        }
    }
    None
}
