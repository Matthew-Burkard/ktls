//! Expression-level compiler checks (need types): unresolved names, `val` reassignment,
//! null-safety warnings, deprecation, call arguments, literal type mismatches.

use std::collections::{HashMap, HashSet};

use super::super::*;
use crate::analysis::diagnostics::{LocalsCache, scope_at};
use crate::analysis::ide::infer::{Def, Engine, LocalKind, Qualifier, fq_of, strip};
use crate::analysis::resolve::Resolved;
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    // Members of objects/classes imported with `import a.Obj.*` are not modelled.
    let star_class_import = cx.snap.file(cx.path).is_some_and(|f| {
        f.summary().imports.iter().any(|i| i.star && e.res.find_classifier(&i.path()).is_some())
    });
    let mut st = State { complete_classes: HashMap::new(), star_class_import, file_names: None, scope_names: None };
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::NAME_REF => name_ref(cx, e, &n, &mut st, out),
            SyntaxKind::USER_TYPE => deprecated_type(cx, e, &n, out),
            SyntaxKind::BINARY_EXPR => {
                assignment(cx, e, &n, out);
                null_checks(e, &n, out);
                nullable_operations(cx, e, &n, out);
            }
            SyntaxKind::PREFIX_EXPR | SyntaxKind::POSTFIX_EXPR => {
                increment(cx, e, &n, out);
                not_null_assertion(e, &n, out);
                nullable_operations(cx, e, &n, out);
            }
            SyntaxKind::INDEX_EXPR | SyntaxKind::LOOP_RANGE => nullable_operations(cx, e, &n, out),
            SyntaxKind::SAFE_QUALIFIED_EXPR => safe_call(e, &n, out),
            SyntaxKind::DOT_QUALIFIED_EXPR => unsafe_call(cx, e, &n, out),
            SyntaxKind::CALL_EXPR => {
                call_arguments(e, &n, out);
                suspend_call(e, &n, out);
            }
            SyntaxKind::PROPERTY | SyntaxKind::VALUE_PARAMETER => literal_initializer(e, &n, out),
            SyntaxKind::RETURN_EXPR => literal_return(e, &n, out),
            SyntaxKind::FUN => literal_expression_body(e, &n, out),
            _ => {}
        }
    }
}

struct State {
    /// Class-like node → its supertypes all resolve (members are fully known).
    complete_classes: HashMap<TextRange, bool>,
    star_class_import: bool,
    /// Names declared in this file (computed on first use).
    file_names: Option<HashSet<String>>,
    /// Names of top-level declarations in scope: imports, this package, default imports.
    scope_names: Option<HashSet<String>>,
}

impl State {
    /// Whether `name` is declared anywhere it could come from. Unresolved references are
    /// only reported for names that exist nowhere (typos, deleted declarations): resolution
    /// of the rest may fail for reasons this engine does not model.
    fn exists(&mut self, cx: &Cx, e: &Engine, name: &str) -> bool {
        if cx.snap.index.declares_name(name) {
            return true;
        }
        let file = self.file_names.get_or_insert_with(|| {
            idents(&cx.root)
                .filter(|t| {
                    t.parent().is_some_and(|p| {
                        matches!(
                            p.kind(),
                            SyntaxKind::PROPERTY
                                | SyntaxKind::FUN
                                | SyntaxKind::VALUE_PARAMETER
                                | SyntaxKind::DESTRUCTURING_ENTRY
                                | SyntaxKind::CLASS
                                | SyntaxKind::OBJECT_DECLARATION
                                | SyntaxKind::TYPEALIAS
                                | SyntaxKind::ENUM_ENTRY
                                | SyntaxKind::TYPE_PARAMETER
                                | SyntaxKind::IMPORT_ALIAS
                        )
                    })
                })
                .map(|t| ast::ident_text(&t))
                .collect()
        });
        if file.contains(name) {
            return true;
        }
        let scope = self.scope_names.get_or_insert_with(|| {
            let mut out = HashSet::new();
            let (classes, callables) = e.imported_symbols();
            out.extend(classes.into_iter().map(|(n, _)| n));
            out.extend(callables.into_iter().map(|c| c.name));
            if let Some(f) = cx.snap.file(cx.path) {
                out.extend(f.summary().imports.iter().filter_map(|i| i.imported_name().map(str::to_string)));
            }
            for pkg in e.scope_packages() {
                let (classes, callables) = e.res.package_contents(&pkg);
                out.extend(classes.into_iter().map(|(n, _)| n));
                out.extend(callables.into_iter().map(|c| c.name));
            }
            out
        });
        scope.contains(name)
    }
}

/// Lambdas whose signature is unknown may have receivers (DSLs): names inside them
/// cannot be judged.
fn in_unknown_lambda(e: &Engine, at: &SyntaxNode) -> bool {
    at.ancestors().any(|a| a.kind() == SyntaxKind::LAMBDA_EXPR && e.lambda_signature(&a).is_none())
}

/// A bare name resolved to a class member (through an implicit receiver) inside a lambda
/// whose receiver is unknown: that receiver may declare the name itself.
pub(super) fn uncertain_implicit_member(e: &Engine, nr: &SyntaxNode, defs: &[Def]) -> bool {
    let qualified = nr.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.first_child().as_ref() != Some(nr))
        || nr.parent().and_then(|c| c.parent()).is_some_and(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.first_child().as_ref() != nr.parent().as_ref());
    let member = defs.iter().any(|d| matches!(d, Def::Symbol { sym, .. } if matches!(sym.kind, SymbolKind::Function | SymbolKind::Property | SymbolKind::Field) && sym.fq_name.rsplit_once('.').is_some_and(|(owner, _)| e.classifier(owner).is_some())));
    !qualified && member && in_unknown_lambda(e, nr)
}

/// Context receivers/parameters bring names into scope that are not modelled.
fn in_context_scope(at: &SyntaxNode) -> bool {
    at.ancestors().any(|a| a.children().any(|c| c.kind() == SyntaxKind::CONTEXT_RECEIVER_LIST))
}

/// `is` checks and casts in the enclosing declaration may smart-cast receivers.
fn smart_cast_possible(at: &SyntaxNode) -> bool {
    let Some(scope) = at.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::CLASS_INITIALIZER | SyntaxKind::SECONDARY_CONSTRUCTOR) && !is_local(a)) else {
        return true;
    };
    tokens(&scope).any(|t| matches!(t.kind(), SyntaxKind::IS_KW | SyntaxKind::NOT_IS_KW | SyntaxKind::AS_KW | SyntaxKind::AS_SAFE_KW))
}

/// All enclosing classes have fully known hierarchies (otherwise inherited names are unknown).
fn receivers_known(e: &Engine, at: &SyntaxNode, st: &mut State) -> bool {
    for (t, _) in e.implicit_receivers(at) {
        if fq_of(&t).is_none_or(|fq| e.classifier(fq).is_none() && !fq.contains("anon_")) {
            return false;
        }
    }
    for anc in at.ancestors().filter(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL)) {
        let key = anc.text_range();
        let known = *st.complete_classes.entry(key).or_insert_with(|| {
            let t = match anc.kind() {
                SyntaxKind::OBJECT_LITERAL => Some(e.anon_type(&anc)),
                _ => e.implicit_receivers(&anc.children().find(|c| c.kind() == SyntaxKind::CLASS_BODY).and_then(|b| b.first_child()).unwrap_or(anc.clone())).into_iter().next().map(|(t, _)| t),
            };
            t.is_some_and(|t| e.supertype_member_names(&t).is_some())
        });
        if !known {
            return false;
        }
    }
    true
}

