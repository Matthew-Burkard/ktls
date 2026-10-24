//! `SMARTCAST_IMPOSSIBLE`: a member of a nullable property used after a null check that
//! would smart cast a stable value, when the property is not stable (a `var`, an open
//! property, one with a custom getter or a delegate).

use super::super::*;
use super::certain::shown;
use crate::analysis::ide::infer::{Def, Engine, non_null};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    if !cx.on(&SMARTCAST_IMPOSSIBLE) {
        return;
    }
    for n in cx.root.descendants().filter(|n| n.kind() == SyntaxKind::DOT_QUALIFIED_EXPR) {
        if !in_error(&n) {
            member_access(cx, e, &n, out);
        }
    }
}

/// `x` or `this.x` (whitespace-free text, without `this.`).
fn reference_text(n: &SyntaxNode) -> Option<String> {
    match n.kind() {
        SyntaxKind::NAME_REF => Some(n.text().to_string()),
        SyntaxKind::DOT_QUALIFIED_EXPR => {
            let mut parts = n.children();
            let (Some(this), Some(name), None) = (parts.next(), parts.next(), parts.next()) else { return None };
            (this.kind() == SyntaxKind::THIS_EXPR && this.text() == "this" && name.kind() == SyntaxKind::NAME_REF).then(|| name.text().to_string())
        }
        _ => None,
    }
}

/// `x != null` (`!=` when `non_null`, else `==`), possibly as an `&&` (or `||`) operand.
fn compares_null(c: &SyntaxNode, name: &str, non_null: bool) -> bool {
    match c.kind() {
        SyntaxKind::PAREN_EXPR => c.children().next().is_some_and(|i| compares_null(&i, name, non_null)),
        SyntaxKind::BINARY_EXPR => {
            let (Some(op), Some((l, r))) = (binary_op(c), binary_operands(c)) else { return false };
            let junction = if non_null { SyntaxKind::AMP2 } else { SyntaxKind::PIPE2 };
            if op.kind() == junction {
                return compares_null(&l, name, non_null) || compares_null(&r, name, non_null);
            }
            let want = if non_null { [SyntaxKind::NOT_EQ, SyntaxKind::NOT_EQ2] } else { [SyntaxKind::EQ2, SyntaxKind::EQ3] };
            if !want.contains(&op.kind()) {
                return false;
            }
            let is_null = |x: &SyntaxNode| x.kind() == SyntaxKind::LITERAL && x.text() == "null";
            (is_null(&r) && reference_text(&l).as_deref() == Some(name)) || (is_null(&l) && reference_text(&r).as_deref() == Some(name))
        }
        _ => false,
    }
}

/// A statement that always leaves (`return`, `throw`, `break`, `continue`), or a block of one.
fn leaves(n: &SyntaxNode) -> bool {
    match n.kind() {
        SyntaxKind::RETURN_EXPR | SyntaxKind::THROW_EXPR | SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR => true,
        SyntaxKind::BLOCK | SyntaxKind::THEN_BRANCH => n.children().last().is_some_and(|l| leaves(&l)),
        _ => false,
    }
}

/// Whether a null check before `at` would make `name` non-null there if it were stable.
fn checked(at: &SyntaxNode, name: &str) -> bool {
    let mut cur = at.clone();
    while let Some(p) = cur.parent() {
        match p.kind() {
            SyntaxKind::FUN | SyntaxKind::CLASS_BODY | SyntaxKind::KOTLIN_FILE | SyntaxKind::LAMBDA_EXPR => return false,
            SyntaxKind::IF_EXPR => {
                let cond = p.children().find(|c| c.kind() == SyntaxKind::CONDITION).and_then(|c| c.children().next());
                if let Some(cond) = cond
                    && (cur.kind() == SyntaxKind::THEN_BRANCH && compares_null(&cond, name, true) || cur.kind() == SyntaxKind::ELSE_BRANCH && compares_null(&cond, name, false))
                {
                    return true;
                }
            }
            SyntaxKind::BINARY_EXPR => {
                if let (Some(op), Some((l, r))) = (binary_op(&p), binary_operands(&p))
                    && r == cur
                    && (op.kind() == SyntaxKind::AMP2 && compares_null(&l, name, true) || op.kind() == SyntaxKind::PIPE2 && compares_null(&l, name, false))
                {
                    return true;
                }
            }
            SyntaxKind::BLOCK => {
                // `if (x == null) return` earlier in the block.
                let guarded = p.children().take_while(|s| *s != cur).any(|s| {
                    s.kind() == SyntaxKind::IF_EXPR
                        && !s.children().any(|c| c.kind() == SyntaxKind::ELSE_BRANCH)
                        && s.children().find(|c| c.kind() == SyntaxKind::CONDITION).and_then(|c| c.children().next()).is_some_and(|c| compares_null(&c, name, false))
                        && s.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH).is_some_and(|t| leaves(&t))
                });
                if guarded {
                    return true;
                }
            }
            _ => {}
        }
        cur = p;
    }
    false
}

