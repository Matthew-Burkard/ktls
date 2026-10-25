//! Certain types: the type of an expression when every step to it is unambiguous, for
//! checks that must not guess.
//!
//! Literals, locals and parameters (declared with a type, or initialized with a certain
//! value), properties with a written type, and calls and member chains on certain
//! receivers whose candidates all return the same type. A call's own type parameters are
//! inferred from the certain types of its arguments and from what its lambdas return
//! (`listOf(user)`, `x.let { it.name }`, `list.map { it.id }`); lambda parameters get
//! their types from the call. Integer literals bind nothing (Kotlin may type them from the
//! expected type), and conflicting bindings leave a parameter unknown. A result that is
//! still a type parameter of the called function is not certain; type arguments that
//! stay unknown are kept as error types (shown `?`): the class and nullability still serve
//! member lookups and null checks, and reading through them (`List<?>.get`) is not certain.

use std::collections::{HashMap, HashSet};

use super::super::*;
use super::expressions::{literal_type, uncertain_implicit_member};
use crate::analysis::ide::infer::{Def, Engine, LocalKind, Qualifier, Subst, class_type, fq_of, strip, substitute};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeArg, TypeRef, Variance};

/// The certain type of `x`. `smart`: smart casts the engine applies (`is` checks) are
/// trusted; otherwise names checked with `is` are not certain.
pub(crate) fn certain_type(e: &Engine, x: &SyntaxNode, smart: bool) -> Option<TypeRef> {
    certain(e, x, smart, 0)
}

fn certain(e: &Engine, x: &SyntaxNode, smart: bool, depth: u32) -> Option<TypeRef> {
    if depth > 8 {
        return None;
    }
    if let Some(lit) = literal_type(x) {
        return Some(match lit {
            Some(fq) => TypeRef::named(fq),
            None => TypeRef::named("kotlin.Nothing").nullable(true),
        });
    }
    match x.kind() {
        SyntaxKind::PAREN_EXPR => certain(e, &x.children().last()?, smart, depth + 1),
        SyntaxKind::THIS_EXPR if x.text() == "this" && !in_unknown_lambda(e, x) && !this_checked(x) => e.type_of(x).and_then(finish),
        SyntaxKind::NAME_REF => name(e, x, smart, depth),
        SyntaxKind::CALL_EXPR => {
            let (cands, projected) = call_candidates(e, x, smart, depth)?;
            same_result(e, cands.iter().map(|(m, s)| return_type(e, m, s, Some(x), projected, smart, depth)))
        }
        // Operators whose value is always a Boolean.
        SyntaxKind::IS_EXPR => Some(TypeRef::named("kotlin.Boolean")),
        SyntaxKind::BINARY_EXPR if binary_op(x).is_some_and(|o| matches!(o.kind(), SyntaxKind::EQ2 | SyntaxKind::NOT_EQ | SyntaxKind::EQ3 | SyntaxKind::NOT_EQ2 | SyntaxKind::LT | SyntaxKind::GT | SyntaxKind::LT_EQ | SyntaxKind::GT_EQ | SyntaxKind::AMP2 | SyntaxKind::PIPE2 | SyntaxKind::IN_KW | SyntaxKind::NOT_IN_KW)) => Some(TypeRef::named("kotlin.Boolean")),
        SyntaxKind::PREFIX_EXPR if binary_op(x).is_some_and(|o| o.kind() == SyntaxKind::EXCL) => Some(TypeRef::named("kotlin.Boolean")),
        SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR => {
            let (recv, sel) = (x.first_child()?, x.last_child()?);
            if recv == sel {
                return None;
            }
            let safe = x.kind() == SyntaxKind::SAFE_QUALIFIED_EXPR;
            let t = match certain(e, &recv, smart, depth + 1) {
                // Members and extensions of the certain receiver type, chosen here (not
                // through the engine's possibly guessed receiver type).
                Some(rt) => {
                    let rt = if safe { non_null(rt) } else { rt };
                    match sel.kind() {
                        SyntaxKind::NAME_REF => {
                            let cands = select_candidates(e, &sel, &rt, None)?;
                            if cands.iter().any(|(m, _)| smart_castable(&sel, m)) {
                                return None;
                            }
                            same_result(e, cands.iter().map(|(m, s)| return_type(e, m, s, None, projected(&rt), smart, depth)))?
                        }
                        SyntaxKind::CALL_EXPR => certain(e, &sel, smart, depth + 1)?,
                        _ => return None,
                    }
                }
                // `Type.member` / `pkg.f()`: names, resolved by the engine.
                None if static_receiver(e, &recv) => match sel.kind() {
                    SyntaxKind::NAME_REF => name(e, &sel, smart, depth)?,
                    SyntaxKind::CALL_EXPR => certain(e, &sel, smart, depth + 1)?,
                    _ => return None,
                },
                None => return None,
            };
            Some(if safe { t.nullable(true) } else { t })
        }
        SyntaxKind::INDEX_EXPR => {
            let recv = x.first_child()?;
            let rt = certain(e, &recv, smart, depth + 1)?;
            let indices = x.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).flat_map(|i| i.children().collect::<Vec<_>>()).count();
            let gets: Vec<(Symbol, Subst)> = e.members_named(&rt, "get").into_iter().filter(|(m, _)| m.kind == SymbolKind::Function && m.params.len() == indices.max(1)).collect();
            // `m[k]` calls the `get` operator.
            same_result(e, gets.iter().map(|(m, s)| return_type(e, m, s, Some(x), projected(&rt), smart, depth)))
        }
        SyntaxKind::POSTFIX_EXPR => {
            let op = binary_op(x)?;
            if op.kind() != SyntaxKind::EXCL2 {
                return None;
            }
            let t = certain(e, &x.first_child()?, smart, depth + 1)?;
            Some(non_null(t))
        }
        SyntaxKind::BINARY_WITH_TYPE => {
            let ty = x.children().find_map(ast::TypeRef::cast)?;
            let safe = x.children().any(|c| c.kind() == SyntaxKind::OPERATION_REFERENCE && c.text() == "as?");
            let t = finish(e.resolve_syntax_type(&ty))?;
            Some(if safe { t.nullable(true) } else { t })
        }
        _ => None,
    }
}

