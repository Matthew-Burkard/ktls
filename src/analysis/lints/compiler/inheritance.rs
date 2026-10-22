//! Class hierarchies: cycles, members inherited with several implementations, extensions
//! that members shadow, and upper bounds that leave a type parameter no choice.

use std::collections::{HashMap, HashSet};

use super::super::*;
use crate::analysis::ide::infer::{Engine, fq_of, strip, substitute};
use crate::symbols::{Modifiers, Symbol, SymbolKind, TypeRef, Visibility};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => {
                if !cyclic(e, &n, out) {
                    many_implementations(e, &n, out);
                }
            }
            SyntaxKind::FUN | SyntaxKind::PROPERTY if n.children().any(|c| c.kind() == SyntaxKind::RECEIVER_TYPE) => shadowed_extension(e, &n, out),
            SyntaxKind::TYPE_PARAMETER => final_bound(e, &n, super::declarations::all_open(cx), out),
            _ => {}
        }
    }
}

/// The supertype entries of a class with their resolved types.
fn supertype_entries(e: &Engine, c: &SyntaxNode) -> Vec<(SyntaxNode, TypeRef)> {
    c.children()
        .filter(|x| x.kind() == SyntaxKind::SUPER_TYPE_LIST)
        .flat_map(|l| l.children().collect::<Vec<_>>())
        .filter_map(|entry| {
            let t = entry.descendants().find_map(ast::TypeRef::cast)?;
            Some((entry, e.resolve_syntax_type(&t)))
        })
        .collect()
}

/// The FQ name of the class declared by `c`.
fn own_fq(e: &Engine, c: &SyntaxNode) -> Option<String> {
    name_token(c)?;
    let names: Vec<String> = c.ancestors().filter(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION)).map(|a| name_token(&a).map(|t| ast::ident_text(&t))).collect::<Option<Vec<_>>>()?;
    if c.ancestors().skip(1).any(|a| a.kind() == SyntaxKind::BLOCK) {
        return None;
    }
    let path: Vec<String> = names.into_iter().rev().collect();
    let pkg = e.package();
    Some(if pkg.is_empty() { path.join(".") } else { format!("{pkg}.{}", path.join(".")) })
}

/// A supertype that (through its own supertypes) inherits from the class itself.
fn cyclic(e: &Engine, c: &SyntaxNode, out: &mut Sink) -> bool {
    let Some(me) = own_fq(e, c) else { return false };
    for (entry, t) in supertype_entries(e, c) {
        let Some(start) = fq_of(&t) else { continue };
        let mut queue = vec![start.to_string()];
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(fq) = queue.pop() {
            if fq == me {
                let at = entry.descendants().find_map(ast::TypeRef::cast).map_or(entry.text_range(), |t| t.syntax().text_range());
                out.report(&CYCLIC_INHERITANCE_HIERARCHY, at, "There's a cycle in the inheritance hierarchy for this type.", None);
                return true;
            }
            if !seen.insert(fq.clone()) || seen.len() > 200 {
                continue;
            }
            let Some(cls) = e.classifier(&fq) else { continue };
            let sym = cls.symbol();
            if !matches!(sym.origin, crate::symbols::Origin::Source { .. }) {
                continue;
            }
            for st in &sym.supertypes {
                if let Some(f) = fq_of(&e.resolve_declared(sym, st)) {
                    queue.push(f.to_string());
                }
            }
        }
    }
    false
}

/// Whether `m` has a body (is not abstract).
fn implemented(m: &Symbol) -> bool {
    !m.modifiers.has(Modifiers::ABSTRACT)
}

