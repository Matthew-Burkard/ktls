//! Annotation placement: `WRONG_ANNOTATION_TARGET` for annotations whose `@Target` (read
//! from project sources, or known for common library annotations) excludes the annotated
//! element, and `@JvmStatic` outside objects and companions.

use super::super::*;
use crate::analysis::ide::infer::{Engine, fq_of};
use crate::symbols::Origin;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for a in cx.root.descendants().filter(|n| n.kind() == SyntaxKind::ANNOTATION) {
        if !cx.in_error(&a) {
            annotation(cx, e, &a, out);
        }
    }
}

/// `@Target`s of library annotations (the class files' targets are not read).
const KNOWN: &[(&str, &[&str])] = &[
    ("kotlin.jvm.JvmStatic", &["FUNCTION", "PROPERTY", "PROPERTY_GETTER", "PROPERTY_SETTER"]),
    ("kotlin.jvm.JvmField", &["FIELD"]),
    ("kotlin.jvm.JvmOverloads", &["FUNCTION", "CONSTRUCTOR"]),
    ("kotlin.jvm.JvmName", &["FUNCTION", "PROPERTY_GETTER", "PROPERTY_SETTER", "FILE"]),
    ("kotlin.jvm.Volatile", &["FIELD"]),
    ("kotlin.concurrent.Volatile", &["FIELD"]),
    ("kotlin.jvm.Transient", &["FIELD"]),
    ("kotlin.jvm.Synchronized", &["FUNCTION", "PROPERTY_GETTER", "PROPERTY_SETTER"]),
    ("kotlin.jvm.JvmInline", &["CLASS"]),
    ("kotlin.jvm.Throws", &["FUNCTION", "PROPERTY_GETTER", "PROPERTY_SETTER", "CONSTRUCTOR"]),
    ("kotlin.jvm.Strictfp", &["FUNCTION", "CONSTRUCTOR", "PROPERTY_GETTER", "PROPERTY_SETTER", "CLASS"]),
    ("kotlin.PublishedApi", &["CLASS", "CONSTRUCTOR", "FUNCTION", "PROPERTY"]),
];

/// The annotated element: the targets it offers (an annotation fits if it allows any) and
/// its description.
fn element(a: &SyntaxNode) -> Option<(Vec<&'static str>, &'static str)> {
    let list = a.ancestors().skip(1).find(|p| p.kind() != SyntaxKind::ANNOTATION_GROUP)?;
    if list.kind() == SyntaxKind::ANNOTATED_EXPR {
        return Some((vec!["EXPRESSION"], "expression"));
    }
    if list.kind() != SyntaxKind::MODIFIER_LIST {
        return None;
    }
    let owner = list.parent()?;
    let in_interface = || owner.parent().and_then(|b| b.parent()).is_some_and(|c| c.kind() == SyntaxKind::CLASS && ast::token(&c, SyntaxKind::INTERFACE_KW).is_some());
    Some(match owner.kind() {
        SyntaxKind::CLASS if has_modifier(&owner, "annotation") => (vec!["ANNOTATION_CLASS", "CLASS"], "annotation class"),
        SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => (vec!["CLASS"], "class"),
        SyntaxKind::FUN => (vec!["FUNCTION"], "function"),
        SyntaxKind::PROPERTY if owner.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::BLOCK | SyntaxKind::SCRIPT_BODY)) => (vec!["LOCAL_VARIABLE"], "local variable"),
        SyntaxKind::PROPERTY => {
            let p = ast::PropertyDecl::cast(owner.clone())?;
            // No backing field: no `FIELD` target.
            let field = !(in_interface() || has_modifier(&owner, "abstract") || p.receiver().is_some() || has_modifier(&owner, "expect"));
            if field { (vec!["PROPERTY", "FIELD"], "member property with backing field") } else { (vec!["PROPERTY"], "member property without backing field or delegate") }
        }
        SyntaxKind::VALUE_PARAMETER => {
            let property = ast::token(&owner, SyntaxKind::VAL_KW).is_some() || ast::token(&owner, SyntaxKind::VAR_KW).is_some();
            if property { (vec!["VALUE_PARAMETER", "PROPERTY", "FIELD"], "value parameter") } else { (vec!["VALUE_PARAMETER"], "value parameter") }
        }
        SyntaxKind::PRIMARY_CONSTRUCTOR | SyntaxKind::SECONDARY_CONSTRUCTOR => (vec!["CONSTRUCTOR"], "constructor"),
        SyntaxKind::PROPERTY_ACCESSOR => match ast::token(&owner, SyntaxKind::IDENT)?.text() {
            "get" => (vec!["PROPERTY_GETTER"], "getter"),
            "set" => (vec!["PROPERTY_SETTER"], "setter"),
            _ => return None,
        },
        SyntaxKind::TYPEALIAS => (vec!["TYPEALIAS"], "typealias"),
        SyntaxKind::TYPE_PARAMETER => (vec!["TYPE_PARAMETER"], "type parameter"),
        SyntaxKind::TYPE_REFERENCE => (vec!["TYPE"], "type usage"),
        _ => return None,
    })
}

