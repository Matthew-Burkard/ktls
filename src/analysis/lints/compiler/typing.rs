//! Types of assignments, conditions and casts: assigned values of the wrong type,
//! non-Boolean conditions, casts that are useless or can never succeed, and `is` checks
//! whose result is known.

use super::super::*;
use super::expressions::{literal_type, type_mismatch};
use super::certain::shown;
use super::when::condition_value;
use crate::analysis::ide::infer::{Def, Engine, fq_of, strip};
use crate::symbols::{Origin, SymbolKind, TypeArg, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants() {
        if cx.in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::BINARY_EXPR => {
                assignment(cx, e, &n, out);
                equality(e, &n, out);
                if binary_op(&n).is_some_and(|o| matches!(o.kind(), SyntaxKind::AMP2 | SyntaxKind::PIPE2)) {
                    for operand in n.children().filter(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE) {
                        condition(e, &operand, out);
                    }
                }
            }
            SyntaxKind::CONDITION if n.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::IF_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR)) => {
                if let Some(c) = n.first_child() {
                    condition(e, &c, out);
                }
            }
            SyntaxKind::PREFIX_EXPR if binary_op(&n).is_some_and(|o| o.kind() == SyntaxKind::EXCL) => {
                // `!` on a `Boolean?` is UNSAFE_OPERATOR_CALL.
                if let Some(c) = n.children().last()
                    && !certain(e, &c).is_some_and(|t| fq_of(&t) == Some("kotlin.Boolean"))
                {
                    condition(e, &c, out);
                }
            }
            SyntaxKind::BINARY_WITH_TYPE => cast(e, &n, out),
            SyntaxKind::IF_EXPR => if_as_expression(e, &n, out),
            SyntaxKind::LOOP_RANGE => loop_range(e, &n, out),
            SyntaxKind::PROPERTY_DELEGATE => delegate(e, &n, out),
            SyntaxKind::WHEN_EXPR => when_values(e, &n, out),
            SyntaxKind::IS_EXPR => is_check(e, &n, out),
            SyntaxKind::THROW_EXPR => thrown(e, &n, out),
            SyntaxKind::CATCH_CLAUSE => caught(e, &n, out),
            _ => {}
        }
    }
}

/// The type of `value` when it is certain: a literal or a certain-typed expression
/// (smart casts applied).
fn certain(e: &Engine, value: &SyntaxNode) -> Option<TypeRef> {
    if let Some(lit) = literal_type(value) {
        return Some(match lit {
            Some(fq) => TypeRef::named(fq),
            None => TypeRef::named("kotlin.Nothing").nullable(true),
        });
    }
    super::certain::certain_type(e, value, true)
}

fn nullable(t: &TypeRef) -> bool {
    matches!(t, TypeRef::Platform(_)) || matches!(strip(t), TypeRef::Named { nullable: true, .. } | TypeRef::TypeParam { nullable: true, .. } | TypeRef::Function { nullable: true, .. })
}

/// `x = value` where `x` is declared with a type.
fn assignment(cx: &Cx, e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    if !assignment_op(n).is_some_and(|op| op.kind() == SyntaxKind::EQ) {
        return;
    }
    let (Some(target), Some(value)) = (n.first_child(), n.children().last()) else { return };
    let name_ref = match target.kind() {
        SyntaxKind::NAME_REF => target.clone(),
        SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR => match target.children().last() {
            Some(s) if s.kind() == SyntaxKind::NAME_REF => s,
            _ => return,
        },
        _ => return,
    };
    let expected = match e.resolve_name_ref(&name_ref).as_slice() {
        [Def::Local(l)] => {
            let Some(decl) = crate::syntax::covering(&e.root, l.name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| matches!(a.kind(), SyntaxKind::PROPERTY | SyntaxKind::VALUE_PARAMETER)) else { return };
            match ast::child::<ast::TypeRef>(&decl) {
                Some(ty) => e.resolve_syntax_type(&ty),
                None => match inferred(e, &decl) {
                    Some(t) => t,
                    None => return,
                },
            }
        }
        [Def::Symbol { sym, subst }] if sym.kind == SymbolKind::Property => {
            // Source properties: only with a written type.
            if let Origin::Source { file, name_range, .. } = &sym.origin {
                let Some(f) = cx.snap.file(file) else { return };
                let root = f.parse().syntax();
                let decl = crate::syntax::covering(&root, *name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| matches!(a.kind(), SyntaxKind::PROPERTY | SyntaxKind::VALUE_PARAMETER));
                // Without a written type: that of an initializer in this file.
                if decl.as_ref().and_then(ast::child::<ast::TypeRef>).is_none() {
                    let Some(t) = decl.filter(|_| file.as_path() == cx.path).and_then(|d| inferred(e, &d)) else { return };
                    report_assignment(e, &value, &t, out);
                    return;
                }
            }
            let Some(t) = e.symbol_type(sym) else { return };
            crate::analysis::ide::infer::substitute(&t, subst)
        }
        _ => return,
    };
    report_assignment(e, &value, &expected, out);
}

