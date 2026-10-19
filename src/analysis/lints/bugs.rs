//! Probable bugs.

use super::*;
use crate::analysis::ide::infer::{Def, LocalKind, fq_of};
use crate::symbols::SymbolKind;

const ARRAY_TYPES: &[&str] = &[
    "Array", "IntArray", "LongArray", "ShortArray", "ByteArray", "CharArray", "FloatArray", "DoubleArray", "BooleanArray", "UIntArray", "ULongArray",
    "UShortArray", "UByteArray",
];

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::BINARY_EXPR => {
                nan_equality(&n, out);
                self_assignment(cx, &n, out);
            }
            SyntaxKind::CLASS => {
                equals_hash_code(&n, out);
                array_in_data_class(&n, out);
            }
            SyntaxKind::CALL_EXPR => throwable_not_thrown(cx, &n, out),
            _ => {}
        }
    }
}

fn is_nan(n: &SyntaxNode) -> bool {
    let t: String = n.text().to_string().chars().filter(|c| !c.is_whitespace()).collect();
    matches!(t.as_str(), "Double.NaN" | "Float.NaN" | "kotlin.Double.NaN" | "kotlin.Float.NaN")
}

fn nan_equality(n: &SyntaxNode, out: &mut Sink) {
    let Some(op) = binary_op(n) else { return };
    let negated = match op.kind() {
        SyntaxKind::EQ2 | SyntaxKind::EQ3 => false,
        SyntaxKind::NOT_EQ | SyntaxKind::NOT_EQ2 => true,
        _ => return,
    };
    let Some((l, r)) = binary_operands(n) else { return };
    let other = if is_nan(&r) {
        l
    } else if is_nan(&l) {
        r
    } else {
        return;
    };
    let text = format!("{}{}.isNaN()", if negated { "!" } else { "" }, operand(&other));
    let always = if negated { "true" } else { "false" };
    out.report(&NAN_EQUALITY, n.text_range(), format!("Equality check with NaN is always {always}; use 'isNaN()'"), Some(Fix::replace("Replace with 'isNaN()'", n.text_range(), text)));
}

fn self_assignment(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !assignment_op(n).is_some_and(|op| op.kind() == SyntaxKind::EQ) {
        return;
    }
    let Some((l, r)) = binary_operands(n) else { return };
    if l.kind() != SyntaxKind::NAME_REF || l.text() != r.text() || r.kind() != SyntaxKind::NAME_REF {
        return;
    }
    // Only local variables: assigning a property to itself may run a custom setter.
    if !cx.on(&SELF_ASSIGNMENT) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let local_var = e.resolve_name_ref(&l).iter().any(|d| matches!(d, Def::Local(l) if l.kind == LocalKind::Var));
    if local_var {
        let fix = n.parent().filter(|p| p.kind() == SyntaxKind::BLOCK).map(|_| Fix::delete("Remove self assignment", deletion_range(cx.text, n.text_range())));
        out.report(&SELF_ASSIGNMENT, n.text_range(), format!("Variable '{}' is assigned to itself", l.text()), fix);
    }
}

fn members(class: &SyntaxNode) -> Vec<SyntaxNode> {
    class.children().filter(|c| c.kind() == SyntaxKind::CLASS_BODY).flat_map(|b| b.children().collect::<Vec<_>>()).collect()
}

/// `override fun equals(other: Any?)` / `override fun hashCode()`.
fn overrides_named(class: &SyntaxNode, name: &str, params: usize) -> bool {
    members(class).iter().any(|m| {
        m.kind() == SyntaxKind::FUN
            && has_modifier(m, "override")
            && name_token(m).is_some_and(|t| t.text() == name)
            && ast::FunDecl::cast(m.clone()).is_some_and(|f| f.parameters().len() == params)
    })
}

fn equals_hash_code(class: &SyntaxNode, out: &mut Sink) {
    if is_interface(class) {
        return;
    }
    let equals = overrides_named(class, "equals", 1);
    let hash = overrides_named(class, "hashCode", 0);
    let Some(name) = name_token(class) else { return };
    let message = match (equals, hash) {
        (true, false) => "Class overrides 'equals()' but not 'hashCode()'",
        (false, true) => "Class overrides 'hashCode()' but not 'equals()'",
        _ => return,
    };
    out.report(&EQUALS_OR_HASH_CODE, name.text_range(), message, None);
}

fn array_in_data_class(class: &SyntaxNode, out: &mut Sink) {
    if !has_modifier(class, "data") || overrides_named(class, "equals", 1) && overrides_named(class, "hashCode", 0) {
        return;
    }
    let Some(ctor) = ast::ClassDecl::cast(class.clone()).and_then(|c| c.primary_constructor()) else { return };
    for p in ctor.parameters() {
        let Some(ty) = p.type_ref() else { continue };
        if p.property_keyword().is_none() {
            continue;
        }
        let inner = ty.inner();
        let user = match &inner {
            Some(t) if t.kind() == SyntaxKind::NULLABLE_TYPE => t.descendants().find(|d| d.kind() == SyntaxKind::USER_TYPE),
            Some(t) if t.kind() == SyntaxKind::USER_TYPE => Some(t.clone()),
            _ => None,
        };
        let Some(ut) = user.and_then(ast::UserType::cast) else { continue };
        let last = ut.segments().last().and_then(|s| s.name_token()).map(|t| ast::ident_text(&t)).unwrap_or_default();
        if ARRAY_TYPES.contains(&last.as_str()) {
            out.report(&ARRAY_IN_DATA_CLASS, ty.syntax().text_range(), "Array property in a data class: override 'equals()' and 'hashCode()' to compare contents", None);
        }
    }
}

/// `IllegalStateException("...")` as a statement, without `throw`.
fn throwable_not_thrown(cx: &Cx, call: &SyntaxNode, out: &mut Sink) {
    // The call itself or `pkg.Exception(...)` as a statement.
    let stmt = match call.parent() {
        Some(p) if p.kind() == SyntaxKind::DOT_QUALIFIED_EXPR && p.children().last().as_ref() == Some(call) => p,
        _ => call.clone(),
    };
    let Some(block) = stmt.parent().filter(|p| p.kind() == SyntaxKind::BLOCK) else { return };
    // The last statement of a lambda or an expression-valued block is its value.
    let last = block.children().last().as_ref() == Some(&stmt);
    let statement_block = block.parent().is_some_and(|p| {
        matches!(p.kind(), SyntaxKind::FUN | SyntaxKind::BODY | SyntaxKind::CLASS_INITIALIZER | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::FINALLY_CLAUSE)
    });
    if last && !statement_block {
        return;
    }
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let name = callee.text().to_string();
    // Cheap filter before asking for types.
    if !(name.ends_with("Exception") || name.ends_with("Error") || name.ends_with("Throwable")) || !cx.on(&THROWABLE_NOT_THROWN) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let defs = e.resolve_name_ref(&callee);
    let constructor = defs.iter().any(|d| matches!(d, Def::Symbol { sym, .. } if matches!(sym.kind, SymbolKind::Constructor | SymbolKind::Class | SymbolKind::TypeAlias)));
    if !constructor {
        return;
    }
    let Some(t) = e.type_of(&stmt) else { return };
    let throwable = e.supertype_closure(&t).iter().any(|s| matches!(fq_of(s), Some("kotlin.Throwable" | "java.lang.Throwable")));
    if throwable {
        out.report(
            &THROWABLE_NOT_THROWN,
            callee.text_range(),
            format!("'{name}' is created but not thrown"),
            Some(Fix::replace("Add 'throw'", TextRange::empty(stmt.text_range().start()), "throw ")),
        );
    }
}