fn name_ref(cx: &Cx, e: &Engine, nr: &SyntaxNode, st: &mut State, out: &mut Sink) {
    let Some(tok) = nr.first_token().filter(|t| t.kind() == SyntaxKind::IDENT) else { return };
    if nr.ancestors().any(|a| matches!(a.kind(), SyntaxKind::ANNOTATION | SyntaxKind::IMPORT_DIRECTIVE | SyntaxKind::PACKAGE_DIRECTIVE)) {
        return;
    }
    let name = ast::ident_text(&tok);
    let expr = match nr.parent() {
        Some(p) if p.kind() == SyntaxKind::CALL_EXPR && p.first_child().as_ref() == Some(nr) => p,
        _ => nr.clone(),
    };
    let Some(outer) = expr.parent() else { return };
    let member = matches!(outer.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && outer.first_child().as_ref() != Some(&expr);
    let callable_ref = outer.kind() == SyntaxKind::CALLABLE_REFERENCE;
    let defs = e.resolve_name_ref(nr);
    // Deprecation: every candidate is deprecated (at this API version).
    let levels: Vec<Option<super::markers::Level>> = defs.iter().map(|d| if let Def::Symbol { sym, .. } = d { super::markers::deprecation(cx, sym) } else { None }).collect();
    if !levels.is_empty() && levels.iter().all(Option::is_some) && !in_deprecated_declaration(nr) && !uncertain_implicit_member(e, nr, &defs) {
        let error = levels.iter().all(|l| *l == Some(super::markers::Level::Error));
        out.report(if error { &DEPRECATION_ERROR } else { &DEPRECATION }, tok.text_range(), format!("'{name}' is deprecated."), None);
    }
    if !defs.is_empty() || !cx.on(&UNRESOLVED_REFERENCE_EXPR) || in_unknown_lambda(e, nr) || in_context_scope(nr) {
        return;
    }
    if member {
        let Some(recv) = outer.first_child() else { return };
        if smart_cast_possible(nr) || st.exists(cx, e, &name) {
            return;
        }
        // `x.f()` with a local `f` of function type with receiver (`it` of `T.() -> R`).
        if e.resolve_name_text(nr, &name).iter().any(|d| matches!(d, Def::Local(_))) {
            return;
        }
        // Only receivers of certain type.
        let Some(t) = certain_receiver(e, &recv) else { return };
        let Some(fq) = fq_of(&t) else { return };
        if matches!(fq, "kotlin.Any" | "kotlin.Nothing") || e.classifier(fq).is_none() || jvm_members(e, &t).is_none_or(|m| m.contains(&name)) {
            return;
        }
        let Some(inherited) = e.supertype_member_names(&t) else { return };
        if inherited.contains(&name) || e.all_members(&t).iter().any(|(m, _)| m.name == name) {
            return;
        }
        out.report(&UNRESOLVED_REFERENCE_EXPR, tok.text_range(), format!("Unresolved reference '{name}' for a receiver of type '{}'.", super::certain::shown(&t)), None);
        return;
    }
    if callable_ref && outer.first_child().as_ref() != Some(nr) {
        return;
    }
    if st.star_class_import || st.exists(cx, e, &name) || !receivers_known(e, nr, st) || e.res.package_exists(&name) || smart_cast_possible(nr) {
        return;
    }
    // Code generators (kapt, KSP) add declarations that are not in the sources yet.
    if cx.module.is_some_and(|m| m.kotlin.compiler_plugins.iter().any(|p| matches!(p.as_str(), "kapt" | "ksp"))) {
        return;
    }
    // Members of implicit receivers declared in libraries.
    for (t, _) in e.implicit_receivers(nr) {
        if jvm_members(e, &t).is_none_or(|m| m.contains(&name)) || e.all_members(&t).iter().any(|(m, _)| m.name == name) {
            return;
        }
    }
    out.report(&UNRESOLVED_REFERENCE_EXPR, tok.text_range(), format!("Unresolved reference '{name}'."), None);
}

/// Kotlin built-in types mapped to JVM classes (`MutableMap` → `java.util.Map`) also expose
/// the Java members (`computeIfAbsent`, `chars()`): their names, or `None` for built-ins whose
/// members are not fully known (`kotlin.reflect`).
fn jvm_members(e: &Engine, t: &TypeRef) -> Option<HashSet<String>> {
    const MAPPED: &[(&str, &str)] = &[
        ("kotlin.CharSequence", "java.lang.CharSequence"),
        ("kotlin.String", "java.lang.String"),
        ("kotlin.Throwable", "java.lang.Throwable"),
        ("kotlin.Enum", "java.lang.Enum"),
        ("kotlin.Number", "java.lang.Number"),
        ("kotlin.Comparable", "java.lang.Comparable"),
        ("kotlin.collections.Iterable", "java.lang.Iterable"),
        ("kotlin.collections.MutableIterable", "java.lang.Iterable"),
        ("kotlin.collections.Iterator", "java.util.Iterator"),
        ("kotlin.collections.MutableIterator", "java.util.Iterator"),
        ("kotlin.collections.Collection", "java.util.Collection"),
        ("kotlin.collections.MutableCollection", "java.util.Collection"),
        ("kotlin.collections.List", "java.util.List"),
        ("kotlin.collections.MutableList", "java.util.List"),
        ("kotlin.collections.Set", "java.util.Set"),
        ("kotlin.collections.MutableSet", "java.util.Set"),
        ("kotlin.collections.Map", "java.util.Map"),
        ("kotlin.collections.MutableMap", "java.util.Map"),
        ("kotlin.collections.Map.Entry", "java.util.Map.Entry"),
        ("kotlin.collections.MutableMap.MutableEntry", "java.util.Map.Entry"),
    ];
    let mut out = HashSet::new();
    for fq in e.supertype_closure(t).iter().filter_map(fq_of) {
        if fq.starts_with("kotlin.reflect.") || fq.starts_with("kotlin.coroutines.") {
            return None;
        }
        if let Some((_, java)) = MAPPED.iter().find(|(k, _)| *k == fq) {
            let c = e.classifier(java)?;
            // Read-only types hide the Java mutators (`List.add`): the members their mutable
            // counterpart adds.
            let mutable = fq.rsplit_once('.').map(|(p, n)| format!("{p}.Mutable{n}")).filter(|m| !fq.contains(".Mutable") && e.classifier(m).is_some());
            let hidden: HashSet<String> = match &mutable {
                Some(m) => {
                    let own: HashSet<String> = e.all_members(&TypeRef::named(fq)).into_iter().map(|(m, _)| m.name).collect();
                    e.all_members(&TypeRef::named(m)).into_iter().map(|(m, _)| m.name).filter(|n| !own.contains(n)).collect()
                }
                None => HashSet::new(),
            };
            for (m, _) in e.all_members(&crate::analysis::ide::infer::class_type(c.symbol())) {
                if !hidden.contains(&m.name) {
                    out.insert(m.name);
                }
            }
        }
    }
    Some(out)
}

/// Uses inside a deprecated declaration are not reported.
fn in_deprecated_declaration(n: &SyntaxNode) -> bool {
    n.ancestors().any(|a| {
        ast::child::<ast::ModifierList>(&a).is_some_and(|m| {
            m.annotations().any(|an| an.type_ref().is_some_and(|t| matches!(t.syntax().text().to_string().as_str(), "Deprecated" | "kotlin.Deprecated" | "java.lang.Deprecated")))
        })
    })
}

fn deprecated_type(cx: &Cx, e: &Engine, ut: &SyntaxNode, out: &mut Sink) {
    if ut.ancestors().any(|a| matches!(a.kind(), SyntaxKind::IMPORT_DIRECTIVE)) {
        return;
    }
    let Some(user) = ast::UserType::cast(ut.clone()) else { return };
    let segs: Vec<(String, TextRange)> = user.segments().iter().filter_map(|s| s.name_token()).map(|t| (ast::ident_text(&t), t.text_range())).collect();
    if segs.is_empty() {
        return;
    }
    let scope = scope_at(ut, &e.package(), &mut LocalsCache::default());
    let names: Vec<&str> = segs.iter().map(|(s, _)| s.as_str()).collect();
    if let Ok(Resolved::Classifier(c)) = e.res.resolve_type_name(&names, &scope)
        && let Some(level) = super::markers::deprecation(cx, c.symbol())
        && !in_deprecated_declaration(ut)
    {
        let (n, r) = segs.last().unwrap();
        let rule = if level == super::markers::Level::Error { &DEPRECATION_ERROR } else { &DEPRECATION };
        out.report(rule, *r, format!("'{n}' is deprecated."), None);
    }
}

// ------------------------------------------------------------ val reassignment

/// What a `val` assignment target is, if reassigning it is an error.
fn val_target(cx: &Cx, e: &Engine, target: &SyntaxNode) -> Option<(String, Option<Fix>)> {
    let nr = match target.kind() {
        SyntaxKind::NAME_REF => target.clone(),
        SyntaxKind::DOT_QUALIFIED_EXPR => target.children().nth(1).filter(|c| c.kind() == SyntaxKind::NAME_REF)?,
        _ => return None,
    };
    let defs = e.resolve_name_ref(&nr);
    let [def] = defs.as_slice() else { return None };
    match def {
        Def::Local(l) => match l.kind {
            LocalKind::Parameter | LocalKind::LambdaParameter => Some((l.name.clone(), None)),
            LocalKind::Val => {
                // `val x: Int` assigned later is a deferred initialization.
                let decl = crate::syntax::covering(&cx.root, l.name_range)?.ancestors().find(|a| a.kind() == SyntaxKind::PROPERTY)?;
                let initialized = ast::token(&decl, SyntaxKind::EQ).is_some() || decl.children().any(|c| c.kind() == SyntaxKind::PROPERTY_DELEGATE);
                let val = ast::token(&decl, SyntaxKind::VAL_KW)?;
                initialized.then(|| (l.name.clone(), Some(Fix::replace("Change to 'var'", val.text_range(), "var"))))
            }
            _ => None,
        },
        Def::Symbol { sym, .. } if sym.kind == SymbolKind::Property || sym.kind == SymbolKind::Field => {
            if sym.modifiers.has(Modifiers::VAR) || sym.kind == SymbolKind::Field && !matches!(sym.origin, Origin::Library { .. }) {
                return None;
            }
            match &sym.origin {
                Origin::Source { file, name_range, .. } => {
                    let f = cx.snap.file(file)?;
                    let root = f.parse().syntax();
                    let decl = crate::syntax::covering(&root, *name_range)?.ancestors().find(|a| matches!(a.kind(), SyntaxKind::PROPERTY | SyntaxKind::VALUE_PARAMETER))?;
                    let keyword = ast::token(&decl, SyntaxKind::VAL_KW)?;
                    if ast::token(&decl, SyntaxKind::VAR_KW).is_some() {
                        return None;
                    }
                    let deferred = decl.kind() == SyntaxKind::PROPERTY
                        && ast::token(&decl, SyntaxKind::EQ).is_none()
                        && !decl.children().any(|c| matches!(c.kind(), SyntaxKind::PROPERTY_DELEGATE | SyntaxKind::PROPERTY_ACCESSOR));
                    let in_init = target.ancestors().any(|a| matches!(a.kind(), SyntaxKind::CLASS_INITIALIZER | SyntaxKind::SECONDARY_CONSTRUCTOR));
                    if deferred && in_init {
                        return None;
                    }
                    let fix = (*file == cx.path).then(|| Fix::replace("Change to 'var'", keyword.text_range(), "var"));
                    Some((sym.name.clone(), fix))
                }
                // Library vals (Kotlin metadata knows `var`).
                _ => (sym.kind == SymbolKind::Property).then(|| (sym.name.clone(), None)),
            }
        }
        _ => None,
    }
}

fn assignment(cx: &Cx, e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    // Compound assignments may call `plusAssign` on a mutable value instead.
    if !assignment_op(n).is_some_and(|op| op.kind() == SyntaxKind::EQ) || !cx.on(&VAL_REASSIGNMENT) {
        return;
    }
    let Some(target) = n.first_child() else { return };
    if let Some((name, fix)) = val_target(cx, e, &target) {
        out.report(&VAL_REASSIGNMENT, target.text_range(), format!("'val' cannot be reassigned: '{name}'."), fix);
    }
}

fn increment(cx: &Cx, e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    if !binary_op(n).is_some_and(|op| matches!(op.kind(), SyntaxKind::PLUS2 | SyntaxKind::MINUS2)) || !cx.on(&VAL_REASSIGNMENT) {
        return;
    }
    let Some(target) = n.children().find(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE) else { return };
    if let Some((name, fix)) = val_target(cx, e, &target) {
        out.report(&VAL_REASSIGNMENT, target.text_range(), format!("'val' cannot be reassigned: '{name}'."), fix);
    }
}

// ------------------------------------------------------------ null safety

/// The type of a member chain's receiver when it is certain, and the engine (which finds
/// the members) sees the same class.
fn certain_receiver(e: &Engine, recv: &SyntaxNode) -> Option<TypeRef> {
    let t = super::certain::certain_type(e, recv, false)?;
    let seen = e.type_of(recv)?;
    (fq_of(&seen) == fq_of(&t) && fq_of(&t).is_some()).then_some(t)
}

/// The declared (written) non-null type of a variable or property used as `expr`.
fn declared_non_null(e: &Engine, expr: &SyntaxNode) -> Option<String> {
    if expr.kind() != SyntaxKind::NAME_REF {
        return None;
    }
    let defs = e.resolve_name_ref(expr);
    let [def] = defs.as_slice() else { return None };
    let (file, range) = match def {
        Def::Local(l) if matches!(l.kind, LocalKind::Val | LocalKind::Var | LocalKind::Parameter) => (e.path.clone(), l.name_range),
        Def::Symbol { sym, .. } if sym.kind == SymbolKind::Property && !sym.modifiers.has(Modifiers::LATEINIT) => match &sym.origin {
            Origin::Source { file, name_range, .. } => (file.clone(), *name_range),
            _ => return None,
        },
        _ => return None,
    };
    let root = if file == e.path { e.root.clone() } else { e.snap.file(&file)?.parse().syntax() };
    let decl = crate::syntax::covering(&root, range)?.ancestors().find(|a| matches!(a.kind(), SyntaxKind::PROPERTY | SyntaxKind::VALUE_PARAMETER))?;
    if has_modifier(&decl, "lateinit") {
        return None;
    }
    let ty = ast::child::<ast::TypeRef>(&decl)?;
    if ty.inner().is_none_or(|i| i.kind() == SyntaxKind::NULLABLE_TYPE || i.kind() == SyntaxKind::DYNAMIC_TYPE) || ty.syntax().text().to_string().trim_end().ends_with('?') {
        return None;
    }
    let shown = ty.syntax().text().to_string();
    // Resolve in the declaring file: type parameters and nullable type aliases do not count.
    let de = if file == e.path { None } else { Engine::new(e.snap, &file) };
    let resolved = de.as_ref().unwrap_or(e).resolve_syntax_type(&ty);
    match &resolved {
        TypeRef::Named { fq: Some(fq), nullable: false, .. } => {
            let c = de.as_ref().unwrap_or(e).classifier(fq);
            let alias = e.res.find_classifier(fq).is_some_and(|c| c.symbol().kind == SymbolKind::TypeAlias);
            (c.is_some() && !alias).then_some(shown)
        }
        TypeRef::Function { nullable: false, .. } => Some(shown),
        _ => None,
    }
}

/// `x.member` where `x` is a local or parameter declared with a nullable type. Smart casts
/// are not modelled, so this only reports when no use of the name in the declaring scope
/// could narrow it (null comparisons, `!!`, `?:`, `is`, assignments, arguments, `when`
/// subjects, extension calls with contracts such as `isNullOrEmpty`).
fn unsafe_call(cx: &Cx, e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&UNSAFE_CALL) {
        return;
    }
    let (Some(recv), Some(sel)) = (n.first_child(), n.last_child()) else { return };
    if recv == sel {
        return;
    }
    let member_ref = match sel.kind() {
        SyntaxKind::NAME_REF => sel.clone(),
        SyntaxKind::CALL_EXPR => match sel.first_child() {
            Some(c) if c.kind() == SyntaxKind::NAME_REF => c,
            _ => return,
        },
        _ => return,
    };
    // A member (not an extension, which may accept a nullable receiver).
    let member_defs = e.resolve_name_ref(&member_ref);
    let [Def::Symbol { sym, .. }] = member_defs.as_slice() else { return };
    if sym.receiver.is_some() || !matches!(sym.kind, SymbolKind::Function | SymbolKind::Property | SymbolKind::Field) || matches!(sym.name.as_str(), "toString" | "hashCode" | "equals") {
        return;
    }
    let report = |shown: &str, out: &mut Sink, extra: Option<Fix>| {
        let Some(dot) = n.children_with_tokens().filter_map(|t| t.into_token()).find(|t| t.kind() == SyntaxKind::DOT) else { return };
        let fixes = vec![Fix::replace("Replace with safe (?.) call", dot.text_range(), "?."), Fix::replace("Add non-null asserted (!!.) call", dot.text_range(), "!!.")];
        out.report_fixes(&UNSAFE_CALL, dot.text_range(), format!("Only safe (?.) or non-null asserted (!!.) calls are allowed on a nullable receiver of type '{shown}'."), fixes.into_iter().chain(extra).collect());
    };
    let nullable = |t: &TypeRef| matches!(t, TypeRef::Named { nullable: true, .. } | TypeRef::Function { nullable: true, .. });
    // Call results (never smart cast): `f().g()`, `m[k].g()`, `a?.f().g()`.
    let call_result = matches!(recv.kind(), SyntaxKind::CALL_EXPR | SyntaxKind::INDEX_EXPR)
        || matches!(recv.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && recv.last_child().is_some_and(|l| l.kind() == SyntaxKind::CALL_EXPR);
    if call_result {
        if let Some(t) = super::certain::certain_type(e, &recv, true)
            && nullable(&t)
            && fq_of(&t) != Some("kotlin.Nothing")
        {
            report(&super::certain::shown(&t), out, None);
        }
        return;
    }
    if recv.kind() == SyntaxKind::NAME_REF
        && let Some(ty) = nullable_local(e, &recv)
    {
        report(&ty, out, surround_with_null_check(cx, n, &recv.text().to_string()));
    } else if let Some(ty) = nullable_property_chain(e, &recv) {
        report(&ty, out, None);
    }
}

/// "Surround with null check": `if (x != null) { statement }` around the statement that
/// contains `at` (not a declaration, whose scope would change).
fn surround_with_null_check(cx: &Cx, at: &SyntaxNode, name: &str) -> Option<Fix> {
    let stmt = at.ancestors().take_while(|a| !matches!(a.kind(), SyntaxKind::LAMBDA_EXPR | SyntaxKind::FUN)).find(|a| a.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK))?;
    if matches!(stmt.kind(), SyntaxKind::PROPERTY | SyntaxKind::FUN | SyntaxKind::CLASS | SyntaxKind::RETURN_EXPR) {
        return None;
    }
    let indent = indent_at(cx.text, stmt.text_range().start());
    let body = stmt.text().to_string().replace('\n', "\n    ");
    Some(Fix::replace("Surround with null check", stmt.text_range(), format!("if ({name} != null) {{\n{indent}    {body}\n{indent}}}")))
}