/// The type of a variable or property without a written type: that of its initializer,
/// when certain (`var x = null` and delegates excluded).
fn inferred(e: &Engine, decl: &SyntaxNode) -> Option<TypeRef> {
    if decl.kind() != SyntaxKind::PROPERTY || decl.children().any(|c| c.kind() == SyntaxKind::PROPERTY_DELEGATE) {
        return None;
    }
    let t = super::certain::certain_type(e, &initializer(decl)?, true)?;
    matches!(&t, TypeRef::Named { fq: Some(fq), .. } if &**fq != "kotlin.Nothing").then_some(t)
}

fn report_assignment(e: &Engine, value: &SyntaxNode, expected: &TypeRef, out: &mut Sink) {
    if matches!(expected, TypeRef::Platform(_)) || matches!(strip(expected), TypeRef::TypeParam { .. }) {
        return;
    }
    for value in super::expressions::branch_leaves(value) {
        if let Some((rule, msg, _, convert)) = type_mismatch(e, &value, expected, &shown(expected)) {
            let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &ASSIGNMENT_TYPE_MISMATCH } else { rule };
            out.report_fixes(rule, value.text_range(), msg, convert.into_iter().collect());
        }
    }
}

/// A condition (`if`/`while`, `&&`/`||`/`!` operand) whose certain type is not `Boolean`.
fn condition(e: &Engine, value: &SyntaxNode, out: &mut Sink) {
    let Some(t) = certain(e, value) else { return };
    let fq = fq_of(&t).unwrap_or_default();
    // `Boolean!` from Java may be used as a condition.
    if fq == "kotlin.Nothing" || fq.is_empty() || fq == "kotlin.Boolean" && (!nullable(&t) || matches!(t, TypeRef::Platform(_))) {
        return;
    }
    out.report(&CONDITION_TYPE_MISMATCH, value.text_range(), format!("Condition type mismatch: inferred type is '{}' but 'Boolean' was expected.", shown(&t)), None);
}

/// Whether `fq` names a class (not an interface, so unrelated classes share no subtype).
fn class_kind(e: &Engine, fq: &str) -> bool {
    e.classifier(fq).is_some_and(|c| matches!(c.symbol().kind, SymbolKind::Class | SymbolKind::Object | SymbolKind::Enum))
}

/// `x as T`: useless when `x` already has type `T`; never succeeding between unrelated
/// classes.
fn cast(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let (Some(value), Some(ty)) = (n.first_child(), n.children().find_map(ast::TypeRef::cast)) else { return };
    let safe = n.children().any(|c| c.kind() == SyntaxKind::OPERATION_REFERENCE && c.text() == "as?");
    let Some(actual) = certain(e, &value) else { return };
    let target = e.resolve_syntax_type(&ty);
    if unchecked(e, &actual, &target, &ty) {
        let at = n.children().find(|c| c.kind() == SyntaxKind::OPERATION_REFERENCE).map_or(n.text_range(), |o| o.text_range());
        out.report(&UNCHECKED_CAST, at, format!("Unchecked cast of '{}' to '{}'.", shown(&actual), shown(&target)), None);
        return;
    }
    let (Some(afq), Some(tfq)) = (fq_of(&actual), fq_of(&target)) else { return };
    if matches!(strip(&target), TypeRef::TypeParam { .. }) || afq == "kotlin.Nothing" {
        return;
    }
    // Unknown type arguments (displayed as errors) never make a cast useless.
    if actual.display() == target.display() && !safe {
        let remove = TextRange::new(value.text_range().end(), n.text_range().end());
        out.report(&USELESS_CAST, TextRange::new(value.text_range().end(), n.text_range().end()), "No cast needed.", Some(Fix::delete("Remove cast", remove)));
        return;
    }
    if [afq, tfq].iter().any(|f| matches!(*f, "kotlin.Any" | "kotlin.Nothing")) || !class_kind(e, afq) || !class_kind(e, tfq) {
        return;
    }
    let up = e.conforms(&TypeRef::named(afq), tfq);
    let down = e.conforms(&TypeRef::named(tfq), afq);
    if up == Some(false) && down == Some(false) {
        out.report(&CAST_NEVER_SUCCEEDS, n.children().find(|c| c.kind() == SyntaxKind::OPERATION_REFERENCE).map_or(n.text_range(), |o| o.text_range()), "This cast can never succeed.", None);
    }
}

