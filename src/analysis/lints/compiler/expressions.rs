//! Expression-level compiler checks (need types): unresolved names, `val` reassignment,
//! null-safety warnings, deprecation, call arguments, literal type mismatches.

use std::collections::{HashMap, HashSet};

use super::super::*;
use crate::analysis::diagnostics::{LocalsCache, scope_at};
use crate::analysis::ide::infer::{Def, Engine, LocalKind, Qualifier, fq_of};
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
            SyntaxKind::USER_TYPE => deprecated_type(e, &n, out),
            SyntaxKind::BINARY_EXPR => {
                assignment(cx, e, &n, out);
                null_checks(e, &n, out);
            }
            SyntaxKind::PREFIX_EXPR | SyntaxKind::POSTFIX_EXPR => {
                increment(cx, e, &n, out);
                not_null_assertion(e, &n, out);
            }
            SyntaxKind::SAFE_QUALIFIED_EXPR => safe_call(e, &n, out),
            SyntaxKind::CALL_EXPR => call_arguments(e, &n, out),
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
    // Deprecation: every candidate is deprecated.
    if !defs.is_empty()
        && defs.iter().all(|d| matches!(d, Def::Symbol { sym, .. } if sym.modifiers.has(Modifiers::DEPRECATED)))
        && !in_deprecated_declaration(nr)
    {
        out.report(&DEPRECATION, tok.text_range(), format!("'{name}' is deprecated."), None);
    }
    if !defs.is_empty() || !cx.on(&UNRESOLVED_REFERENCE_EXPR) || in_unknown_lambda(e, nr) || in_context_scope(nr) {
        return;
    }
    if member {
        let Some(recv) = outer.first_child() else { return };
        if smart_cast_possible(nr) || st.exists(cx, e, &name) {
            return;
        }
        // Only receivers whose type is written down (not inferred).
        let Some(t) = declared_type(e, &recv) else { return };
        let Some(fq) = fq_of(&t) else { return };
        if matches!(fq, "kotlin.Any" | "kotlin.Nothing") || e.classifier(fq).is_none() || jvm_members(e, &t).is_none_or(|m| m.contains(&name)) {
            return;
        }
        let Some(inherited) = e.supertype_member_names(&t) else { return };
        if inherited.contains(&name) || e.all_members(&t).iter().any(|(m, _)| m.name == name) {
            return;
        }
        out.report(&UNRESOLVED_REFERENCE_EXPR, tok.text_range(), format!("Unresolved reference '{name}'."), None);
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
            for (m, _) in e.all_members(&crate::analysis::ide::infer::class_type(c.symbol())) {
                out.insert(m.name);
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

fn deprecated_type(e: &Engine, ut: &SyntaxNode, out: &mut Sink) {
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
        && c.symbol().modifiers.has(Modifiers::DEPRECATED)
        && !in_deprecated_declaration(ut)
    {
        let (n, r) = segs.last().unwrap();
        out.report(&DEPRECATION, *r, format!("'{n}' is deprecated."), None);
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

/// The written type of a variable or property used as `expr`, or of `this`.
fn declared_type(e: &Engine, expr: &SyntaxNode) -> Option<TypeRef> {
    if expr.kind() == SyntaxKind::THIS_EXPR {
        return (expr.text() == "this").then(|| e.type_of(expr)).flatten();
    }
    if expr.kind() != SyntaxKind::NAME_REF {
        return None;
    }
    let defs = e.resolve_name_ref(expr);
    let [def] = defs.as_slice() else { return None };
    let (file, range) = match def {
        Def::Local(l) if matches!(l.kind, LocalKind::Val | LocalKind::Var | LocalKind::Parameter) => (e.path.clone(), l.name_range),
        Def::Symbol { sym, .. } if sym.kind == SymbolKind::Property => match &sym.origin {
            Origin::Source { file, name_range, .. } => (file.clone(), *name_range),
            _ => return None,
        },
        _ => return None,
    };
    if file != e.path {
        return None;
    }
    let decl = crate::syntax::covering(&e.root, range)?.ancestors().find(|a| matches!(a.kind(), SyntaxKind::PROPERTY | SyntaxKind::VALUE_PARAMETER))?;
    let ty = ast::child::<ast::TypeRef>(&decl)?;
    let t = e.resolve_syntax_type(&ty);
    matches!(t, TypeRef::Named { fq: Some(_), .. }).then_some(t)
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
    // Member calls: only on receivers whose type is written down, or on classes/packages
    // (inferred types of call chains are not precise enough to judge overloads).
    if let Some(outer) = call.parent().filter(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR))
        && outer.first_child().as_ref() != Some(call)
    {
        let Some(recv) = outer.first_child() else { return };
        let qualified = matches!(e.resolve_qualifier(&recv), Qualifier::Static(_) | Qualifier::Package(_));
        if !qualified && declared_type(e, &recv).is_none() {
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
        return;
    }
    let name = callee.text().to_string();
    if results.len() == 1 {
        let (sym, m) = &results[0];
        match m.as_ref().unwrap() {
            Mismatch::TooMany(r) => out.report(&TOO_MANY_ARGUMENTS, *r, format!("Too many arguments for '{}'.", sym.signature()), None),
            Mismatch::NamedNotFound(n, r) => out.report(&NAMED_PARAMETER_NOT_FOUND, *r, format!("No parameter with name '{n}' found."), None),
            Mismatch::Missing(p) => {
                let at = list.as_ref().and_then(|l| l.last_token()).map(|t| t.text_range()).unwrap_or(callee.text_range());
                out.report(&NO_VALUE_FOR_PARAMETER, at, format!("No value passed for parameter '{p}'."), None);
            }
        }
        return;
    }
    let shown: Vec<String> = results.iter().take(3).map(|(s, _)| s.signature()).collect();
    let more = if results.len() > 3 { format!("\n... and {} more", results.len() - 3) } else { String::new() };
    out.report(&NONE_APPLICABLE, callee.text_range(), format!("None of the following candidates is applicable for '{name}':\n{}{more}", shown.join("\n")), None);
}

// ------------------------------------------------------------ literal types

/// The type of a literal expression (`1` → `kotlin.Int`, `-1L`, `"a"`, `null` → `None`).
fn literal_type(n: &SyntaxNode) -> Option<Option<&'static str>> {
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

/// A literal of type `lit` (or `null`) where `expected` is declared: the mismatch message.
fn literal_mismatch(e: &Engine, value: &SyntaxNode, expected: &ast::TypeRef) -> Option<(&'static Rule, String)> {
    let lit = literal_type(value)?;
    let t = e.resolve_syntax_type(expected);
    let shown = expected.syntax().text().to_string();
    let (fq, nullable) = match &t {
        TypeRef::Named { fq: Some(fq), nullable, .. } => (fq.to_string(), *nullable),
        TypeRef::Function { nullable, .. } => ("kotlin.Function".to_string(), *nullable),
        _ => return None,
    };
    let c = e.res.find_classifier(&fq);
    if fq != "kotlin.Function" && c.as_ref().is_none_or(|c| c.symbol().kind == SymbolKind::TypeAlias) {
        return None;
    }
    let Some(lit) = lit else {
        return (!nullable).then(|| (&NULL_FOR_NONNULL_TYPE, format!("Null cannot be a value of a non-null type '{shown}'.")));
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
    Some((&INITIALIZER_TYPE_MISMATCH, format!("Type mismatch: expected '{shown}', actual '{actual}'.")))
}

fn literal_initializer(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(ty) = ast::child::<ast::TypeRef>(n) else { return };
    let Some(value) = initializer(n) else { return };
    if let Some((rule, msg)) = literal_mismatch(e, &value, &ty) {
        out.report(rule, value.text_range(), msg, None);
    }
}

fn literal_return(e: &Engine, ret: &SyntaxNode, out: &mut Sink) {
    if ret.children().any(|c| c.kind() == SyntaxKind::LABEL) {
        return;
    }
    let Some(value) = ret.children().last() else { return };
    let Some(f) = ret.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::LAMBDA_EXPR | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::PROPERTY_ACCESSOR)) else { return };
    if f.kind() != SyntaxKind::FUN {
        return;
    }
    let Some(ty) = ast::FunDecl::cast(f).and_then(|d| d.return_type()) else { return };
    if let Some((rule, msg)) = literal_mismatch(e, &value, &ty) {
        let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &RETURN_TYPE_MISMATCH } else { rule };
        out.report(rule, value.text_range(), msg, None);
    }
}

fn literal_expression_body(e: &Engine, f: &SyntaxNode, out: &mut Sink) {
    let decl = ast::FunDecl::cast(f.clone()).unwrap();
    let (Some(ty), true) = (decl.return_type(), decl.has_expression_body()) else { return };
    let Some(value) = initializer(f) else { return };
    if let Some((rule, msg)) = literal_mismatch(e, &value, &ty) {
        let rule = if std::ptr::eq(rule, &INITIALIZER_TYPE_MISMATCH) { &RETURN_TYPE_MISMATCH } else { rule };
        out.report(rule, value.text_range(), msg, None);
    }
}
