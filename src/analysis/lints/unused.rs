//! Unused imports, variables, parameters and private declarations; `var` that can be `val`.

use std::collections::HashSet;

use super::*;

/// Names called through operator syntax (or delegation), never by name.
const CONVENTION_NAMES: &[&str] = &[
    "plus", "minus", "times", "div", "rem", "mod", "rangeTo", "rangeUntil", "unaryPlus", "unaryMinus", "not", "inc", "dec",
    "contains", "get", "set", "invoke", "compareTo", "equals", "iterator", "next", "hasNext", "getValue", "setValue",
    "provideDelegate", "plusAssign", "minusAssign", "timesAssign", "divAssign", "remAssign",
];

/// Java serialization hooks, called reflectively.
const SERIALIZATION_NAMES: &[&str] = &["serialVersionUID", "readResolve", "writeReplace", "readObject", "writeObject", "readObjectNoData"];

fn is_convention_name(name: &str) -> bool {
    CONVENTION_NAMES.contains(&name) || name.strip_prefix("component").is_some_and(|n| n.parse::<u32>().is_ok())
}

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    if cx.on(&UNUSED_IMPORT) {
        unused_imports(cx, out);
    }
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::PROPERTY if is_local(&n) => local_variable(cx, &n, out),
            SyntaxKind::FUN => {
                unused_parameters(&n, out);
                unused_declaration(cx, &n, out);
            }
            SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS => unused_declaration(cx, &n, out),
            SyntaxKind::VALUE_PARAMETER => unused_constructor_property(cx, &n, out),
            _ => {}
        }
    }
}

fn unused_imports(cx: &Cx, out: &mut Sink) {
    let Some(file) = cx.snap.file(cx.path) else { return };
    let summary = file.summary();
    // Names used anywhere outside the import list (identifiers and KDoc links).
    let mut used: HashSet<String> = HashSet::new();
    let mut kdoc = String::new();
    for t in tokens(&cx.root) {
        if t.kind() == SyntaxKind::IDENT && !t.parent_ancestors().any(|a| a.kind() == SyntaxKind::IMPORT_LIST) {
            used.insert(ast::ident_text(&t));
        } else if t.kind() == SyntaxKind::KDOC {
            kdoc.push_str(t.text());
        }
    }
    let resolver = cx.engine().map(|_| cx.snap.resolver(cx.path));
    let mut seen = HashSet::new();
    for imp in &summary.imports {
        if imp.star || imp.segments.is_empty() {
            continue;
        }
        let name = imp.alias.clone().unwrap_or_else(|| imp.segments.last().unwrap().0.clone());
        let duplicate = !seen.insert((imp.path(), imp.alias.clone()));
        if !duplicate {
            if used.contains(&name) || is_convention_name(&imp.segments.last().unwrap().0) || kdoc_mentions(&kdoc, &name) {
                continue;
            }
            // Unresolved imports are already errors; with no types available, stay quiet.
            match &resolver {
                Some(r) if r.resolve_import(imp).is_ok() => {}
                _ => continue,
            }
        }
        let range = imp.range;
        out.report(&UNUSED_IMPORT, range, "Unused import directive", Some(Fix::delete("Remove unused import", deletion_range(cx.text, range))));
    }
}

