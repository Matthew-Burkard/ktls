//! Lowering of Kotlin syntax trees to declaration summaries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::symbols::{Modifiers, Origin, Param, Symbol, SymbolKind, TypeArg, TypeParam, TypeRef, Variance, Visibility};
use crate::syntax::ast::{self, AstNode, Decl};
use crate::syntax::{Parse, SyntaxKind, SyntaxNode, TextRange};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Kotlin,
    KotlinScript,
    Java,
}

impl Language {
    pub fn of(path: &Path) -> Option<Language> {
        match path.extension()?.to_str()? {
            "kt" => Some(Language::Kotlin),
            "kts" => Some(Language::KotlinScript),
            "java" => Some(Language::Java),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Import {
    pub segments: Vec<(String, TextRange)>,
    pub star: bool,
    pub alias: Option<String>,
    pub range: TextRange,
}

impl Import {
    pub fn path(&self) -> String {
        self.segments.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(".")
    }

    /// The name this import introduces into scope (`alias` or last segment).
    pub fn imported_name(&self) -> Option<&str> {
        if self.star {
            return None;
        }
        self.alias.as_deref().or_else(|| self.segments.last().map(|(s, _)| s.as_str()))
    }
}

/// A declaration and its nested declarations.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceDecl {
    pub symbol: Symbol,
    pub children: Vec<SourceDecl>,
}

/// Everything about a file needed for project-wide indexing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileSummary {
    pub path: PathBuf,
    pub language: Language,
    pub package: String,
    pub imports: Vec<Import>,
    pub decls: Vec<SourceDecl>,
    /// `@file:JvmName` (Kotlin) for the facade class name.
    pub jvm_name: Option<String>,
}

impl FileSummary {
    /// Classifier declarations (any depth) with their FQ names.
    pub fn classifiers(&self) -> Vec<&SourceDecl> {
        fn walk<'a>(d: &'a SourceDecl, out: &mut Vec<&'a SourceDecl>) {
            if d.symbol.kind.is_classifier() || d.symbol.kind == SymbolKind::EnumEntry && !d.children.is_empty() {
                out.push(d);
            }
            for c in &d.children {
                walk(c, out);
            }
        }
        let mut out = Vec::new();
        for d in &self.decls {
            walk(d, &mut out);
        }
        out
    }

    /// Hash of what other files can observe (package + declaration signatures).
    pub fn shape_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.package.hash(&mut h);
        fn walk(d: &SourceDecl, h: &mut impl Hasher) {
            d.symbol.kind.hash(h);
            d.symbol.name.hash(h);
            d.symbol.fq_name.hash(h);
            d.symbol.supertypes.hash(h);
            d.symbol.type_params.hash(h);
            d.symbol.params.hash(h);
            d.symbol.ty.hash(h);
            d.symbol.receiver.hash(h);
            d.symbol.visibility.hash(h);
            d.symbol.modifiers.hash(h);
            d.children.len().hash(h);
            for c in &d.children {
                walk(c, h);
            }
        }
        for d in &self.decls {
            walk(d, &mut h);
        }
        h.finish()
    }
}

pub fn summarize(path: &Path, parse: &Parse, language: Language) -> FileSummary {
    let file = parse.file();
    let package = file.package().and_then(|p| p.name()).map(|n| n.text()).unwrap_or_default();
    let imports = file
        .imports()
        .filter_map(|imp| {
            let qn = imp.path()?;
            let segments = qn.segments().iter().map(|t| (ast::ident_text(t), t.text_range())).collect();
            Some(Import { segments, star: imp.is_star(), alias: imp.alias().map(|a| ast::ident_text(&a)), range: imp.range() })
        })
        .collect();
    let jvm_name = file.file_annotations().iter().find_map(|a| {
        let name = a.type_ref()?.syntax().text().to_string();
        if name.trim() != "JvmName" && name.trim() != "kotlin.jvm.JvmName" {
            return None;
        }
        let text = a.syntax().text().to_string();
        let start = text.find('"')? + 1;
        let end = text[start..].find('"')? + start;
        Some(text[start..end].to_string())
    });
    let mut lower = Lowerer { path: path.to_path_buf(), type_params: Vec::new() };
    let mut decls = Vec::new();
    for d in file.declarations() {
        decls.extend(lower.decl(&d, &package, false));
    }
    FileSummary { path: path.to_path_buf(), language, package, imports, decls, jvm_name }
}