/// The `@Target` of a project annotation class, if it has one.
fn source_targets(cx: &Cx, e: &Engine, fq: &str) -> Option<Vec<String>> {
    let c = e.classifier(fq)?;
    let Origin::Source { file, range, .. } = &c.symbol().origin else { return None };
    let root = if file == cx.path { cx.root.clone() } else { cx.snap.file(file)?.parse().syntax() };
    let decl = node_at(&root, *range, &[SyntaxKind::CLASS])?;
    let target = modifiers(&decl)?.annotations().find(|a| a.syntax().children().find(|c| c.kind() == SyntaxKind::TYPE_REFERENCE).is_some_and(|t| matches!(t.text().to_string().as_str(), "Target" | "kotlin.annotation.Target")))?;
    let args = target.syntax().children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST)?;
    let mut out = Vec::new();
    for v in args.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT) {
        let last = v.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF).last()?;
        out.push(last.text().to_string());
    }
    Some(out)
}

/// Arguments that are not compile-time constants: project `val`s that are not `const`, and
/// calls other than array constructors and annotation instantiations.
fn constant_arguments(e: &Engine, a: &SyntaxNode, out: &mut Sink) {
    use crate::analysis::ide::infer::Def;
    use crate::symbols::{Modifiers, SymbolKind};
    let Some(list) = a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST) else { return };
    for arg in list.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT) {
        let Some(value) = arg.children().filter(|c| c.kind() != SyntaxKind::VALUE_ARGUMENT_NAME).last() else { continue };
        let name = match value.kind() {
            SyntaxKind::NAME_REF => Some(value.clone()),
            SyntaxKind::DOT_QUALIFIED_EXPR => value.last_child().filter(|l| l.kind() == SyntaxKind::NAME_REF),
            _ => None,
        };
        let Some(name) = name else { continue };
        let defs = e.resolve_name_ref(&name);
        let non_const = match defs.as_slice() {
            [Def::Symbol { sym, .. }] => sym.kind == SymbolKind::Property && !sym.modifiers.has(Modifiers::CONST) && matches!(sym.origin, Origin::Source { .. }),
            [Def::Local(_)] => true,
            _ => false,
        };
        if non_const {
            out.report(&ANNOTATION_ARGUMENT_MUST_BE_CONST, value.text_range(), "An annotation argument must be a compile-time constant.", None);
        }
    }
}

fn annotation(cx: &Cx, e: &Engine, a: &SyntaxNode, out: &mut Sink) {
    constant_arguments(e, a, out);
    // A use-site target (`@get:`, `@field:`) picks the element explicitly.
    if a.children().any(|c| c.kind() == SyntaxKind::ANNOTATION_TARGET) {
        return;
    }
    let Some(tr) = a.children().find_map(ast::TypeRef::cast) else { return };
    let t = e.resolve_syntax_type(&tr);
    let Some(fq) = fq_of(&t) else { return };
    let Some((offered, what)) = element(a) else { return };
    let allowed: Vec<String> = match KNOWN.iter().find(|(k, _)| *k == fq) {
        Some((_, targets)) => targets.iter().map(|s| s.to_string()).collect(),
        None => match source_targets(cx, e, fq) {
            Some(t) => t,
            None => return,
        },
    };
    if !offered.iter().any(|o| allowed.iter().any(|a| a == o)) {
        out.report(&WRONG_ANNOTATION_TARGET, a.text_range(), format!("This annotation is not applicable to target '{what}'."), Some(Fix::delete("Remove inapplicable annotation", a.text_range())));
        return;
    }
    if fq == "kotlin.jvm.JvmStatic" {
        jvm_static(a, out);
    }
}

/// `@JvmStatic` on a member of a class (not an object or companion) or at top level.
fn jvm_static(a: &SyntaxNode, out: &mut Sink) {
    let Some(decl) = a.ancestors().find(|p| matches!(p.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::PROPERTY_ACCESSOR)) else { return };
    let decl = if decl.kind() == SyntaxKind::PROPERTY_ACCESSOR { match decl.parent() { Some(p) => p, None => return } } else { decl };
    let container = decl.parent().filter(|p| p.kind() == SyntaxKind::CLASS_BODY).and_then(|b| b.parent());
    let in_object = container.as_ref().is_some_and(|c| c.kind() == SyntaxKind::OBJECT_DECLARATION);
    let local = decl.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::BLOCK | SyntaxKind::SCRIPT_BODY));
    if !in_object && !local {
        out.report(&JVM_STATIC_NOT_IN_OBJECT_OR_COMPANION, a.text_range(), "Only members in named objects and companion objects can be annotated with '@JvmStatic'.", Some(Fix::delete("Remove '@JvmStatic'", a.text_range())));
    }
}
