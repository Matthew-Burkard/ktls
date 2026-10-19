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
                    SyntaxKind::FUN => unit_return_type(&n, out),
                    SyntaxKind::IF_EXPR => redundant_if(&n, out),
                    SyntaxKind::CLASS_BODY => empty_class_body(&n, out),
                    SyntaxKind::PRIMARY_CONSTRUCTOR => empty_constructor(&n, out),
                    SyntaxKind::CALL_EXPR => lambda_parens(&n, out),
                    SyntaxKind::PROPERTY_ACCESSOR => accessor(&n, out),
                    SyntaxKind::CLASS_INITIALIZER => init_block(cx, &n, out),
                    SyntaxKind::NAME_REF if n.text() == "Unit" => unit_expression(cx, &n, out),
                    SyntaxKind::BLOCK => unnecessary_variable(&n, out),
                    SyntaxKind::STRING_TEMPLATE => string_template(cx, &n, out),
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
