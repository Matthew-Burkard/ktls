//! Go to implementation (subclasses and overrides), go to declaration (overridden member,
//! `expect`), go to type definition, and the call and type hierarchies.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use super::infer::{Def, Engine, class_type, fq_of, strip, substitute};
use super::{NavTarget, def_location, defs_at, defs_of_token, references, token_at};
use crate::analysis::Snapshot;
use crate::analysis::source::SourceDecl;
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeRef};
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

/// A project class that inherits from the searched one, with its file.
#[derive(Clone, Debug)]
pub struct Inheritor {
    pub path: PathBuf,
    pub symbol: Symbol,
}

fn simple_name(t: &TypeRef) -> Option<String> {
    match strip(t) {
        TypeRef::Named { name, .. } => Some(name.rsplit('.').next().unwrap_or(name).split('<').next().unwrap_or_default().to_string()),
        _ => None,
    }
}

fn classifiers(decls: &[SourceDecl], out: &mut Vec<Symbol>) {
    for d in decls {
        if d.symbol.kind.is_classifier() {
            out.push(d.symbol.clone());
        }
        classifiers(&d.children, out);
    }
}

/// Every project class that has `fq` among its supertypes, directly or not.
///
/// Searches breadth first by name: a class can only inherit from `fq` through a declared
/// supertype named like `fq` or like an inheritor already found. Each candidate is
/// checked in its own file's context.
pub fn inheritors(snap: &Snapshot, fq: &str, cancelled: &(dyn Fn() -> bool + Sync)) -> Vec<Inheritor> {
    let mut found: Vec<Inheritor> = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([fq.to_string()]);
    let mut frontier: HashSet<String> = HashSet::from([fq.rsplit('.').next().unwrap_or(fq).to_string()]);
    while !frontier.is_empty() && !cancelled() {
        let files: Vec<PathBuf> = snap.files.values().filter(|f| frontier.iter().any(|n| f.text.contains(n.as_str()))).map(|f| f.path.clone()).collect();
        let round: Vec<Inheritor> = files
            .par_iter()
            .flat_map_iter(|path| {
                let mut out = Vec::new();
                let Some(file) = snap.file(path) else { return out };
                let mut syms = Vec::new();
                classifiers(&file.summary().decls, &mut syms);
                syms.retain(|s| !seen.contains(&s.fq_name) && s.supertypes.iter().filter_map(simple_name).any(|n| frontier.contains(&n)));
                if syms.is_empty() || cancelled() {
                    return out;
                }
                let Some(e) = Engine::new(snap, path) else { return out };
                for s in syms {
                    if e.supertype_closure(&class_type(&s)).iter().skip(1).any(|t| fq_of(t) == Some(fq)) {
                        out.push(Inheritor { path: path.clone(), symbol: s });
                    }
                }
                out
            })
            .collect();
        frontier.clear();
        for i in round {
            if seen.insert(i.symbol.fq_name.clone()) {
                frontier.insert(i.symbol.name.clone());
                found.push(i);
            }
        }
    }
    found.sort_by(|a, b| a.symbol.fq_name.cmp(&b.symbol.fq_name));
    found
}

fn target(path: &Path, sym: &Symbol) -> Option<NavTarget> {
    match &sym.origin {
        Origin::Source { name_range, .. } => Some(NavTarget { path: path.to_path_buf(), range: *name_range }),
        _ => None,
    }
}

/// Implementations of the class or member at `offset`: inheriting classes, or overriding
/// members. Other declarations fall back to their definition.
pub fn implementations(snap: &Snapshot, path: &Path, offset: TextSize, cancelled: &(dyn Fn() -> bool + Sync)) -> Vec<NavTarget> {
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let Some((_, defs)) = defs_at(&e, offset) else { return Vec::new() };
    let Some(Def::Symbol { sym, .. }) = defs.first() else {
        return defs.iter().filter_map(|d| def_location(snap, &e, d, false)).collect();
    };
    if sym.kind.is_classifier() && sym.kind != SymbolKind::TypeAlias {
        return inheritors(snap, &sym.fq_name, cancelled).iter().filter_map(|i| target(&i.path, &i.symbol)).collect();
    }
    // Members: overrides in the inheritors of the topmost project declaration.
    let Some((root, root_class)) = e.override_root(sym).filter(|(_, c)| e.classifier(c).is_some()) else {
        return def_location(snap, &e, &defs[0], false).into_iter().collect();
    };
    let mut out = Vec::new();
    for i in inheritors(snap, &root_class, cancelled) {
        let Some(file) = snap.file(&i.path) else { continue };
        let summary = file.summary();
        let Some(decl) = find_decl(&summary.decls, &i.symbol.fq_name) else { continue };
        let Some(fe) = Engine::new(snap, &i.path) else { continue };
        for m in decl.children.iter().map(|c| &c.symbol).filter(|m| m.name == sym.name && m.modifiers.has(Modifiers::OVERRIDE)) {
            // Below the selected member only: in subclasses of its class.
            if fe.overrides(m, &root, &root_class) && (root.fq_name == sym.fq_name || in_subclass_of(&fe, m, sym)) {
                out.extend(target(&i.path, m));
            }
        }
    }
    out
}

