//! Uses a declaration does not allow: indexed assignment without a `set` operator,
//! assignment through a private setter, `isInitialized` on a property that is not
//! `lateinit`, and spreading a nullable array.

use super::super::*;
use super::certain_type;
use crate::analysis::ide::infer::{Def, Engine, fq_of};
use crate::symbols::{Modifiers, SymbolKind, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants() {
        let relevant = matches!(n.kind(), SyntaxKind::BINARY_EXPR | SyntaxKind::VALUE_ARGUMENT | SyntaxKind::DOT_QUALIFIED_EXPR);
        if !relevant || in_error(&n) || super::certain::in_unknown_lambda(e, &n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::BINARY_EXPR if assignment_op(&n).is_some() => {
                let Some(target) = n.first_child() else { continue };
                if target.kind() == SyntaxKind::INDEX_EXPR {
                    no_set(e, &target, out);
                } else if assignment_op(&n).is_some_and(|o| o.kind() == SyntaxKind::EQ) {
                    invisible_setter(cx, e, &target, out);
                }
            }
            SyntaxKind::VALUE_ARGUMENT => spread(e, &n, out),
            SyntaxKind::DOT_QUALIFIED_EXPR => is_initialized(e, &n, out),
            _ => {}
        }
    }
}

/// `list[0] = 1` on a read-only `List` (or any type without a `set` operator).
fn no_set(e: &Engine, index: &SyntaxNode, out: &mut Sink) {
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
    if !e.members_named(&t, "set").is_empty() || !e.extensions(&t, "set").is_empty() || !e.member_extensions(index, &t, "set").is_empty() {
        return;
    }
    let shown = super::certain::shown(&t);
    out.report(&NO_SET_METHOD, index.text_range(), format!("No 'set' operator for indexed assignment on '{shown}'."), None);
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