/// Why `sym` cannot be smart cast; `None` if it can (or that is unknown).
fn instability(cx: &Cx, sym: &Symbol) -> Option<&'static str> {
    if sym.modifiers.has(Modifiers::VAR) {
        return Some("is a mutable property that could have been changed by this time");
    }
    if sym.modifiers.has(Modifiers::OPEN) || sym.modifiers.has(Modifiers::ABSTRACT) {
        return Some("is a property that has an open or custom getter");
    }
    // Custom getters and delegates are in the declaration's syntax (this file only).
    let Origin::Source { file, range, .. } = &sym.origin else { return None };
    if file != cx.path {
        return None;
    }
    let decl = cx.root.covering_element(*range).ancestors().find(|a| a.kind() == SyntaxKind::PROPERTY && a.text_range() == *range)?;
    let p = ast::PropertyDecl::cast(decl.clone())?;
    if p.has_delegate() {
        return Some("is a delegated property");
    }
    let custom_getter = decl.children().filter(|c| c.kind() == SyntaxKind::PROPERTY_ACCESSOR).any(|a| ast::token(&a, SyntaxKind::IDENT).is_some_and(|t| t.text() == "get") && (ast::token(&a, SyntaxKind::EQ).is_some() || a.children().any(|c| c.kind() == SyntaxKind::BLOCK)));
    custom_getter.then_some("is a property that has an open or custom getter")
}

fn member_access(cx: &Cx, e: &Engine, n: &SyntaxNode, out: &mut Sink) {
    let mut parts = n.children();
    let (Some(recv), Some(sel), None) = (parts.next(), parts.next(), parts.next()) else { return };
    let Some(name) = reference_text(&recv) else { return };
    let prop_ref = if recv.kind() == SyntaxKind::NAME_REF { recv.clone() } else { match recv.last_child() { Some(l) => l, None => return } };
    let defs = e.resolve_name_ref(&prop_ref);
    let [Def::Symbol { sym, subst }] = defs.as_slice() else { return };
    if sym.kind != SymbolKind::Property || sym.receiver.is_some() {
        return;
    }
    let Some(declared) = sym.ty.as_ref().map(|t| e.resolve_declared(sym, t)) else { return };
    if !matches!(declared, TypeRef::Named { nullable: true, .. }) {
        return;
    }
    // A member of the non-null type (extensions may take nullable receivers).
    let member_ref = match sel.kind() {
        SyntaxKind::NAME_REF => sel.clone(),
        SyntaxKind::CALL_EXPR => match sel.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) {
            Some(c) => c,
            None => return,
        },
        _ => return,
    };
    let member = e.resolve_name_ref(&member_ref);
    if !matches!(member.as_slice(), [Def::Symbol { sym: m, .. }] if m.receiver.is_none() && !matches!(m.name.as_str(), "toString" | "hashCode" | "equals")) {
        return;
    }
    let Some(why) = instability(cx, sym) else { return };
    if !checked(n, &name) {
        return;
    }
    let t = non_null(crate::analysis::ide::infer::substitute(&declared, subst));
    let fixes = match n.children_with_tokens().filter_map(|t| t.into_token()).find(|t| t.kind() == SyntaxKind::DOT) {
        Some(dot) => vec![Fix::replace("Replace with safe (?.) call", dot.text_range(), "?."), Fix::replace("Add non-null asserted (!!.) call", dot.text_range(), "!!.")],
        None => Vec::new(),
    };
    out.report_fixes(&SMARTCAST_IMPOSSIBLE, recv.text_range(), format!("Smart cast to '{}' is impossible, because '{name}' {why}.", shown(&t)), fixes);
}
