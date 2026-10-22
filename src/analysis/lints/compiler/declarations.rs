//! Declaration-level compiler checks: modifiers, bodies, initialization, class structure,
//! supertypes, abstract members, `override`, redeclarations.

use std::collections::HashMap;

use super::super::*;
use crate::analysis::diagnostics::{LocalsCache, scope_at};
use crate::analysis::ide::completion::override_stub;
use crate::analysis::resolve::Resolved;
use crate::symbols::{Modifiers, Symbol, SymbolKind, TypeRef};

const VISIBILITY: &[&str] = &["public", "private", "protected", "internal"];
const PRIMITIVES: &[&str] = &["Int", "Long", "Short", "Byte", "Float", "Double", "Boolean", "Char", "UInt", "ULong", "UShort", "UByte"];

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::MODIFIER_LIST => modifiers_of(&n, out),
            SyntaxKind::FUN => function(&n, all_open(cx), out),
            SyntaxKind::PROPERTY => property(cx, &n, out),
            SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL => {
                class_structure(&n, out);
                supertypes(cx, &n, out);
                implementations(cx, &n, out);
            }
            SyntaxKind::CLASS_BODY | SyntaxKind::KOTLIN_FILE => redeclarations(&n, out),
            _ => {}
        }
        if matches!(n.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS) {
            expect_actual(cx, &n, out);
        }
    }
}

/// An all-open compiler plugin (Spring, JPA...) may make annotated classes open.
pub(super) fn all_open(cx: &Cx) -> bool {
    cx.module.is_some_and(|m| m.kotlin.compiler_plugins.iter().any(|p| matches!(p.as_str(), "allopen" | "all-open" | "spring" | "jpa" | "noarg")))
}

fn remove_modifier(t: &SyntaxToken) -> TextRange {
    let end = t.next_token().filter(|n| n.kind() == SyntaxKind::WHITESPACE).map_or(t.text_range().end(), |n| n.text_range().end());
    TextRange::new(t.text_range().start(), end)
}

fn modifiers_of(list: &SyntaxNode, out: &mut Sink) {
    let Some(decl) = list.parent() else { return };
    let ml = ast::ModifierList::cast(list.clone()).unwrap();
    let mods: Vec<SyntaxToken> = ml.modifiers().collect();
    let mut seen: HashMap<String, SyntaxToken> = HashMap::new();
    for m in &mods {
        if seen.contains_key(m.text()) {
            out.report(&REPEATED_MODIFIER, m.text_range(), format!("Repeated '{}'.", m.text()), Some(Fix::delete(format!("Remove '{}'", m.text()), remove_modifier(m))));
            continue;
        }
        seen.insert(m.text().to_string(), m.clone());
    }
    let visibilities: Vec<&SyntaxToken> = mods.iter().filter(|m| VISIBILITY.contains(&m.text())).collect();
    if let [first, rest @ ..] = visibilities.as_slice() {
        for v in rest.iter().filter(|v| v.text() != first.text()) {
            out.report(&INCOMPATIBLE_MODIFIERS, v.text_range(), format!("Modifier '{}' is incompatible with '{}'.", v.text(), first.text()), Some(Fix::delete(format!("Remove '{}'", v.text()), remove_modifier(v))));
        }
    }
    // Reported on whichever of the pair comes second.
    for (a, b) in [("final", "open"), ("final", "abstract"), ("final", "sealed"), ("data", "open"), ("data", "abstract"), ("data", "sealed"), ("data", "inner"), ("private", "abstract")] {
        // `private abstract class` is fine; only members cannot be both.
        if a == "private" && !matches!(decl.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY) {
            continue;
        }
        if let (Some(ta), Some(tb)) = (seen.get(a), seen.get(b)) {
            let (first, second) = if ta.text_range().start() < tb.text_range().start() { (ta, tb) } else { (tb, ta) };
            out.report(
                &INCOMPATIBLE_MODIFIERS,
                second.text_range(),
                format!("Modifier '{}' is incompatible with '{}'.", second.text(), first.text()),
                Some(Fix::delete(format!("Remove '{}'", second.text()), remove_modifier(second))),
            );
        }
    }
    // `const` only applies to vals.
    if let Some(c) = seen.get("const") {
        let what = match decl.kind() {
            SyntaxKind::PROPERTY if ast::token(&decl, SyntaxKind::VAR_KW).is_some() => Some("var"),
            SyntaxKind::PROPERTY if is_local(&decl) => Some("local variable"),
            SyntaxKind::PROPERTY => None,
            SyntaxKind::FUN => Some("function"),
            SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => Some("class"),
            _ => Some("declaration"),
        };
        if let Some(what) = what {
            out.report(&WRONG_MODIFIER_TARGET, c.text_range(), format!("Modifier 'const' is not applicable to '{what}'."), Some(Fix::delete("Remove 'const'", remove_modifier(c))));
        }
    }
    if let Some(l) = seen.get("lateinit")
        && decl.kind() != SyntaxKind::PROPERTY
    {
        out.report(&WRONG_MODIFIER_TARGET, l.text_range(), "Modifier 'lateinit' is only applicable to properties.", None);
    }
}

