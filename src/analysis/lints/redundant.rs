//! Code that can be removed without changing behaviour.

use super::*;

/// Soft keywords that are safer to keep quoted.
const KEEP_BACKTICKS: &[&str] = &["get", "set", "field", "value", "init", "constructor", "where", "by", "catch", "finally", "import", "companion", "dynamic"];

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for el in cx.root.descendants_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::SEMI => semicolon(&t, out),
                SyntaxKind::IDENT if t.text().starts_with('`') => backticks(&t, out),
                _ => {}
            },
            rowan::NodeOrToken::Node(n) => {
                if in_error(&n) {
                    continue;
                }
                match n.kind() {
                    SyntaxKind::LONG_STRING_TEMPLATE_ENTRY => template_braces(cx, &n, out),
                    SyntaxKind::MODIFIER_LIST => modifier_list(cx, &n, out),
                    SyntaxKind::FUN => {
                        unit_return_type(&n, out);
                        redundant_suspend(cx, &n, out);
                        redundant_override(cx, &n, out);
                        nullable_return_type(cx, &n, out);
                    }
                    SyntaxKind::CALL_EXPR if n.first_child().is_some_and(|c| matches!(c.text().to_string().as_str(), "requireNotNull" | "checkNotNull")) => require_not_null(cx, &n, out),
                    SyntaxKind::RETURN_EXPR => elvis_return_null(cx, &n, out),
                    SyntaxKind::IF_EXPR => redundant_if(&n, out),
                    SyntaxKind::CLASS_BODY => empty_class_body(&n, out),
                    SyntaxKind::PRIMARY_CONSTRUCTOR => {
                        empty_constructor(&n, out);
                        constructor_keyword(&n, out);
                    }
                    SyntaxKind::ENUM_ENTRY => enum_entry_parens(&n, out),
                    SyntaxKind::CALL_EXPR => lambda_parens(&n, out),
                    SyntaxKind::PROPERTY_ACCESSOR => accessor(&n, out),
                    SyntaxKind::CLASS_INITIALIZER => init_block(cx, &n, out),
                    SyntaxKind::NAME_REF if n.text() == "Unit" => unit_expression(cx, &n, out),
                    SyntaxKind::BLOCK => unnecessary_variable(&n, out),
                    SyntaxKind::STRING_TEMPLATE => string_template(cx, &n, out),
                    SyntaxKind::LAMBDA_EXPR => lambda_arrow(cx, &n, out),
                    SyntaxKind::TYPE_ARGUMENT_LIST => explicit_type_arguments(cx, &n, out),
                    SyntaxKind::DOT_QUALIFIED_EXPR => {
                        useless_call_on_not_null(cx, &n, out);
                        expression_qualifier(cx, &n, out);
                        companion_reference(cx, &n, out);
                        as_sequence(cx, &n, out);
                        run_catching(cx, &n, out);
                    }
                    SyntaxKind::PROPERTY => explicit_type(cx, &n, out),
                    SyntaxKind::USER_TYPE => type_qualifier(cx, &n, out),
                    _ => {}
                }
            }
        }
    }
}

fn semicolon(t: &SyntaxToken, out: &mut Sink) {
    let Some(parent) = t.parent() else { return };
    // `enum class E { A, B; fun f() }` needs it; so do empty loop/branch bodies (`while (x);`).
    if parent.children().any(|c| c.kind() == SyntaxKind::ENUM_ENTRY)
        || matches!(
            parent.kind(),
            SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR | SyntaxKind::IF_EXPR | SyntaxKind::THEN_BRANCH | SyntaxKind::ELSE_BRANCH | SyntaxKind::BODY | SyntaxKind::WHEN_ENTRY | SyntaxKind::ERROR
        )
    {
        return;
    }
    if prev_significant(t).is_some_and(|p| matches!(p.kind(), SyntaxKind::SEMI | SyntaxKind::ELSE_KW | SyntaxKind::ARROW | SyntaxKind::R_PAREN | SyntaxKind::DO_KW)) {
        return;
    }
    match next_significant(t, false) {
        None => {}
        Some(n) if n.kind() == SyntaxKind::R_CURLY || n.kind() == SyntaxKind::EOF => {}
        Some(n) if n.kind() == SyntaxKind::NEWLINE => {
            // Without the `;` the next line could continue this statement.
            if next_significant(&n, true).is_some_and(|f| {
                matches!(
                    f.kind(),
                    SyntaxKind::L_CURLY | SyntaxKind::L_PAREN | SyntaxKind::L_BRACK | SyntaxKind::DOT | SyntaxKind::SAFE_DOT | SyntaxKind::ELVIS | SyntaxKind::AMP2 | SyntaxKind::PIPE2 | SyntaxKind::COLON2 | SyntaxKind::QUESTION
                )
            }) {
                return;
            }
        }
        Some(_) => return,
    }
    out.report(&REDUNDANT_SEMICOLON, t.text_range(), "Redundant semicolon", Some(Fix::delete("Remove semicolon", t.text_range())));
}

fn backticks(t: &SyntaxToken, out: &mut Sink) {
    let inner = ast::ident_text(t);
    let mut chars = inner.chars();
    let valid = chars.next().is_some_and(|c| c.is_alphabetic() || c == '_') && chars.all(|c| c.is_alphanumeric() || c == '_');
    if !valid || inner.chars().all(|c| c == '_') || SyntaxKind::from_keyword(&inner).is_some() || KEEP_BACKTICKS.contains(&inner.as_str()) {
        return;
    }
    // Inside KDoc and string literals backticks are plain text; only real identifiers get here.
    out.report(&REDUNDANT_BACKTICKS, t.text_range(), "Remove redundant backticks", Some(Fix::replace("Remove backticks", t.text_range(), inner)));
}

fn template_braces(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let mut exprs = n.children();
    let (Some(e), None) = (exprs.next(), exprs.next()) else { return };
    let name = match e.kind() {
        SyntaxKind::NAME_REF => match e.first_token() {
            Some(t) if t.kind() == SyntaxKind::IDENT && !t.text().starts_with('`') => t.text().to_string(),
            _ => return,
        },
        SyntaxKind::THIS_EXPR if e.text() == "this" => "this".to_string(),
        _ => return,
    };
    if has_comments(n) {
        return;
    }
    let next = cx.text[usize::from(n.text_range().end())..].chars().next();
    if next.is_some_and(|c| c.is_alphanumeric() || c == '_') {
        return;
    }
    let dollars = n.first_token().map(|t| t.text().trim_end_matches('{').to_string()).unwrap_or_else(|| "$".into());
    out.report(
        &REDUNDANT_CURLY_BRACES,
        n.text_range(),
        "Redundant curly braces in string template",
        Some(Fix::replace("Remove curly braces", n.text_range(), format!("{dollars}{name}"))),
    );
}

