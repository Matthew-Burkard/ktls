//! Expressions, statements and blocks.

use super::super::kind::SyntaxKind::{self, *};
use super::super::parser::{CompletedMarker, Parser};
use super::types::{type_argument_list, type_arguments_ahead, type_reference};
use super::{
    DeclContext, annotation, class_body, super_type_entry, declaration, declaration_ahead, destructuring_declaration, function_body,
    modifier_list, skip_annotation_ahead, value_parameter_list,
};

// ------------------------------------------------------------- statements

/// `{ statements }` as a BLOCK node.
pub(crate) fn block(p: &mut Parser) {
    let m = p.start();
    if p.too_deep() && p.at(L_CURLY) {
        p.skip_too_deep();
        m.complete(p, BLOCK);
        return;
    }
    if !p.expect(L_CURLY) {
        m.complete(p, BLOCK);
        return;
    }
    let old = p.no_trailing_lambda;
    p.no_trailing_lambda = false;
    p.with_newlines(true, statements);
    p.no_trailing_lambda = old;
    p.expect(R_CURLY);
    m.complete(p, BLOCK);
}

/// Statements up to (not including) a closing `}`.
fn statements(p: &mut Parser) {
    while !p.at(R_CURLY) && !p.at_eof() {
        if p.bump_if(SEMI) {
            continue;
        }
        statement_or_declaration(p, DeclContext::Local);
        statement_separator(p);
    }
}

pub(crate) fn statement_separator(p: &mut Parser) {
    if p.at(SEMI) || p.at(R_CURLY) || p.at_eof() || p.nl_before() {
        return;
    }
    p.err_recover_until("Unexpected tokens (use ';' to separate expressions on the same line)", |p| {
        p.nl_before() || p.at(SEMI)
    });
}

pub(crate) fn statement_or_declaration(p: &mut Parser, ctx: DeclContext) {
    if declaration_ahead(p, ctx) {
        declaration(p, ctx);
        return;
    }
    statement(p);
}

const ASSIGNMENT_OPS: &[SyntaxKind] = &[EQ, PLUS_EQ, MINUS_EQ, STAR_EQ, SLASH_EQ, PERCENT_EQ];

/// An expression statement, including assignments.
fn statement(p: &mut Parser) {
    let Some(lhs) = expression_opt(p) else {
        // Nothing parsed: make progress.
        if p.at(R_CURLY) || p.at_eof() {
            return;
        }
        p.err_and_bump("Expecting an element");
        return;
    };
    if ASSIGNMENT_OPS.contains(&p.current()) && !p.nl_before() {
        let m = lhs.precede(p);
        p.bump();
        expression(p);
        m.complete(p, BINARY_EXPR);
    }
}

/// Body of `if`/`for`/`while`/`when` branches: a block or a single statement.
fn control_structure_body(p: &mut Parser, wrapper: SyntaxKind) {
    let m = p.start();
    if p.at(L_CURLY) && lambda_parameters_ahead(p) {
        // `-> { x: Int -> x }` is a lambda, not a block.
        statement(p);
    } else if p.at(L_CURLY) {
        block(p);
    } else if declaration_ahead(p, DeclContext::Local) {
        declaration(p, DeclContext::Local);
    } else {
        statement(p);
    }
    m.complete(p, wrapper);
}

// ------------------------------------------------------------ expressions

pub(crate) fn expression(p: &mut Parser) {
    if expression_opt(p).is_none() {
        p.error_expecting("an expression");
    }
}

pub(crate) fn expression_opt(p: &mut Parser) -> Option<CompletedMarker> {
    expr_bp(p, 1)
}

#[derive(Clone, Copy)]
enum Op {
    Binary,
    IsCheck,
    AsCast,
    Infix,
}

