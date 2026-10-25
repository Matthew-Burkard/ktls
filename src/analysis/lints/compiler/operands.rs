//! Operators applied to operands their functions do not accept: `1 + "a"`, `true + 1`,
//! `-"a"`, `1 < "a"`.

use super::super::*;
use super::certain_type;
use super::expressions::{has_type_param, type_mismatch};
use crate::analysis::ide::infer::{Engine, Subst, fq_of, strip, substitute};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants().filter(|n| matches!(n.kind(), SyntaxKind::BINARY_EXPR | SyntaxKind::PREFIX_EXPR)) {
        if in_error(&n) || super::certain::in_unknown_lambda(e, &n) {
            continue;
        }
        if n.kind() == SyntaxKind::BINARY_EXPR {
            binary(e, &n, out);
        } else {
            prefix(e, &n, out);
        }
    }
}

/// The operand's non-null class type, when certain and agreed on by the engine (no smart
/// cast it does not model), for built-in or project classes (whose operator extensions
/// are all visible).
fn operand(e: &Engine, x: &SyntaxNode) -> Option<TypeRef> {
    let t = certain_type(e, x, true)?;
    let TypeRef::Named { fq: Some(fq), nullable: false, .. } = &t else { return None };
    if e.type_of(x).as_ref().map(strip).and_then(fq_of) != Some(&**fq) {
        return None;
    }
    let cls = e.classifier(fq)?;
    let builtin = matches!(&**fq, "kotlin.Int" | "kotlin.Long" | "kotlin.Short" | "kotlin.Byte" | "kotlin.Double" | "kotlin.Float" | "kotlin.Boolean" | "kotlin.Char" | "kotlin.String");
    let project = matches!(cls.symbol().origin, Origin::Source { .. }) && cls.symbol().kind == SymbolKind::Class;
    (builtin || project).then_some(t)
}

/// `operator` functions named `name` callable on `t` at `at`.
fn functions(e: &Engine, at: &SyntaxNode, t: &TypeRef, name: &str) -> Option<Vec<(Symbol, Subst)>> {
    let all: Vec<(Symbol, Subst)> = e.members_named(t, name).into_iter().chain(e.member_extensions(at, t, name)).chain(e.extensions(t, name)).collect();
    // A property of that name could be invoked, and functions without `operator` are
    // reported elsewhere; leave those alone.
    all.iter().all(|(s, _)| s.kind == SymbolKind::Function).then_some(())?;
    let (operators, others): (Vec<_>, Vec<_>) = all.into_iter().partition(|(s, _)| s.modifiers.has(Modifiers::OPERATOR));
    (!operators.is_empty() || others.is_empty()).then_some(operators)
}

fn binary(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(op) = binary_op(n) else { return };
    let name = match op.kind() {
        SyntaxKind::PLUS => "plus",
        SyntaxKind::MINUS => "minus",
        SyntaxKind::STAR => "times",
        SyntaxKind::SLASH => "div",
        SyntaxKind::PERCENT => "rem",
        SyntaxKind::LT | SyntaxKind::GT | SyntaxKind::LT_EQ | SyntaxKind::GT_EQ => "compareTo",
        _ => return,
    };
    let Some((l, r)) = binary_operands(n) else { return };
    let Some(lt) = operand(e, &l) else { return };
    let Some(candidates) = functions(e, n, &lt, name) else { return };
    let shown = super::certain::shown(&lt);
    if candidates.is_empty() {
        out.report(&UNRESOLVED_REFERENCE_EXPR, op.text_range(), format!("Unresolved reference '{name}' for operator '{}' on '{shown}'.", op.text()), None);
        return;
    }
    // The right operand's type must be certain and accepted by none.
    if certain_type(e, &r, true).is_none() {
        return;
    }
    let mut rejected = 0;
    for (sym, subst) in &candidates {
        let [param] = sym.params.as_slice() else { return };
        let mut subst = subst.clone();
        for tp in &sym.type_params {
            subst.remove(&tp.name);
        }
        let t = substitute(&e.resolve_declared(sym, &param.ty), &subst);
        if has_type_param(&t) || !matches!(t, TypeRef::Named { fq: Some(_), .. }) {
            return;
        }
        if type_mismatch(e, &r, &t, &t.display()).is_none() {
            return;
        }
        rejected += 1;
    }
    let signatures: Vec<String> = candidates.iter().take(3).map(|(s, _)| s.signature()).collect();
    let more = if rejected > 3 { format!("\n... and {} more", rejected - 3) } else { String::new() };
    out.report(&NONE_APPLICABLE, op.text_range(), format!("None of the following candidates is applicable for operator '{}':\n{}{more}", op.text(), signatures.join("\n")), None);
}

/// `-x` / `+x` on a type without `unaryMinus` / `unaryPlus`.
fn prefix(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(op) = binary_op(n) else { return };
    let name = match op.kind() {
        SyntaxKind::MINUS => "unaryMinus",
        SyntaxKind::PLUS => "unaryPlus",
        _ => return,
    };
    let Some(x) = n.children().find(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE) else { return };
    // `-1` is a literal.
    if literal_type_of(&x) {
        return;
    }
    let Some(t) = operand(e, &x) else { return };
    if functions(e, n, &t, name).is_some_and(|c| c.is_empty()) {
        out.report(&UNRESOLVED_REFERENCE_EXPR, op.text_range(), format!("Unresolved reference '{name}' for operator '{}' on '{}'.", op.text(), super::certain::shown(&t)), None);
    }
}

fn literal_type_of(x: &SyntaxNode) -> bool {
    x.kind() == SyntaxKind::LITERAL && x.first_token().is_some_and(|t| matches!(t.kind(), SyntaxKind::INT_LITERAL | SyntaxKind::FLOAT_LITERAL))
}
