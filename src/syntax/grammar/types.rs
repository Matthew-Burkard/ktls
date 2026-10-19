//! Type references, type parameters and type arguments.

use super::super::kind::SyntaxKind::*;
use super::super::parser::Parser;
use super::{annotation, skip_balanced};

/// TYPE_REFERENCE wrapping any type.
pub(crate) fn type_reference(p: &mut Parser) {
    type_reference_inner(p, false);
}

/// The receiver type of an extension declaration: like a type reference, but a
/// trailing `.name` followed by `(`/`:`/`=`… is left for the declaration.
pub(crate) fn user_type_receiver(p: &mut Parser) {
    type_reference_inner(p, true);
}

fn type_reference_inner(p: &mut Parser, receiver: bool) {
    let m = p.start();
    type_modifiers(p);
    non_null_type(p, receiver);
    m.complete(p, TYPE_REFERENCE);
}

fn type_modifiers(p: &mut Parser) {
    let suspend = |p: &Parser| p.at_soft("suspend") && matches!(p.nth(1), L_PAREN | IDENT | AT);
    let context = |p: &Parser| p.at_soft("context") && p.nth_at(1, L_PAREN);
    if !(p.at(AT) || suspend(p) || context(p)) {
        return;
    }
    let m = p.start();
    loop {
        if p.at(AT) {
            annotation(p);
        } else if context(p) {
            super::context_receivers(p);
        } else if suspend(p) {
            let md = p.start();
            p.bump();
            md.complete(p, MODIFIER);
        } else {
            break;
        }
    }
    m.complete(p, MODIFIER_LIST);
}

/// A type possibly followed by `?` and `& Any`.
fn non_null_type(p: &mut Parser, receiver: bool) {
    let Some(mut ty) = simple_type(p, receiver) else {
        return;
    };
    while p.at(QUESTION) && !p.nl_before() {
        let m = ty.precede(p);
        p.bump();
        ty = m.complete(p, NULLABLE_TYPE);
    }
    // Function type with a (possibly nullable / parenthesized) receiver: `A?.() -> B`
    if p.at(DOT) && p.nth_at(1, L_PAREN) && function_type_params_ahead(p, 1) {
        let wrapper = ty.precede(p);
        let recv = wrapper.complete(p, TYPE_REFERENCE);
        let r = recv.precede(p);
        let r = r.complete(p, FUNCTION_TYPE_RECEIVER);
        let f = r.precede(p);
        p.bump(); // .
        function_type_rest(p);
        ty = f.complete(p, FUNCTION_TYPE);
    }
    if p.at(AMP) && !p.nl_before() {
        let m = ty.precede(p);
        p.bump();
        let rhs = p.start();
        simple_type(p, false);
        rhs.complete(p, TYPE_REFERENCE);
        m.complete(p, DEFINITELY_NON_NULL_TYPE);
    }
}

fn simple_type(p: &mut Parser, receiver: bool) -> Option<super::super::parser::CompletedMarker> {
    match p.current() {
        L_PAREN => {
            if function_type_params_ahead(p, 0) {
                let m = p.start();
                function_type_rest(p);
                Some(m.complete(p, FUNCTION_TYPE))
            } else {
                let m = p.start();
                p.bump();
                type_reference(p);
                p.expect(R_PAREN);
                Some(m.complete(p, PARENTHESIZED_TYPE))
            }
        }
        IDENT if p.at_soft("dynamic") && !matches!(p.nth(1), DOT | LT) => {
            let m = p.start();
            p.bump();
            Some(m.complete(p, DYNAMIC_TYPE))
        }
        IDENT => Some(user_type(p, receiver)),
        _ => {
            p.error_expecting("type");
            None
        }
    }
}

/// `(params) -> ret`, starting at `(`.
fn function_type_rest(p: &mut Parser) {
    let params = p.start();
    p.bump(); // (
    while !p.at(R_PAREN) && !p.at_eof() {
        let param = p.start();
        if p.at(IDENT) && p.nth_at(1, COLON) {
            p.bump();
            p.bump();
        }
        type_reference(p);
        param.complete(p, VALUE_PARAMETER);
        if !p.bump_if(COMMA) {
            break;
        }
    }
    p.expect(R_PAREN);
    params.complete(p, FUNCTION_TYPE_PARAMETER_LIST);
    if p.expect(ARROW) {
        type_reference(p);
    }
}

/// Token `n` is `(`: is the matching `)` followed by `->`?
fn function_type_params_ahead(p: &Parser, n: usize) -> bool {
    let end = skip_balanced(p, n, L_PAREN, R_PAREN);
    p.nth_at(end, ARROW)
}