/// A certain type as shown in messages: unknown type arguments as `*`.
pub(super) fn shown(t: &TypeRef) -> String {
    t.display().replace("<error>", "*")
}

/// `x!!`: the type without `?`.
fn non_null(t: TypeRef) -> TypeRef {
    match t {
        TypeRef::Named { name, fq, args, .. } => TypeRef::Named { name, fq, args, nullable: false },
        TypeRef::Function { receiver, params, ret, suspend, .. } => TypeRef::Function { receiver, params, ret, suspend, nullable: false },
        TypeRef::Platform(inner) => non_null(*inner),
        other => other,
    }
}

/// `this` may be smart cast (`when (this) { is T -> … }`, `if (this is T)`), which is not
/// modelled: any `is` check of `this` in the enclosing function.
fn this_checked(x: &SyntaxNode) -> bool {
    let Some(f) = x.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::CLASS_BODY)) else { return false };
    f.descendants().any(|d| match d.kind() {
        SyntaxKind::IS_EXPR => d.first_child().is_some_and(|c| c.kind() == SyntaxKind::THIS_EXPR),
        SyntaxKind::WHEN_SUBJECT => d.children().next().is_some_and(|c| c.kind() == SyntaxKind::THIS_EXPR),
        _ => false,
    })
}

pub(super) fn in_unknown_lambda(e: &Engine, at: &SyntaxNode) -> bool {
    at.ancestors().any(|a| a.kind() == SyntaxKind::LAMBDA_EXPR && e.lambda_signature(&a).is_none())
}

/// The number of arguments of a call (trailing lambdas included).
fn arity(c: &SyntaxNode) -> usize {
    c.children().find(|l| l.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).map_or(0, |l| l.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).count()) + c.children().filter(|l| l.kind() == SyntaxKind::LAMBDA_ARGUMENT).count()
}

fn fits(s: &Symbol, n: usize) -> bool {
    let required = s.params.iter().filter(|p| !p.has_default && !p.vararg).count();
    n >= required && (n <= s.params.len() || s.params.iter().any(|p| p.vararg))
}

/// `recv` names a class or package (`Type.member`, `pkg.f()`).
fn static_receiver(e: &Engine, recv: &SyntaxNode) -> bool {
    matches!(recv.kind(), SyntaxKind::NAME_REF | SyntaxKind::DOT_QUALIFIED_EXPR) && matches!(e.resolve_qualifier(recv), Qualifier::Static(_) | Qualifier::Package(_))
}

