//! Inline functions and the lambdas passed to functions: non-local `return`s, inline
//! parameters used as values or called where they cannot be inlined, public inline
//! functions using non-public declarations, and suspension points in `crossinline` lambdas.

use super::super::*;
use crate::analysis::ide::infer::{Def, Engine, LocalKind, strip};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeRef, Visibility};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::RETURN_EXPR => non_local_return(cx, e, &n, out),
            SyntaxKind::FUN if has_modifier(&n, "inline") => {
                inline_parameters(cx, e, &n, out);
                public_api(e, &n, out);
            }
            SyntaxKind::CALL_EXPR => crossinline_suspension(cx, e, &n, out),
            _ => {}
        }
    }
}

/// How a lambda passed to a call runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Passed {
    /// Inlined into the caller (non-local returns allowed).
    Inline,
    /// Inlined, but called from another context (`crossinline`).
    Crossinline,
    /// Stored or called later: a non-inline function, a `noinline` parameter.
    NotInline,
}

/// The call a lambda is an argument of, and its callee name.
fn lambda_call(lambda: &SyntaxNode) -> Option<(SyntaxNode, SyntaxNode)> {
    let mut parent = lambda.parent()?;
    if parent.kind() == SyntaxKind::LABELED_EXPR {
        parent = parent.parent()?;
    }
    let call = match parent.kind() {
        SyntaxKind::LAMBDA_ARGUMENT => parent.parent()?,
        SyntaxKind::VALUE_ARGUMENT => parent.parent()?.parent()?,
        _ => return None,
    };
    let callee = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF)?;
    (call.kind() == SyntaxKind::CALL_EXPR).then_some((call, callee))
}

/// `crossinline` / `noinline` on parameter `index` of a project function.
fn parameter_modifiers(cx: &Cx, sym: &Symbol, index: usize) -> Option<(bool, bool)> {
    let Origin::Source { file, name_range, .. } = &sym.origin else { return None };
    let root = cx.snap.file(file)?.parse().syntax();
    let f = crate::syntax::covering(&root, *name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| a.kind() == SyntaxKind::FUN)?;
    let p = f.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST)?.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER).nth(index)?;
    Some((has_modifier(&p, "crossinline"), has_modifier(&p, "noinline")))
}

/// Whether `t` is a non-null function type (inline parameters of other types are not inlined).
fn inlinable_type(t: &TypeRef) -> Option<bool> {
    match strip(t) {
        TypeRef::Function { nullable, .. } => Some(!nullable),
        TypeRef::Named { fq: Some(fq), nullable, .. } if fq.starts_with("kotlin.Function") || fq.starts_with("kotlin.coroutines.SuspendFunction") => Some(!nullable),
        TypeRef::Named { fq: Some(_), .. } => Some(false),
        _ => None,
    }
}

/// How `lambda` runs, when every candidate of its call agrees. `None`: not an argument
/// (or unknown).
fn passed(cx: &Cx, e: &Engine, lambda: &SyntaxNode) -> Option<Passed> {
    let (_, callee) = lambda_call(lambda)?;
    let candidates = e.call_candidates(&callee)?;
    if candidates.iter().all(|(s, _)| !s.modifiers.has(Modifiers::INLINE)) {
        return Some(Passed::NotInline);
    }
    if !candidates.iter().all(|(s, _)| s.modifiers.has(Modifiers::INLINE)) {
        return None;
    }
    let (sym, index) = e.lambda_parameter(lambda)?;
    let ty = e.resolve_declared(&sym, &sym.params.get(index)?.ty);
    if !inlinable_type(&ty)? {
        return Some(Passed::NotInline);
    }
    Some(match parameter_modifiers(cx, &sym, index) {
        Some((_, true)) => Passed::NotInline,
        Some((true, _)) => Passed::Crossinline,
        _ => Passed::Inline,
    })
}

