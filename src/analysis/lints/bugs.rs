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
                collection_reassignment(cx, &n, out);
            }
            SyntaxKind::CLASS => {
                equals_hash_code(&n, out);
                array_in_data_class(&n, out);
                leaking_this(cx, &n, out);
            }
            SyntaxKind::CALL_EXPR => {
                throwable_not_thrown(cx, &n, out);
                unused_copy(cx, &n, out);
                coroutine_calls(cx, &n, out);
            }
            SyntaxKind::DESTRUCTURING_DECLARATION => destructuring_names(cx, &n, out),
            SyntaxKind::FUN => {
                main_return_unit(cx, &n, out);
                lambda_expression_body(cx, &n, out);
                implicit_nullable_nothing(cx, &n, out);
            }
            SyntaxKind::PROPERTY => implicit_nullable_nothing(cx, &n, out),
            SyntaxKind::DELEGATED_SUPER_TYPE_ENTRY => delegation_to_var(cx, &n, out),
            SyntaxKind::PREFIX_EXPR => wrap_unary(cx, &n, out),
            SyntaxKind::NAME_REF if n.text() == "GlobalScope" => global_scope(cx, &n, out),
            SyntaxKind::PROPERTY_ACCESSOR => recursive_accessor(cx, &n, out),
            SyntaxKind::NAME_REF if n.text() == "it" => nested_it(cx, &n, out),
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

/// Code in `inner` that runs as part of `outer` (not in a nested function, class or lambda).
fn runs_directly(inner: &SyntaxNode, outer: &SyntaxNode) -> bool {
    inner.ancestors().take_while(|a| a != outer).all(|a| {
        !matches!(a.kind(), SyntaxKind::LAMBDA_EXPR | SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::CLASS | SyntaxKind::OBJECT_LITERAL | SyntaxKind::OBJECT_DECLARATION)
    })
}

/// `val x get() = x`, `var x set(v) { x = v }`: the accessor calls itself.
fn recursive_accessor(cx: &Cx, acc: &SyntaxNode, out: &mut Sink) {
    let Some(prop) = acc.parent().filter(|p| p.kind() == SyntaxKind::PROPERTY) else { return };
    let (Some(name), Some(kw)) = (name_token(&prop), acc.first_token()) else { return };
    if !cx.on(&RECURSIVE_ACCESSOR) {
        return;
    }
    let name = ast::ident_text(&name);
    let getter = match kw.text() {
        "get" => true,
        "set" => false,
        _ => return,
    };
    let decl = ast::PropertyDecl::cast(prop.clone()).unwrap();
    // `field` needs a backing field: an initializer, and no receiver or interface.
    let field_ok = decl.receiver().is_none() && initializer(&prop).is_some() && !owner(&prop).is_some_and(|o| is_interface(&o));
    for nr in acc.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF && d.text() == name.as_str()) {
        if !runs_directly(&nr, acc) {
            continue;
        }
        // The property itself: `x` or `this.x` (not a call, a reference or another receiver's).
        let parent = nr.parent();
        let expr = match &parent {
            Some(p) if matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.first_child().as_ref() != Some(&nr) => {
                if p.kind() == SyntaxKind::SAFE_QUALIFIED_EXPR || !p.first_child().is_some_and(|r| r.kind() == SyntaxKind::THIS_EXPR && r.text() == "this") {
                    continue;
                }
                p.clone()
            }
            Some(p) if matches!(p.kind(), SyntaxKind::CALL_EXPR | SyntaxKind::CALLABLE_REFERENCE | SyntaxKind::VALUE_ARGUMENT_NAME) => continue,
            _ => nr.clone(),
        };
        let assigned = expr.parent().is_some_and(|p| assignment_op(&p).is_some() && p.first_child().as_ref() == Some(&expr));
        if getter == assigned {
            continue;
        }
        if let Some(e) = cx.engine() {
            if !e.resolve_name_ref(&nr).iter().any(|d| matches!(d, Def::Symbol { sym, .. } if sym.kind == SymbolKind::Property)) {
                continue;
            }
        } else if acc.descendants().any(|d| d.kind() == SyntaxKind::VALUE_PARAMETER && name_token(&d).is_some_and(|t| ast::ident_text(&t) == name)) {
            continue;
        }
        let fix = field_ok.then(|| Fix::replace("Replace with 'field'", expr.text_range(), "field"));
        let message = if getter { "Recursive property accessor: the getter reads its own property" } else { "Recursive property accessor: the setter assigns its own property" };
        out.report(&RECURSIVE_ACCESSOR, expr.text_range(), message, fix);
    }
}