fn has_body(f: &SyntaxNode) -> bool {
    ast::token(f, SyntaxKind::EQ).is_some() || f.children().any(|c| c.kind() == SyntaxKind::BLOCK)
}

/// Declared inside an `expect` or `external` class (members have no bodies there).
fn in_bodyless_container(n: &SyntaxNode) -> bool {
    n.ancestors().skip(1).any(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION) && (has_modifier(&a, "expect") || has_modifier(&a, "external")))
}

fn function(f: &SyntaxNode, plugins: bool, out: &mut Sink) {
    let Some(name) = name_token(f) else { return };
    let owner = owner(f);
    let abstract_ = has_modifier(f, "abstract");
    if abstract_ && has_body(f) {
        out.report(&ABSTRACT_FUNCTION_WITH_BODY, name.text_range(), format!("Function '{}' with body cannot be abstract.", name.text()), None);
    }
    if !has_body(f) && !abstract_ && !["expect", "external", "actual"].iter().any(|m| has_modifier(f, m)) && !in_bodyless_container(f) {
        match &owner {
            Some(o) if is_interface(o) => {}
            Some(_) => out.report(&NON_ABSTRACT_FUNCTION_WITH_NO_BODY, name.text_range(), format!("Function '{}' without a body must be abstract.", name.text()), None),
            None if f.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::KOTLIN_FILE | SyntaxKind::BLOCK)) => {
                out.report(&NON_MEMBER_FUNCTION_NO_BODY, name.text_range(), format!("Function '{}' must have a body.", name.text()), None);
            }
            None => {}
        }
    }
    if abstract_ && let Some(o) = &owner && concrete_container(o) {
        out.report(&ABSTRACT_FUNCTION_IN_NON_ABSTRACT_CLASS, name.text_range(), format!("Abstract function '{}' in non-abstract class.", name.text()), None);
    }
    open_member(f, owner.as_ref(), plugins, out);
}

/// A class or object that cannot have abstract members.
fn concrete_container(o: &SyntaxNode) -> bool {
    match o.kind() {
        SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL => true,
        SyntaxKind::CLASS => !is_interface(o) && !["abstract", "sealed", "enum", "expect"].iter().any(|m| has_modifier(o, m)),
        _ => false,
    }
}

/// `open` members of final classes and objects.
/// `plugins`: an all-open compiler plugin may make the class open.
fn open_member(decl: &SyntaxNode, owner: Option<&SyntaxNode>, plugins: bool, out: &mut Sink) {
    let (Some(o), Some(open)) = (owner, modifiers(decl).and_then(|m| m.modifiers().find(|t| t.text() == "open"))) else { return };
    // Framework annotations (`@Transactional`) usually come with proxies.
    if has_annotations(decl) {
        return;
    }
    match o.kind() {
        SyntaxKind::OBJECT_DECLARATION => {
            out.report(&NON_FINAL_MEMBER_IN_OBJECT, open.text_range(), "'open' has no effect in an object.", Some(Fix::delete("Remove 'open'", remove_modifier(&open))));
        }
        SyntaxKind::CLASS if !is_interface(o) && !["open", "abstract", "sealed", "enum"].iter().any(|m| has_modifier(o, m)) && !has_annotations(o) && !plugins => {
            out.report(&NON_FINAL_MEMBER_IN_FINAL_CLASS, open.text_range(), "'open' has no effect in a final class.", Some(Fix::delete("Remove 'open'", remove_modifier(&open))));
        }
        _ => {}
    }
}

