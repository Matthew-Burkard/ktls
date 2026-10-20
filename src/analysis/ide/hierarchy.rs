//! Go to implementation (subclasses and overrides) and go to type definition.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use super::infer::{Def, Engine, class_type, fq_of, strip, substitute};
use super::{NavTarget, def_location, defs_at};
use crate::analysis::Snapshot;
use crate::analysis::source::SourceDecl;
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeRef};
use crate::syntax::{SyntaxKind, TextSize};

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