/// The lambda whose implicit `it` a reference `it` means, and whether that lambda takes
/// exactly one parameter.
fn it_lambda(e: &crate::analysis::ide::infer::Engine, it: &SyntaxNode) -> Option<SyntaxNode> {
    let defs = e.resolve_name_ref(it);
    let [Def::Local(l)] = defs.as_slice() else { return None };
    if l.kind != LocalKind::It {
        return None;
    }
    let lambda = it.ancestors().find(|a| a.kind() == SyntaxKind::LAMBDA_EXPR && a.first_token().is_some_and(|t| t.text_range() == l.name_range))?;
    e.lambda_signature(&lambda).is_some_and(|(_, ps)| ps.len() == 1).then_some(lambda)
}

/// `it` of a lambda nested in another lambda whose `it` is used too.
fn nested_it(cx: &Cx, it: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&NESTED_IT) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let Some(inner) = it_lambda(e, it) else { return };
    // `it.let { … it … }`: the scope function on the outer `it` takes its place on purpose.
    let call = inner.parent().filter(|p| p.kind() == SyntaxKind::LAMBDA_ARGUMENT).and_then(|a| a.parent()).filter(|c| c.kind() == SyntaxKind::CALL_EXPR);
    if let Some(call) = call
        && let Some(q) = call.parent().filter(|q| matches!(q.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && q.last_child().as_ref() == Some(&call))
        && q.first_child().is_some_and(|r| r.text() == "it")
        && let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF)
        && e.resolve_name_ref(&callee).iter().any(|d| matches!(d, Def::Symbol { sym, .. } if matches!(sym.fq_name.as_str(), "kotlin.let" | "kotlin.also" | "kotlin.takeIf" | "kotlin.takeUnless")))
    {
        return;
    }
    // The enclosing lambda, in the same function.
    let Some(outer) = inner.ancestors().skip(1).take_while(|a| !matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::CLASS | SyntaxKind::OBJECT_LITERAL)).find(|a| a.kind() == SyntaxKind::LAMBDA_EXPR) else { return };
    if outer.children().any(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST) || e.lambda_signature(&outer).is_none_or(|(_, ps)| ps.len() != 1) {
        return;
    }
    let outer_used = outer
        .descendants()
        .filter(|d| d.kind() == SyntaxKind::NAME_REF && d.text() == "it" && !inner.text_range().contains_range(d.text_range()))
        .any(|d| it_lambda(e, &d).as_ref() == Some(&outer));
    if outer_used {
        out.report(&NESTED_IT, it.text_range(), "Implicit parameter 'it' of the enclosing lambda is shadowed", None);
    }
}

/// A top-level `fun main()` (or `main(args: Array<String>)`) whose result is not `Unit`,
/// typically `fun main() = runBlocking { … }` ending in a value: it is no entry point.
fn main_return_unit(cx: &Cx, f: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&MAIN_RETURN_UNIT) || f.parent().is_none_or(|p| p.kind() != SyntaxKind::KOTLIN_FILE) {
        return;
    }
    let Some(name) = name_token(f).filter(|t| t.text() == "main") else { return };
    let Some(decl) = ast::FunDecl::cast(f.clone()) else { return };
    if decl.receiver().is_some() || !decl.type_parameters().is_empty() {
        return;
    }
    let params = decl.parameters();
    let entry_params = match params.as_slice() {
        [] => true,
        [p] => ast::child::<ast::TypeRef>(p.syntax()).is_some_and(|t| t.syntax().text().to_string().replace(' ', "") == "Array<String>"),
        _ => false,
    };
    if !entry_params {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let result = match decl.return_type() {
        Some(t) => e.resolve_syntax_type(&t),
        None if decl.has_expression_body() => {
            let Some(eq) = ast::token(f, SyntaxKind::EQ) else { return };
            let Some(body) = f.children().find(|c| c.text_range().start() >= eq.text_range().end()) else { return };
            let Some(t) = crate::analysis::lints::certain_type(e, &body, false) else { return };
            t
        }
        None => return,
    };
    let Some(fq) = crate::analysis::ide::infer::fq_of(&result) else { return };
    if fq == "kotlin.Unit" {
        return;
    }
    out.report(&MAIN_RETURN_UNIT, name.text_range(), format!("'main' should return 'Unit' (it returns '{}'), or it is not an entry point", result.display()), None);
}

