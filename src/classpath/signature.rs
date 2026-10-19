//! JVM descriptors and generic signatures, mapped to Kotlin types.

use crate::symbols::{TypeArg, TypeParam, TypeRef, Variance};

/// Java class (internal name) → Kotlin class FQ name, for mapped types.
pub fn map_java_class(internal: &str) -> Option<&'static str> {
    Some(match internal {
        "java/lang/Object" => "kotlin.Any",
        "java/lang/String" => "kotlin.String",
        "java/lang/CharSequence" => "kotlin.CharSequence",
        "java/lang/Throwable" => "kotlin.Throwable",
        "java/lang/Cloneable" => "kotlin.Cloneable",
        "java/lang/Number" => "kotlin.Number",
        "java/lang/Comparable" => "kotlin.Comparable",
        "java/lang/Enum" => "kotlin.Enum",
        "java/lang/annotation/Annotation" => "kotlin.Annotation",
        "java/lang/Integer" => "kotlin.Int",
        "java/lang/Long" => "kotlin.Long",
        "java/lang/Short" => "kotlin.Short",
        "java/lang/Byte" => "kotlin.Byte",
        "java/lang/Character" => "kotlin.Char",
        "java/lang/Boolean" => "kotlin.Boolean",
        "java/lang/Float" => "kotlin.Float",
        "java/lang/Double" => "kotlin.Double",
        "java/lang/Void" => "kotlin.Nothing",
        "java/lang/Iterable" => "kotlin.collections.MutableIterable",
        "java/util/Iterator" => "kotlin.collections.MutableIterator",
        "java/util/ListIterator" => "kotlin.collections.MutableListIterator",
        "java/util/Collection" => "kotlin.collections.MutableCollection",
        "java/util/List" => "kotlin.collections.MutableList",
        "java/util/Set" => "kotlin.collections.MutableSet",
        "java/util/Map" => "kotlin.collections.MutableMap",
        "java/util/Map$Entry" => "kotlin.collections.MutableMap.MutableEntry",
        _ => return None,
    })
}

fn primitive(c: u8) -> Option<&'static str> {
    Some(match c {
        b'I' => "kotlin.Int",
        b'J' => "kotlin.Long",
        b'Z' => "kotlin.Boolean",
        b'B' => "kotlin.Byte",
        b'S' => "kotlin.Short",
        b'C' => "kotlin.Char",
        b'F' => "kotlin.Float",
        b'D' => "kotlin.Double",
        b'V' => "kotlin.Unit",
        _ => return None,
    })
}

fn primitive_array(c: u8) -> Option<&'static str> {
    Some(match c {
        b'I' => "kotlin.IntArray",
        b'J' => "kotlin.LongArray",
        b'Z' => "kotlin.BooleanArray",
        b'B' => "kotlin.ByteArray",
        b'S' => "kotlin.ShortArray",
        b'C' => "kotlin.CharArray",
        b'F' => "kotlin.FloatArray",
        b'D' => "kotlin.DoubleArray",
        _ => return None,
    })
}

/// Java internal class name → Kotlin-style FQ name (`java/util/Map$Entry` → `java.util.Map.Entry`).
///
/// `$` is only treated as a nesting separator when followed by an uppercase letter, which
/// matches virtually all real nested classes.
pub fn internal_to_fq(internal: &str) -> String {
    let mut out = String::with_capacity(internal.len());
    let bytes = internal.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'/' => out.push('.'),
            b'$' if bytes.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic() || *n == b'_') && i > 0 && bytes[i - 1] != b'/' => out.push('.'),
            _ => out.push(b as char),
        }
    }
    out
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn peek(&self) -> u8 {
        self.s.get(self.i).copied().unwrap_or(0)
    }
    fn bump(&mut self) -> u8 {
        let c = self.peek();
        self.i += 1;
        c
    }
    fn ident(&mut self, stop: &[u8]) -> String {
        let start = self.i;
        while self.i < self.s.len() && !stop.contains(&self.s[self.i]) {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }

    fn type_params(&mut self) -> Vec<TypeParam> {
        let mut out = Vec::new();
        if self.peek() != b'<' {
            return out;
        }
        self.bump();
        while self.peek() != b'>' && self.peek() != 0 {
            let name = self.ident(b":");
            let mut bounds = Vec::new();
            while self.peek() == b':' {
                self.bump();
                if matches!(self.peek(), b'L' | b'T' | b'[') {
                    let b = self.field_type();
                    // `T extends Object` is the default bound.
                    if b.fq_name() != Some("kotlin.Any") {
                        bounds.push(b);
                    }
                }
            }
            out.push(TypeParam { name, bounds, variance: Variance::Invariant, reified: false });
        }
        self.bump();
        out
    }

    fn field_type(&mut self) -> TypeRef {
        match self.bump() {
            b'L' => self.class_type(),
            b'T' => {
                let name = self.ident(b";");
                self.bump();
                TypeRef::Platform(Box::new(TypeRef::TypeParam { name, nullable: false }))
            }
            b'[' => {
                if let Some(arr) = primitive_array(self.peek()) {
                    self.bump();
                    return TypeRef::Platform(Box::new(TypeRef::named(arr)));
                }
                let elem = self.field_type();
                TypeRef::Platform(Box::new(TypeRef::with_args(
                    "kotlin.Array",
                    vec![TypeArg::Type { variance: Variance::Out, ty: elem }],
                )))
            }
            c => match primitive(c) {
                Some(p) => TypeRef::named(p),
                None => TypeRef::Error,
            },
        }
    }

    fn class_type(&mut self) -> TypeRef {
        // after 'L': pkg/Outer<args>.Inner<args>;
        let mut internal = String::new();
        let mut args = Vec::new();
        loop {
            internal.push_str(&self.ident(b"<.;"));
            if self.peek() == b'<' {
                self.bump();
                args.clear();
                while self.peek() != b'>' && self.peek() != 0 {
                    args.push(match self.peek() {
                        b'*' => {
                            self.bump();
                            TypeArg::Star
                        }
                        b'+' => {
                            self.bump();
                            TypeArg::Type { variance: Variance::Out, ty: self.field_type() }
                        }
                        b'-' => {
                            self.bump();
                            TypeArg::Type { variance: Variance::In, ty: self.field_type() }
                        }
                        _ => TypeArg::Type { variance: Variance::Invariant, ty: self.field_type() },
                    });
                }
                self.bump();
            }
            if self.peek() == b'.' {
                self.bump();
                internal.push('$');
                continue;
            }
            self.bump(); // ';'
            break;
        }
        let fq = map_java_class(&internal).map(str::to_string).unwrap_or_else(|| internal_to_fq(&internal));
        TypeRef::Platform(Box::new(TypeRef::Named { name: fq.clone(), fq: Some(fq.into()), args, nullable: false }))
    }
}

