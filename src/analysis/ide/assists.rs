//! Refactoring assists at the cursor or selection: extract variable, specify / remove an
//! explicit type, convert an expression body to a block body, add names to call
//! arguments, and replace `it` with a named lambda parameter.

use std::path::Path;

use super::completion::{import_edit, suggested_name};
use super::infer::{Def, Engine, LocalKind, strip};
use super::inlay::{in_scope, shown, value_type};
use crate::analysis::diagnostics::{LocalsCache, scope_at};
use crate::analysis::lints::{Edit, Fix, indent_at, initializer};
use crate::analysis::resolve::Resolved;
use crate::analysis::{FileData, Snapshot};
use crate::symbols::{TypeArg, TypeRef};
use crate::syntax::ast::{self, AstNode};
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};

pub const EXTRACT: &str = "refactor.extract";
pub const REWRITE: &str = "refactor.rewrite";
pub const INLINE: &str = "refactor.inline";

pub fn assists(snap: &Snapshot, path: &Path, range: TextRange) -> Vec<(&'static str, Fix)> {
    let (Some(e), Some(file)) = (Engine::new(snap, path), snap.file(path)) else { return Vec::new() };
    let mut out = Vec::new();
    if !range.is_empty() {
        out.extend(extract_variable(&e, file, range).into_iter().map(|f| (EXTRACT, f)));
        out.extend(super::extract::extract_function(&e, file, range).map(|f| (EXTRACT, f)));
    }
    // Between two tokens: the name or keyword (`n|)` means `n`).
    let at = e.root.token_at_offset(range.start());
    let Some(tok) = at.clone().find(|t| t.kind() == SyntaxKind::IDENT || t.kind().is_keyword()).or_else(|| at.right_biased()) else { return out };
    out.extend(explicit_type(&e, file, &tok).map(|f| (REWRITE, f)));
    out.extend(block_body(&e, file, &tok).map(|f| (REWRITE, f)));
    out.extend(argument_names(&e, &tok).map(|f| (REWRITE, f)));
    out.extend(name_it(&e, &tok).map(|f| (REWRITE, f)));
    out.extend(super::rewrites::rewrites(&e, file, &tok).into_iter().map(|f| (if f.title.starts_with("Inline") { INLINE } else { REWRITE }, f)));
    out
}

// ------------------------------------------------------------ types as text

/// `t` written in this file at `at`, with the imports it needs. `None` if it cannot be
/// written (unknown parts, type parameters of a called function).
pub(super) fn type_text(e: &Engine, file: &FileData, t: &TypeRef, at: &SyntaxToken) -> Option<(String, Vec<Edit>)> {
    if !in_scope(t, at) {
        return None;
    }
    let text = shown(t)?;
    let parent = at.parent()?;
    let scope = scope_at(&parent, &e.package(), &mut LocalsCache::default());
    let mut fqs = Vec::new();
    named(t, &mut fqs);
    let mut edits = Vec::new();
    for fq in fqs {
        let simple = fq.rsplit('.').next().unwrap_or(&fq);
        let resolves = matches!(e.res.resolve_type_name(&[simple], &scope), Ok(Resolved::Classifier(c)) if c.fq_name() == fq);
        if !resolves {
            let (range, text) = import_edit(file, &fq);
            edits.push(Edit { range, text });
        }
    }
    Some((text, edits))
}

fn named(t: &TypeRef, out: &mut Vec<String>) {
    match t {
        TypeRef::Named { fq: Some(fq), args, .. } => {
            if !out.contains(&fq.to_string()) {
                out.push(fq.to_string());
            }
            for a in args {
                if let TypeArg::Type { ty, .. } = a {
                    named(ty, out);
                }
            }
        }
        TypeRef::Function { receiver, params, ret, .. } => {
            for p in receiver.iter().map(|r| &**r).chain(params).chain(std::iter::once(&**ret)) {
                named(p, out);
            }
        }
        TypeRef::Platform(inner) => named(inner, out),
        _ => {}
    }
}

// ------------------------------------------------------------ extract variable

/// Contexts that evaluate their contents conditionally or repeatedly: moving an expression
/// out of them would change when it runs.
pub(super) const LAZY: &[SyntaxKind] = &[
    SyntaxKind::THEN_BRANCH,
    SyntaxKind::ELSE_BRANCH,
    SyntaxKind::WHEN_ENTRY,
    SyntaxKind::BODY,
    SyntaxKind::LAMBDA_EXPR,
    SyntaxKind::ANONYMOUS_FUNCTION,
    SyntaxKind::OBJECT_LITERAL,
    SyntaxKind::TRY_EXPR,
    SyntaxKind::WHILE_EXPR,
    SyntaxKind::DO_WHILE_EXPR,
];

