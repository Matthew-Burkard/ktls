//! A lightweight Java source scanner: package, imports and declarations
//! (classes, interfaces, enums, records, annotations and their members).

use std::path::Path;

use super::source::{FileSummary, Import, Language, SourceDecl};
use crate::symbols::{Modifiers, Origin, Param, Symbol, SymbolKind, TypeArg, TypeParam, TypeRef, Variance, Visibility};
use crate::syntax::{TextRange, TextSize};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Punct(char),
    Ellipsis,
    Literal,
}

struct Lexed {
    toks: Vec<(Tok, TextRange)>,
    /// Javadoc comment ending right before token i.
    docs: Vec<Option<String>>,
}

fn lex(src: &str) -> Lexed {
    let b = src.as_bytes();
    let mut i = 0;
    let mut toks = Vec::new();
    let mut docs = Vec::new();
    let mut pending_doc: Option<String> = None;
    let range = |s: usize, e: usize| TextRange::new(TextSize::from(s as u32), TextSize::from(e as u32));
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i..].starts_with(b"/*") {
            let start = i;
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            if src[start..].starts_with("/**") {
                pending_doc = Some(src[start..i].to_string());
            }
            continue;
        }
        let start = i;
        if b[i..].starts_with(b"\"\"\"") {
            i += 3;
            while i + 2 < b.len() && !b[i..].starts_with(b"\"\"\"") {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 3).min(b.len());
            toks.push((Tok::Literal, range(start, i)));
        } else if c == b'"' || c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != c && b[i] != b'\n' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            toks.push((Tok::Literal, range(start, i)));
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            toks.push((Tok::Literal, range(start, i)));
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            while !src.is_char_boundary(i) {
                i += 1;
            }
            toks.push((Tok::Ident(src[start..i].to_string()), range(start, i)));
        } else if b[i..].starts_with(b"...") {
            i += 3;
            toks.push((Tok::Ellipsis, range(start, i)));
        } else {
            i += 1;
            while !src.is_char_boundary(i) {
                i += 1;
            }
            toks.push((Tok::Punct(c as char), range(start, i)));
        }
        docs.push(pending_doc.take());
    }
    Lexed { toks, docs }
}

const MODIFIERS: &[&str] = &[
    "public", "protected", "private", "static", "abstract", "final", "native", "synchronized", "transient", "volatile", "strictfp",
    "default", "sealed", "non-sealed",
];

struct P<'a> {
    toks: &'a [(Tok, TextRange)],
    docs: &'a [Option<String>],
    i: usize,
    path: &'a Path,
}