/// The expression after `=` in a declaration.
pub(super) fn expression_body(decl: &SyntaxNode) -> Option<SyntaxNode> {
    let eq = ast::token(decl, SyntaxKind::EQ)?;
    decl.children().find(|c| c.text_range().start() >= eq.text_range().end())
}

/// `fun f() = { … }`: the function returns the lambda.
fn lambda_expression_body(cx: &Cx, f: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&LAMBDA_EXPRESSION_BODY) {
        return;
    }
    let Some(decl) = ast::FunDecl::cast(f.clone()) else { return };
    if decl.return_type().is_some() {
        return;
    }
    let Some(body) = expression_body(f).filter(|b| b.kind() == SyntaxKind::LAMBDA_EXPR) else { return };
    let Some(eq) = ast::token(f, SyntaxKind::EQ) else { return };
    let fix = (!body.children().any(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST)).then(|| Fix::delete("Remove '=' (run the block)", TextRange::new(eq.text_range().start(), body.text_range().start())));
    out.report(&LAMBDA_EXPRESSION_BODY, eq.text_range(), "Function returns a lambda: '= { … }' is not a block body", fix);
}

/// `var x = null`, or an open/override `fun f() = null`: implicitly of type `Nothing?`.
fn implicit_nullable_nothing(cx: &Cx, d: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&IMPLICIT_NULLABLE_NOTHING) || ast::child::<ast::TypeRef>(d).is_some_and(|t| d.kind() == SyntaxKind::PROPERTY || ast::FunDecl::cast(d.clone()).and_then(|f| f.return_type()).is_some_and(|r| r.syntax() == t.syntax())) {
        return;
    }
    let Some(body) = expression_body(d) else { return };
    if body.kind() != SyntaxKind::LITERAL || body.first_token().is_none_or(|t| t.kind() != SyntaxKind::NULL_KW) {
        return;
    }
    let applies = match d.kind() {
        SyntaxKind::PROPERTY => ast::PropertyDecl::cast(d.clone()).is_some_and(|p| p.is_var() && p.type_ref().is_none()) || (has_modifier(d, "open") || has_modifier(d, "override")) && ast::PropertyDecl::cast(d.clone()).is_some_and(|p| p.type_ref().is_none()),
        _ => (has_modifier(d, "open") || has_modifier(d, "override")) && ast::FunDecl::cast(d.clone()).is_some_and(|f| f.return_type().is_none()),
    };
    if !applies {
        return;
    }
    let Some(name) = name_token(d) else { return };
    out.report(&IMPLICIT_NULLABLE_NOTHING, name.text_range(), "Implicit 'Nothing?' type", None);
}

/// `class A(var x: I) : I by x`: later assignments to `x` do not change the delegate.
fn delegation_to_var(cx: &Cx, entry: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&DELEGATION_TO_VAR) {
        return;
    }
    let Some(target) = entry.children().last().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let Some(class) = entry.ancestors().find(|a| a.kind() == SyntaxKind::CLASS) else { return };
    let Some(ctor) = class.children().find(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR) else { return };
    let param = ctor.descendants().filter(|d| d.kind() == SyntaxKind::VALUE_PARAMETER).find(|p| name_token(p).is_some_and(|t| target.text() == t.text()));
    let Some(var) = param.and_then(|p| ast::token(&p, SyntaxKind::VAR_KW)) else { return };
    out.report(&DELEGATION_TO_VAR, target.text_range(), "Delegating to 'var' property does not take its changes into account", Some(Fix::replace("Change 'var' to 'val'", var.text_range(), "val")));
}

