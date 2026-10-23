//! JVM signature clashes: declarations of one class (or file facade) that compile to the
//! same method name and erased parameter types (`val x` and `fun getX()`,
//! `f(List<String>)` and `f(List<Int>)`).

use super::super::*;
use crate::analysis::ide::infer::Engine;
use crate::symbols::{Modifiers, TypeArg, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    // Files with `@file:JvmName` / `@file:JvmMultifileClass` share or rename the facade.
    let file_annotations = cx.root.children().filter(|c| c.kind() == SyntaxKind::FILE_ANNOTATION_LIST).any(|l| l.text().to_string().contains("Jvm"));
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::CLASS_BODY => container(e, &n, out),
            SyntaxKind::KOTLIN_FILE if !file_annotations => container(e, &n, out),
            _ => {}
        }
    }
}

/// A JVM method a declaration compiles to: its name and erased parameter and return types.
struct Method {
    name: String,
    params: Vec<String>,
    ret: String,
    /// What to show and where (the declaration's name).
    shown: String,
    at: TextRange,
    /// Written parameter types (identical ones are `CONFLICTING_OVERLOADS` instead).
    written: Vec<String>,
    function: bool,
}

/// JVM class of a Kotlin built-in or mapped type.
fn jvm_class(fq: &str, nullable: bool) -> String {
    let primitive = match fq {
        "kotlin.Int" => Some(("int", "java.lang.Integer")),
        "kotlin.Long" => Some(("long", "java.lang.Long")),
        "kotlin.Short" => Some(("short", "java.lang.Short")),
        "kotlin.Byte" => Some(("byte", "java.lang.Byte")),
        "kotlin.Char" => Some(("char", "java.lang.Character")),
        "kotlin.Boolean" => Some(("boolean", "java.lang.Boolean")),
        "kotlin.Float" => Some(("float", "java.lang.Float")),
        "kotlin.Double" => Some(("double", "java.lang.Double")),
        _ => None,
    };
    if let Some((p, boxed)) = primitive {
        return if nullable { boxed } else { p }.to_string();
    }
    match fq {
        "kotlin.Any" => "java.lang.Object",
        "kotlin.String" => "java.lang.String",
        "kotlin.CharSequence" => "java.lang.CharSequence",
        "kotlin.Number" => "java.lang.Number",
        "kotlin.Comparable" => "java.lang.Comparable",
        "kotlin.Throwable" => "java.lang.Throwable",
        "kotlin.Enum" => "java.lang.Enum",
        "kotlin.Unit" => "kotlin.Unit",
        "kotlin.collections.List" | "kotlin.collections.MutableList" => "java.util.List",
        "kotlin.collections.Collection" | "kotlin.collections.MutableCollection" => "java.util.Collection",
        "kotlin.collections.Set" | "kotlin.collections.MutableSet" => "java.util.Set",
        "kotlin.collections.Map" | "kotlin.collections.MutableMap" => "java.util.Map",
        "kotlin.collections.Iterable" | "kotlin.collections.MutableIterable" => "java.lang.Iterable",
        "kotlin.collections.Iterator" | "kotlin.collections.MutableIterator" => "java.util.Iterator",
        "kotlin.collections.ListIterator" | "kotlin.collections.MutableListIterator" => "java.util.ListIterator",
        "kotlin.collections.Map.Entry" | "kotlin.collections.MutableMap.MutableEntry" => "java.util.Map$Entry",
        other => other,
    }
    .to_string()
}

/// The erased JVM type of `t`; `None` when unknown (type parameters, value classes,
/// unresolved types).
fn erase(e: &Engine, t: &TypeRef) -> Option<String> {
    match t {
        TypeRef::Platform(inner) => erase(e, inner),
        TypeRef::Function { receiver, params, suspend, .. } => {
            let n = params.len() + usize::from(receiver.is_some()) + usize::from(*suspend);
            Some(format!("kotlin.jvm.functions.Function{n}"))
        }
        TypeRef::Named { fq: Some(fq), args, nullable, .. } => {
            if fq.as_ref() == "kotlin.Array" {
                let elem = match args.first()? {
                    TypeArg::Type { ty, .. } => erase(e, &ty.clone().nullable(true))?,
                    TypeArg::Star => "java.lang.Object".into(),
                };
                return Some(format!("{elem}[]"));
            }
            // Value classes are mangled; type aliases need expanding.
            let c = e.classifier(fq)?;
            if c.symbol().modifiers.has(Modifiers::VALUE) || c.symbol().kind == crate::symbols::SymbolKind::TypeAlias {
                return None;
            }
            Some(jvm_class(fq, *nullable))
        }
        _ => None,
    }
}

/// `x` → `getX`, `isOpen` → `isOpen`; and the setter name.
fn accessor_names(name: &str) -> (String, String) {
    let is_prefixed = name.starts_with("is") && name[2..].chars().next().is_some_and(|c| !c.is_lowercase());
    let cap = |s: &str| {
        let mut c = s.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    };
    if is_prefixed { (name.to_string(), format!("set{}", &name[2..])) } else { (format!("get{}", cap(name)), format!("set{}", cap(name))) }
}

