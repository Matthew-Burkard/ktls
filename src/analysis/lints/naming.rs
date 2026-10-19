//! Naming conventions (https://kotlinlang.org/docs/coding-conventions.html#naming-rules).

use super::*;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    package(cx, out);
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS => class_name(&n, out),
            SyntaxKind::FUN => function_name(cx, &n, out),
            SyntaxKind::PROPERTY => property_name(&n, out),
            SyntaxKind::VALUE_PARAMETER if n.parent().and_then(|l| l.parent()).is_some_and(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR) => {
                if ast::ValueParameter::cast(n.clone()).is_some_and(|p| p.property_keyword().is_some()) {
                    property_name(&n, out);
                }
            }
            SyntaxKind::ENUM_ENTRY => enum_entry_name(&n, out),
            _ => {}
        }
    }
}

/// Backticked names are exempt from every rule.
fn plain_name(n: &SyntaxNode) -> Option<(SyntaxToken, String)> {
    let t = name_token(n)?;
    (!t.text().starts_with('`')).then(|| (t.clone(), t.text().to_string()))
}

fn upper_camel(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_uppercase()) && s.chars().all(|c| c.is_alphanumeric())
}

fn lower_camel(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_lowercase()) && s.chars().all(|c| c.is_alphanumeric())
}

fn screaming(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_uppercase()) && s.chars().all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '_')
}

fn class_name(n: &SyntaxNode, out: &mut Sink) {
    let Some((tok, name)) = plain_name(n) else { return };
    if !upper_camel(&name) {
        out.report(&CLASS_NAME, tok.text_range(), format!("Class name '{name}' should be UpperCamelCase"), None);
    }
}

fn function_name(cx: &Cx, f: &SyntaxNode, out: &mut Sink) {
    let Some((tok, name)) = plain_name(f) else { return };
    if has_modifier(f, "override") {
        return;
    }
    let ok = if cx.test_source {
        // Test names may use underscores (`test_parses_input`).
        name.chars().next().is_some_and(|c| c.is_lowercase()) && name.chars().all(|c| c.is_alphanumeric() || c == '_')
    } else {
        lower_camel(&name)
    };
    if ok {
        return;
    }
    // Factory functions may be named like the type they return; annotated ones
    // (`@Composable`) follow their framework's conventions.
    if upper_camel(&name) {
        let returns_same = ast::FunDecl::cast(f.clone())
            .and_then(|d| d.return_type())
            .and_then(|t| t.syntax().descendants().find_map(ast::UserType::cast))
            .and_then(|ut| ut.segments().last().and_then(|s| s.name_token()))
            .is_some_and(|t| ast::ident_text(&t) == name);
        let no_type = ast::FunDecl::cast(f.clone()).is_some_and(|d| d.return_type().is_none());
        if returns_same || no_type || has_annotations(f) {
            return;
        }
    }
    out.report(&FUNCTION_NAME, tok.text_range(), format!("Function name '{name}' should be lowerCamelCase"), None);
}

fn property_name(p: &SyntaxNode, out: &mut Sink) {
    let Some((tok, name)) = plain_name(p) else { return };
    if name == "_" || has_modifier(p, "override") {
        return;
    }
    let private = has_modifier(p, "private");
    let local = is_local(p);
    let parent = p.parent();
    let top_or_object = parent.as_ref().is_some_and(|x| {
        x.kind() == SyntaxKind::KOTLIN_FILE || x.kind() == SyntaxKind::CLASS_BODY && x.parent().is_some_and(|o| o.kind() == SyntaxKind::OBJECT_DECLARATION)
    });
    let (rule, ok, expected) = if has_modifier(p, "const") {
        (&CONST_PROPERTY_NAME, screaming(&name), "SCREAMING_SNAKE_CASE")
    } else if local {
        (&LOCAL_VARIABLE_NAME, lower_camel(&name), "lowerCamelCase")
    } else if top_or_object {
        // Top-level and object properties: IntelliJ's `[A-Za-z][_A-Za-z\d]*` (constants,
        // singletons, DSL column names...).
        let stripped = name.strip_prefix('_').unwrap_or(&name);
        let ok = stripped.chars().next().is_some_and(char::is_alphabetic) && stripped.chars().all(|c| c.is_alphanumeric() || c == '_');
        (&PROPERTY_NAME, ok, "lowerCamelCase")
    } else {
        let stripped = if private { name.strip_prefix('_').unwrap_or(&name) } else { &name };
        (&PROPERTY_NAME, lower_camel(stripped), "lowerCamelCase")
    };
    if !ok {
        out.report(rule, tok.text_range(), format!("Property name '{name}' should be {expected}"), None);
    }
}

fn enum_entry_name(n: &SyntaxNode, out: &mut Sink) {
    let Some((tok, name)) = plain_name(n) else { return };
    if !(screaming(&name) || upper_camel(&name)) {
        out.report(&ENUM_ENTRY_NAME, tok.text_range(), format!("Enum entry name '{name}' should be UPPER_CASE or UpperCamelCase"), None);
    }
}

fn package(cx: &Cx, out: &mut Sink) {
    let Some(pkg) = cx.root.children().find_map(ast::PackageDirective::cast).and_then(|p| p.name()) else { return };
    let segments: Vec<String> = pkg.segments().iter().map(ast::ident_text).collect();
    let range = pkg.syntax().text_range();
    let valid = |s: &str| s.chars().next().is_some_and(|c| c.is_lowercase() || c == '_') && s.chars().all(|c| c.is_alphanumeric() || c == '_');
    if cx.on(&PACKAGE_NAME) && !pkg.segments().iter().any(|t| t.text().starts_with('`')) && !segments.iter().all(|s| valid(s)) {
        out.report(&PACKAGE_NAME, range, format!("Package name '{}' should be lowercase", segments.join(".")), None);
    }
    if !cx.on(&PACKAGE_DIRECTORY_MISMATCH) {
        return;
    }
    let Some(root) = cx.snap.workspace.source_root_of(cx.path) else { return };
    let Some(dir) = cx.path.parent().and_then(|d| d.strip_prefix(root).ok()) else { return };
    let dirs: Vec<String> = dir.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    // The common root package may be omitted from the directory structure.
    if segments.ends_with(&dirs) {
        return;
    }
    let expected = dirs.join(".");
    let fix = (!dirs.is_empty() && dirs.iter().all(|s| valid(s))).then(|| Fix::replace(format!("Change package to '{expected}'"), range, expected.clone()));
    out.report(&PACKAGE_DIRECTORY_MISMATCH, range, format!("Package '{}' does not match the directory '{}'", segments.join("."), dir.display()), fix);
}
