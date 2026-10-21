//! Declaration signatures: `vararg` and `reified` misuse, defaults in overrides, `T::class`
//! of a non-reified type parameter, `inline` without lambdas, `??`, and declarations that
//! expose types less visible than themselves.

use super::super::*;
use crate::analysis::ide::infer::{Engine, strip};
use crate::symbols::{Origin, SymbolKind, TypeArg, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::VALUE_PARAMETER_LIST => parameters(&n, out),
            SyntaxKind::TYPE_PARAMETER => reified(&n, out),
            SyntaxKind::CLASS_LITERAL => class_literal(&n, out),
            SyntaxKind::NULLABLE_TYPE => redundant_nullable(&n, out),
            SyntaxKind::FUN if has_modifier(&n, "inline") => nothing_to_inline(cx, &n, out),
            _ => {}
        }
    }
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants() {
        if matches!(n.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::SECONDARY_CONSTRUCTOR) && !in_error(&n) {
            exposure(cx, e, &n, out);
        }
    }
}

/// More than one `vararg`, and default values in an `override fun`.
fn parameters(list: &SyntaxNode, out: &mut Sink) {
    let params: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER).collect();
    for p in params.iter().filter(|p| has_modifier(p, "vararg")).skip(1) {
        if let Some(m) = modifiers(p).and_then(|l| l.modifiers().find(|t| t.text() == "vararg")) {
            out.report(&MULTIPLE_VARARG_PARAMETERS, m.text_range(), "Multiple vararg parameters are prohibited.", None);
        }
    }
    if !list.parent().is_some_and(|f| f.kind() == SyntaxKind::FUN && has_modifier(&f, "override")) {
        return;
    }
    for p in &params {
        let (Some(eq), Some(value)) = (ast::token(p, SyntaxKind::EQ), initializer(p)) else {
            continue;
        };
        let start = eq.prev_token().filter(|t| t.kind() == SyntaxKind::WHITESPACE).map_or(eq.text_range().start(), |t| t.text_range().start());
        let fix = Fix::delete("Remove default value", TextRange::new(start, value.text_range().end()));
        out.report(&DEFAULT_VALUE_NOT_ALLOWED_IN_OVERRIDE, value.text_range(), "An overriding function is not allowed to specify default values for its parameters.", Some(fix));
    }
}

/// `reified` on a type parameter of something other than an inline function (or an inline
/// property).
fn reified(tp: &SyntaxNode, out: &mut Sink) {
    let Some(m) = modifiers(tp).and_then(|l| l.modifiers().find(|t| t.text() == "reified")) else {
        return;
    };
    let Some(owner) = tp.parent().and_then(|l| l.parent()) else {
        return;
    };
    if matches!(owner.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY) && has_modifier(&owner, "inline") {
        return;
    }
    // A property is inline through its accessors.
    if owner.kind() == SyntaxKind::PROPERTY && owner.children().any(|a| a.kind() == SyntaxKind::PROPERTY_ACCESSOR && has_modifier(&a, "inline")) {
        return;
    }
    let fix = (owner.kind() == SyntaxKind::FUN).then(|| ast::token(&owner, SyntaxKind::FUN_KW)).flatten().map(|kw| Fix::replace("Add 'inline' modifier", TextRange::empty(kw.text_range().start()), "inline "));
    out.report(&REIFIED_TYPE_PARAMETER_NO_INLINE, m.text_range(), "Only type parameters of inline functions can be reified.", fix);
}

/// The type parameter named `name` declared by the nearest declaration that has one.
fn type_parameter(at: &SyntaxNode, name: &str) -> Option<SyntaxNode> {
    for a in at.ancestors() {
        if !matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::CLASS | SyntaxKind::PROPERTY | SyntaxKind::TYPEALIAS) {
            continue;
        }
        let found = a.children().filter(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST).flat_map(|l| l.children()).find(|tp| name_token(tp).is_some_and(|t| t.text() == name));
        if found.is_some() {
            return found;
        }
        // A class's members do not see an outer class's type parameters (unless inner),
        // and nested classes or local classes with the same name would shadow: stop at
        // the first class.
        if a.kind() == SyntaxKind::CLASS && !has_modifier(&a, "inner") {
            return None;
        }
    }
    None
}

/// `T::class` of a type parameter that is not reified.
fn class_literal(n: &SyntaxNode, out: &mut Sink) {
    let Some(r) = n.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else {
        return;
    };
    let name = r.text().to_string();
    let Some(tp) = type_parameter(n, &name) else {
        return;
    };
    if has_modifier(&tp, "reified") {
        return;
    }
    out.report(&TYPE_PARAMETER_AS_REIFIED, r.text_range(), format!("Cannot use '{name}' as reified type parameter. Use a class instead."), None);
}

/// `T??`.
fn redundant_nullable(n: &SyntaxNode, out: &mut Sink) {
    if !n.first_child().is_some_and(|c| c.kind() == SyntaxKind::NULLABLE_TYPE) {
        return;
    }
    let Some(q) = n.last_token().filter(|t| t.kind() == SyntaxKind::QUESTION) else {
        return;
    };
    out.report(&REDUNDANT_NULLABLE, q.text_range(), "Redundant '?'.", Some(Fix::delete("Remove redundant '?'", q.text_range())));
}