/// The candidates of a call (with their receiver substitution), and whether its receiver
/// has `*` or unknown type arguments. A member call on a certain receiver chooses its own
/// candidates; other calls take the resolver's, which must all fit the arguments.
fn call_candidates(e: &Engine, c: &SyntaxNode, smart: bool, depth: u32) -> Option<(Vec<(Symbol, Subst)>, bool)> {
    let outer = c.parent().filter(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.last_child().as_ref() == Some(c) && p.first_child().as_ref() != Some(c));
    if let Some(outer) = &outer {
        let recv = outer.first_child()?;
        if let Some(rt) = certain(e, &recv, smart, depth + 1) {
            let rt = if outer.kind() == SyntaxKind::SAFE_QUALIFIED_EXPR { non_null(rt) } else { rt };
            return Some((select_candidates(e, c, &rt, Some(c))?, projected(&rt)));
        }
        if !static_receiver(e, &recv) {
            return None;
        }
    }
    let callee = c.first_child().filter(|n| n.kind() == SyntaxKind::NAME_REF)?;
    let defs = e.resolve_name_ref(&callee);
    if defs.is_empty() || uncertain_implicit_member(e, &callee, &defs) {
        return None;
    }
    // The resolver falls back to candidates that do not fit; the call is then unclear.
    let n = arity(c);
    let mut out = Vec::new();
    for d in defs {
        let Def::Symbol { sym, subst } = d else { return None };
        if matches!(sym.kind, SymbolKind::Function | SymbolKind::Constructor) && !fits(&sym, n) {
            return None;
        }
        out.push((sym, subst));
    }
    Some((out, false))
}

/// Candidates for member `sel` (a name, or a call `sel`) of a receiver of certain type `rt`:
/// members win over extensions, overrides hide what they override, and among extensions
/// the most specific receivers win.
fn select_candidates(e: &Engine, sel: &SyntaxNode, rt: &TypeRef, call: Option<&SyntaxNode>) -> Option<Vec<(Symbol, Subst)>> {
    let callee = match call {
        Some(c) => c.first_child().filter(|n| n.kind() == SyntaxKind::NAME_REF)?,
        None => sel.clone(),
    };
    let name = ast::ident_text(&callee.first_token()?);
    let n = call.map(arity);
    let wanted = |m: &Symbol| match n {
        Some(n) => m.kind == SymbolKind::Function && fits(m, n) || matches!(m.kind, SymbolKind::Property | SymbolKind::Field) && m.params.is_empty(),
        None => matches!(m.kind, SymbolKind::Property | SymbolKind::Field),
    };
    let mut members: Vec<(Symbol, Subst)> = Vec::new();
    for (m, s) in e.members_named(rt, &name) {
        if m.receiver.is_some() || !wanted(&m) {
            continue;
        }
        // The nearest declaration of a signature hides the overridden ones.
        let key = |m: &Symbol| m.params.iter().map(|p| p.ty.display()).collect::<Vec<_>>();
        if members.iter().any(|(o, _)| key(o) == key(&m) && o.kind == m.kind) {
            continue;
        }
        members.push((m, s));
    }
    // A function name without a call: a member function exists, so it is not a value.
    if call.is_none() && e.members_named(rt, &name).iter().any(|(m, _)| m.kind == SymbolKind::Function) && members.is_empty() {
        return None;
    }
    if !members.is_empty() {
        return Some(members);
    }
    let mut exts = e.member_extensions(sel, rt, &name);
    if exts.is_empty() {
        exts = e.extensions(rt, &name);
    }
    exts.retain(|(m, _)| wanted(m));
    // Most specific receivers: drop candidates whose receiver class is a supertype of
    // another's (a type parameter receiver is the least specific).
    let receivers: Vec<Option<String>> = exts.iter().map(|(m, _)| m.receiver.as_ref().and_then(|r| fq_of(&e.resolve_declared(m, r)).map(str::to_string))).collect();
    let keep: Vec<bool> = receivers
        .iter()
        .map(|a| {
            !receivers.iter().any(|b| match (a, b) {
                (None, Some(_)) => true,
                (Some(a), Some(b)) => a != b && e.conforms(&TypeRef::named(b), a) == Some(true),
                _ => false,
            })
        })
        .collect();
    let chosen: Vec<(Symbol, Subst)> = exts.into_iter().zip(keep).filter(|(_, k)| *k).map(|(c, _)| c).collect();
    (!chosen.is_empty()).then_some(chosen)
}

/// A usable result: a resolved class or function type; unknown type arguments become `*`;
/// a type parameter is not certain.
fn finish(t: TypeRef) -> Option<TypeRef> {
    fn args(t: TypeRef) -> TypeRef {
        match t {
            TypeRef::Named { name, fq, args: a, nullable } => {
                let a = a
                    .into_iter()
                    .map(|arg| match arg {
                        TypeArg::Type { variance, ty } if matches!(strip(&ty), TypeRef::TypeParam { .. } | TypeRef::Error) => TypeArg::Type { variance, ty: TypeRef::Error },
                        TypeArg::Type { variance, ty } => TypeArg::Type { variance, ty: args(ty) },
                        TypeArg::Star => TypeArg::Star,
                    })
                    .collect();
                TypeRef::Named { name, fq, args: a, nullable }
            }
            TypeRef::Platform(inner) => TypeRef::Platform(Box::new(args(*inner))),
            other => other,
        }
    }
    match strip(&t) {
        TypeRef::Named { fq: Some(_), .. } | TypeRef::Function { .. } => Some(args(t)),
        _ => None,
    }
}