/// Lower a syntactic type to a (still unresolved) [`TypeRef`]; names in
/// `type_params` become type parameters.
pub fn lower_type(t: &ast::TypeRef, type_params: &[String]) -> TypeRef {
    Lowerer { path: PathBuf::new(), type_params: type_params.to_vec() }.ty(t)
}

/// Lower the members of a class body (e.g. of an anonymous `object : T { ... }`).
pub fn lower_body_members(path: &Path, body: &ast::ClassBody, container: &str) -> Vec<SourceDecl> {
    let mut l = Lowerer { path: path.to_path_buf(), type_params: Vec::new() };
    body.members().flat_map(|m| l.decl(&m, container, false)).collect()
}

struct Lowerer {
    path: PathBuf,
    /// Type parameter names in scope (outermost first).
    type_params: Vec<String>,
}

fn qualify(container: &str, name: &str) -> String {
    if container.is_empty() { name.to_string() } else { format!("{container}.{name}") }
}

pub fn visibility_of(mods: &Option<ast::ModifierList>) -> Visibility {
    let Some(m) = mods else { return Visibility::Public };
    if m.has("private") {
        Visibility::Private
    } else if m.has("protected") {
        Visibility::Protected
    } else if m.has("internal") {
        Visibility::Internal
    } else {
        Visibility::Public
    }
}

fn modifiers_of(mods: &Option<ast::ModifierList>) -> Modifiers {
    let mut out = Modifiers::default();
    if let Some(m) = mods {
        for t in m.modifiers() {
            out.set(Modifiers::from_keyword(t.text()));
        }
        if m.annotations().any(|a| a.type_ref().is_some_and(|t| t.syntax().text() == "Deprecated")) {
            out.set(Modifiers::DEPRECATED);
        }
    }
    out
}

impl Lowerer {
    fn origin(&self, node: &SyntaxNode, name_range: Option<TextRange>) -> Origin {
        let range = node.text_range();
        Origin::Source { file: self.path.clone(), range, name_range: name_range.unwrap_or(TextRange::empty(range.start())) }
    }

    fn with_type_params<R>(&mut self, tps: &[ast::TypeParameter], f: impl FnOnce(&mut Self, Vec<TypeParam>) -> R) -> R {
        let names: Vec<String> = tps.iter().filter_map(|t| t.name_token()).map(|t| ast::ident_text(&t)).collect();
        let n = names.len();
        self.type_params.extend(names);
        let lowered = tps
            .iter()
            .map(|tp| {
                let mods: Option<ast::ModifierList> = ast::child(tp.syntax());
                TypeParam {
                    name: tp.name_token().map(|t| ast::ident_text(&t)).unwrap_or_default(),
                    bounds: tp.bound().map(|b| self.ty(&b)).into_iter().collect(),
                    variance: match &mods {
                        Some(m) if m.syntax().text().to_string().split_whitespace().any(|w| w == "in") => Variance::In,
                        Some(m) if m.has("out") => Variance::Out,
                        _ => Variance::Invariant,
                    },
                    reified: mods.as_ref().is_some_and(|m| m.has("reified")),
                }
            })
            .collect();
        let r = f(self, lowered);
        self.type_params.truncate(self.type_params.len() - n);
        r
    }

    pub fn ty(&self, t: &ast::TypeRef) -> TypeRef {
        let suspend = t.modifiers().is_some_and(|m| m.has("suspend"));
        let Some(inner) = t.inner() else { return TypeRef::Error };
        let mut out = self.ty_node(&inner);
        if suspend
            && let TypeRef::Function { suspend, .. } = &mut out {
                *suspend = true;
            }
        out
    }