/// Whether a type written in source may be a function type (aliases and type parameters
/// may be).
fn may_be_function(cx: &Cx, t: &ast::TypeRef) -> bool {
    let s = t.syntax();
    if s.descendants().any(|d| d.kind() == SyntaxKind::FUNCTION_TYPE) {
        return true;
    }
    let Some(e) = cx.engine() else { return true };
    match strip(&e.resolve_syntax_type(t)) {
        TypeRef::Named { fq: Some(fq), .. } => fq.starts_with("kotlin.Function") || fq.starts_with("kotlin.coroutines.SuspendFunction") || e.classifier(fq).is_none_or(|c| c.symbol().kind == SymbolKind::TypeAlias),
        TypeRef::Named { fq: None, .. } | TypeRef::Error | TypeRef::Dynamic => true,
        _ => false,
    }
}

/// An `inline` function without parameters (or receiver) of function type and without
/// reified type parameters: inlining gains nothing.
fn nothing_to_inline(cx: &Cx, f: &SyntaxNode, out: &mut Sink) {
    if ["expect", "actual", "override", "external"].iter().any(|m| has_modifier(f, m)) || is_local(f) {
        return;
    }
    let reified = f.children().filter(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST).flat_map(|l| l.children()).any(|tp| has_modifier(&tp, "reified"));
    if reified {
        return;
    }
    let receiver = f.children().find(|c| c.kind() == SyntaxKind::RECEIVER_TYPE).and_then(|r| r.children().find_map(ast::TypeRef::cast));
    let params = f.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST).flat_map(|l| l.children()).filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER);
    let types: Vec<ast::TypeRef> = receiver.into_iter().chain(params.filter_map(|p| ast::child::<ast::TypeRef>(&p))).collect();
    if types.iter().any(|t| may_be_function(cx, t)) {
        return;
    }
    let Some(m) = modifiers(f).and_then(|l| l.modifiers().find(|t| t.text() == "inline")) else {
        return;
    };
    let fix = Fix::delete("Remove 'inline' modifier", TextRange::new(m.text_range().start(), m.next_token().filter(|t| t.kind() == SyntaxKind::WHITESPACE).map_or(m.text_range().end(), |t| t.text_range().end())));
    out.report(&NOTHING_TO_INLINE, m.text_range(), "Expected performance impact from inlining is insignificant. Inlining works best for functions with parameters of function types.", Some(fix));
}

/// Effective visibility, from most to least visible.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Vis {
    Public,
    Internal,
    Private,
}

impl Vis {
    fn name(self) -> &'static str {
        match self {
            Vis::Public => "public",
            Vis::Internal => "internal",
            Vis::Private => "private",
        }
    }
}

/// The effective visibility of a declaration (`own`: its own modifiers count). `None` for
/// local declarations and anything `protected` (not compared here).
fn effective(d: &SyntaxNode, own: bool) -> Option<Vis> {
    let mut v = Vis::Public;
    for (i, a) in d.ancestors().enumerate() {
        match a.kind() {
            SyntaxKind::CLASS_BODY | SyntaxKind::VALUE_PARAMETER_LIST | SyntaxKind::KOTLIN_FILE => {
                continue;
            }
            // A constructor property's visibility is its own, not the constructor's.
            SyntaxKind::PRIMARY_CONSTRUCTOR if i > 0 => continue,
            SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS | SyntaxKind::PRIMARY_CONSTRUCTOR | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::VALUE_PARAMETER => {
                if i == 0 && !own {
                    continue;
                }
                if has_modifier(&a, "protected") {
                    return None;
                }
                if has_modifier(&a, "private") {
                    v = Vis::Private;
                } else if has_modifier(&a, "internal") {
                    v = v.max(Vis::Internal);
                }
            }
            _ => return None,
        }
    }
    Some(v)
}

/// The least visible source classifier (with its visibility) among the classes of `t`.
fn least_visible(cx: &Cx, e: &Engine, t: &TypeRef) -> Option<(Vis, String)> {
    let mut worst: Option<(Vis, String)> = None;
    let mut visit = |fq: &str| {
        let Some(c) = e.classifier(fq) else { return };
        let sym = c.symbol();
        if sym.kind == SymbolKind::TypeAlias {
            return;
        }
        let Origin::Source { file, name_range, .. } = &sym.origin else {
            return;
        };
        let root = if file.as_path() == cx.path {
            cx.root.clone()
        } else {
            let Some(f) = cx.snap.file(file) else { return };
            f.parse().syntax()
        };
        let decl = crate::syntax::covering(&root, *name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION));
        let Some(v) = decl.and_then(|d| effective(&d, true)) else {
            return;
        };
        if worst.as_ref().is_none_or(|(w, _)| v > *w) {
            worst = Some((v, sym.name.clone()));
        }
    };
    fn walk(t: &TypeRef, visit: &mut impl FnMut(&str)) {
        match strip(t) {
            TypeRef::Named { fq, args, .. } => {
                if let Some(fq) = fq {
                    visit(fq);
                }
                for a in args {
                    if let TypeArg::Type { ty, .. } = a {
                        walk(ty, visit);
                    }
                }
            }
            TypeRef::Function { receiver, params, ret, .. } => {
                for p in receiver.iter().map(|r| r.as_ref()).chain(params.iter()).chain(std::iter::once(ret.as_ref())) {
                    walk(p, visit);
                }
            }
            _ => {}
        }
    }
    walk(t, &mut visit);
    worst
}