/// (precedence, newline allowed before operator, op)
fn binary_op(p: &Parser) -> Option<(u8, bool, Op)> {
    Some(match p.current() {
        PIPE2 => (1, true, Op::Binary),
        AMP2 => (2, true, Op::Binary),
        EQ2 | NOT_EQ | EQ3 | NOT_EQ2 => (3, false, Op::Binary),
        LT | GT | LT_EQ | GT_EQ => (4, false, Op::Binary),
        IN_KW | NOT_IN_KW => (5, false, Op::Binary),
        IS_KW | NOT_IS_KW => (5, false, Op::IsCheck),
        ELVIS => (6, true, Op::Binary),
        IDENT if !p.nl_before() && can_start_expression(p.nth(1)) => (7, false, Op::Infix),
        RANGE | RANGE_UNTIL => (8, false, Op::Binary),
        PLUS | MINUS => (9, false, Op::Binary),
        STAR | SLASH | PERCENT => (10, false, Op::Binary),
        AS_KW | AS_SAFE_KW => (11, true, Op::AsCast),
        _ => return None,
    })
}

fn can_start_expression(k: SyntaxKind) -> bool {
    matches!(
        k,
        IDENT
            | INT_LITERAL
            | FLOAT_LITERAL
            | CHAR_LITERAL
            | STRING_OPEN
            | TRUE_KW
            | FALSE_KW
            | NULL_KW
            | THIS_KW
            | SUPER_KW
            | L_PAREN
            | L_CURLY
            | L_BRACK
            | IF_KW
            | WHEN_KW
            | TRY_KW
            | OBJECT_KW
            | FUN_KW
            | RETURN_KW
            | THROW_KW
            | BREAK_KW
            | CONTINUE_KW
            | COLON2
            | MINUS
            | PLUS
            | EXCL
            | PLUS2
            | MINUS2
            | AT
            | FOR_KW
            | WHILE_KW
            | DO_KW
    )
}

fn expr_bp(p: &mut Parser, min_bp: u8) -> Option<CompletedMarker> {
    let mut lhs = prefix_expr(p)?;
    let mut links = 0;
    while let Some((bp, nl_ok, op)) = binary_op(p) {
        if bp < min_bp || (p.nl_before() && !nl_ok) {
            break;
        }
        if p.too_deep() {
            p.skip_rest_of_chain();
            break;
        }
        links += 1;
        p.enter_link();
        let m = lhs.precede(p);
        match op {
            Op::IsCheck => {
                op_ref(p);
                type_reference(p);
                lhs = m.complete(p, IS_EXPR);
            }
            Op::AsCast => {
                op_ref(p);
                type_reference(p);
                lhs = m.complete(p, BINARY_WITH_TYPE);
            }
            Op::Binary | Op::Infix => {
                op_ref(p);
                if expr_bp(p, bp + 1).is_none() {
                    p.error_expecting("an element");
                }
                lhs = m.complete(p, BINARY_EXPR);
            }
        }
    }
    p.leave_links(links);
    Some(lhs)
}

fn op_ref(p: &mut Parser) {
    let m = p.start();
    p.bump();
    m.complete(p, OPERATION_REFERENCE);
}

/// `ident@` directly followed by `@` (no whitespace): a label definition.
fn label_definition_ahead(p: &Parser) -> bool {
    p.at(IDENT) && p.nth_at(1, AT) && p.nth_joint(1)
}

fn prefix_expr(p: &mut Parser) -> Option<CompletedMarker> {
    if p.too_deep() && can_start_expression(p.current()) {
        return p.skip_too_deep();
    }
    match p.current() {
        MINUS | PLUS | PLUS2 | MINUS2 | EXCL => {
            let m = p.start();
            op_ref(p);
            if prefix_expr(p).is_none() {
                p.error_expecting("an expression");
            }
            Some(m.complete(p, PREFIX_EXPR))
        }
        AT => {
            let m = p.start();
            while p.at(AT) {
                annotation(p);
            }
            if prefix_expr(p).is_none() {
                p.error_expecting("an expression");
            }
            Some(m.complete(p, ANNOTATED_EXPR))
        }
        IDENT if label_definition_ahead(p) => {
            let m = p.start();
            let l = p.start();
            p.bump();
            p.bump();
            l.complete(p, LABEL);
            if prefix_expr(p).is_none() {
                p.error_expecting("an expression");
            }
            Some(m.complete(p, LABELED_EXPR))
        }
        _ => postfix_expr(p),
    }
}