/// The one type all candidates return (`None` if they differ or any is unknown).
fn same_result(_e: &Engine, results: impl Iterator<Item = Option<TypeRef>>) -> Option<TypeRef> {
    let mut found: Option<TypeRef> = None;
    for r in results {
        let r = r?;
        match &found {
            Some(f) if f.display() != r.display() => return None,
            Some(_) => {}
            None => found = Some(r),
        }
    }
    found
}

/// What calling (or reading) `sym` gives, with the substitution of its receiver, the
/// call's explicit type arguments and the type parameters inferred from its arguments.
#[allow(clippy::too_many_arguments)]
fn return_type(e: &Engine, sym: &Symbol, subst: &Subst, call: Option<&SyntaxNode>, projected: bool, smart: bool, depth: u32) -> Option<TypeRef> {
    match sym.kind {
        SymbolKind::Constructor => {
            let class = e.classifier(sym.fq_name.trim_end_matches(".<init>"))?;
            return constructed(e, class.symbol(), std::slice::from_ref(sym), call, smart, depth);
        }
        k if k.is_classifier() => {
            return match k {
                // A class name called: its constructors.
                SymbolKind::Class | SymbolKind::Enum if call.is_some() => {
                    let ctors: Vec<Symbol> = e.declared_members(&sym.fq_name).into_iter().filter(|m| m.kind == SymbolKind::Constructor && call.is_none_or(|c| fits(m, arity(c)))).collect();
                    constructed(e, sym, &ctors, call, smart, depth)
                }
                SymbolKind::Object if call.is_none() => finish(class_type(sym)),
                _ => None,
            };
        }
        SymbolKind::EnumEntry => {
            let owner = sym.fq_name.rsplit_once('.')?.0;
            return finish(TypeRef::named(owner));
        }
        // A function name without a call is not a value (but a reference).
        SymbolKind::Function if call.is_none() => return None,
        SymbolKind::Function | SymbolKind::Property | SymbolKind::Field => {}
        _ => return None,
    }
    // Source declarations need a written type (or a block body: Unit).
    let ret = sym.ty.as_ref()?;
    let mut s = subst.clone();
    // A member function's own type parameters shadow its class's.
    if sym.receiver.is_none() {
        for tp in &sym.type_params {
            s.remove(&tp.name);
        }
    }
    if let Some(targs) = call.and_then(|c| c.children().find(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST)) {
        for (tp, a) in sym.type_params.iter().zip(targs.children().filter(|c| c.kind() == SyntaxKind::TYPE_PROJECTION)) {
            if let Some(t) = ast::child::<ast::TypeRef>(&a) {
                s.insert(tp.name.clone(), e.resolve_syntax_type(&t));
            }
        }
    }
    // Through a receiver with `*` (or unknown) arguments, type parameters inside the result
    // are captured types, not their bounds: `IColumnType<T>` read on `LiteralOp<*>`.
    let declared = e.resolve_declared(sym, ret);
    // Read through `*`, a type parameter is its upper bound (`Alias<*>.delegate` is a
    // `Table`), not `Any?`: left unknown.
    if projected && matches!(strip(&declared), TypeRef::TypeParam { name, .. } if !sym.type_params.iter().any(|tp| &tp.name == name)) {
        return None;
    }
    let declared = if projected { erase_nested(declared) } else { declared };
    // The call's own type parameters, from its arguments.
    if let Some(c) = call.filter(|c| c.kind() == SyntaxKind::CALL_EXPR && sym.kind == SymbolKind::Function) {
        let vars: HashSet<String> = sym.type_params.iter().map(|tp| tp.name.clone()).filter(|n| !s.contains_key(n)).collect();
        if !vars.is_empty() {
            let inferred = infer(e, sym, &s, c, vars, None, smart, depth);
            s.extend(inferred);
        }
    }
    let t = substitute(&declared, &s);
    // Invoking a property of function type.
    let t = if call.is_some() && matches!(sym.kind, SymbolKind::Property | SymbolKind::Field) {
        match strip(&t) {
            TypeRef::Function { ret, .. } => (**ret).clone(),
            _ => return None,
        }
    } else {
        t
    };
    finish(t)
}

/// Whether `t` has `*` or unknown type arguments.
fn projected(t: &TypeRef) -> bool {
    matches!(strip(t), TypeRef::Named { args, .. } if args.iter().any(|a| matches!(a, TypeArg::Star | TypeArg::Type { ty: TypeRef::Error, .. })))
}

