//! Uses a declaration does not allow: indexing without a `get` or `set` operator, indexed
//! assignment of values `set` does not accept,
//! assignment through a private setter, `isInitialized` on a property that is not
//! `lateinit`, and spreading a nullable array.

use super::super::*;
use super::certain_type;
use super::expressions::{has_type_param, type_mismatch};
use crate::analysis::ide::infer::{Def, Engine, Subst, fq_of, substitute};
use crate::symbols::{Modifiers, Symbol, SymbolKind, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants() {
        let relevant = matches!(n.kind(), SyntaxKind::BINARY_EXPR | SyntaxKind::VALUE_ARGUMENT | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::INDEX_EXPR);
        if !relevant || in_error(&n) || super::certain::in_unknown_lambda(e, &n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::BINARY_EXPR if assignment_op(&n).is_some() => {
                let Some(target) = n.first_child() else { continue };
                if target.kind() == SyntaxKind::INDEX_EXPR {
                    no_operator(e, &target, "set", out);
                    if assignment_op(&n).is_some_and(|o| o.kind() == SyntaxKind::EQ) {
                        indexed_set(e, &n, &target, out);
                    }
                } else if assignment_op(&n).is_some_and(|o| o.kind() == SyntaxKind::EQ) {
                    invisible_setter(cx, e, &target, out);
                }
            }
            // `a[i] = x` needs only `set`; `a[i] += x` both.
            SyntaxKind::INDEX_EXPR => {
                let set = n.parent().is_some_and(|p| assignment_op(&p).is_some_and(|o| o.kind() == SyntaxKind::EQ) && p.first_child().as_ref() == Some(&n));
                if !set {
                    no_operator(e, &n, "get", out);
                }
            }
            SyntaxKind::VALUE_ARGUMENT => spread(e, &n, out),
            SyntaxKind::DOT_QUALIFIED_EXPR => is_initialized(e, &n, out),
            _ => {}
        }
    }
}

/// `list[0] = 1` on a read-only `List` (or any type without a `set` operator), `1[0]` on a
/// type without `get`.
fn no_operator(e: &Engine, index: &SyntaxNode, name: &str, out: &mut Sink) {
    let Some(recv) = index.first_child() else { return };
    let Some(t) = certain_type(e, &recv, true) else { return };
    let Some(fq) = fq_of(&t) else { return };
    if matches!(t, TypeRef::Named { nullable: true, .. }) || e.classifier(fq).is_none() || matches!(fq, "kotlin.Any" | "kotlin.Nothing") {
        return;
    }
    // The engine must see the same class (no smart cast it does not model).
    if e.type_of(&recv).as_ref().and_then(fq_of) != Some(fq) {
        return;
    }
    if !e.members_named(&t, name).is_empty() || !e.extensions(&t, name).is_empty() || !e.member_extensions(index, &t, name).is_empty() {
        return;
    }
    let shown = super::certain::shown(&t);
    if name == "set" {
        out.report(&NO_SET_METHOD, index.text_range(), format!("No 'set' operator for indexed assignment on '{shown}'."), None);
    } else {
        out.report(&NO_GET_METHOD, index.text_range(), format!("No 'get' operator for indexed access on '{shown}'."), None);
    }
}

/// `m["a"] = "b"` on a `MutableMap<String, Int>`: the indices and the value against every
/// `set` operator of the receiver's certain type.
fn indexed_set(e: &Engine, assign: &SyntaxNode, index: &SyntaxNode, out: &mut Sink) {
    let (Some(recv), Some(value)) = (index.first_child(), assign.children().last()) else { return };
    let Some(t) = certain_type(e, &recv, true) else { return };
    if !matches!(t, TypeRef::Named { nullable: false, .. }) || has_type_param(&t) || e.type_of(&recv).as_ref().and_then(fq_of) != fq_of(&t) {
        return;
    }
    let Some(list) = index.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST) else { return };
    // The indices are bare expressions.
    let mut args: Vec<SyntaxNode> = list.children().collect();
    args.push(value);
    let candidates: Vec<(Symbol, Subst)> = e.members_named(&t, "set").into_iter().chain(e.extensions(&t, "set")).chain(e.member_extensions(index, &t, "set")).collect();
    if candidates.is_empty() {
        return;
    }
    let mut first = None;
    for (sym, subst) in &candidates {
        if sym.kind != SymbolKind::Function || sym.params.len() != args.len() || sym.params.iter().any(|p| p.vararg) {
            return;
        }
        // The function's own type parameters stay unknown.
        if sym.type_params.iter().any(|tp| !subst.contains_key(&tp.name)) {
            return;
        }
        let mut rejected = None;
        for (p, a) in sym.params.iter().zip(&args) {
            let pt = substitute(&e.resolve_declared(sym, &p.ty), subst);
            if has_type_param(&pt) || !matches!(pt, TypeRef::Named { fq: Some(_), .. }) {
                return;
            }
            if let Some((rule, message, _, convert)) = type_mismatch(e, a, &pt, &super::certain::shown(&pt)) {
                rejected = Some((a.clone(), rule, message, convert));
                break;
            }
        }
        match rejected {
            Some(r) => {
                first.get_or_insert(r);
            }
            None => return,
        }
    }
    let Some((at, rule, message, convert)) = first else { return };
    let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &ARGUMENT_TYPE_MISMATCH } else { rule };
    out.report(rule, at.text_range(), message, convert);
}