fn postfix_expr(p: &mut Parser) -> Option<CompletedMarker> {
    let mut lhs = primary(p)?;
    let mut links = 0;
    loop {
        if p.too_deep() {
            p.skip_rest_of_chain();
            break;
        }
        links += 1;
        p.enter_link();
        lhs = match p.current() {
            PLUS2 | MINUS2 | EXCL2 if !p.nl_before() => {
                let m = lhs.precede(p);
                op_ref(p);
                m.complete(p, POSTFIX_EXPR)
            }
            L_PAREN | LT | L_CURLY | IDENT if call_suffix_ahead(p) => {
                let m = lhs.precede(p);
                call_suffix(p);
                m.complete(p, CALL_EXPR)
            }
            L_BRACK if !p.nl_before() => {
                let m = lhs.precede(p);
                index_suffix(p);
                m.complete(p, INDEX_EXPR)
            }
            DOT => {
                let m = lhs.precede(p);
                p.bump();
                selector(p);
                m.complete(p, DOT_QUALIFIED_EXPR)
            }
            QUESTION if at_safe_dot(p) => {
                let m = lhs.precede(p);
                p.bump();
                p.bump();
                selector(p);
                m.complete(p, SAFE_QUALIFIED_EXPR)
            }
            COLON2 | QUESTION if p.at(COLON2) || (p.nth_at(1, COLON2) && p.nth_joint(1)) => {
                // `Foo::bar`, `Foo?::bar`
                let m = lhs.precede(p);
                p.bump_if(QUESTION);
                p.bump();
                if p.at(CLASS_KW) {
                    p.bump();
                    m.complete(p, CLASS_LITERAL)
                } else {
                    name_ref(p);
                    m.complete(p, CALLABLE_REFERENCE)
                }
            }
            _ => break,
        };
    }
    p.leave_links(links);
    Some(lhs)
}

/// `?.` is lexed as two joint tokens so that `String?.foo` works in types.
pub(crate) fn at_safe_dot(p: &Parser) -> bool {
    p.at(QUESTION) && p.nth_at(1, DOT) && p.nth_joint(1)
}

/// After `.`: a name with optional call suffixes (`a.b<T>(x) { }`).
fn selector(p: &mut Parser) {
    let mut sel = match p.current() {
        IDENT => {
            let m = p.start();
            p.bump();
            m.complete(p, NAME_REF)
        }
        CLASS_KW => {
            // `Foo.class` is Java syntax, but keep the tree sane.
            p.err_and_bump("Use '::class' in Kotlin");
            return;
        }
        THIS_KW | SUPER_KW => {
            let Some(e) = primary(p) else { return };
            e
        }
        _ => {
            p.error_expecting("name");
            return;
        }
    };
    while matches!(p.current(), L_PAREN | LT | L_CURLY | IDENT) && call_suffix_ahead(p) {
        let m = sel.precede(p);
        call_suffix(p);
        sel = m.complete(p, CALL_EXPR);
    }
}

fn call_suffix_ahead(p: &Parser) -> bool {
    if p.nl_before() {
        return false;
    }
    match p.current() {
        L_PAREN => true,
        LT => type_arguments_ahead(p),
        L_CURLY => !p.no_trailing_lambda,
        // `foo lit@{ ... }`
        IDENT => !p.no_trailing_lambda && label_definition_ahead(p) && p.nth_at(2, L_CURLY),
        _ => false,
    }
}