fn modifier_list(cx: &Cx, list: &SyntaxNode, out: &mut Sink) {
    let Some(decl) = list.parent() else { return };
    if !matches!(
        decl.kind(),
        SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::PRIMARY_CONSTRUCTOR
    ) {
        return;
    }
    let ml = ast::ModifierList::cast(list.clone()).unwrap();
    let is_override = ml.has("override");
    let remove = |t: &SyntaxToken| {
        // The modifier and the whitespace after it.
        let end = t.next_token().filter(|n| n.kind() == SyntaxKind::WHITESPACE).map_or(t.text_range().end(), |n| n.text_range().end());
        TextRange::new(t.text_range().start(), end)
    };
    let kotlin = cx.module.map(|m| &m.kotlin);
    for m in ml.modifiers() {
        match m.text() {
            "public" if !is_override && !kotlin.is_some_and(|k| k.explicit_api) && !is_local(&decl) => {
                out.report(&REDUNDANT_VISIBILITY, m.text_range(), "Redundant visibility modifier", Some(Fix::delete("Remove 'public' modifier", remove(&m))));
            }
            "final" if !is_override => {
                // all-open compiler plugins (Spring, JPA...) make annotated classes open.
                let plugins = kotlin.is_some_and(|k| k.compiler_plugins.iter().any(|p| matches!(p.as_str(), "allopen" | "spring" | "jpa" | "all-open")));
                let annotated = decl.ancestors().any(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION) && has_annotations(&a));
                if !plugins && !annotated && !owner(&decl).is_some_and(|o| is_interface(&o)) {
                    out.report(&REDUNDANT_MODALITY, m.text_range(), "Redundant modality modifier", Some(Fix::delete("Remove 'final' modifier", remove(&m))));
                }
            }
            "open" if owner(&decl).is_some_and(|o| is_interface(&o)) && matches!(decl.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY) => {
                out.report(&REDUNDANT_MODALITY, m.text_range(), "Redundant modality modifier", Some(Fix::delete("Remove 'open' modifier", remove(&m))));
            }
            "abstract" if is_interface(&decl) => {
                out.report(&REDUNDANT_MODALITY, m.text_range(), "Redundant modality modifier", Some(Fix::delete("Remove 'abstract' modifier", remove(&m))));
            }
            "abstract" if owner(&decl).is_some_and(|o| is_interface(&o)) && !has_body(&decl) => {
                out.report(&REDUNDANT_MODALITY, m.text_range(), "Redundant modality modifier", Some(Fix::delete("Remove 'abstract' modifier", remove(&m))));
            }
            _ => {}
        }
    }
}

fn has_body(decl: &SyntaxNode) -> bool {
    match decl.kind() {
        SyntaxKind::FUN => ast::token(decl, SyntaxKind::EQ).is_some() || decl.children().any(|c| c.kind() == SyntaxKind::BLOCK),
        SyntaxKind::PROPERTY => ast::token(decl, SyntaxKind::EQ).is_some() || decl.children().any(|c| matches!(c.kind(), SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::PROPERTY_DELEGATE)),
        _ => true,
    }
}

fn unit_return_type(f: &SyntaxNode, out: &mut Sink) {
    let decl = ast::FunDecl::cast(f.clone()).unwrap();
    let Some(ty) = decl.return_type() else { return };
    let text = ty.syntax().text().to_string();
    if text != "Unit" && text != "kotlin.Unit" || decl.body_block().is_none() || decl.has_expression_body() {
        return;
    }
    let Some(colon) = ast::token(f, SyntaxKind::COLON) else { return };
    let start = prev_significant(&colon).map_or(colon.text_range().start(), |p| p.text_range().end());
    out.report(
        &REDUNDANT_UNIT_RETURN_TYPE,
        ty.syntax().text_range(),
        "Redundant 'Unit' return type",
        Some(Fix::delete("Remove explicit 'Unit' return type", TextRange::new(start, ty.syntax().text_range().end()))),
    );
}

/// The single expression of a branch (`x` or `{ x }`).
fn branch_value(b: &SyntaxNode) -> Option<SyntaxNode> {
    let e = b.first_child()?;
    if e.kind() == SyntaxKind::BLOCK { only_statement(&e) } else { Some(e) }
}

fn bool_literal(e: &SyntaxNode) -> Option<bool> {
    if e.kind() != SyntaxKind::LITERAL {
        return None;
    }
    match e.first_token()?.kind() {
        SyntaxKind::TRUE_KW => Some(true),
        SyntaxKind::FALSE_KW => Some(false),
        _ => None,
    }
}

/// `return true` / `return false` (unlabelled).
fn returned_bool(e: &SyntaxNode) -> Option<bool> {
    if e.kind() != SyntaxKind::RETURN_EXPR || e.children().any(|c| c.kind() == SyntaxKind::LABEL) {
        return None;
    }
    bool_literal(&e.children().last()?)
}

fn redundant_if(n: &SyntaxNode, out: &mut Sink) {
    let then = n.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH);
    let els = n.children().find(|c| c.kind() == SyntaxKind::ELSE_BRANCH);
    let (Some(then), Some(els)) = (then, els) else { return };
    let (Some(t), Some(e)) = (branch_value(&then), branch_value(&els)) else { return };
    let Some(cond) = n.children().find(|c| c.kind() == SyntaxKind::CONDITION).and_then(|c| c.first_child()) else { return };
    let (value, is_return) = match (bool_literal(&t), bool_literal(&e), returned_bool(&t), returned_bool(&e)) {
        (Some(a), Some(b), _, _) if a != b => (a, false),
        (_, _, Some(a), Some(b)) if a != b => (a, true),
        _ => return,
    };
    let Some(if_kw) = ast::token(n, SyntaxKind::IF_KW) else { return };
    let expr = if value { cond.text().to_string() } else { format!("!{}", operand(&cond)) };
    let replacement = if is_return { format!("return {expr}") } else { expr };
    let fix = (!has_comments(n)).then(|| Fix::replace("Simplify 'if'", n.text_range(), replacement));
    out.report(&REDUNDANT_IF, if_kw.text_range(), "Redundant 'if' statement", fix);
}

fn empty_class_body(body: &SyntaxNode, out: &mut Sink) {
    if !body.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION)) {
        return;
    }
    let empty = body.children_with_tokens().all(|e| matches!(e.kind(), SyntaxKind::L_CURLY | SyntaxKind::R_CURLY | SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE));
    if !empty {
        return;
    }
    let Some(open) = body.first_token() else { return };
    let start = prev_significant(&open).map_or(body.text_range().start(), |p| p.text_range().end());
    out.report(&REDUNDANT_EMPTY_CLASS_BODY, body.text_range(), "Redundant empty class body", Some(Fix::delete("Remove empty class body", TextRange::new(start, body.text_range().end()))));
}

