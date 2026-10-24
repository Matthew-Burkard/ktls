//! Declarations that are not allowed in their form: type parameters of objects and enum
//! classes, named anonymous functions, `tailrec` on open members or without tail calls.

use super::super::*;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::OBJECT_DECLARATION | SyntaxKind::CLASS => {
                type_parameters(&n, out);
                local_classifier(&n, out);
                class_members(&n, out);
            }
            SyntaxKind::TYPEALIAS => type_alias(&n, out),
            SyntaxKind::WHEN_EXPR => when_shape(&n, out),
            SyntaxKind::TYPE_PARAMETER => extension_bound(&n, out),
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
        let end = kw.next_token().filter(|t| t.kind().is_trivia()).map_or(kw.text_range().end(), |t| t.text_range().end());
        out.report(&TAILREC_ON_VIRTUAL_MEMBER_ERROR, kw.text_range(), "Tailrec is not allowed on open members.", Some(Fix::delete("Remove 'tailrec' modifier", TextRange::new(kw.text_range().start(), end))));
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

fn is_interface(c: &SyntaxNode) -> bool {
    c.kind() == SyntaxKind::CLASS && ast::token(c, SyntaxKind::INTERFACE_KW).is_some()
}

/// Interfaces, named objects and enum classes in function bodies.
fn local_classifier(n: &SyntaxNode, out: &mut Sink) {
    if n.parent().is_none_or(|p| p.kind() != SyntaxKind::BLOCK) {
        return;
    }
    let Some(name) = name_token(n) else { return };
    let shown = ast::ident_text(&name);
    if n.kind() == SyntaxKind::OBJECT_DECLARATION {
        out.report(&LOCAL_OBJECT_NOT_ALLOWED, name.text_range(), format!("Named object '{shown}' is a singleton and cannot be local. Try to use anonymous object instead."), None);
    } else if is_interface(n) {
        out.report(&LOCAL_INTERFACE_NOT_ALLOWED, name.text_range(), format!("'{shown}' is an interface so it cannot be local. Try to use anonymous object or abstract class instead."), None);
    } else if has_modifier(n, "enum") {
        out.report(&LOCAL_ENUM_NOT_ALLOWED, name.text_range(), format!("Enum class '{shown}' cannot be local."), None);
    }
}

/// Rules about what a class or object contains.
fn class_members(n: &SyntaxNode, out: &mut Sink) {
    let body = n.children().find(|c| c.kind() == SyntaxKind::CLASS_BODY);
    let members: Vec<SyntaxNode> = body.iter().flat_map(|b| b.children()).collect();
    let object = n.kind() == SyntaxKind::OBJECT_DECLARATION;
    let local = n.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK);
    let inner = has_modifier(n, "inner");
    let mut companions = 0;
    for m in &members {
        match m.kind() {
            SyntaxKind::OBJECT_DECLARATION if has_modifier(m, "companion") => {
                companions += 1;
                let kw = modifiers(m).and_then(|l| l.modifiers().find(|t| t.text() == "companion"));
                let range = kw.map_or(m.text_range(), |k| k.text_range());
                if object || local || inner {
                    out.report(&COMPANION_OBJECT_NOT_ALLOWED, range, "A companion object is not allowed here.", None);
                } else if companions > 1 {
                    out.report(&MANY_COMPANION_OBJECTS, range, "Only one companion object is allowed per class.", None);
                }
            }
            SyntaxKind::CLASS if has_modifier(m, "inner") && (object || is_interface(n)) => {
                let Some(kw) = modifiers(m).and_then(|l| l.modifiers().find(|t| t.text() == "inner")) else { continue };
                let container = if object { "object" } else { "interface" };
                let end = kw.next_token().filter(|t| t.kind().is_trivia()).map_or(kw.text_range().end(), |t| t.text_range().end());
                out.report(&WRONG_MODIFIER_CONTAINING_DECLARATION, kw.text_range(), format!("Modifier 'inner' is not applicable inside '{container}'."), Some(Fix::delete("Remove 'inner' modifier", TextRange::new(kw.text_range().start(), end))));
            }
            SyntaxKind::SECONDARY_CONSTRUCTOR if object => {
                let range = ast::token(m, SyntaxKind::IDENT).map_or(m.text_range(), |t| t.text_range());
                out.report(&SECONDARY_CONSTRUCTOR_IN_OBJECT, range, "Constructors are not allowed for objects.", None);
            }
            _ => {}
        }
    }
    if n.kind() != SyntaxKind::CLASS {
        return;
    }
    // A primary constructor: every secondary one delegates to `this(…)`.
    if n.children().any(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR) {
        for m in members.iter().filter(|m| m.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR) {
            let delegates_to_this = m.children().find(|c| c.kind() == SyntaxKind::CONSTRUCTOR_DELEGATION_CALL).is_some_and(|d| ast::token(&d, SyntaxKind::THIS_KW).is_some());
            if !delegates_to_this {
                let range = ast::token(m, SyntaxKind::IDENT).map_or(m.text_range(), |t| t.text_range());
                out.report(&PRIMARY_CONSTRUCTOR_DELEGATION_CALL_EXPECTED, range, "Primary constructor call expected.", None);
            }
        }
    }
    if is_interface(n) && let Some(entry) = n.children().filter(|c| c.kind() == SyntaxKind::SUPER_TYPE_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).find(|e| e.kind() == SyntaxKind::DELEGATED_SUPER_TYPE_ENTRY) {
        out.report(&DELEGATION_IN_INTERFACE, entry.text_range(), "Interfaces cannot use delegation.", None);
    }
    if has_modifier(n, "data") {
        for p in n.children().filter(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR).flat_map(|c| c.descendants().filter(|d| d.kind() == SyntaxKind::VALUE_PARAMETER).collect::<Vec<_>>()) {
            if let Some(kw) = modifiers(&p).and_then(|l| l.modifiers().find(|t| t.text() == "vararg")) {
                out.report(&DATA_CLASS_VARARG_PARAMETER, kw.text_range(), "Primary constructor vararg parameters are prohibited for data classes.", None);
            }
        }
    }
}