/// `x is T` that is always true (`x` is a non-null `T`) — or always false for `!is`.
fn is_check(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let (Some(value), Some(ty)) = (n.first_child(), n.children().find_map(ast::TypeRef::cast)) else { return };
    let negated = n.children().any(|c| c.kind() == SyntaxKind::OPERATION_REFERENCE && c.text() == "!is");
    // The smart cast from this very check does not apply to its own subject, but an
    // enclosing one would: use the declared type.
    let Some(actual) = certain(e, &value) else { return };
    let target = e.resolve_syntax_type(&ty);
    let (Some(afq), Some(tfq)) = (fq_of(&actual), fq_of(&target)) else { return };
    if nullable(&actual) && !nullable(&target) || matches!(strip(&target), TypeRef::TypeParam { .. }) || afq == "kotlin.Nothing" {
        return;
    }
    if e.conforms(&TypeRef::named(afq), tfq) == Some(true) {
        let result = if negated { "false" } else { "true" };
        out.report(&USELESS_IS_CHECK, n.text_range(), format!("Check for instance is always '{result}'."), None);
    } else if incompatible(e, &actual, &target) {
        out.report(&INCOMPATIBLE_TYPES, ty.syntax().text_range(), format!("Incompatible types '{}' and '{}'.", ty.syntax().text(), shown(&actual)), None);
    }
}

/// Two types whose values can never be equal: unrelated classes (each class has one
/// superclass, so they share no subclass).
fn incompatible(e: &Engine, a: &TypeRef, b: &TypeRef) -> bool {
    let (Some(afq), Some(bfq)) = (fq_of(a), fq_of(b)) else { return false };
    if [afq, bfq].iter().any(|f| matches!(*f, "kotlin.Any" | "kotlin.Nothing")) || !class_kind(e, afq) || !class_kind(e, bfq) {
        return false;
    }
    e.conforms(&TypeRef::named(afq), bfq) == Some(false) && e.conforms(&TypeRef::named(bfq), afq) == Some(false)
}

/// An integer literal (of whichever integer type) compared with a value of class `t`
/// unrelated to all of them.
fn int_literal_incompatible(e: &Engine, v: &SyntaxNode, t: &TypeRef) -> bool {
    literal_type(v) == Some(Some("kotlin.Int")) && ["kotlin.Int", "kotlin.Long", "kotlin.Short", "kotlin.Byte"].iter().all(|i| incompatible(e, &TypeRef::named(i), t))
}

/// The certain type of an operand, except integer literals (their type follows the other
/// side).
fn operand_type(e: &Engine, v: &SyntaxNode) -> Option<TypeRef> {
    if v.descendants_with_tokens().any(|t| t.kind() == SyntaxKind::INT_LITERAL) && literal_type(v).is_some() {
        return None;
    }
    certain(e, v)
}

/// `a == b` / `a != b` between unrelated classes.
fn equality(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(op) = binary_op(n).filter(|o| matches!(o.kind(), SyntaxKind::EQ2 | SyntaxKind::NOT_EQ)) else { return };
    let Some((l, r)) = binary_operands(n) else { return };
    for (lit, other) in [(&l, &r), (&r, &l)] {
        if let Some(t) = operand_type(e, other)
            && int_literal_incompatible(e, lit, &t)
        {
            let (a, b) = if lit == &l { ("Int".to_string(), shown(&t)) } else { (shown(&t), "Int".to_string()) };
            out.report(&EQUALITY_NOT_APPLICABLE, op.text_range(), format!("Operator '{}' cannot be applied to '{a}' and '{b}'.", op.text()), None);
            return;
        }
    }
    let (Some(a), Some(b)) = (operand_type(e, &l), operand_type(e, &r)) else { return };
    if incompatible(e, &a, &b) {
        out.report(&EQUALITY_NOT_APPLICABLE, op.text_range(), format!("Operator '{}' cannot be applied to '{}' and '{}'.", op.text(), shown(&a), shown(&b)), None);
    }
}

