//! `when` exhaustiveness: enum, Boolean, nullable and (source) sealed subjects must be
//! covered; subject-less `when` used as a value needs `else`.

use std::collections::HashSet;

use super::super::*;
use crate::analysis::diagnostics::{LocalsCache, scope_at};
use crate::analysis::ide::infer::{Engine, class_type, fq_of};
use crate::analysis::resolve::{Classifier, Resolved};
use crate::symbols::{Modifiers, Symbol, SymbolKind, TypeRef, short_class_name};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants().filter(|n| n.kind() == SyntaxKind::WHEN_EXPR) {
        if !in_error(&n) {
            when_expr(cx, e, &n, out);
        }
    }
}

/// The `when`'s value is used.
fn used_as_expression(n: &SyntaxNode) -> bool {
    let Some(p) = n.parent() else { return false };
    match p.kind() {
        SyntaxKind::PROPERTY | SyntaxKind::RETURN_EXPR | SyntaxKind::VALUE_ARGUMENT | SyntaxKind::PAREN_EXPR | SyntaxKind::BINARY_EXPR | SyntaxKind::LONG_STRING_TEMPLATE_ENTRY | SyntaxKind::THROW_EXPR => true,
        SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR => p.first_child().as_ref() == Some(n),
        SyntaxKind::FUN => ast::token(&p, SyntaxKind::EQ).is_some(),
        _ => false,
    }
}

/// What the branches cover.
#[derive(Default)]
struct Covered {
    names: HashSet<String>,
    types: Vec<String>,
    null: bool,
    bools: HashSet<bool>,
}

enum Subject {
    Boolean,
    Enum(Vec<String>),
    Sealed(Symbol),
}

fn when_expr(cx: &Cx, e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(kw) = ast::token(n, SyntaxKind::WHEN_KW) else { return };
    let entries: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY).collect();
    let else_entry = entries.iter().find(|en| ast::token(en, SyntaxKind::ELSE_KW).is_some());
    let Some(subject) = n.children().find(|c| c.kind() == SyntaxKind::WHEN_SUBJECT) else {
        if else_entry.is_none() && used_as_expression(n) {
            out.report(&NO_ELSE_IN_WHEN, kw.text_range(), "'when' expression must be exhaustive. Add an 'else' branch.", add_branches(cx, n, &["else".into()]));
        }
        return;
    };
    let expr = match subject.children().find(|c| c.kind() == SyntaxKind::PROPERTY) {
        Some(p) => initializer(&p),
        None => subject.first_child(),
    };
    let Some(expr) = expr else { return };
    let Some(t) = e.type_of(&expr) else { return };
    let nullable = matches!(&t, TypeRef::Named { nullable: true, .. });
    let Some(fq) = fq_of(&t).map(str::to_string) else { return };
    let kind = if fq == "kotlin.Boolean" {
        Subject::Boolean
    } else {
        let Some(c) = e.classifier(&fq) else { return };
        let sym = c.symbol().clone();
        if sym.modifiers.has(Modifiers::EXPECT) {
            return;
        }
        match (&c, sym.kind) {
            (_, SymbolKind::Enum) => {
                let entries: Vec<String> = e.all_static_members(&c).1.into_iter().filter(|(m, _)| m.kind == SymbolKind::EnumEntry).map(|(m, _)| m.name).collect();
                if entries.is_empty() {
                    return;
                }
                Subject::Enum(entries)
            }
            (Classifier::Source(_), SymbolKind::Class | SymbolKind::Interface) if sym.modifiers.has(Modifiers::SEALED) => Subject::Sealed(sym),
            _ => return,
        }
    };
    // What the branches cover; unknown conditions (`in`, `!is`, arbitrary values) stop the check.
    let mut covered = Covered::default();
    let scope = scope_at(n, &e.package(), &mut LocalsCache::default());
    for en in entries.iter().filter(|en| ast::token(en, SyntaxKind::ELSE_KW).is_none()) {
        if en.children().any(|c| c.kind() == SyntaxKind::WHEN_GUARD) {
            continue;
        }
        for cond in en.children().filter(|c| c.kind() == SyntaxKind::WHEN_CONDITION) {
            if tokens(&cond).any(|t| matches!(t.kind(), SyntaxKind::NOT_IS_KW | SyntaxKind::IN_KW | SyntaxKind::NOT_IN_KW)) {
                return;
            }
            if tokens(&cond).any(|t| t.kind() == SyntaxKind::IS_KW) {
                let Some(ut) = cond.descendants().find_map(ast::UserType::cast) else { return };
                let names: Vec<String> = ut.segments().iter().filter_map(|s| s.name_token()).map(|t| ast::ident_text(&t)).collect();
                let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                let Ok(Resolved::Classifier(c)) = e.res.resolve_type_name(&refs, &scope) else { return };
                covered.types.push(c.fq_name().to_string());
                continue;
            }
            let Some(v) = cond.first_child() else { return };
            match v.kind() {
                SyntaxKind::LITERAL => match v.first_token().map(|t| t.kind()) {
                    Some(SyntaxKind::NULL_KW) => covered.null = true,
                    Some(SyntaxKind::TRUE_KW) => {
                        covered.bools.insert(true);
                    }
                    Some(SyntaxKind::FALSE_KW) => {
                        covered.bools.insert(false);
                    }
                    _ => return,
                },
                SyntaxKind::NAME_REF | SyntaxKind::DOT_QUALIFIED_EXPR => match &kind {
                    Subject::Enum(_) => {
                        let last = v.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF).last().and_then(|l| l.first_token());
                        let Some(last) = last else { return };
                        covered.names.insert(ast::ident_text(&last));
                    }
                    _ => {
                        // An object.
                        let Some(vt) = e.type_of(&v) else { return };
                        let Some(vfq) = fq_of(&vt) else { return };
                        if e.classifier(vfq).is_none_or(|c| c.symbol().kind != SymbolKind::Object) {
                            return;
                        }
                        covered.types.push(vfq.to_string());
                    }
                },
                _ => return,
            }
        }
    }
    let mut missing: Vec<String> = Vec::new();
    match &kind {
        Subject::Boolean => {
            for b in [true, false] {
                if !covered.bools.contains(&b) {
                    missing.push(b.to_string());
                }
            }
        }
        Subject::Enum(entries) => {
            let short = short_class_name(&fq);
            missing.extend(entries.iter().filter(|en| !covered.names.contains(*en)).map(|en| format!("{short}.{en}")));
        }
        Subject::Sealed(sym) => {
            let Some(m) = missing_subclasses(e, sym, &covered, 0) else { return };
            missing.extend(m);
        }
    }
    if nullable && !covered.null {
        missing.push("null".into());
    }
    let missing_fixes = |missing: &[String]| -> Vec<Fix> { add_branches(cx, n, missing).into_iter().chain(add_branches(cx, n, &["else".into()])).collect() };
    match (missing.is_empty(), else_entry) {
        (true, Some(el)) => {
            let fix = Fix::delete("Remove 'else' branch", deletion_range(cx.text, el.text_range()));
            let at = ast::token(el, SyntaxKind::ELSE_KW).map(|t| t.text_range()).unwrap_or(el.text_range());
            out.report(&REDUNDANT_ELSE_IN_WHEN, at, "'when' is exhaustive so 'else' is redundant here.", Some(fix));
        }
        (false, None) => {
            let shown: Vec<String> = missing.iter().map(|m| format!("'{m}'")).collect();
            let what = if used_as_expression(n) { "expression" } else { "statement" };
            out.report_fixes(
                &NO_ELSE_IN_WHEN,
                kw.text_range(),
                format!("'when' {what} must be exhaustive. Add the {} branch{} or an 'else' branch.", shown.join(", "), if missing.len() > 1 { "es" } else { "" }),
                missing_fixes(&missing),
            );
        }
        _ => {}
    }
}

