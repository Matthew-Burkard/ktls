//! Control-flow and scope checks: missing `return`, unreachable code, shadowing.

use super::super::*;
use crate::analysis::ide::infer::Engine;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::FUN => missing_return(cx, &n, out),
            SyntaxKind::BLOCK => unreachable(&n, out),
            _ => {}
        }
    }
    if let Some(e) = cx.engine() {
        for n in cx.root.descendants() {
            if in_error(&n) {
                continue;
            }
            match n.kind() {
                SyntaxKind::PROPERTY if is_local(&n) => shadowing(e, &n, out),
                SyntaxKind::LAMBDA_EXPR => {
                    for p in n.children().filter(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST).flat_map(|l| l.children().collect::<Vec<_>>()) {
                        lambda_parameter(e, &n, &p, out);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Nodes of `body` that belong to the function itself (not to nested functions or classes).
fn own_descendants(body: &SyntaxNode) -> impl Iterator<Item = SyntaxNode> + '_ {
    body.descendants().filter(move |d| {
        !d.ancestors().take_while(|a| a != body).any(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::CLASS | SyntaxKind::OBJECT_LITERAL | SyntaxKind::OBJECT_DECLARATION))
    })
}

fn missing_return(cx: &Cx, f: &SyntaxNode, out: &mut Sink) {
    let decl = ast::FunDecl::cast(f.clone()).unwrap();
    let (Some(ty), Some(block)) = (decl.return_type(), decl.body_block()) else { return };
    let ty_text = ty.syntax().text().to_string();
    if matches!(ty_text.as_str(), "Unit" | "kotlin.Unit" | "Nothing" | "kotlin.Nothing") {
        return;
    }
    let block = block.syntax().clone();
    // Any `return`/`throw` (labelled returns leave lambdas, not the function).
    let exits = own_descendants(&block).any(|d| match d.kind() {
        SyntaxKind::RETURN_EXPR => !d.children().any(|c| matches!(c.kind(), SyntaxKind::LABEL | SyntaxKind::LABEL_QUALIFIER)),
        SyntaxKind::THROW_EXPR => true,
        _ => false,
    });
    if exits {
        return;
    }
    // The last statement could be a loop that never ends, or a call returning `Nothing`
    // (`TODO()`, `error()`): calls only count when their type is known not to be `Nothing`.
    if let Some(last) = block.children().last() {
        if matches!(last.kind(), SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR | SyntaxKind::FOR_EXPR) {
            return;
        }
        let has_calls = last.descendants().any(|d| d.kind() == SyntaxKind::CALL_EXPR);
        if has_calls {
            let simple_call = matches!(last.kind(), SyntaxKind::CALL_EXPR | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR)
                && last.descendants().filter(|d| d.kind() == SyntaxKind::CALL_EXPR).count() == 1
                && !last.descendants().any(|d| d.kind() == SyntaxKind::LAMBDA_EXPR);
            let returns_something = simple_call
                && cx.engine().and_then(|e| e.type_of(&last)).is_some_and(|t| crate::analysis::ide::infer::fq_of(&t).is_some_and(|fq| fq != "kotlin.Nothing"));
            if !returns_something {
                return;
            }
        }
    }
    let Some(close) = block.last_token().filter(|t| t.kind() == SyntaxKind::R_CURLY) else { return };
    out.report(&NO_RETURN, close.text_range(), "Missing return statement.", None);
}

fn unreachable(block: &SyntaxNode, out: &mut Sink) {
    let stmts: Vec<SyntaxNode> = block.children().collect();
    let Some(i) = stmts.iter().position(|s| matches!(s.kind(), SyntaxKind::RETURN_EXPR | SyntaxKind::THROW_EXPR | SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR)) else { return };
    let rest = &stmts[i + 1..];
    let (Some(first), Some(last)) = (rest.first(), rest.last()) else { return };
    // Local declarations may still be referenced (functions, classes).
    if rest.iter().any(|s| matches!(s.kind(), SyntaxKind::FUN | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION)) {
        return;
    }
    let range = TextRange::new(first.text_range().start(), last.text_range().end());
    let del = TextRange::new(stmts[i].text_range().end(), last.text_range().end());
    out.report(&UNREACHABLE_CODE, range, "Unreachable code.", Some(Fix::delete("Remove unreachable code", del)));
}

/// A local `val`/`var` that redeclares one in the same block (error) or hides an outer
/// local (warning).
fn shadowing(e: &Engine, p: &SyntaxNode, out: &mut Sink) {
    let mut names: Vec<SyntaxToken> = name_token(p).into_iter().collect();
    for d in p.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) {
        names.extend(d.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_ENTRY).filter_map(|c| name_token(&c)));
    }
    for tok in names {
        let name = ast::ident_text(&tok);
        if name == "_" {
            continue;
        }
        let Some(outer) = e.find_local(p, &name) else { continue };
        if outer.name_range == tok.text_range() {
            continue;
        }
        let same_block = crate::syntax::covering(&e.root, outer.name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| a.kind() == SyntaxKind::PROPERTY).is_some_and(|d| d.parent() == p.parent());
        if same_block {
            out.report(&REDECLARATION_LOCAL, tok.text_range(), format!("Conflicting declarations: '{name}'."), None);
        } else {
            out.report(&NAME_SHADOWING, tok.text_range(), format!("Name shadowed: '{name}'."), None);
        }
    }
}

fn lambda_parameter(e: &Engine, lambda: &SyntaxNode, p: &SyntaxNode, out: &mut Sink) {
    let mut names: Vec<SyntaxToken> = name_token(p).into_iter().collect();
    for d in p.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) {
        names.extend(d.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_ENTRY).filter_map(|c| name_token(&c)));
    }
    for tok in names {
        let name = ast::ident_text(&tok);
        if name == "_" {
            continue;
        }
        // Search from outside the lambda.
        if let Some(outer) = e.find_local(lambda, &name)
            && outer.name_range != tok.text_range()
            && outer.kind != crate::analysis::ide::infer::LocalKind::It
        {
            out.report(&NAME_SHADOWING, tok.text_range(), format!("Name shadowed: '{name}'."), None);
        }
    }
}
