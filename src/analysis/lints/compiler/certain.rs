//! Certain types: the type of an expression when every step to it is unambiguous, for
//! checks that must not guess.
//!
//! Literals, locals and parameters (declared with a type, or initialized with a certain
//! value), properties with a written type, and calls and member chains on certain
//! receivers whose candidates all return the same type. A result that is still a type
//! parameter of the called function (`fun <T> f(): T`) is not certain; type arguments that
//! stay unknown are kept as error types (shown `?`): the class and nullability still serve
//! member lookups and null checks, and reading through them (`List<?>.get`) is not certain.

use super::super::*;
use super::expressions::{literal_type, uncertain_implicit_member};
use crate::analysis::ide::infer::{Def, Engine, LocalKind, Qualifier, Subst, class_type, fq_of, strip, substitute};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeArg, TypeRef};

/// The certain type of `x`. `smart`: smart casts the engine applies (`is` checks) are
/// trusted; otherwise names checked with `is` are not certain.
pub(super) fn certain_type(e: &Engine, x: &SyntaxNode, smart: bool) -> Option<TypeRef> {
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
        SyntaxKind::THIS_EXPR if x.text() == "this" && !in_unknown_lambda(e, x) => e.type_of(x).and_then(finish),
        SyntaxKind::NAME_REF => name(e, x, smart, depth),
        SyntaxKind::CALL_EXPR => call(e, x, depth),
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
                        SyntaxKind::NAME_REF => select(e, &sel, &rt, None)?,
                        SyntaxKind::CALL_EXPR => select(e, &sel, &rt, Some(&sel))?,
                        _ => return None,
                    }
                }
                // `Type.member` / `pkg.f()`: names, resolved by the engine.
                None if matches!(recv.kind(), SyntaxKind::NAME_REF | SyntaxKind::DOT_QUALIFIED_EXPR) && matches!(e.resolve_qualifier(&recv), Qualifier::Static(_) | Qualifier::Package(_)) => match sel.kind() {
                    SyntaxKind::NAME_REF => name(e, &sel, smart, depth)?,
                    SyntaxKind::CALL_EXPR => call(e, &sel, depth)?,
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
            same_result(e, gets.iter().map(|(m, s)| return_type(e, m, s, Some(x), projected(&rt))))
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

fn in_unknown_lambda(e: &Engine, at: &SyntaxNode) -> bool {
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

/// Member `sel` (a name, or a call `sel`) of a receiver of certain type `rt`: members win
/// over extensions, overrides hide what they override, and among extensions the most
/// specific receivers win. All remaining candidates must agree.
fn select(e: &Engine, sel: &SyntaxNode, rt: &TypeRef, call: Option<&SyntaxNode>) -> Option<TypeRef> {
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
        return same_result(e, members.iter().map(|(m, s)| return_type(e, m, s, call, projected(rt))));
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
    let chosen: Vec<&(Symbol, Subst)> = exts.iter().zip(keep).filter(|(_, k)| *k).map(|(c, _)| c).collect();
    if chosen.is_empty() {
        return None;
    }
    same_result(e, chosen.iter().map(|(m, s)| return_type(e, m, s, call, projected(rt))))
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

/// What calling (or reading) `sym` gives, with the substitution of its receiver and the
/// call's explicit type arguments.
fn return_type(e: &Engine, sym: &Symbol, subst: &Subst, call: Option<&SyntaxNode>, projected: bool) -> Option<TypeRef> {
    match sym.kind {
        SymbolKind::Constructor => {
            let class = e.classifier(sym.fq_name.trim_end_matches(".<init>"))?;
            return finish(class_type(class.symbol()));
        }
        k if k.is_classifier() => {
            return match k {
                // A class name called: its constructor.
                SymbolKind::Class | SymbolKind::Enum if call.is_some() => finish(class_type(sym)),
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
    let declared = if projected { erase_nested(declared) } else { declared };
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
            if matches!(sym.origin, Origin::Source { .. })
                && sym.kind == SymbolKind::Property
                && !sym.modifiers.has(Modifiers::VAR)
                && (sym.ty.as_ref().is_some_and(|t| matches!(t, TypeRef::Named { nullable: true, .. })) || checked_with_is(nr, &sym.name))
            {
                return None;
            }
            return_type(e, sym, subst, None, false)
        }
        _ => None,
    }
}

/// A local or parameter: its written type, or the certain type of its initializer, with
/// the engine's smart casts.
fn local(e: &Engine, nr: &SyntaxNode, l: &crate::analysis::ide::infer::LocalDef, smart: bool, depth: u32) -> Option<TypeRef> {
    if !matches!(l.kind, LocalKind::Val | LocalKind::Var | LocalKind::Parameter) {
        return None;
    }
    let decl = crate::syntax::covering(&e.root, l.name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| matches!(a.kind(), SyntaxKind::VALUE_PARAMETER | SyntaxKind::PROPERTY))?;
    if has_modifier(&decl, "vararg") {
        return None;
    }
    let base = match ast::child::<ast::TypeRef>(&decl) {
        Some(t) => finish(e.resolve_syntax_type(&t))?,
        None if decl.kind() == SyntaxKind::PROPERTY && decl.children().all(|c| c.kind() != SyntaxKind::PROPERTY_DELEGATE) => certain(e, &initializer(&decl)?, smart, depth + 1)?,
        None => return None,
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

/// A call: every candidate the resolver keeps must return the same type.
fn call(e: &Engine, c: &SyntaxNode, _depth: u32) -> Option<TypeRef> {
    let callee = c.first_child().filter(|n| n.kind() == SyntaxKind::NAME_REF)?;
    let defs = e.resolve_name_ref(&callee);
    if defs.is_empty() || uncertain_implicit_member(e, &callee, &defs) {
        return None;
    }
    // The resolver falls back to candidates that do not fit; the call is then unclear.
    let n = arity(c);
    if defs.iter().any(|d| matches!(d, Def::Symbol { sym, .. } if matches!(sym.kind, SymbolKind::Function | SymbolKind::Constructor) && !fits(sym, n))) {
        return None;
    }
    same_result(
        e,
        defs.iter().map(|d| match d {
            Def::Symbol { sym, subst } => return_type(e, sym, subst, Some(c), false),
            _ => None,
        }),
    )
}