fn empty_constructor(ctor: &SyntaxNode, out: &mut Sink) {
    let Some(class) = ctor.parent() else { return };
    if modifiers(ctor).is_some() || ast::token(ctor, SyntaxKind::IDENT).is_some() || has_comments(ctor) {
        return;
    }
    let Some(params) = ctor.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST) else { return };
    if params.children().next().is_some() {
        return;
    }
    let secondary = class.children().filter(|c| c.kind() == SyntaxKind::CLASS_BODY).any(|b| b.children().any(|m| m.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR));
    if secondary || ["data", "value", "inline", "expect", "actual"].iter().any(|m| has_modifier(&class, m)) {
        return;
    }
    out.report(&REDUNDANT_EMPTY_CONSTRUCTOR, ctor.text_range(), "Remove empty primary constructor", Some(Fix::delete("Remove empty primary constructor", ctor.text_range())));
}

/// `class A constructor(x: Int)`.
fn constructor_keyword(ctor: &SyntaxNode, out: &mut Sink) {
    let Some(kw) = ast::token(ctor, SyntaxKind::IDENT).filter(|t| t.text() == "constructor") else { return };
    if modifiers(ctor).is_some() || has_comments(ctor) {
        return;
    }
    let start = prev_significant(&kw).map_or(kw.text_range().start(), |t| t.text_range().end());
    out.report(&REDUNDANT_CONSTRUCTOR_KEYWORD, kw.text_range(), "Redundant 'constructor' keyword", Some(Fix::delete("Remove redundant 'constructor' keyword", TextRange::new(start, kw.text_range().end()))));
}

/// `enum class E { A(), B }`.
fn enum_entry_parens(entry: &SyntaxNode, out: &mut Sink) {
    let Some(args) = entry.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST) else { return };
    if args.children().next().is_some() || has_comments(&args) {
        return;
    }
    out.report(&REDUNDANT_ENUM_CONSTRUCTOR_INVOCATION, args.text_range(), "Redundant enum constructor invocation", Some(Fix::delete("Remove enum constructor invocation", args.text_range())));
}

fn lambda_parens(call: &SyntaxNode, out: &mut Sink) {
    let Some(args) = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST) else { return };
    if args.children().next().is_some() || has_comments(&args) {
        return;
    }
    // Only `foo() { }`: the argument list directly precedes the trailing lambda.
    let next = args.next_sibling();
    if !next.is_some_and(|n| n.kind() == SyntaxKind::LAMBDA_ARGUMENT) {
        return;
    }
    out.report(&REDUNDANT_LAMBDA_PARENS, args.text_range(), "Unnecessary parentheses in function call with lambda", Some(Fix::delete("Remove unnecessary parentheses", args.text_range())));
}

fn accessor(acc: &SyntaxNode, out: &mut Sink) {
    let Some(kw) = acc.first_token() else { return };
    if modifiers(acc).is_some() || has_comments(acc) {
        return;
    }
    let Some(prop) = acc.parent() else { return };
    let bare = ast::token(acc, SyntaxKind::L_PAREN).is_none();
    let field_only = |e: &SyntaxNode| e.kind() == SyntaxKind::NAME_REF && e.text() == "field";
    let redundant = match kw.text() {
        "get" => {
            let body = acc.children().find(|c| c.kind() != SyntaxKind::TYPE_REFERENCE && c.kind() != SyntaxKind::VALUE_PARAMETER_LIST);
            let returns_field = match &body {
                None => bare,
                Some(b) if b.kind() == SyntaxKind::BLOCK => only_statement(b).is_some_and(|s| {
                    s.kind() == SyntaxKind::RETURN_EXPR && s.children().count() == 1 && s.first_child().is_some_and(|e| field_only(&e))
                }),
                Some(e) => ast::token(acc, SyntaxKind::EQ).is_some() && field_only(e),
            };
            returns_field && ast::token(&prop, SyntaxKind::EQ).is_some()
        }
        "set" => {
            let param = acc
                .children()
                .find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST)
                .and_then(|l| l.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER));
            let body = acc.children().find(|c| c.kind() == SyntaxKind::BLOCK);
            match (param, body) {
                (None, None) => bare,
                (Some(p), Some(b)) if !has_annotations(&p) => {
                    let pname = name_token(&p).map(|t| t.text().to_string()).unwrap_or_default();
                    only_statement(&b).is_some_and(|s| {
                        assignment_op(&s).is_some_and(|op| op.kind() == SyntaxKind::EQ)
                            && binary_operands(&s).is_some_and(|(l, r)| field_only(&l) && r.kind() == SyntaxKind::NAME_REF && r.text() == pname.as_str())
                    })
                }
                _ => false,
            }
        }
        _ => false,
    };
    if !redundant {
        return;
    }
    let start = prev_significant(&kw).map_or(acc.text_range().start(), |p| p.text_range().end());
    let (rule, what) = if kw.text() == "get" { (&REDUNDANT_GETTER, "getter") } else { (&REDUNDANT_SETTER, "setter") };
    out.report(rule, acc.text_range(), format!("Redundant {what}"), Some(Fix::delete(format!("Remove redundant {what}"), TextRange::new(start, acc.text_range().end()))));
}

fn init_block(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let Some(block) = n.children().find(|c| c.kind() == SyntaxKind::BLOCK) else { return };
    if is_empty_block(&block) && !has_comments(n) {
        out.report(&REDUNDANT_INIT_BLOCK, n.text_range(), "Redundant empty initializer block", Some(Fix::delete("Remove initializer block", deletion_range(cx.text, n.text_range()))));
    }
}

fn unit_expression(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let Some(parent) = n.parent() else { return };
    match parent.kind() {
        // A `Unit` statement that is not the block's value.
        SyntaxKind::BLOCK if parent.children().last().as_ref() != Some(n) => {
            out.report(&REDUNDANT_UNIT, n.text_range(), "Redundant 'Unit'", Some(Fix::delete("Remove redundant 'Unit'", deletion_range(cx.text, n.text_range()))));
        }
        SyntaxKind::RETURN_EXPR if !parent.children().any(|c| c.kind() == SyntaxKind::LABEL) => {
            // Only in functions returning Unit with a block body.
            let Some(f) = parent.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::LAMBDA_EXPR | SyntaxKind::ANONYMOUS_FUNCTION)) else { return };
            if f.kind() != SyntaxKind::FUN {
                return;
            }
            let decl = ast::FunDecl::cast(f).unwrap();
            let unit = decl.return_type().is_none_or(|t| t.syntax().text() == "Unit");
            if !unit || decl.has_expression_body() {
                return;
            }
            let Some(ret) = parent.first_token() else { return };
            out.report(
                &REDUNDANT_UNIT,
                n.text_range(),
                "Redundant 'Unit'",
                Some(Fix::delete("Remove redundant 'Unit'", TextRange::new(ret.text_range().end(), n.text_range().end()))),
            );
        }
        _ => {}
    }
}