/// Type aliases: top level only, no bounds on their parameters.
fn type_alias(n: &SyntaxNode, out: &mut Sink) {
    if n.parent().is_some_and(|p| !matches!(p.kind(), SyntaxKind::KOTLIN_FILE | SyntaxKind::SCRIPT_BODY)) {
        let range = name_token(n).map_or(n.text_range(), |t| t.text_range());
        out.report(&TOPLEVEL_TYPEALIASES_ONLY, range, "Nested and local type aliases are not supported.", None);
    }
    for p in n.children().filter(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).filter(|p| p.kind() == SyntaxKind::TYPE_PARAMETER) {
        if let Some(bound) = p.children().find(|c| c.kind() == SyntaxKind::TYPE_REFERENCE) {
            out.report(&BOUND_ON_TYPE_ALIAS_PARAMETER_NOT_ALLOWED, bound.text_range(), "Bounds are not allowed on type alias parameters.", None);
        }
    }
}

/// `when (var x = …)`, and `a, b ->` in a `when` without subject.
fn when_shape(w: &SyntaxNode, out: &mut Sink) {
    match w.children().find(|c| c.kind() == SyntaxKind::WHEN_SUBJECT) {
        Some(subject) => {
            if let Some(p) = subject.children().find(|c| c.kind() == SyntaxKind::PROPERTY) {
                let decl = ast::PropertyDecl::cast(p.clone());
                let bad = decl.as_ref().is_some_and(|d| d.is_var() || d.has_delegate() || !d.has_initializer() || d.is_destructuring());
                if bad {
                    out.report(&ILLEGAL_DECLARATION_IN_WHEN_SUBJECT, p.text_range(), "Illegal variable declaration in 'when' subject: should be a simple val with an initializer.", None);
                }
            }
        }
        None => {
            for entry in w.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY) {
                let conditions: Vec<SyntaxNode> = entry.children().filter(|c| c.kind() == SyntaxKind::WHEN_CONDITION).collect();
                if conditions.len() > 1 {
                    let range = TextRange::new(conditions[0].text_range().start(), conditions[conditions.len() - 1].text_range().end());
                    let joined = conditions.iter().map(|c| c.text().to_string()).collect::<Vec<_>>().join(" || ");
                    out.report(&COMMA_IN_WHEN_CONDITION_WITHOUT_ARGUMENT, range, "Deprecated syntax. Use '||' instead of commas in when-condition for 'when' without argument.", Some(Fix::replace("Replace ',' with '||'", range, joined)));
                }
            }
        }
    }
}

/// `<T : String.() -> Unit>`.
fn extension_bound(p: &SyntaxNode, out: &mut Sink) {
    let Some(bound) = p.children().find(|c| c.kind() == SyntaxKind::TYPE_REFERENCE) else { return };
    let function = bound.descendants().find(|d| d.kind() == SyntaxKind::FUNCTION_TYPE);
    if function.is_some_and(|f| f.parent().as_ref() == Some(&bound) && f.children().any(|c| c.kind() == SyntaxKind::FUNCTION_TYPE_RECEIVER)) {
        out.report(&UPPER_BOUND_IS_EXTENSION_FUNCTION_TYPE, bound.text_range(), "Extension function type cannot be used as an upper bound.", None);
    }
}