/// A member that two unrelated super-interfaces both implement, not overridden here.
fn many_implementations(e: &Engine, c: &SyntaxNode, out: &mut Sink) {
    if is_interface(c) || has_modifier(c, "expect") {
        return;
    }
    let entries = supertype_entries(e, c);
    if entries.iter().any(|(en, _)| en.kind() == SyntaxKind::DELEGATED_SUPER_TYPE_ENTRY) {
        return;
    }
    let mut interfaces: Vec<(String, TypeRef)> = Vec::new();
    for (_, t) in &entries {
        let Some(fq) = fq_of(t) else { return };
        let Some(cls) = e.classifier(fq) else { return };
        let kotlin = match &cls {
            crate::analysis::resolve::Classifier::Library(lc) => lc.is_kotlin,
            crate::analysis::resolve::Classifier::Source(_) => true,
            _ => false,
        };
        match cls.symbol().kind {
            SymbolKind::Interface if kotlin => interfaces.push((fq.to_string(), t.clone())),
            // A superclass may implement the member itself: not compared here.
            _ if fq == "kotlin.Any" => {}
            _ => return,
        }
    }
    if interfaces.len() < 2 {
        return;
    }
    // Members declared here.
    let own: HashSet<(String, usize)> = c
        .children()
        .filter(|x| x.kind() == SyntaxKind::CLASS_BODY)
        .flat_map(|b| b.children())
        .filter(|m| matches!(m.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY))
        .chain(c.children().filter(|x| x.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR).flat_map(|p| p.descendants()).filter(|v| v.kind() == SyntaxKind::VALUE_PARAMETER))
        .filter_map(|m| {
            let n = name_token(&m).map(|t| ast::ident_text(&t))?;
            let arity = m.children().filter(|l| l.kind() == SyntaxKind::VALUE_PARAMETER_LIST).flat_map(|l| l.children()).filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER).count();
            Some((n, if m.kind() == SyntaxKind::FUN { arity } else { usize::MAX }))
        })
        .collect();
    // Signature → the interfaces implementing it.
    let mut impls: HashMap<(String, usize, Vec<String>), Vec<String>> = HashMap::new();
    for (fq, _) in &interfaces {
        for m in e.declared_members(fq) {
            if !matches!(m.kind, SymbolKind::Function | SymbolKind::Property) || !implemented(&m) || m.visibility == Visibility::Private || m.receiver.is_some() || m.modifiers.has(Modifiers::STATIC) {
                continue;
            }
            let arity = if m.kind == SymbolKind::Function { m.params.len() } else { usize::MAX };
            let types: Vec<String> = m.params.iter().map(|p| e.resolve_declared(&m, &p.ty).display()).collect();
            impls.entry((m.name.clone(), arity, types)).or_default().push(fq.clone());
        }
    }
    let mut reported: HashSet<String> = HashSet::new();
    let mut keys: Vec<_> = impls.into_iter().collect();
    keys.sort();
    for ((name, arity, _), from) in keys {
        if from.len() < 2 || own.contains(&(name.clone(), arity)) || ["equals", "hashCode", "toString"].contains(&name.as_str()) {
            continue;
        }
        // One interface extending the other overrides its implementation.
        let related = from.iter().any(|a| from.iter().any(|b| a != b && e.conforms(&TypeRef::named(a), b) != Some(false)));
        if related || !reported.insert(name.clone()) {
            continue;
        }
        let at = name_token(c).map_or(c.text_range(), |t| t.text_range());
        let shown: Vec<&str> = from.iter().map(|f| f.rsplit('.').next().unwrap_or(f)).collect();
        out.report(&MANY_IMPL_MEMBER_NOT_IMPLEMENTED, at, format!("'{}' must override '{name}' because it inherits multiple implementations of it (from {}).", name_token(c).map(|t| t.text().to_string()).unwrap_or_default(), shown.join(", ")), None);
    }
}

