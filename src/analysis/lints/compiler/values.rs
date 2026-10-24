//! Expressions whose value is known to be pointless or wrong: division by a zero constant,
//! `?: null`, `null` branches for non-null subjects, `==` between arrays, `is` checks
//! against erased type arguments.

use super::super::*;
use super::certain_type;
use crate::analysis::ide::infer::{Engine, fq_of};
use crate::symbols::{TypeArg, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::BINARY_EXPR => binary(e, &n, out),
            SyntaxKind::WHEN_EXPR => null_in_when(e, &n, out),
            SyntaxKind::IS_EXPR => erased_check(e, &n, out),
            _ => {}
        }
    }
}

const INTEGRAL: &[&str] = &["kotlin.Int", "kotlin.Long", "kotlin.Short", "kotlin.Byte"];

fn is_zero(n: &SyntaxNode) -> bool {
    n.kind() == SyntaxKind::LITERAL && n.first_token().is_some_and(|t| t.kind() == SyntaxKind::INT_LITERAL && matches!(t.text().to_ascii_lowercase().as_str(), "0" | "0l" | "0x0" | "0b0"))
}

fn is_null(n: &SyntaxNode) -> bool {
    n.kind() == SyntaxKind::LITERAL && n.first_token().is_some_and(|t| t.kind() == SyntaxKind::NULL_KW)
}

fn non_null_class(t: &TypeRef) -> Option<&str> {
    match crate::analysis::ide::infer::strip(t) {
        TypeRef::Named { fq: Some(fq), nullable: false, .. } => Some(fq),
        _ => None,
    }
}

fn is_array(fq: &str) -> bool {
    fq == "kotlin.Array" || fq.starts_with("kotlin.") && fq.ends_with("Array") && !fq[7..].contains('.')
}

fn binary(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let (Some(op), Some((l, r))) = (binary_op(n).or_else(|| assignment_op(n)), binary_operands(n)) else { return };
    match op.kind() {
        SyntaxKind::SLASH | SyntaxKind::PERCENT | SyntaxKind::SLASH_EQ | SyntaxKind::PERCENT_EQ if is_zero(&r) => {
            if certain_type(e, &l, true).as_ref().and_then(non_null_class).is_some_and(|fq| INTEGRAL.contains(&fq)) {
                out.report(&DIVISION_BY_ZERO, r.text_range(), "Division by zero.", None);
            }
        }
        SyntaxKind::ELVIS if is_null(&r) => {
            let del = TextRange::new(l.text_range().end(), r.text_range().end());
            out.report(&USELESS_ELVIS_RIGHT_IS_NULL, TextRange::new(op.text_range().start(), r.text_range().end()), "Right-hand side of elvis is useless ('null').", Some(Fix::delete("Remove useless elvis operator", del)));
        }
        SyntaxKind::EQ2 | SyntaxKind::NOT_EQ => {
            let array = |x: &SyntaxNode| certain_type(e, x, true).as_ref().and_then(non_null_class).is_some_and(is_array);
            if array(&l) && array(&r) {
                let not = if op.kind() == SyntaxKind::NOT_EQ { "!" } else { "" };
                let recv = if is_primary(&l) { l.text().to_string() } else { format!("({})", l.text()) };
                let fix = Fix::replace("Replace with 'contentEquals'", n.text_range(), format!("{not}{recv}.contentEquals({})", r.text()));
                out.report(&ARRAY_EQUALITY_OPERATOR_CAN_BE_REPLACED_WITH_EQUALS, op.text_range(), "Dangerous array comparison: arrays are compared by identity. Replace with 'contentEquals'.", Some(fix));
            }
        }
        _ => {}
    }
}

/// `when (x) { null -> … }` with `x` of a non-null type.
fn null_in_when(e: &Engine, w: &SyntaxNode, out: &mut Sink) {
    let Some(subject) = w.children().find(|c| c.kind() == SyntaxKind::WHEN_SUBJECT).and_then(|s| s.children().last()) else { return };
    if subject.kind() == SyntaxKind::PROPERTY {
        return;
    }
    let Some(t) = certain_type(e, &subject, true) else { return };
    if non_null_class(&t).is_none() {
        return;
    }
    for c in w.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY).flat_map(|en| en.children().filter(|c| c.kind() == SyntaxKind::WHEN_CONDITION).collect::<Vec<_>>()) {
        if let Some(v) = c.children().next().filter(is_null) {
            out.report(&SENSELESS_NULL_IN_WHEN, v.text_range(), format!("Expression under 'when' is never equal to null (its type '{}' is not nullable).", t.display()), None);
        }
    }
}

/// `x is List<String>`: type arguments that the value's type does not determine cannot be
/// checked at run time. Reported when the value's class has no type parameters (so nothing
/// carries over) and the target has a type argument other than `*`.
fn erased_check(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(value) = n.first_child() else { return };
    let Some(tr) = n.children().find_map(ast::TypeRef::cast) else { return };
    let target = e.resolve_syntax_type(&tr);
    let TypeRef::Named { args, fq: Some(target_fq), .. } = crate::analysis::ide::infer::strip(&target) else { return };
    if !args.iter().any(|a| matches!(a, TypeArg::Type { .. })) || is_array(target_fq) {
        return;
    }
    // Reified type parameters of inline functions are fine, but they never reach this:
    // only concrete arguments count.
    if args.iter().any(|a| matches!(a, TypeArg::Type { ty, .. } if matches!(crate::analysis::ide::infer::strip(ty), TypeRef::TypeParam { .. }))) {
        return;
    }
    let Some(t) = certain_type(e, &value, false) else { return };
    let Some(fq) = fq_of(&t) else { return };
    let Some(c) = e.classifier(fq) else { return };
    if !c.symbol().type_params.is_empty() {
        return;
    }
    // Every supertype must be non-generic too (a `class X : List<String>` fixes them).
    if e.supertype_closure(&t).iter().any(|s| matches!(crate::analysis::ide::infer::strip(s), TypeRef::Named { args, .. } if !args.is_empty())) {
        return;
    }
    out.report(&CANNOT_CHECK_FOR_ERASED, tr.syntax().text_range(), format!("Cannot check for instance of erased type: {}", tr.syntax().text()), None);
}