fn kdoc_mentions(kdoc: &str, name: &str) -> bool {
    kdoc.match_indices(name).any(|(i, _)| {
        let before = kdoc[..i].chars().next_back();
        let after = kdoc[i + name.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_') && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// `IDENT`s named `name` in `scope` after `after`, excluding `except`.
fn mentions(scope: &SyntaxNode, name: &str, after: TextSize, except: &SyntaxToken) -> Vec<SyntaxToken> {
    idents(scope).filter(|t| t.text_range().start() >= after && t != except && ast::ident_text(t) == name).collect()
}

fn local_variable(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let Some(name_tok) = name_token(n) else { return };
    let name = ast::ident_text(&name_tok);
    if name == "_" {
        return;
    }
    let Some(block) = n.parent() else { return };
    let uses = mentions(&block, &name, n.text_range().end(), &name_tok);
    if uses.is_empty() && cx.on(&UNUSED_VARIABLE) {
        let init = initializer(n);
        let decl_range = deletion_range(cx.text, n.text_range());
        let fix = match &init {
            Some(e) if !is_pure(e) => Fix::replace(format!("Remove variable '{name}'"), n.text_range(), e.text().to_string()),
            _ => Fix::delete(format!("Remove variable '{name}'"), decl_range),
        };
        let fix = (!has_comments(n) && !has_annotations(n)).then_some(fix);
        out.report(&UNUSED_VARIABLE, name_tok.text_range(), format!("Variable '{name}' is never used"), fix);
        return;
    }
    if !cx.on(&CAN_BE_VAL) {
        return;
    }
    let Some(var) = ast::token(n, SyntaxKind::VAR_KW) else { return };
    let decl = ast::PropertyDecl::cast(n.clone()).unwrap();
    if !decl.has_initializer() || decl.has_delegate() {
        return;
    }
    if !uses.iter().any(is_write) {
        out.report(
            &CAN_BE_VAL,
            var.text_range(),
            "Variable is never modified, so it can be declared using 'val'",
            Some(Fix::replace("Change to 'val'", var.text_range(), "val")),
        );
    }
}

/// The token is assigned, incremented, or referenced as a (mutable) property.
fn is_write(t: &SyntaxToken) -> bool {
    let Some(nr) = t.parent().filter(|p| p.kind() == SyntaxKind::NAME_REF) else { return false };
    let Some(parent) = nr.parent() else { return false };
    match parent.kind() {
        SyntaxKind::BINARY_EXPR => assignment_op(&parent).is_some() && parent.first_child().as_ref() == Some(&nr),
        SyntaxKind::PREFIX_EXPR | SyntaxKind::POSTFIX_EXPR => {
            binary_op(&parent).is_some_and(|op| matches!(op.kind(), SyntaxKind::PLUS2 | SyntaxKind::MINUS2))
        }
        SyntaxKind::CALLABLE_REFERENCE => true,
        _ => false,
    }
}

/// Evaluating the expression has no side effects.
fn is_pure(e: &SyntaxNode) -> bool {
    match e.kind() {
        SyntaxKind::LITERAL | SyntaxKind::NAME_REF | SyntaxKind::THIS_EXPR | SyntaxKind::CLASS_LITERAL | SyntaxKind::CALLABLE_REFERENCE => true,
        SyntaxKind::STRING_TEMPLATE => e.children().all(|c| matches!(c.kind(), SyntaxKind::LITERAL_STRING_ENTRY | SyntaxKind::ESCAPE_STRING_ENTRY)),
        _ => false,
    }
}

fn unused_parameters(f: &SyntaxNode, out: &mut Sink) {
    if !(is_local(f) || has_modifier(f, "private")) || has_annotations(f) {
        return;
    }
    if ["override", "open", "abstract", "actual", "expect", "operator", "external"].iter().any(|m| has_modifier(f, m)) {
        return;
    }
    let decl = ast::FunDecl::cast(f.clone()).unwrap();
    let body = decl.body_block().map(|b| b.syntax().clone()).or_else(|| initializer(f));
    let Some(body) = body else { return };
    if is_empty_block(&body) || is_todo(&body) || name_token(f).is_some_and(|t| t.text() == "main") {
        return;
    }
    for p in decl.parameters() {
        let Some(tok) = p.name_token() else { continue };
        let name = ast::ident_text(&tok);
        if name == "_" || has_annotations(p.syntax()) {
            continue;
        }
        if mentions(f, &name, tok.text_range().end(), &tok).is_empty() {
            out.report(&UNUSED_PARAMETER, tok.text_range(), format!("Parameter '{name}' is never used"), None);
        }
    }
}

/// `TODO()` / `{ TODO() }`.
fn is_todo(body: &SyntaxNode) -> bool {
    let e = if body.kind() == SyntaxKind::BLOCK {
        let Some(s) = only_statement(body) else { return false };
        s
    } else {
        body.clone()
    };
    e.kind() == SyntaxKind::CALL_EXPR && e.first_child().is_some_and(|c| c.text() == "TODO")
}

fn unused_declaration(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&UNUSED_SYMBOL) {
        return;
    }
    let local = is_local(n);
    if n.kind() == SyntaxKind::PROPERTY && local {
        return; // UNUSED_VARIABLE
    }
    if !(local || has_modifier(n, "private")) || has_annotations(n) {
        return;
    }
    if ["override", "operator", "expect", "actual", "external", "companion"].iter().any(|m| has_modifier(n, m)) {
        return;
    }
    let Some(name_tok) = name_token(n) else { return };
    let name = ast::ident_text(&name_tok);
    if SERIALIZATION_NAMES.contains(&name.as_str()) || name == "main" || name == "_" {
        return;
    }
    // Members of annotated classes may be used by frameworks (serialization, DI, ORM).
    if n.ancestors().skip(1).any(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION) && has_annotations(&a)) {
        return;
    }
    if n.kind() == SyntaxKind::PROPERTY && ast::PropertyDecl::cast(n.clone()).unwrap().is_destructuring() {
        return;
    }
    let scope = if local { n.parent().unwrap() } else { cx.root.clone() };
    let after = if local { n.text_range().end() } else { TextSize::from(0) };
    // Recursive uses inside the declaration itself do not count.
    let used = idents(&scope).any(|t| t.text_range().start() >= after && !n.text_range().contains_range(t.text_range()) && ast::ident_text(&t) == name);
    if used || named_in_annotation(&cx.root, &name) {
        return;
    }
    let what = match n.kind() {
        SyntaxKind::FUN if local => "Local function",
        SyntaxKind::FUN => "Private function",
        SyntaxKind::PROPERTY => "Private property",
        SyntaxKind::TYPEALIAS => "Type alias",
        SyntaxKind::OBJECT_DECLARATION => "Object",
        _ if local => "Local class",
        _ => "Private class",
    };
    let fix = Fix::delete(format!("Remove '{name}'"), deletion_range(cx.text, n.text_range()));
    out.report(&UNUSED_SYMBOL, name_tok.text_range(), format!("{what} '{name}' is never used"), Some(fix));
}

/// Frameworks refer to members by name in annotation arguments
/// (`@MethodSource("cases")`, `@EnabledIf("available")`, `@EnabledIf("Class#method")`).
fn named_in_annotation(root: &SyntaxNode, name: &str) -> bool {
    tokens(root).any(|t| {
        t.kind() == SyntaxKind::STRING_TEXT
            && t.text().rsplit(['#', '.']).next() == Some(name)
            && t.parent_ancestors().any(|a| a.kind() == SyntaxKind::ANNOTATION)
    })
}

/// `class A(private val x: Int)` where `x` is never used.
fn unused_constructor_property(cx: &Cx, p: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&UNUSED_SYMBOL) || !has_modifier(p, "private") || has_annotations(p) {
        return;
    }
    let vp = ast::ValueParameter::cast(p.clone()).unwrap();
    if vp.property_keyword().is_none() {
        return;
    }
    let Some(class) = p.ancestors().find(|a| a.kind() == SyntaxKind::CLASS) else { return };
    if !p.ancestors().any(|a| a.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR) {
        return;
    }
    if has_annotations(&class) || ["data", "value", "inline", "expect", "actual", "annotation"].iter().any(|m| has_modifier(&class, m)) {
        return;
    }
    let Some(tok) = vp.name_token() else { return };
    let name = ast::ident_text(&tok);
    if idents(&cx.root).any(|t| t != tok && ast::ident_text(&t) == name) || named_in_annotation(&cx.root, &name) {
        return;
    }
    out.report(&UNUSED_SYMBOL, tok.text_range(), format!("Private property '{name}' is never used"), None);
}