/// Type parameters inside type arguments become unknown (the top level is kept).
fn erase_nested(t: TypeRef) -> TypeRef {
    fn erase(t: TypeRef) -> TypeRef {
        match t {
            TypeRef::TypeParam { .. } => TypeRef::Error,
            TypeRef::Named { name, fq, args, nullable } => TypeRef::Named {
                name,
                fq,
                args: args
                    .into_iter()
                    .map(|a| match a {
                        TypeArg::Type { variance, ty } => TypeArg::Type { variance, ty: erase(ty) },
                        star => star,
                    })
                    .collect(),
                nullable,
            },
            TypeRef::Platform(inner) => TypeRef::Platform(Box::new(erase(*inner))),
            other => other,
        }
    }
    match t {
        TypeRef::Named { name, fq, args, nullable } => TypeRef::Named {
            name,
            fq,
            args: args
                .into_iter()
                .map(|a| match a {
                    TypeArg::Type { variance, ty } => TypeArg::Type { variance, ty: erase(ty) },
                    star => star,
                })
                .collect(),
            nullable,
        },
        TypeRef::Platform(inner) => TypeRef::Platform(Box::new(erase_nested(*inner))),
        other => other,
    }
}

/// A name used as a value: a local, a property, an object or enum entry.
fn name(e: &Engine, nr: &SyntaxNode, smart: bool, depth: u32) -> Option<TypeRef> {
    let defs = e.resolve_name_ref(nr);
    if uncertain_implicit_member(e, nr, &defs) {
        return None;
    }
    match defs.as_slice() {
        [Def::Local(l)] => local(e, nr, l, smart, depth),
        [Def::Symbol { sym, subst }] => {
            // A project `val` may be smart cast where it is read: by a null check when its
            // type is nullable, or by an `is` check.
            if smart_castable(nr, sym) && !unchecked_nullable_property(nr, sym) {
                return None;
            }
            return_type(e, sym, subst, None, false, smart, depth)
        }
        _ => None,
    }
}

/// A project `val` read at `at` may be smart cast there: by a null check when its type is
/// nullable, or by an `is` check.
fn smart_castable(at: &SyntaxNode, sym: &Symbol) -> bool {
    matches!(sym.origin, Origin::Source { .. })
        && sym.kind == SymbolKind::Property
        && !sym.modifiers.has(Modifiers::VAR)
        && (sym.ty.as_ref().is_some_and(|t| matches!(t, TypeRef::Named { nullable: true, .. })) || checked_with_is(at, &sym.name))
}

/// A nullable project `val` named nowhere else in the function reading it, so nothing there
/// can have smart cast it.
fn unchecked_nullable_property(at: &SyntaxNode, sym: &Symbol) -> bool {
    if !sym.ty.as_ref().is_some_and(|t| matches!(t, TypeRef::Named { nullable: true, .. })) || checked_with_is(at, &sym.name) {
        return false;
    }
    let Some(body) = at.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::CLASS_INITIALIZER | SyntaxKind::SECONDARY_CONSTRUCTOR)) else { return false };
    !body.descendants().any(|d| d.kind() == SyntaxKind::NAME_REF && d != *at && d.first_token().is_some_and(|t| ast::ident_text(&t) == sym.name))
}

/// A local or parameter: its written type, or the certain type of its initializer, with
/// the engine's smart casts.
fn local(e: &Engine, nr: &SyntaxNode, l: &crate::analysis::ide::infer::LocalDef, smart: bool, depth: u32) -> Option<TypeRef> {
    let base = match l.kind {
        // `it`: the single parameter of the lambda opened at `name_range`.
        LocalKind::It => {
            let lambda = crate::syntax::covering(&e.root, l.name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| a.kind() == SyntaxKind::LAMBDA_EXPR)?;
            lambda_parameter(e, &lambda, Some(0), smart, depth)?
        }
        LocalKind::Val | LocalKind::Var | LocalKind::Parameter | LocalKind::LambdaParameter => {
            let decl = crate::syntax::covering(&e.root, l.name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| matches!(a.kind(), SyntaxKind::VALUE_PARAMETER | SyntaxKind::PROPERTY))?;
            // Destructured entries (`(a, b) ->`) are not their parameter.
            if has_modifier(&decl, "vararg") || name_token(&decl).is_none_or(|t| t.text_range() != l.name_range) {
                return None;
            }
            match ast::child::<ast::TypeRef>(&decl) {
                Some(t) => finish(e.resolve_syntax_type(&t))?,
                None if decl.kind() == SyntaxKind::PROPERTY && decl.children().all(|c| c.kind() != SyntaxKind::PROPERTY_DELEGATE) => certain(e, &initializer(&decl)?, smart, depth + 1)?,
                // A lambda parameter: typed by the call the lambda is passed to.
                None => {
                    let list = decl.parent().filter(|p| p.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST)?;
                    let index = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER).position(|c| c == decl)?;
                    lambda_parameter(e, &list.parent()?, Some(index), smart, depth)?
                }
            }
        }
        _ => return None,
    };
    let name = ast::ident_text(&nr.first_token()?);
    let checked_with_is = checked_with_is(nr, &name);
    let seen = e.type_of(nr)?;
    // Checked with `is`: certain only where the engine applied the cast (some, such as
    // `when (val x = …) { is T -> … }` nested in other branches, it does not model).
    if checked_with_is {
        return (smart && fq_of(&seen) != fq_of(&base)).then(|| finish(seen)).flatten();
    }
    // The engine's view (null checks applied) must agree on the class.
    if fq_of(&seen) != fq_of(&base) {
        return None;
    }
    // Keep the certain type arguments, take nullability from the engine.
    let nullable = matches!(strip(&seen), TypeRef::Named { nullable: true, .. });
    Some(match base {
        TypeRef::Platform(inner) if !nullable => TypeRef::Platform(inner),
        other => {
            let t = non_null(other);
            if nullable { t.nullable(true) } else { t }
        }
    })
}