/// The nullable type of local or parameter `nr` at this use, when no earlier use (or one
/// in an enclosing loop) narrows it in a way the engine does not model, and the engine's
/// smart casts leave it nullable.
pub(super) fn nullable_local(e: &Engine, nr: &SyntaxNode) -> Option<String> {
    let name = ast::ident_text(&nr.first_token()?);
    let defs = e.resolve_name_ref(nr);
    let [Def::Local(local)] = defs.as_slice() else { return None };
    if !matches!(local.kind, LocalKind::Parameter | LocalKind::Val | LocalKind::Var) {
        return None;
    }
    // Declared with a nullable type: written, or that of a certain initializer.
    let decl = crate::syntax::covering(&e.root, local.name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| matches!(a.kind(), SyntaxKind::VALUE_PARAMETER | SyntaxKind::PROPERTY))?;
    if decl.kind() == SyntaxKind::PROPERTY && !is_local(&decl) || has_modifier(&decl, "vararg") {
        return None;
    }
    let ty_text = match ast::child::<ast::TypeRef>(&decl) {
        Some(ty) => ty.syntax().text().to_string().trim().to_string(),
        None => match initializer(&decl).and_then(|v| super::certain::certain_type(e, &v, true)) {
            Some(t) if matches!(t, TypeRef::Named { nullable: true, .. }) => super::certain::shown(&t),
            _ => return None,
        },
    };
    if !ty_text.ends_with('?') {
        return None;
    }
    // The scope the name is visible in.
    let scope = match decl.kind() {
        SyntaxKind::PROPERTY => decl.parent(),
        _ => decl.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::LAMBDA_EXPR | SyntaxKind::CLASS | SyntaxKind::FOR_EXPR | SyntaxKind::CATCH_CLAUSE)),
    }?;
    // Uses before this one, or in a loop around it (they run before the next iteration).
    let loops: Vec<SyntaxNode> = nr.ancestors().take_while(|a| *a != scope).filter(|a| matches!(a.kind(), SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR)).collect();
    let earlier = |r: &SyntaxNode| r.text_range().start() < nr.text_range().start() || loops.iter().any(|l| l.text_range().contains_range(r.text_range()));
    let narrowed = scope.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF && d != nr && d.first_token().is_some_and(|t| ast::ident_text(&t) == name)).any(|r| earlier(&r) && may_narrow(e, &r));
    if narrowed {
        return None;
    }
    // Still nullable after the checks the engine understands.
    let still_nullable = e.type_of(nr).is_some_and(|t| matches!(crate::analysis::ide::infer::strip(&t), TypeRef::Named { nullable: true, .. } | TypeRef::TypeParam { nullable: true, .. } | TypeRef::Function { nullable: true, .. }));
    still_nullable.then_some(ty_text)
}

/// An operand certainly `null`able here: a local or parameter (see `nullable_local`), a
/// property chain (see `nullable_property_chain`), or a call result. Its type as shown and
/// the type without `?`.
fn nullable_operand(e: &Engine, x: &SyntaxNode) -> Option<(String, TypeRef)> {
    let shown = match x.kind() {
        SyntaxKind::NAME_REF => nullable_local(e, x)?,
        SyntaxKind::DOT_QUALIFIED_EXPR if x.last_child().is_some_and(|l| l.kind() == SyntaxKind::NAME_REF) => nullable_property_chain(e, x)?,
        SyntaxKind::CALL_EXPR | SyntaxKind::INDEX_EXPR | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR => {
            let t = super::certain::certain_type(e, x, true)?;
            if !matches!(t, TypeRef::Named { nullable: true, ref fq, .. } if fq.as_deref() != Some("kotlin.Nothing")) {
                return None;
            }
            super::certain::shown(&t)
        }
        _ => return None,
    };
    let t = e.type_of(x)?;
    let TypeRef::Named { name, fq, args, .. } = crate::analysis::ide::infer::strip(&t).clone() else { return None };
    Some((shown, TypeRef::Named { name, fq, args, nullable: false }))
}