impl P<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i).map(|(t, _)| t)
    }
    fn peek_n(&self, n: usize) -> Option<&Tok> {
        self.toks.get(self.i + n).map(|(t, _)| t)
    }
    fn range(&self) -> TextRange {
        self.toks.get(self.i).map(|(_, r)| *r).unwrap_or_default()
    }
    fn is_punct(&self, c: char) -> bool {
        self.peek() == Some(&Tok::Punct(c))
    }
    fn is_ident(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(x)) if x == s)
    }
    fn ident(&mut self) -> Option<String> {
        match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.i += 1;
                Some(s)
            }
            _ => None,
        }
    }
    fn eat(&mut self, c: char) -> bool {
        if self.is_punct(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    /// Skip a balanced group starting at `open`.
    fn skip_group(&mut self, open: char, close: char) {
        let mut depth = 0;
        while let Some(t) = self.peek() {
            if *t == Tok::Punct(open) {
                depth += 1;
            } else if *t == Tok::Punct(close) {
                depth -= 1;
                if depth == 0 {
                    self.i += 1;
                    return;
                }
            }
            self.i += 1;
        }
    }

    fn qualified(&mut self) -> Vec<(String, TextRange)> {
        let mut out = Vec::new();
        loop {
            let r = self.range();
            match self.ident() {
                Some(s) => out.push((s, r)),
                None => break,
            }
            if self.is_punct('.') && matches!(self.peek_n(1), Some(Tok::Ident(_))) {
                self.i += 1;
            } else {
                break;
            }
        }
        out
    }

    fn annotations(&mut self) -> bool {
        let mut deprecated = false;
        while self.is_punct('@') && !matches!(self.peek_n(1), Some(Tok::Ident(s)) if s == "interface") {
            self.i += 1;
            let q = self.qualified();
            if q.last().is_some_and(|(n, _)| n == "Deprecated") {
                deprecated = true;
            }
            if self.is_punct('(') {
                self.skip_group('(', ')');
            }
        }
        deprecated
    }

    fn modifiers(&mut self) -> (u16, Modifiers) {
        let mut vis = 0u16; // 1 public, 2 protected, 3 private
        let mut m = Modifiers::default();
        loop {
            if self.annotations() {
                m.set(Modifiers::DEPRECATED);
            }
            match self.peek() {
                Some(Tok::Ident(s)) if MODIFIERS.contains(&s.as_str()) || (s == "non" && self.peek_n(1) == Some(&Tok::Punct('-'))) => {
                    match s.as_str() {
                        "public" => vis = 1,
                        "protected" => vis = 2,
                        "private" => vis = 3,
                        "static" => m.set(Modifiers::STATIC),
                        "abstract" => m.set(Modifiers::ABSTRACT),
                        "final" => m.set(Modifiers::FINAL),
                        "sealed" => m.set(Modifiers::SEALED),
                        "non" => self.i += 2, // non-sealed
                        _ => {}
                    }
                    self.i += 1;
                }
                _ => return (vis, m),
            }
        }
    }

    fn type_params(&mut self) -> Vec<TypeParam> {
        let mut out = Vec::new();
        if !self.eat('<') {
            return out;
        }
        while !self.is_punct('>') && self.peek().is_some() {
            self.annotations();
            let Some(name) = self.ident() else { break };
            let mut bounds = Vec::new();
            if self.is_ident("extends") {
                self.i += 1;
                loop {
                    bounds.push(self.ty());
                    if !self.eat('&') {
                        break;
                    }
                }
            }
            out.push(TypeParam { name, bounds, variance: Variance::Invariant, reified: false });
            if !self.eat(',') {
                break;
            }
        }
        self.eat('>');
        out
    }

    fn ty(&mut self) -> TypeRef {
        self.annotations();
        let names = self.qualified();
        if names.is_empty() {
            if self.is_punct('?') {
                self.i += 1;
                return TypeRef::named("kotlin.Any").nullable(true);
            }
            return TypeRef::Error;
        }
        let mut args = Vec::new();
        if self.eat('<') {
            while !self.is_punct('>') && self.peek().is_some() {
                if self.eat('?') {
                    if self.is_ident("extends") {
                        self.i += 1;
                        args.push(TypeArg::Type { variance: Variance::Out, ty: self.ty() });
                    } else if self.is_ident("super") {
                        self.i += 1;
                        args.push(TypeArg::Type { variance: Variance::In, ty: self.ty() });
                    } else {
                        args.push(TypeArg::Star);
                    }
                } else {
                    args.push(TypeArg::Type { variance: Variance::Invariant, ty: self.ty() });
                }
                if !self.eat(',') {
                    break;
                }
            }
            self.eat('>');
            // `Outer<A>.Inner`
            while self.is_punct('.') {
                self.i += 1;
                self.qualified();
                if self.is_punct('<') {
                    self.skip_group('<', '>');
                }
            }
        }
        let name: String = names.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(".");
        let mut t = match name.as_str() {
            "int" => TypeRef::named("kotlin.Int"),
            "long" => TypeRef::named("kotlin.Long"),
            "boolean" => TypeRef::named("kotlin.Boolean"),
            "byte" => TypeRef::named("kotlin.Byte"),
            "short" => TypeRef::named("kotlin.Short"),
            "char" => TypeRef::named("kotlin.Char"),
            "float" => TypeRef::named("kotlin.Float"),
            "double" => TypeRef::named("kotlin.Double"),
            "void" => TypeRef::named("kotlin.Unit"),
            _ => TypeRef::Named { name, fq: None, args, nullable: false },
        };
        while self.is_punct('[') && self.peek_n(1) == Some(&Tok::Punct(']')) {
            self.i += 2;
            t = TypeRef::with_args("kotlin.Array", vec![TypeArg::Type { variance: Variance::Out, ty: t }]);
        }
        t
    }

    fn decl_type(&mut self, container: &str, outer_kind: Option<SymbolKind>) -> Option<SourceDecl> {
        let doc = self.docs.get(self.i).cloned().flatten();
        let start = self.range().start();
        let (vis, mut mods) = self.modifiers();
        let kind = match self.peek()? {
            Tok::Ident(s) if s == "class" => SymbolKind::Class,
            Tok::Ident(s) if s == "interface" => SymbolKind::Interface,
            Tok::Ident(s) if s == "enum" => SymbolKind::Enum,
            Tok::Ident(s) if s == "record" && matches!(self.peek_n(1), Some(Tok::Ident(_))) => SymbolKind::Class,
            Tok::Punct('@') if matches!(self.peek_n(1), Some(Tok::Ident(s)) if s == "interface") => {
                self.i += 1;
                SymbolKind::Annotation
            }
            _ => return None,
        };
        let is_record = self.is_ident("record");
        self.i += 1;
        let name_range = self.range();
        let name = self.ident()?;
        let fq = if container.is_empty() { name.clone() } else { format!("{container}.{name}") };
        let mut sym = Symbol::new(kind, &name, fq.clone(), Origin::Source { file: self.path.to_path_buf(), range: TextRange::new(start, name_range.end()), name_range });
        sym.doc = doc;
        sym.visibility = match vis {
            1 => Visibility::Public,
            2 => Visibility::Protected,
            3 => Visibility::Private,
            _ if outer_kind == Some(SymbolKind::Interface) => Visibility::Public,
            _ => Visibility::PackagePrivate,
        };
        if kind == SymbolKind::Class && !mods.has(Modifiers::FINAL) && !is_record {
            if mods.has(Modifiers::ABSTRACT) {
                mods.0 &= !Modifiers::ABSTRACT;
                sym.modifiers.set(Modifiers::ABSTRACT);
            } else {
                sym.modifiers.set(Modifiers::OPEN);
            }
        }
        if outer_kind.is_some() && !mods.has(Modifiers::STATIC) && kind == SymbolKind::Class && outer_kind != Some(SymbolKind::Interface) {
            sym.modifiers.set(Modifiers::INNER);
        }
        sym.type_params = self.type_params();
        let mut children = Vec::new();
        if is_record && self.is_punct('(') {
            self.i += 1;
            let mut params = Vec::new();
            while !self.is_punct(')') && self.peek().is_some() {
                self.annotations();
                let ty = self.ty();
                let r = self.range();
                let Some(pname) = self.ident() else { break };
                let mut acc = Symbol::new(SymbolKind::Function, &pname, format!("{fq}.{pname}"), Origin::Source { file: self.path.to_path_buf(), range: r, name_range: r });
                acc.ty = Some(ty.clone());
                children.push(SourceDecl { symbol: acc, children: Vec::new() });
                params.push(Param { name: pname, ty, has_default: false, vararg: false });
                if !self.eat(',') {
                    break;
                }
            }
            self.eat(')');
            let mut ctor = Symbol::new(SymbolKind::Constructor, "<init>", format!("{fq}.<init>"), sym.origin.clone());
            ctor.params = params;
            ctor.ty = Some(TypeRef::Named { name: fq.clone(), fq: Some(fq.as_str().into()), args: vec![], nullable: false });
            children.push(SourceDecl { symbol: ctor, children: Vec::new() });
        }
        loop {
            if self.is_ident("extends") || self.is_ident("implements") || self.is_ident("permits") {
                let permits = self.is_ident("permits");
                self.i += 1;
                loop {
                    let t = self.ty();
                    if !permits {
                        sym.supertypes.push(t);
                    }
                    if !self.eat(',') {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        if self.is_punct('{') {
            self.i += 1;
            if kind == SymbolKind::Enum {
                // constants
                loop {
                    self.annotations();
                    let r = self.range();
                    let Some(Tok::Ident(n)) = self.peek().cloned() else { break };
                    if !matches!(self.peek_n(1), Some(Tok::Punct(',' | ';' | '(' | '{' | '}'))) {
                        break;
                    }
                    self.i += 1;
                    if self.is_punct('(') {
                        self.skip_group('(', ')');
                    }
                    if self.is_punct('{') {
                        self.skip_group('{', '}');
                    }
                    let mut e = Symbol::new(SymbolKind::EnumEntry, &n, format!("{fq}.{n}"), Origin::Source { file: self.path.to_path_buf(), range: r, name_range: r });
                    e.ty = Some(TypeRef::Named { name: fq.clone(), fq: Some(fq.as_str().into()), args: vec![], nullable: false });
                    children.push(SourceDecl { symbol: e, children: Vec::new() });
                    if !self.eat(',') {
                        break;
                    }
                }
                self.eat(';');
            }
            self.members(&fq, kind, &name, &mut children);
            self.eat('}');
        }
        if let Origin::Source { range, .. } = &mut sym.origin {
            let end = self.toks.get(self.i.saturating_sub(1)).map(|(_, r)| r.end()).unwrap_or(range.end());
            *range = TextRange::new(range.start(), end.max(range.start()));
        }
        Some(SourceDecl { symbol: sym, children })
    }

    fn members(&mut self, fq: &str, kind: SymbolKind, class_name: &str, out: &mut Vec<SourceDecl>) {
        while !self.is_punct('}') && self.peek().is_some() {
            let before = self.i;
            if self.eat(';') {
                continue;
            }
            // initializer blocks
            if self.is_punct('{') || (self.is_ident("static") && self.peek_n(1) == Some(&Tok::Punct('{'))) {
                if self.is_ident("static") {
                    self.i += 1;
                }
                self.skip_group('{', '}');
                continue;
            }
            let save = self.i;
            if let Some(d) = self.decl_type(fq, Some(kind)) {
                out.push(d);
                continue;
            }
            self.i = save;
            let doc = self.docs.get(self.i).cloned().flatten();
            let (vis, mods) = self.modifiers();
            let tps = self.type_params();
            let visibility = match vis {
                1 => Visibility::Public,
                2 => Visibility::Protected,
                3 => Visibility::Private,
                _ if kind == SymbolKind::Interface || kind == SymbolKind::Annotation => Visibility::Public,
                _ => Visibility::PackagePrivate,
            };
            // constructor: Name (
            if self.is_ident(class_name) && self.peek_n(1) == Some(&Tok::Punct('(')) {
                let r = self.range();
                self.i += 1;
                let params = self.params();
                self.skip_method_rest();
                let mut s = Symbol::new(SymbolKind::Constructor, "<init>", format!("{fq}.<init>"), Origin::Source { file: self.path.to_path_buf(), range: r, name_range: r });
                s.visibility = visibility;
                s.params = params;
                s.ty = Some(TypeRef::Named { name: fq.to_string(), fq: Some(fq.into()), args: vec![], nullable: false });
                s.doc = doc;
                out.push(SourceDecl { symbol: s, children: Vec::new() });
                continue;
            }
            let ty = self.ty();
            let r = self.range();
            let Some(name) = self.ident() else {
                // unknown construct: skip to the next ';' or block
                self.i = before.max(self.i);
                while let Some(t) = self.peek() {
                    match t {
                        Tok::Punct(';') => {
                            self.i += 1;
                            break;
                        }
                        Tok::Punct('{') => {
                            self.skip_group('{', '}');
                            break;
                        }
                        Tok::Punct('}') => break,
                        _ => self.i += 1,
                    }
                }
                if self.i == before {
                    self.i += 1;
                }
                continue;
            };
            let origin = Origin::Source { file: self.path.to_path_buf(), range: r, name_range: r };
            if self.is_punct('(') {
                let params = self.params();
                let has_body = self.skip_method_rest();
                let mut s = Symbol::new(SymbolKind::Function, &name, format!("{fq}.{name}"), origin);
                s.visibility = visibility;
                s.modifiers = mods;
                if kind == SymbolKind::Interface && !has_body && !mods.has(Modifiers::STATIC) {
                    s.modifiers.set(Modifiers::ABSTRACT);
                }
                s.type_params = tps;
                s.params = params;
                s.ty = Some(ty);
                s.doc = doc;
                out.push(SourceDecl { symbol: s, children: Vec::new() });
            } else {
                // field(s): T a = ..., b;
                let mut names = vec![(name, origin)];
                loop {
                    while self.is_punct('[') {
                        self.skip_group('[', ']');
                    }
                    if self.eat('=') {
                        let mut depth = 0;
                        while let Some(t) = self.peek() {
                            match t {
                                Tok::Punct('(' | '{' | '[') => depth += 1,
                                Tok::Punct(')' | '}' | ']') => {
                                    if depth == 0 {
                                        break;
                                    }
                                    depth -= 1;
                                }
                                Tok::Punct(',' | ';') if depth == 0 => break,
                                _ => {}
                            }
                            self.i += 1;
                        }
                    }
                    if self.eat(',') {
                        let r = self.range();
                        if let Some(n) = self.ident() {
                            names.push((n, Origin::Source { file: self.path.to_path_buf(), range: r, name_range: r }));
                            continue;
                        }
                    }
                    break;
                }
                self.eat(';');
                for (n, origin) in names {
                    let mut s = Symbol::new(SymbolKind::Field, &n, format!("{fq}.{n}"), origin);
                    s.visibility = visibility;
                    s.modifiers = mods;
                    if kind == SymbolKind::Interface {
                        s.modifiers.set(Modifiers::STATIC);
                    } else if !mods.has(Modifiers::FINAL) {
                        s.modifiers.set(Modifiers::VAR);
                    }
                    s.ty = Some(ty.clone());
                    s.doc = doc.clone();
                    out.push(SourceDecl { symbol: s, children: Vec::new() });
                }
            }
            if self.i == before {
                self.i += 1;
            }
        }
    }

    fn params(&mut self) -> Vec<Param> {
        let mut out = Vec::new();
        self.eat('(');
        while !self.is_punct(')') && self.peek().is_some() {
            let before = self.i;
            self.modifiers();
            let mut ty = self.ty();
            let vararg = self.peek() == Some(&Tok::Ellipsis);
            if vararg {
                self.i += 1;
            }
            let name = self.ident().unwrap_or_else(|| format!("p{}", out.len()));
            while self.is_punct('[') {
                self.skip_group('[', ']');
                ty = TypeRef::with_args("kotlin.Array", vec![TypeArg::Type { variance: Variance::Out, ty }]);
            }
            out.push(Param { name, ty, has_default: false, vararg });
            if !self.eat(',') {
                break;
            }
            if self.i == before {
                self.i += 1;
            }
        }
        self.eat(')');
        out
    }

    /// After the parameter list: `throws ...`, then a body or `;` / `default ...;`. Returns whether a body exists.
    fn skip_method_rest(&mut self) -> bool {
        while let Some(t) = self.peek() {
            match t {
                Tok::Punct('{') => {
                    self.skip_group('{', '}');
                    return true;
                }
                Tok::Punct(';') => {
                    self.i += 1;
                    return false;
                }
                Tok::Punct('}') => return false,
                _ => self.i += 1,
            }
        }
        false
    }
}

pub fn summarize(path: &Path, text: &str) -> FileSummary {
    let lexed = lex(text);
    let mut p = P { toks: &lexed.toks, docs: &lexed.docs, i: 0, path };
    let mut package = String::new();
    let mut imports = Vec::new();
    let mut decls = Vec::new();
    while p.peek().is_some() {
        let before = p.i;
        if p.is_ident("package") {
            p.i += 1;
            package = p.qualified().into_iter().map(|(s, _)| s).collect::<Vec<_>>().join(".");
            p.eat(';');
            continue;
        }
        if p.is_ident("import") {
            let start = p.range().start();
            p.i += 1;
            let is_static = p.is_ident("static");
            if is_static {
                p.i += 1;
            }
            let segments = p.qualified();
            let star = p.is_punct('.') && p.peek_n(1) == Some(&Tok::Punct('*'));
            if star {
                p.i += 2;
            }
            let end = p.range().end();
            p.eat(';');
            if !segments.is_empty() {
                imports.push(Import { segments, star, alias: None, range: TextRange::new(start, end.max(start)) });
            }
            continue;
        }
        if let Some(d) = p.decl_type(&package, None) {
            decls.push(d);
            continue;
        }
        if p.i == before {
            p.i += 1;
        }
    }
    FileSummary { path: path.to_path_buf(), language: Language::Java, package, imports, decls, jvm_name: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_java_sources() {
        let src = r#"
package com.example.java;

import java.util.List;
import java.util.*;
import static java.lang.Math.max;

/** Javadoc */
@Deprecated
public final class Util<T extends Comparable<T>> extends Base implements Runnable, java.io.Serializable {
    public static final int MAX = 10, MIN = 0;
    private String name = "x(y)";
    protected List<? extends T>[] items;

    public Util(String name) { this.name = name; }
    Util() { this("default"); }

    public static <R> R convert(T input, String... rest) throws Exception { return null; }
    @Override public void run() { if (true) { } }
    int[] numbers() { return new int[0]; }

    public static class Nested { }
    interface Callback { void call(int x); default void noop() {} }
    enum Mode { FAST("f") { }, SLOW; Mode() {} Mode(String s) {} }
    record Point(int x, int y) { }
    public @interface Marker { String value() default ""; }
}
"#;
        let s = summarize(Path::new("/x/Util.java"), src);
        assert_eq!(s.package, "com.example.java");
        assert_eq!(s.imports.len(), 3);
        assert!(s.imports[1].star);
        assert_eq!(s.imports[2].path(), "java.lang.Math.max");
        let util = &s.decls[0];
        assert_eq!(util.symbol.fq_name, "com.example.java.Util");
        assert_eq!(util.symbol.doc.as_deref(), Some("/** Javadoc */"));
        assert_eq!(util.symbol.supertypes.len(), 3);
        assert_eq!(util.symbol.type_params[0].name, "T");
        let names: Vec<String> = util.children.iter().map(|c| format!("{:?}:{}", c.symbol.kind, c.symbol.name)).collect();
        assert_eq!(
            names,
            vec![
                "Field:MAX", "Field:MIN", "Field:name", "Field:items", "Constructor:<init>", "Constructor:<init>", "Function:convert", "Function:run",
                "Function:numbers", "Class:Nested", "Interface:Callback", "Enum:Mode", "Class:Point", "Annotation:Marker"
            ]
        );
        let convert = &util.children[6].symbol;
        assert!(convert.params[1].vararg);
        assert!(convert.modifiers.has(Modifiers::STATIC));
        let callback = &util.children[10];
        assert!(callback.children[0].symbol.modifiers.has(Modifiers::ABSTRACT));
        assert!(!callback.children[1].symbol.modifiers.has(Modifiers::ABSTRACT));
        let mode = &util.children[11];
        let entries: Vec<_> = mode.children.iter().filter(|c| c.symbol.kind == SymbolKind::EnumEntry).map(|c| c.symbol.name.as_str()).collect();
        assert_eq!(entries, vec!["FAST", "SLOW"]);
        let point = &util.children[12];
        assert_eq!(point.children.iter().filter(|c| c.symbol.kind == SymbolKind::Function).count(), 2);
    }

    #[test]
    fn tolerates_garbage() {
        for src in ["class", "class {", "public class A { void f( }", "@", "import", "enum E { A, B", "class A { int x = ; }"] {
            summarize(Path::new("/x/A.java"), src);
        }
    }
}