/// `name` is checked with `is` in a condition (or a `when` has `is` branches) before `at`
/// in the function (or class body, file) around it: the engine may not model the smart
/// cast. An `is` whose value is only stored or returned narrows nothing.
fn checked_with_is(at: &SyntaxNode, name: &str) -> bool {
    let scope = at.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::LAMBDA_EXPR | SyntaxKind::CLASS_BODY | SyntaxKind::KOTLIN_FILE));
    let narrowing = |d: &SyntaxNode| {
        d.kind() == SyntaxKind::WHEN_CONDITION
            || d.ancestors().skip(1).take_while(|a| !matches!(a.kind(), SyntaxKind::BLOCK | SyntaxKind::PROPERTY | SyntaxKind::RETURN_EXPR)).any(|a| {
                matches!(a.kind(), SyntaxKind::CONDITION | SyntaxKind::WHEN_CONDITION | SyntaxKind::WHEN_ENTRY | SyntaxKind::PREFIX_EXPR)
                    || a.kind() == SyntaxKind::BINARY_EXPR && binary_op(&a).is_some_and(|o| matches!(o.kind(), SyntaxKind::AMP2 | SyntaxKind::PIPE2))
                    || a.kind() == SyntaxKind::CALL_EXPR && a.first_child().is_some_and(|c| matches!(c.text().to_string().as_str(), "require" | "check" | "assert"))
            })
    };
    scope.is_some_and(|s| {
        s.descendants().filter(|d| matches!(d.kind(), SyntaxKind::IS_EXPR | SyntaxKind::WHEN_CONDITION) && d.text().to_string().contains("is ") && d.text_range().start() < at.text_range().start() && narrowing(d)).any(|d| {
            d.kind() == SyntaxKind::WHEN_CONDITION || d.descendants().any(|r| r.kind() == SyntaxKind::NAME_REF && r.first_token().is_some_and(|t| ast::ident_text(&t) == name))
        })
    })
}

/// A constructor call's class type: the class's type parameters inferred through the
/// constructor's parameters (all constructors that fit must agree).
fn constructed(e: &Engine, class: &Symbol, ctors: &[Symbol], call: Option<&SyntaxNode>, smart: bool, depth: u32) -> Option<TypeRef> {
    let t = class_type(class);
    let vars: HashSet<String> = class.type_params.iter().map(|tp| tp.name.clone()).collect();
    let (Some(c), false) = (call.filter(|c| c.kind() == SyntaxKind::CALL_EXPR), vars.is_empty()) else { return finish(t) };
    // Explicit type arguments: `Box<String>(x)`.
    if let Some(targs) = c.children().find(|n| n.kind() == SyntaxKind::TYPE_ARGUMENT_LIST) {
        let mut s = Subst::new();
        for (tp, a) in class.type_params.iter().zip(targs.children().filter(|n| n.kind() == SyntaxKind::TYPE_PROJECTION)) {
            if let Some(ty) = ast::child::<ast::TypeRef>(&a) {
                s.insert(tp.name.clone(), e.resolve_syntax_type(&ty));
            }
        }
        return finish(substitute(&t, &s));
    }
    if ctors.is_empty() {
        return finish(t);
    }
    same_result(e, ctors.iter().map(|ctor| finish(substitute(&t, &infer(e, ctor, &Subst::new(), c, vars.clone(), None, smart, depth)))))
}

/// The type parameters `vars` of a called declaration, as bound by the call's arguments.
#[derive(Default)]
struct Inference {
    vars: HashSet<String>,
    bound: HashMap<String, TypeRef>,
    unknown: HashSet<String>,
}