fn property(cx: &Cx, p: &SyntaxNode, out: &mut Sink) {
    let Some(name) = name_token(p) else { return };
    let decl = ast::PropertyDecl::cast(p.clone()).unwrap();
    let owner = owner(p);
    let init = decl.has_initializer();
    let accessors: Vec<_> = decl.accessors().collect();
    let getter_body = accessors.iter().any(|a| a.syntax().first_token().is_some_and(|t| t.text() == "get") && (ast::token(a.syntax(), SyntaxKind::EQ).is_some() || a.syntax().children().any(|c| c.kind() == SyntaxKind::BLOCK)));
    let n = name.text();
    // lateinit
    if let Some(l) = modifiers(p).and_then(|m| m.modifiers().find(|t| t.text() == "lateinit")) {
        let ty = decl.type_ref();
        let ty_text = ty.as_ref().map(|t| t.syntax().text().to_string()).unwrap_or_default();
        let problem = if !decl.is_var() {
            Some("'lateinit' is allowed only on mutable properties")
        } else if init {
            Some("'lateinit' is not allowed on properties with an initializer")
        } else if decl.has_delegate() {
            Some("'lateinit' is not allowed on delegated properties")
        } else if accessors.iter().any(|a| ast::token(a.syntax(), SyntaxKind::EQ).is_some() || a.syntax().children().any(|c| c.kind() == SyntaxKind::BLOCK)) {
            // `private set` alone is fine.
            Some("'lateinit' is not allowed on properties with a custom getter or setter")
        } else if ty.as_ref().is_some_and(|t| t.inner().is_some_and(|i| i.kind() == SyntaxKind::NULLABLE_TYPE)) {
            Some("'lateinit' is not allowed on properties of nullable types")
        } else if PRIMITIVES.contains(&ty_text.as_str()) {
            Some("'lateinit' is not allowed on properties of primitive types")
        } else if has_modifier(p, "abstract") {
            Some("'lateinit' is not allowed on abstract properties")
        } else {
            None
        };
        if let Some(msg) = problem {
            out.report(&INAPPLICABLE_LATEINIT_MODIFIER, l.text_range(), format!("{msg}."), Some(Fix::delete("Remove 'lateinit'", remove_modifier(&l))));
        }
    }
    // const
    if has_modifier(p, "const") && !decl.is_var() && !is_local(p) {
        let in_object_or_top = p.parent().is_some_and(|x| x.kind() == SyntaxKind::KOTLIN_FILE) || owner.as_ref().is_some_and(|o| o.kind() == SyntaxKind::OBJECT_DECLARATION);
        if !in_object_or_top {
            out.report(&CONST_VAL_NOT_TOP_LEVEL_OR_OBJECT, name.text_range(), "'const val' is only allowed at top level or in objects.", None);
        } else if !init && !decl.has_delegate() && !has_modifier(p, "expect") {
            out.report(&CONST_VAL_WITHOUT_INITIALIZER, name.text_range(), "'const val' must have an initializer.", None);
        } else if getter_body {
            out.report(&CONST_VAL_WITH_GETTER, name.text_range(), "'const val' cannot have a getter.", None);
        }
    }
    let abstract_ = has_modifier(p, "abstract");
    if abstract_ && init {
        out.report(&ABSTRACT_PROPERTY_WITH_INITIALIZER, name.text_range(), format!("Abstract property '{n}' cannot have an initializer."), None);
    }
    if abstract_ && getter_body {
        out.report(&ABSTRACT_PROPERTY_WITH_GETTER, name.text_range(), format!("Abstract property '{n}' cannot have a getter body."), None);
    }
    if abstract_ && let Some(o) = &owner && concrete_container(o) {
        out.report(&ABSTRACT_PROPERTY_IN_NON_ABSTRACT_CLASS, name.text_range(), format!("Abstract property '{n}' in non-abstract class."), None);
    }
    if let Some(o) = &owner
        && is_interface(o)
        && init
    {
        out.report(&PROPERTY_INITIALIZER_IN_INTERFACE, name.text_range(), "Property initializers are not allowed in interfaces.", None);
    }
    open_member(p, owner.as_ref(), all_open(cx), out);
    // Initialization of non-local properties.
    let exempt = is_local(p)
        || init
        || decl.has_delegate()
        || !accessors.is_empty()
        || decl.receiver().is_some()
        || decl.is_destructuring()
        || ["abstract", "lateinit", "expect", "external", "const"].iter().any(|m| has_modifier(p, m))
        || in_bodyless_container(p)
        || owner.as_ref().is_some_and(is_interface);
    if exempt {
        return;
    }
    match &owner {
        None if p.parent().is_some_and(|x| x.kind() == SyntaxKind::KOTLIN_FILE) => {
            out.report(&MUST_BE_INITIALIZED, name.text_range(), format!("Property '{n}' must be initialized."), None);
        }
        Some(o) => {
            // Initialized in an `init` block, or by secondary constructors.
            let body = p.parent().unwrap();
            let assigned_in_init = body
                .children()
                .filter(|c| c.kind() == SyntaxKind::CLASS_INITIALIZER || c.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR)
                .any(|c| idents(&c).any(|t| ast::ident_text(&t) == ast::ident_text(&name)));
            let secondary = body.children().any(|c| c.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR);
            if !assigned_in_init && !(secondary && o.kind() == SyntaxKind::CLASS) {
                out.report(&MUST_BE_INITIALIZED_OR_BE_ABSTRACT, name.text_range(), format!("Property '{n}' must be initialized or be abstract."), None);
            }
        }
        None => {}
    }
}