/// `when (x) { v -> }` where `v`'s class is unrelated to `x`'s.
fn when_values(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(subject) = n.children().find(|c| c.kind() == SyntaxKind::WHEN_SUBJECT) else { return };
    let expr = match subject.children().find(|c| c.kind() == SyntaxKind::PROPERTY) {
        Some(p) => initializer(&p),
        None => subject.first_child(),
    };
    let Some(st) = expr.and_then(|x| operand_type(e, &x)) else { return };
    let conditions = n.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY).flat_map(|en| en.children().filter(|c| c.kind() == SyntaxKind::WHEN_CONDITION).collect::<Vec<_>>());
    let conditions: Vec<SyntaxNode> = conditions.collect();
    // `is T` branches for a class unrelated to the subject's.
    for c in &conditions {
        let is = c.children().find(|o| o.kind() == SyntaxKind::OPERATION_REFERENCE).and_then(|o| o.first_token()).is_some_and(|t| matches!(t.kind(), SyntaxKind::IS_KW | SyntaxKind::NOT_IS_KW));
        if let Some(ty) = c.children().find_map(ast::TypeRef::cast).filter(|_| is)
            && incompatible(e, &e.resolve_syntax_type(&ty), &st)
        {
            out.report(&INCOMPATIBLE_TYPES, ty.syntax().text_range(), format!("Incompatible types '{}' and '{}'.", ty.syntax().text(), shown(&st)), None);
        }
    }
    for v in conditions.iter().filter_map(condition_value) {
        if int_literal_incompatible(e, &v, &st) {
            out.report(&INCOMPATIBLE_TYPES, v.text_range(), format!("Incompatible types 'Int' and '{}'.", shown(&st)), None);
            continue;
        }
        let Some(vt) = operand_type(e, &v) else { continue };
        if incompatible(e, &vt, &st) {
            out.report(&INCOMPATIBLE_TYPES, v.text_range(), format!("Incompatible types '{}' and '{}'.", shown(&vt), shown(&st)), None);
        }
    }
}

/// `throw x` where `x` is certainly not a `Throwable`.
fn thrown(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(v) = n.children().last() else { return };
    let Some(t) = certain(e, &v) else { return };
    let Some(fq) = fq_of(&t) else { return };
    if fq != "kotlin.Nothing" && e.conforms(&TypeRef::named(fq), "kotlin.Throwable") == Some(false) {
        out.report(&TYPE_MISMATCH, v.text_range(), format!("Type mismatch: inferred type is '{}' but 'Throwable' was expected.", shown(&t)), None);
    }
}

/// `catch (e: T)` where `T` is certainly not a `Throwable`.
fn caught(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    // The parameter's own type (not an annotation's: `catch (@Suppress("x") e: E)`).
    let param = n.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST).and_then(|l| l.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER));
    let Some(ty) = param.and_then(|p| ast::child::<ast::TypeRef>(&p)) else { return };
    let t = e.resolve_syntax_type(&ty);
    let Some(fq) = fq_of(&t) else { return };
    if e.conforms(&TypeRef::named(fq), "kotlin.Throwable") == Some(false) {
        out.report(&TYPE_MISMATCH, ty.syntax().text_range(), format!("Type mismatch: '{}' is not a subtype of 'Throwable'.", ty.syntax().text()), None);
    }
}

/// `if` without `else` whose value is used.
fn if_as_expression(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    if n.children().any(|c| c.kind() == SyntaxKind::ELSE_BRANCH) || ast::token(n, SyntaxKind::ELSE_KW).is_some() {
        return;
    }
    let Some(p) = n.parent() else { return };
    let used = match p.kind() {
        SyntaxKind::PROPERTY => initializer(&p).as_ref() == Some(n),
        SyntaxKind::VALUE_ARGUMENT => true,
        SyntaxKind::BINARY_EXPR => assignment_op(&p).is_some_and(|o| o.kind() == SyntaxKind::EQ) && p.children().last().as_ref() == Some(n),
        // An expression body of a function with a (non-Unit) result type.
        SyntaxKind::FUN => initializer(&p).as_ref() == Some(n) && ast::FunDecl::cast(p.clone()).and_then(|f| f.return_type()).is_some_and(|t| !matches!(e.resolve_syntax_type(&t).fq_name(), Some("kotlin.Unit") | None)),
        _ => false,
    };
    if used && let Some(kw) = ast::token(n, SyntaxKind::IF_KW) {
        let fix = Fix::replace("Add 'else' branch", TextRange::empty(n.text_range().end()), " else TODO()");
        out.report(&INVALID_IF_AS_EXPRESSION, kw.text_range(), "'if' must have both main and 'else' branches if used as an expression.", Some(fix));
    }
}