/// `val x = e` immediately followed by `return x`.
fn unnecessary_variable(block: &SyntaxNode, out: &mut Sink) {
    let stmts: Vec<SyntaxNode> = block.children().collect();
    for w in stmts.windows(2) {
        let (decl, ret) = (&w[0], &w[1]);
        if decl.kind() != SyntaxKind::PROPERTY || ret.kind() != SyntaxKind::RETURN_EXPR || ret.children().count() != 1 {
            continue;
        }
        let pd = ast::PropertyDecl::cast(decl.clone()).unwrap();
        if pd.is_var() || pd.type_ref().is_some() || pd.has_delegate() || pd.is_destructuring() || modifiers(decl).is_some() || has_comments(decl) {
            continue;
        }
        let (Some(name_tok), Some(init)) = (name_token(decl), initializer(decl)) else { continue };
        let name = ast::ident_text(&name_tok);
        if !ret.first_child().is_some_and(|v| v.kind() == SyntaxKind::NAME_REF && v.text() == name.as_str()) {
            continue;
        }
        let others = idents(block).filter(|t| *t != name_tok && ast::ident_text(t) == name).count();
        if others != 1 || ret.children().any(|c| c.kind() == SyntaxKind::LABEL) {
            continue;
        }
        let range = TextRange::new(decl.text_range().start(), ret.text_range().end());
        out.report(
            &UNNECESSARY_VARIABLE,
            name_tok.text_range(),
            format!("Variable '{name}' is only returned and can be inlined"),
            Some(Fix::replace(format!("Inline variable '{name}'"), range, format!("return {}", init.text()))),
        );
    }
}

/// `"$s"` where `s` is a non-null String.
fn string_template(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let mut entries = n.children();
    let (Some(entry), None) = (entries.next(), entries.next()) else { return };
    if !matches!(entry.kind(), SyntaxKind::SHORT_STRING_TEMPLATE_ENTRY | SyntaxKind::LONG_STRING_TEMPLATE_ENTRY) {
        return;
    }
    let Some(expr) = entry.first_child() else { return };
    if !cx.on(&REDUNDANT_STRING_TEMPLATE) || has_comments(n) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let Some(t) = e.type_of(&expr) else { return };
    if !matches!(&t, crate::symbols::TypeRef::Named { fq: Some(fq), nullable: false, .. } if &**fq == "kotlin.String") {
        return;
    }
    out.report(&REDUNDANT_STRING_TEMPLATE, n.text_range(), "Redundant string template", Some(Fix::replace("Remove string template", n.text_range(), expr.text().to_string())));
}

/// The call a lambda is an argument of.
fn call_of_lambda(lambda: &SyntaxNode) -> Option<SyntaxNode> {
    let arg = lambda.parent().filter(|p| matches!(p.kind(), SyntaxKind::LAMBDA_ARGUMENT | SyntaxKind::VALUE_ARGUMENT))?;
    match arg.kind() {
        SyntaxKind::LAMBDA_ARGUMENT => arg.parent(),
        _ => arg.parent()?.parent(),
    }
    .filter(|c| c.kind() == SyntaxKind::CALL_EXPR)
}

/// `{ -> … }` and `{ it -> … }`: the arrow (and `it`) say nothing the lambda would not.
fn lambda_arrow(cx: &Cx, lambda: &SyntaxNode, out: &mut Sink) {
    let (Some(list), Some(arrow)) = (lambda.children().find(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST), ast::token(lambda, SyntaxKind::ARROW)) else { return };
    if !cx.on(&REDUNDANT_LAMBDA_ARROW) || has_comments(&list) {
        return;
    }
    let params: Vec<SyntaxNode> = list.children().collect();
    let implicit_it = match params.as_slice() {
        [] => false,
        [p] if p.kind() == SyntaxKind::VALUE_PARAMETER && p.text() == "it" => true,
        _ => return,
    };
    // As a call argument the parameter list may choose the overload: only with one candidate.
    if let Some(call) = call_of_lambda(lambda) {
        let Some(e) = cx.engine() else { return };
        let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
        if e.call_candidates(&callee).is_none_or(|c| c.len() != 1) {
            return;
        }
    }
    let end = next_significant(&arrow, true).map_or(arrow.text_range().end(), |t| t.text_range().start());
    let start = if implicit_it { list.text_range().start() } else { arrow.text_range().start() };
    let (message, title) = if implicit_it { ("Redundant lambda parameter 'it'", "Remove 'it ->'") } else { ("Redundant lambda arrow", "Remove arrow") };
    out.report(&REDUNDANT_LAMBDA_ARROW, TextRange::new(start, arrow.text_range().end()), message, Some(Fix::delete(title, TextRange::new(start, end))));
}

/// `listOf<Int>(1, 2)`: every type parameter is the declared type of parameters whose
/// arguments all have exactly that type.
fn explicit_type_arguments(cx: &Cx, list: &SyntaxNode, out: &mut Sink) {
    let Some(call) = list.parent().filter(|p| p.kind() == SyntaxKind::CALL_EXPR) else { return };
    if !cx.on(&REDUNDANT_TYPE_ARGUMENTS) || call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) || has_comments(list) {
        return;
    }
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let Some(args) = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).map(|l| l.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).collect::<Vec<_>>()) else { return };
    if args.is_empty() || args.iter().any(|a| a.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME) || ast::token(a, SyntaxKind::STAR).is_some()) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let defs = e.resolve_name_ref(&callee);
    let [crate::analysis::ide::infer::Def::Symbol { sym, .. }] = defs.as_slice() else { return };
    let projections: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::TYPE_PROJECTION).collect();
    if sym.kind != crate::symbols::SymbolKind::Function || projections.len() != sym.type_params.len() {
        return;
    }
    let params: Vec<crate::symbols::TypeRef> = sym.params.iter().map(|p| e.resolve_declared(sym, &p.ty)).collect();
    let vararg = sym.params.iter().position(|p| p.vararg);
    for (tp, proj) in sym.type_params.iter().zip(&projections) {
        // A plain class type, written without arguments.
        let Some(tr) = proj.children().find_map(ast::TypeRef::cast) else { return };
        let want = e.resolve_syntax_type(&tr);
        let crate::symbols::TypeRef::Named { fq: Some(want_fq), args: want_args, nullable: false, .. } = &want else { return };
        if !want_args.is_empty() {
            return;
        }
        let direct = |t: &crate::symbols::TypeRef| matches!(t, crate::symbols::TypeRef::TypeParam { name, nullable: false } if *name == tp.name);
        // Used elsewhere than as a whole parameter type (`List<T>`, return-only): the
        // arguments may not determine it.
        let mentions = |t: &crate::symbols::TypeRef| t.display().split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == tp.name);
        if params.iter().any(|p| !direct(p) && mentions(p)) || sym.receiver.as_ref().is_some_and(|r| mentions(&e.resolve_declared(sym, r))) {
            return;
        }
        let mut bound = 0;
        for (i, a) in args.iter().enumerate() {
            let Some(p) = (match vararg {
                Some(v) if i >= v => params.get(v),
                _ => params.get(i),
            }) else {
                return;
            };
            if !direct(p) {
                continue;
            }
            let Some(value) = a.first_child() else { return };
            match certain_type(e, &value, false) {
                Some(crate::symbols::TypeRef::Named { fq: Some(fq), args, nullable: false, .. }) if fq == *want_fq && args.is_empty() => bound += 1,
                _ => return,
            }
        }
        if bound == 0 {
            return;
        }
    }
    out.report(&REDUNDANT_TYPE_ARGUMENTS, list.text_range(), "Remove explicit type arguments", Some(Fix::delete("Remove explicit type arguments", list.text_range())));
}