/// Operators, indexing and `for` loops on nullable values: the operator functions they
/// call need a non-null receiver (unless one is declared on a nullable type, like
/// `String?.plus`).
fn nullable_operations(cx: &Cx, e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&UNSAFE_OPERATOR_CALL) && !cx.on(&UNSAFE_CALL) && !cx.on(&ITERATOR_ON_NULLABLE) {
        return;
    }
    let accepts_null = |t: &TypeRef, name: &str, at: &SyntaxNode| {
        e.extensions(t, name).into_iter().chain(e.member_extensions(at, t, name)).any(|(m, _)| {
            m.receiver.as_ref().is_some_and(|r| matches!(e.resolve_declared(&m, r), TypeRef::Named { nullable: true, .. } | TypeRef::TypeParam { nullable: true, .. } | TypeRef::Platform(_)))
        })
    };
    match n.kind() {
        SyntaxKind::BINARY_EXPR => {
            let Some(op) = binary_op(n) else { return };
            let name = match op.kind() {
                SyntaxKind::PLUS => "plus",
                SyntaxKind::MINUS => "minus",
                SyntaxKind::STAR => "times",
                SyntaxKind::SLASH => "div",
                SyntaxKind::PERCENT => "rem",
                SyntaxKind::RANGE => "rangeTo",
                SyntaxKind::RANGE_UNTIL => "rangeUntil",
                SyntaxKind::LT | SyntaxKind::GT | SyntaxKind::LT_EQ | SyntaxKind::GT_EQ => "compareTo",
                SyntaxKind::IN_KW | SyntaxKind::NOT_IN_KW => "contains",
                _ => return,
            };
            let Some((l, r)) = binary_operands(n) else { return };
            // `x in c` calls `c.contains(x)`.
            let recv = if name == "contains" { r } else { l };
            let Some((shown, t)) = nullable_operand(e, &recv) else { return };
            if accepts_null(&t, name, n) {
                return;
            }
            let fix = is_primary(&recv).then(|| Fix::replace("Add non-null asserted (!!) call", TextRange::empty(recv.text_range().end()), "!!"));
            out.report(&UNSAFE_OPERATOR_CALL, op.text_range(), format!("Operator '{}' is not allowed on a nullable receiver of type '{shown}'.", op.text()), fix);
        }
        SyntaxKind::PREFIX_EXPR => {
            let Some(op) = binary_op(n) else { return };
            let name = match op.kind() {
                SyntaxKind::MINUS => "unaryMinus",
                SyntaxKind::PLUS => "unaryPlus",
                SyntaxKind::EXCL => "not",
                _ => return,
            };
            let Some(operand) = n.children().find(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE) else { return };
            let Some((shown, t)) = nullable_operand(e, &operand) else { return };
            if !accepts_null(&t, name, n) {
                let fix = is_primary(&operand).then(|| Fix::replace("Add non-null asserted (!!) call", TextRange::empty(operand.text_range().end()), "!!"));
                out.report(&UNSAFE_OPERATOR_CALL, op.text_range(), format!("Operator '{}' is not allowed on a nullable receiver of type '{shown}'.", op.text()), fix);
            }
        }
        SyntaxKind::INDEX_EXPR => {
            let Some(recv) = n.first_child() else { return };
            let set = n.parent().is_some_and(|p| assignment_op(&p).is_some() && p.first_child().as_ref() == Some(n));
            let name = if set { "set" } else { "get" };
            let Some((shown, t)) = nullable_operand(e, &recv) else { return };
            if accepts_null(&t, name, n) {
                return;
            }
            let Some(open) = n.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).and_then(|l| l.first_token()) else { return };
            out.report(&UNSAFE_CALL, open.text_range(), format!("Only safe (?.) or non-null asserted (!!.) calls are allowed on a nullable receiver of type '{shown}'."), None);
        }
        SyntaxKind::LOOP_RANGE => {
            let Some(v) = n.first_child() else { return };
            let Some((_, t)) = nullable_operand(e, &v) else { return };
            if !accepts_null(&t, "iterator", n) {
                out.report(&ITERATOR_ON_NULLABLE, v.text_range(), "Not nullable value required to call an 'iterator()' method on for-loop range.", None);
            }
        }
        _ => {}
    }
}

/// `a.b` (see [`property_chain`]) where property `b` has a nullable class type: as shown.
fn nullable_property_chain(e: &Engine, recv: &SyntaxNode) -> Option<String> {
    // Declared nullable (`String?`; a type parameter's argument may only look so: `*`).
    property_chain(e, recv, |t| matches!(t, TypeRef::Named { nullable: true, .. })).map(|t| super::certain::shown(&t))
}

/// `b.v` (names only) where property `v` is declared `T?` and the same chain is not used
/// earlier in the function: its type with the receiver's type arguments.
pub(super) fn nullable_type_param_chain(e: &Engine, recv: &SyntaxNode) -> Option<TypeRef> {
    property_chain(e, recv, |t| matches!(t, TypeRef::TypeParam { nullable: true, .. }))
}

/// `a.b` (names only) where the declared type of property `b` passes `declared`, and the
/// same chain is not used earlier in the function (where a check could smart cast it).
fn property_chain(e: &Engine, recv: &SyntaxNode, declared: impl Fn(&TypeRef) -> bool) -> Option<TypeRef> {
    if recv.kind() != SyntaxKind::DOT_QUALIFIED_EXPR || recv.descendants().any(|d| !matches!(d.kind(), SyntaxKind::NAME_REF | SyntaxKind::DOT_QUALIFIED_EXPR)) {
        return None;
    }
    let sel = recv.last_child().filter(|c| c.kind() == SyntaxKind::NAME_REF)?;
    let defs = e.resolve_name_ref(&sel);
    let [Def::Symbol { sym, subst }] = defs.as_slice() else { return None };
    if sym.kind != SymbolKind::Property {
        return None;
    }
    let ty = e.resolve_declared(sym, sym.ty.as_ref()?);
    if !declared(&ty) || in_unknown_lambda(e, recv) {
        return None;
    }
    let t = crate::analysis::ide::infer::substitute(&ty, subst);
    let text: String = recv.text().to_string().chars().filter(|c| !c.is_whitespace()).collect();
    let scope = recv.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::CLASS_INITIALIZER | SyntaxKind::SECONDARY_CONSTRUCTOR) && !is_local(a))?;
    // The same chain checked earlier (`a.b != null`, `a.b!!`, `a.b ?: …`).
    let narrowed = scope.descendants().filter(|d| d.kind() == SyntaxKind::DOT_QUALIFIED_EXPR && d.text_range().start() < recv.text_range().start()).any(|d| {
        let dt: String = d.text().to_string().chars().filter(|c| !c.is_whitespace()).collect();
        // The engine does not smart cast property chains: null checks count too.
        dt == text && (d.parent().is_some_and(|p| null_check_or_is(&p)) || may_narrow(e, &d))
    });
    (!narrowed).then_some(t)
}

/// `x == null`, `x != null` (also `===`/`!==`), or `x is T` / `x !is T`.
fn null_check_or_is(p: &SyntaxNode) -> bool {
    match p.kind() {
        SyntaxKind::IS_EXPR => true,
        SyntaxKind::BINARY_EXPR => {
            binary_op(p).is_some_and(|op| matches!(op.kind(), SyntaxKind::EQ2 | SyntaxKind::NOT_EQ | SyntaxKind::EQ3 | SyntaxKind::NOT_EQ2))
                && p.children().any(|c| c.kind() == SyntaxKind::LITERAL && c.text() == "null")
        }
        _ => false,
    }
}

/// The check `n` sits (through `!`, parentheses, `&&`, `||`) in a condition the engine's
/// smart casts understand.
fn in_modeled_condition(n: &SyntaxNode) -> bool {
    let mut cur = n.clone();
    while let Some(p) = cur.parent() {
        match p.kind() {
            SyntaxKind::PAREN_EXPR | SyntaxKind::PREFIX_EXPR => {}
            SyntaxKind::BINARY_EXPR if binary_op(&p).is_some_and(|op| matches!(op.kind(), SyntaxKind::AMP2 | SyntaxKind::PIPE2)) => {}
            SyntaxKind::CONDITION => return p.parent().is_some_and(|s| matches!(s.kind(), SyntaxKind::IF_EXPR | SyntaxKind::WHILE_EXPR)),
            SyntaxKind::WHEN_CONDITION => return true,
            SyntaxKind::VALUE_ARGUMENT => {
                let call = p.parent().and_then(|l| l.parent());
                return call.and_then(|c| c.first_child()).is_some_and(|c| matches!(c.text().to_string().as_str(), "require" | "check" | "assert"));
            }
            _ => return false,
        }
        cur = p;
    }
    false
}