/// An extension with the signature of a member of its receiver type: calls always pick
/// the member.
fn shadowed_extension(e: &Engine, f: &SyntaxNode, out: &mut Sink) {
    if f.children().any(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST) || is_local(f) {
        return;
    }
    let Some(name) = name_token(f) else { return };
    let Some(recv) = f.children().find(|c| c.kind() == SyntaxKind::RECEIVER_TYPE).and_then(|r| r.children().find_map(ast::TypeRef::cast)) else { return };
    let rt = e.resolve_syntax_type(&recv);
    let TypeRef::Named { fq: Some(fq), nullable: false, .. } = &rt else { return };
    if matches!(&**fq, "kotlin.Any" | "kotlin.Nothing") || e.classifier(fq).is_none() {
        return;
    }
    let n = ast::ident_text(&name);
    let params: Vec<SyntaxNode> = f.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST).flat_map(|l| l.children()).filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER).collect();
    if params.iter().any(|p| has_modifier(p, "vararg")) {
        return;
    }
    let mut ours: Vec<String> = Vec::new();
    for p in &params {
        let Some(t) = ast::child::<ast::TypeRef>(p) else { return };
        let ty = e.resolve_syntax_type(&t);
        if fq_of(&ty).is_none() {
            return;
        }
        ours.push(strip(&ty).display());
    }
    let special = has_modifier(f, "operator") || has_modifier(f, "infix");
    let kind = if f.kind() == SyntaxKind::FUN { SymbolKind::Function } else { SymbolKind::Property };
    for (m, subst) in e.members_named(&rt, &n) {
        let member_kind = if m.kind == SymbolKind::Field { SymbolKind::Property } else { m.kind };
        if member_kind != kind || m.visibility != Visibility::Public || m.params.len() != ours.len() || m.receiver.is_some() || m.params.iter().any(|p| p.vararg) {
            continue;
        }
        // An `operator`/`infix` extension is still used where the member is not.
        if special && !(m.modifiers.has(Modifiers::OPERATOR) || m.modifiers.has(Modifiers::INFIX)) {
            continue;
        }
        let theirs: Vec<String> = m.params.iter().map(|p| strip(&substitute(&e.resolve_declared(&m, &p.ty), &subst)).display()).collect();
        if theirs == ours {
            let what = if kind == SymbolKind::Function { "function" } else { "property" };
            out.report(&EXTENSION_SHADOWED_BY_MEMBER, name.text_range(), format!("Extension is shadowed by a member: {what} '{n}' of '{}'.", rt.display()), None);
            return;
        }
    }
}

/// `<T : String>`: a final class as the upper bound.
fn final_bound(e: &Engine, tp: &SyntaxNode, all_open: bool, out: &mut Sink) {
    let Some(t) = ast::child::<ast::TypeRef>(tp) else { return };
    let Some(owner) = tp.parent().and_then(|l| l.parent()) else { return };
    if has_modifier(&owner, "override") {
        return;
    }
    let ty = e.resolve_syntax_type(&t);
    // `T : String?` (String or String?) and `T : Box<S>` (any S) still leave a choice.
    let TypeRef::Named { fq: Some(fq), args, nullable: false, .. } = &ty else { return };
    if !args.is_empty() {
        return;
    }
    let Some(cls) = e.classifier(fq) else { return };
    let sym = cls.symbol();
    let open = [Modifiers::OPEN, Modifiers::ABSTRACT, Modifiers::SEALED].iter().any(|m| sym.modifiers.has(*m));
    if !matches!(sym.kind, SymbolKind::Class | SymbolKind::Enum | SymbolKind::Object) || open || matches!(&**fq, "kotlin.Any" | "kotlin.Nothing") {
        return;
    }
    // Java classes are compared by Java rules; project classes may be opened by an
    // all-open compiler plugin.
    if matches!(cls, crate::analysis::resolve::Classifier::Library(ref lc) if !lc.is_kotlin) || all_open && matches!(sym.origin, crate::symbols::Origin::Source { .. }) {
        return;
    }
    out.report(&FINAL_UPPER_BOUND, t.syntax().text_range(), format!("'{}' is a final type, and thus a value of the type parameter is predetermined.", t.syntax().text()), None);
}
