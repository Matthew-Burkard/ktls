//! `return`, `break`/`continue`: returns that do not match the function, and jumps outside
//! loops (definite assignment is in `dataflow`).

use super::super::*;
use super::expressions::type_mismatch;
use crate::analysis::ide::infer::Engine;
use crate::symbols::TypeRef;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR => outside_loop(&n, out),
            _ => {}
        }
    }
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants().filter(|n| matches!(n.kind(), SyntaxKind::RETURN_EXPR | SyntaxKind::FUN)) {
        if in_error(&n) {
            continue;
        }
        if n.kind() == SyntaxKind::RETURN_EXPR {
            return_value(e, &n, out);
        } else {
            expression_body(e, &n, out);
        }
    }
}

/// `fun f(): T = value` with a value of the wrong type.
fn expression_body(e: &Engine, f: &SyntaxNode, out: &mut Sink) {
    let Some(decl) = ast::FunDecl::cast(f.clone()) else { return };
    let (Some(t), Some(v)) = (decl.return_type(), initializer(f)) else { return };
    let ty = e.resolve_syntax_type(&t);
    if matches!(ty, TypeRef::Platform(_) | TypeRef::TypeParam { .. }) {
        return;
    }
    if let Some((rule, msg, _, convert)) = type_mismatch(e, &v, &ty, &t.syntax().text().to_string()) {
        let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &RETURN_TYPE_MISMATCH } else { rule };
        out.report_fixes(rule, v.text_range(), msg, convert.into_iter().collect());
    }
}

/// What a `return` (without label) returns from: the nearest function, accessor,
/// constructor or initializer (lambdas are crossed: their `return` is non-local).
fn return_target(ret: &SyntaxNode) -> Option<SyntaxNode> {
    ret.ancestors().skip(1).find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::CLASS_INITIALIZER | SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION))
}

/// `return` without a value in a function that returns one, a value returned from a
/// `Unit` function, and `return` in initializers.
fn return_value(e: &Engine, ret: &SyntaxNode, out: &mut Sink) {
    if ret.children().any(|c| matches!(c.kind(), SyntaxKind::LABEL | SyntaxKind::LABEL_QUALIFIER)) {
        return;
    }
    let value = ret.children().last();
    let Some(target) = return_target(ret) else { return };
    let at = ret.first_token().map_or(ret.text_range(), |t| t.text_range());
    match target.kind() {
        SyntaxKind::CLASS_INITIALIZER => out.report(&RETURN_NOT_ALLOWED, at, "'return' is not allowed in an initializer block.", None),
        SyntaxKind::SECONDARY_CONSTRUCTOR => {
            if let Some(v) = value {
                out.report(&RETURN_NOT_ALLOWED, v.text_range(), "A constructor cannot return a value.", None);
            }
        }
        SyntaxKind::FUN => {
            let Some(decl) = ast::FunDecl::cast(target.clone()) else { return };
            if decl.has_expression_body() {
                return;
            }
            match (decl.return_type(), value) {
                // `return` alone in a function with a (non-Unit) result.
                (Some(t), None) => {
                    let ty = e.resolve_syntax_type(&t);
                    let unit = matches!(ty.fq_name(), Some("kotlin.Unit"));
                    let known = matches!(&ty, TypeRef::Named { fq: Some(_), .. } | TypeRef::Function { .. });
                    if known && !unit {
                        out.report(&RETURN_TYPE_MISMATCH, at, format!("This function must return a value of type '{}'.", t.syntax().text()), None);
                    }
                }
                // A value of the wrong type.
                (Some(t), Some(v)) => {
                    let ty = e.resolve_syntax_type(&t);
                    if matches!(ty, TypeRef::Platform(_) | TypeRef::TypeParam { .. }) {
                        return;
                    }
                    if let Some((rule, msg, _, convert)) = type_mismatch(e, &v, &ty, &t.syntax().text().to_string()) {
                        let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &RETURN_TYPE_MISMATCH } else { rule };
                        out.report_fixes(rule, v.text_range(), msg, convert.into_iter().collect());
                    }
                }
                // A value from a block-bodied function without a result type (Unit).
                (None, Some(v)) => {
                    if let Some((rule, msg, _, _)) = type_mismatch(e, &v, &TypeRef::named("kotlin.Unit"), "Unit") {
                        let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &RETURN_TYPE_MISMATCH } else { rule };
                        let fix = decl.syntax().children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST).zip(e.type_of(&v)).map(|(l, t)| Fix::replace(format!("Specify return type '{}'", t.display()), TextRange::empty(l.text_range().end()), format!(": {}", t.display())));
                        out.report(rule, v.text_range(), msg, fix);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// `break` / `continue` (without label) not inside a loop of the same function.
fn outside_loop(jump: &SyntaxNode, out: &mut Sink) {
    if jump.children().any(|c| matches!(c.kind(), SyntaxKind::LABEL | SyntaxKind::LABEL_QUALIFIER)) {
        return;
    }
    for a in jump.ancestors().skip(1) {
        match a.kind() {
            SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR => return,
            // Inline lambdas may break out of an enclosing loop (Kotlin 2.2).
            SyntaxKind::LAMBDA_EXPR => return,
            SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::CLASS_INITIALIZER => break,
            _ => {}
        }
    }
    let kw = if jump.kind() == SyntaxKind::BREAK_EXPR { "break" } else { "continue" };
    let at = jump.first_token().map_or(jump.text_range(), |t| t.text_range());
    out.report(&BREAK_OR_CONTINUE_OUTSIDE_A_LOOP, at, format!("'{kw}' is only allowed inside a loop."), None);
}
