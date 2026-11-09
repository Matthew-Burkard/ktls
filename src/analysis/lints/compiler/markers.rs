//! Declaration markers: deprecation levels (`@Deprecated(level = …)`, and for libraries
//! `@DeprecatedSinceKotlin` against the module's API version) and opt-in requirements
//! (annotations that are themselves `@RequiresOptIn`), of project and library declarations.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::super::*;
use crate::analysis::ide::completion::import_edit;
use crate::analysis::ide::infer::{Def, Engine, fq_of};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind};

/// The declaration node of a project symbol, in its file's tree.
pub(super) fn source_decl(cx: &Cx, sym: &Symbol) -> Option<SyntaxNode> {
    let Origin::Source { file, name_range, .. } = &sym.origin else { return None };
    let root = if file.as_path() == cx.path { cx.root.clone() } else { cx.snap.file(file)?.parse().syntax() };
    crate::syntax::covering(&root, *name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| {
        matches!(
            a.kind(),
            SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS | SyntaxKind::ENUM_ENTRY | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::VALUE_PARAMETER
        )
    })
}

/// The annotations of a declaration whose type is written as one of `names`.
fn annotations_named<'n>(decl: &SyntaxNode, names: &'n [&'n str]) -> impl Iterator<Item = ast::Annotation> + 'n {
    modifiers(decl).into_iter().flat_map(|m| m.annotations().collect::<Vec<_>>()).filter(move |a| a.type_ref().is_some_and(|t| names.contains(&t.syntax().text().to_string().as_str())))
}

/// An annotation argument names the enum constant `name` (`level = DeprecationLevel.ERROR`).
fn mentions(a: &ast::Annotation, names: &[&str]) -> bool {
    a.syntax().descendants_with_tokens().filter_map(|t| t.into_token()).any(|t| t.kind() == SyntaxKind::IDENT && names.contains(&t.text()))
}

/// How a use of a deprecated declaration is reported.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Level {
    Warning,
    Error,
}

/// `major.minor` of a version string.
fn version(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.split('.');
    Some((parts.next()?.parse().ok()?, parts.next().and_then(|m| m.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()).unwrap_or(0)))
}

/// The deprecation level of `sym` at this module's Kotlin API version (`None`: not
/// deprecated, or not yet).
pub(super) fn deprecation(cx: &Cx, sym: &Symbol) -> Option<Level> {
    if !sym.modifiers.has(Modifiers::DEPRECATED) {
        return None;
    }
    if matches!(sym.origin, Origin::Source { .. }) {
        let error = source_decl(cx, sym).is_some_and(|d| annotations_named(&d, &["Deprecated", "kotlin.Deprecated"]).any(|a| mentions(&a, &["ERROR", "HIDDEN"])));
        return Some(if error { Level::Error } else { Level::Warning });
    }
    let declared = if sym.modifiers.has(Modifiers::DEPRECATED_ERROR) { Level::Error } else { Level::Warning };
    let Some(since) = sym.annotations.iter().find_map(|a| a.strip_prefix("kotlin.DeprecatedSinceKotlin(")?.strip_suffix(')')) else { return Some(declared) };
    // The level grows with the API version; unknown versions keep the declared one.
    let Some(api) = cx.module.and_then(|m| m.kotlin.api_version.as_deref().or(m.kotlin.language_version.as_deref()).or(m.kotlin.version.as_deref())).and_then(version) else {
        return Some(declared);
    };
    let reached = |key: &str| since.split(',').filter_map(|kv| kv.split_once('=')).any(|(k, v)| k == key && version(v).is_some_and(|v| v <= api));
    if reached("errorSince") || reached("hiddenSince") {
        Some(Level::Error)
    } else if reached("warningSince") {
        Some(Level::Warning)
    } else {
        None
    }
}

/// An opt-in marker: its FQ name and level.
#[derive(Clone)]
struct Marker {
    fq: String,
    name: String,
    level: Level,
}