/// `a.b.C<T>.D<U>`
pub(crate) fn user_type(p: &mut Parser, receiver: bool) -> super::super::parser::CompletedMarker {
    let m = p.start();
    loop {
        let seg = p.start();
        p.expect(IDENT);
        if p.at(LT) && !p.nl_before() {
            type_argument_list(p);
        }
        seg.complete(p, USER_TYPE_SEGMENT);
        if !(p.at(DOT) && p.nth_at(1, IDENT)) {
            break;
        }
        if receiver && !matches!(p.nth(2), DOT | LT | QUESTION) {
            // `.name` belongs to the declaration, not the receiver type.
            break;
        }
        p.bump(); // .
    }
    m.complete(p, USER_TYPE)
}

pub(crate) fn type_argument_list(p: &mut Parser) {
    let m = p.start();
    p.bump(); // <
    while !p.at(GT) && !p.at_eof() {
        if p.at(STAR) {
            let s = p.start();
            p.bump();
            s.complete(p, STAR_PROJECTION);
        } else {
            let proj = p.start();
            projection_modifiers(p);
            type_reference(p);
            proj.complete(p, TYPE_PROJECTION);
        }
        if !p.bump_if(COMMA) {
            break;
        }
    }
    p.expect(GT);
    m.complete(p, TYPE_ARGUMENT_LIST);
}

fn projection_modifiers(p: &mut Parser) {
    let variance = |p: &Parser| p.at(IN_KW) || (p.at_soft("out") && matches!(p.nth(1), IDENT | L_PAREN | AT));
    if !(variance(p) || p.at(AT)) {
        return;
    }
    let m = p.start();
    loop {
        if p.at(AT) {
            annotation(p);
        } else if variance(p) {
            let md = p.start();
            p.bump();
            md.complete(p, MODIFIER);
        } else {
            break;
        }
    }
    m.complete(p, MODIFIER_LIST);
}

pub(crate) fn type_parameter_list(p: &mut Parser) {
    let m = p.start();
    p.bump(); // <
    while !p.at(GT) && !p.at_eof() {
        let tp = p.start();
        type_parameter_modifiers(p);
        if !p.bump_if(IDENT) {
            p.error_expecting("type parameter name");
            tp.complete(p, ERROR);
            break;
        }
        if p.bump_if(COLON) {
            type_reference(p);
        }
        tp.complete(p, TYPE_PARAMETER);
        if !p.bump_if(COMMA) {
            break;
        }
    }
    p.expect(GT);
    m.complete(p, TYPE_PARAMETER_LIST);
}

fn type_parameter_modifiers(p: &mut Parser) {
    let is_mod = |p: &Parser| {
        p.at(IN_KW) || ((p.at_soft("out") || p.at_soft("reified")) && matches!(p.nth(1), IDENT | AT))
    };
    if !(is_mod(p) || p.at(AT)) {
        return;
    }
    let m = p.start();
    loop {
        if p.at(AT) {
            annotation(p);
        } else if is_mod(p) {
            let md = p.start();
            p.bump();
            md.complete(p, MODIFIER);
        } else {
            break;
        }
    }
    m.complete(p, MODIFIER_LIST);
}

/// `where T : A, U : B`
pub(crate) fn type_constraints(p: &mut Parser) {
    let m = p.start();
    p.bump(); // where
    loop {
        let c = p.start();
        while p.at(AT) {
            annotation(p);
        }
        p.expect(IDENT);
        p.expect(COLON);
        type_reference(p);
        c.complete(p, TYPE_CONSTRAINT);
        if !p.bump_if(COMMA) {
            break;
        }
    }
    m.complete(p, TYPE_CONSTRAINT_LIST);
}

/// Speculatively check whether `<` at the current position starts a type
/// argument list of a call (`foo<Int>()`), as opposed to a comparison.
pub(crate) fn type_arguments_ahead(p: &Parser) -> bool {
    debug_assert!(p.at(LT));
    let mut depth = 0i32;
    let mut parens = 0i32;
    let mut i = 0;
    while i < 256 {
        match p.nth(i) {
            LT => depth += 1,
            GT => {
                depth -= 1;
                if depth == 0 {
                    let next = p.nth(i + 1);
                    return matches!(next, L_PAREN | COLON2 | DOT | QUESTION | R_PAREN | COMMA | SEMI | EOF | R_BRACK | R_CURLY | EXCL2)
                        || (next == L_CURLY && !p.nth_nl_before(i + 1))
                        || (next == IDENT && p.nth_at(i + 2, AT) && p.nth_joint(i + 2))
                        || p.nth_nl_before(i + 1);
                }
            }
            IDENT | DOT | COMMA | QUESTION | STAR | IN_KW | AT | AMP | COLON2 => {}
            L_PAREN => parens += 1,
            R_PAREN => {
                parens -= 1;
                if parens < 0 {
                    return false;
                }
            }
            ARROW | COLON if parens > 0 || p.nth(i - 1) == R_PAREN => {}
            _ => return false,
        }
        i += 1;
    }
    false
}