fn extract_variable(e: &Engine, file: &FileData, range: TextRange) -> Vec<Fix> {
    let Some(expr) = selected_expression(e, file, range) else { return Vec::new() };
    let text = &file.text;
    let value = expr.text().to_string();
    let mut out = Vec::new();
    let declare = |statement: &SyntaxNode, name: &str| Edit { range: TextRange::empty(statement.text_range().start()), text: format!("val {name} = {value}\n{}", indent_at(text, statement.text_range().start())) };
    if let Some(statement) = statement_of(&expr) {
        let name = unique_name(e, &statement, &name_for(e, &expr));
        let edits = if statement == expr {
            vec![Edit { range: expr.text_range(), text: format!("val {name} = {value}") }]
        } else {
            vec![declare(&statement, &name), Edit { range: expr.text_range(), text: name.clone() }]
        };
        out.push(Fix::new("Extract variable", edits));
    }
    // Every equal expression (whitespace aside) in the outermost block of the function or
    // lambda, declared before the first one.
    let Some(scope) = expr.ancestors().take_while(|a| !matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::LAMBDA_EXPR | SyntaxKind::ANONYMOUS_FUNCTION)).filter(|a| a.kind() == SyntaxKind::BLOCK).last() else { return out };
    let norm = |n: &SyntaxNode| n.text().to_string().split_whitespace().collect::<String>();
    let key = norm(&expr);
    let mut occurrences: Vec<SyntaxNode> = scope.descendants().filter(|d| d.kind() == expr.kind() && norm(d) == key).collect();
    occurrences.dedup_by(|b, a| a.text_range().contains_range(b.text_range()));
    if occurrences.len() < 2 {
        return out;
    }
    let Some(first_stmt) = statement_of(&occurrences[0]).filter(|s| s.parent().as_ref() == Some(&scope) && *s != occurrences[0]) else { return out };
    let name = unique_name(e, &first_stmt, &name_for(e, &expr));
    let mut edits = vec![declare(&first_stmt, &name)];
    edits.extend(occurrences.iter().map(|o| Edit { range: o.text_range(), text: name.clone() }));
    out.push(Fix::new(format!("Extract variable (replace all {} occurrences)", occurrences.len()), edits));
    out
}

/// The selected expression and the block statement it belongs to, when it can be moved
/// in front of that statement.
pub(super) fn extraction_site(e: &Engine, file: &FileData, range: TextRange) -> Option<(SyntaxNode, SyntaxNode)> {
    let expr = selected_expression(e, file, range)?;
    let statement = statement_of(&expr)?;
    Some((expr, statement))
}

/// The expression exactly covered by the selection (whitespace aside).
fn selected_expression(e: &Engine, file: &FileData, range: TextRange) -> Option<SyntaxNode> {
    let text = &file.text;
    let sel = &text[usize::from(range.start())..usize::from(range.end())];
    let lead = sel.len() - sel.trim_start().len();
    let range = TextRange::new(range.start() + TextSize::from(lead as u32), range.start() + TextSize::from(sel.trim_end().len() as u32));
    let mut expr = match e.root.covering_element(range) {
        rowan::NodeOrToken::Node(n) => n,
        rowan::NodeOrToken::Token(t) => t.parent()?,
    };
    // The innermost node may be a token's parent (`NAME_REF`); climb to the outermost
    // expression with exactly this range.
    while !is_expression(&expr) && expr.text_range() == range {
        expr = expr.parent()?;
    }
    while expr.parent().is_some_and(|p| p.text_range() == range && is_expression(&p)) {
        expr = expr.parent()?;
    }
    (expr.text_range() == range && is_expression(&expr)).then_some(expr)
}

/// The statement of the enclosing block that `expr` can be evaluated in front of: not
/// across conditional, short-circuit, loop or lambda code.
fn statement_of(expr: &SyntaxNode) -> Option<SyntaxNode> {
    let mut statement = expr.clone();
    loop {
        let parent = statement.parent()?;
        if parent.kind() == SyntaxKind::BLOCK {
            break;
        }
        if LAZY.contains(&parent.kind()) || short_circuit_right(&parent, &statement) {
            return None;
        }
        statement = parent;
    }
    if statement.kind() == SyntaxKind::PROPERTY && statement.text_range() == expr.text_range() {
        return None;
    }
    Some(statement)
}

pub(super) fn is_expression(n: &SyntaxNode) -> bool {
    use SyntaxKind::*;
    matches!(
        n.kind(),
        NAME_REF | CALL_EXPR | DOT_QUALIFIED_EXPR | SAFE_QUALIFIED_EXPR | BINARY_EXPR | PREFIX_EXPR | POSTFIX_EXPR | PAREN_EXPR | LITERAL | STRING_TEMPLATE | IF_EXPR | WHEN_EXPR | LAMBDA_EXPR | INDEX_EXPR | BINARY_WITH_TYPE | IS_EXPR | THIS_EXPR | CALLABLE_REFERENCE | OBJECT_LITERAL | TRY_EXPR | COLLECTION_LITERAL
    ) && !(n.kind() == NAME_REF && n.parent().is_some_and(|p| matches!(p.kind(), CALL_EXPR | DOT_QUALIFIED_EXPR | SAFE_QUALIFIED_EXPR) && p.first_child().as_ref() != Some(n)))
}