/// A use of a name that could smart-cast it to non-null afterwards.
fn may_narrow(e: &Engine, r: &SyntaxNode) -> bool {
    let Some(p) = r.parent() else { return false };
    // An assignment (`if (x == null) x = y`), unless of a value that is nullable too.
    if assignment_op(&p).is_some_and(|o| o.kind() == SyntaxKind::EQ) && p.first_child().as_ref() == Some(r) {
        let value = p.children().filter(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE).nth(1);
        return !value.and_then(|v| e.type_of(&v)).is_some_and(|t| matches!(t, TypeRef::Named { nullable: true, .. }));
    }
    match p.kind() {
        // `x == null` / `x is T` where the engine applies them (an `if`/`while` condition,
        // `when` entry, `&&`/`||` operand, `require`): the smart-cast type decides.
        SyntaxKind::BINARY_EXPR | SyntaxKind::IS_EXPR if null_check_or_is(&p) && in_modeled_condition(&p) => false,
        SyntaxKind::BINARY_EXPR => binary_op(&p).is_some_and(|op| {
            matches!(op.kind(), SyntaxKind::EQ2 | SyntaxKind::NOT_EQ | SyntaxKind::EQ3 | SyntaxKind::NOT_EQ2 | SyntaxKind::ELVIS)
                || op.kind() == SyntaxKind::EQ && p.first_child().as_ref() == Some(r)
                || op.kind() == SyntaxKind::IDENT // infix calls
        }),
        // An argument: of a function that may check it (`requireNotNull(x)`, contracts), or
        // of a call used as a condition.
        SyntaxKind::VALUE_ARGUMENT => {
            let call = p.parent().and_then(|l| l.parent()).filter(|c| c.kind() == SyntaxKind::CALL_EXPR);
            let callee = call.as_ref().and_then(|c| c.first_child()).map(|c| c.text().to_string()).unwrap_or_default();
            let checking = ["require", "check", "assert", "verify", "ensure", "expect"].iter().any(|p| callee.starts_with(p)) || callee.contains("NotNull");
            let condition = p.ancestors().take_while(|a| !matches!(a.kind(), SyntaxKind::BLOCK)).any(|a| {
                matches!(a.kind(), SyntaxKind::CONDITION | SyntaxKind::WHEN_CONDITION | SyntaxKind::WHEN_SUBJECT | SyntaxKind::PREFIX_EXPR) || a.kind() == SyntaxKind::BINARY_EXPR && binary_op(&a).is_some_and(|o| matches!(o.kind(), SyntaxKind::AMP2 | SyntaxKind::PIPE2 | SyntaxKind::ELVIS))
            });
            checking || condition || call.is_none()
        }
        SyntaxKind::IS_EXPR | SyntaxKind::POSTFIX_EXPR | SyntaxKind::WHEN_SUBJECT | SyntaxKind::BINARY_WITH_TYPE | SyntaxKind::PAREN_EXPR => true,
        // `x?.size == 1 && x.values…` narrows through the comparison.
        SyntaxKind::WHEN_CONDITION | SyntaxKind::SAFE_QUALIFIED_EXPR => true,
        // `x.isNullOrEmpty()`, `x.let { … }`: extensions, possibly with contracts.
        SyntaxKind::DOT_QUALIFIED_EXPR if p.first_child().as_ref() == Some(r) => {
            let Some(sel) = p.last_child() else { return true };
            let m = if sel.kind() == SyntaxKind::CALL_EXPR { sel.first_child() } else { Some(sel) };
            let Some(m) = m.filter(|m| m.kind() == SyntaxKind::NAME_REF) else { return true };
            !matches!(e.resolve_name_ref(&m).as_slice(), [Def::Symbol { sym, .. }] if sym.receiver.is_none())
        }
        _ => false,
    }
}

fn safe_call(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(recv) = n.first_child() else { return };
    let Some(ty) = declared_non_null(e, &recv) else { return };
    let ops: Vec<SyntaxToken> = n
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|t| matches!(t.kind(), SyntaxKind::QUESTION | SyntaxKind::DOT | SyntaxKind::SAFE_DOT))
        .collect();
    let (Some(first), Some(last)) = (ops.first(), ops.last()) else { return };
    let range = TextRange::new(first.text_range().start(), last.text_range().end());
    out.report(&UNNECESSARY_SAFE_CALL, range, format!("Unnecessary safe call on a non-null receiver of type '{ty}'."), Some(Fix::replace("Replace with '.'", range, ".")));
}

fn not_null_assertion(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    if n.kind() != SyntaxKind::POSTFIX_EXPR {
        return;
    }
    let Some(op) = binary_op(n).filter(|o| o.kind() == SyntaxKind::EXCL2) else { return };
    let Some(operand) = n.first_child() else { return };
    let Some(ty) = declared_non_null(e, &operand) else { return };
    out.report(&UNNECESSARY_NOT_NULL_ASSERTION, op.text_range(), format!("Unnecessary non-null assertion (!!) on a non-null receiver of type '{ty}'."), Some(Fix::delete("Remove '!!'", op.text_range())));
}

fn null_checks(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(op) = binary_op(n) else { return };
    let Some((l, r)) = binary_operands(n) else { return };
    match op.kind() {
        SyntaxKind::ELVIS => {
            let Some(ty) = declared_non_null(e, &l) else { return };
            let range = TextRange::new(op.text_range().start(), r.text_range().end());
            let del = TextRange::new(l.text_range().end(), r.text_range().end());
            out.report(&USELESS_ELVIS, range, format!("Elvis operator (?:) always returns the left operand of non-nullable type '{ty}'."), Some(Fix::delete("Remove elvis operator", del)));
        }
        SyntaxKind::EQ2 | SyntaxKind::NOT_EQ | SyntaxKind::EQ3 | SyntaxKind::NOT_EQ2 => {
            let is_null = |x: &SyntaxNode| x.kind() == SyntaxKind::LITERAL && x.first_token().is_some_and(|t| t.kind() == SyntaxKind::NULL_KW);
            let value = if is_null(&r) {
                l
            } else if is_null(&l) {
                r
            } else {
                return;
            };
            if declared_non_null(e, &value).is_none() {
                return;
            }
            let always = matches!(op.kind(), SyntaxKind::NOT_EQ | SyntaxKind::NOT_EQ2);
            out.report(&SENSELESS_COMPARISON, n.text_range(), format!("Condition '{}' is always '{always}'.", n.text()), None);
        }
        _ => {}
    }
}

// ------------------------------------------------------------ suspension

/// A call to a `suspend` function outside a coroutine: not in a `suspend` function or
/// lambda (inline lambdas inherit their context).
fn suspend_call(e: &Engine, call: &SyntaxNode, out: &mut Sink) {
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    // Cheap first: the resolved target is `suspend`.
    if !e.resolve_name_ref(&callee).iter().any(|d| matches!(d, Def::Symbol { sym, .. } if sym.modifiers.has(Modifiers::SUSPEND))) {
        return;
    }
    // Every candidate (at all resolution levels) is `suspend`.
    let Some(candidates) = e.call_candidates(&callee) else { return };
    let suspend = !candidates.is_empty() && candidates.iter().all(|(sym, _)| sym.kind == SymbolKind::Function && sym.modifiers.has(Modifiers::SUSPEND));
    if !suspend {
        return;
    }
    let mut function = None;
    for a in call.ancestors().skip(1) {
        match a.kind() {
            SyntaxKind::LAMBDA_EXPR => match e.lambda_kind(&a) {
                None => return,
                Some((true, _)) => return,
                Some((false, true)) => continue,
                // A plain lambda: only when the function it is passed to is certain.
                Some((false, false)) if lambda_callee_unique(e, &a) => break,
                Some((false, false)) => return,
            },
            SyntaxKind::FUN => {
                if has_modifier(&a, "suspend") {
                    return;
                }
                function = Some(a);
                break;
            }
            SyntaxKind::ANONYMOUS_FUNCTION => return,
            SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::CLASS_INITIALIZER | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::CLASS_BODY | SyntaxKind::KOTLIN_FILE | SyntaxKind::SCRIPT_BODY => break,
            _ => {}
        }
    }
    let name = callee.text().to_string();
    let fix = function.as_ref().and_then(|f| {
        let kw = f.children_with_tokens().filter_map(|t| t.into_token()).find(|t| t.kind() == SyntaxKind::FUN_KW)?;
        let fname = ast::token(f, SyntaxKind::IDENT).map(|t| ast::ident_text(&t)).unwrap_or_default();
        Some(Fix::replace(format!("Make '{fname}' suspend"), TextRange::empty(kw.text_range().start()), "suspend "))
    });
    let msg = format!("Suspend function '{name}' should be called only from a coroutine or another suspend function.");
    out.report(&ILLEGAL_SUSPEND_FUNCTION_CALL, callee.text_range(), msg, fix);
}

/// A trailing lambda took the last parameter's place, so adding parameters after it
/// would not match the call.
fn trailing_lambda_extra(call: &SyntaxNode, sym: &Symbol) -> bool {
    call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) && !sym.params.is_empty()
}

/// The call a lambda is passed to has exactly one candidate.
fn lambda_callee_unique(e: &Engine, lambda: &SyntaxNode) -> bool {
    let call = lambda.ancestors().skip(1).find(|a| a.kind() == SyntaxKind::CALL_EXPR);
    let Some(callee) = call.and_then(|c| c.first_child()).filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return false };
    e.call_candidates(&callee).is_some_and(|c| c.len() == 1)
}

// ------------------------------------------------------------ call arguments

enum Mismatch {
    TooMany(TextRange),
    NamedNotFound(String, TextRange),
    Missing(String),
}