fn class_structure(c: &SyntaxNode, out: &mut Sink) {
    let name = name_token(c);
    let at = name.as_ref().map(|t| t.text_range()).unwrap_or_else(|| c.first_token().map(|t| t.text_range()).unwrap_or(c.text_range()));
    let body_members: Vec<SyntaxNode> = c.children().filter(|x| x.kind() == SyntaxKind::CLASS_BODY).flat_map(|b| b.children().collect::<Vec<_>>()).collect();
    let primary = c.children().find(|x| x.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR);
    let interface = is_interface(c);
    let object = matches!(c.kind(), SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL);
    if interface || object {
        let rule = if interface { &CONSTRUCTOR_IN_INTERFACE } else { &CONSTRUCTOR_IN_OBJECT };
        let what = if interface { "An interface" } else { "An object" };
        for ctor in primary.iter().chain(body_members.iter().filter(|m| m.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR)) {
            out.report(rule, ctor.text_range(), format!("{what} cannot have constructors."), None);
        }
    }
    if interface {
        for init in body_members.iter().filter(|m| m.kind() == SyntaxKind::CLASS_INITIALIZER) {
            out.report(&ANONYMOUS_INITIALIZER_IN_INTERFACE, init.first_token().map(|t| t.text_range()).unwrap_or(init.text_range()), "Interfaces cannot have initializer blocks.", None);
        }
        for call in c.children().filter(|x| x.kind() == SyntaxKind::SUPER_TYPE_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).filter(|e| e.kind() == SyntaxKind::SUPER_TYPE_CALL_ENTRY) {
            out.report(&SUPERTYPE_INITIALIZED_IN_INTERFACE, call.text_range(), "Interfaces cannot initialize supertypes.", None);
        }
    }
    if c.kind() == SyntaxKind::CLASS && has_modifier(c, "data") {
        let params: Vec<SyntaxNode> = primary
            .iter()
            .flat_map(|p| p.children().filter(|l| l.kind() == SyntaxKind::VALUE_PARAMETER_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).collect::<Vec<_>>())
            .filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER)
            .collect();
        if params.is_empty() {
            out.report(&DATA_CLASS_WITHOUT_PARAMETERS, at, "Data class must have at least one primary constructor parameter.", None);
        }
        for p in params {
            let vp = ast::ValueParameter::cast(p.clone()).unwrap();
            if vp.property_keyword().is_none()
                && let Some(t) = vp.name_token()
            {
                out.report(
                    &DATA_CLASS_NOT_PROPERTY_PARAMETER,
                    t.text_range(),
                    "Data class primary constructor must only have property ('val' / 'var') parameters.",
                    Some(Fix::replace("Add 'val'", TextRange::empty(t.text_range().start()), "val ")),
                );
            }
        }
    }
}

/// Supertype list rules (needs resolution).
fn supertypes(cx: &Cx, c: &SyntaxNode, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    let entries: Vec<SyntaxNode> = c.children().filter(|x| x.kind() == SyntaxKind::SUPER_TYPE_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).collect();
    if entries.is_empty() || has_modifier(c, "expect") {
        return;
    }
    let package = e.package();
    let scope = scope_at(c, &package, &mut LocalsCache::default());
    let interface = is_interface(c);
    let secondary = c.children().filter(|x| x.kind() == SyntaxKind::CLASS_BODY).any(|b| b.children().any(|m| m.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR));
    let primary = c.children().any(|x| x.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR);
    let plugins = all_open(cx);
    let mut classes = 0;
    for entry in entries {
        let Some(ut) = entry.descendants().find_map(ast::UserType::cast) else { continue };
        let names: Vec<String> = ut.segments().iter().filter_map(|s| s.name_token()).map(|t| ast::ident_text(&t)).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let Ok(Resolved::Classifier(cls)) = e.res.resolve_type_name(&refs, &scope) else { continue };
        let sym = cls.symbol();
        if sym.fq_name == "kotlin.Any" {
            continue;
        }
        let range = ut.syntax().text_range();
        match sym.kind {
            SymbolKind::Class => {
                classes += 1;
                if interface {
                    out.report(&INTERFACE_WITH_SUPERCLASS, range, "An interface cannot inherit from a class.", None);
                    continue;
                }
                if classes > 1 {
                    out.report(&MANY_CLASSES_IN_SUPERTYPE_LIST, range, "Only one class may appear in a supertype list.", None);
                }
                let open = sym.modifiers.has(Modifiers::OPEN) || sym.modifiers.has(Modifiers::ABSTRACT) || sym.modifiers.has(Modifiers::SEALED);
                let source = matches!(sym.origin, crate::symbols::Origin::Source { .. });
                if !open && !(source && plugins) && entry.kind() != SyntaxKind::DELEGATED_SUPER_TYPE_ENTRY {
                    out.report(&FINAL_SUPERTYPE, range, format!("This type is final, so it cannot be extended: '{}'.", sym.name), None);
                }
                // `: Base` without `()` needs a constructor call when there is a primary
                // constructor (or none at all and no secondary ones).
                if entry.kind() == SyntaxKind::SUPER_TYPE_ENTRY && (primary || !secondary || c.kind() != SyntaxKind::CLASS) {
                    out.report(
                        &SUPERTYPE_NOT_INITIALIZED,
                        range,
                        "This type has a constructor, so it must be initialized here.",
                        Some(Fix::replace("Add constructor call", TextRange::empty(range.end()), "()")),
                    );
                }
            }
            SymbolKind::Object | SymbolKind::Enum => {
                out.report(&FINAL_SUPERTYPE, range, format!("This type is final, so it cannot be extended: '{}'.", sym.name), None);
            }
            _ => {}
        }
    }
}