/// `child` is the right operand of `&&`, `||`, `?:` or the selector of `?.` in `parent`.
pub(super) fn short_circuit_right(parent: &SyntaxNode, child: &SyntaxNode) -> bool {
    let first = parent.first_child().as_ref() == Some(child);
    match parent.kind() {
        SyntaxKind::BINARY_EXPR => !first && parent.children().find(|c| c.kind() == SyntaxKind::OPERATION_REFERENCE).is_some_and(|o| matches!(o.text().to_string().as_str(), "&&" | "||" | "?:")),
        SyntaxKind::SAFE_QUALIFIED_EXPR => !first,
        _ => false,
    }
}

/// A name for the value of `expr`: the called function or property (`getUser()` → `user`),
/// else from its type.
fn name_for(e: &Engine, expr: &SyntaxNode) -> String {
    let last = match expr.kind() {
        SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR => expr.children().last(),
        _ => Some(expr.clone()),
    };
    let called = last.and_then(|l| match l.kind() {
        SyntaxKind::CALL_EXPR => l.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF),
        SyntaxKind::NAME_REF => Some(l),
        _ => None,
    });
    if let Some(n) = called.map(|c| c.text().to_string()) {
        let stripped = ["get", "is", "to", "as", "create", "find", "load", "read", "compute"].iter().find_map(|p| n.strip_prefix(p).filter(|r| r.starts_with(char::is_uppercase)));
        match stripped {
            Some(r) => {
                let mut c = r.chars();
                return c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default();
            }
            None if !n.starts_with(char::is_uppercase) && expr.kind() != SyntaxKind::NAME_REF => return n,
            _ => {}
        }
    }
    e.type_of(expr).map(|t| suggested_name(strip(&t))).unwrap_or_else(|| "value".into())
}

/// `base`, or `base2`, `base3`... if a local of that name is visible at `at`.
pub(super) fn unique_name(e: &Engine, at: &SyntaxNode, base: &str) -> String {
    let taken: Vec<String> = e.locals_at(at).into_iter().map(|l| l.name).collect();
    let mut name = base.to_string();
    let mut i = 2;
    while taken.contains(&name) {
        name = format!("{base}{i}");
        i += 1;
    }
    name
}

// ------------------------------------------------------------ explicit types

/// "Specify type explicitly" on a property without a type; "Remove explicit type" when the
/// initializer has exactly the declared type.
fn explicit_type(e: &Engine, file: &FileData, tok: &SyntaxToken) -> Option<Fix> {
    let prop = tok.parent_ancestors().take_while(|a| !matches!(a.kind(), SyntaxKind::BLOCK | SyntaxKind::CLASS_BODY)).find(|a| a.kind() == SyntaxKind::PROPERTY)?;
    // On the declaration, not in its initializer.
    let value = initializer(&prop)?;
    if tok.text_range().start() >= value.text_range().start() {
        return None;
    }
    let name = ast::token(&prop, SyntaxKind::IDENT)?;
    let declared = ast::child::<ast::TypeRef>(&prop);
    let t = value_type(e, &value)?;
    match declared {
        None => {
            let (text, mut edits) = type_text(e, file, &t, &name)?;
            edits.push(Edit { range: TextRange::empty(name.text_range().end()), text: format!(": {text}") });
            Some(Fix::new(format!("Specify type explicitly ({text})"), edits))
        }
        Some(ty) => {
            if e.resolve_syntax_type(&ty).display() != t.display() || value.kind() == SyntaxKind::LITERAL && value.text() == "null" {
                return None;
            }
            Some(Fix::delete("Remove explicit type", TextRange::new(name.text_range().end(), ty.syntax().text_range().end())))
        }
    }
}

// ------------------------------------------------------------ block body

