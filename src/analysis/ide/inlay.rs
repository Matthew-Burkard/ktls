//! Inlay hints: inferred types of declarations and parameter names at call sites.

use std::path::Path;

use serde::Deserialize;

use super::infer::{Def, Engine, LocalKind};
use crate::analysis::Snapshot;
use crate::symbols::{SymbolKind, TypeRef};
use crate::syntax::ast::{self, AstNode};
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};

/// `inlayHints` settings.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct InlayConfig {
    /// Types of properties, locals, loop variables and destructuring entries without one.
    pub types: bool,
    /// The type of the implicit `it` and of untyped lambda parameters.
    pub lambda_parameters: bool,
    /// Return types of expression-bodied functions without one.
    pub return_types: bool,
    pub parameter_names: ParameterNames,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ParameterNames {
    None,
    /// Only for literal arguments (`"x"`, `1`, `true`, `null`) of functions with several
    /// parameters, like IntelliJ.
    Literals,
    All,
}

impl Default for InlayConfig {
    fn default() -> Self {
        InlayConfig { types: true, lambda_parameters: true, return_types: true, parameter_names: ParameterNames::Literals }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintKind {
    Type,
    Parameter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    pub offset: TextSize,
    pub label: String,
    pub kind: HintKind,
}

pub fn inlay_hints(snap: &Snapshot, path: &Path, range: TextRange, config: &InlayConfig) -> Vec<Hint> {
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let mut out = Vec::new();
    for n in e.root.descendants().filter(|n| n.text_range().intersect(range).is_some()) {
        match n.kind() {
            SyntaxKind::PROPERTY if config.types => property(&e, &n, &mut out),
            SyntaxKind::DESTRUCTURING_ENTRY if config.types => local_name(&e, &n, &mut out),
            SyntaxKind::VALUE_PARAMETER if config.types && n.parent().is_some_and(|p| p.kind() == SyntaxKind::FOR_EXPR) => local_name(&e, &n, &mut out),
            SyntaxKind::VALUE_PARAMETER if config.lambda_parameters && n.parent().is_some_and(|p| p.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST) => local_name(&e, &n, &mut out),
            SyntaxKind::LAMBDA_EXPR if config.lambda_parameters => implicit_it(&e, &n, &mut out),
            SyntaxKind::FUN if config.return_types => return_type(&e, &n, &mut out),
            SyntaxKind::VALUE_ARGUMENT_LIST if config.parameter_names != ParameterNames::None => parameter_names(&e, &n, config.parameter_names, &mut out),
            _ => {}
        }
    }
    out.retain(|h| range.contains_inclusive(h.offset));
    out.sort_by_key(|h| h.offset);
    out
}

/// A type worth showing: known, and not an error.
fn shown(t: &TypeRef) -> Option<String> {
    let s = t.display();
    (!s.is_empty() && !s.contains("<error>") && !s.starts_with('<')).then_some(s)
}

/// Type parameters mentioned by `t`.
fn type_params(t: &TypeRef, out: &mut Vec<String>) {
    match t {
        TypeRef::TypeParam { name, .. } => out.push(name.clone()),
        TypeRef::Named { args, .. } => {
            for a in args {
                if let crate::symbols::TypeArg::Type { ty, .. } = a {
                    type_params(ty, out);
                }
            }
        }
        TypeRef::Function { receiver, params, ret, .. } => {
            for p in receiver.iter().map(|r| &**r).chain(params).chain(std::iter::once(&**ret)) {
                type_params(p, out);
            }
        }
        TypeRef::Platform(inner) => type_params(inner, out),
        _ => {}
    }
}

/// `t` only mentions type parameters declared around `at` (not ones of a called function
/// that inference failed to bind).
fn in_scope(t: &TypeRef, at: &SyntaxToken) -> bool {
    let mut names = Vec::new();
    type_params(t, &mut names);
    names.iter().all(|n| {
        at.parent_ancestors().any(|a| {
            a.children().filter(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).any(|tp| ast::token(&tp, SyntaxKind::IDENT).is_some_and(|i| i.text() == n))
        })
    })
}

fn type_hint(tok: &SyntaxToken, t: &TypeRef, out: &mut Vec<Hint>) {
    if !in_scope(t, tok) {
        return;
    }
    if let Some(s) = shown(t) {
        out.push(Hint { offset: tok.text_range().end(), label: format!(": {s}"), kind: HintKind::Type });
    }
}

/// The initializer states its type: a literal, constructor call, cast or string.
fn obvious(e: &Engine, init: &SyntaxNode) -> bool {
    match init.kind() {
        SyntaxKind::LITERAL | SyntaxKind::STRING_TEMPLATE | SyntaxKind::BINARY_WITH_TYPE | SyntaxKind::OBJECT_LITERAL => true,
        SyntaxKind::PREFIX_EXPR => init.children().any(|c| c.kind() == SyntaxKind::LITERAL),
        SyntaxKind::CALL_EXPR | SyntaxKind::DOT_QUALIFIED_EXPR => {
            // `Foo(…)` / `pkg.Foo(…)` / `Foo.Nested(…)`
            let call = if init.kind() == SyntaxKind::CALL_EXPR { Some(init.clone()) } else { init.last_child().filter(|c| c.kind() == SyntaxKind::CALL_EXPR) };
            let Some(callee) = call.and_then(|c| c.first_child()).filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return false };
            e.resolve_name_ref(&callee).iter().any(|d| matches!(d, Def::Symbol { sym, .. } if sym.kind.is_classifier() || sym.kind == SymbolKind::Constructor))
        }
        // `Color.RED`, `Unit`, objects.
        SyntaxKind::NAME_REF => e.resolve_name_ref(init).iter().any(|d| matches!(d, Def::Symbol { sym, .. } if sym.kind.is_classifier())),
        _ => false,
    }
}

/// The values an expression can produce: the last expression of each `if`/`when`/`try`
/// branch (recursively), or the expression itself.
fn results(n: &SyntaxNode, out: &mut Vec<SyntaxNode>) {
    let last = |b: &SyntaxNode| -> Option<SyntaxNode> {
        let inner = if b.kind() == SyntaxKind::BLOCK { b.clone() } else { b.first_child()? };
        if inner.kind() == SyntaxKind::BLOCK { inner.children().last() } else { Some(inner) }
    };
    let branches: Vec<SyntaxNode> = match n.kind() {
        SyntaxKind::IF_EXPR => n.children().filter(|c| matches!(c.kind(), SyntaxKind::THEN_BRANCH | SyntaxKind::ELSE_BRANCH)).collect(),
        SyntaxKind::WHEN_EXPR => n.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY).filter_map(|en| en.children().find(|c| c.kind() == SyntaxKind::BODY)).collect(),
        SyntaxKind::TRY_EXPR => n
            .children()
            .filter(|c| c.kind() == SyntaxKind::BLOCK)
            .chain(n.children().filter(|c| c.kind() == SyntaxKind::CATCH_CLAUSE).filter_map(|c| c.children().find(|b| b.kind() == SyntaxKind::BLOCK)))
            .collect(),
        SyntaxKind::PAREN_EXPR => n.first_child().into_iter().collect(),
        _ => {
            out.push(n.clone());
            return;
        }
    };
    for b in branches {
        match last(&b) {
            Some(v) => results(&v, out),
            None => out.push(b),
        }
    }
}