fn call_suffix(p: &mut Parser) {
    if p.at(LT) {
        type_argument_list(p);
        if p.at(L_PAREN) && !p.nl_before() {
            value_argument_list(p);
        }
    } else if p.at(L_PAREN) {
        value_argument_list(p);
    }
    // trailing lambda(s)
    while !p.nl_before() && !p.no_trailing_lambda && (p.at(L_CURLY) || (label_definition_ahead(p) && p.nth_at(2, L_CURLY))) {
        let m = p.start();
        if p.at(L_CURLY) {
            lambda(p);
        } else {
            let le = p.start();
            let l = p.start();
            p.bump();
            p.bump();
            l.complete(p, LABEL);
            lambda(p);
            le.complete(p, LABELED_EXPR);
        }
        m.complete(p, LAMBDA_ARGUMENT);
    }
}

fn index_suffix(p: &mut Parser) {
    let m = p.start();
    p.bump(); // [
    p.with_newlines(false, |p| {
        while !p.at(R_BRACK) && !p.at_eof() {
            expression(p);
            if !p.bump_if(COMMA) {
                break;
            }
        }
    });
    p.expect(R_BRACK);
    m.complete(p, VALUE_ARGUMENT_LIST);
}

pub(crate) fn value_argument_list(p: &mut Parser) {
    let m = p.start();
    p.bump(); // (
    let old = p.no_trailing_lambda;
    p.no_trailing_lambda = false;
    p.newlines.push(false);
    // A `}` / `]` cannot start an argument: the `(` was never closed.
    while !p.at(R_PAREN) && !p.at_eof() && !p.at(R_CURLY) && !p.at(R_BRACK) {
        let arg = p.start();
        if p.at(IDENT) && p.nth_at(1, EQ) {
            let n = p.start();
            p.bump();
            n.complete(p, VALUE_ARGUMENT_NAME);
            p.bump(); // =
        }
        p.bump_if(STAR);
        if expression_opt(p).is_none() {
            arg.complete(p, ERROR);
            p.err_recover_until("Expecting an argument", |p| p.at(COMMA) || p.at(R_PAREN));
        } else {
            arg.complete(p, VALUE_ARGUMENT);
        }
        if !p.bump_if(COMMA) {
            break;
        }
    }
    p.no_trailing_lambda = old;
    p.newlines.pop();
    if !p.expect(R_PAREN) && !(p.at_eof() || p.at(R_CURLY) || p.at(R_BRACK) || p.nl_before()) {
        p.err_recover_until("Unexpected tokens in argument list", |p| {
            p.at(R_PAREN) || p.nl_before() || p.at(R_CURLY)
        });
        p.bump_if(R_PAREN);
    }
    m.complete(p, VALUE_ARGUMENT_LIST);
}

fn name_ref(p: &mut Parser) {
    if p.at(IDENT) {
        let m = p.start();
        p.bump();
        m.complete(p, NAME_REF);
    } else {
        p.error_expecting("name");
    }
}

/// `@label` directly after `this`, `return`, ...
fn label_qualifier(p: &mut Parser) {
    if p.at(AT) && p.nth_joint(0) && p.nth_at(1, IDENT) && p.nth_joint(1) {
        let m = p.start();
        p.bump();
        p.bump();
        m.complete(p, LABEL_QUALIFIER);
    }
}