/// Abstract members a concrete class must implement, and `override`s of nothing.
fn implementations(cx: &Cx, c: &SyntaxNode, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    if has_modifier(c, "expect") || has_modifier(c, "external") {
        return;
    }
    let t = if c.kind() == SyntaxKind::OBJECT_LITERAL {
        e.anon_type(c)
    } else {
        let Some(name) = name_token(c) else { return };
        // The class's own type: its FQ name from the file summary.
        let Some(sym) = class_symbol(cx, &name) else { return };
        crate::analysis::ide::infer::class_type(&sym)
    };
    let body = c.children().find(|x| x.kind() == SyntaxKind::CLASS_BODY);
    let delegates = c.descendants().any(|d| d.kind() == SyntaxKind::DELEGATED_SUPER_TYPE_ENTRY);
    let concrete = !is_interface(c) && !["abstract", "sealed", "enum"].iter().any(|m| has_modifier(c, m));
    if concrete
        && !delegates
        && cx.on(&ABSTRACT_MEMBER_NOT_IMPLEMENTED)
        && let Some(missing) = e.unimplemented_members(&t)
        && let Some(first) = missing.first()
    {
        let at = name_token(c).map(|t| t.text_range()).unwrap_or_else(|| c.first_token().map(|t| t.text_range()).unwrap_or(c.text_range()));
        let what = match c.kind() {
            SyntaxKind::OBJECT_DECLARATION => format!("Object '{}'", name_token(c).map(|t| t.text().to_string()).unwrap_or_default()),
            SyntaxKind::OBJECT_LITERAL => "Object".to_string(),
            _ => format!("Class '{}'", name_token(c).map(|t| t.text().to_string()).unwrap_or_default()),
        };
        let names: Vec<String> = missing.iter().map(|m| format!("'{}'", m.symbol.name)).collect();
        let rule = if first.from_interface { &ABSTRACT_MEMBER_NOT_IMPLEMENTED } else { &ABSTRACT_CLASS_MEMBER_NOT_IMPLEMENTED };
        let indent = format!("{}    ", indent_at(cx.text, c.text_range().start()));
        let stubs: Vec<String> = missing.iter().map(|m| format!("{indent}{}", override_stub(e, &m.symbol, &m.subst, &indent))).collect();
        let fix = match &body {
            // Before the closing `}`, replacing the whitespace in front of it.
            Some(b) => b.last_token().filter(|t| t.kind() == SyntaxKind::R_CURLY).map(|close| {
                let base = indent_at(cx.text, c.text_range().start());
                let mut start = close.text_range().start();
                let mut prev = close.prev_token();
                while let Some(p) = prev.filter(|p| matches!(p.kind(), SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE)) {
                    start = p.text_range().start();
                    prev = p.prev_token();
                }
                Fix::replace("Implement members", TextRange::new(start, close.text_range().start()), format!("\n{}\n{base}", stubs.join("\n\n")))
            }),
            None => Some(Fix::replace("Implement members", TextRange::empty(c.text_range().end()), format!(" {{\n{}\n{}}}", stubs.join("\n\n"), indent_at(cx.text, c.text_range().start())))),
        };
        out.report(rule, at, format!("{what} is not abstract and does not implement abstract members: {}.", names.join(", ")), fix);
    }
    hidden_and_final(cx, e, c, &t, body.as_ref(), out);
    // `override` of nothing.
    let Some(body) = body else { return };
    let members: Vec<SyntaxNode> = body.children().filter(|m| matches!(m.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY) && has_modifier(m, "override")).collect();
    let ctor_overrides: Vec<SyntaxNode> = c
        .children()
        .filter(|x| x.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR)
        .flat_map(|p| p.descendants().filter(|v| v.kind() == SyntaxKind::VALUE_PARAMETER && has_modifier(v, "override")).collect::<Vec<_>>())
        .collect();
    if members.is_empty() && ctor_overrides.is_empty() || !cx.on(&NOTHING_TO_OVERRIDE) {
        return;
    }
    let Some(names) = e.supertype_member_names(&t) else { return };
    for m in members.iter().chain(ctor_overrides.iter()) {
        let Some(name) = name_token(m) else { continue };
        let n = ast::ident_text(&name);
        if names.contains(&n) || matches!(n.as_str(), "equals" | "hashCode" | "toString") {
            continue;
        }
        let fix = modifiers(m).and_then(|l| l.modifiers().find(|t| t.text() == "override")).map(|t| Fix::delete("Remove 'override'", remove_modifier(&t)));
        out.report(&NOTHING_TO_OVERRIDE, name.text_range(), format!("'{n}' overrides nothing."), fix);
    }
}