    fn ty_node(&self, n: &SyntaxNode) -> TypeRef {
        match n.kind() {
            SyntaxKind::USER_TYPE => {
                let ut = ast::UserType::cast(n.clone()).unwrap();
                let segs = ut.segments();
                let names: Vec<String> = segs.iter().filter_map(|s| s.name_token()).map(|t| ast::ident_text(&t)).collect();
                let args = segs
                    .last()
                    .and_then(|s| s.type_arguments())
                    .map(|al| {
                        al.arguments()
                            .map(|a| {
                                if a.kind() == SyntaxKind::STAR_PROJECTION {
                                    return TypeArg::Star;
                                }
                                let mods: Option<ast::ModifierList> = ast::child(&a);
                                let variance = match &mods {
                                    Some(m) if m.syntax().text().to_string().split_whitespace().any(|w| w == "in") => Variance::In,
                                    Some(m) if m.has("out") => Variance::Out,
                                    _ => Variance::Invariant,
                                };
                                let ty = ast::child::<ast::TypeRef>(&a).map(|t| self.ty(&t)).unwrap_or(TypeRef::Error);
                                TypeArg::Type { variance, ty }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if names.len() == 1 && self.type_params.contains(&names[0]) {
                    return TypeRef::TypeParam { name: names[0].clone(), nullable: false };
                }
                TypeRef::Named { name: names.join("."), fq: None, args, nullable: false }
            }
            SyntaxKind::NULLABLE_TYPE => match n.children().next() {
                Some(c) => self.ty_node(&c).nullable(true),
                None => TypeRef::Error,
            },
            SyntaxKind::PARENTHESIZED_TYPE => ast::child::<ast::TypeRef>(n).map(|t| self.ty(&t)).unwrap_or(TypeRef::Error),
            SyntaxKind::DEFINITELY_NON_NULL_TYPE => n.children().next().map(|c| self.ty_node(&c)).unwrap_or(TypeRef::Error),
            SyntaxKind::DYNAMIC_TYPE => TypeRef::Dynamic,
            SyntaxKind::FUNCTION_TYPE => {
                let receiver = n
                    .children()
                    .find(|c| c.kind() == SyntaxKind::FUNCTION_TYPE_RECEIVER)
                    .and_then(|r| ast::child::<ast::TypeRef>(&r))
                    .map(|t| Box::new(self.ty(&t)));
                let params = n
                    .children()
                    .find(|c| c.kind() == SyntaxKind::FUNCTION_TYPE_PARAMETER_LIST)
                    .map(|l| l.children().filter_map(|p| ast::child::<ast::TypeRef>(&p)).map(|t| self.ty(&t)).collect())
                    .unwrap_or_default();
                let ret = ast::children::<ast::TypeRef>(n).last().map(|t| self.ty(&t)).unwrap_or(TypeRef::Error);
                TypeRef::Function { receiver, params, ret: Box::new(ret), suspend: false, nullable: false }
            }
            _ => TypeRef::Error,
        }
    }

    fn params(&self, ps: &[ast::ValueParameter]) -> Vec<Param> {
        ps.iter()
            .map(|p| Param {
                name: p.name_token().map(|t| ast::ident_text(&t)).unwrap_or_default(),
                ty: p.type_ref().map(|t| self.ty(&t)).unwrap_or(TypeRef::Error),
                has_default: p.has_default(),
                vararg: p.is_vararg(),
            })
            .collect()
    }

    fn decl(&mut self, d: &Decl, container: &str, in_interface: bool) -> Vec<SourceDecl> {
        let mods = d.modifiers();
        let name_tok = d.name_token();
        let name = name_tok.as_ref().map(ast::ident_text);
        let name_range = name_tok.as_ref().map(|t| t.text_range());
        let mut modifiers = modifiers_of(&mods);
        let visibility = visibility_of(&mods);
        match d {
            Decl::Class(c) => {
                let Some(name) = name else { return Vec::new() };
                let fq = qualify(container, &name);
                let kind = if c.is_interface() {
                    SymbolKind::Interface
                } else if modifiers.0 & Modifiers::from_keyword("enum") != 0 || mods.as_ref().is_some_and(|m| m.has("enum")) {
                    SymbolKind::Enum
                } else if mods.as_ref().is_some_and(|m| m.has("annotation")) {
                    SymbolKind::Annotation
                } else {
                    SymbolKind::Class
                };
                if c.is_fun_interface() {
                    modifiers.set(Modifiers::FUN_INTERFACE);
                }
                let tps = c.type_parameters();
                self.with_type_params(&tps, |this, type_params| {
                    let mut sym = Symbol::new(kind, &name, fq.clone(), this.origin(c.syntax(), name_range));
                    sym.visibility = visibility;
                    sym.modifiers = modifiers;
                    sym.type_params = type_params;
                    sym.supertypes = c.super_types().iter().filter_map(|s| s.type_ref()).map(|t| this.ty(&t)).collect();
                    sym.doc = d.kdoc();
                    let mut children = Vec::new();
                    if let Some(pc) = c.primary_constructor() {
                        let mut ctor = Symbol::new(SymbolKind::Constructor, "<init>", format!("{fq}.<init>"), this.origin(pc.syntax(), None));
                        ctor.visibility = visibility_of(&ast::child(pc.syntax()));
                        let params = pc.parameters();
                        ctor.params = this.params(&params);
                        ctor.ty = Some(TypeRef::Named { name: fq.clone(), fq: Some(fq.as_str().into()), args: vec![], nullable: false });
                        children.push(SourceDecl { symbol: ctor, children: Vec::new() });
                        for p in params.iter().filter(|p| p.property_keyword().is_some()) {
                            let Some(t) = p.name_token() else { continue };
                            let pname = ast::ident_text(&t);
                            let mut prop = Symbol::new(SymbolKind::Property, &pname, format!("{fq}.{pname}"), this.origin(p.syntax(), Some(t.text_range())));
                            let pmods = p.modifiers();
                            prop.visibility = visibility_of(&pmods);
                            prop.modifiers = modifiers_of(&pmods);
                            if p.property_keyword().is_some_and(|k| k.kind() == SyntaxKind::VAR_KW) {
                                prop.modifiers.set(Modifiers::VAR);
                            }
                            prop.ty = p.type_ref().map(|t| this.ty(&t));
                            children.push(SourceDecl { symbol: prop, children: Vec::new() });
                        }
                    }
                    if let Some(body) = c.body() {
                        let iface = kind == SymbolKind::Interface;
                        for m in body.members() {
                            children.extend(this.decl(&m, &fq, iface));
                        }
                    }
                    vec![SourceDecl { symbol: sym, children }]
                })
            }
            Decl::Object(o) => {
                let is_companion = mods.as_ref().is_some_and(|m| m.has("companion"));
                let name = name.unwrap_or_else(|| if is_companion { "Companion".into() } else { String::new() });
                if name.is_empty() {
                    return Vec::new();
                }
                let fq = qualify(container, &name);
                let mut sym = Symbol::new(SymbolKind::Object, &name, fq.clone(), self.origin(o.syntax(), name_range));
                sym.visibility = visibility;
                sym.modifiers = modifiers;
                sym.supertypes = o.super_types().iter().filter_map(|s| s.type_ref()).map(|t| self.ty(&t)).collect();
                sym.doc = d.kdoc();
                let mut children = Vec::new();
                if let Some(body) = o.body() {
                    for m in body.members() {
                        children.extend(self.decl(&m, &fq, false));
                    }
                }
                vec![SourceDecl { symbol: sym, children }]
            }
            Decl::Fun(f) => {
                let Some(name) = name else { return Vec::new() };
                let tps = f.type_parameters();
                self.with_type_params(&tps, |this, type_params| {
                    let mut sym = Symbol::new(SymbolKind::Function, &name, qualify(container, &name), this.origin(f.syntax(), name_range));
                    sym.visibility = visibility;
                    sym.modifiers = modifiers;
                    if in_interface && f.body_block().is_none() && !f.has_expression_body() {
                        sym.modifiers.set(Modifiers::ABSTRACT);
                    }
                    sym.type_params = type_params;
                    sym.receiver = f.receiver().map(|r| this.ty(&r));
                    sym.params = this.params(&f.parameters());
                    sym.ty = match f.return_type() {
                        Some(t) => Some(this.ty(&t)),
                        // Block body without a declared type returns Unit.
                        None if !f.has_expression_body() => Some(TypeRef::named("kotlin.Unit")),
                        None => None,
                    };
                    sym.doc = d.kdoc();
                    vec![SourceDecl { symbol: sym, children: Vec::new() }]
                })
            }
            Decl::Property(p) => {
                if p.is_destructuring() {
                    // Top-level / member destructuring is not allowed; local ones are not indexed.
                    return Vec::new();
                }
                let Some(name) = name else { return Vec::new() };
                let tps = p.type_parameters();
                self.with_type_params(&tps, |this, type_params| {
                    let mut sym = Symbol::new(SymbolKind::Property, &name, qualify(container, &name), this.origin(p.syntax(), name_range));
                    sym.visibility = visibility;
                    sym.modifiers = modifiers;
                    if p.is_var() {
                        sym.modifiers.set(Modifiers::VAR);
                    }
                    // Interface properties without an initializer or accessors are abstract.
                    if in_interface && !p.has_initializer() && !p.has_delegate() && p.accessors().next().is_none() {
                        sym.modifiers.set(Modifiers::ABSTRACT);
                    }
                    sym.type_params = type_params;
                    sym.receiver = p.receiver().map(|r| this.ty(&r));
                    sym.ty = p.type_ref().map(|t| this.ty(&t));
                    sym.doc = d.kdoc();
                    vec![SourceDecl { symbol: sym, children: Vec::new() }]
                })
            }
            Decl::TypeAlias(a) => {
                let Some(name) = name else { return Vec::new() };
                let tps = a.type_parameters();
                self.with_type_params(&tps, |this, type_params| {
                    let mut sym = Symbol::new(SymbolKind::TypeAlias, &name, qualify(container, &name), this.origin(a.syntax(), name_range));
                    sym.visibility = visibility;
                    sym.modifiers = modifiers;
                    sym.type_params = type_params;
                    sym.ty = a.aliased().map(|t| this.ty(&t));
                    sym.doc = d.kdoc();
                    vec![SourceDecl { symbol: sym, children: Vec::new() }]
                })
            }
            Decl::Constructor(c) => {
                let mut sym = Symbol::new(SymbolKind::Constructor, "<init>", qualify(container, "<init>"), self.origin(c.syntax(), None));
                sym.visibility = visibility;
                sym.params = self.params(&c.parameters());
                sym.ty = Some(TypeRef::Named { name: container.to_string(), fq: Some(container.into()), args: vec![], nullable: false });
                vec![SourceDecl { symbol: sym, children: Vec::new() }]
            }
            Decl::EnumEntry(e) => {
                let Some(name) = name else { return Vec::new() };
                let mut sym = Symbol::new(SymbolKind::EnumEntry, &name, qualify(container, &name), self.origin(e.syntax(), name_range));
                sym.ty = Some(TypeRef::Named { name: container.to_string(), fq: Some(container.into()), args: vec![], nullable: false });
                let mut children = Vec::new();
                if let Some(body) = ast::child::<ast::ClassBody>(e.syntax()) {
                    for m in body.members() {
                        children.extend(self.decl(&m, &qualify(container, &name), false));
                    }
                }
                vec![SourceDecl { symbol: sym, children }]
            }
        }
    }
}

pub type SummaryRef = Arc<FileSummary>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax;

    fn summary(text: &str) -> FileSummary {
        let p = syntax::parse(text, false);
        summarize(Path::new("/x/Test.kt"), &p, Language::Kotlin)
    }

    #[test]
    fn lowers_declarations() {
        let s = summary(
            r#"@file:JvmName("Utils")
package com.example

import kotlinx.coroutines.flow.Flow
import java.util.*
import foo.Bar as Baz

/** A user */
data class User<T : Comparable<T>>(val name: String, var age: Int = 0, other: T) : Base(), Comparable<User<T>> {
    fun greet(prefix: String?): String = "$prefix $name"
    companion object { const val MAX = 3 }
    inner class Nested
    enum class Kind { A, B }
}

interface Repo { fun all(): Flow<User<String>>; fun count() = 0 }
fun <T> List<T>.second(): T = this[1]
val Int.isEven: Boolean get() = this % 2 == 0
typealias Users = List<User<String>>
suspend fun run(block: suspend Scope.(Int) -> Unit) {}
"#,
        );
        assert_eq!(s.package, "com.example");
        assert_eq!(s.jvm_name.as_deref(), Some("Utils"));
        assert_eq!(s.imports.len(), 3);
        assert!(s.imports[1].star);
        assert_eq!(s.imports[2].imported_name(), Some("Baz"));
        let user = &s.decls[0];
        assert_eq!(user.symbol.fq_name, "com.example.User");
        assert!(user.symbol.modifiers.has(Modifiers::DATA));
        assert_eq!(user.symbol.doc.as_deref(), Some("/** A user */"));
        assert_eq!(user.symbol.signature(), "data class User<T : Comparable<T>> : Base, Comparable<User<T>>");
        let names: Vec<_> = user.children.iter().map(|c| format!("{:?}:{}", c.symbol.kind, c.symbol.name)).collect();
        assert_eq!(names, vec!["Constructor:<init>", "Property:name", "Property:age", "Function:greet", "Object:Companion", "Class:Nested", "Enum:Kind"]);
        assert_eq!(user.children[0].symbol.signature(), "constructor(name: String, age: Int = …, other: T)");
        assert_eq!(user.children[3].symbol.signature(), "fun greet(prefix: String?): String");
        let kind = &user.children[6];
        assert_eq!(kind.symbol.fq_name, "com.example.User.Kind");
        assert_eq!(kind.children.len(), 2);
        let repo = &s.decls[1];
        assert!(repo.children[0].symbol.modifiers.has(Modifiers::ABSTRACT));
        assert!(!repo.children[1].symbol.modifiers.has(Modifiers::ABSTRACT));
        assert_eq!(s.decls[2].symbol.signature(), "fun <T> List<T>.second(): T");
        assert_eq!(s.decls[3].symbol.signature(), "val Int.isEven: Boolean");
        assert_eq!(s.decls[4].symbol.signature(), "typealias Users = List<User<String>>");
        assert_eq!(s.decls[5].symbol.signature(), "suspend fun run(block: suspend Scope.(Int) -> Unit): Unit");
        let classifiers: Vec<_> = s.classifiers().iter().map(|d| d.symbol.fq_name.clone()).collect();
        assert_eq!(
            classifiers,
            vec!["com.example.User", "com.example.User.Companion", "com.example.User.Nested", "com.example.User.Kind", "com.example.Repo", "com.example.Users"]
        );
    }

    #[test]
    fn shape_hash_ignores_bodies() {
        let a = summary("fun f() { println(1) }\nclass A");
        let b = summary("fun f() {\n    println(2)\n    println(3)\n}\nclass A");
        let c = summary("fun f(x: Int) {}\nclass A");
        assert_eq!(a.shape_hash(), b.shape_hash());
        assert_ne!(a.shape_hash(), c.shape_hash());
    }
}