/// Direct subclasses of a sealed source class not covered by the branches, as branch
/// conditions (`is Sub` / `Obj`). `None` if the hierarchy cannot be determined.
fn missing_subclasses(e: &Engine, sealed: &Symbol, covered: &Covered, depth: u32) -> Option<Vec<String>> {
    if depth > 4 {
        return None;
    }
    let mut out = Vec::new();
    for sub in sealed_subclasses(e, sealed) {
        let t = class_type(&sub);
        let closure: Vec<String> = e.supertype_closure(&t).iter().filter_map(fq_of).map(str::to_string).collect();
        if covered.types.iter().any(|c| closure.contains(c)) {
            continue;
        }
        if sub.modifiers.has(Modifiers::SEALED) {
            out.extend(missing_subclasses(e, &sub, covered, depth + 1)?);
            continue;
        }
        let short = short_class_name(&sub.fq_name);
        out.push(if sub.kind == SymbolKind::Object { short } else { format!("is {short}") });
    }
    Some(out)
}

/// Classes in the sealed class's package (top-level and nested) that extend it directly.
fn sealed_subclasses(e: &Engine, sealed: &Symbol) -> Vec<Symbol> {
    let pkg = {
        let parts: Vec<&str> = sealed.fq_name.split('.').collect();
        let first_upper = parts.iter().position(|p| p.chars().next().is_some_and(char::is_uppercase)).unwrap_or(parts.len());
        parts[..first_upper].join(".")
    };
    let mut queue: Vec<String> = e.res.package_contents(&pkg).0.into_iter().map(|(_, fq)| fq).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    while let Some(fq) = queue.pop() {
        if !seen.insert(fq.clone()) {
            continue;
        }
        let Some(c) = e.classifier(&fq) else { continue };
        let Classifier::Source(sym) = &c else { continue };
        queue.extend(e.all_static_members(&c).0.into_iter().map(|(_, n)| n));
        if sym.supertypes.iter().any(|st| fq_of(&e.resolve_declared(sym, st)) == Some(sealed.fq_name.as_str())) {
            out.push(sym.clone());
        }
    }
    out.sort_by(|a, b| a.fq_name.cmp(&b.fq_name));
    out
}

/// Insert `cond -> TODO()` lines before the closing brace of the `when`.
fn add_branches(cx: &Cx, n: &SyntaxNode, conds: &[String]) -> Option<Fix> {
    let close = n.last_token().filter(|t| t.kind() == SyntaxKind::R_CURLY)?;
    let base = indent_at(cx.text, n.text_range().start());
    let indent = format!("{base}    ");
    let mut start = close.text_range().start();
    let mut prev = close.prev_token();
    while let Some(p) = prev.filter(|p| matches!(p.kind(), SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE)) {
        start = p.text_range().start();
        prev = p.prev_token();
    }
    let lines: Vec<String> = conds.iter().map(|c| format!("{indent}{c} -> TODO()")).collect();
    let title = if conds.len() == 1 && conds[0] == "else" { "Add 'else' branch".to_string() } else { "Add remaining branches".to_string() };
    Some(Fix::replace(title, TextRange::new(start, close.text_range().start()), format!("\n{}\n{base}", lines.join("\n"))))
}