/// `(stdlib function, replacement)`: `None` drops the call.
const NULLABLE_RECEIVER_CALLS: &[(&str, Option<&str>)] = &[
    ("kotlin.collections.orEmpty", None),
    ("kotlin.text.orEmpty", None),
    ("kotlin.collections.isNullOrEmpty", Some("isEmpty")),
    ("kotlin.text.isNullOrEmpty", Some("isEmpty")),
    ("kotlin.text.isNullOrBlank", Some("isBlank")),
];

/// `s.orEmpty()`, `s.isNullOrEmpty()` on a value that is never null.
fn useless_call_on_not_null(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let mut parts = n.children();
    let (Some(recv), Some(call), None) = (parts.next(), parts.next(), parts.next()) else { return };
    if call.kind() != SyntaxKind::CALL_EXPR || !cx.on(&USELESS_CALL_ON_NOT_NULL) || has_comments(n) {
        return;
    }
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let name = callee.text().to_string();
    if !matches!(name.as_str(), "orEmpty" | "isNullOrEmpty" | "isNullOrBlank") || call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let defs = e.resolve_name_ref(&callee);
    let [crate::analysis::ide::infer::Def::Symbol { sym, .. }] = defs.as_slice() else { return };
    let Some(&(_, replacement)) = NULLABLE_RECEIVER_CALLS.iter().find(|(fq, _)| *fq == sym.fq_name) else { return };
    if !matches!(certain_type(e, &recv, true), Some(crate::symbols::TypeRef::Named { nullable: false, .. })) {
        return;
    }
    match replacement {
        None => {
            let fix = Fix::replace("Remove useless call", n.text_range(), recv.text().to_string());
            out.report(&USELESS_CALL_ON_NOT_NULL, TextRange::new(recv.text_range().end(), n.text_range().end()), format!("Useless call '{name}()' on a value that is never null"), Some(fix));
        }
        Some(r) => {
            let fix = Fix::replace(format!("Replace with '{r}()'"), callee.text_range(), r);
            out.report(&USELESS_CALL_ON_NOT_NULL, callee.text_range(), format!("Call on a value that is never null can be replaced with '{r}()'"), Some(fix));
        }
    }
}

/// `val x: Int = 1`, `val u: User = User(…)`: a local's written type that the initializer
/// states anyway.
fn explicit_type(cx: &Cx, p: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&REDUNDANT_EXPLICIT_TYPE) || p.parent().is_none_or(|b| b.kind() != SyntaxKind::BLOCK) || has_comments(p) {
        return;
    }
    let Some(decl) = ast::PropertyDecl::cast(p.clone()) else { return };
    let Some(ty) = decl.type_ref() else { return };
    if decl.is_destructuring() || decl.has_delegate() || ty.syntax().text().to_string().contains(['?', '<']) {
        return;
    }
    let Some(eq) = ast::token(p, SyntaxKind::EQ) else { return };
    let Some(init) = p.children().find(|c| c.text_range().start() >= eq.text_range().end()) else { return };
    let Some(e) = cx.engine() else { return };
    let declared = e.resolve_syntax_type(&ty);
    let Some(fq) = crate::analysis::ide::infer::fq_of(&declared) else { return };
    let stated = match init.kind() {
        SyntaxKind::LITERAL => {
            let Some(t) = init.first_token() else { return };
            let text = t.text().to_ascii_lowercase();
            match t.kind() {
                SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW => "kotlin.Boolean",
                SyntaxKind::CHAR_LITERAL => "kotlin.Char",
                SyntaxKind::INT_LITERAL if text.contains('u') => return,
                SyntaxKind::INT_LITERAL if text.ends_with('l') => "kotlin.Long",
                SyntaxKind::INT_LITERAL => "kotlin.Int",
                SyntaxKind::FLOAT_LITERAL if text.ends_with('f') => "kotlin.Float",
                SyntaxKind::FLOAT_LITERAL => "kotlin.Double",
                _ => return,
            }
        }
        SyntaxKind::STRING_TEMPLATE => "kotlin.String",
        SyntaxKind::CALL_EXPR => {
            let Some(callee) = init.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
            if init.children().any(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST) {
                return;
            }
            let defs = e.resolve_name_ref(&callee);
            let constructs = !defs.is_empty()
                && defs.iter().all(|d| match d {
                    crate::analysis::ide::infer::Def::Symbol { sym, .. } => match sym.kind {
                        crate::symbols::SymbolKind::Constructor => sym.fq_name.rsplit_once('.').is_some_and(|(c, _)| c == fq),
                        k => k.is_classifier() && sym.fq_name == fq,
                    },
                    _ => false,
                });
            if !constructs {
                return;
            }
            fq
        }
        _ => return,
    };
    if stated != fq {
        return;
    }
    let Some(colon) = ast::token(p, SyntaxKind::COLON) else { return };
    let start = prev_significant(&colon).map_or(colon.text_range().start(), |t| t.text_range().end());
    out.report(&REDUNDANT_EXPLICIT_TYPE, ty.syntax().text_range(), "Explicitly given type is redundant here", Some(Fix::delete("Remove explicit type specification", TextRange::new(start, ty.syntax().text_range().end()))));
}

fn report_qualifier(out: &mut Sink, range: TextRange) {
    out.report(&REDUNDANT_QUALIFIER, range, "Redundant qualifier name", Some(Fix::delete("Remove redundant qualifier name", range)));
}

/// In a class header (annotations, constructor, supertypes): nested classes are not in
/// scope there.
fn in_class_header(n: &SyntaxNode) -> bool {
    n.ancestors().find(|a| matches!(a.kind(), SyntaxKind::CLASS_BODY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL)).is_some_and(|a| a.kind() != SyntaxKind::CLASS_BODY)
}

/// Whether dropping `qualifier` (the names before the kept ones) is plain: it is a
/// package, or a class around `at`. Not `java.lang` (its classes are mostly mapped to
/// Kotlin types) and not a supertype's nested class (kept for readability).
fn removable_qualifier(e: &crate::analysis::ide::infer::Engine, at: &SyntaxNode, qualifier: &[&str], full: &str) -> bool {
    if full.starts_with("java.lang.") {
        return false;
    }
    if e.classifier_named(at, qualifier).is_none() {
        return true;
    }
    let Some(last) = qualifier.last() else { return false };
    at.ancestors().filter(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION)).any(|a| name_token(&a).is_some_and(|t| ast::ident_text(&t) == *last))
}