/// Engines for other files and the markers found, shared across one file's check.
struct Markers<'a> {
    engines: HashMap<PathBuf, Option<Engine<'a>>>,
    /// Marker annotation class FQ → level (`None`: not a marker).
    levels: HashMap<String, Option<Level>>,
    /// Declaration (file, name range) → the markers it requires.
    decls: HashMap<(PathBuf, TextRange), Vec<Marker>>,
}

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    if !cx.on(&OPT_IN_USAGE) && !cx.on(&OPT_IN_USAGE_ERROR) {
        return;
    }
    let state = RefCell::new(Markers { engines: HashMap::new(), levels: HashMap::new(), decls: HashMap::new() });
    for n in cx.root.descendants() {
        if !matches!(n.kind(), SyntaxKind::NAME_REF | SyntaxKind::USER_TYPE) || cx.in_error(&n) {
            continue;
        }
        if n.ancestors().any(|a| matches!(a.kind(), SyntaxKind::IMPORT_DIRECTIVE | SyntaxKind::PACKAGE_DIRECTIVE | SyntaxKind::ANNOTATION | SyntaxKind::FILE_ANNOTATION_LIST)) {
            continue;
        }
        let syms: Vec<Symbol> = match n.kind() {
            SyntaxKind::NAME_REF => {
                let defs = e.resolve_name_ref(&n);
                if super::expressions::uncertain_implicit_member(e, &n, &defs) {
                    continue;
                }
                defs.into_iter().filter_map(|d| if let Def::Symbol { sym, .. } = d { Some(sym) } else { None }).collect()
            }
            _ => {
                // The outermost user type only (`a.B` is one reference).
                if n.parent().is_some_and(|p| p.kind() == SyntaxKind::USER_TYPE) {
                    continue;
                }
                let Some(t) = n.parent().filter(|p| p.kind() == SyntaxKind::TYPE_REFERENCE).and_then(ast::TypeRef::cast) else { continue };
                let ty = e.resolve_syntax_type(&t);
                fq_of(&ty).and_then(|fq| e.classifier(fq)).map(|c| vec![c.symbol().clone()]).unwrap_or_default()
            }
        };
        let Some(first) = syms.first() else { continue };
        let mut required = required_markers(cx, &state, first);
        // Every candidate must require the marker.
        for s in &syms[1..] {
            let theirs = required_markers(cx, &state, s);
            required.retain(|m| theirs.iter().any(|t| t.fq == m.fq));
        }
        for m in required {
            if opted_in(cx, &n, &m) {
                continue;
            }
            let at = match n.kind() {
                SyntaxKind::USER_TYPE => n.descendants_with_tokens().filter_map(|t| t.into_token()).filter(|t| t.kind() == SyntaxKind::IDENT).last().map_or(n.text_range(), |t| t.text_range()),
                _ => n.text_range(),
            };
            let (rule, verb) = match m.level {
                Level::Error => (&OPT_IN_USAGE_ERROR, "must"),
                Level::Warning => (&OPT_IN_USAGE, "should"),
            };
            let msg = format!("This declaration needs opt-in. Its usage {verb} be marked with '@{0}' or '@OptIn({0}::class)'.", m.name);
            out.report(rule, at, msg, opt_in_fix(cx, &n, &m));
        }
    }
}

/// The markers a use of `sym` requires: annotations on it (and on the class, for a
/// constructor). Members do not take their class's markers: through an unmarked subclass
/// (or anonymous object) they need no opt-in.
fn required_markers<'a>(cx: &Cx<'a>, state: &RefCell<Markers<'a>>, sym: &Symbol) -> Vec<Marker> {
    let Origin::Source { file, name_range, .. } = &sym.origin else {
        // Library declarations carry their annotations' names.
        let class = (sym.kind == SymbolKind::Constructor).then(|| sym.fq_name.rsplit_once('.').and_then(|(c, _)| cx.engine()?.classifier(c))).flatten();
        let names = sym.annotations.iter().chain(class.iter().flat_map(|c| c.symbol().annotations.iter()));
        return names.filter_map(|fq| marker_named(cx, state, fq)).collect();
    };
    let key = (file.clone(), *name_range);
    if let Some(m) = state.borrow().decls.get(&key) {
        return m.clone();
    }
    let mut found = Vec::new();
    if let Some(decl) = source_decl(cx, sym) {
        let class = (sym.kind == SymbolKind::Constructor).then(|| decl.ancestors().find(|a| a.kind() == SyntaxKind::CLASS)).flatten();
        for owner in std::iter::once(decl.clone()).chain(class) {
            for a in modifiers(&owner).into_iter().flat_map(|m| m.annotations().collect::<Vec<_>>()) {
                let Some(t) = a.type_ref() else { continue };
                if let Some(m) = marker(cx, state, file, &t) {
                    found.push(m);
                }
            }
        }
    }
    state.borrow_mut().decls.insert(key, found.clone());
    found
}

/// The opt-in marker an annotation (written in `file`) is, if any.
fn marker<'a>(cx: &Cx<'a>, state: &RefCell<Markers<'a>>, file: &Path, t: &ast::TypeRef) -> Option<Marker> {
    // The annotation's class, resolved where it is written.
    let fq = {
        let mut st = state.borrow_mut();
        let fq = if file == cx.path {
            cx.engine().and_then(|e| fq_of(&e.resolve_syntax_type(t)).map(str::to_string))
        } else {
            let e = st.engines.entry(file.to_path_buf()).or_insert_with(|| Engine::new(cx.snap, file));
            e.as_ref().and_then(|e| fq_of(&e.resolve_syntax_type(t)).map(str::to_string))
        };
        fq?
    };
    marker_named(cx, state, &fq)
}