/// Parsed method signature.
pub struct MethodSig {
    pub type_params: Vec<TypeParam>,
    pub params: Vec<TypeRef>,
    pub ret: TypeRef,
}

/// Parse a method descriptor or generic signature.
pub fn method(sig: &str) -> MethodSig {
    let mut p = P { s: sig.as_bytes(), i: 0 };
    let type_params = p.type_params();
    let mut params = Vec::new();
    if p.peek() == b'(' {
        p.bump();
        while p.peek() != b')' && p.peek() != 0 {
            params.push(p.field_type());
        }
        p.bump();
    }
    let ret = p.field_type();
    MethodSig { type_params, params, ret }
}

pub fn field(sig: &str) -> TypeRef {
    P { s: sig.as_bytes(), i: 0 }.field_type()
}

/// Class signature: (type params, superclass, interfaces).
pub fn class(sig: &str) -> (Vec<TypeParam>, Vec<TypeRef>) {
    let mut p = P { s: sig.as_bytes(), i: 0 };
    let tps = p.type_params();
    let mut supers = Vec::new();
    while p.peek() == b'L' {
        supers.push(p.field_type());
    }
    (tps, supers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors() {
        let m = method("(ILjava/lang/String;[J[[Ljava/util/List;)V");
        let shown: Vec<String> = m.params.iter().map(|t| t.display()).collect();
        assert_eq!(shown, vec!["Int", "String!", "LongArray!", "Array<out Array<out MutableList!>!>!"]);
        assert_eq!(m.ret.display(), "Unit");
    }

    #[test]
    fn generic_signatures() {
        let m = method("<K:Ljava/lang/Object;V::Ljava/lang/Comparable<-TV;>;>(Ljava/util/Map<TK;+TV;>;)Ljava/util/Map$Entry<TK;TV;>;");
        assert_eq!(m.type_params.len(), 2);
        assert!(m.type_params[0].bounds.is_empty());
        assert_eq!(m.type_params[1].bounds[0].display(), "Comparable<in V!>!");
        assert_eq!(m.params[0].display(), "MutableMap<K!, out V!>!");
        assert_eq!(m.ret.display(), "MutableMap.MutableEntry<K!, V!>!");
        let (tps, supers) = class("<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;Ljava/util/List<TE;>;Ljava/util/RandomAccess;");
        assert_eq!(tps[0].name, "E");
        assert_eq!(supers.len(), 3);
        assert_eq!(supers[2].fq_name(), Some("java.util.RandomAccess"));
        assert_eq!(field("Lcom/x/Outer<Ljava/lang/String;>.Inner<Ljava/lang/Integer;>;").fq_name(), Some("com.x.Outer.Inner"));
    }

    #[test]
    fn internal_names() {
        assert_eq!(internal_to_fq("java/util/Map$Entry"), "java.util.Map.Entry");
        assert_eq!(internal_to_fq("com/x/Foo$1"), "com.x.Foo$1");
        assert_eq!(internal_to_fq("com/x/$Proxy"), "com.x.$Proxy");
    }
}