/// `java.util.UUID` in a type where `UUID` means the same class.
fn type_qualifier(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let segments: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == SyntaxKind::USER_TYPE_SEGMENT).collect();
    if segments.len() < 2 || !cx.on(&REDUNDANT_QUALIFIER) || segments[..segments.len() - 1].iter().any(|s| s.children().any(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST)) {
        return;
    }
    if in_class_header(n) {
        return;
    }
    let Some(names) = segments.iter().map(|s| ast::token(s, SyntaxKind::IDENT).map(|t| ast::ident_text(&t))).collect::<Option<Vec<_>>>() else { return };
    let Some(e) = cx.engine() else { return };
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let Some(full) = e.classifier_named(n, &names) else { return };
    for k in 1..names.len() {
        if e.classifier_named(n, &names[k..]).as_deref() == Some(full.as_str()) && removable_qualifier(e, n, &names[..k], &full) {
            report_qualifier(out, TextRange::new(segments[0].text_range().start(), segments[k].text_range().start()));
            return;
        }
    }
}

/// The names of a chain of plain names (`a.b.C`): each prefix with its last name token.
fn name_chain(n: &SyntaxNode) -> Option<Vec<(SyntaxNode, SyntaxToken)>> {
    match n.kind() {
        SyntaxKind::NAME_REF => Some(vec![(n.clone(), ast::token(n, SyntaxKind::IDENT)?)]),
        SyntaxKind::DOT_QUALIFIED_EXPR => {
            let mut parts = n.children();
            let (Some(recv), Some(sel), None) = (parts.next(), parts.next(), parts.next()) else { return None };
            if sel.kind() != SyntaxKind::NAME_REF || has_comments(n) {
                return None;
            }
            let mut chain = name_chain(&recv)?;
            chain.push((n.clone(), ast::token(&sel, SyntaxKind::IDENT)?));
            Some(chain)
        }
        _ => None,
    }
}

/// `a.B.C(…)`: the names of the chain and the called name, when the called name starts
/// with an uppercase letter (a constructor call).
fn constructor_chain(n: &SyntaxNode) -> Option<Vec<(SyntaxNode, SyntaxToken)>> {
    if n.kind() != SyntaxKind::DOT_QUALIFIED_EXPR {
        return None;
    }
    let mut parts = n.children();
    let (Some(recv), Some(call), None) = (parts.next(), parts.next(), parts.next()) else { return None };
    let callee = call.first_child().filter(|c| call.kind() == SyntaxKind::CALL_EXPR && c.kind() == SyntaxKind::NAME_REF)?;
    let name = ast::token(&callee, SyntaxKind::IDENT)?;
    if !name.text().starts_with(char::is_uppercase) || call.children().any(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST) {
        return None;
    }
    let mut chain = name_chain(&recv)?;
    chain.push((n.clone(), name));
    Some(chain)
}

/// `java.util.UUID.randomUUID()`, `a.Outer.Nested.X` in an expression: the qualifiers in
/// front of a class that its shorter name already means.
fn expression_qualifier(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    use crate::analysis::ide::infer::{Def, Qualifier};
    // Only maximal chains of names.
    if n.parent().is_some_and(|p| p.kind() == SyntaxKind::DOT_QUALIFIED_EXPR && (name_chain(&p).is_some() || constructor_chain(&p).is_some())) || !cx.on(&REDUNDANT_QUALIFIER) {
        return;
    }
    if n.ancestors().any(|a| matches!(a.kind(), SyntaxKind::IMPORT_DIRECTIVE | SyntaxKind::PACKAGE_DIRECTIVE)) || in_class_header(n) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let (chain, j, full) = if let Some(chain) = constructor_chain(n) {
        // `a.Outer.Nested()`: the whole chain names the class.
        let names: Vec<String> = chain.iter().map(|(_, t)| ast::ident_text(t)).collect();
        let Some(full) = e.classifier_named(n, &names.iter().map(String::as_str).collect::<Vec<_>>()) else { return };
        let j = chain.len() - 1;
        (chain, j, full)
    } else {
        let Some(chain) = name_chain(n) else { return };
        // The longest prefix that is a class (the rest are its members).
        let Some((j, full)) = chain.iter().enumerate().rev().find_map(|(j, (p, _))| match e.resolve_qualifier(p) {
            Qualifier::Static(c) if j > 0 => Some((j, c.fq_name().to_string())),
            _ => None,
        }) else {
            return;
        };
        (chain, j, full)
    };
    if chain.len() < 2 {
        return;
    }
    let names: Vec<String> = chain.iter().map(|(_, t)| ast::ident_text(t)).collect();
    for k in 1..=j {
        // A value of that name would win in an expression.
        if e.resolve_name_text(n, &names[k]).iter().any(|d| match d {
            Def::Local(_) => true,
            Def::Symbol { sym, .. } => !sym.kind.is_classifier(),
            Def::Package(_) => false,
        }) {
            continue;
        }
        let suffix: Vec<&str> = names[k..=j].iter().map(String::as_str).collect();
        let qualifier: Vec<&str> = names[..k].iter().map(String::as_str).collect();
        if e.classifier_named(n, &suffix).as_deref() == Some(full.as_str()) && removable_qualifier(e, n, &qualifier, &full) {
            let start = chain[0].0.text_range().start();
            report_qualifier(out, TextRange::new(start, chain[k].1.text_range().start()));
            return;
        }
    }
}

/// Whether anything in `body` may suspend: a call that is or may be `suspend` (unresolved,
/// invoking a function-typed value, an infix call), or `coroutineContext`.
fn may_suspend(e: &crate::analysis::ide::infer::Engine, body: &SyntaxNode) -> bool {
    use crate::analysis::ide::infer::Def;
    let suspends = |defs: &[Def]| {
        defs.is_empty()
            || defs.iter().any(|d| match d {
                Def::Symbol { sym, .. } => sym.modifiers.has(crate::symbols::Modifiers::SUSPEND) || !matches!(sym.kind, crate::symbols::SymbolKind::Function | crate::symbols::SymbolKind::Constructor) && !sym.kind.is_classifier(),
                // A function-typed local or parameter may be a suspend function.
                Def::Local(_) => true,
                Def::Package(_) => false,
            })
    };
    for n in body.descendants() {
        match n.kind() {
            SyntaxKind::CALL_EXPR => {
                let Some(callee) = n.first_child() else { return true };
                if callee.kind() != SyntaxKind::NAME_REF || suspends(&e.resolve_name_ref(&callee)) {
                    return true;
                }
            }
            SyntaxKind::BINARY_EXPR if binary_op(&n).is_some_and(|o| o.kind() == SyntaxKind::IDENT) => {
                let Some(op) = n.children().find(|c| c.kind() == SyntaxKind::OPERATION_REFERENCE) else { return true };
                if suspends(&e.resolve_infix(&op)) {
                    return true;
                }
            }
            SyntaxKind::NAME_REF if n.text() == "coroutineContext" => return true,
            // `for` over a suspending iterator, delegated or overloaded operators: rare, but
            // `super` calls and callable references are left alone too.
            SyntaxKind::SUPER_EXPR | SyntaxKind::CALLABLE_REFERENCE => return true,
            _ => {}
        }
    }
    false
}