/// A bare `return` inside lambdas: each one between it and its function must be inlined
/// (not `crossinline`).
fn non_local_return(cx: &Cx, e: &Engine, ret: &SyntaxNode, out: &mut Sink) {
    if ret.children().any(|c| c.kind() == SyntaxKind::LABEL_QUALIFIER) {
        return;
    }
    let Some(kw) = ret.first_token() else { return };
    for a in ret.ancestors().skip(1) {
        match a.kind() {
            SyntaxKind::LAMBDA_EXPR => {
                let argument = lambda_call(&a).is_some();
                match passed(cx, e, &a) {
                    Some(Passed::Inline) => continue,
                    Some(Passed::Crossinline) => {
                        out.report(&NON_LOCAL_RETURN_NOT_ALLOWED, kw.text_range(), "'return' is not allowed here: the lambda is passed to a 'crossinline' parameter.", None);
                        return;
                    }
                    Some(Passed::NotInline) => {}
                    None if argument => return,
                    // A lambda stored in a variable or used as a value.
                    None if a.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::PROPERTY | SyntaxKind::BLOCK)) => {}
                    None => return,
                }
                out.report(&RETURN_NOT_ALLOWED, kw.text_range(), "'return' is not allowed here.", None);
                return;
            }
            SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::CLASS_BODY | SyntaxKind::KOTLIN_FILE => return,
            _ => {}
        }
    }
}

/// Inline parameters of `f` (non-null function types without `noinline`): only called, or
/// passed on to other inline functions; called only where they are inlined (or marked
/// `crossinline`).
fn inline_parameters(cx: &Cx, e: &Engine, f: &SyntaxNode, out: &mut Sink) {
    let Some(params) = f.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST) else { return };
    let Some(body) = f.children().find(|c| c.kind() == SyntaxKind::BLOCK).or_else(|| initializer(f)) else { return };
    for p in params.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER) {
        if has_modifier(&p, "noinline") || has_modifier(&p, "vararg") {
            continue;
        }
        let (Some(name), Some(ty)) = (name_token(&p), ast::child::<ast::TypeRef>(&p)) else { continue };
        if inlinable_type(&e.resolve_syntax_type(&ty)) != Some(true) {
            continue;
        }
        let crossinline = has_modifier(&p, "crossinline");
        let pname = ast::ident_text(&name);
        let keyword_at = p.first_token().map_or(name.text_range().start(), |t| t.text_range().start());
        for nr in body.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF && d.first_token().is_some_and(|t| ast::ident_text(&t) == pname)) {
            let mine = e.resolve_name_ref(&nr).iter().any(|d| matches!(d, Def::Local(l) if l.kind == LocalKind::Parameter && l.name_range == name.text_range()));
            if !mine {
                continue;
            }
            let parent = nr.parent();
            let called = parent.as_ref().is_some_and(|p| p.kind() == SyntaxKind::CALL_EXPR && p.first_child().as_ref() == Some(&nr))
                || parent.as_ref().is_some_and(|p| {
                    p.kind() == SyntaxKind::DOT_QUALIFIED_EXPR && p.first_child().as_ref() == Some(&nr) && p.last_child().is_some_and(|c| c.kind() == SyntaxKind::CALL_EXPR && c.first_child().is_some_and(|n| n.text() == "invoke"))
                });
            if called {
                // Called in a lambda that is not inlined here, or a local function or object.
                if crossinline {
                    continue;
                }
                let mut elsewhere = false;
                for a in nr.ancestors().take_while(|a| a != &body) {
                    match a.kind() {
                        SyntaxKind::LAMBDA_EXPR => match passed(cx, e, &a) {
                            Some(Passed::Inline) => {}
                            Some(_) => elsewhere = true,
                            None => break,
                        },
                        SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::OBJECT_LITERAL | SyntaxKind::CLASS => elsewhere = true,
                        _ => {}
                    }
                }
                if elsewhere {
                    let fix = Fix::replace("Add 'crossinline' modifier", TextRange::empty(keyword_at), "crossinline ");
                    out.report(&NON_LOCAL_RETURN_NOT_ALLOWED, nr.text_range(), format!("Can't inline '{pname}' here: it may contain non-local returns. Add 'crossinline' modifier to parameter declaration '{pname}'."), Some(fix));
                }
                continue;
            }
            // Passed as an argument: fine to inline functions, not to others.
            let argument_call = parent.as_ref().filter(|p| p.kind() == SyntaxKind::VALUE_ARGUMENT).and_then(|a| a.parent()).and_then(|l| l.parent()).filter(|c| c.kind() == SyntaxKind::CALL_EXPR);
            if let Some(call) = argument_call {
                let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { continue };
                let Some(candidates) = e.call_candidates(&callee) else { continue };
                if candidates.is_empty() || candidates.iter().any(|(s, _)| s.modifiers.has(Modifiers::INLINE)) {
                    continue;
                }
            } else if parent.as_ref().is_some_and(|p| matches!(p.kind(), SyntaxKind::SAFE_QUALIFIED_EXPR | SyntaxKind::CALLABLE_REFERENCE | SyntaxKind::DOT_QUALIFIED_EXPR)) {
                // `block?.invoke()`, `block::invoke`, `block.other`: left to the compiler.
                continue;
            }
            let fix = Fix::replace("Add 'noinline' modifier", TextRange::empty(keyword_at), "noinline ");
            out.report(&USAGE_IS_NOT_INLINABLE, nr.text_range(), format!("Illegal usage of inline-parameter '{pname}'. Add 'noinline' modifier to the parameter declaration."), Some(fix));
        }
    }
}

