//! Explicit API mode (`explicitApi()`): public declarations need a visibility modifier and,
//! for functions and properties, an explicit type.

use super::super::*;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    if !cx.module.is_some_and(|m| m.kotlin.explicit_api) || cx.test_source {
        return;
    }
    for n in cx.root.descendants() {
        if matches!(n.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::TYPEALIAS) && !cx.in_error(&n) && is_public_api(&n) {
            declaration(cx, &n, out);
        }
    }
}

/// Part of the public API: not local, private, internal, protected or an override, in public
/// classes only (companions included).
pub(crate) fn is_public_api(n: &SyntaxNode) -> bool {
    let hidden = |d: &SyntaxNode| ["private", "internal", "protected"].iter().any(|m| has_modifier(d, m));
    if hidden(n) || has_modifier(n, "override") || n.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::BLOCK | SyntaxKind::SCRIPT_BODY)) {
        return false;
    }
    // Every enclosing declaration public too (and no anonymous objects or local classes).
    for a in n.ancestors().skip(1) {
        match a.kind() {
            SyntaxKind::OBJECT_LITERAL | SyntaxKind::BLOCK | SyntaxKind::FUN => return false,
            SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION if hidden(&a) => return false,
            _ => {}
        }
    }
    true
}

fn declaration(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let Some(name) = name_token(n) else { return };
    // Properties of data classes are exempt.
    if n.kind() == SyntaxKind::PROPERTY && n.parent().and_then(|b| b.parent()).is_some_and(|c| has_modifier(&c, "data")) {
        return;
    }
    // Enum entries' and companions' members follow their class; `main` is an entry point.
    if n.kind() == SyntaxKind::FUN && name.text() == "main" && n.parent().is_some_and(|p| p.kind() == SyntaxKind::KOTLIN_FILE) {
        return;
    }
    if !has_modifier(n, "public") {
        let at = modifiers(n).map_or_else(|| n.first_token().map_or(name.text_range().start(), |t| t.text_range().start()), |m| m.syntax().text_range().end());
        let at = if modifiers(n).is_some() { at } else { n.children_with_tokens().find(|t| !t.kind().is_trivia()).map_or(at, |t| t.text_range().start()) };
        let sep = if modifiers(n).is_some() { " public" } else { "public " };
        let fix = Fix::replace("Make 'public' explicitly", TextRange::empty(at), sep);
        out.report(&NO_EXPLICIT_VISIBILITY_IN_API_MODE, name.text_range(), "Visibility must be specified in explicit API mode.", Some(fix));
    }
    let implicit_type = match n.kind() {
        SyntaxKind::FUN => ast::FunDecl::cast(n.clone()).is_some_and(|f| f.return_type().is_none() && f.has_expression_body()),
        SyntaxKind::PROPERTY => ast::PropertyDecl::cast(n.clone()).is_some_and(|p| p.type_ref().is_none() && !p.is_destructuring()),
        _ => false,
    };
    if implicit_type {
        let fix = cx.engine().and_then(|e| {
            let value = initializer(n).or_else(|| n.children().filter(|c| c.kind() == SyntaxKind::PROPERTY_ACCESSOR).find_map(|a| initializer(&a)))?;
            let t = e.type_of(&value)?;
            let shown = t.display().replace('!', "");
            if shown.contains("<error>") || shown.is_empty() {
                return None;
            }
            let at = match n.kind() {
                SyntaxKind::FUN => n.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST)?.text_range().end(),
                _ => name.text_range().end(),
            };
            Some(Fix::replace(format!("Specify type explicitly ('{shown}')"), TextRange::empty(at), format!(": {shown}")))
        });
        out.report(&NO_EXPLICIT_RETURN_TYPE_IN_API_MODE, name.text_range(), "Return type must be specified in explicit API mode.", fix);
    }
}
