//! Operators applied to operands their functions do not accept: `1 + "a"`, `true + 1`,
//! `-"a"`, `1 < "a"`, `"a"++`, `1 in 2`.

use super::super::*;
use super::certain_type;
use super::expressions::{has_type_param, type_mismatch};
use crate::analysis::ide::infer::{Engine, Subst, fq_of, strip, substitute};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants().filter(|n| matches!(n.kind(), SyntaxKind::BINARY_EXPR | SyntaxKind::PREFIX_EXPR | SyntaxKind::POSTFIX_EXPR)) {
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
    // `a += b`: `plusAssign`, or `plus` and an assignment.
    if let Some(op) = assignment_op(n) {
        let names: &[&str] = match op.kind() {
            SyntaxKind::PLUS_EQ => &["plusAssign", "plus"],
            SyntaxKind::MINUS_EQ => &["minusAssign", "minus"],
            SyntaxKind::STAR_EQ => &["timesAssign", "times"],
            SyntaxKind::SLASH_EQ => &["divAssign", "div"],
            SyntaxKind::PERCENT_EQ => &["remAssign", "rem"],
            _ => return,
        };
        let (Some(l), Some(r)) = (n.first_child(), n.children().last()) else { return };
        if matches!(op.kind(), SyntaxKind::PLUS_EQ | SyntaxKind::MINUS_EQ) {
            collection_element(e, n, &op, &l, &r, out);
        }
        applicable(e, n, &op, &l, &r, names, out);
        return;
    }
    let Some(op) = binary_op(n) else { return };
    let name = match op.kind() {
        SyntaxKind::PLUS => "plus",
        SyntaxKind::MINUS => "minus",
        SyntaxKind::STAR => "times",
        SyntaxKind::SLASH => "div",
        SyntaxKind::PERCENT => "rem",
        SyntaxKind::RANGE => "rangeTo",
        SyntaxKind::RANGE_UNTIL => "rangeUntil",
        SyntaxKind::LT | SyntaxKind::GT | SyntaxKind::LT_EQ | SyntaxKind::GT_EQ => "compareTo",
        SyntaxKind::IN_KW | SyntaxKind::NOT_IN_KW => "contains",
        _ => return,
    };
    let Some((l, r)) = binary_operands(n) else { return };
    // `x in c` calls `c.contains(x)`.
    if name == "contains" {
        applicable(e, n, &op, &r, &l, &[name], out);
    } else {
        applicable(e, n, &op, &l, &r, &[name], out);
    }
}

/// Operator `op` on `l` and `r`, calling a function of one of `names`: unresolved when
/// none exists, inapplicable when every one rejects `r`'s certain type.
fn applicable(e: &Engine, n: &SyntaxNode, op: &SyntaxToken, l: &SyntaxNode, r: &SyntaxNode, names: &[&str], out: &mut Sink) {
    let Some(lt) = operand(e, l) else { return };
    let mut candidates = Vec::new();
    for name in names {
        let Some(found) = functions(e, n, &lt, name) else { return };
        candidates.extend(found);
    }
    let shown = super::certain::shown(&lt);
    if candidates.is_empty() {
        out.report(&UNRESOLVED_REFERENCE_EXPR, op.text_range(), format!("Unresolved reference '{}' for operator '{}' on '{shown}'.", names[0], op.text()), None);
        return;
    }
    // The right operand's type must be certain and accepted by none.
    let Some(rt) = certain_type(e, r, true) else { return };
    let mut rejected = 0;
    for (sym, subst) in &candidates {
        let [param] = sym.params.as_slice() else { return };
        // `T.rangeTo(that: T)` for `Comparable` `T`: unrelated classes have no common `T`.
        if !sym.type_params.is_empty() {
            let unrelated = matches!(sym.name.as_str(), "rangeTo" | "rangeUntil")
                && fq_of(&rt).is_some_and(|r| e.conforms(&TypeRef::named(r), fq_of(&lt).unwrap_or_default()) == Some(false) && e.conforms(&lt, r) == Some(false));
            if !unrelated {
                return;
            }
            rejected += 1;
            continue;
        }
        let t = substitute(&e.resolve_declared(sym, &param.ty), subst);
        if has_type_param(&t) || !matches!(t, TypeRef::Named { fq: Some(_), .. }) {
            return;
        }
        if type_mismatch(e, r, &t, &t.display()).is_none() {
            return;
        }
        rejected += 1;
    }
    let signatures: Vec<String> = candidates.iter().take(3).map(|(s, _)| s.signature()).collect();
    let more = if rejected > 3 { format!("\n... and {} more", rejected - 3) } else { String::new() };
    out.report(&NONE_APPLICABLE, op.text_range(), format!("None of the following candidates is applicable for operator '{}':\n{}{more}", op.text(), signatures.join("\n")), None);
}

/// `list += "s"` on a `MutableList<Int>`: the standard `plusAssign` / `minusAssign` of
/// mutable collections take an element of their type, or a collection, array or sequence.
fn collection_element(e: &Engine, n: &SyntaxNode, op: &SyntaxToken, l: &SyntaxNode, r: &SyntaxNode, out: &mut Sink) {
    let Some(lt) = certain_type(e, l, true) else { return };
    if !matches!(lt, TypeRef::Named { nullable: false, .. }) || has_type_param(&lt) || e.type_of(l).as_ref().map(strip).and_then(fq_of) != fq_of(&lt) {
        return;
    }
    let Some(element) = e.supertype_closure(&lt).into_iter().find_map(|st| match st {
        TypeRef::Named { fq: Some(fq), args, .. } if &*fq == "kotlin.collections.MutableCollection" => match args.as_slice() {
            [crate::symbols::TypeArg::Type { ty, .. }] => Some(ty.clone()),
            _ => None,
        },
        _ => None,
    }) else {
        return;
    };
    if has_type_param(&element) || !matches!(element, TypeRef::Named { fq: Some(_), .. }) {
        return;
    }
    // Only the standard library's operators: no other `plusAssign` / `plus` is in scope.
    let name = if op.kind() == SyntaxKind::PLUS_EQ { "plusAssign" } else { "minusAssign" };
    let all: Vec<(Symbol, Subst)> = e.members_named(&lt, name).into_iter().chain(e.member_extensions(n, &lt, name)).chain(e.extensions(&lt, name)).collect();
    if all.is_empty() || all.iter().any(|(s, _)| !s.fq_name.starts_with("kotlin.collections.")) {
        return;
    }
    let Some(rt) = certain_type(e, r, true) else { return };
    let Some(rfq) = fq_of(&rt) else { return };
    let collection = ["kotlin.collections.Iterable", "kotlin.Array", "kotlin.sequences.Sequence"].iter().any(|c| e.conforms(&rt, c) != Some(false))
        || rfq.starts_with("kotlin.") && rfq.ends_with("Array");
    if collection {
        return;
    }
    if type_mismatch(e, r, &element, &super::certain::shown(&element)).is_some() {
        let shown = super::certain::shown(&rt);
        out.report(&NONE_APPLICABLE, op.text_range(), format!("None of the following candidates is applicable for operator '{}': '{shown}' is not an element of '{}'.", op.text(), super::certain::shown(&lt)), None);
    }
}

/// `-x` / `+x` / `x++` / `x--` on a type without `unaryMinus` / `unaryPlus` / `inc` / `dec`.
fn prefix(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(op) = binary_op(n) else { return };
    let name = match op.kind() {
        SyntaxKind::MINUS => "unaryMinus",
        SyntaxKind::PLUS => "unaryPlus",
        SyntaxKind::PLUS2 => "inc",
        SyntaxKind::MINUS2 => "dec",
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