/// Go to declaration: for an `override`, the member it overrides; for an `actual`
/// declaration, its `expect`; otherwise the definition.
pub fn declaration(snap: &Snapshot, path: &Path, offset: TextSize) -> Vec<NavTarget> {
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let Some((_, defs)) = defs_at(&e, offset) else { return Vec::new() };
    let mut out: Vec<NavTarget> = Vec::new();
    for d in &defs {
        let found = match d {
            Def::Symbol { sym, .. } if sym.modifiers.has(Modifiers::ACTUAL) => expects(snap, sym),
            Def::Symbol { sym, .. } if sym.modifiers.has(Modifiers::OVERRIDE) => overridden(&e, sym).and_then(|b| def_location(snap, &e, &Def::Symbol { sym: b, subst: Default::default() }, !snap.workspace.offline)).into_iter().collect(),
            _ => Vec::new(),
        };
        let found = if found.is_empty() { def_location(snap, &e, d, !snap.workspace.offline).into_iter().collect() } else { found };
        for t in found {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// The supertype member that the `override` member `sym` overrides.
fn overridden(e: &Engine, sym: &Symbol) -> Option<Symbol> {
    let (container, _) = sym.fq_name.rsplit_once('.')?;
    let c = e.classifier(container)?;
    e.hidden_member(&class_type(c.symbol()), sym).map(|(m, _)| m)
}

/// The `expect` declarations matching the `actual` declaration `sym`.
fn expects(snap: &Snapshot, sym: &Symbol) -> Vec<NavTarget> {
    fn walk(path: &Path, ds: &[SourceDecl], sym: &Symbol, out: &mut Vec<NavTarget>) {
        for d in ds {
            let s = &d.symbol;
            if s.modifiers.has(Modifiers::EXPECT) && s.fq_name == sym.fq_name && s.kind.is_classifier() == sym.kind.is_classifier() && s.params.len() == sym.params.len() {
                out.extend(target(path, s));
            }
            if sym.fq_name.starts_with(&format!("{}.", s.fq_name)) {
                walk(path, &d.children, sym, out);
            }
        }
    }
    let mut out = Vec::new();
    for f in snap.files.values().filter(|f| f.text.contains("expect") && f.text.contains(sym.name.as_str())) {
        walk(&f.path, &f.summary().decls, sym, &mut out);
    }
    out
}

/// `m`'s class inherits from the class that declares `sym`.
fn in_subclass_of(e: &Engine, m: &Symbol, sym: &Symbol) -> bool {
    let (Some((mc, _)), Some((sc, _))) = (m.fq_name.rsplit_once('.'), sym.fq_name.rsplit_once('.')) else { return false };
    e.supertype_closure(&TypeRef::named(mc)).iter().skip(1).any(|t| fq_of(t) == Some(sc))
}

fn find_decl<'a>(decls: &'a [SourceDecl], fq: &str) -> Option<&'a SourceDecl> {
    for d in decls {
        if d.symbol.fq_name == fq && d.symbol.kind.is_classifier() {
            return Some(d);
        }
        if fq.starts_with(&format!("{}.", d.symbol.fq_name))
            && let Some(found) = find_decl(&d.children, fq)
        {
            return Some(found);
        }
    }
    None
}

/// The class of the value at `offset` (the type of a variable, property, call or
/// expression). Type arguments are not followed: `List<User>` goes to `List`.
pub fn type_definition(snap: &Snapshot, path: &Path, offset: TextSize) -> Vec<NavTarget> {
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let Some((_, defs)) = defs_at(&e, offset) else { return Vec::new() };
    let ty = match defs.first() {
        Some(Def::Local(l)) => l.ty.clone(),
        Some(Def::Symbol { sym, subst }) if !sym.kind.is_classifier() => e.symbol_type(sym).map(|t| substitute(&t, subst)),
        // A class name stands for itself (also as a constructor call).
        Some(Def::Symbol { sym, .. }) => Some(class_type(sym)),
        _ => None,
    };
    // Fall back to the expression's type (`foo()`, `it`).
    let ty = ty.or_else(|| {
        let tok = e.root.token_at_offset(offset).right_biased()?;
        let nr = tok.parent().filter(|p| p.kind() == SyntaxKind::NAME_REF)?;
        let expr = nr.parent().filter(|p| p.kind() == SyntaxKind::CALL_EXPR).unwrap_or(nr);
        e.type_of(&expr)
    });
    let Some(fq) = ty.as_ref().and_then(fq_of) else { return Vec::new() };
    let Some(c) = e.classifier(fq) else { return Vec::new() };
    def_location(snap, &e, &Def::Symbol { sym: c.symbol().clone(), subst: Default::default() }, !snap.workspace.offline).into_iter().collect()
}

/// Project classes that list `fq` among their declared supertypes.
pub fn direct_inheritors(snap: &Snapshot, fq: &str) -> Vec<Inheritor> {
    let simple = fq.rsplit('.').next().unwrap_or(fq).to_string();
    let files: Vec<PathBuf> = snap.files.values().filter(|f| f.text.contains(simple.as_str())).map(|f| f.path.clone()).collect();
    let mut found: Vec<Inheritor> = files
        .par_iter()
        .flat_map_iter(|path| {
            let mut out = Vec::new();
            let Some(file) = snap.file(path) else { return out };
            let mut syms = Vec::new();
            classifiers(&file.summary().decls, &mut syms);
            syms.retain(|s| s.supertypes.iter().filter_map(simple_name).any(|n| n == simple));
            if syms.is_empty() {
                return out;
            }
            let Some(e) = Engine::new(snap, path) else { return out };
            for s in syms {
                if s.supertypes.iter().any(|st| fq_of(&e.resolve_declared(&s, st)) == Some(fq)) {
                    out.push(Inheritor { path: path.clone(), symbol: s });
                }
            }
            out
        })
        .collect();
    found.sort_by(|a, b| a.symbol.fq_name.cmp(&b.symbol.fq_name));
    found
}

/// How to find a hierarchy item again in a later request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// A token (in a project file) that refers to or declares the item.
    Token(PathBuf, TextSize),
    /// A class by FQ name, resolved in the context of a file.
    Type(PathBuf, String),
}