fn primary(p: &mut Parser) -> Option<CompletedMarker> {
    let m = p.start();
    let kind = match p.current() {
        L_PAREN => {
            p.bump();
            let old = p.no_trailing_lambda;
            p.no_trailing_lambda = false;
            p.with_newlines(false, expression);
            p.no_trailing_lambda = old;
            p.expect(R_PAREN);
            PAREN_EXPR
        }
        INT_LITERAL | FLOAT_LITERAL | CHAR_LITERAL | TRUE_KW | FALSE_KW | NULL_KW => {
            p.bump();
            LITERAL
        }
        STRING_OPEN => {
            string_template(p);
            STRING_TEMPLATE
        }
        IDENT => {
            p.bump();
            NAME_REF
        }
        THIS_KW => {
            p.bump();
            label_qualifier(p);
            THIS_EXPR
        }
        SUPER_KW => {
            p.bump();
            if p.at(LT) && p.nth_joint(0) {
                p.bump();
                type_reference(p);
                p.expect(GT);
            }
            label_qualifier(p);
            SUPER_EXPR
        }
        IF_KW => {
            if_expr(p);
            IF_EXPR
        }
        WHEN_KW => {
            when_expr(p);
            WHEN_EXPR
        }
        TRY_KW => {
            try_expr(p);
            TRY_EXPR
        }
        FOR_KW => {
            for_expr(p);
            FOR_EXPR
        }
        WHILE_KW => {
            p.bump();
            condition(p);
            if p.at(SEMI) {
                // `while (x);`
            } else {
                control_structure_body(p, BODY);
            }
            WHILE_EXPR
        }
        DO_KW => {
            p.bump();
            if !p.at(WHILE_KW) {
                control_structure_body(p, BODY);
            }
            if p.expect(WHILE_KW) {
                condition(p);
            }
            DO_WHILE_EXPR
        }
        OBJECT_KW => {
            p.bump();
            if p.at(COLON) {
                let list = p.start();
                p.bump();
                loop {
                    super_type_entry(p);
                    if !p.bump_if(COMMA) {
                        break;
                    }
                }
                list.complete(p, SUPER_TYPE_LIST);
            }
            if p.at(L_CURLY) {
                class_body(p, false);
            } else {
                p.error_expecting("'{'");
            }
            OBJECT_LITERAL
        }
        FUN_KW => {
            anonymous_function(p);
            ANONYMOUS_FUNCTION
        }
        L_CURLY => {
            m.abandon(p);
            return Some(lambda(p));
        }
        COLON2 => {
            p.bump();
            if p.at(CLASS_KW) {
                p.bump();
                CLASS_LITERAL
            } else {
                name_ref(p);
                CALLABLE_REFERENCE
            }
        }
        L_BRACK => {
            p.bump();
            p.with_newlines(false, |p| {
                while !p.at(R_BRACK) && !p.at_eof() {
                    expression(p);
                    if !p.bump_if(COMMA) {
                        break;
                    }
                }
            });
            p.expect(R_BRACK);
            COLLECTION_LITERAL
        }
        RETURN_KW => {
            p.bump();
            label_qualifier(p);
            if !p.nl_before() && can_start_expression(p.current()) {
                expression(p);
            }
            RETURN_EXPR
        }
        THROW_KW => {
            p.bump();
            expression(p);
            THROW_EXPR
        }
        BREAK_KW | CONTINUE_KW => {
            let kind = if p.at(BREAK_KW) { BREAK_EXPR } else { CONTINUE_EXPR };
            p.bump();
            label_qualifier(p);
            kind
        }
        _ => {
            m.abandon(p);
            return None;
        }
    };
    Some(m.complete(p, kind))
}

fn condition(p: &mut Parser) {
    if !p.expect(L_PAREN) {
        return;
    }
    let c = p.start();
    let old = p.no_trailing_lambda;
    p.no_trailing_lambda = false;
    p.with_newlines(false, expression);
    p.no_trailing_lambda = old;
    c.complete(p, CONDITION);
    if !p.expect(R_PAREN) {
        p.err_recover_until("Unexpected tokens in condition", |p| p.at(R_PAREN) || p.at(L_CURLY) || p.nl_before());
        p.bump_if(R_PAREN);
    }
}

fn if_expr(p: &mut Parser) {
    p.bump(); // if
    condition(p);
    if !p.at(ELSE_KW) && !p.at(SEMI) {
        control_structure_body(p, THEN_BRANCH);
    }
    // `if (a) b; else c` and `else` on the next line are both allowed.
    // `else ->` on the next line belongs to an enclosing `when`.
    if p.at(SEMI) && p.nth_at(1, ELSE_KW) && !p.nth_at(2, ARROW) {
        p.bump();
    }
    if p.at(ELSE_KW) && !p.nth_at(1, ARROW) {
        p.bump();
        if p.at(SEMI) {
            return;
        }
        control_structure_body(p, ELSE_BRANCH);
    }
}