/// `fun f() = expr` → `fun f(): T { return expr }` (on the declaration).
fn block_body(e: &Engine, file: &FileData, tok: &SyntaxToken) -> Option<Fix> {
    let f = tok.parent_ancestors().take_while(|a| a.kind() != SyntaxKind::BLOCK).find(|a| a.kind() == SyntaxKind::FUN)?;
    let decl = ast::FunDecl::cast(f.clone())?;
    if !decl.has_expression_body() {
        return None;
    }
    let value = initializer(&f)?;
    if tok.text_range().start() >= value.text_range().start() {
        return None;
    }
    let eq = ast::token(&f, SyntaxKind::EQ)?;
    let indent = indent_at(&file.text, f.text_range().start());
    let t = match decl.return_type() {
        Some(r) => e.resolve_syntax_type(&r),
        None => value_type(e, &value)?,
    };
    let unit = matches!(t.fq_name(), Some("kotlin.Unit"));
    let nothing = matches!(t.fq_name(), Some("kotlin.Nothing"));
    let mut edits = Vec::new();
    // `: T` when the type was inferred (a block body needs it), in the same edit as the body.
    let mut declared = String::new();
    if decl.return_type().is_none() && !unit {
        let close = f.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST)?;
        let (text, imports) = type_text(e, file, &t, &close.last_token()?)?;
        edits.extend(imports);
        declared = format!(": {text}");
    }
    let stmt = if unit || nothing { value.text().to_string() } else { format!("return {}", value.text()) };
    // From the end of the parameter list / return type, through `= expr`.
    let start = eq.prev_token().filter(|p| p.kind() == SyntaxKind::WHITESPACE).map_or(eq.text_range().start(), |p| p.text_range().start());
    edits.push(Edit { range: TextRange::new(start, value.text_range().end()), text: format!("{declared} {{\n{indent}    {stmt}\n{indent}}}") });
    Some(Fix::new("Convert to block body", edits))
}

// ------------------------------------------------------------ argument names

/// Name every positional argument of the call around the cursor (Kotlin functions only).
fn argument_names(e: &Engine, tok: &SyntaxToken) -> Option<Fix> {
    let list = tok.parent_ancestors().take_while(|a| !matches!(a.kind(), SyntaxKind::BLOCK | SyntaxKind::LAMBDA_EXPR)).find(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT_LIST)?;
    let call = list.parent().filter(|c| c.kind() == SyntaxKind::CALL_EXPR)?;
    let (sym, _) = e.resolve_call_target(&call, false)?;
    if !matches!(sym.origin, crate::symbols::Origin::Source { .. }) && sym.params.iter().any(|p| p.name.starts_with("arg") || p.name.starts_with('p') && p.name[1..].parse::<u32>().is_ok()) {
        return None;
    }
    let args: Vec<SyntaxNode> = list.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).collect();
    let mut edits = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME) {
            break;
        }
        let p = sym.params.get(i)?;
        if p.vararg || ast::token(a, SyntaxKind::STAR).is_some() {
            return None;
        }
        edits.push(Edit { range: TextRange::empty(a.text_range().start()), text: format!("{} = ", p.name) });
    }
    if edits.is_empty() {
        return None;
    }
    let title = if edits.len() == 1 { "Add name to argument" } else { "Add names to call arguments" };
    Some(Fix::new(title, edits))
}

// ------------------------------------------------------------ it → named parameter

/// On `it` (or the lambda's `{`): `{ it.x }` → `{ user -> user.x }`.
fn name_it(e: &Engine, tok: &SyntaxToken) -> Option<Fix> {
    let lambda = if tok.kind() == SyntaxKind::L_CURLY {
        tok.parent().filter(|p| p.kind() == SyntaxKind::LAMBDA_EXPR)?
    } else {
        let r = tok.parent().filter(|p| p.kind() == SyntaxKind::NAME_REF && tok.text() == "it")?;
        let defs = e.resolve_name_ref(&r);
        let [Def::Local(l)] = defs.as_slice() else { return None };
        if l.kind != LocalKind::It {
            return None;
        }
        let brace = e.root.covering_element(l.name_range);
        brace.ancestors().find(|a| a.kind() == SyntaxKind::LAMBDA_EXPR)?
    };
    if lambda.children_with_tokens().any(|t| matches!(t.kind(), SyntaxKind::ARROW | SyntaxKind::VALUE_PARAMETER_LIST)) {
        return None;
    }
    let (_, params) = e.lambda_signature(&lambda)?;
    let [param] = params.as_slice() else { return None };
    let open = lambda.first_token()?;
    let body = lambda.children().find(|c| c.kind() == SyntaxKind::BLOCK)?;
    let name = unique_name(e, &body, &suggested_name(strip(param)));
    let mut edits = vec![Edit { range: TextRange::empty(open.text_range().end()), text: format!(" {name} ->") }];
    // Uses of this lambda's `it` (not of nested lambdas).
    for r in body.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF && d.text() == "it") {
        if let [Def::Local(l)] = e.resolve_name_ref(&r).as_slice()
            && l.kind == LocalKind::It
            && e.root.covering_element(l.name_range).ancestors().find(|a| a.kind() == SyntaxKind::LAMBDA_EXPR).as_ref() == Some(&lambda)
        {
            edits.push(Edit { range: r.text_range(), text: name.clone() });
        }
    }
    Some(Fix::new(format!("Replace 'it' with explicit parameter '{name}'"), edits))
}