/// The opt-in marker the annotation class `fq` is, if any.
fn marker_named<'a>(cx: &Cx<'a>, state: &RefCell<Markers<'a>>, fq: &str) -> Option<Marker> {
    let fq = fq.to_string();
    let cached = state.borrow().levels.get(&fq).copied();
    let level = match cached {
        Some(l) => l,
        None => {
            let e = cx.engine()?;
            let level = e.classifier(&fq).and_then(|c| {
                let sym = c.symbol();
                if sym.modifiers.has(Modifiers::OPT_IN_MARKER) {
                    return Some(if sym.modifiers.has(Modifiers::OPT_IN_WARNING) { Level::Warning } else { Level::Error });
                }
                let d = source_decl(cx, sym)?;
                let a = annotations_named(&d, &["RequiresOptIn", "kotlin.RequiresOptIn"]).next()?;
                Some(if mentions(&a, &["WARNING"]) { Level::Warning } else { Level::Error })
            });
            state.borrow_mut().levels.insert(fq.clone(), level);
            level
        }
    }?;
    let name = fq.rsplit('.').next().unwrap_or(&fq).to_string();
    Some(Marker { fq, name, level })
}

/// The use is opted in: by the module's compiler options, or an enclosing declaration (or
/// the file) annotated with `@OptIn(M::class)` or with the marker itself.
fn opted_in(cx: &Cx, n: &SyntaxNode, m: &Marker) -> bool {
    if cx.module.is_some_and(|md| md.kotlin.opt_in.contains(&m.fq)) {
        return true;
    }
    let names = [m.name.as_str(), m.fq.as_str()];
    let satisfies = |a: &ast::Annotation| {
        let Some(t) = a.type_ref() else { return false };
        let written = t.syntax().text().to_string();
        if names.contains(&written.as_str()) {
            return true;
        }
        matches!(written.as_str(), "OptIn" | "kotlin.OptIn")
            && a.syntax().descendants().filter(|d| d.kind() == SyntaxKind::CLASS_LITERAL).any(|c| c.first_child().is_some_and(|r| names.contains(&r.text().to_string().as_str())))
    };
    let file_level = cx.root.children().filter(|c| c.kind() == SyntaxKind::FILE_ANNOTATION_LIST).flat_map(|l| l.children()).filter_map(ast::Annotation::cast).any(|a| satisfies(&a));
    // Declarations (modifier lists) and annotated statements: `@OptIn(M::class) a.b = c`
    // parses with the annotation on the first operand only, but applies to the statement.
    let annotated_statement = |a: &SyntaxNode| {
        a.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK)
            && a.descendants().take_while(|d| d.text_range().start() == a.text_range().start()).any(|d| d.kind() == SyntaxKind::ANNOTATED_EXPR && d.children().filter_map(ast::Annotation::cast).any(|an| satisfies(&an)))
    };
    file_level || n.ancestors().any(|a| modifiers(&a).is_some_and(|ml| ml.annotations().any(|an| satisfies(&an))) || a.children().filter_map(ast::Annotation::cast).any(|an| satisfies(&an)) || annotated_statement(&a))
}

/// "Opt in for 'M' on 'f'": `@OptIn(M::class)` on the enclosing declaration.
fn opt_in_fix(cx: &Cx, n: &SyntaxNode, m: &Marker) -> Option<Fix> {
    let decl = n.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION) && !is_local(a))?;
    let name = name_token(&decl).map(|t| t.text().to_string()).unwrap_or_default();
    let start = decl.text_range().start();
    let indent = indent_at(cx.text, start);
    let mut edits = vec![Edit { range: TextRange::empty(start), text: format!("@OptIn({}::class)\n{indent}", m.name) }];
    let package = cx.snap.file(cx.path).map(|f| f.summary().package.clone()).unwrap_or_default();
    let imported = cx.snap.file(cx.path).is_some_and(|f| f.summary().imports.iter().any(|i| i.path() == m.fq));
    if m.fq.rsplit_once('.').map_or("", |(p, _)| p) != package && !imported
        && let Some(file) = cx.snap.file(cx.path)
    {
        let (range, text) = import_edit(file, &m.fq);
        edits.push(Edit { range, text });
    }
    Some(Fix::new(format!("Opt in for '{}' on '{name}'", m.name), edits))
}