fn when_expr(p: &mut Parser) {
    p.bump(); // when
    if p.at(L_PAREN) {
        p.bump();
        let s = p.start();
        p.with_newlines(false, |p| {
            if declaration_ahead(p, DeclContext::Local) {
                declaration(p, DeclContext::Local);
            } else {
                expression(p);
            }
        });
        s.complete(p, WHEN_SUBJECT);
        p.expect(R_PAREN);
    }
    if !p.expect(L_CURLY) {
        return;
    }
    p.with_newlines(true, |p| {
        while !p.at(R_CURLY) && !p.at_eof() {
            if p.bump_if(SEMI) {
                continue;
            }
            when_entry(p);
        }
    });
    p.expect(R_CURLY);
}

fn when_entry(p: &mut Parser) {
    let m = p.start();
    if p.at(ELSE_KW) {
        p.bump();
    } else {
        loop {
            let c = p.start();
            match p.current() {
                IN_KW | NOT_IN_KW => {
                    op_ref(p);
                    expression(p);
                }
                IS_KW | NOT_IS_KW => {
                    op_ref(p);
                    type_reference(p);
                }
                _ => {
                    if expression_opt(p).is_none() {
                        c.complete(p, ERROR);
                        p.err_recover_until("Expecting a when-condition", |p| {
                            p.at(ARROW) || p.at(R_CURLY) || p.nl_before()
                        });
                        break;
                    }
                }
            }
            c.complete(p, WHEN_CONDITION);
            if !p.bump_if(COMMA) || p.at(ARROW) {
                break;
            }
        }
        if p.at(IF_KW) {
            let g = p.start();
            p.bump();
            expression(p);
            g.complete(p, WHEN_GUARD);
        }
    }
    if p.expect(ARROW) {
        control_structure_body(p, BODY);
    } else if !p.at(R_CURLY) {
        p.err_recover_until("Expecting '->'", |p| p.nl_before() || p.at(R_CURLY));
    }
    m.complete(p, WHEN_ENTRY);
    statement_separator(p);
}

fn try_expr(p: &mut Parser) {
    p.bump(); // try
    block(p);
    let mut any = false;
    while p.at_soft("catch") {
        any = true;
        let c = p.start();
        p.bump();
        if p.at(L_PAREN) {
            value_parameter_list(p);
        } else {
            p.error_expecting("'('");
        }
        block(p);
        c.complete(p, CATCH_CLAUSE);
    }
    if p.at_soft("finally") {
        any = true;
        let f = p.start();
        p.bump();
        block(p);
        f.complete(p, FINALLY_CLAUSE);
    }
    if !any {
        p.error_expecting("'catch' or 'finally'");
    }
}

fn for_expr(p: &mut Parser) {
    p.bump(); // for
    if !p.expect(L_PAREN) {
        return;
    }
    let param = p.start();
    modifier_list(p);
    if p.at(VAL_KW) || p.at(VAR_KW) {
        p.error("'val'/'var' is not allowed on a loop parameter");
        p.bump();
    }
    if p.at(L_PAREN) {
        destructuring_declaration(p);
    } else if !p.bump_if(IDENT) {
        p.error_expecting("a variable name");
    }
    if p.bump_if(COLON) {
        type_reference(p);
    }
    param.complete(p, VALUE_PARAMETER);
    if p.expect(IN_KW) {
        let r = p.start();
        p.with_newlines(false, expression);
        r.complete(p, LOOP_RANGE);
    }
    p.expect(R_PAREN);
    if !p.at(SEMI) {
        control_structure_body(p, BODY);
    }
}

