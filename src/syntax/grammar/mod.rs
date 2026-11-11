//! Kotlin grammar: files, declarations and modifiers.
//!
//! Types live in [`types`], expressions and statements in [`expressions`].

mod expressions;
mod types;

use super::kind::{SyntaxKind::*, is_modifier_keyword};
use super::parser::{CompletedMarker, Marker, Parser};

pub(crate) use expressions::{block, expression};
use expressions::{statement_or_declaration, value_argument_list};
use types::{type_reference, user_type_receiver};

pub(crate) fn kotlin_file(p: &mut Parser, script: bool) {
    let file = p.start();

    file_annotations(p);
    if p.at(PACKAGE_KW) || (p.at(AT) && package_after_modifiers(p)) {
        package_directive(p);
    }
    while p.bump_if(SEMI) {}
    import_list(p);

    while !p.at_eof() {
        if p.bump_if(SEMI) {
            continue;
        }
        if p.at_soft("import") && p.nth_at(1, IDENT) {
            // Imports after declarations: still parse them so that resolution works.
            let list = p.start();
            p.error("Imports are only allowed at the beginning of the file");
            import_directive(p);
            list.complete(p, IMPORT_LIST);
            continue;
        }
        if declaration_ahead(p, DeclContext::TopLevel) {
            declaration(p, DeclContext::TopLevel);
            continue;
        }
        if matches!(p.current(), R_CURLY | R_PAREN | R_BRACK) {
            p.err_and_bump("Unexpected closing bracket");
            continue;
        }
        if script {
            statement_or_declaration(p, DeclContext::Local);
            expressions::statement_separator(p);
            continue;
        }
        p.err_recover_until("Expecting a top level declaration", |p| {
            p.nl_before() && declaration_ahead(p, DeclContext::TopLevel)
        });
    }
    file.complete(p, KOTLIN_FILE);
}

fn file_annotations(p: &mut Parser) {
    if !(p.at(AT) && p.nth_at_soft(1, "file") && p.nth_at(2, COLON)) {
        return;
    }
    let m = p.start();
    while p.at(AT) && p.nth_at_soft(1, "file") && p.nth_at(2, COLON) {
        annotation(p);
    }
    m.complete(p, FILE_ANNOTATION_LIST);
}

fn package_after_modifiers(p: &Parser) -> bool {
    let i = skip_modifiers_ahead(p, 0);
    p.nth_at(i, PACKAGE_KW)
}

fn package_directive(p: &mut Parser) {
    let m = p.start();
    modifier_list(p);
    p.expect(PACKAGE_KW);
    if p.at(IDENT) && !p.nl_before() {
        qualified_name(p, false);
    } else {
        p.error_expecting("package name");
    }
    m.complete(p, PACKAGE_DIRECTIVE);
}

fn import_list(p: &mut Parser) {
    if !p.at_soft("import") {
        return;
    }
    let m = p.start();
    while p.at_soft("import") && (p.nth_at(1, IDENT) || p.nth_nl_before(1) || p.nth_at(1, EOF)) {
        import_directive(p);
        p.bump_if(SEMI);
    }
    m.complete(p, IMPORT_LIST);
}

fn import_directive(p: &mut Parser) {
    let m = p.start();
    p.bump(); // `import`
    if p.at(IDENT) && !p.nl_before() {
        qualified_name(p, true);
        if p.at(AS_KW) {
            let alias = p.start();
            p.bump();
            if !p.bump_if(IDENT) {
                p.error_expecting("alias name");
            }
            alias.complete(p, IMPORT_ALIAS);
        }
    } else {
        p.error_expecting("qualified name");
    }
    m.complete(p, IMPORT_DIRECTIVE);
}

