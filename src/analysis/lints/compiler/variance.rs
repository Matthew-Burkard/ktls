//! Declaration-site variance: an `out` type parameter of a class only in `out` positions
//! of its non-private members (return types, `val` types), an `in` one only in `in`
//! positions (parameter types). Positions flip through `in` type parameters and function
//! type parameters, and are invariant through invariant ones.

use super::super::*;
use crate::analysis::ide::infer::{Engine, fq_of};
use crate::symbols::Variance;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants() {
        if cx.in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::CLASS => class(e, &n, out),
            SyntaxKind::USER_TYPE => projections(e, &n, out),
            _ => {}
        }
    }
}

/// Use-site projections against the declared variance (`List<out T>`, `Comparable<out T>`)
/// and projections of a supertype's immediate arguments.
fn projections(e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let Some(last) = n.children().filter(|c| c.kind() == SyntaxKind::USER_TYPE_SEGMENT).last() else { return };
    let Some(args) = last.children().find(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST) else { return };
    let supertype = n.parent().filter(|p| p.kind() == SyntaxKind::TYPE_REFERENCE).and_then(|t| t.parent()).is_some_and(|p| matches!(p.kind(), SyntaxKind::SUPER_TYPE_ENTRY | SyntaxKind::SUPER_TYPE_CALL_ENTRY | SyntaxKind::DELEGATED_SUPER_TYPE_ENTRY));
    let elements: Vec<_> = args.children_with_tokens().filter(|t| !t.kind().is_trivia() && !matches!(t.kind(), SyntaxKind::LT | SyntaxKind::GT | SyntaxKind::COMMA)).collect();
    let mut variances: Option<Vec<Variance>> = None;
    for (i, el) in elements.iter().enumerate() {
        let star = el.as_token().is_some_and(|t| t.kind() == SyntaxKind::STAR) || el.as_node().is_some_and(|p| p.text().to_string().trim_start().starts_with('*'));
        let Some(proj) = el.as_node().filter(|p| p.kind() == SyntaxKind::TYPE_PROJECTION) else {
            if star && supertype {
                out.report(&PROJECTION_IN_IMMEDIATE_ARGUMENT_TO_SUPERTYPE, el.text_range(), "Projections are not allowed for immediate arguments of a supertype.", None);
            }
            continue;
        };
        let Some(mods) = modifiers(proj) else {
            if star && supertype {
                out.report(&PROJECTION_IN_IMMEDIATE_ARGUMENT_TO_SUPERTYPE, proj.text_range(), "Projections are not allowed for immediate arguments of a supertype.", None);
            }
            continue;
        };
        let kw = mods.syntax().descendants_with_tokens().filter_map(|t| t.into_token()).find(|t| matches!(t.text(), "in" | "out"));
        let Some(kw) = kw else { continue };
        if supertype {
            out.report(&PROJECTION_IN_IMMEDIATE_ARGUMENT_TO_SUPERTYPE, proj.text_range(), "Projections are not allowed for immediate arguments of a supertype.", None);
            continue;
        }
        if variances.is_none() {
            let Some(tr) = n.ancestors().find_map(ast::TypeRef::cast) else { return };
            let t = e.resolve_syntax_type(&tr);
            let Some(cls) = fq_of(&t).and_then(|fq| e.classifier(fq)) else { return };
            // Type aliases and Java classes (all invariant) are left out.
            if cls.symbol().kind == crate::symbols::SymbolKind::TypeAlias {
                return;
            }
            variances = Some(cls.symbol().type_params.iter().map(|p| p.variance).collect());
        }
        let Some(declared) = variances.as_ref().and_then(|v| v.get(i)) else { return };
        let used = if kw.text() == "out" { Variance::Out } else { Variance::In };
        let shown = n.text().to_string();
        match (declared, used) {
            (Variance::Out, Variance::Out) | (Variance::In, Variance::In) => {
                // The keyword and the space after it.
                let end = kw.next_token().filter(|t| t.kind().is_trivia()).map_or(kw.text_range().end(), |t| t.text_range().end());
                out.report(&REDUNDANT_PROJECTION, kw.text_range(), format!("Redundant projection: the corresponding type parameter of {shown} has the same variance."), Some(Fix::delete("Remove redundant projection", TextRange::new(kw.text_range().start(), end))));
            }
            (Variance::Out, Variance::In) | (Variance::In, Variance::Out) => {
                out.report(&CONFLICTING_PROJECTION, kw.text_range(), format!("Projection is conflicting with the variance of the corresponding type parameter of {shown}. Remove the projection or replace it with '*'."), None);
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pos {
    Out,
    In,
    Invariant,
}

impl Pos {
    fn flip(self) -> Pos {
        match self {
            Pos::Out => Pos::In,
            Pos::In => Pos::Out,
            Pos::Invariant => Pos::Invariant,
        }
    }

    /// The position of a type argument for a parameter of variance `v`.
    fn through(self, v: Variance) -> Pos {
        match v {
            Variance::Out => self,
            Variance::In => self.flip(),
            Variance::Invariant => Pos::Invariant,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Pos::Out => "out",
            Pos::In => "in",
            Pos::Invariant => "invariant",
        }
    }
}

/// The class's `in`/`out` type parameters.
fn declared(c: &SyntaxNode) -> Vec<(String, Variance)> {
    c.children()
        .filter(|x| x.kind() == SyntaxKind::TYPE_PARAMETER_LIST)
        .flat_map(|l| l.children().filter(|p| p.kind() == SyntaxKind::TYPE_PARAMETER).collect::<Vec<_>>())
        .filter_map(|p| {
            let name = name_token(&p)?.text().to_string();
            let v = if has_modifier(&p, "out") {
                Variance::Out
            } else if has_modifier(&p, "in") {
                Variance::In
            } else {
                return None;
            };
            Some((name, v))
        })
        .collect()
}

fn class(e: &Engine, c: &SyntaxNode, out: &mut Sink) {
    let params = declared(c);
    if params.is_empty() {
        return;
    }
    // Primary constructor properties.
    for p in c.children().filter(|x| x.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR).flat_map(|p| p.descendants().filter(|d| d.kind() == SyntaxKind::VALUE_PARAMETER).collect::<Vec<_>>()) {
        let pos = if ast::token(&p, SyntaxKind::VAR_KW).is_some() {
            Pos::Invariant
        } else if ast::token(&p, SyntaxKind::VAL_KW).is_some() {
            Pos::Out
        } else {
            continue;
        };
        if has_modifier(&p, "private") {
            continue;
        }
        if let Some(t) = ast::child::<ast::TypeRef>(&p) {
            walk(e, t.syntax(), pos, &params, out);
        }
    }
    let Some(body) = c.children().find(|x| x.kind() == SyntaxKind::CLASS_BODY) else { return };
    for m in body.children() {
        if has_modifier(&m, "private") {
            continue;
        }
        match m.kind() {
            SyntaxKind::FUN => {
                let Some(f) = ast::FunDecl::cast(m.clone()) else { continue };
                // The function's own type parameters hide the class's of the same name.
                let own: Vec<String> = f.type_parameters().iter().filter_map(|t| name_token(t.syntax()).map(|n| n.text().to_string())).collect();
                let visible: Vec<(String, Variance)> = params.iter().filter(|(n, _)| !own.contains(n)).cloned().collect();
                if let Some(r) = f.receiver() {
                    walk(e, r.syntax(), Pos::In, &visible, out);
                }
                for p in f.parameters() {
                    if let Some(t) = ast::child::<ast::TypeRef>(p.syntax()) {
                        walk(e, t.syntax(), Pos::In, &visible, out);
                    }
                }
                if let Some(r) = f.return_type() {
                    walk(e, r.syntax(), Pos::Out, &visible, out);
                }
            }
            SyntaxKind::PROPERTY => {
                let Some(p) = ast::PropertyDecl::cast(m.clone()) else { continue };
                // A `var` with a private setter is only read from outside.
                let private_setter = m.children().any(|a| a.kind() == SyntaxKind::PROPERTY_ACCESSOR && ast::token(&a, SyntaxKind::IDENT).is_some_and(|t| t.text() == "set") && has_modifier(&a, "private"));
                let pos = if p.is_var() && !private_setter { Pos::Invariant } else { Pos::Out };
                if let Some(r) = p.receiver() {
                    walk(e, r.syntax(), Pos::In, &params, out);
                }
                if let Some(t) = p.type_ref() {
                    walk(e, t.syntax(), pos, &params, out);
                }
            }
            _ => {}
        }
    }
}

/// Check the type `n` (a `TYPE_REFERENCE` or a type inside one) in position `pos`.
fn walk(e: &Engine, n: &SyntaxNode, pos: Pos, params: &[(String, Variance)], out: &mut Sink) {
    match n.kind() {
        SyntaxKind::TYPE_REFERENCE => {
            // `@UnsafeVariance T` is exempt.
            let unsafe_variance = n.children().filter(|c| c.kind() == SyntaxKind::MODIFIER_LIST).any(|m| m.text().to_string().contains("UnsafeVariance"));
            if !unsafe_variance && let Some(inner) = n.children().find(|c| matches!(c.kind(), SyntaxKind::USER_TYPE | SyntaxKind::NULLABLE_TYPE | SyntaxKind::FUNCTION_TYPE)) {
                walk(e, &inner, pos, params, out);
            }
        }
        SyntaxKind::NULLABLE_TYPE => {
            if let Some(inner) = n.children().find(|c| matches!(c.kind(), SyntaxKind::USER_TYPE | SyntaxKind::FUNCTION_TYPE | SyntaxKind::TYPE_REFERENCE)) {
                walk(e, &inner, pos, params, out);
            }
        }
        SyntaxKind::FUNCTION_TYPE => {
            for r in n.children().filter(|c| c.kind() == SyntaxKind::FUNCTION_TYPE_RECEIVER).flat_map(|r| r.children().collect::<Vec<_>>()) {
                walk(e, &r, pos.flip(), params, out);
            }
            for p in n.children().filter(|c| c.kind() == SyntaxKind::FUNCTION_TYPE_PARAMETER_LIST).flat_map(|l| l.descendants().filter(|d| d.kind() == SyntaxKind::VALUE_PARAMETER).collect::<Vec<_>>()) {
                if let Some(t) = p.children().find(|c| c.kind() == SyntaxKind::TYPE_REFERENCE) {
                    walk(e, &t, pos.flip(), params, out);
                }
            }
            if let Some(ret) = n.children().find(|c| c.kind() == SyntaxKind::TYPE_REFERENCE) {
                walk(e, &ret, pos, params, out);
            }
        }
        SyntaxKind::USER_TYPE => {
            let segments: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == SyntaxKind::USER_TYPE_SEGMENT).collect();
            let Some(last) = segments.last() else { return };
            let args = last.children().find(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST);
            // A class type parameter itself.
            if segments.len() == 1 && args.is_none() {
                let Some(name) = ast::token(last, SyntaxKind::IDENT).map(|t| t.text().to_string()) else { return };
                let Some((_, v)) = params.iter().find(|(p, _)| *p == name) else { return };
                let declared = if *v == Variance::Out { Pos::Out } else { Pos::In };
                if pos != declared {
                    let shown = n.ancestors().filter(|a| a.kind() == SyntaxKind::TYPE_REFERENCE).last().map(|t| t.text().to_string()).unwrap_or_else(|| name.clone());
                    out.report(
                        &TYPE_VARIANCE_CONFLICT,
                        n.text_range(),
                        format!("Type parameter {name} is declared as '{}' but occurs in '{}' position in type {shown}.", declared.name(), pos.name()),
                        None,
                    );
                }
                return;
            }
            let Some(args) = args else { return };
            // The class's declared variances, through use-site projections.
            let Some(tr) = n.ancestors().find_map(ast::TypeRef::cast) else { return };
            let t = e.resolve_syntax_type(&tr);
            let Some(fq) = fq_of(&t) else { return };
            let Some(cls) = e.classifier(fq) else { return };
            let variances: Vec<Variance> = cls.symbol().type_params.iter().map(|p| p.variance).collect();
            // Arguments in order (a `*` may be a bare token rather than a projection node).
            let elements = args.children_with_tokens().filter(|t| !t.kind().is_trivia() && !matches!(t.kind(), SyntaxKind::LT | SyntaxKind::GT | SyntaxKind::COMMA));
            for (i, el) in elements.enumerate() {
                let Some(proj) = el.into_node().filter(|p| p.kind() == SyntaxKind::TYPE_PROJECTION) else { continue };
                let text = proj.text().to_string();
                if text.trim_start().starts_with('*') || modifiers(&proj).is_some_and(|m| m.syntax().text().to_string().contains("UnsafeVariance")) {
                    continue;
                }
                let v = if has_modifier(&proj, "out") {
                    Variance::Out
                } else if has_modifier(&proj, "in") {
                    Variance::In
                } else {
                    match variances.get(i) {
                        Some(v) => *v,
                        None => return,
                    }
                };
                if let Some(inner) = proj.children().find(|c| c.kind() == SyntaxKind::TYPE_REFERENCE) {
                    walk(e, &inner, pos.through(v), params, out);
                }
            }
        }
        _ => {}
    }
}
