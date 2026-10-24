//! What names in expressions refer to: a class without a companion used as a value, a
//! function used without a call, abstract members called through `super`; `val`/`var` on
//! parameters that cannot be properties; `const` initializers that are not constant;
//! destructuring without `componentN` functions.

use super::super::*;
use super::certain::{certain_type, in_unknown_lambda, shown};
use crate::analysis::ide::infer::{Def, Engine};
use crate::symbols::{Modifiers, Origin, SymbolKind};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if !in_error(&n) && n.kind() == SyntaxKind::VALUE_PARAMETER {
            val_on_parameter(&n, out);
        }
    }
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::NAME_REF => value_reference(e, &n, out),
            SyntaxKind::DOT_QUALIFIED_EXPR => abstract_super_call(e, &n, out),
            SyntaxKind::PROPERTY => {
                const_initializer(e, &n, out);
                components(e, &n, out);
            }
            _ => {}
        }
    }
}

/// `fun f(val x: Int)`, `for (val i in xs)`, `catch (val e: E)`.
fn val_on_parameter(p: &SyntaxNode, out: &mut Sink) {
    let Some(kw) = ast::token(p, SyntaxKind::VAL_KW).or_else(|| ast::token(p, SyntaxKind::VAR_KW)) else { return };
    let owner = match p.parent() {
        Some(l) if l.kind() == SyntaxKind::VALUE_PARAMETER_LIST => l.parent(),
        other => other,
    };
    let Some(owner) = owner else { return };
    let (rule, what) = match owner.kind() {
        SyntaxKind::FUN => (&VAL_OR_VAR_ON_FUN_PARAMETER, "function"),
        SyntaxKind::SECONDARY_CONSTRUCTOR => (&VAL_OR_VAR_ON_SECONDARY_CONSTRUCTOR_PARAMETER, "secondary constructor"),
        SyntaxKind::FOR_EXPR => (&VAL_OR_VAR_ON_LOOP_PARAMETER, "loop"),
        SyntaxKind::CATCH_CLAUSE => (&VAL_OR_VAR_ON_CATCH_PARAMETER, "catch"),
        _ => return,
    };
    let end = kw.next_token().filter(|t| t.kind().is_trivia()).map_or(kw.text_range().end(), |t| t.text_range().end());
    out.report(rule, kw.text_range(), format!("'{}' on {what} parameter is not allowed.", kw.text()), Some(Fix::delete(format!("Remove '{}'", kw.text()), TextRange::new(kw.text_range().start(), end))));
}

/// Whether `n` is used as a value (not called, not a receiver, not referenced with `::`).
fn value_position(n: &SyntaxNode) -> Option<bool> {
    let p = n.parent()?;
    Some(match p.kind() {
        SyntaxKind::CALL_EXPR => p.first_child().as_ref() != Some(n),
        SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR => p.first_child().as_ref() != Some(n),
        SyntaxKind::CALLABLE_REFERENCE | SyntaxKind::CLASS_LITERAL | SyntaxKind::LABEL_QUALIFIER | SyntaxKind::USER_TYPE_SEGMENT => false,
        SyntaxKind::BINARY_EXPR => !(assignment_op(&p).is_some() && p.first_child().as_ref() == Some(n)),
        _ => true,
    })
}