#[derive(Clone, Debug)]
pub struct HierItem {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    pub deprecated: bool,
    pub path: PathBuf,
    pub range: TextRange,
    pub selection: TextRange,
    pub anchor: Anchor,
}

fn symbol_item(snap: &Snapshot, e: &Engine, sym: &Symbol, anchor: Anchor) -> Option<HierItem> {
    let nav = def_location(snap, e, &Def::Symbol { sym: sym.clone(), subst: Default::default() }, false)?;
    let range = match &sym.origin {
        Origin::Source { range, .. } if range.contains_range(nav.range) => *range,
        _ => nav.range,
    };
    let name = match sym.kind {
        SymbolKind::Constructor => sym.fq_name.trim_end_matches(".<init>").rsplit('.').next().unwrap_or_default().to_string(),
        _ => sym.name.clone(),
    };
    let container = sym.fq_name.rsplit_once('.').map(|(c, _)| c.trim_end_matches(".<init>").to_string());
    let detail = if sym.kind.is_classifier() { container } else { Some(sym.signature()) };
    Some(HierItem { name, detail, kind: sym.kind, deprecated: sym.modifiers.has(Modifiers::DEPRECATED), path: nav.path, range, selection: nav.range, anchor })
}

/// The function or constructor at `offset`, as the root of a call hierarchy.
pub fn prepare_call_hierarchy(snap: &Snapshot, path: &Path, offset: TextSize) -> Option<HierItem> {
    let e = Engine::new(snap, path)?;
    let tok = token_at(&e.root, offset)?;
    let (_, defs) = defs_of_token(&e, &tok)?;
    let sym = defs.into_iter().find_map(|d| match d {
        Def::Symbol { sym, .. } if matches!(sym.kind, SymbolKind::Function | SymbolKind::Constructor) => Some(sym),
        _ => None,
    })?;
    symbol_item(snap, &e, &sym, Anchor::Token(path.to_path_buf(), tok.text_range().start()))
}