fn anonymous_function(p: &mut Parser) {
    p.bump(); // fun
    if !p.at(L_PAREN) {
        // receiver type: `fun Int.(x: Int) = ...`
        let r = p.start();
        type_reference(p);
        r.complete(p, RECEIVER_TYPE);
        p.expect(DOT);
    }
    if p.at(L_PAREN) {
        value_parameter_list(p);
    } else {
        p.error_expecting("'('");
    }
    if p.bump_if(COLON) {
        type_reference(p);
    }
    function_body(p);
}

/// Does the lambda starting at `{` have an explicit parameter list (`{ a, b -> }`)?
fn lambda_parameters_ahead(p: &Parser) -> bool {
    let mut i = 1;
    let mut parens = 0;
    let mut angles = 0;
    while i < 512 {
        match p.nth(i) {
            ARROW if parens == 0 && angles == 0 => return true,
            ARROW => {}
            IDENT | COMMA | COLON | DOT | QUESTION | STAR | IN_KW => {}
            AT => i = skip_annotation_ahead(p, i) - 1,
            L_PAREN => parens += 1,
            R_PAREN => {
                if parens == 0 {
                    return false;
                }
                parens -= 1;
            }
            LT => angles += 1,
            GT => {
                if angles == 0 {
                    return false;
                }
                angles -= 1;
            }
            _ => return false,
        }
        i += 1;
    }
    false
}

fn lambda(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    let has_params = lambda_parameters_ahead(p);
    p.bump(); // {
    p.newlines.push(true);
    if has_params {
        let params = p.start();
        while !p.at(ARROW) && !p.at_eof() {
            let param = p.start();
            modifier_list(p);
            if p.at(L_PAREN) {
                destructuring_declaration(p);
            } else if !p.bump_if(IDENT) {
                p.error_expecting("parameter name");
            }
            if p.bump_if(COLON) {
                type_reference(p);
            }
            param.complete(p, VALUE_PARAMETER);
            if !p.bump_if(COMMA) {
                break;
            }
        }
        params.complete(p, LAMBDA_PARAMETER_LIST);
        p.expect(ARROW);
    }
    let body = p.start();
    let old = p.no_trailing_lambda;
    p.no_trailing_lambda = false;
    p.with_newlines(true, statements);
    p.no_trailing_lambda = old;
    body.complete(p, BLOCK);
    p.newlines.pop();
    p.expect(R_CURLY);
    m.complete(p, LAMBDA_EXPR)
}

fn string_template(p: &mut Parser) {
    p.bump(); // opening quote
    loop {
        match p.current() {
            STRING_TEXT => {
                let m = p.start();
                p.bump();
                m.complete(p, LITERAL_STRING_ENTRY);
            }
            STRING_ESCAPE => {
                let m = p.start();
                p.bump();
                m.complete(p, ESCAPE_STRING_ENTRY);
            }
            SHORT_TEMPLATE_START => {
                let m = p.start();
                p.bump();
                let r = p.start();
                let kind = if p.at(THIS_KW) { THIS_EXPR } else { NAME_REF };
                p.bump();
                r.complete(p, kind);
                m.complete(p, SHORT_STRING_TEMPLATE_ENTRY);
            }
            LONG_TEMPLATE_START => {
                let m = p.start();
                p.bump();
                let old = p.no_trailing_lambda;
                p.no_trailing_lambda = false;
                expression(p);
                p.no_trailing_lambda = old;
                if !p.at(LONG_TEMPLATE_END) {
                    p.err_recover_until("Expecting '}'", |p| p.at(LONG_TEMPLATE_END) || p.at(STRING_CLOSE));
                }
                p.bump_if(LONG_TEMPLATE_END);
                m.complete(p, LONG_STRING_TEMPLATE_ENTRY);
            }
            STRING_CLOSE => {
                p.bump();
                return;
            }
            _ => {
                p.error_expecting("'\"'");
                return;
            }
        }
    }
}