/// `val x = Foo` (no companion) and `val f = compute` (a function, not called).
fn value_reference(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    if value_position(n) != Some(true) || n.ancestors().any(|a| matches!(a.kind(), SyntaxKind::IMPORT_DIRECTIVE | SyntaxKind::PACKAGE_DIRECTIVE | SyntaxKind::ANNOTATION)) {
        return;
    }
    let Some(name) = ast::token(n, SyntaxKind::IDENT).map(|t| ast::ident_text(&t)) else { return };
    let defs = e.resolve_name_ref(n);
    if defs.is_empty() || in_unknown_lambda(e, n) {
        return;
    }
    // A bare name of top-level or local functions only, with no member of that name on an
    // implicit receiver (members may be properties the resolver does not prefer).
    let bare = n.parent().is_none_or(|p| !matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR));
    let functions_only = defs.iter().all(|d| match d {
        Def::Local(l) => l.kind == crate::analysis::ide::infer::LocalKind::Function,
        Def::Symbol { sym, .. } => sym.kind == SymbolKind::Function && sym.receiver.is_none() && sym.name == name && sym.fq_name.rsplit_once('.').is_none_or(|(owner, _)| e.res.find_classifier(owner).is_none()),
        Def::Package(_) => false,
    });
    if bare && functions_only && e.implicit_receivers(n).iter().all(|(t, _)| e.members_named(t, &name).is_empty()) {
        let fix = Fix::replace("Add '()'", TextRange::empty(n.text_range().end()), "()");
        out.report(&FUNCTION_CALL_EXPECTED, n.text_range(), format!("Function invocation '{name}()' expected."), Some(fix));
        return;
    }
    let symbols: Vec<_> = defs.iter().filter_map(|d| match d {
        Def::Symbol { sym, .. } => Some(sym),
        _ => None,
    }).collect();
    if symbols.len() != defs.len() {
        return;
    }
    let [class] = symbols.as_slice() else { return };
    if !matches!(class.kind, SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum | SymbolKind::Annotation) {
        return;
    }
    // Kotlin library classes: their companions (possibly named) are not reliably known.
    let java_or_source = match e.res.find_classifier(&class.fq_name) {
        Some(crate::analysis::resolve::Classifier::Library(lc)) => !lc.is_kotlin,
        Some(crate::analysis::resolve::Classifier::Source(_)) => true,
        _ => false,
    };
    if !java_or_source {
        return;
    }
    // A selector `x.Foo` names a nested class (also an error, but another one).
    if n.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR)) {
        return;
    }
    if e.classifier(&format!("{}.Companion", class.fq_name)).is_some() || e.classifier(&class.fq_name).is_some_and(|c| c.symbol().modifiers.has(Modifiers::EXPECT)) {
        return;
    }
    // A companion with another name.
    if let Some(c) = e.classifier(&class.fq_name)
        && e.all_static_members(&c).0.iter().any(|(_, fq)| e.classifier(fq).is_some_and(|k| k.symbol().modifiers.has(Modifiers::COMPANION)))
    {
        return;
    }
    out.report(&NO_COMPANION_OBJECT, n.text_range(), format!("Classifier '{name}' does not have a companion object, and thus must be initialized here."), None);
}

/// `super.f()` where `f` is abstract in the supertype.
fn abstract_super_call(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let mut parts = n.children();
    let (Some(sup), Some(sel)) = (parts.next(), parts.next()) else { return };
    if sup.kind() != SyntaxKind::SUPER_EXPR {
        return;
    }
    let member = match sel.kind() {
        SyntaxKind::NAME_REF => sel.clone(),
        SyntaxKind::CALL_EXPR => match sel.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) {
            Some(c) => c,
            None => return,
        },
        _ => return,
    };
    let defs = e.resolve_name_ref(&member);
    if !defs.is_empty() && defs.iter().all(|d| matches!(d, Def::Symbol { sym, .. } if sym.modifiers.has(Modifiers::ABSTRACT))) {
        out.report(&ABSTRACT_SUPER_CALL, member.text_range(), "Abstract member cannot be accessed directly.", None);
    }
}

/// `const val X = compute()` or `= other` (a non-`const` property of this project).
fn const_initializer(e: &Engine, p: &SyntaxNode, out: &mut Sink) {
    if !has_modifier(p, "const") {
        return;
    }
    let Some(init) = initializer(p) else { return };
    let name = match init.kind() {
        SyntaxKind::NAME_REF => init.clone(),
        SyntaxKind::DOT_QUALIFIED_EXPR => match init.last_child().filter(|l| l.kind() == SyntaxKind::NAME_REF) {
            Some(l) => l,
            None => return,
        },
        SyntaxKind::CALL_EXPR => match init.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) {
            Some(c) => c,
            None => return,
        },
        _ => return,
    };
    let defs = e.resolve_name_ref(&name);
    let [Def::Symbol { sym, .. }] = defs.as_slice() else { return };
    let source = matches!(sym.origin, Origin::Source { .. });
    let non_const = match sym.kind {
        SymbolKind::Property => source && !sym.modifiers.has(Modifiers::CONST),
        SymbolKind::Function => source && init.kind() == SyntaxKind::CALL_EXPR,
        _ => false,
    };
    if non_const {
        out.report(&CONST_VAL_WITH_NON_CONST_INITIALIZER, init.text_range(), "Const 'val' initializer should be a constant value.", None);
    }
}

/// `val (a, b) = x` where `x`'s type has no `component2()`.
fn components(e: &Engine, p: &SyntaxNode, out: &mut Sink) {
    let Some(d) = p.children().find(|c| c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) else { return };
    let Some(init) = initializer(p) else { return };
    let Some(t) = certain_type(e, &init, true) else { return };
    if matches!(t, crate::symbols::TypeRef::Named { nullable: true, .. }) || crate::analysis::ide::infer::fq_of(&t).is_none_or(|fq| fq == "kotlin.Nothing") {
        return;
    }
    for (i, entry) in d.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_ENTRY).enumerate() {
        if e.component_defs(&entry).is_some_and(|defs| defs.is_empty()) {
            out.report(&COMPONENT_FUNCTION_MISSING, entry.text_range(), format!("Destructuring declaration initializer of type '{}' must have a 'component{}()' function.", shown(&t), i + 1), None);
        }
    }
}