/// The declaration a call inside `n` belongs to: function, constructor, property or class.
fn caller_of(n: &SyntaxNode) -> Option<SyntaxNode> {
    n.ancestors().find(|a| match a.kind() {
        SyntaxKind::FUN | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => true,
        SyntaxKind::PROPERTY => !crate::analysis::lints::is_local(a),
        _ => false,
    })
}

fn syntax_item(path: &Path, decl: &SyntaxNode) -> Option<HierItem> {
    let name_tok = ast::token(decl, SyntaxKind::IDENT).or_else(|| decl.first_token())?;
    let kind = match decl.kind() {
        SyntaxKind::FUN => SymbolKind::Function,
        SyntaxKind::SECONDARY_CONSTRUCTOR => SymbolKind::Constructor,
        SyntaxKind::PROPERTY => SymbolKind::Property,
        SyntaxKind::OBJECT_DECLARATION => SymbolKind::Object,
        _ => SymbolKind::Class,
    };
    let name = if kind == SymbolKind::Constructor { "constructor".to_string() } else { ast::ident_text(&name_tok) };
    let container: Vec<String> = decl
        .ancestors()
        .skip(1)
        .filter(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::FUN))
        .filter_map(|a| ast::token(&a, SyntaxKind::IDENT))
        .map(|t| ast::ident_text(&t))
        .collect();
    Some(HierItem {
        name,
        detail: (!container.is_empty()).then(|| container.into_iter().rev().collect::<Vec<_>>().join(".")),
        kind,
        deprecated: false,
        path: path.to_path_buf(),
        range: decl.text_range(),
        selection: name_tok.text_range(),
        anchor: Anchor::Token(path.to_path_buf(), name_tok.text_range().start()),
    })
}

/// Who calls the item: each calling declaration with the call ranges in its file.
pub fn incoming_calls(snap: &Snapshot, anchor: &Anchor, cancelled: &(dyn Fn() -> bool + Sync)) -> Vec<(HierItem, Vec<TextRange>)> {
    let Anchor::Token(path, offset) = anchor else { return Vec::new() };
    let Some(refs) = references(snap, path, *offset, false, cancelled) else { return Vec::new() };
    let mut out: Vec<(HierItem, Vec<TextRange>)> = Vec::new();
    for r in refs {
        let Some(file) = snap.file(&r.path) else { continue };
        let root = file.parse().syntax();
        let Some(el) = crate::syntax::covering(&root, r.range) else { continue };
        let node = match el {
            rowan::NodeOrToken::Node(n) => n,
            rowan::NodeOrToken::Token(t) => match t.parent() {
                Some(p) => p,
                None => continue,
            },
        };
        // Imports and type references are not calls.
        if node.ancestors().any(|a| matches!(a.kind(), SyntaxKind::IMPORT_LIST | SyntaxKind::KDOC)) {
            continue;
        }
        let Some(caller) = caller_of(&node) else { continue };
        let Some(item) = syntax_item(&r.path, &caller) else { continue };
        match out.iter_mut().find(|(i, _)| i.path == item.path && i.range == item.range) {
            Some((_, ranges)) => ranges.push(r.range),
            None => out.push((item, vec![r.range])),
        }
    }
    out
}

