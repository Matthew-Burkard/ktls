//! Declaration model shared by source files and libraries.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use rowan::TextRange;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    Class,
    Interface,
    Object,
    Enum,
    EnumEntry,
    Annotation,
    TypeAlias,
    Function,
    Constructor,
    Property,
    /// Java field.
    Field,
    TypeParameter,
    Package,
}

impl SymbolKind {
    pub fn is_classifier(self) -> bool {
        matches!(self, SymbolKind::Class | SymbolKind::Interface | SymbolKind::Object | SymbolKind::Enum | SymbolKind::Annotation | SymbolKind::TypeAlias)
    }

    pub fn is_callable(self) -> bool {
        matches!(self, SymbolKind::Function | SymbolKind::Constructor | SymbolKind::Property | SymbolKind::Field | SymbolKind::EnumEntry)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Visibility {
    #[default]
    Public,
    Internal,
    Protected,
    Private,
    /// Java package-private.
    PackagePrivate,
}

/// Modifier flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Modifiers(pub u32);

impl Modifiers {
    pub const ABSTRACT: u32 = 1 << 0;
    pub const OPEN: u32 = 1 << 1;
    pub const FINAL: u32 = 1 << 2;
    pub const SEALED: u32 = 1 << 3;
    pub const DATA: u32 = 1 << 4;
    pub const VALUE: u32 = 1 << 5;
    pub const INNER: u32 = 1 << 6;
    pub const COMPANION: u32 = 1 << 7;
    pub const INLINE: u32 = 1 << 8;
    pub const SUSPEND: u32 = 1 << 9;
    pub const OPERATOR: u32 = 1 << 10;
    pub const INFIX: u32 = 1 << 11;
    pub const TAILREC: u32 = 1 << 12;
    pub const EXTERNAL: u32 = 1 << 13;
    pub const CONST: u32 = 1 << 14;
    pub const LATEINIT: u32 = 1 << 15;
    pub const OVERRIDE: u32 = 1 << 16;
    pub const EXPECT: u32 = 1 << 17;
    pub const ACTUAL: u32 = 1 << 18;
    pub const VAR: u32 = 1 << 19;
    pub const STATIC: u32 = 1 << 20;
    pub const FUN_INTERFACE: u32 = 1 << 21;
    pub const DEPRECATED: u32 = 1 << 22;
    pub const VARARG: u32 = 1 << 23;
    pub const SYNTHETIC: u32 = 1 << 24;
    pub const HAS_DEFAULT: u32 = 1 << 25;
    /// `@Deprecated(level = ERROR/HIDDEN)` (library declarations).
    pub const DEPRECATED_ERROR: u32 = 1 << 26;
    /// An annotation class annotated `@RequiresOptIn` (library declarations).
    pub const OPT_IN_MARKER: u32 = 1 << 27;
    /// ...with `level = WARNING`.
    pub const OPT_IN_WARNING: u32 = 1 << 28;
    /// `@Deprecated(level = HIDDEN)`: invisible to source code (library declarations).
    pub const HIDDEN: u32 = 1 << 29;
    /// `@kotlin.internal.LowPriorityInOverloadResolution`: loses to other applicable candidates.
    pub const LOW_PRIORITY: u32 = 1 << 30;

    pub fn has(self, flag: u32) -> bool {
        self.0 & flag != 0
    }

    pub fn set(&mut self, flag: u32) {
        self.0 |= flag;
    }

    pub fn from_keyword(kw: &str) -> u32 {
        match kw {
            "abstract" => Self::ABSTRACT,
            "open" => Self::OPEN,
            "final" => Self::FINAL,
            "sealed" => Self::SEALED,
            "data" => Self::DATA,
            "value" => Self::VALUE,
            "inner" => Self::INNER,
            "companion" => Self::COMPANION,
            "inline" => Self::INLINE,
            "suspend" => Self::SUSPEND,
            "operator" => Self::OPERATOR,
            "infix" => Self::INFIX,
            "tailrec" => Self::TAILREC,
            "external" => Self::EXTERNAL,
            "const" => Self::CONST,
            "lateinit" => Self::LATEINIT,
            "override" => Self::OVERRIDE,
            "expect" => Self::EXPECT,
            "actual" => Self::ACTUAL,
            "vararg" => Self::VARARG,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variance {
    Invariant,
    In,
    Out,
}

/// A reference to a type, possibly unresolved (source) or fully qualified (libraries).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeRef {
    /// A class / typealias type. `name` is as written in source (`Map.Entry`), or the
    /// fully qualified name for library types; `fq` is filled in once resolved.
    Named { name: String, fq: Option<Arc<str>>, args: Vec<TypeArg>, nullable: bool },
    Function { receiver: Option<Box<TypeRef>>, params: Vec<TypeRef>, ret: Box<TypeRef>, suspend: bool, nullable: bool },
    TypeParam { name: String, nullable: bool },
    /// Java type whose nullability is unknown (`String!`).
    Platform(Box<TypeRef>),
    Dynamic,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeArg {
    Star,
    Type { variance: Variance, ty: TypeRef },
}

impl TypeRef {
    pub fn named(fq: &str) -> TypeRef {
        TypeRef::Named { name: fq.to_string(), fq: Some(fq.into()), args: Vec::new(), nullable: false }
    }

    pub fn with_args(fq: &str, args: Vec<TypeArg>) -> TypeRef {
        TypeRef::Named { name: fq.to_string(), fq: Some(fq.into()), args, nullable: false }
    }

    pub fn nullable(mut self, n: bool) -> TypeRef {
        match &mut self {
            TypeRef::Named { nullable, .. } | TypeRef::Function { nullable, .. } | TypeRef::TypeParam { nullable, .. } => *nullable |= n,
            TypeRef::Platform(inner) => return inner.as_ref().clone().nullable(n),
            _ => {}
        }
        self
    }

    pub fn fq_name(&self) -> Option<&str> {
        match self {
            TypeRef::Named { fq, .. } => fq.as_deref(),
            TypeRef::Platform(t) => t.fq_name(),
            _ => None,
        }
    }

    /// Kotlin-style rendering with simple names (`List<String>?`).
    pub fn display(&self) -> String {
        let mut s = String::new();
        self.render(&mut s, false);
        s
    }

    /// Rendering with fully qualified names where known.
    pub fn display_qualified(&self) -> String {
        let mut s = String::new();
        self.render(&mut s, true);
        s
    }

    fn render(&self, out: &mut String, qualified: bool) {
        match self {
            TypeRef::Named { name, fq, args, nullable } => {
                let shown = match (qualified, fq) {
                    (true, Some(fq)) => fq.to_string(),
                    (false, Some(fq)) => short_class_name(fq),
                    _ => name.clone(),
                };
                // kotlin.FunctionN → (A, B) -> R
                if let Some(fq) = fq
                    && let Some(n) = fq.strip_prefix("kotlin.Function").or_else(|| fq.strip_prefix("kotlin.coroutines.SuspendFunction"))
                        && n.parse::<u32>().is_ok() && !args.is_empty() {
                            if *nullable {
                                out.push('(');
                            }
                            if fq.contains("Suspend") {
                                out.push_str("suspend ");
                            }
                            out.push('(');
                            for (i, a) in args[..args.len() - 1].iter().enumerate() {
                                if i > 0 {
                                    out.push_str(", ");
                                }
                                a.render(out, qualified);
                            }
                            out.push_str(") -> ");
                            args.last().unwrap().render(out, qualified);
                            if *nullable {
                                out.push_str(")?");
                            }
                            return;
                        }
                out.push_str(&shown);
                if !args.is_empty() {
                    out.push('<');
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        a.render(out, qualified);
                    }
                    out.push('>');
                }
                if *nullable {
                    out.push('?');
                }
            }
            TypeRef::Function { receiver, params, ret, suspend, nullable } => {
                if *nullable {
                    out.push('(');
                }
                if *suspend {
                    out.push_str("suspend ");
                }
                if let Some(r) = receiver {
                    r.render(out, qualified);
                    out.push('.');
                }
                out.push('(');
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    p.render(out, qualified);
                }
                out.push_str(") -> ");
                ret.render(out, qualified);
                if *nullable {
                    out.push_str(")?");
                }
            }
            TypeRef::TypeParam { name, nullable } => {
                out.push_str(name);
                if *nullable {
                    out.push('?');
                }
            }
            TypeRef::Platform(inner) => {
                inner.render(out, qualified);
                out.push('!');
            }
            TypeRef::Dynamic => out.push_str("dynamic"),
            TypeRef::Error => out.push_str("<error>"),
        }
    }
}

impl TypeArg {
    fn render(&self, out: &mut String, qualified: bool) {
        match self {
            TypeArg::Star => out.push('*'),
            TypeArg::Type { variance, ty } => {
                match variance {
                    Variance::In => out.push_str("in "),
                    Variance::Out => out.push_str("out "),
                    Variance::Invariant => {}
                }
                ty.render(out, qualified);
            }
        }
    }
}

impl fmt::Display for TypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// `kotlin.collections.Map.Entry` → `Map.Entry` (keeps nested-class qualification).
pub fn short_class_name(fq: &str) -> String {
    let parts: Vec<&str> = fq.split('.').collect();
    let first_upper = parts.iter().position(|p| p.chars().next().is_some_and(|c| c.is_uppercase())).unwrap_or(parts.len().saturating_sub(1));
    parts[first_upper..].join(".")
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeParam {
    pub name: String,
    pub bounds: Vec<TypeRef>,
    pub variance: Variance,
    pub reified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Param {
    pub name: String,
    pub ty: TypeRef,
    pub has_default: bool,
    pub vararg: bool,
}

/// Where a declaration comes from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    Source { file: PathBuf, range: TextRange, name_range: TextRange },
    Library { archive: PathBuf, class: String },
    /// Kotlin built-ins (`kotlin.Int`, `kotlin.Any`...), defined in `.kotlin_builtins`.
    Builtin,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Symbol {
    pub kind: SymbolKind,
    pub name: String,
    /// Fully qualified name: `pkg.Outer.Inner` for classifiers, `pkg.Outer.member` for members.
    pub fq_name: String,
    pub visibility: Visibility,
    pub modifiers: Modifiers,
    pub type_params: Vec<TypeParam>,
    /// Extension receiver.
    pub receiver: Option<TypeRef>,
    pub params: Vec<Param>,
    /// Return type (functions), property/field type, aliased type (typealias).
    pub ty: Option<TypeRef>,
    pub supertypes: Vec<TypeRef>,
    pub origin: Origin,
    pub doc: Option<String>,
    /// FQ names of the annotations of a library declaration (noise such as nullability
    /// and `kotlin.internal` left out). `kotlin.DeprecatedSinceKotlin` is kept with its
    /// versions: `kotlin.DeprecatedSinceKotlin(warningSince=1.5,errorSince=2.1)`.
    pub annotations: Vec<String>,
}

impl Symbol {
    pub fn new(kind: SymbolKind, name: &str, fq_name: String, origin: Origin) -> Symbol {
        Symbol {
            kind,
            name: name.to_string(),
            fq_name,
            visibility: Visibility::Public,
            modifiers: Modifiers::default(),
            type_params: Vec::new(),
            receiver: None,
            params: Vec::new(),
            ty: None,
            supertypes: Vec::new(),
            origin,
            doc: None,
            annotations: Vec::new(),
        }
    }

    pub fn package(&self) -> &str {
        match self.fq_name.rfind('.') {
            Some(i) if self.kind.is_classifier() || self.kind == SymbolKind::Package => {
                // the package is everything before the first capitalised segment
                let segs: Vec<&str> = self.fq_name.split('.').collect();
                let idx = segs.iter().position(|s| s.chars().next().is_some_and(|c| c.is_uppercase()));
                match idx {
                    Some(0) | None => &self.fq_name[..i],
                    Some(n) => {
                        let len: usize = segs[..n].iter().map(|s| s.len() + 1).sum::<usize>() - 1;
                        &self.fq_name[..len]
                    }
                }
            }
            Some(i) => &self.fq_name[..i],
            None => "",
        }
    }

    /// A one line, Kotlin-like signature (`fun <T> List<T>.second(): T`).
    pub fn signature(&self) -> String {
        let mut s = String::new();
        match self.visibility {
            Visibility::Public => {}
            Visibility::Internal => s.push_str("internal "),
            Visibility::Protected => s.push_str("protected "),
            Visibility::Private => s.push_str("private "),
            Visibility::PackagePrivate => {}
        }
        let m = self.modifiers;
        let implicit_abstract = self.kind == SymbolKind::Interface;
        for (flag, kw) in [
            (Modifiers::EXPECT, "expect"),
            (Modifiers::ACTUAL, "actual"),
            (Modifiers::OVERRIDE, "override"),
            (if implicit_abstract { 0 } else { Modifiers::ABSTRACT }, "abstract"),
            (Modifiers::SEALED, "sealed"),
            (Modifiers::DATA, "data"),
            (Modifiers::VALUE, "value"),
            (Modifiers::INNER, "inner"),
            (Modifiers::COMPANION, "companion"),
            (Modifiers::INLINE, "inline"),
            (Modifiers::SUSPEND, "suspend"),
            (Modifiers::OPERATOR, "operator"),
            (Modifiers::INFIX, "infix"),
            (Modifiers::CONST, "const"),
            (Modifiers::LATEINIT, "lateinit"),
        ] {
            if m.has(flag) {
                s.push_str(kw);
                s.push(' ');
            }
        }
        let tps = |s: &mut String| {
            if !self.type_params.is_empty() {
                s.push('<');
                for (i, tp) in self.type_params.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    if tp.reified {
                        s.push_str("reified ");
                    }
                    match tp.variance {
                        Variance::In => s.push_str("in "),
                        Variance::Out => s.push_str("out "),
                        Variance::Invariant => {}
                    }
                    s.push_str(&tp.name);
                    if let Some(b) = tp.bounds.first() {
                        s.push_str(" : ");
                        s.push_str(&b.display());
                    }
                }
                s.push('>');
            }
        };
        match self.kind {
            SymbolKind::Class | SymbolKind::Interface | SymbolKind::Object | SymbolKind::Enum | SymbolKind::Annotation => {
                s.push_str(match self.kind {
                    SymbolKind::Interface if m.has(Modifiers::FUN_INTERFACE) => "fun interface ",
                    SymbolKind::Interface => "interface ",
                    SymbolKind::Object => "object ",
                    SymbolKind::Enum => "enum class ",
                    SymbolKind::Annotation => "annotation class ",
                    _ => "class ",
                });
                s.push_str(&self.name);
                tps(&mut s);
                if !self.supertypes.is_empty() {
                    s.push_str(" : ");
                    s.push_str(&self.supertypes.iter().map(|t| t.display()).collect::<Vec<_>>().join(", "));
                }
            }
            SymbolKind::TypeAlias => {
                s.push_str("typealias ");
                s.push_str(&self.name);
                tps(&mut s);
                if let Some(t) = &self.ty {
                    // Qualified: aliases usually point to a same-named class elsewhere.
                    s.push_str(" = ");
                    s.push_str(&t.display_qualified());
                }
            }
            SymbolKind::Function | SymbolKind::Constructor => {
                if self.kind == SymbolKind::Constructor {
                    s.push_str("constructor");
                } else {
                    s.push_str("fun ");
                    if !self.type_params.is_empty() {
                        tps(&mut s);
                        s.push(' ');
                    }
                    if let Some(r) = &self.receiver {
                        s.push_str(&r.display());
                        s.push('.');
                    }
                    s.push_str(&self.name);
                }
                s.push('(');
                for (i, p) in self.params.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    if p.vararg {
                        s.push_str("vararg ");
                    }
                    s.push_str(&p.name);
                    s.push_str(": ");
                    s.push_str(&p.ty.display());
                    if p.has_default {
                        s.push_str(" = …");
                    }
                }
                s.push(')');
                if self.kind == SymbolKind::Function
                    && let Some(t) = &self.ty {
                        s.push_str(": ");
                        s.push_str(&t.display());
                    }
            }
            SymbolKind::Property | SymbolKind::Field => {
                s.push_str(if m.has(Modifiers::VAR) { "var " } else { "val " });
                if !self.type_params.is_empty() {
                    tps(&mut s);
                    s.push(' ');
                }
                if let Some(r) = &self.receiver {
                    s.push_str(&r.display());
                    s.push('.');
                }
                s.push_str(&self.name);
                if let Some(t) = &self.ty {
                    s.push_str(": ");
                    s.push_str(&t.display());
                }
            }
            SymbolKind::EnumEntry => s.push_str(&self.name),
            SymbolKind::TypeParameter => s.push_str(&self.name),
            SymbolKind::Package => {
                s.push_str("package ");
                s.push_str(&self.fq_name);
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let mut f = Symbol::new(SymbolKind::Function, "second", "pkg.second".into(), Origin::Builtin);
        f.type_params.push(TypeParam { name: "T".into(), bounds: vec![], variance: Variance::Invariant, reified: false });
        f.receiver = Some(TypeRef::with_args("kotlin.collections.List", vec![TypeArg::Type { variance: Variance::Invariant, ty: TypeRef::TypeParam { name: "T".into(), nullable: false } }]));
        f.ty = Some(TypeRef::TypeParam { name: "T".into(), nullable: true });
        assert_eq!(f.signature(), "fun <T> List<T>.second(): T?");
        let fun_ty = TypeRef::with_args(
            "kotlin.Function1",
            vec![
                TypeArg::Type { variance: Variance::In, ty: TypeRef::named("kotlin.Int") },
                TypeArg::Type { variance: Variance::Out, ty: TypeRef::named("kotlin.Unit") },
            ],
        );
        assert_eq!(fun_ty.display(), "(in Int) -> out Unit");
        assert_eq!(short_class_name("java.util.Map.Entry"), "Map.Entry");
        let c = Symbol::new(SymbolKind::Class, "Entry", "java.util.Map.Entry".into(), Origin::Builtin);
        assert_eq!(c.package(), "java.util");
    }
}