/// A declaration whose signature (or supertype list) uses a type that is less visible.
fn exposure(cx: &Cx, e: &Engine, d: &SyntaxNode, out: &mut Sink) {
    if is_local(d) {
        return;
    }
    let mut check = |owner_vis: Option<Vis>, t: &ast::TypeRef, rule: &'static Rule, what: &str, part: &str| {
        let Some(v) = owner_vis else { return };
        if v == Vis::Private {
            return;
        }
        let Some((tv, name)) = least_visible(cx, e, &e.resolve_syntax_type(t)) else {
            return;
        };
        if tv > v {
            out.report(rule, t.syntax().text_range(), format!("'{}' {what} exposes its '{}' {part} '{name}'.", v.name(), tv.name()), None);
        }
    };
    match d.kind() {
        SyntaxKind::FUN | SyntaxKind::SECONDARY_CONSTRUCTOR => {
            let what = if d.kind() == SyntaxKind::FUN { "function" } else { "constructor" };
            let vis = effective(d, true);
            if let Some(ret) = ast::FunDecl::cast(d.clone()).and_then(|f| f.return_type()) {
                check(vis, &ret, &EXPOSED_FUNCTION_RETURN_TYPE, what, "return type");
            }
            if let Some(r) = d.children().find(|c| c.kind() == SyntaxKind::RECEIVER_TYPE).and_then(|r| r.children().find_map(ast::TypeRef::cast)) {
                check(vis, &r, &EXPOSED_RECEIVER_TYPE, what, "receiver type");
            }
            for p in d.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST).flat_map(|l| l.children()) {
                if let Some(t) = ast::child::<ast::TypeRef>(&p) {
                    check(vis, &t, &EXPOSED_PARAMETER_TYPE, what, "parameter type");
                }
            }
        }
        SyntaxKind::PROPERTY => {
            let vis = effective(d, true);
            if let Some(t) = ast::child::<ast::TypeRef>(d) {
                check(vis, &t, &EXPOSED_PROPERTY_TYPE, "property", "type");
            }
            if let Some(r) = d.children().find(|c| c.kind() == SyntaxKind::RECEIVER_TYPE).and_then(|r| r.children().find_map(ast::TypeRef::cast)) {
                check(vis, &r, &EXPOSED_RECEIVER_TYPE, "property", "receiver type");
            }
        }
        _ => {
            let class_vis = effective(d, true);
            // Primary constructor parameters, and the properties they declare.
            if let Some(ctor) = d.children().find(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR) {
                let ctor_vis = effective(&ctor, true);
                for p in ctor.descendants().filter(|v| v.kind() == SyntaxKind::VALUE_PARAMETER && v.parent().and_then(|l| l.parent()).as_ref() == Some(&ctor)) {
                    let Some(t) = ast::child::<ast::TypeRef>(&p) else {
                        continue;
                    };
                    let ty = e.resolve_syntax_type(&t);
                    let worst = least_visible(cx, e, &ty).map(|(v, _)| v);
                    let exposed_by = |v: Option<Vis>| v.zip(worst).is_some_and(|(v, w)| v != Vis::Private && w > v);
                    if exposed_by(ctor_vis) {
                        check(ctor_vis, &t, &EXPOSED_PARAMETER_TYPE, "constructor", "parameter type");
                    } else if p.children_with_tokens().any(|t| matches!(t.kind(), SyntaxKind::VAL_KW | SyntaxKind::VAR_KW)) {
                        check(effective(&p, true), &t, &EXPOSED_PROPERTY_TYPE, "property", "type");
                    }
                }
            }
            let entries = d.children().filter(|x| x.kind() == SyntaxKind::SUPER_TYPE_LIST).flat_map(|l| l.children().collect::<Vec<_>>());
            for entry in entries {
                let Some(t) = entry.descendants().find_map(ast::TypeRef::cast) else {
                    continue;
                };
                let interface = matches!(strip(&e.resolve_syntax_type(&t)), TypeRef::Named { fq: Some(fq), .. } if e.classifier(fq).is_some_and(|c| c.symbol().kind == SymbolKind::Interface));
                // A class may implement a less visible interface; an interface may not
                // extend one.
                match (interface, is_interface(d)) {
                    (true, true) => check(class_vis, &t, &EXPOSED_SUPER_INTERFACE, "sub-interface", "supertype"),
                    (false, _) => check(class_vis, &t, &EXPOSED_SUPER_CLASS, if d.kind() == SyntaxKind::OBJECT_DECLARATION { "object" } else { "subclass" }, "supertype"),
                    _ => {}
                }
            }
        }
    }
}