impl Inference {
    /// The variables in `t` become unknown.
    fn forget(&mut self, t: &TypeRef) {
        fn walk(t: &TypeRef, vars: &HashSet<String>, out: &mut HashSet<String>) {
            match t {
                TypeRef::TypeParam { name, .. } if vars.contains(name) => {
                    out.insert(name.clone());
                }
                TypeRef::Named { args, .. } => {
                    for a in args {
                        if let TypeArg::Type { ty, .. } = a {
                            walk(ty, vars, out);
                        }
                    }
                }
                TypeRef::Function { receiver, params, ret, .. } => {
                    for p in receiver.iter().map(|r| r.as_ref()).chain(params).chain(std::iter::once(ret.as_ref())) {
                        walk(p, vars, out);
                    }
                }
                TypeRef::Platform(inner) => walk(inner, vars, out),
                _ => {}
            }
        }
        walk(t, &self.vars, &mut self.unknown);
    }

    /// Match the declared type of a parameter against the certain type of its argument.
    fn bind(&mut self, e: &Engine, declared: &TypeRef, actual: &TypeRef) {
        match strip(declared) {
            TypeRef::TypeParam { name, nullable } if self.vars.contains(name) => {
                let a = if *nullable { non_null(actual.clone()) } else { actual.clone() };
                match self.bound.get(name) {
                    // Different types: Kotlin takes a common supertype, not followed here.
                    Some(b) if b.display() != a.display() => {
                        self.unknown.insert(name.clone());
                    }
                    Some(_) => {}
                    None => {
                        self.bound.insert(name.clone(), a);
                    }
                }
            }
            TypeRef::Named { fq: Some(fq), args, .. } if !args.is_empty() => {
                // The argument seen as the parameter's class (`List<String>` as `Collection<T>`).
                let view = if fq_of(actual) == Some(&**fq) { Some(strip(actual).clone()) } else { e.supertype_closure(actual).into_iter().find(|st| fq_of(st) == Some(&**fq)) };
                let Some(TypeRef::Named { args: actual_args, .. }) = view else {
                    self.forget(declared);
                    return;
                };
                if actual_args.len() != args.len() {
                    self.forget(declared);
                    return;
                }
                for (d, a) in args.iter().zip(&actual_args) {
                    match (d, a) {
                        // `in` positions only give lower bounds.
                        (TypeArg::Type { variance: Variance::In, ty }, _) => self.forget(ty),
                        (TypeArg::Type { ty: d, .. }, TypeArg::Type { ty: a, .. }) if !matches!(a, TypeRef::Error) => self.bind(e, d, a),
                        (TypeArg::Type { ty, .. }, _) => self.forget(ty),
                        (TypeArg::Star, _) => {}
                    }
                }
            }
            other => self.forget(other),
        }
    }

    fn result(self) -> Subst {
        let unknown = self.unknown;
        self.bound.into_iter().filter(|(k, _)| !unknown.contains(k)).collect()
    }
}

/// A call's arguments matched to the parameters of `sym`: (parameter index, expression).
fn arguments(sym: &Symbol, c: &SyntaxNode) -> Option<Vec<(usize, SyntaxNode)>> {
    let mut out = Vec::new();
    let list: Vec<SyntaxNode> = c.children().find(|l| l.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).map(|l| l.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).collect()).unwrap_or_default();
    let vararg = sym.params.iter().position(|p| p.vararg);
    let mut named = false;
    for (i, a) in list.iter().enumerate() {
        // Spread arguments (`*array`) are not matched.
        if ast::token(a, SyntaxKind::STAR).is_some() {
            return None;
        }
        let index = match a.children().find(|n| n.kind() == SyntaxKind::VALUE_ARGUMENT_NAME).and_then(|n| ast::token(&n, SyntaxKind::IDENT)) {
            Some(n) => {
                named = true;
                let n = ast::ident_text(&n);
                sym.params.iter().position(|p| p.name == n)?
            }
            None if named => return None,
            // A vararg collects the positional arguments from its position on.
            None => match vararg {
                Some(v) if i >= v => v,
                _ => {
                    sym.params.get(i)?;
                    i
                }
            },
        };
        out.push((index, a.children().filter(|n| n.kind() != SyntaxKind::VALUE_ARGUMENT_NAME).last()?));
    }
    for l in c.children().filter(|n| n.kind() == SyntaxKind::LAMBDA_ARGUMENT) {
        out.push((sym.params.len().checked_sub(1)?, l.children().last()?));
    }
    Some(out)
}

/// The lambda expression of an argument (through a label).
fn lambda_of(x: &SyntaxNode) -> Option<SyntaxNode> {
    match x.kind() {
        SyntaxKind::LAMBDA_EXPR => Some(x.clone()),
        SyntaxKind::LABELED_EXPR => x.children().find(|n| n.kind() == SyntaxKind::LAMBDA_EXPR),
        _ => None,
    }
}