/// `expect` declarations without an `actual`, and `actual` ones without an `expect`.
fn expect_actual(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    use crate::analysis::ide::hierarchy::counterparts;
    let (expect, actual) = (has_modifier(n, "expect"), has_modifier(n, "actual"));
    if !expect && !actual {
        return;
    }
    let Some(name) = name_token(n) else { return };
    let Some(sym) = decl_symbol(cx, &name, |_| true) else { return };
    let what = match n.kind() {
        SyntaxKind::FUN => "function",
        SyntaxKind::PROPERTY => "property",
        SyntaxKind::TYPEALIAS => "type alias",
        _ => "class",
    };
    let shown = ast::ident_text(&name);
    let top_level = n.parent().is_some_and(|p| p.kind() == SyntaxKind::KOTLIN_FILE);
    if expect && top_level && cx.on(&NO_ACTUAL_FOR_EXPECT) && counterparts(cx.snap, &sym, Modifiers::ACTUAL).is_empty() {
        out.report(&NO_ACTUAL_FOR_EXPECT, name.text_range(), format!("Expected {what} '{shown}' has no actual declaration in any module."), None);
    }
    if actual && cx.on(&ACTUAL_WITHOUT_EXPECT) && counterparts(cx.snap, &sym, Modifiers::EXPECT).is_empty() {
        let fix = modifiers(n).and_then(|l| l.modifiers().find(|t| t.text() == "actual")).map(|t| Fix::delete("Remove 'actual'", remove_modifier(&t)));
        out.report(&ACTUAL_WITHOUT_EXPECT, name.text_range(), format!("Actual {what} '{shown}' has no corresponding expected declaration."), fix);
    }
}

/// Members declared without `override` that match a supertype member (they need it), and
/// `override`s of final members.
fn hidden_and_final(cx: &Cx, e: &Engine, c: &SyntaxNode, t: &TypeRef, body: Option<&SyntaxNode>, out: &mut Sink) {
    if c.kind() == SyntaxKind::OBJECT_LITERAL {
        return;
    }
    let ctor_params = c.children().filter(|x| x.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR).flat_map(|p| p.descendants()).filter(|v| v.kind() == SyntaxKind::VALUE_PARAMETER && v.children_with_tokens().any(|t| matches!(t.kind(), SyntaxKind::VAL_KW | SyntaxKind::VAR_KW)));
    let members = body.into_iter().flat_map(|b| b.children()).filter(|m| matches!(m.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY));
    for m in ctor_params.chain(members) {
        let Some(name) = name_token(&m) else { continue };
        let Some(sym) = decl_symbol(cx, &name, |s| matches!(s.kind, SymbolKind::Function | SymbolKind::Property)) else { continue };
        let Some((base, class, subst)) = e.overridden_member(t, &sym) else { continue };
        let class_name = class.rsplit('.').next().unwrap_or(&class);
        let n = ast::ident_text(&name);
        let interface = e.classifier(&class).is_some_and(|c| c.symbol().kind == SymbolKind::Interface);
        let open = interface || base.modifiers.has(Modifiers::OPEN) || base.modifiers.has(Modifiers::ABSTRACT) || base.modifiers.has(Modifiers::OVERRIDE) && !base.modifiers.has(Modifiers::FINAL);
        let add_override = Edit { range: TextRange::empty(override_position(&m)), text: "override ".into() };
        let make_open = |also: Vec<Edit>| open_fix(cx, &base, &format!("{class_name}.{n}"), also);
        if has_modifier(&m, "override") {
            if !open {
                out.report(&OVERRIDING_FINAL_MEMBER, name.text_range(), format!("'{n}' in '{class_name}' is final and cannot be overridden."), make_open(Vec::new()));
            }
            override_signature(e, &m, &name, &sym, &base, &subst, class_name, out);
        } else if open {
            let fix = Fix::new("Add 'override' modifier", vec![add_override]);
            out.report(&VIRTUAL_MEMBER_HIDDEN, name.text_range(), format!("'{n}' hides member of supertype '{class_name}' and needs an 'override' modifier."), Some(fix));
        } else {
            out.report(&VIRTUAL_MEMBER_HIDDEN, name.text_range(), format!("'{n}' hides final member of supertype '{class_name}'."), make_open(vec![add_override]));
        }
    }
}