fn call_arguments(e: &Engine, call: &SyntaxNode, out: &mut Sink) {
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let list = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST);
    // `Array<T>::class` parses like a call without arguments.
    if list.is_none() && !call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) {
        return;
    }
    if in_unknown_lambda(e, call) || in_context_scope(call) {
        return;
    }
    // Member calls: only on receivers of certain type, or on classes/packages.
    if let Some(outer) = call.parent().filter(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR))
        && outer.first_child().as_ref() != Some(call)
    {
        let Some(recv) = outer.first_child() else { return };
        let qualified = matches!(e.resolve_qualifier(&recv), Qualifier::Static(_) | Qualifier::Package(_));
        if !qualified && certain_receiver(e, &recv).is_none() {
            return;
        }
    }
    let args: Vec<SyntaxNode> = list.iter().flat_map(|l| l.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).collect::<Vec<_>>()).collect();
    if args.iter().any(|a| ast::token(a, SyntaxKind::STAR).is_some()) {
        return;
    }
    let trailing_lambda = call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT);
    let named: Vec<(String, TextRange)> = args
        .iter()
        .filter_map(|a| a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME))
        .filter_map(|n| ast::token(&n, SyntaxKind::IDENT))
        .map(|t| (ast::ident_text(&t), t.text_range()))
        .collect();
    // Positional arguments keep their index (they may follow named ones in their own place).
    let positional: Vec<(usize, &SyntaxNode)> = args.iter().enumerate().filter(|(_, a)| !a.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME)).collect();
    let Some(candidates) = e.call_candidates(&callee) else { return };
    // Java parameter names are not known reliably: named arguments are not checked there.
    let java_like = |s: &Symbol| s.params.iter().any(|p| p.name.starts_with('p') && p.name[1..].parse::<u32>().is_ok() || p.name.starts_with("arg") && p.name[3..].parse::<u32>().is_ok());
    if !named.is_empty() && candidates.iter().any(|(s, _)| java_like(s)) {
        return;
    }
    let fit = |sym: &Symbol| -> Option<Mismatch> {
        let params = &sym.params;
        let mut assigned = vec![false; params.len()];
        // A vararg passed by name no longer absorbs positional arguments.
        let vararg = params.iter().position(|p| p.vararg).filter(|v| !named.iter().any(|(n, _)| *n == params[*v].name));
        for &(i, a) in &positional {
            match (vararg, i) {
                (Some(v), i) if i >= v => assigned[v] = true,
                (_, i) if i < params.len() => assigned[i] = true,
                _ => return Some(Mismatch::TooMany(a.text_range())),
            }
        }
        for (n, r) in &named {
            match params.iter().position(|p| p.name == *n) {
                Some(i) => assigned[i] = true,
                None => return Some(Mismatch::NamedNotFound(n.clone(), *r)),
            }
        }
        if trailing_lambda {
            match params.len().checked_sub(1) {
                Some(last) if !assigned[last] => assigned[last] = true,
                _ => return Some(Mismatch::TooMany(call.children().find(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT).unwrap().text_range())),
            }
        }
        // Overrides inherit default values from the overridden declaration; library
        // metadata does not say whether a member overrides.
        let library_member = !matches!(sym.origin, Origin::Source { .. }) && sym.fq_name.rsplit_once('.').is_some_and(|(owner, _)| e.classifier(owner).is_some());
        if sym.modifiers.has(Modifiers::OVERRIDE) || library_member {
            return None;
        }
        params.iter().zip(&assigned).find(|(p, a)| !**a && !p.has_default && !p.vararg).map(|(p, _)| Mismatch::Missing(p.name.clone()))
    };
    let results: Vec<(&Symbol, Option<Mismatch>)> = candidates.iter().map(|(s, _)| (s, fit(s))).collect();
    if results.iter().any(|(_, m)| m.is_none()) {
        // Argument types, when there is no overload to choose from.
        if let [(sym, subst)] = candidates.as_slice() {
            argument_types(e, sym, subst, &args, out);
            return;
        }
        // Several overloads: none accepts a certain argument's type.
        let fitting: Vec<&(Symbol, crate::analysis::ide::infer::Subst)> = candidates.iter().zip(&results).filter(|(_, (_, m))| m.is_none()).map(|(c, _)| c).collect();
        if !fitting.is_empty() && fitting.iter().all(|(s, sub)| !argument_mismatches(e, s, sub, &args).is_empty()) {
            let shown: Vec<String> = fitting.iter().take(3).map(|(s, _)| s.signature()).collect();
            let more = if fitting.len() > 3 { format!("\n... and {} more", fitting.len() - 3) } else { String::new() };
            out.report(&NONE_APPLICABLE, callee.text_range(), format!("None of the following candidates is applicable for '{}':\n{}{more}", callee.text(), shown.join("\n")), None);
        }
        return;
    }
    let name = callee.text().to_string();
    if results.len() == 1 {
        let (sym, m) = &results[0];
        match m.as_ref().unwrap() {
            Mismatch::TooMany(r) => {
                // The extra positional arguments become new parameters.
                let extra: Vec<SyntaxNode> = positional.iter().filter(|(i, _)| *i >= sym.params.len()).map(|(_, a)| (*a).clone()).collect();
                let fix = (!extra.is_empty() && !trailing_lambda_extra(call, sym)).then(|| crate::analysis::ide::signature_change::add_parameter_fix(e.snap, e, sym, &extra)).flatten();
                out.report(&TOO_MANY_ARGUMENTS, *r, format!("Too many arguments for '{}'.", sym.signature()), fix);
            }
            Mismatch::NamedNotFound(n, r) => {
                let arg = args.iter().find(|a| a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME).is_some_and(|c| c.text_range().contains_range(*r)));
                let fix = arg.and_then(|a| crate::analysis::ide::signature_change::add_parameter_fix(e.snap, e, sym, std::slice::from_ref(a)));
                out.report(&NAMED_PARAMETER_NOT_FOUND, *r, format!("No parameter with name '{n}' found."), fix);
            }
            Mismatch::Missing(p) => {
                let at = list.as_ref().and_then(|l| l.last_token()).map(|t| t.text_range()).unwrap_or(callee.text_range());
                // "Add 'p = TODO()'" before `)` (not with a trailing lambda in the way).
                let fix = list.as_ref().and_then(|l| l.last_token().filter(|t| t.kind() == SyntaxKind::R_PAREN).map(|close| (l, close))).map(|(l, close)| {
                    let sep = if l.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT) { ", " } else { "" };
                    Fix::replace(format!("Add '{p} = TODO()'"), TextRange::empty(close.text_range().start()), format!("{sep}{p} = TODO()"))
                });
                out.report(&NO_VALUE_FOR_PARAMETER, at, format!("No value passed for parameter '{p}'."), fix);
            }
        }
        return;
    }
    let shown: Vec<String> = results.iter().take(3).map(|(s, _)| s.signature()).collect();
    let more = if results.len() > 3 { format!("\n... and {} more", results.len() - 3) } else { String::new() };
    out.report(&NONE_APPLICABLE, callee.text_range(), format!("None of the following candidates is applicable for '{name}':\n{}{more}", shown.join("\n")), None);
}

/// Literal and certain-typed arguments of a call to the single candidate `sym`.
/// A certain argument whose type does not fit its parameter.
struct ArgMismatch {
    value: SyntaxNode,
    rule: &'static Rule,
    message: String,
    /// The type the parameter could change to.
    to: String,
    convert: Option<Fix>,
    param: String,
}

/// The literal and certain-typed arguments of a call to `sym` that do not fit.
fn argument_mismatches(e: &Engine, sym: &Symbol, subst: &crate::analysis::ide::infer::Subst, args: &[SyntaxNode]) -> Vec<ArgMismatch> {
    let mut out = Vec::new();
    // The function's own type parameters shadow the class's (`fun <T> f(v: T)` in `C<T>`).
    let mut subst = subst.clone();
    for tp in &sym.type_params {
        subst.remove(&tp.name);
    }
    for (index, a) in args.iter().enumerate() {
        let name = a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME).and_then(|n| ast::token(&n, SyntaxKind::IDENT)).map(|t| ast::ident_text(&t));
        // A positional argument takes the parameter at its own index (named ones before it
        // must be in their own positions).
        let param = match &name {
            Some(n) => sym.params.iter().find(|p| p.name == *n),
            None => sym.params.get(index),
        };
        let Some(param) = param.filter(|p| !p.vararg) else {
            // Positions after a vararg are ambiguous.
            if name.is_none() {
                return out;
            }
            continue;
        };
        let Some(value) = a.children().filter(|c| c.kind() != SyntaxKind::VALUE_ARGUMENT_NAME).last() else { continue };
        let t = crate::analysis::ide::infer::substitute(&e.resolve_declared(sym, &param.ty), &subst);
        // Java parameters (platform types) accept null; generic ones need inference.
        if matches!(t, TypeRef::Platform(_)) || has_type_param(&t) {
            continue;
        }
        if let Some((rule, message, to, convert)) = type_mismatch(e, &value, &t, &t.display()) {
            let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &ARGUMENT_TYPE_MISMATCH } else { rule };
            out.push(ArgMismatch { value, rule, message, to, convert, param: param.name.clone() });
        }
    }
    out
}

/// Literal and certain-typed arguments of a call to the single candidate `sym`.
fn argument_types(e: &Engine, sym: &Symbol, subst: &crate::analysis::ide::infer::Subst, args: &[SyntaxNode], out: &mut Sink) {
    for m in argument_mismatches(e, sym, subst, args) {
        let change = parameter_type_fix(e, sym, &m.param, &m.to);
        out.report_fixes(m.rule, m.value.text_range(), m.message, m.convert.into_iter().chain(change).collect());
    }
}

/// "Change parameter 'p' type to 'T'" on a project function (possibly in another file).
fn parameter_type_fix(e: &Engine, sym: &Symbol, param: &str, to: &str) -> Option<Fix> {
    let Origin::Source { file, name_range, .. } = &sym.origin else { return None };
    let root = e.snap.file(file)?.parse().syntax();
    let decl = crate::syntax::covering(&root, *name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::CLASS | SyntaxKind::SECONDARY_CONSTRUCTOR))?;
    let p = decl
        .descendants()
        .filter(|d| d.kind() == SyntaxKind::VALUE_PARAMETER)
        .find(|d| d.ancestors().skip(2).find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::CLASS | SyntaxKind::SECONDARY_CONSTRUCTOR)).as_ref() == Some(&decl) && ast::token(d, SyntaxKind::IDENT).is_some_and(|t| ast::ident_text(&t) == param))?;
    let ty = ast::child::<ast::TypeRef>(&p)?;
    let mut fix = Fix::new(format!("Change parameter '{param}' type to '{to}'"), Vec::new());
    let edit = Edit { range: ty.syntax().text_range(), text: to.to_string() };
    if file.as_path() == e.path.as_path() {
        fix.edits.push(edit);
    } else {
        fix.elsewhere.push((file.to_path_buf(), edit));
    }
    Some(fix)
}

fn has_type_param(t: &TypeRef) -> bool {
    match t {
        TypeRef::TypeParam { .. } | TypeRef::Error | TypeRef::Dynamic => true,
        TypeRef::Named { fq, args, .. } => fq.is_none() || args.iter().any(|a| matches!(a, crate::symbols::TypeArg::Type { ty, .. } if has_type_param(ty))),
        TypeRef::Function { receiver, params, ret, .. } => receiver.as_deref().is_some_and(has_type_param) || params.iter().any(has_type_param) || has_type_param(ret),
        TypeRef::Platform(inner) => has_type_param(inner),
    }
}

// ------------------------------------------------------------ literal types

