//! Labels: `return@l`, `break@l`, `continue@l`, `this@l` and `super@l` must name something
//! around them (a labeled loop or lambda, the function a lambda is passed to, an enclosing
//! function or class); `break` and `continue` need a loop label.

use super::super::*;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for q in cx.root.descendants().filter(|n| n.kind() == SyntaxKind::LABEL_QUALIFIER) {
        if !in_error(&q) {
            qualifier(&q, out);
        }
    }
}

/// What a label names.
#[derive(PartialEq, Eq)]
enum Target {
    Loop,
    Lambda,
    Function,
    Class,
}

/// The explicit label on `n` (`l@ n`).
fn label_of(n: &SyntaxNode) -> Option<String> {
    let labeled = n.parent().filter(|p| p.kind() == SyntaxKind::LABELED_EXPR)?;
    let label = labeled.children().find(|c| c.kind() == SyntaxKind::LABEL)?;
    ast::token(&label, SyntaxKind::IDENT).map(|t| ast::ident_text(&t))
}

/// The name of the function a lambda (or anonymous function) is passed to: its implicit label.
fn callee_label(lambda: &SyntaxNode) -> Option<String> {
    let mut parent = lambda.parent()?;
    if parent.kind() == SyntaxKind::LABELED_EXPR {
        parent = parent.parent()?;
    }
    let call = match parent.kind() {
        SyntaxKind::LAMBDA_ARGUMENT => parent.parent()?,
        SyntaxKind::VALUE_ARGUMENT => parent.parent()?.parent()?,
        _ => return None,
    };
    let callee = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF)?;
    ast::token(&callee, SyntaxKind::IDENT).map(|t| ast::ident_text(&t))
}

/// Every label visible from `at`, with what it names.
fn labels_around(at: &SyntaxNode) -> Vec<(String, Target)> {
    let mut out = Vec::new();
    for a in at.ancestors().skip(1) {
        match a.kind() {
            SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR => out.extend(label_of(&a).map(|l| (l, Target::Loop))),
            SyntaxKind::LAMBDA_EXPR | SyntaxKind::ANONYMOUS_FUNCTION => {
                out.extend(label_of(&a).map(|l| (l, Target::Lambda)));
                out.extend(callee_label(&a).map(|l| (l, Target::Lambda)));
            }
            SyntaxKind::FUN | SyntaxKind::PROPERTY => out.extend(name_token(&a).map(|t| (ast::ident_text(&t), Target::Function))),
            SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => match name_token(&a) {
                Some(t) => out.push((ast::ident_text(&t), Target::Class)),
                // An unnamed companion object.
                None => out.push(("Companion".into(), Target::Class)),
            },
            // Any other labeled expression.
            _ => out.extend(label_of(&a).map(|l| (l, Target::Lambda))),
        }
    }
    out
}

fn qualifier(q: &SyntaxNode, out: &mut Sink) {
    let Some(name_tok) = ast::token(q, SyntaxKind::IDENT) else { return };
    let name = ast::ident_text(&name_tok);
    let Some(owner) = q.parent() else { return };
    let around = labels_around(&owner);
    let targets: Vec<&Target> = around.iter().filter(|(l, _)| *l == name).map(|(_, t)| t).collect();
    match owner.kind() {
        SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR => {
            if targets.is_empty() {
                out.report(&UNRESOLVED_LABEL, q.text_range(), format!("Unresolved label '{name}'."), None);
            } else if !targets.contains(&&Target::Loop) {
                let kw = owner.first_token().map(|t| t.text().to_string()).unwrap_or_default();
                out.report(&NOT_A_LOOP_LABEL, q.text_range(), format!("The label '@{name}' does not denote a loop, so '{kw}' cannot use it."), None);
            }
        }
        SyntaxKind::RETURN_EXPR | SyntaxKind::THIS_EXPR | SyntaxKind::SUPER_EXPR if targets.is_empty() => {
            out.report(&UNRESOLVED_LABEL, q.text_range(), format!("Unresolved label '{name}'."), None);
        }
        _ => {}
    }
}