/// `-1.inc()` is `-(1.inc())`.
fn wrap_unary(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&WRAP_UNARY_OPERATOR) {
        return;
    }
    let Some(op) = binary_op(n).filter(|o| matches!(o.kind(), SyntaxKind::MINUS | SyntaxKind::PLUS)) else { return };
    let Some(operand) = n.children().find(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE) else { return };
    if operand.kind() != SyntaxKind::DOT_QUALIFIED_EXPR {
        return;
    }
    let Some(lit) = operand.first_child().filter(|l| l.kind() == SyntaxKind::LITERAL && l.first_token().is_some_and(|t| matches!(t.kind(), SyntaxKind::INT_LITERAL | SyntaxKind::FLOAT_LITERAL))) else { return };
    let rest = &operand.text().to_string()[usize::from(lit.text_range().end() - operand.text_range().start())..];
    let fix = Fix::replace("Wrap unary operator and value with ()", n.text_range(), format!("({}{}){rest}", op.text(), lit.text()));
    out.report(&WRAP_UNARY_OPERATOR, n.text_range(), "Wrap unary operator and value with ()", Some(fix));
}

/// `list += x` on a `var list: List<T>`: a new list is created each time.
fn collection_reassignment(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let Some(op) = assignment_op(n).filter(|o| matches!(o.kind(), SyntaxKind::PLUS_EQ | SyntaxKind::MINUS_EQ)) else { return };
    if !cx.on(&SUSPICIOUS_COLLECTION_REASSIGNMENT) {
        return;
    }
    let Some((target, value)) = binary_operands(n) else { return };
    if target.kind() != SyntaxKind::NAME_REF {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let var = match e.resolve_name_ref(&target).as_slice() {
        [Def::Local(l)] => l.kind == LocalKind::Var,
        [Def::Symbol { sym, .. }] => sym.kind == SymbolKind::Property && sym.modifiers.has(crate::symbols::Modifiers::VAR),
        _ => false,
    };
    if !var {
        return;
    }
    let Some(t) = e.type_of(&target) else { return };
    let Some(kind) = fq_of(&t).and_then(|fq| match fq {
        "kotlin.collections.List" => Some("list"),
        "kotlin.collections.Set" => Some("set"),
        "kotlin.collections.Map" => Some("map"),
        _ => None,
    }) else {
        return;
    };
    let sign = if op.kind() == SyntaxKind::PLUS_EQ { "+" } else { "-" };
    let fix = Fix::replace("Replace with ordinary assignment", n.text_range(), format!("{} = {} {sign} {}", target.text(), target.text(), value.text()));
    out.report(&SUSPICIOUS_COLLECTION_REASSIGNMENT, op.text_range(), format!("'{}' creates a new {kind} under the hood", op.text()), Some(fix));
}

fn global_scope(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&GLOBAL_COROUTINE_USAGE) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    if matches!(e.resolve_name_ref(n).as_slice(), [Def::Symbol { sym, .. }] if sym.fq_name == "kotlinx.coroutines.GlobalScope") {
        out.report(&GLOBAL_COROUTINE_USAGE, n.text_range(), "Usage of a GlobalScope instance is highly discouraged", None);
    }
}

