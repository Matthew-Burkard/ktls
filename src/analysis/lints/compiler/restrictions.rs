//! Declarations that are not allowed in their form: type parameters of objects and enum
//! classes, named anonymous functions, `tailrec` on open members or without tail calls.

use super::super::*;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::OBJECT_DECLARATION | SyntaxKind::CLASS => type_parameters(&n, out),
            SyntaxKind::ANONYMOUS_FUNCTION => anonymous_name(&n, out),
            SyntaxKind::FUN if has_modifier(&n, "tailrec") => tailrec(&n, out),
            SyntaxKind::PROPERTY if has_modifier(&n, "lateinit") => unnecessary_lateinit(&n, out),
            _ => {}
        }
    }
}

fn type_parameters(n: &SyntaxNode, out: &mut Sink) {
    let Some(list) = n.children().find(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST) else { return };
    if n.kind() == SyntaxKind::OBJECT_DECLARATION {
        out.report(&TYPE_PARAMETERS_IN_OBJECT, list.text_range(), "Type parameters are not allowed for objects.", Some(Fix::delete("Remove type parameters", list.text_range())));
    } else if has_modifier(n, "enum") {
        out.report(&TYPE_PARAMETERS_IN_ENUM, list.text_range(), "Enum class cannot have type parameters.", Some(Fix::delete("Remove type parameters", list.text_range())));
    }
}

/// `fun name() {}` as an expression: the parser takes the name for a receiver type that
/// no `.` follows.
fn anonymous_name(f: &SyntaxNode, out: &mut Sink) {
    let Some(recv) = f.children().find(|c| c.kind() == SyntaxKind::RECEIVER_TYPE) else { return };
    let dotted = recv.next_sibling_or_token().into_iter().chain(std::iter::successors(recv.next_sibling_or_token(), |t| t.next_sibling_or_token())).find(|t| !t.kind().is_trivia()).is_some_and(|t| t.kind() == SyntaxKind::DOT);
    if dotted || recv.text().to_string().contains(['<', '.', '?', '(']) {
        return;
    }
    let start = f.first_token().map_or(recv.text_range().start(), |t| t.text_range().end());
    out.report(&ANONYMOUS_FUNCTION_WITH_NAME, recv.text_range(), "Anonymous functions with names are prohibited.", Some(Fix::delete("Remove the function name", TextRange::new(start, recv.text_range().end()))));
}

fn tailrec(f: &SyntaxNode, out: &mut Sink) {
    let Some(name) = name_token(f) else { return };
    let Some(kw) = modifiers(f).and_then(|m| m.modifiers().find(|t| t.text() == "tailrec")) else { return };
    let interface_member = f.parent().and_then(|b| b.parent()).is_some_and(|c| c.kind() == SyntaxKind::CLASS && ast::token(&c, SyntaxKind::INTERFACE_KW).is_some());
    let overridable = has_modifier(f, "open") || has_modifier(f, "abstract") || interface_member || has_modifier(f, "override") && !has_modifier(f, "final") && f.ancestors().find(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL)).is_some_and(|c| c.kind() == SyntaxKind::CLASS && (has_modifier(&c, "open") || has_modifier(&c, "abstract") || has_modifier(&c, "sealed")));
    if overridable {
        out.report(&TAILREC_ON_VIRTUAL_MEMBER_ERROR, kw.text_range(), "Tailrec is not allowed on open members.", None);
        return;
    }
    let Some(decl) = ast::FunDecl::cast(f.clone()) else { return };
    if decl.body_block().is_none() && !decl.has_expression_body() {
        return;
    }
    let fname = ast::ident_text(&name);
    let calls_itself = f.descendants().filter(|d| d.kind() == SyntaxKind::CALL_EXPR).any(|c| c.first_child().is_some_and(|n| n.kind() == SyntaxKind::NAME_REF && n.text() == fname.as_str()));
    if !calls_itself {
        let end = kw.next_token().filter(|t| t.kind().is_trivia()).map_or(kw.text_range().end(), |t| t.text_range().end());
        out.report(&NO_TAIL_CALLS_FOUND, name.text_range(), "A function is marked as tail-recursive but no tail calls are found.", Some(Fix::delete("Remove 'tailrec' modifier", TextRange::new(kw.text_range().start(), end))));
    }
}

/// A `lateinit var` of a class without secondary constructors that an `init` block
/// assigns directly (not in a branch or lambda).
fn unnecessary_lateinit(p: &SyntaxNode, out: &mut Sink) {
    let Some(body) = p.parent().filter(|b| b.kind() == SyntaxKind::CLASS_BODY) else { return };
    let Some(name) = name_token(p).map(|t| ast::ident_text(&t)) else { return };
    if body.children().any(|m| m.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR) {
        return;
    }
    let assigned = body.children().filter(|m| m.kind() == SyntaxKind::CLASS_INITIALIZER).flat_map(|i| i.children().filter(|c| c.kind() == SyntaxKind::BLOCK).flat_map(|b| b.children().collect::<Vec<_>>()).collect::<Vec<_>>()).any(|s| {
        assignment_op(&s).is_some_and(|o| o.kind() == SyntaxKind::EQ)
            && s.first_child().is_some_and(|t| t.text() == name.as_str() || t.text() == format!("this.{name}").as_str())
    });
    // Only when nothing else in an `init` block or initializer could read it first.
    if !assigned {
        return;
    }
    let Some(kw) = modifiers(p).and_then(|m| m.modifiers().find(|t| t.text() == "lateinit")) else { return };
    let end = kw.next_token().filter(|t| t.kind().is_trivia()).map_or(kw.text_range().end(), |t| t.text_range().end());
    out.report(&UNNECESSARY_LATEINIT, kw.text_range(), "Lateinit is unnecessary: definitely initialized in constructors", Some(Fix::delete("Remove 'lateinit' modifier", TextRange::new(kw.text_range().start(), end))));
}