/// An `override` that changes what it overrides: `val` for a `var`, less visibility, or a
/// type that does not fit.
#[allow(clippy::too_many_arguments)]
fn override_signature(e: &Engine, m: &SyntaxNode, name: &SyntaxToken, sym: &Symbol, base: &Symbol, subst: &crate::analysis::ide::infer::Subst, class_name: &str, out: &mut Sink) {
    use crate::symbols::Visibility;
    let n = ast::ident_text(name);
    let var = m.children_with_tokens().any(|t| t.kind() == SyntaxKind::VAR_KW);
    if sym.kind == SymbolKind::Property && base.modifiers.has(Modifiers::VAR) && !var {
        let fix = ast::token(m, SyntaxKind::VAL_KW).map(|v| Fix::replace("Change to 'var'", v.text_range(), "var"));
        out.report(&VAR_OVERRIDDEN_BY_VAL, name.text_range(), format!("Val-property cannot override var-property '{class_name}.{n}'."), fix);
    }
    // Visibility: only an explicit modifier can be weaker.
    if let Some(v) = modifiers(m).and_then(|l| l.modifiers().find(|t| VISIBILITY.contains(&t.text()))) {
        let weaker = match base.visibility {
            Visibility::Public => v.text() != "public",
            Visibility::Protected => matches!(v.text(), "private" | "internal"),
            Visibility::Internal => matches!(v.text(), "private" | "protected"),
            _ => false,
        };
        if weaker {
            let shown = match base.visibility {
                Visibility::Public => "public",
                Visibility::Protected => "protected",
                _ => "internal",
            };
            out.report(&CANNOT_WEAKEN_ACCESS_PRIVILEGE, v.text_range(), format!("Cannot weaken access privilege '{shown}' for '{n}' in '{class_name}'."), Some(Fix::replace(format!("Make '{n}' {shown}"), v.text_range(), shown)));
        }
    }
    // Types.
    let written = match m.kind() {
        SyntaxKind::FUN => ast::FunDecl::cast(m.clone()).and_then(|f| f.return_type()),
        _ => ast::child::<ast::TypeRef>(m),
    };
    let ours = match written {
        Some(t) => e.resolve_syntax_type(&t),
        // A block-bodied function without a result type returns Unit.
        None if m.kind() == SyntaxKind::FUN && ast::FunDecl::cast(m.clone()).is_some_and(|f| !f.has_expression_body()) && m.children().any(|c| c.kind() == SyntaxKind::BLOCK) => TypeRef::named("kotlin.Unit"),
        None => return,
    };
    let Some(theirs) = base.ty.as_ref().map(|t| crate::analysis::ide::infer::substitute(&e.resolve_declared(base, t), subst)) else { return };
    let (TypeRef::Named { fq: Some(ofq), nullable: on, .. }, TypeRef::Named { fq: Some(tfq), nullable: tn, .. }) = (&ours, &theirs) else { return };
    if [ofq, tfq].iter().any(|f| e.classifier(f).is_none_or(|c| c.symbol().kind == SymbolKind::TypeAlias)) {
        return;
    }
    let fits = |a: &str, an: bool, b: &str, bn: bool| -> Option<bool> { Some((!an || bn) && e.conforms(&TypeRef::named(a), b)?) };
    let ok = match (sym.kind, var) {
        (SymbolKind::Property, true) => fits(ofq, *on, tfq, *tn).zip(fits(tfq, *tn, ofq, *on)).map(|(a, b)| a && b),
        _ => fits(ofq, *on, tfq, *tn),
    };
    if ok != Some(false) {
        return;
    }
    let at = written_range(m).unwrap_or(name.text_range());
    let (rule, what) = match (sym.kind, var) {
        (SymbolKind::Property, true) => (&VAR_TYPE_MISMATCH_ON_OVERRIDE, "Type of"),
        (SymbolKind::Property, false) => (&PROPERTY_TYPE_MISMATCH_ON_OVERRIDE, "Type of"),
        _ => (&RETURN_TYPE_MISMATCH_ON_OVERRIDE, "Return type of"),
    };
    out.report(rule, at, format!("{what} '{n}' is not a subtype of the overridden member '{class_name}.{n}' of type '{}'.", theirs.display()), None);
}

/// The written type (return type, property type) of a declaration.
fn written_range(m: &SyntaxNode) -> Option<TextRange> {
    match m.kind() {
        SyntaxKind::FUN => ast::FunDecl::cast(m.clone()).and_then(|f| f.return_type()).map(|t| t.syntax().text_range()),
        _ => ast::child::<ast::TypeRef>(m).map(|t| t.syntax().text_range()),
    }
}

/// "Make 'Base.name' open" for a final member declared in the project (plus `also` here).
fn open_fix(cx: &Cx, base: &crate::symbols::Symbol, shown: &str, also: Vec<Edit>) -> Option<Fix> {
    let crate::symbols::Origin::Source { file, name_range, .. } = &base.origin else { return None };
    let root = cx.snap.file(file)?.parse().syntax();
    let decl = crate::syntax::covering(&root, *name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::VALUE_PARAMETER))?;
    // The class must be open too (else the class itself cannot be extended).
    let class = decl.ancestors().find(|a| a.kind() == SyntaxKind::CLASS)?;
    if !["open", "abstract", "sealed"].iter().any(|m| has_modifier(&class, m)) && !is_interface(&class) {
        return None;
    }
    let edit = match modifiers(&decl).and_then(|l| l.modifiers().find(|t| t.text() == "final")) {
        // `final override`: dropping `final` leaves it open.
        Some(f) => Edit { range: remove_modifier(&f), text: String::new() },
        None => Edit { range: TextRange::empty(override_position(&decl)), text: "open ".into() },
    };
    let mut fix = Fix::new(format!("Make '{shown}' open"), also);
    if file.as_path() == cx.path {
        fix.edits.push(edit);
    } else {
        fix.elsewhere.push((file.to_path_buf(), edit));
    }
    Some(fix)
}

