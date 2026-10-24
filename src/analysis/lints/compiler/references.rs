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
            SyntaxKind::USER_TYPE => enum_entry_type(e, &n, out),
            SyntaxKind::CLASS_BODY => initialization_order(e, &n, out),
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
        let fix = modifiers(p).and_then(|m| m.modifiers().find(|t| t.text() == "const")).map(|kw| {
            let end = kw.next_token().filter(|t| t.kind().is_trivia()).map_or(kw.text_range().end(), |t| t.text_range().end());
            Fix::delete("Remove 'const' modifier", TextRange::new(kw.text_range().start(), end))
        });
        out.report(&CONST_VAL_WITH_NON_CONST_INITIALIZER, init.text_range(), "Const 'val' initializer should be a constant value.", fix);
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

/// `x is Color.RED` and `val c: Color.RED`: an enum entry is not a type.
fn enum_entry_type(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    if n.parent().is_some_and(|p| p.kind() == SyntaxKind::USER_TYPE) {
        return;
    }
    let segments: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == SyntaxKind::USER_TYPE_SEGMENT).collect();
    if segments.len() < 2 || segments.iter().any(|s| s.children().any(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST)) {
        return;
    }
    let Some(names) = segments.iter().map(|s| ast::token(s, SyntaxKind::IDENT).map(|t| ast::ident_text(&t))).collect::<Option<Vec<_>>>() else { return };
    let (last, prefix) = names.split_last().expect("two segments");
    let prefix: Vec<&str> = prefix.iter().map(String::as_str).collect();
    let Some(fq) = e.classifier_named(n, &prefix) else { return };
    let Some(c) = e.classifier(&fq) else { return };
    if c.symbol().kind != SymbolKind::Enum || e.classifier(&format!("{fq}.{last}")).is_some() {
        return;
    }
    if !e.all_static_members(&c).1.iter().any(|(m, _)| m.kind == SymbolKind::EnumEntry && m.name == *last) {
        return;
    }
    let is_check = n.ancestors().skip(1).take_while(|a| !matches!(a.kind(), SyntaxKind::BLOCK | SyntaxKind::FUN | SyntaxKind::CLASS_BODY)).any(|a| a.kind() == SyntaxKind::IS_EXPR || a.kind() == SyntaxKind::WHEN_CONDITION && ast::token(&a, SyntaxKind::IS_KW).is_some());
    if is_check {
        // `x is E.A` → `x == E.A`; in a `when` branch `is E.A ->` → `E.A ->`.
        let is_node = n.ancestors().find(|a| a.kind() == SyntaxKind::IS_EXPR || a.kind() == SyntaxKind::WHEN_CONDITION);
        let kw = is_node.as_ref().and_then(|a| a.descendants_with_tokens().filter_map(|t| t.into_token()).find(|t| t.kind() == SyntaxKind::IS_KW));
        let fix = kw.zip(is_node).map(|(kw, a)| {
            if a.kind() == SyntaxKind::IS_EXPR {
                Fix::replace("Replace 'is' with '=='", kw.text_range(), "==")
            } else {
                let end = kw.next_token().filter(|t| t.kind().is_trivia()).map_or(kw.text_range().end(), |t| t.text_range().end());
                Fix::delete("Remove 'is'", TextRange::new(kw.text_range().start(), end))
            }
        });
        out.report(&IS_ENUM_ENTRY, n.text_range(), "'is' over enum entry is prohibited, use comparison instead.", fix);
    } else {
        out.report(&ENUM_ENTRY_AS_TYPE, n.text_range(), "Use of enum entry names as types is prohibited, use enum type instead.", None);
    }
}

/// `val a = b + 1` before `val b = 2`: initializers and `init` blocks run in order, so `b`
/// is read before it is set.
fn initialization_order(e: &Engine, body: &SyntaxNode, out: &mut Sink) {
    let members: Vec<SyntaxNode> = body.children().collect();
    // Properties with a backing field set by their initializer.
    let stored = |p: &SyntaxNode| {
        ast::PropertyDecl::cast(p.clone()).is_some_and(|d| d.has_initializer() && !d.has_delegate() && d.receiver().is_none() && !has_modifier(p, "const") && !has_modifier(p, "lateinit"))
            && !p.children().filter(|c| c.kind() == SyntaxKind::PROPERTY_ACCESSOR).any(|a| ast::token(&a, SyntaxKind::IDENT).is_some_and(|t| t.text() == "get"))
    };
    for (i, m) in members.iter().enumerate() {
        let region = match m.kind() {
            SyntaxKind::PROPERTY => initializer(m),
            SyntaxKind::CLASS_INITIALIZER => Some(m.clone()),
            _ => None,
        };
        let Some(region) = region else { continue };
        for r in region.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF) {
            // Lambdas, objects and local functions may run later.
            let deferred = |a: &SyntaxNode| matches!(a.kind(), SyntaxKind::LAMBDA_EXPR | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::OBJECT_LITERAL | SyntaxKind::FUN | SyntaxKind::CLASS);
            if r.ancestors().take_while(|a| *a != region).any(|a| deferred(&a)) || deferred(&region) {
                continue;
            }
            // A bare name or `this.name`.
            if let Some(p) = r.parent().filter(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR))
                && p.first_child().as_ref() != Some(&r)
                && p.first_child().is_none_or(|f| f.kind() != SyntaxKind::THIS_EXPR || f.text() != "this")
            {
                continue;
            }
            // Assigned rather than read (`b = 1` in an init block).
            if r.parent().is_some_and(|p| assignment_op(&p).is_some_and(|o| o.kind() == SyntaxKind::EQ) && p.first_child().as_ref() == Some(&r)) {
                continue;
            }
            let defs = e.resolve_name_ref(&r);
            let [Def::Symbol { sym, .. }] = defs.as_slice() else { continue };
            if sym.kind != SymbolKind::Property {
                continue;
            }
            let Origin::Source { range, file, .. } = &sym.origin else { continue };
            if *file != e.path {
                continue;
            }
            let Some(later) = members[i + 1..].iter().find(|l| l.kind() == SyntaxKind::PROPERTY && l.text_range() == *range) else { continue };
            if stored(later) {
                out.report(&UNINITIALIZED_VARIABLE, r.text_range(), format!("Variable '{}' must be initialized.", r.text()), None);
            }
        }
    }
}