/// A project declaration annotated `@PublishedApi` (source symbols carry no annotations).
fn published_api(e: &Engine, sym: &Symbol) -> bool {
    let Origin::Source { file, name_range, .. } = &sym.origin else { return false };
    let Some(f) = e.snap.file(file) else { return false };
    let root = f.parse().syntax();
    let decl = crate::syntax::covering(&root, *name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION));
    decl.and_then(|d| modifiers(&d)).is_some_and(|m| m.annotations().any(|a| a.type_ref().is_some_and(|t| t.syntax().text().to_string().ends_with("PublishedApi"))))
}

/// The function is part of the public API: not private or internal, and neither is any
/// class around it; not local.
fn is_public_api(f: &SyntaxNode) -> bool {
    let hidden = |n: &SyntaxNode| has_modifier(n, "private") || has_modifier(n, "internal");
    if hidden(f) || is_local(f) {
        return false;
    }
    f.ancestors().skip(1).all(|a| match a.kind() {
        SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => !hidden(&a),
        SyntaxKind::OBJECT_LITERAL | SyntaxKind::FUN | SyntaxKind::BLOCK => false,
        _ => true,
    })
}

/// A public inline function's body is copied into callers: it may only use public API
/// (internal declarations need `@PublishedApi`).
fn public_api(e: &Engine, f: &SyntaxNode, out: &mut Sink) {
    if !is_public_api(f) {
        return;
    }
    let Some(body) = f.children().find(|c| c.kind() == SyntaxKind::BLOCK).or_else(|| initializer(f)) else { return };
    for nr in body.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF) {
        // Local classes, objects and functions are compiled separately.
        if nr.ancestors().take_while(|a| a != &body).any(|a| matches!(a.kind(), SyntaxKind::OBJECT_LITERAL | SyntaxKind::CLASS | SyntaxKind::FUN)) {
            continue;
        }
        let defs = e.resolve_name_ref(&nr);
        let [Def::Symbol { sym, .. }] = defs.as_slice() else { continue };
        if !matches!(sym.origin, Origin::Source { .. }) || sym.kind == SymbolKind::TypeParameter {
            continue;
        }
        let published = published_api(e, sym);
        let what = match sym.visibility {
            Visibility::Private => "private",
            Visibility::Internal if !published => "internal",
            _ => continue,
        };
        out.report(&NON_PUBLIC_CALL_FROM_PUBLIC_INLINE, nr.text_range(), format!("Public-API inline function cannot access non-public-API {what} declaration '{}'.", nr.text()), None);
    }
}

// ------------------------------------------------------------ crossinline suspension

/// A suspend call in a lambda passed to a `crossinline` parameter, inside a suspend
/// function or lambda: the lambda may run in another context.
fn crossinline_suspension(cx: &Cx, e: &Engine, call: &SyntaxNode, out: &mut Sink) {
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    if !e.resolve_name_ref(&callee).iter().any(|d| matches!(d, Def::Symbol { sym, .. } if sym.modifiers.has(Modifiers::SUSPEND))) {
        return;
    }
    let Some(candidates) = e.call_candidates(&callee) else { return };
    if candidates.is_empty() || !candidates.iter().all(|(s, _)| s.kind == SymbolKind::Function && s.modifiers.has(Modifiers::SUSPEND)) {
        return;
    }
    let mut crossinline = false;
    for a in call.ancestors().skip(1) {
        match a.kind() {
            SyntaxKind::LAMBDA_EXPR => match e.lambda_kind(&a) {
                Some((true, _)) => break,
                Some((false, true)) => {
                    if passed(cx, e, &a) == Some(Passed::Crossinline) {
                        crossinline = true;
                    }
                }
                _ => return,
            },
            SyntaxKind::FUN if has_modifier(&a, "suspend") => break,
            SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::CLASS_BODY | SyntaxKind::KOTLIN_FILE => return,
            _ => {}
        }
    }
    if crossinline {
        out.report(&NON_LOCAL_SUSPENSION_POINT, callee.text_range(), "Suspension functions can only be called within coroutine body (the lambda is passed to a 'crossinline' parameter).", None);
    }
}