/// `a.b.c` (and optionally a trailing `.*` for imports).
fn qualified_name(p: &mut Parser, allow_star: bool) {
    let m = p.start();
    p.expect(IDENT);
    while p.at(DOT) {
        if p.nth_at(1, IDENT) {
            p.bump();
            p.bump();
        } else if allow_star && p.nth_at(1, STAR) {
            p.bump();
            p.bump();
            break;
        } else {
            p.bump();
            p.error_expecting("name");
            break;
        }
    }
    m.complete(p, DOT_QUALIFIED_NAME);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeclContext {
    TopLevel,
    ClassBody,
    Local,
}

/// Is there a declaration (after optional modifiers) at the current position?
pub(crate) fn declaration_ahead(p: &Parser, ctx: DeclContext) -> bool {
    let i = skip_modifiers_ahead(p, 0);
    let has_modifiers = i > 0;
    match p.nth(i) {
        CLASS_KW | INTERFACE_KW | VAL_KW | VAR_KW | TYPEALIAS_KW => true,
        FUN_KW => {
            ctx != DeclContext::Local || has_modifiers || matches!(p.nth(i + 1), IDENT | LT | INTERFACE_KW)
        }
        OBJECT_KW => ctx != DeclContext::Local || p.nth_at(i + 1, IDENT),
        IDENT if ctx == DeclContext::ClassBody => {
            (p.nth_at_soft(i, "init") && p.nth_at(i + 1, L_CURLY)) || p.nth_at_soft(i, "constructor")
        }
        _ => false,
    }
}

/// Parse a declaration; caller has checked [`declaration_ahead`].
pub(crate) fn declaration(p: &mut Parser, ctx: DeclContext) -> Option<CompletedMarker> {
    let m = p.start();
    let mods = modifier_list(p);
    let done = match p.current() {
        CLASS_KW | INTERFACE_KW => class(p, m, mods.is_enum),
        FUN_KW if p.nth_at(1, INTERFACE_KW) => {
            p.bump();
            class(p, m, false)
        }
        FUN_KW => fun(p, m),
        VAL_KW | VAR_KW => property(p, m, ctx),
        OBJECT_KW => object_declaration(p, m),
        TYPEALIAS_KW => typealias(p, m),
        IDENT if p.at_soft("init") => {
            p.bump();
            block(p);
            m.complete(p, CLASS_INITIALIZER)
        }
        IDENT if p.at_soft("constructor") => secondary_constructor(p, m),
        _ => {
            p.error_expecting("declaration");
            m.complete(p, ERROR)
        }
    };
    Some(done)
}

#[derive(Default)]
pub(crate) struct Modifiers {
    pub is_enum: bool,
    pub any: bool,
}

/// Returns the token index after any modifiers/annotations starting at `start`.
pub(crate) fn skip_modifiers_ahead(p: &Parser, start: usize) -> usize {
    let mut i = start;
    loop {
        match p.nth(i) {
            AT => i = skip_annotation_ahead(p, i),
            IDENT if p.nth_text(i) == "context" && p.nth_at(i + 1, L_PAREN) => {
                i = skip_balanced(p, i + 1, L_PAREN, R_PAREN);
            }
            IDENT if modifier_at(p, i) => i += 1,
            _ => return i,
        }
    }
}

/// Whether the identifier at `i` is a modifier keyword in modifier position.
fn modifier_at(p: &Parser, i: usize) -> bool {
    if !is_modifier_keyword(p.nth_text(i)) {
        return false;
    }
    match p.nth(i + 1) {
        CLASS_KW | INTERFACE_KW | FUN_KW | VAL_KW | VAR_KW | OBJECT_KW | TYPEALIAS_KW | AT => true,
        IDENT => {
            let next = p.nth_text(i + 1);
            !p.nth_nl_before(i + 1)
                || is_modifier_keyword(next)
                || matches!(next, "constructor" | "init" | "get" | "set" | "context")
        }
        _ => false,
    }
}

pub(crate) fn skip_annotation_ahead(p: &Parser, start: usize) -> usize {
    let mut i = start + 1; // '@'
    if p.nth_at(i, IDENT) && p.nth_at(i + 1, COLON) && p.nth_joint(i + 1) && is_use_site_target(p.nth_text(i)) {
        i += 2;
    }
    if p.nth_at(i, L_BRACK) {
        return skip_balanced(p, i, L_BRACK, R_BRACK);
    }
    if !p.nth_at(i, IDENT) {
        return i;
    }
    i += 1;
    while p.nth_at(i, DOT) && p.nth_at(i + 1, IDENT) {
        i += 2;
    }
    if p.nth_at(i, LT) && p.nth_joint(i) {
        i = skip_balanced(p, i, LT, GT);
    }
    if p.nth_at(i, L_PAREN) && !p.nth_nl_before(i) {
        i = skip_balanced(p, i, L_PAREN, R_PAREN);
    }
    i
}

/// `start` must point at `open`; returns the index after the matching `close`.
pub(crate) fn skip_balanced(p: &Parser, start: usize, open: super::kind::SyntaxKind, close: super::kind::SyntaxKind) -> usize {
    let mut depth = 0;
    let mut i = start;
    loop {
        let k = p.nth(i);
        if k == EOF {
            return i;
        }
        if k == open {
            depth += 1;
        } else if k == close {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
        i += 1;
    }
}

fn is_use_site_target(s: &str) -> bool {
    matches!(s, "field" | "file" | "property" | "get" | "set" | "receiver" | "param" | "setparam" | "delegate" | "all")
}

/// Parses modifiers and annotations into a MODIFIER_LIST (if any).
pub(crate) fn modifier_list(p: &mut Parser) -> Modifiers {
    let mut mods = Modifiers::default();
    if !(p.at(AT) || (p.at(IDENT) && (modifier_at(p, 0) || (p.at_soft("context") && p.nth_at(1, L_PAREN))))) {
        return mods;
    }
    let m = p.start();
    loop {
        if p.at(AT) {
            annotation(p);
        } else if p.at_soft("context") && p.nth_at(1, L_PAREN) {
            context_receivers(p);
        } else if p.at(IDENT) && modifier_at(p, 0) {
            if p.current_text() == "enum" {
                mods.is_enum = true;
            }
            let md = p.start();
            p.bump();
            md.complete(p, MODIFIER);
        } else {
            break;
        }
        mods.any = true;
    }
    m.complete(p, MODIFIER_LIST);
    mods
}

pub(crate) fn context_receivers(p: &mut Parser) {
    let m = p.start();
    p.bump(); // context
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
    m.complete(p, CONTEXT_RECEIVER_LIST);
}

/// `@Ann`, `@Ann(args)`, `@target:Ann`, `@[A B]`.
pub(crate) fn annotation(p: &mut Parser) {
    let m = p.start();
    p.bump(); // @
    if p.at(IDENT) && p.nth_at(1, COLON) && p.nth_joint(1) && is_use_site_target(p.current_text()) {
        let t = p.start();
        p.bump();
        p.bump();
        t.complete(p, ANNOTATION_TARGET);
    }
    if p.at(L_BRACK) {
        p.bump();
        while p.at(IDENT) {
            let entry = p.start();
            annotation_body(p);
            entry.complete(p, ANNOTATION);
        }
        p.expect(R_BRACK);
        m.complete(p, ANNOTATION_GROUP);
        return;
    }
    if p.at(IDENT) {
        annotation_body(p);
    } else {
        p.error_expecting("annotation name");
    }
    m.complete(p, ANNOTATION);
}

fn annotation_body(p: &mut Parser) {
    let ty = p.start();
    types::user_type(p, false);
    ty.complete(p, TYPE_REFERENCE);
    if p.at(L_PAREN) && !p.nl_before() {
        value_argument_list(p);
    }
}

// ---------------------------------------------------------------- classes

fn class(p: &mut Parser, m: Marker, is_enum: bool) -> CompletedMarker {
    p.bump(); // class | interface
    if !p.bump_if(IDENT) {
        p.error_expecting("class name");
    }
    if p.at(LT) {
        types::type_parameter_list(p);
    }
    if primary_constructor_ahead(p) {
        primary_constructor(p);
    }
    if p.at(COLON) {
        super_type_list(p);
    }
    if p.at_soft("where") {
        types::type_constraints(p);
    }
    if p.at(L_CURLY) {
        class_body(p, is_enum);
    }
    m.complete(p, CLASS)
}

fn primary_constructor_ahead(p: &Parser) -> bool {
    if p.at(L_PAREN) {
        return true;
    }
    if !(p.at(AT) || p.at(IDENT)) {
        return false;
    }
    let i = skip_modifiers_ahead(p, 0);
    p.nth_at_soft(i, "constructor")
}

fn primary_constructor(p: &mut Parser) {
    let m = p.start();
    modifier_list(p);
    if p.at_soft("constructor") {
        p.bump();
    }
    if p.at(L_PAREN) {
        value_parameter_list(p);
    } else {
        p.error_expecting("'('");
    }
    m.complete(p, PRIMARY_CONSTRUCTOR);
}

fn super_type_list(p: &mut Parser) {
    let m = p.start();
    p.bump(); // :
    loop {
        super_type_entry(p);
        if !p.bump_if(COMMA) {
            break;
        }
    }
    m.complete(p, SUPER_TYPE_LIST);
}

pub(crate) fn super_type_entry(p: &mut Parser) {
    let m = p.start();
    type_reference(p);
    if p.at(L_PAREN) && !p.nl_before() {
        value_argument_list(p);
        m.complete(p, SUPER_TYPE_CALL_ENTRY);
    } else if p.at_soft("by") {
        p.bump();
        let old = p.no_trailing_lambda;
        p.no_trailing_lambda = true;
        expression(p);
        p.no_trailing_lambda = old;
        m.complete(p, DELEGATED_SUPER_TYPE_ENTRY);
    } else {
        m.complete(p, SUPER_TYPE_ENTRY);
    }
}

pub(crate) fn class_body(p: &mut Parser, is_enum: bool) {
    let m = p.start();
    if p.too_deep() {
        p.skip_too_deep();
        m.complete(p, CLASS_BODY);
        return;
    }
    p.bump(); // {
    p.with_newlines(true, |p| {
        if is_enum {
            enum_entries(p);
        }
        class_members(p);
    });
    p.expect(R_CURLY);
    m.complete(p, CLASS_BODY);
}

fn class_members(p: &mut Parser) {
    while !p.at(R_CURLY) && !p.at_eof() {
        if p.bump_if(SEMI) {
            continue;
        }
        if declaration_ahead(p, DeclContext::ClassBody) {
            declaration(p, DeclContext::ClassBody);
            continue;
        }
        p.err_recover_until("Expecting member declaration", |p| {
            p.at(R_CURLY) || (p.nl_before() && declaration_ahead(p, DeclContext::ClassBody))
        });
    }
}

fn enum_entries(p: &mut Parser) {
    loop {
        let i = skip_modifiers_ahead(p, 0);
        let is_entry = p.nth_at(i, IDENT)
            && !declaration_ahead(p, DeclContext::ClassBody)
            && matches!(p.nth(i + 1), COMMA | SEMI | L_PAREN | L_CURLY | R_CURLY | IDENT | AT | EOF);
        if !is_entry {
            break;
        }
        let m = p.start();
        modifier_list(p);
        p.bump(); // name
        if p.at(L_PAREN) {
            value_argument_list(p);
        }
        if p.at(L_CURLY) {
            class_body(p, false);
        }
        m.complete(p, ENUM_ENTRY);
        if !p.bump_if(COMMA) {
            break;
        }
    }
    p.bump_if(SEMI);
}

fn object_declaration(p: &mut Parser, m: Marker) -> CompletedMarker {
    p.bump(); // object
    p.bump_if(IDENT);
    if p.at(LT) {
        types::type_parameter_list(p);
    }
    // Not allowed (CONSTRUCTOR_IN_OBJECT), but parsed like Kotlin does.
    if primary_constructor_ahead(p) && !p.nl_before() {
        primary_constructor(p);
    }
    if p.at(COLON) {
        super_type_list(p);
    }
    if p.at(L_CURLY) {
        class_body(p, false);
    }
    m.complete(p, OBJECT_DECLARATION)
}

fn secondary_constructor(p: &mut Parser, m: Marker) -> CompletedMarker {
    p.bump(); // constructor
    if p.at(L_PAREN) {
        value_parameter_list(p);
    } else {
        p.error_expecting("'('");
    }
    if p.at(COLON) {
        p.bump();
        let call = p.start();
        if p.at(THIS_KW) || p.at(SUPER_KW) {
            p.bump();
        } else {
            p.error_expecting("'this' or 'super'");
        }
        if p.at(L_PAREN) {
            value_argument_list(p);
        }
        call.complete(p, CONSTRUCTOR_DELEGATION_CALL);
    }
    if p.at(L_CURLY) {
        block(p);
    }
    m.complete(p, SECONDARY_CONSTRUCTOR)
}

fn typealias(p: &mut Parser, m: Marker) -> CompletedMarker {
    p.bump(); // typealias
    if !p.bump_if(IDENT) {
        p.error_expecting("type alias name");
    }
    if p.at(LT) {
        types::type_parameter_list(p);
    }
    if p.expect(EQ) {
        type_reference(p);
    }
    m.complete(p, TYPEALIAS)
}

// ------------------------------------------------------- functions & properties

/// Does a receiver type precede the declaration name? (`fun String.foo()`)
fn receiver_ahead(p: &Parser) -> bool {
    let mut i = 0;
    while p.nth_at(i, AT) {
        i = skip_annotation_ahead(p, i);
    }
    if p.nth_at(i, L_PAREN) || (i > 0 && p.nth_at(i, IDENT)) {
        return true;
    }
    p.nth_at(i, IDENT) && matches!(p.nth(i + 1), DOT | LT | QUESTION)
}

fn fun(p: &mut Parser, m: Marker) -> CompletedMarker {
    p.bump(); // fun
    if p.at(LT) {
        types::type_parameter_list(p);
    }
    if receiver_ahead(p) {
        let r = p.start();
        user_type_receiver(p);
        r.complete(p, RECEIVER_TYPE);
        p.expect(DOT);
    }
    if !p.bump_if(IDENT) {
        p.error_expecting("function name");
    }
    if p.at(L_PAREN) {
        value_parameter_list(p);
    } else {
        p.error_expecting("'('");
    }
    if p.bump_if(COLON) {
        type_reference(p);
    }
    if p.at_soft("where") {
        types::type_constraints(p);
    }
    function_body(p);
    m.complete(p, FUN)
}

pub(crate) fn function_body(p: &mut Parser) {
    if p.at(L_CURLY) {
        block(p);
    } else if p.at(EQ) {
        p.bump();
        expression(p);
    }
}

fn property(p: &mut Parser, m: Marker, ctx: DeclContext) -> CompletedMarker {
    p.bump(); // val | var
    if p.at(LT) {
        types::type_parameter_list(p);
    }
    if p.at(L_PAREN) && !p.nth_at(skip_balanced(p, 0, L_PAREN, R_PAREN), DOT) {
        destructuring_declaration(p);
    } else {
        if receiver_ahead(p) {
            let r = p.start();
            user_type_receiver(p);
            r.complete(p, RECEIVER_TYPE);
            p.expect(DOT);
        }
        if !p.bump_if(IDENT) {
            p.error_expecting("property name");
        }
    }
    if p.bump_if(COLON) {
        type_reference(p);
    }
    if p.at_soft("where") {
        types::type_constraints(p);
    }
    if p.at(EQ) {
        p.bump();
        expression(p);
    } else if p.at_soft("by") {
        let d = p.start();
        p.bump();
        expression(p);
        d.complete(p, PROPERTY_DELEGATE);
    }
    if ctx != DeclContext::Local {
        property_accessors(p);
    }
    m.complete(p, PROPERTY)
}

fn accessor_ahead(p: &Parser) -> bool {
    let start = if p.at(SEMI) { 1 } else { 0 };
    let i = skip_modifiers_ahead(p, start);
    if !(p.nth_at_soft(i, "get") || p.nth_at_soft(i, "set")) {
        return false;
    }
    matches!(p.nth(i + 1), L_PAREN | EQ | L_CURLY | SEMI | R_CURLY | EOF | COLON) || p.nth_nl_before(i + 1)
}

fn property_accessors(p: &mut Parser) {
    for _ in 0..2 {
        if !accessor_ahead(p) {
            return;
        }
        p.bump_if(SEMI);
        let m = p.start();
        modifier_list(p);
        let is_setter = p.at_soft("set");
        p.bump(); // get | set
        if p.at(L_PAREN) {
            if is_setter {
                value_parameter_list(p);
            } else {
                p.bump();
                p.expect(R_PAREN);
            }
        }
        if p.bump_if(COLON) {
            type_reference(p);
        }
        function_body(p);
        m.complete(p, PROPERTY_ACCESSOR);
    }
}

pub(crate) fn destructuring_declaration(p: &mut Parser) {
    let m = p.start();
    p.bump(); // (
    while !p.at(R_PAREN) && !p.at_eof() {
        let e = p.start();
        modifier_list(p);
        if p.at(VAL_KW) || p.at(VAR_KW) {
            p.bump();
        }
        if !p.bump_if(IDENT) {
            p.error_expecting("name");
            e.complete(p, ERROR);
            break;
        }
        if p.bump_if(COLON) {
            type_reference(p);
        }
        if p.at(EQ) {
            p.bump();
            expression(p);
        }
        e.complete(p, DESTRUCTURING_ENTRY);
        if !p.bump_if(COMMA) {
            break;
        }
    }
    p.expect(R_PAREN);
    m.complete(p, DESTRUCTURING_DECLARATION);
}

pub(crate) fn value_parameter_list(p: &mut Parser) {
    parameters(p, false);
}

/// `catch (e: T)`: the parameter needs a type.
pub(crate) fn typed_value_parameter_list(p: &mut Parser) {
    parameters(p, true);
}

fn parameters(p: &mut Parser, type_required: bool) {
    let m = p.start();
    p.bump(); // (
    p.with_newlines(false, |p| {
        while !p.at(R_PAREN) && !p.at_eof() && !p.at(R_CURLY) && !p.at(L_CURLY) {
            if !value_parameter(p, type_required) {
                break;
            }
            if !p.bump_if(COMMA) {
                break;
            }
        }
    });
    if !p.expect(R_PAREN) {
        // Recover inside the parameter list so the rest of the declaration survives.
        if !p.at(L_CURLY) && !p.at(EQ) && !p.at(COLON) && !p.at(R_CURLY) && !p.at_eof() {
            p.err_recover_until("Unexpected tokens in parameter list", |p| {
                p.at(R_PAREN) || p.at(L_CURLY) || p.at(EQ)
            });
            p.bump_if(R_PAREN);
        }
    }
    m.complete(p, VALUE_PARAMETER_LIST);
}

fn value_parameter(p: &mut Parser, type_required: bool) -> bool {
    let m = p.start();
    modifier_list(p);
    if p.at(VAL_KW) || p.at(VAR_KW) {
        p.bump();
    }
    if !p.at(IDENT) {
        p.error_expecting("parameter name");
        m.complete(p, ERROR);
        return false;
    }
    if type_required && !p.nth_at(1, COLON) {
        p.error("Parameters must have type annotation");
    }
    p.bump();
    if p.bump_if(COLON) {
        type_reference(p);
    }
    if p.at(EQ) {
        p.bump();
        expression(p);
    }
    m.complete(p, VALUE_PARAMETER);
    true
}