/// `suspend fun f() { … }` calling no suspend functions.
fn redundant_suspend(cx: &Cx, f: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&REDUNDANT_SUSPEND) {
        return;
    }
    let Some(m) = modifiers(f) else { return };
    let Some(kw) = m.modifiers().find(|t| t.text() == "suspend") else { return };
    if ["override", "open", "abstract", "actual", "expect", "external"].iter().any(|x| has_modifier(f, x)) || m.annotations().next().is_some() {
        return;
    }
    // Interface members are open.
    if f.parent().and_then(|b| b.parent()).is_some_and(|c| c.kind() == SyntaxKind::CLASS && ast::token(&c, SyntaxKind::INTERFACE_KW).is_some()) {
        return;
    }
    let Some(decl) = ast::FunDecl::cast(f.clone()) else { return };
    let body = match decl.body_block() {
        Some(b) => b.syntax().clone(),
        None if decl.has_expression_body() => f.clone(),
        None => return,
    };
    let Some(e) = cx.engine() else { return };
    if may_suspend(e, &body) {
        return;
    }
    let end = kw.next_token().filter(|t| t.kind().is_trivia()).map_or(kw.text_range().end(), |t| t.text_range().end());
    out.report(&REDUNDANT_SUSPEND, kw.text_range(), "Redundant 'suspend' modifier", Some(Fix::delete("Remove 'suspend' modifier", TextRange::new(kw.text_range().start(), end))));
}

/// `Config.Companion.load()` → `Config.load()`.
fn companion_reference(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    use crate::analysis::ide::infer::Qualifier;
    let mut parts = n.children();
    let (Some(recv), Some(sel), None) = (parts.next(), parts.next(), parts.next()) else { return };
    if recv.kind() != SyntaxKind::DOT_QUALIFIED_EXPR || !cx.on(&REDUNDANT_COMPANION_REFERENCE) || has_comments(&recv) {
        return;
    }
    let mut inner = recv.children();
    let (Some(class), Some(companion), None) = (inner.next(), inner.next(), inner.next()) else { return };
    if companion.kind() != SyntaxKind::NAME_REF || companion.text() != "Companion" {
        return;
    }
    let member = match sel.kind() {
        SyntaxKind::NAME_REF => sel.clone(),
        SyntaxKind::CALL_EXPR => match sel.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) {
            Some(c) => c,
            None => return,
        },
        _ => return,
    };
    let Some(e) = cx.engine() else { return };
    let Qualifier::Static(c) = e.resolve_qualifier(&class) else { return };
    let object = match e.resolve_qualifier(&recv) {
        Qualifier::Static(obj) => obj.fq_name().to_string(),
        Qualifier::Value(t) => match crate::analysis::ide::infer::fq_of(&t) {
            Some(fq) => fq.to_string(),
            None => return,
        },
        _ => return,
    };
    if object != format!("{}.Companion", c.fq_name()) {
        return;
    }
    // A nested class of that name would win over the companion member.
    if e.classifier(&format!("{}.{}", c.fq_name(), member.text())).is_some() {
        return;
    }
    let range = TextRange::new(class.text_range().end(), companion.text_range().end());
    out.report(&REDUNDANT_COMPANION_REFERENCE, companion.text_range(), "Redundant Companion reference", Some(Fix::delete("Remove redundant Companion reference", range)));
}

/// `return x ?: return null`.
fn elvis_return_null(cx: &Cx, ret: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&REDUNDANT_ELVIS_RETURN_NULL) || ret.children().any(|c| c.kind() == SyntaxKind::LABEL_QUALIFIER) {
        return;
    }
    let Some(value) = ret.children().find(|c| c.kind() == SyntaxKind::BINARY_EXPR) else { return };
    let (Some(op), Some((l, r))) = (binary_op(&value), binary_operands(&value)) else { return };
    if op.kind() != SyntaxKind::ELVIS || r.kind() != SyntaxKind::RETURN_EXPR || r.children().any(|c| c.kind() == SyntaxKind::LABEL_QUALIFIER) || r.text().to_string().split_whitespace().collect::<Vec<_>>() != ["return", "null"] {
        return;
    }
    let range = TextRange::new(l.text_range().end(), r.text_range().end());
    out.report(&REDUNDANT_ELVIS_RETURN_NULL, TextRange::new(op.text_range().start(), r.text_range().end()), "Redundant '?: return null'", Some(Fix::delete("Remove redundant '?: return null'", range)));
}

/// `override fun f(a: A) = super.f(a)`.
fn redundant_override(cx: &Cx, f: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&REDUNDANT_OVERRIDE) || !has_modifier(f, "override") || has_comments(f) {
        return;
    }
    let Some(m) = modifiers(f) else { return };
    // Anything besides `override` (visibility, `final`, annotations) changes something.
    if m.annotations().next().is_some() || m.modifiers().any(|t| t.text() != "override") {
        return;
    }
    // Data classes generate `equals`/`hashCode`/`toString` unless overridden.
    if f.parent().and_then(|b| b.parent()).is_some_and(|c| has_modifier(&c, "data") || has_modifier(&c, "value") || has_modifier(&c, "enum")) {
        return;
    }
    let Some(decl) = ast::FunDecl::cast(f.clone()) else { return };
    let Some(name) = name_token(f) else { return };
    let value = match decl.body_block() {
        Some(b) => {
            let Some(s) = only_statement(b.syntax()) else { return };
            if s.kind() == SyntaxKind::RETURN_EXPR { s.first_child() } else { Some(s) }
        }
        None => f.children().find(|c| c.text_range().start() >= ast::token(f, SyntaxKind::EQ).map_or(f.text_range().end(), |t| t.text_range().end())),
    };
    let Some(value) = value.filter(|v| v.kind() == SyntaxKind::DOT_QUALIFIED_EXPR) else { return };
    let mut parts = value.children();
    let (Some(sup), Some(call)) = (parts.next(), parts.next()) else { return };
    if sup.kind() != SyntaxKind::SUPER_EXPR || sup.text() != "super" || call.kind() != SyntaxKind::CALL_EXPR || call.first_child().is_none_or(|c| c.text() != name.text()) || call.children().any(|c| matches!(c.kind(), SyntaxKind::LAMBDA_ARGUMENT | SyntaxKind::TYPE_ARGUMENT_LIST)) {
        return;
    }
    let params: Vec<String> = decl.parameters().iter().filter_map(|p| name_token(p.syntax()).map(|t| ast::ident_text(&t))).collect();
    let args: Option<Vec<String>> = call
        .children()
        .find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST)
        .map(|l| l.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).map(|a| if a.children().count() == 1 && ast::token(&a, SyntaxKind::STAR).is_none() { a.text().to_string() } else { String::new() }).collect());
    if args.as_ref() != Some(&params) {
        return;
    }
    out.report(&REDUNDANT_OVERRIDE, name.text_range(), "Redundant overriding method", Some(Fix::delete("Remove redundant overriding method", deletion_range(cx.text, f.text_range()))));
}