/// Annotated with a JVM name or field annotation (`@JvmName`, `@get:JvmName`, `@JvmField`,
/// `@JvmStatic`, `@JvmOverloads`), which change the generated methods.
fn jvm_annotated(n: &SyntaxNode) -> bool {
    modifiers(n).is_some_and(|m| m.annotations().any(|a| a.syntax().text().to_string().contains("Jvm")))
}

fn written_type(e: &Engine, tr: Option<ast::TypeRef>) -> Option<(String, TypeRef)> {
    let tr = tr?;
    Some((tr.syntax().text().to_string().split_whitespace().collect(), e.resolve_syntax_type(&tr)))
}

fn methods(e: &Engine, decl: &SyntaxNode) -> Vec<Method> {
    let Some(name) = name_token(decl) else { return Vec::new() };
    if jvm_annotated(decl) || has_modifier(decl, "expect") || has_modifier(decl, "external") {
        return Vec::new();
    }
    let n = ast::ident_text(&name);
    match decl.kind() {
        SyntaxKind::FUN => {
            let Some(f) = ast::FunDecl::cast(decl.clone()) else { return Vec::new() };
            // Generic functions erase to their bounds: left out.
            if decl.children().any(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST) {
                return Vec::new();
            }
            let mut params = Vec::new();
            let mut written = Vec::new();
            if let Some(r) = f.receiver() {
                let Some((w, t)) = written_type(e, Some(r)) else { return Vec::new() };
                let Some(j) = erase(e, &t) else { return Vec::new() };
                params.push(j);
                written.push(w);
            }
            for p in decl.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER) {
                let Some((w, t)) = written_type(e, ast::child::<ast::TypeRef>(&p)) else { return Vec::new() };
                let t = if has_modifier(&p, "vararg") { TypeRef::with_args("kotlin.Array", vec![TypeArg::Type { variance: crate::symbols::Variance::Out, ty: t }]) } else { t };
                let Some(j) = erase(e, &t) else { return Vec::new() };
                params.push(j);
                written.push(w);
            }
            if has_modifier(decl, "suspend") {
                params.push("kotlin.coroutines.Continuation".into());
            }
            let ret = match f.return_type() {
                Some(r) => match erase(e, &e.resolve_syntax_type(&r)) {
                    Some(j) => j,
                    None => return Vec::new(),
                },
                None if f.body_block().is_some() => "kotlin.Unit".into(),
                None => return Vec::new(),
            };
            let ret = if ret == "kotlin.Unit" { "void".into() } else { ret };
            vec![Method { name: n.clone(), params, ret, shown: format!("fun {n}"), at: name.text_range(), written, function: true }]
        }
        SyntaxKind::PROPERTY => {
            let Some(p) = ast::PropertyDecl::cast(decl.clone()) else { return Vec::new() };
            let accessors: Vec<SyntaxNode> = decl.children().filter(|c| c.kind() == SyntaxKind::PROPERTY_ACCESSOR).collect();
            // Private properties with default accessors have no accessor methods; `const`
            // and fields have none either.
            let custom = accessors.iter().any(|a| ast::token(a, SyntaxKind::EQ).is_some() || a.children().any(|c| c.kind() == SyntaxKind::BLOCK));
            if has_modifier(decl, "private") && !custom || has_modifier(decl, "const") || p.is_destructuring() || accessors.iter().any(jvm_annotated) {
                return Vec::new();
            }
            let Some((_, t)) = written_type(e, p.type_ref()) else { return Vec::new() };
            let Some(ty) = erase(e, &t) else { return Vec::new() };
            let mut receiver = Vec::new();
            if let Some(r) = p.receiver() {
                let Some(j) = erase(e, &e.resolve_syntax_type(&r)) else { return Vec::new() };
                receiver.push(j);
            }
            let (get, set) = accessor_names(&n);
            let mut out = vec![Method { name: get, params: receiver.clone(), ret: ty.clone(), shown: format!("val {n}"), at: name.text_range(), written: Vec::new(), function: false }];
            if p.is_var() {
                let mut params = receiver;
                params.push(ty);
                out.push(Method { name: set, params, ret: "void".into(), shown: format!("var {n}"), at: name.text_range(), written: Vec::new(), function: false });
            }
            out
        }
        _ => Vec::new(),
    }
}

fn container(e: &Engine, c: &SyntaxNode, out: &mut Sink) {
    let all: Vec<Method> = c.children().filter(|d| matches!(d.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY)).flat_map(|d| methods(e, &d)).collect();
    for (i, a) in all.iter().enumerate() {
        for (j, b) in all.iter().enumerate() {
            if i == j || a.at == b.at || a.name != b.name || a.params != b.params || a.ret != b.ret {
                continue;
            }
            // Two functions with the same written signature: CONFLICTING_OVERLOADS.
            if a.function && b.function && a.written == b.written {
                continue;
            }
            let signature = format!("{}({})", a.name, a.params.join(", "));
            out.report(&CONFLICTING_JVM_DECLARATIONS, a.at, format!("Platform declaration clash: '{}' and '{}' have the same JVM signature '{signature}'.", a.shown, b.shown), None);
            break;
        }
    }
}