/// Where `override` goes: after annotations and visibility/modality modifiers, before the
/// others (`override suspend fun`) and the keyword.
fn override_position(m: &SyntaxNode) -> TextSize {
    let keyword = m
        .children_with_tokens()
        .filter_map(|t| t.into_token())
        .find(|t| matches!(t.kind(), SyntaxKind::FUN_KW | SyntaxKind::VAL_KW | SyntaxKind::VAR_KW))
        .map_or(m.text_range().start(), |t| t.text_range().start());
    let Some(list) = modifiers(m) else { return keyword };
    list.modifiers()
        .find(|t| !VISIBILITY.contains(&t.text()) && !matches!(t.text(), "open" | "abstract" | "final" | "expect" | "actual"))
        .map_or(keyword, |t| t.text_range().start())
}

/// The indexed symbol of the class whose name token is `name`.
fn class_symbol(cx: &Cx, name: &SyntaxToken) -> Option<crate::symbols::Symbol> {
    decl_symbol(cx, name, |s| s.kind.is_classifier())
}

/// The indexed symbol of the declaration whose name token is `name`.
fn decl_symbol(cx: &Cx, name: &SyntaxToken, wanted: impl Fn(&crate::symbols::Symbol) -> bool + Copy) -> Option<crate::symbols::Symbol> {
    let summary = cx.snap.file(cx.path)?.summary();
    fn walk(ds: &[crate::analysis::source::SourceDecl], r: TextRange, wanted: impl Fn(&crate::symbols::Symbol) -> bool + Copy) -> Option<&crate::symbols::Symbol> {
        for d in ds {
            if let crate::symbols::Origin::Source { name_range, .. } = &d.symbol.origin
                && *name_range == r
                && wanted(&d.symbol)
            {
                return Some(&d.symbol);
            }
            if let Some(s) = walk(&d.children, r, wanted) {
                return Some(s);
            }
        }
        None
    }
    walk(&summary.decls, name.text_range(), wanted).cloned()
}

/// Two properties or classes with one name, or two functions with one signature, in the
/// same class body or file.
fn redeclarations(container: &SyntaxNode, out: &mut Sink) {
    let mut props: HashMap<String, SyntaxToken> = HashMap::new();
    let mut classes: HashMap<String, SyntaxToken> = HashMap::new();
    let mut funs: HashMap<(String, String), SyntaxToken> = HashMap::new();
    let top = container.kind() == SyntaxKind::KOTLIN_FILE;
    for d in container.children() {
        if ["expect", "actual"].iter().any(|m| has_modifier(&d, m)) {
            continue;
        }
        let Some(name) = name_token(&d) else { continue };
        let n = ast::ident_text(&name);
        match d.kind() {
            SyntaxKind::PROPERTY => {
                let decl = ast::PropertyDecl::cast(d.clone()).unwrap();
                let key = format!("{}{n}", decl.receiver().map(|r| norm(&r.syntax().text().to_string()) + ".").unwrap_or_default());
                if let Some(prev) = props.get(&key) {
                    for t in [prev, &name] {
                        out.report(&REDECLARATION_LOCAL, t.text_range(), format!("Conflicting declarations: property '{n}'."), None);
                    }
                } else {
                    props.insert(key, name);
                }
            }
            // Top-level classes are checked across the module elsewhere.
            SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS if !top => {
                if let Some(prev) = classes.get(&n) {
                    for t in [prev, &name] {
                        out.report(&REDECLARATION_LOCAL, t.text_range(), format!("Redeclaration: '{n}'."), None);
                    }
                } else {
                    classes.insert(n, name);
                }
            }
            SyntaxKind::FUN => {
                let f = ast::FunDecl::cast(d.clone()).unwrap();
                let params: Vec<String> =
                    f.parameters().iter().map(|p| format!("{}{}", if p.is_vararg() { "vararg " } else { "" }, p.type_ref().map(|t| norm(&t.syntax().text().to_string())).unwrap_or_default())).collect();
                let receiver = f.receiver().map(|r| norm(&r.syntax().text().to_string())).unwrap_or_default();
                // Type parameters with their bounds are part of the signature.
                let tps = d.children().filter(|c| matches!(c.kind(), SyntaxKind::TYPE_PARAMETER_LIST | SyntaxKind::TYPE_CONSTRAINT_LIST)).map(|c| norm(&c.text().to_string())).collect::<String>();
                let sig = format!("{tps}{receiver}.{n}({})", params.join(","));
                if let Some(prev) = funs.get(&(n.clone(), sig.clone())) {
                    for t in [prev, &name] {
                        out.report(&CONFLICTING_OVERLOADS, t.text_range(), format!("Conflicting overloads: '{n}({})'.", params.join(", ")), None);
                    }
                } else {
                    funs.insert((n, sig), name);
                }
            }
            _ => {}
        }
    }
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}