/// A cast the runtime cannot check: from a non-generic type to a type with type arguments
/// (or a function type), or to a type parameter that is not reified.
fn unchecked(e: &Engine, actual: &TypeRef, target: &TypeRef, written: &ast::TypeRef) -> bool {
    let TypeRef::Named { fq: Some(afq), args, .. } = strip(actual) else { return false };
    if &**afq == "kotlin.Nothing" || !args.is_empty() || e.classifier(afq).is_none_or(|c| !c.symbol().type_params.is_empty()) {
        return false;
    }
    match strip(target) {
        TypeRef::TypeParam { name, .. } => super::signatures::type_parameter(written.syntax(), name).is_some_and(|tp| !has_modifier(&tp, "reified")),
        // A subclass fixing the arguments (`class Names : List<String>`) is an upcast.
        TypeRef::Named { fq: Some(tfq), args, .. } => args.iter().any(|a| matches!(a, TypeArg::Type { .. })) && e.conforms(&TypeRef::named(afq), tfq) == Some(false),
        TypeRef::Function { .. } => &**afq == "kotlin.Any",
        _ => false,
    }
}

/// The certain type of `v`, when it is a non-null Kotlin or project class type.
fn kotlin_class_type(e: &Engine, v: &SyntaxNode) -> Option<TypeRef> {
    let t = certain(e, v)?;
    let TypeRef::Named { fq: Some(fq), nullable: false, .. } = &t else { return None };
    let c = e.classifier(fq)?;
    let kotlin = match &c {
        crate::analysis::resolve::Classifier::Library(lc) => lc.is_kotlin,
        crate::analysis::resolve::Classifier::Source(s) => matches!(&s.origin, Origin::Source { file, .. } if file.extension().is_some_and(|x| x == "kt" || x == "kts")),
        _ => false,
    };
    (kotlin && &**fq != "kotlin.Nothing" && c.symbol().kind != SymbolKind::TypeAlias).then_some(t)
}

/// Whether `t` has a member or applicable extension named `name` at `at` (member
/// extensions of implicit receivers included).
fn has_function(e: &Engine, at: &SyntaxNode, t: &TypeRef, name: &str) -> bool {
    !e.members_named(t, name).is_empty() || !e.extensions(t, name).is_empty() || !e.member_extensions(at, t, name).is_empty()
}

/// Inside a local class or object expression: their (inherited) member extensions are not
/// in scope for the engine.
fn in_local_class(n: &SyntaxNode) -> bool {
    n.ancestors().any(|a| a.kind() == SyntaxKind::OBJECT_LITERAL || matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION) && a.ancestors().any(|b| b.kind() == SyntaxKind::BLOCK))
}

/// `for (x in v)` where `v`'s type has no `iterator()`.
fn loop_range(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    if in_local_class(n) {
        return;
    }
    let Some(v) = n.first_child() else { return };
    let Some(t) = kotlin_class_type(e, &v) else { return };
    if !has_function(e, n, &t, "iterator") {
        out.report(&ITERATOR_MISSING, v.text_range(), format!("For-loop range must have an 'iterator()' method: '{}' has none.", shown(&t)), None);
    }
}

/// `val x by d` where `d`'s type has no `getValue` (or, for a `var`, no `setValue`).
fn delegate(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    if in_local_class(n) {
        return;
    }
    let Some(v) = n.children().last() else { return };
    let Some(t) = kotlin_class_type(e, &v) else { return };
    if has_function(e, n, &t, "provideDelegate") {
        return;
    }
    let var = n.parent().is_some_and(|p| ast::token(&p, SyntaxKind::VAR_KW).is_some());
    for name in ["getValue", "setValue"].into_iter().take(if var { 2 } else { 1 }) {
        if !has_function(e, n, &t, name) {
            out.report(&DELEGATE_SPECIAL_FUNCTION_MISSING, v.text_range(), format!("Property delegate must have a '{name}' method: '{}' has none.", shown(&t)), None);
        }
    }
}