/// What the item calls: each called function or constructor with the call ranges.
pub fn outgoing_calls(snap: &Snapshot, anchor: &Anchor) -> Vec<(HierItem, Vec<TextRange>)> {
    let Anchor::Token(path, offset) = anchor else { return Vec::new() };
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let Some(tok) = token_at(&e.root, *offset) else { return Vec::new() };
    // The declaration: the anchor itself, or what it refers to.
    let decl_at = match tok.parent().filter(|p| super::DECLARATIONS.contains(&p.kind()) || p.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR) {
        Some(_) => Some((path.clone(), tok.text_range())),
        None => defs_of_token(&e, &tok).and_then(|(_, defs)| {
            defs.into_iter().find_map(|d| match d {
                Def::Symbol { sym, .. } => match sym.origin {
                    Origin::Source { file, name_range, .. } => Some((file, name_range)),
                    _ => None,
                },
                _ => None,
            })
        }),
    };
    let Some((file, name_range)) = decl_at else { return Vec::new() };
    let Some(fe) = Engine::new(snap, &file) else { return Vec::new() };
    let Some(decl) = crate::syntax::covering(&fe.root, name_range).and_then(|el| el.parent()).and_then(|p| caller_of_self(&p)) else { return Vec::new() };
    let mut out: Vec<(HierItem, Vec<TextRange>)> = Vec::new();
    let mut keys: Vec<(String, String)> = Vec::new();
    for nr in decl.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF) {
        // Calls made by nested declarations belong to them.
        if caller_of(&nr).as_ref() != Some(&decl) {
            continue;
        }
        if !nr.parent().is_some_and(|p| p.kind() == SyntaxKind::CALL_EXPR && p.first_child().as_ref() == Some(&nr)) {
            continue;
        }
        let Some(sym) = fe.resolve_name_ref(&nr).into_iter().find_map(|d| match d {
            Def::Symbol { sym, .. } if matches!(sym.kind, SymbolKind::Function | SymbolKind::Constructor) || sym.kind.is_classifier() => Some(sym),
            _ => None,
        }) else {
            continue;
        };
        let key = (sym.fq_name.clone(), sym.signature());
        match keys.iter().position(|k| *k == key) {
            Some(i) => out[i].1.push(nr.text_range()),
            None => {
                let Some(item) = symbol_item(snap, &fe, &sym, Anchor::Token(file.clone(), nr.text_range().start())) else { continue };
                keys.push(key);
                out.push((item, vec![nr.text_range()]));
            }
        }
    }
    out
}

/// `n` if it is a declaration that can make calls, else its caller.
fn caller_of_self(n: &SyntaxNode) -> Option<SyntaxNode> {
    if matches!(n.kind(), SyntaxKind::FUN | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION) { Some(n.clone()) } else { caller_of(n) }
}

/// The class at `offset`, as the root of a type hierarchy.
pub fn prepare_type_hierarchy(snap: &Snapshot, path: &Path, offset: TextSize) -> Option<HierItem> {
    let e = Engine::new(snap, path)?;
    let (_, defs) = defs_at(&e, offset)?;
    let sym = defs.into_iter().find_map(|d| match d {
        Def::Symbol { sym, .. } if sym.kind.is_classifier() => Some(sym),
        Def::Symbol { sym, .. } if sym.kind == SymbolKind::Constructor => e.classifier(sym.fq_name.trim_end_matches(".<init>")).map(|c| c.symbol().clone()),
        _ => None,
    })?;
    symbol_item(snap, &e, &sym, Anchor::Type(path.to_path_buf(), sym.fq_name.clone()))
}

pub fn supertypes(snap: &Snapshot, anchor: &Anchor) -> Vec<HierItem> {
    let Anchor::Type(path, fq) = anchor else { return Vec::new() };
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let Some(c) = e.classifier(fq) else { return Vec::new() };
    let sym = c.symbol().clone();
    // Resolve declared supertypes in the declaring file when it is a project class.
    let home = match &sym.origin {
        Origin::Source { file, .. } if file != path => Engine::new(snap, file),
        _ => None,
    };
    let de = home.as_ref().unwrap_or(&e);
    let ctx = match &sym.origin {
        Origin::Source { file, .. } => file.clone(),
        _ => path.clone(),
    };
    let mut out = Vec::new();
    // Classes without a declared supertype extend `Any` (interfaces show none).
    let implicit = [TypeRef::named("kotlin.Any")];
    let declared = if sym.supertypes.is_empty() && sym.fq_name != "kotlin.Any" && sym.kind != SymbolKind::Interface { &implicit[..] } else { &sym.supertypes[..] };
    for st in declared {
        let t = de.resolve_declared(&sym, st);
        let Some(sfq) = fq_of(&t) else { continue };
        let Some(sc) = de.classifier(sfq) else { continue };
        out.extend(symbol_item(snap, de, sc.symbol(), Anchor::Type(ctx.clone(), sfq.to_string())));
    }
    out
}

pub fn subtypes(snap: &Snapshot, anchor: &Anchor) -> Vec<HierItem> {
    let Anchor::Type(path, fq) = anchor else { return Vec::new() };
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    direct_inheritors(snap, fq).into_iter().filter_map(|i| symbol_item(snap, &e, &i.symbol, Anchor::Type(i.path.clone(), i.symbol.fq_name.clone()))).collect()
}