fn stdlib(cx: &Cx, callee: &SyntaxNode) -> bool {
    use crate::analysis::ide::infer::Def;
    let Some(e) = cx.engine() else { return false };
    let defs = e.resolve_name_ref(callee);
    !defs.is_empty() && defs.iter().all(|d| matches!(d, Def::Symbol { sym, .. } if sym.fq_name.starts_with("kotlin.")))
}

fn non_null_value(cx: &Cx, x: &SyntaxNode) -> bool {
    let Some(e) = cx.engine() else { return false };
    matches!(crate::analysis::lints::certain_type(e, x, true), Some(crate::symbols::TypeRef::Named { nullable: false, ref fq, .. }) if fq.as_deref() != Some("kotlin.Nothing"))
}

/// `requireNotNull(x)` with `x` never null.
fn require_not_null(cx: &Cx, call: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&REDUNDANT_REQUIRE_NOT_NULL) {
        return;
    }
    let Some(callee) = call.first_child() else { return };
    let Some(list) = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST) else { return };
    let args: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT).collect();
    let [arg] = args.as_slice() else { return };
    let Some(value) = arg.children().last() else { return };
    if !stdlib(cx, &callee) || !non_null_value(cx, &value) {
        return;
    }
    let statement = call.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK);
    let fix = if statement { Fix::delete(format!("Remove '{}' call", callee.text()), deletion_range(cx.text, call.text_range())) } else { Fix::replace(format!("Remove '{}' call", callee.text()), call.text_range(), value.text().to_string()) };
    out.report(&REDUNDANT_REQUIRE_NOT_NULL, callee.text_range(), format!("Redundant '{}' call", callee.text()), Some(fix));
}

const SEQUENCE_TERMINALS: &[&str] = &["toList", "toSet", "toMutableList", "toMutableSet", "first", "firstOrNull", "last", "lastOrNull", "single", "singleOrNull", "count", "any", "all", "none", "forEach", "joinToString", "sum", "max", "min", "maxOrNull", "minOrNull"];

/// `xs.asSequence().toList()`.
fn as_sequence(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&REDUNDANT_AS_SEQUENCE) {
        return;
    }
    let mut parts = n.children();
    let (Some(inner), Some(terminal)) = (parts.next(), parts.next()) else { return };
    let Some(t_callee) = terminal.first_child().filter(|c| terminal.kind() == SyntaxKind::CALL_EXPR && c.kind() == SyntaxKind::NAME_REF) else { return };
    if !SEQUENCE_TERMINALS.contains(&t_callee.text().to_string().as_str()) || inner.kind() != SyntaxKind::DOT_QUALIFIED_EXPR {
        return;
    }
    let mut ip = inner.children();
    let (Some(recv), Some(call)) = (ip.next(), ip.next()) else { return };
    let Some(callee) = call.first_child().filter(|c| call.kind() == SyntaxKind::CALL_EXPR && c.kind() == SyntaxKind::NAME_REF && c.text() == "asSequence") else { return };
    if call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) || !stdlib(cx, &callee) {
        return;
    }
    // Iterables and arrays have the same terminal operations.
    let Some(e) = cx.engine() else { return };
    let Some(t) = e.type_of(&recv) else { return };
    let iterable = e.supertype_closure(&t).iter().filter_map(crate::analysis::ide::infer::fq_of).any(|f| f == "kotlin.collections.Iterable" || f == "kotlin.Array");
    if !iterable {
        return;
    }
    let range = TextRange::new(recv.text_range().end(), call.text_range().end());
    out.report(&REDUNDANT_AS_SEQUENCE, callee.text_range(), "Redundant 'asSequence' call", Some(Fix::delete("Remove 'asSequence' call", range)));
}

/// `runCatching { … }.getOrThrow()` → `run { … }`.
fn run_catching(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&REDUNDANT_RUN_CATCHING) {
        return;
    }
    let mut parts = n.children();
    let (Some(rc), Some(get)) = (parts.next(), parts.next()) else { return };
    if get.kind() != SyntaxKind::CALL_EXPR || get.first_child().is_none_or(|c| c.text() != "getOrThrow") || get.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).is_some_and(|l| l.children().next().is_some()) {
        return;
    }
    let Some(callee) = rc.first_child().filter(|c| rc.kind() == SyntaxKind::CALL_EXPR && c.kind() == SyntaxKind::NAME_REF && c.text() == "runCatching") else { return };
    let Some(lambda) = rc.children().find(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) else { return };
    if rc.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).is_some_and(|l| l.children().next().is_some()) || !stdlib(cx, &callee) {
        return;
    }
    out.report(&REDUNDANT_RUN_CATCHING, callee.text_range(), "Redundant 'runCatching' call", Some(Fix::replace("Replace with 'run'", n.text_range(), format!("run {}", lambda.text()))));
}

/// `fun f(): String? = "x"`: a final function whose every result is non-null.
fn nullable_return_type(cx: &Cx, f: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&REDUNDANT_NULLABLE_RETURN_TYPE) || ["open", "abstract", "override", "expect", "actual", "external"].iter().any(|m| has_modifier(f, m)) {
        return;
    }
    if f.parent().and_then(|b| b.parent()).is_some_and(|c| c.kind() == SyntaxKind::CLASS && ast::token(&c, SyntaxKind::INTERFACE_KW).is_some()) {
        return;
    }
    let Some(decl) = ast::FunDecl::cast(f.clone()) else { return };
    let Some(ret) = decl.return_type() else { return };
    let Some(q) = ret.syntax().children().find(|c| c.kind() == SyntaxKind::NULLABLE_TYPE).and_then(|n| n.last_token()).filter(|t| t.kind() == SyntaxKind::QUESTION) else { return };
    if ret.syntax().text().to_string().matches('?').count() != 1 || cx.engine().is_none() {
        return;
    }
    let ok = match decl.body_block() {
        Some(body) => {
            let returns: Vec<SyntaxNode> = body.syntax().descendants().filter(|d| d.kind() == SyntaxKind::RETURN_EXPR && !d.children().any(|c| c.kind() == SyntaxKind::LABEL_QUALIFIER)).filter(|r| r.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION)).as_ref() == Some(f)).collect();
            !returns.is_empty() && returns.iter().all(|r| r.first_child().is_some_and(|v| non_null_value(cx, &v)))
        }
        None => initializer(f).is_some_and(|v| non_null_value(cx, &v)),
    };
    if ok {
        out.report(&REDUNDANT_NULLABLE_RETURN_TYPE, q.text_range(), format!("'{}' is always non-null type", ret.syntax().text()), Some(Fix::delete("Make not-nullable", q.text_range())));
    }
}