/// In a non-final class, open members used (or `this` passed on) while constructing:
/// initializer blocks, property initializers and constructor bodies.
fn leaking_this(cx: &Cx, class: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&LEAKING_THIS) || !(has_modifier(class, "open") || has_modifier(class, "abstract") || has_modifier(class, "sealed")) || ast::token(class, SyntaxKind::INTERFACE_KW).is_some() {
        return;
    }
    let Some(body) = class.children().find(|c| c.kind() == SyntaxKind::CLASS_BODY) else { return };
    let Some(e) = cx.engine() else { return };
    let mut regions: Vec<SyntaxNode> = Vec::new();
    for m in body.children() {
        match m.kind() {
            SyntaxKind::CLASS_INITIALIZER => regions.push(m.clone()),
            SyntaxKind::SECONDARY_CONSTRUCTOR => regions.extend(m.children().filter(|c| c.kind() == SyntaxKind::BLOCK)),
            SyntaxKind::PROPERTY if ast::PropertyDecl::cast(m.clone()).is_some_and(|p| !p.has_delegate()) => regions.extend(expression_body(&m)),
            _ => {}
        }
    }
    for region in regions {
        for n in region.descendants() {
            // Code in lambdas and local declarations may run later.
            if n.ancestors().take_while(|a| *a != region).any(|a| matches!(a.kind(), SyntaxKind::LAMBDA_EXPR | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::OBJECT_LITERAL | SyntaxKind::FUN | SyntaxKind::CLASS)) {
                continue;
            }
            match n.kind() {
                SyntaxKind::NAME_REF => {
                    // A bare name or `this.name`.
                    let qualified = n.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.first_child().as_ref() != Some(&n) && p.first_child().is_none_or(|f| f.kind() != SyntaxKind::THIS_EXPR));
                    let callee_of = n.parent().is_some_and(|p| p.kind() == SyntaxKind::CALL_EXPR) && n.parent().and_then(|p| p.parent()).is_some_and(|g| matches!(g.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && g.first_child().is_some_and(|f| f.kind() != SyntaxKind::THIS_EXPR) && g.first_child() != n.parent());
                    if qualified || callee_of {
                        continue;
                    }
                    let defs = e.resolve_name_ref(&n);
                    let [Def::Symbol { sym, .. }] = defs.as_slice() else { continue };
                    let open = sym.modifiers.has(crate::symbols::Modifiers::OPEN) || sym.modifiers.has(crate::symbols::Modifiers::ABSTRACT);
                    if !open || sym.receiver.is_some() || !matches!(sym.kind, SymbolKind::Function | SymbolKind::Property) || sym.modifiers.has(crate::symbols::Modifiers::FINAL) {
                        continue;
                    }
                    // Members of this class or its supertypes only.
                    let is_member = sym.fq_name.rsplit_once('.').is_some_and(|(owner, _)| e.classifier(owner).is_some_and(|c| c.symbol().kind.is_classifier()));
                    if !is_member {
                        continue;
                    }
                    let what = if sym.kind == SymbolKind::Function { "Calling non-final function" } else { "Accessing non-final property" };
                    out.report(&LEAKING_THIS, n.text_range(), format!("{what} {} in constructor", sym.name), None);
                }
                SyntaxKind::THIS_EXPR if n.text() == "this" && n.parent().is_some_and(|p| p.kind() == SyntaxKind::VALUE_ARGUMENT) => {
                    out.report(&LEAKING_THIS, n.text_range(), "Leaking 'this' in constructor of non-final class", None);
                }
                _ => {}
            }
        }
    }
}

/// `p.copy(age = 1)` as a statement: the copy is thrown away.
fn unused_copy(cx: &Cx, call: &SyntaxNode, out: &mut Sink) {
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF && c.text() == "copy") else { return };
    if !cx.on(&UNUSED_COPY_RESULT) {
        return;
    }
    let expr = match call.parent() {
        Some(p) if matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.last_child().as_ref() == Some(call) => p,
        _ => call.clone(),
    };
    // A statement, and not the value of a lambda or block expression.
    let Some(block) = expr.parent().filter(|b| b.kind() == SyntaxKind::BLOCK) else { return };
    if block.children().last().as_ref() == Some(&expr) && block.parent().is_some_and(|p| p.kind() != SyntaxKind::FUN && p.kind() != SyntaxKind::CLASS_INITIALIZER && p.kind() != SyntaxKind::SECONDARY_CONSTRUCTOR && p.kind() != SyntaxKind::PROPERTY_ACCESSOR) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let defs = e.resolve_name_ref(&callee);
    let [Def::Symbol { sym, .. }] = defs.as_slice() else { return };
    let Some((owner, _)) = sym.fq_name.rsplit_once('.') else { return };
    if e.classifier(owner).is_some_and(|c| c.symbol().modifiers.has(crate::symbols::Modifiers::DATA)) {
        out.report(&UNUSED_COPY_RESULT, callee.text_range(), "The result of a 'copy' call is unused", None);
    }
}