/// The type of a literal expression (`1` → `kotlin.Int`, `-1L`, `"a"`, `null` → `None`).
pub(super) fn literal_type(n: &SyntaxNode) -> Option<Option<&'static str>> {
    let lit = match n.kind() {
        SyntaxKind::LITERAL => n.clone(),
        SyntaxKind::PREFIX_EXPR if binary_op(n).is_some_and(|o| o.kind() == SyntaxKind::MINUS) => {
            let inner = n.children().find(|c| c.kind() == SyntaxKind::LITERAL)?;
            if !inner.first_token().is_some_and(|t| matches!(t.kind(), SyntaxKind::INT_LITERAL | SyntaxKind::FLOAT_LITERAL)) {
                return None;
            }
            inner
        }
        SyntaxKind::STRING_TEMPLATE => return Some(Some("kotlin.String")),
        _ => return None,
    };
    let t = lit.first_token()?;
    let text = t.text().to_ascii_lowercase();
    Some(match t.kind() {
        SyntaxKind::NULL_KW => None,
        SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW => Some("kotlin.Boolean"),
        SyntaxKind::CHAR_LITERAL => Some("kotlin.Char"),
        SyntaxKind::FLOAT_LITERAL if text.ends_with('f') => Some("kotlin.Float"),
        SyntaxKind::FLOAT_LITERAL => Some("kotlin.Double"),
        SyntaxKind::INT_LITERAL if text.ends_with("ul") => Some("kotlin.ULong"),
        SyntaxKind::INT_LITERAL if text.ends_with('u') => Some("kotlin.UInt"),
        SyntaxKind::INT_LITERAL if text.ends_with('l') => Some("kotlin.Long"),
        SyntaxKind::INT_LITERAL => Some("kotlin.Int"),
        _ => return None,
    })
}

/// A literal, or a value of certain type (see `certain::certain_type`), where type `t` (written
/// `shown`) is expected: the rule, message and the type the declaration could change to.
pub(super) fn type_mismatch(e: &Engine, value: &SyntaxNode, t: &TypeRef, shown: &str) -> Option<(&'static Rule, String, String, Option<Fix>)> {
    mismatch_of(e, value, t, shown, false)
}

/// Like [`type_mismatch`] against a written (not substituted) type, also comparing type
/// arguments of variables and properties (calls infer theirs from the expected type).
pub(super) fn declared_type_mismatch(e: &Engine, value: &SyntaxNode, t: &TypeRef, shown: &str) -> Option<(&'static Rule, String, String, Option<Fix>)> {
    mismatch_of(e, value, t, shown, true)
}

fn mismatch_of(e: &Engine, value: &SyntaxNode, t: &TypeRef, shown: &str, check_arguments: bool) -> Option<(&'static Rule, String, String, Option<Fix>)> {
    let (fq, nullable) = match t {
        TypeRef::Named { fq: Some(fq), nullable, .. } => (fq.to_string(), *nullable),
        TypeRef::Function { nullable, .. } => ("kotlin.Function".to_string(), *nullable),
        _ => return None,
    };
    let c = e.res.find_classifier(&fq);
    if fq != "kotlin.Function" && c.as_ref().is_none_or(|c| c.symbol().kind == SymbolKind::TypeAlias) {
        return None;
    }
    // Java classes: only `null` and nullable values of the same class.
    if matches!(c, Some(crate::analysis::resolve::Classifier::Library(ref lc)) if !lc.is_kotlin) {
        return java_null_mismatch(e, value, &fq, nullable, shown);
    }
    let Some(lit) = literal_type(value) else {
        if fq == "kotlin.Function" {
            return None;
        }
        if !nullable && let Some(actual) = nullable_safe_call(e, value).filter(|a| e.conforms(a, &fq) == Some(true)) {
            let actual_shown = super::certain::shown(&actual);
            return Some((&INITIALIZER_TYPE_MISMATCH, format!("Type mismatch: inferred type is '{actual_shown}' but '{shown}' was expected."), actual_shown, None));
        }
        // A nullable property chain (`u.nick`) not checked earlier.
        if !nullable
            && value.kind() == SyntaxKind::DOT_QUALIFIED_EXPR
            && let Some(shown_actual) = nullable_property_chain(e, value)
            && e.type_of(value).is_some_and(|t| fq_of(&t).is_some_and(|a| e.conforms(&TypeRef::named(a), &fq) == Some(true)))
        {
            let fix = Fix::replace("Add non-null asserted (!!) call", TextRange::empty(value.text_range().end()), "!!");
            return Some((&INITIALIZER_TYPE_MISMATCH, format!("Type mismatch: inferred type is '{shown_actual}' but '{shown}' was expected."), shown_actual, Some(fix)));
        }
        let actual = super::certain::certain_type(e, value, false)?;
        // A nullable value where a non-null one is expected (locals: unless narrowed in a
        // way the engine does not model).
        let null_mismatch = !nullable
            && matches!(&actual, TypeRef::Named { nullable: true, fq: Some(f), .. } if &**f != "kotlin.Nothing")
            && (value.kind() != SyntaxKind::NAME_REF || !matches!(e.resolve_name_ref(value).as_slice(), [Def::Local(_)]) || nullable_local(e, value).is_some());
        let names_only = value.descendants_with_tokens().all(|d| matches!(d.kind(), SyntaxKind::NAME_REF | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::IDENT | SyntaxKind::DOT | SyntaxKind::THIS_EXPR | SyntaxKind::THIS_KW) || d.kind().is_trivia());
        let arguments = check_arguments && names_only && arguments_conflict(e, &actual, t);
        if e.conforms(&actual, &fq)? && !null_mismatch && !arguments {
            return None;
        }
        let actual_shown = super::certain::shown(&actual);
        if null_mismatch && e.conforms(&actual, &fq) == Some(true) {
            let fix = Fix::replace("Add non-null asserted (!!) call", TextRange::empty(value.text_range().end()), "!!");
            return Some((&INITIALIZER_TYPE_MISMATCH, format!("Type mismatch: inferred type is '{actual_shown}' but '{shown}' was expected."), actual_shown, Some(fix)));
        }
        let to = if nullable && !actual_shown.ends_with('?') { format!("{actual_shown}?") } else { actual_shown.clone() };
        // A cast when the expected type is a subtype (`any as String`: the value may already
        // be one), else a conversion call.
        let cast = fq_of(&actual).filter(|a| e.conforms(&TypeRef::named(&fq), a) == Some(true)).map(|_| Fix::replace(format!("Cast to '{shown}'"), value.text_range(), format!("{} as {shown}", operand(value))));
        let convert = cast.or_else(|| actual.fq_name().and_then(|a| conversion(value, a, &fq, false)));
        return Some((&INITIALIZER_TYPE_MISMATCH, format!("Type mismatch: expected '{shown}', actual '{actual_shown}'."), to, convert));
    };
    let Some(lit) = lit else {
        let to = if matches!(t, TypeRef::Function { .. }) { format!("({shown})?") } else { format!("{shown}?") };
        return (!nullable).then(|| (&NULL_FOR_NONNULL_TYPE, format!("Null cannot be a value of a non-null type '{shown}'."), to, None));
    };
    // Integer literals adapt to any integer type; others need a supertype match.
    if lit == "kotlin.Int" && matches!(fq.as_str(), "kotlin.Long" | "kotlin.Short" | "kotlin.Byte") {
        return None;
    }
    if matches!(lit, "kotlin.UInt") && matches!(fq.as_str(), "kotlin.ULong" | "kotlin.UShort" | "kotlin.UByte") {
        return None;
    }
    let closure = e.supertype_closure(&TypeRef::named(lit));
    if closure.iter().any(|s| fq_of(s) == Some(fq.as_str())) || fq == "kotlin.Any" {
        return None;
    }
    let actual = lit.trim_start_matches("kotlin.");
    let to = if nullable { format!("{actual}?") } else { actual.to_string() };
    let convert = conversion(value, lit, &fq, true);
    Some((&INITIALIZER_TYPE_MISMATCH, format!("Type mismatch: expected '{shown}', actual '{actual}'."), to, convert))
}

/// `null` or a certainly nullable value of the class itself where a non-null Java class is
/// expected (its subtypes are not known well enough to judge other values).
fn java_null_mismatch(e: &Engine, value: &SyntaxNode, fq: &str, nullable: bool, shown: &str) -> Option<(&'static Rule, String, String, Option<Fix>)> {
    if nullable {
        return None;
    }
    if literal_type(value) == Some(None) {
        return Some((&NULL_FOR_NONNULL_TYPE, format!("Null cannot be a value of a non-null type '{shown}'."), format!("{shown}?"), None));
    }
    let actual = super::certain::certain_type(e, value, false).filter(|a| matches!(a, TypeRef::Named { nullable: true, .. }) && fq_of(a) == Some(fq)).or_else(|| nullable_safe_call(e, value).filter(|a| fq_of(a) == Some(fq)))?;
    let actual_shown = super::certain::shown(&actual);
    let fix = (value.kind() != SyntaxKind::SAFE_QUALIFIED_EXPR).then(|| Fix::replace("Add non-null asserted (!!) call", TextRange::empty(value.text_range().end()), "!!"));
    Some((&INITIALIZER_TYPE_MISMATCH, format!("Type mismatch: inferred type is '{actual_shown}' but '{shown}' was expected."), actual_shown, fix))
}

/// `x?.let { … }` on a certainly nullable `x` whose result the certain types do not cover
/// (a generic lambda result): nullable, of the class the engine infers.
fn nullable_safe_call(e: &Engine, value: &SyntaxNode) -> Option<TypeRef> {
    if value.kind() != SyntaxKind::SAFE_QUALIFIED_EXPR || super::certain::certain_type(e, value, false).is_some() {
        return None;
    }
    let recv = super::certain::certain_type(e, &value.first_child()?, true)?;
    if !matches!(recv, TypeRef::Named { nullable: true, ref fq, .. } if fq.as_deref() != Some("kotlin.Nothing")) {
        return None;
    }
    let TypeRef::Named { name, fq: Some(fq), args, .. } = crate::analysis::ide::infer::strip(&e.type_of(value)?).clone() else { return None };
    (&*fq != "kotlin.Nothing" && &*fq != "kotlin.Unit").then_some(TypeRef::Named { name, fq: Some(fq), args, nullable: true })
}