/// `a.x = 1` where `x` has a `private set` and the assignment is outside its class (or file,
/// for top-level properties).
fn invisible_setter(cx: &Cx, e: &Engine, target: &SyntaxNode, out: &mut Sink) {
    let name_ref = match target.kind() {
        SyntaxKind::NAME_REF => target.clone(),
        SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR => match target.last_child() {
            Some(l) if l.kind() == SyntaxKind::NAME_REF => l,
            _ => return,
        },
        _ => return,
    };
    let defs = e.resolve_name_ref(&name_ref);
    let [Def::Symbol { sym, .. }] = defs.as_slice() else { return };
    if sym.kind != SymbolKind::Property || !sym.modifiers.has(Modifiers::VAR) {
        return;
    }
    let crate::symbols::Origin::Source { file, .. } = &sym.origin else { return };
    let Some(decl) = super::markers::source_decl(cx, sym) else { return };
    let private_setter = decl.children().any(|a| {
        a.kind() == SyntaxKind::PROPERTY_ACCESSOR && ast::token(&a, SyntaxKind::IDENT).is_some_and(|t| t.text() == "set") && has_modifier(&a, "private")
    });
    if !private_setter {
        return;
    }
    // Visible in the declaring class (and what it contains), or the file for top-level ones.
    let owner = decl.ancestors().skip(1).find(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL | SyntaxKind::KOTLIN_FILE));
    let inside = file.as_path() == cx.path
        && owner.as_ref().is_some_and(|o| o.kind() == SyntaxKind::KOTLIN_FILE || o.text_range().contains_range(target.text_range()));
    if inside {
        return;
    }
    out.report(&INVISIBLE_SETTER, name_ref.text_range(), format!("Cannot assign to '{}': the setter is private.", sym.name), None);
}

/// `*array` where the array may be `null`.
fn spread(e: &Engine, arg: &SyntaxNode, out: &mut Sink) {
    let Some(star) = ast::token(arg, SyntaxKind::STAR) else { return };
    let Some(value) = arg.children().find(|c| c.kind() != SyntaxKind::VALUE_ARGUMENT_NAME) else { return };
    let Some(t) = certain_type(e, &value, true) else { return };
    if !matches!(&t, TypeRef::Named { nullable: true, fq: Some(fq), .. } if &**fq != "kotlin.Nothing") {
        return;
    }
    // The engine's smart casts must agree that it is still nullable.
    if !e.type_of(&value).is_some_and(|s| matches!(crate::analysis::ide::infer::strip(&s), TypeRef::Named { nullable: true, .. })) {
        return;
    }
    let fix = Fix::replace("Add non-null asserted (!!) call", TextRange::empty(value.text_range().end()), "!!");
    out.report(&SPREAD_OF_NULLABLE, TextRange::new(star.text_range().start(), value.text_range().end()), "The spread operator (*foo) may not be applied to an argument of nullable type.", Some(fix));
}

/// `::x.isInitialized` / `this::x.isInitialized` where `x` is not `lateinit`.
fn is_initialized(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let (Some(recv), Some(sel)) = (n.first_child(), n.last_child()) else { return };
    if recv.kind() != SyntaxKind::CALLABLE_REFERENCE || sel.kind() != SyntaxKind::NAME_REF || sel.text() != "isInitialized" {
        return;
    }
    let Some(prop) = recv.children().filter(|c| c.kind() == SyntaxKind::NAME_REF).last() else { return };
    let defs = e.resolve_name_ref(&prop);
    let [Def::Symbol { sym, .. }] = defs.as_slice() else { return };
    if sym.kind != SymbolKind::Property || sym.modifiers.has(Modifiers::LATEINIT) || !matches!(sym.origin, crate::symbols::Origin::Source { .. }) {
        return;
    }
    out.report(&LATEINIT_INTRINSIC_CALL_ON_NON_LATEINIT, sel.text_range(), "This declaration can only be called on a reference to a lateinit property.", None);
}