/// Bind the type parameters `vars` of `sym` (called by `c`, receiver substitution `s`)
/// from its arguments. `stop_at`: a lambda whose own parameters are being typed (its body
/// and the lambdas after it are not used).
#[allow(clippy::too_many_arguments)]
fn infer(e: &Engine, sym: &Symbol, s: &Subst, c: &SyntaxNode, vars: HashSet<String>, stop_at: Option<&SyntaxNode>, smart: bool, depth: u32) -> Subst {
    let mut inf = Inference { vars, ..Default::default() };
    let Some(args) = arguments(sym, c) else { return Subst::new() };
    let declared = |i: usize| substitute(&e.resolve_declared(sym, &sym.params[i].ty), s);
    // Value arguments first, then lambdas (whose parameter types may need them).
    let (lambdas, values): (Vec<_>, Vec<_>) = args.into_iter().partition(|(_, x)| lambda_of(x).is_some());
    for (i, x) in &values {
        let d = declared(*i);
        // Integer literals adapt to the expected type: they bind nothing.
        let int_literal = literal_type(x) == Some(Some("kotlin.Int"));
        match certain(e, x, smart, depth + 1) {
            Some(a) if !int_literal => inf.bind(e, &d, &a),
            _ => inf.forget(&d),
        }
    }
    for (i, x) in &lambdas {
        let lambda = lambda_of(x).unwrap();
        if stop_at == Some(&lambda) {
            break;
        }
        let d = declared(*i);
        let TypeRef::Function { ret, .. } = strip(&d) else {
            inf.forget(&d);
            continue;
        };
        if matches!(ret.fq_name(), Some("kotlin.Unit")) {
            continue;
        }
        match lambda_result(e, &lambda, smart, depth) {
            Some(r) => inf.bind(e, ret, &r),
            None => inf.forget(ret),
        }
    }
    inf.result()
}

/// What a lambda returns: its last expression (`Unit` for a statement); `None` if unknown
/// or when it also returns through a label.
fn lambda_result(e: &Engine, lambda: &SyntaxNode, smart: bool, depth: u32) -> Option<TypeRef> {
    if lambda.descendants().any(|r| r.kind() == SyntaxKind::RETURN_EXPR && r.children().any(|c| matches!(c.kind(), SyntaxKind::LABEL | SyntaxKind::LABEL_QUALIFIER))) {
        return None;
    }
    let block = lambda.children().find(|n| n.kind() == SyntaxKind::BLOCK)?;
    let Some(last) = block.children().last() else { return Some(TypeRef::named("kotlin.Unit")) };
    let statement = matches!(last.kind(), SyntaxKind::PROPERTY | SyntaxKind::FUN | SyntaxKind::CLASS | SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR) || assignment_op(&last).is_some();
    if statement {
        return Some(TypeRef::named("kotlin.Unit"));
    }
    certain(e, &last, smart, depth + 1)
}

/// The type of parameter `index` of `lambda` (its receiver for `None`), from the call it is
/// passed to: the parameter's function type with the type parameters bound by the
/// arguments before it.
fn lambda_parameter(e: &Engine, lambda: &SyntaxNode, index: Option<usize>, smart: bool, depth: u32) -> Option<TypeRef> {
    let mut arg = lambda.parent()?;
    if arg.kind() == SyntaxKind::LABELED_EXPR {
        arg = arg.parent()?;
    }
    let c = match arg.kind() {
        SyntaxKind::LAMBDA_ARGUMENT => arg.parent()?,
        SyntaxKind::VALUE_ARGUMENT => arg.parent()?.parent()?,
        _ => return None,
    };
    let (cands, projected) = call_candidates(e, &c, smart, depth + 1)?;
    same_result(
        e,
        cands.iter().map(|(sym, subst)| {
            if sym.kind != SymbolKind::Function {
                return None;
            }
            let (i, _) = arguments(sym, &c)?.into_iter().find(|(_, x)| lambda_of(x).as_ref() == Some(lambda))?;
            let mut s = subst.clone();
            if sym.receiver.is_none() {
                for tp in &sym.type_params {
                    s.remove(&tp.name);
                }
            }
            let vars: HashSet<String> = sym.type_params.iter().map(|tp| tp.name.clone()).filter(|n| !s.contains_key(n)).collect();
            let inferred = infer(e, sym, &s, &c, vars, Some(lambda), smart, depth);
            s.extend(inferred);
            let declared = e.resolve_declared(sym, &sym.params[i].ty);
            let declared = if projected { erase_nested(declared) } else { declared };
            let TypeRef::Function { receiver, params, .. } = strip(&substitute(&declared, &s)).clone() else { return None };
            let t = match index {
                None => *receiver?,
                Some(n) => params.get(n)?.clone(),
            };
            finish(t)
        }),
    )
}