/// The type of `expr`, unless its branches disagree (without smart casts and common
/// supertypes, the engine takes the first branch, which may be wrong).
fn value_type(e: &Engine, expr: &SyntaxNode) -> Option<TypeRef> {
    let mut values = Vec::new();
    results(expr, &mut values);
    let t = e.type_of(expr)?;
    if values.len() > 1 {
        let shown_t = t.display();
        for v in &values {
            // Jumps (`return`, `throw`, `TODO()`) fit any type.
            if matches!(v.kind(), SyntaxKind::RETURN_EXPR | SyntaxKind::THROW_EXPR | SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR) {
                continue;
            }
            let vt = e.type_of(v)?;
            if super::infer::fq_of(&vt) == Some("kotlin.Nothing") {
                continue;
            }
            if vt.display().trim_end_matches('?') != shown_t.trim_end_matches('?') {
                return None;
            }
        }
    }
    Some(t)
}

fn property(e: &Engine, p: &SyntaxNode, out: &mut Vec<Hint>) {
    if ast::child::<ast::TypeRef>(p).is_some() {
        return;
    }
    let Some(name) = ast::token(p, SyntaxKind::IDENT) else { return };
    let Some(init) = p.children().filter(|c| c.kind() != SyntaxKind::MODIFIER_LIST && c.kind() != SyntaxKind::PROPERTY_ACCESSOR && c.kind() != SyntaxKind::PROPERTY_DELEGATE).last() else { return };
    if obvious(e, &init) || init.text_range().start() < name.text_range().end() {
        return;
    }
    if let Some(t) = value_type(e, &init) {
        type_hint(&name, &t, out);
    }
}

/// Loop variables, lambda parameters and destructuring entries without a type.
fn local_name(e: &Engine, decl: &SyntaxNode, out: &mut Vec<Hint>) {
    if ast::child::<ast::TypeRef>(decl).is_some() {
        return;
    }
    let Some(tok) = ast::token(decl, SyntaxKind::IDENT).filter(|t| t.text() != "_") else { return };
    if let Some(Def::Local(l)) = super::declaration_def(e, decl, &tok)
        && let Some(t) = &l.ty
    {
        type_hint(&tok, t, out);
    }
}