/// `val (age, name) = person` with `Person(val name: String, val age: Int)`.
fn destructuring_names(cx: &Cx, d: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&DESTRUCTURING_WRONG_NAME) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    // The destructured value: a declaration's initializer, a `for` loop range or a lambda
    // parameter's type are not followed (only declarations).
    let Some(decl) = d.parent().filter(|p| p.kind() == SyntaxKind::PROPERTY) else { return };
    let Some(value) = expression_body(&decl) else { return };
    let Some(t) = crate::analysis::lints::certain_type(e, &value, true) else { return };
    let Some(fq) = fq_of(&t) else { return };
    let Some(c) = e.classifier(fq) else { return };
    if !c.symbol().modifiers.has(crate::symbols::Modifiers::DATA) {
        return;
    }
    let Some(ctor) = e.constructors(fq).into_iter().find(|k| matches!(&k.origin, crate::symbols::Origin::Source { .. })) else { return };
    let components: Vec<String> = ctor.params.iter().map(|p| p.name.clone()).collect();
    for (i, entry) in d.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_ENTRY).enumerate() {
        let Some(tok) = name_token(&entry) else { continue };
        let name = ast::ident_text(&tok);
        if name == "_" || components.get(i).is_some_and(|c| *c == name) {
            continue;
        }
        if let Some(j) = components.iter().position(|c| *c == name) {
            out.report(&DESTRUCTURING_WRONG_NAME, tok.text_range(), format!("Variable name '{name}' matches the name of a different component (component{} is '{}')", j + 1, components.get(i).map_or("none", String::as_str)), None);
        }
    }
}


/// `runBlocking` in a suspend function, and `async { }` whose `Deferred` is dropped.
fn coroutine_calls(cx: &Cx, call: &SyntaxNode, out: &mut Sink) {
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let name = callee.text().to_string();
    if !matches!(name.as_str(), "runBlocking" | "async") || !cx.on(&RUN_BLOCKING_IN_SUSPEND) && !cx.on(&DEFERRED_RESULT_UNUSED) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let defs = e.resolve_name_ref(&callee);
    let [Def::Symbol { sym, .. }, ..] = defs.as_slice() else { return };
    if !sym.fq_name.starts_with("kotlinx.coroutines.") {
        return;
    }
    if name == "runBlocking" {
        // Directly in a suspend function (not in a lambda, which may run elsewhere).
        let owner = call.ancestors().skip(1).find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::LAMBDA_EXPR | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::CLASS_BODY));
        if owner.is_some_and(|o| o.kind() == SyntaxKind::FUN && has_modifier(&o, "suspend")) && cx.on(&RUN_BLOCKING_IN_SUSPEND) {
            out.report(&RUN_BLOCKING_IN_SUSPEND, callee.text_range(), "Blocking call 'runBlocking' in a suspend function: call the code directly or use 'withContext'", None);
        }
        return;
    }
    let expr = match call.parent() {
        Some(p) if matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.last_child().as_ref() == Some(call) => p,
        _ => call.clone(),
    };
    let Some(block) = expr.parent().filter(|b| b.kind() == SyntaxKind::BLOCK) else { return };
    // The last statement of a lambda is its value.
    if block.children().last().as_ref() == Some(&expr) && block.parent().is_some_and(|p| p.kind() == SyntaxKind::LAMBDA_EXPR) {
        return;
    }
    if cx.on(&DEFERRED_RESULT_UNUSED) {
        out.report(&DEFERRED_RESULT_UNUSED, callee.text_range(), "Deferred result is never used: use 'launch' or 'await' it", Some(Fix::replace("Replace with 'launch'", callee.text_range(), "launch")));
    }
}