/// `List<User>` where `List<String>` is expected: type arguments of the same class that do
/// not fit its parameters' variance (`out`: subtypes, invariant: the same class, `in`:
/// supertypes). Unknown classes and type parameters never conflict.
fn arguments_conflict(e: &Engine, actual: &TypeRef, expected: &TypeRef) -> bool {
    use crate::symbols::{TypeArg, Variance};
    let (TypeRef::Named { fq: Some(a), args: aa, .. }, TypeRef::Named { fq: Some(x), args: xa, .. }) = (strip(actual), strip(expected)) else { return false };
    if a != x || aa.len() != xa.len() || aa.is_empty() {
        return false;
    }
    let Some(c) = e.classifier(a) else { return false };
    let declared: Vec<Variance> = c.symbol().type_params.iter().map(|p| p.variance).collect();
    if declared.len() != aa.len() {
        return false;
    }
    for ((arg, exp), decl) in aa.iter().zip(xa).zip(declared) {
        let (TypeArg::Type { ty: at, .. }, TypeArg::Type { variance: use_site, ty: xt }) = (arg, exp) else { continue };
        if matches!(at, TypeRef::Platform(_)) || matches!(xt, TypeRef::Platform(_)) {
            continue;
        }
        let (TypeRef::Named { fq: Some(af), nullable: an, .. }, TypeRef::Named { fq: Some(xf), nullable: xn, .. }) = (at, xt) else { continue };
        let variance = if *use_site != Variance::Invariant { *use_site } else { decl };
        let conflict = match variance {
            Variance::Out => (*an && !*xn) || e.conforms(&TypeRef::named(af), xf) == Some(false),
            Variance::In => (*xn && !*an) || e.conforms(&TypeRef::named(xf), af) == Some(false),
            Variance::Invariant => {
                // Equal classes only (both known); nested arguments are not compared.
                an != xn || af != xf && e.conforms(&TypeRef::named(af), xf).is_some() && e.conforms(&TypeRef::named(xf), af).is_some()
            }
        };
        if conflict {
            return true;
        }
    }
    false
}

/// "Convert to 'Int'": a conversion call (`.toInt()`, `.toString()`, `.code`), or for a
/// literal its other spelling (`"1"`, `1.0`, `1L`, `2.5f`).
fn conversion(value: &SyntaxNode, actual: &str, expected: &str, literal: bool) -> Option<Fix> {
    const NUMBERS: &[&str] = &["kotlin.Int", "kotlin.Long", "kotlin.Short", "kotlin.Byte", "kotlin.Double", "kotlin.Float"];
    let target = expected.strip_prefix("kotlin.")?;
    let text = value.text().to_string();
    let int_literal = literal && actual == "kotlin.Int" && text.chars().all(|c| c.is_ascii_digit() || c == '_' || c == '-');
    let new = match (actual, expected) {
        _ if literal && expected == "kotlin.String" && actual != "kotlin.Char" => format!("\"{text}\""),
        ("kotlin.Int", "kotlin.Double") if int_literal => format!("{text}.0"),
        ("kotlin.Int", "kotlin.Float") if int_literal => format!("{text}f"),
        ("kotlin.Double", "kotlin.Float") if literal && !text.ends_with('f') => format!("{text}f"),
        ("kotlin.Char", "kotlin.Int") => format!("{}.code", receiver_text(value, &text)),
        (_, "kotlin.String") => format!("{}.toString()", receiver_text(value, &text)),
        (a, e) if (NUMBERS.contains(&a) || a == "kotlin.String") && NUMBERS.contains(&e) => format!("{}.to{target}()", receiver_text(value, &text)),
        _ => return None,
    };
    Some(Fix::replace(format!("Convert to '{target}'"), value.text_range(), new))
}

/// `text` as a call receiver: parenthesized unless it is a simple expression.
fn receiver_text(value: &SyntaxNode, text: &str) -> String {
    let simple = matches!(value.kind(), SyntaxKind::NAME_REF | SyntaxKind::CALL_EXPR | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR | SyntaxKind::PAREN_EXPR | SyntaxKind::LITERAL | SyntaxKind::STRING_TEMPLATE);
    if simple { text.to_string() } else { format!("({text})") }
}

/// The expressions that make up the value of `v`: the branch results of an `if` with
/// `else`, a `when`, a `try` and its `catch`es (jumps left out), through parentheses.
pub(super) fn branch_leaves(v: &SyntaxNode) -> Vec<SyntaxNode> {
    let last = |b: &SyntaxNode| -> Option<SyntaxNode> {
        let e = b.first_child()?;
        if e.kind() == SyntaxKind::BLOCK { e.children().last() } else { Some(e) }
    };
    let block_last = |b: &SyntaxNode| b.children().find(|c| c.kind() == SyntaxKind::BLOCK).and_then(|blk| blk.children().last());
    let mut out = Vec::new();
    match v.kind() {
        SyntaxKind::PAREN_EXPR => {
            if let Some(i) = v.children().next() {
                out.extend(branch_leaves(&i));
            }
        }
        SyntaxKind::IF_EXPR => {
            if !v.children().any(|c| c.kind() == SyntaxKind::ELSE_BRANCH) {
                return out;
            }
            for b in v.children().filter(|c| matches!(c.kind(), SyntaxKind::THEN_BRANCH | SyntaxKind::ELSE_BRANCH)) {
                out.extend(last(&b).map(|l| branch_leaves(&l)).unwrap_or_default());
            }
        }
        SyntaxKind::WHEN_EXPR => {
            for b in v.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY).filter_map(|en| en.children().find(|c| c.kind() == SyntaxKind::BODY)) {
                out.extend(last(&b).map(|l| branch_leaves(&l)).unwrap_or_default());
            }
        }
        SyntaxKind::TRY_EXPR => {
            out.extend(block_last(v).map(|l| branch_leaves(&l)).unwrap_or_default());
            for c in v.children().filter(|c| c.kind() == SyntaxKind::CATCH_CLAUSE) {
                out.extend(block_last(&c).map(|l| branch_leaves(&l)).unwrap_or_default());
            }
        }
        SyntaxKind::RETURN_EXPR | SyntaxKind::THROW_EXPR | SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR => {}
        // `a ?: b`: the fallback (the left side is non-null when taken).
        SyntaxKind::BINARY_EXPR if binary_op(v).is_some_and(|o| o.kind() == SyntaxKind::ELVIS) => {
            if let Some((_, r)) = binary_operands(v) {
                out.extend(branch_leaves(&r));
            }
        }
        _ => out.push(v.clone()),
    }
    out
}

/// A value of the wrong type where `expected` is declared, with fixes changing the
/// declared type (`what`: "type" / "return type") or converting the value.
fn mismatch(e: &Engine, value: &SyntaxNode, expected: &ast::TypeRef, what: &str) -> Option<(&'static Rule, String, Vec<Fix>)> {
    let t = e.resolve_syntax_type(expected);
    let (rule, msg, to, convert) = declared_type_mismatch(e, value, &t, &expected.syntax().text().to_string())?;
    let change = Fix::replace(format!("Change {what} to '{to}'"), expected.syntax().text_range(), to);
    Some((rule, msg, std::iter::once(change).chain(convert).collect()))
}

fn literal_initializer(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    // A vararg's default value is an array of the written type.
    if has_modifier(n, "vararg") {
        return;
    }
    let Some(ty) = ast::child::<ast::TypeRef>(n) else { return };
    let Some(value) = initializer(n) else { return };
    if value.kind() == SyntaxKind::LAMBDA_EXPR {
        lambda_result(e, &value, &ty, out);
        return;
    }
    for value in branch_leaves(&value) {
        if let Some((rule, msg, fixes)) = mismatch(e, &value, &ty, "type") {
            out.report_fixes(rule, value.text_range(), msg, fixes);
        }
    }
}

/// `val f: () -> Int = { "x" }`: the lambda's last expression against the declared result.
fn lambda_result(e: &Engine, lambda: &SyntaxNode, declared: &ast::TypeRef, out: &mut Sink) {
    let Some(function) = declared.syntax().children().find(|c| c.kind() == SyntaxKind::FUNCTION_TYPE) else { return };
    let Some(result) = function.children().find_map(ast::TypeRef::cast) else { return };
    let t = e.resolve_syntax_type(&result);
    if matches!(fq_of(&t), Some("kotlin.Unit") | None) {
        return;
    }
    let Some(last) = lambda.children().find(|c| c.kind() == SyntaxKind::BLOCK).and_then(|b| b.children().last()) else { return };
    for v in branch_leaves(&last) {
        if let Some((rule, msg, _, convert)) = type_mismatch(e, &v, &t, &result.syntax().text().to_string()) {
            let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &RETURN_TYPE_MISMATCH } else { rule };
            out.report_fixes(rule, v.text_range(), msg, convert.into_iter().collect());
        }
    }
}

fn literal_return(e: &Engine, ret: &SyntaxNode, out: &mut Sink) {
    if ret.children().any(|c| matches!(c.kind(), SyntaxKind::LABEL | SyntaxKind::LABEL_QUALIFIER)) {
        return;
    }
    let Some(value) = ret.children().last() else { return };
    let Some(f) = ret.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::LAMBDA_EXPR | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::PROPERTY_ACCESSOR)) else { return };
    if f.kind() != SyntaxKind::FUN {
        return;
    }
    let Some(ty) = ast::FunDecl::cast(f).and_then(|d| d.return_type()) else { return };
    for value in branch_leaves(&value) {
        if let Some((rule, msg, fixes)) = mismatch(e, &value, &ty, "return type") {
            let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &RETURN_TYPE_MISMATCH } else { rule };
            out.report_fixes(rule, value.text_range(), msg, fixes);
        }
    }
}

fn literal_expression_body(e: &Engine, f: &SyntaxNode, out: &mut Sink) {
    let decl = ast::FunDecl::cast(f.clone()).unwrap();
    let (Some(ty), true) = (decl.return_type(), decl.has_expression_body()) else { return };
    let Some(value) = initializer(f) else { return };
    for value in branch_leaves(&value) {
        if let Some((rule, msg, fixes)) = mismatch(e, &value, &ty, "return type") {
            let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &RETURN_TYPE_MISMATCH } else { rule };
            out.report_fixes(rule, value.text_range(), msg, fixes);
        }
    }
}