/// `{ it: T -> ` before the body of a lambda that uses `it`.
fn implicit_it(e: &Engine, lambda: &SyntaxNode, out: &mut Vec<Hint>) {
    if lambda.children().any(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST) {
        return;
    }
    let Some(body) = lambda.children().find(|c| c.kind() == SyntaxKind::BLOCK) else { return };
    let uses_it = body.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF).any(|nr| {
        nr.first_token().is_some_and(|t| t.text() == "it") && e.find_local(&nr, "it").is_some_and(|l| l.kind == LocalKind::It && lambda.text_range().contains_range(nr.text_range()) && nearest_lambda(&nr).as_ref() == Some(lambda))
    });
    if !uses_it {
        return;
    }
    let Some((_, params)) = e.lambda_signature(lambda) else { return };
    let [t] = params.as_slice() else { return };
    let Some(open) = lambda.first_token() else { return };
    if let Some(s) = shown(t).filter(|_| in_scope(t, &open)) {
        out.push(Hint { offset: open.text_range().end(), label: format!(" it: {s} ->"), kind: HintKind::Type });
    }
}

fn nearest_lambda(n: &SyntaxNode) -> Option<SyntaxNode> {
    n.ancestors().find(|a| a.kind() == SyntaxKind::LAMBDA_EXPR)
}

/// `fun f() = expr` → `: T` after the parameter list.
fn return_type(e: &Engine, f: &SyntaxNode, out: &mut Vec<Hint>) {
    let decl = ast::FunDecl::cast(f.clone()).unwrap();
    if decl.return_type().is_some() || ast::token(f, SyntaxKind::EQ).is_none() {
        return;
    }
    let Some(params) = f.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST) else { return };
    let Some(body) = f.children().last().filter(|b| b.text_range().start() > params.text_range().end()) else { return };
    if obvious(e, &body) {
        return;
    }
    if let Some(t) = value_type(e, &body)
        && let Some(close) = params.last_token()
        && in_scope(&t, &close)
        && let Some(s) = shown(&t)
    {
        out.push(Hint { offset: close.text_range().end(), label: format!(": {s}"), kind: HintKind::Type });
    }
}

fn is_literal(arg: &SyntaxNode) -> bool {
    match arg.kind() {
        SyntaxKind::LITERAL | SyntaxKind::STRING_TEMPLATE => true,
        SyntaxKind::PREFIX_EXPR => arg.children().any(|c| c.kind() == SyntaxKind::LITERAL),
        _ => false,
    }
}

/// `name:` before positional arguments.
fn parameter_names(e: &Engine, list: &SyntaxNode, mode: ParameterNames, out: &mut Vec<Hint>) {
    let Some(call) = list.parent().filter(|c| c.kind() == SyntaxKind::CALL_EXPR) else { return };
    let args: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT).collect();
    let wanted = |a: &SyntaxNode| {
        a.children().all(|c| c.kind() != SyntaxKind::VALUE_ARGUMENT_NAME) && a.children().last().is_some_and(|v| mode == ParameterNames::All || is_literal(&v))
    };
    if !args.iter().any(wanted) {
        return;
    }
    let Some((sym, _)) = e.resolve_call_target(&call, false) else { return };
    if !matches!(sym.kind, SymbolKind::Function | SymbolKind::Constructor) {
        return;
    }
    // A single parameter is clear from the function name (`println("x")`, `listOf(1, 2)`).
    if mode == ParameterNames::Literals && sym.params.len() == 1 {
        return;
    }
    let mut vararg_seen = false;
    for (i, a) in args.iter().enumerate() {
        // Positional arguments only map by position before any named one.
        if a.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME) {
            break;
        }
        let Some(p) = sym.params.get(i).or_else(|| sym.params.last().filter(|p| p.vararg)) else { break };
        let value = a.children().last();
        let first_vararg = p.vararg && !std::mem::replace(&mut vararg_seen, true);
        if !wanted(a) || p.vararg && !first_vararg || meaningless(&p.name) {
            continue;
        }
        // `f(name)` for parameter `name` says it already.
        if value.as_ref().is_some_and(|v| v.kind() == SyntaxKind::NAME_REF && v.text().to_string().eq_ignore_ascii_case(&p.name)) {
            continue;
        }
        let label = if p.vararg { format!("...{}:", p.name) } else { format!("{}:", p.name) };
        out.push(Hint { offset: a.text_range().start(), label, kind: HintKind::Parameter });
    }
}

/// Synthesized names of Java parameters compiled without `-parameters`.
fn meaningless(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    name.strip_prefix("arg").is_some_and(digits) || name.strip_prefix('p').is_some_and(digits) || name.starts_with('$')
}
