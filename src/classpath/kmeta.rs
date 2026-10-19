//! Kotlin metadata (`@kotlin.Metadata` and `.kotlin_builtins`) decoding.
//!
//! Field numbers and flag layouts follow `metadata.proto` / `jvm_metadata.proto`
//! and `Flags.java` of the Kotlin compiler.

use std::path::Path;

use crate::symbols::{Modifiers, Origin, Param, Symbol, SymbolKind, TypeArg, TypeParam, TypeRef, Variance, Visibility};

// ------------------------------------------------------------- protobuf

#[derive(Clone, Copy, Debug)]
enum Val<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

#[derive(Clone, Debug, Default)]
pub struct Msg<'a> {
    fields: Vec<(u32, Val<'a>)>,
}

fn varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        let b = *data.get(*pos)?;
        *pos += 1;
        result |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

impl<'a> Msg<'a> {
    pub fn parse(data: &'a [u8]) -> Option<Msg<'a>> {
        let mut fields = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            let key = varint(data, &mut pos)?;
            let field = (key >> 3) as u32;
            match key & 7 {
                0 => fields.push((field, Val::Int(varint(data, &mut pos)?))),
                1 => {
                    let b = data.get(pos..pos + 8)?;
                    fields.push((field, Val::Int(u64::from_le_bytes(b.try_into().ok()?))));
                    pos += 8;
                }
                2 => {
                    let len = varint(data, &mut pos)? as usize;
                    fields.push((field, Val::Bytes(data.get(pos..pos + len)?)));
                    pos += len;
                }
                5 => {
                    let b = data.get(pos..pos + 4)?;
                    fields.push((field, Val::Int(u32::from_le_bytes(b.try_into().ok()?) as u64)));
                    pos += 4;
                }
                _ => return None,
            }
        }
        Some(Msg { fields })
    }

    pub fn int(&self, f: u32) -> Option<i64> {
        self.fields.iter().rev().find_map(|(n, v)| match (n, v) {
            (n, Val::Int(i)) if *n == f => Some(*i as i64),
            _ => None,
        })
    }

    pub fn has(&self, f: u32) -> bool {
        self.fields.iter().any(|(n, _)| *n == f)
    }

    /// Repeated int field (packed or not).
    pub fn ints(&self, f: u32) -> Vec<i64> {
        let mut out = Vec::new();
        for (n, v) in &self.fields {
            if *n != f {
                continue;
            }
            match v {
                Val::Int(i) => out.push(*i as i64),
                Val::Bytes(b) => {
                    let mut pos = 0;
                    while pos < b.len() {
                        match varint(b, &mut pos) {
                            Some(i) => out.push(i as i64),
                            None => break,
                        }
                    }
                }
            }
        }
        out
    }

    pub fn msg(&self, f: u32) -> Option<Msg<'a>> {
        self.fields.iter().rev().find_map(|(n, v)| match (n, v) {
            (n, Val::Bytes(b)) if *n == f => Msg::parse(b),
            _ => None,
        })
    }

    pub fn msgs(&self, f: u32) -> Vec<Msg<'a>> {
        self.fields
            .iter()
            .filter_map(|(n, v)| match v {
                Val::Bytes(b) if *n == f => Msg::parse(b),
                _ => None,
            })
            .collect()
    }

    pub fn string(&self, f: u32) -> Option<String> {
        self.fields.iter().rev().find_map(|(n, v)| match (n, v) {
            (n, Val::Bytes(b)) if *n == f => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
    }

    pub fn strings(&self, f: u32) -> Vec<String> {
        self.fields
            .iter()
            .filter_map(|(n, v)| match v {
                Val::Bytes(b) if *n == f => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect()
    }
}

// ------------------------------------------------------------- d1 bytes

/// `BitEncoding.decodeBytes`.
pub fn decode_bytes(data: &[Vec<u16>]) -> Vec<u8> {
    let mut data: Vec<&[u16]> = data.iter().map(|v| v.as_slice()).collect();
    if let Some(first) = data.first().copied()
        && let Some(&marker) = first.first() {
            if marker == 0 {
                // UTF-8 mode: every char holds one byte.
                data[0] = &first[1..];
                return data.iter().flat_map(|s| s.iter().map(|&c| c as u8)).collect();
            }
            if marker == 1 {
                data[0] = &first[1..];
            }
        }
    let mut bytes: Vec<u8> = data.iter().flat_map(|s| s.iter().map(|&c| c as u8)).collect();
    for b in &mut bytes {
        *b = (b.wrapping_add(0x7F)) & 0x7F;
    }
    // decode7to8
    let result_len = 7 * bytes.len() / 8;
    let mut out = vec![0u8; result_len];
    let mut byte_index = 0;
    let mut bit = 0;
    for o in out.iter_mut() {
        let first = (bytes[byte_index] as u32) >> bit;
        byte_index += 1;
        let second = ((bytes.get(byte_index).copied().unwrap_or(0) as u32) & ((1 << (bit + 1)) - 1)) << (7 - bit);
        *o = (first + second) as u8;
        if bit == 6 {
            byte_index += 1;
            bit = 0;
        } else {
            bit += 1;
        }
    }
    out
}

// --------------------------------------------------------- name resolvers

pub trait NameResolver {
    fn string(&self, idx: i64) -> String;
    /// Dotted FQ name of a class (`kotlin.collections.Map.Entry`).
    fn class_fq(&self, idx: i64) -> String;
}

const PREDEFINED: &[&str] = &[
    "kotlin/Any",
    "kotlin/Nothing",
    "kotlin/Unit",
    "kotlin/Throwable",
    "kotlin/Number",
    "kotlin/Byte",
    "kotlin/Double",
    "kotlin/Float",
    "kotlin/Int",
    "kotlin/Long",
    "kotlin/Short",
    "kotlin/Boolean",
    "kotlin/Char",
    "kotlin/CharSequence",
    "kotlin/String",
    "kotlin/Comparable",
    "kotlin/Enum",
    "kotlin/Array",
    "kotlin/ByteArray",
    "kotlin/DoubleArray",
    "kotlin/FloatArray",
    "kotlin/IntArray",
    "kotlin/LongArray",
    "kotlin/ShortArray",
    "kotlin/BooleanArray",
    "kotlin/CharArray",
    "kotlin/Cloneable",
    "kotlin/Annotation",
    "kotlin/collections/Iterable",
    "kotlin/collections/MutableIterable",
    "kotlin/collections/Collection",
    "kotlin/collections/MutableCollection",
    "kotlin/collections/List",
    "kotlin/collections/MutableList",
    "kotlin/collections/Set",
    "kotlin/collections/MutableSet",
    "kotlin/collections/Map",
    "kotlin/collections/MutableMap",
    "kotlin/collections/Map.Entry",
    "kotlin/collections/MutableMap.MutableEntry",
    "kotlin/collections/Iterator",
    "kotlin/collections/MutableIterator",
    "kotlin/collections/ListIterator",
    "kotlin/collections/MutableListIterator",
];

struct Record {
    predefined: Option<i64>,
    string: Option<String>,
    operation: i64,
    substring: Vec<i64>,
    replace_char: Vec<i64>,
}

/// `JvmNameResolver`: StringTableTypes records over the `d2` strings.
pub struct JvmNameResolver<'s> {
    records: Vec<std::rc::Rc<Record>>,
    strings: &'s [String],
}

impl<'s> JvmNameResolver<'s> {
    pub fn new(types: &Msg, strings: &'s [String]) -> Self {
        let mut records = Vec::new();
        for r in types.msgs(1) {
            let rec = std::rc::Rc::new(Record {
                predefined: r.int(2),
                string: r.string(6),
                operation: r.int(3).unwrap_or(0),
                substring: r.ints(4),
                replace_char: r.ints(5),
            });
            let range = r.int(1).unwrap_or(1).max(1);
            for _ in 0..range {
                records.push(rec.clone());
            }
        }
        JvmNameResolver { records, strings }
    }
}

impl NameResolver for JvmNameResolver<'_> {
    fn string(&self, idx: i64) -> String {
        let i = idx as usize;
        let rec = self.records.get(i);
        let mut s = match rec {
            Some(r) if r.string.is_some() => r.string.clone().unwrap(),
            Some(r) if r.predefined.is_some_and(|p| (p as usize) < PREDEFINED.len()) => PREDEFINED[r.predefined.unwrap() as usize].to_string(),
            _ => self.strings.get(i).cloned().unwrap_or_default(),
        };
        let Some(r) = rec else { return s };
        if r.substring.len() >= 2 {
            let (b, e) = (r.substring[0] as usize, r.substring[1] as usize);
            let chars: Vec<char> = s.chars().collect();
            if b <= e && e <= chars.len() {
                s = chars[b..e].iter().collect();
            }
        }
        if r.replace_char.len() >= 2
            && let (Some(from), Some(to)) = (char::from_u32(r.replace_char[0] as u32), char::from_u32(r.replace_char[1] as u32)) {
                s = s.replace(from, &to.to_string());
            }
        match r.operation {
            1 => s = s.replace('$', "."),
            2 => {
                if s.len() >= 2 {
                    s = s[1..s.len() - 1].to_string();
                }
                s = s.replace('$', ".");
            }
            _ => {}
        }
        s
    }

    fn class_fq(&self, idx: i64) -> String {
        let s = self.string(idx);
        s.trim_start_matches('.').replace('/', ".")
    }
}

/// `NameResolverImpl` used by `.kotlin_builtins`: StringTable + QualifiedNameTable.
pub struct TableNameResolver {
    strings: Vec<String>,
    /// (parent, short name index, kind) — kind: 0 CLASS, 1 PACKAGE, 2 LOCAL
    names: Vec<(i64, i64, i64)>,
}

impl TableNameResolver {
    pub fn new(strings: &Msg, names: &Msg) -> Self {
        TableNameResolver {
            strings: strings.strings(1),
            names: names.msgs(1).iter().map(|q| (q.int(1).unwrap_or(-1), q.int(2).unwrap_or(0), q.int(3).unwrap_or(1))).collect(),
        }
    }
}

impl NameResolver for TableNameResolver {
    fn string(&self, idx: i64) -> String {
        self.strings.get(idx as usize).cloned().unwrap_or_default()
    }

    fn class_fq(&self, idx: i64) -> String {
        let mut packages = Vec::new();
        let mut classes = Vec::new();
        let mut i = idx;
        let mut guard = 0;
        while i >= 0 && guard < 64 {
            let Some(&(parent, short, kind)) = self.names.get(i as usize) else { break };
            let name = self.string(short);
            if kind == 1 { packages.push(name) } else { classes.push(name) }
            i = parent;
            guard += 1;
        }
        packages.reverse();
        classes.reverse();
        let mut s = packages.join(".");
        if !s.is_empty() && !classes.is_empty() {
            s.push('.');
        }
        s.push_str(&classes.join("."));
        s
    }
}

// --------------------------------------------------------------- decoding

fn visibility(flags: i64) -> Visibility {
    match (flags >> 1) & 0b111 {
        0 => Visibility::Internal,
        1 | 4 => Visibility::Private,
        2 => Visibility::Protected,
        5 => Visibility::Private, // local
        _ => Visibility::Public,
    }
}

fn modality(flags: i64, m: &mut Modifiers) {
    match (flags >> 4) & 0b11 {
        1 => m.set(Modifiers::OPEN),
        2 => m.set(Modifiers::ABSTRACT),
        3 => m.set(Modifiers::SEALED),
        _ => {}
    }
}

fn bit(flags: i64, n: u32) -> bool {
    (flags >> n) & 1 == 1
}

struct Ctx<'r> {
    nr: &'r dyn NameResolver,
    table: Vec<Msg<'r>>,
    first_nullable: i64,
    /// Type parameter ids in scope → names (innermost last).
    type_params: Vec<(i64, String)>,
}

impl<'r> Ctx<'r> {
    fn with_table(&self, table_msg: Option<Msg<'r>>) -> Ctx<'r> {
        let (table, first_nullable) = match table_msg {
            Some(t) => (t.msgs(1), t.int(2).unwrap_or(-1)),
            None => (self.table.clone(), self.first_nullable),
        };
        Ctx { nr: self.nr, table, first_nullable, type_params: self.type_params.clone() }
    }

    /// A type given either inline (`inline_field`) or as an index into the type table.
    fn type_of(&self, m: &Msg<'r>, inline_field: u32, id_field: u32) -> Option<TypeRef> {
        if let Some(t) = m.msg(inline_field) {
            return Some(self.ty(&t, false));
        }
        let id = m.int(id_field)?;
        let t = self.table.get(id as usize)?;
        Some(self.ty(t, self.first_nullable >= 0 && id >= self.first_nullable))
    }

    fn ty(&self, t: &Msg<'r>, force_nullable: bool) -> TypeRef {
        let nullable = t.int(3) == Some(1) || force_nullable;
        let args = t
            .msgs(2)
            .iter()
            .map(|a| match a.int(1).unwrap_or(2) {
                3 => TypeArg::Star,
                p => TypeArg::Type {
                    variance: match p {
                        0 => Variance::In,
                        1 => Variance::Out,
                        _ => Variance::Invariant,
                    },
                    ty: self.type_of(a, 2, 3).unwrap_or(TypeRef::Error),
                },
            })
            .collect::<Vec<_>>();
        let base = if let Some(c) = t.int(6) {
            let fq = self.nr.class_fq(c);
            TypeRef::Named { name: fq.clone(), fq: Some(fq.into()), args, nullable }
        } else if let Some(c) = t.int(12) {
            let fq = self.nr.class_fq(c);
            TypeRef::Named { name: fq.clone(), fq: Some(fq.into()), args, nullable }
        } else if let Some(id) = t.int(7) {
            let name = self.type_params.iter().rev().find(|(i, _)| *i == id).map(|(_, n)| n.clone()).unwrap_or_else(|| format!("T{id}"));
            TypeRef::TypeParam { name, nullable }
        } else if let Some(n) = t.int(9) {
            TypeRef::TypeParam { name: self.nr.string(n), nullable }
        } else {
            TypeRef::Error
        };
        let base = self.function_type(t, base);
        // Flexible types (`T!`) carry an upper bound.
        if t.has(5) || t.has(8) { TypeRef::Platform(Box::new(base)) } else { base }
    }

    /// `kotlin.FunctionN<…>` with the suspend flag / `@ExtensionFunctionType` → a function type.
    fn function_type(&self, t: &Msg<'r>, base: TypeRef) -> TypeRef {
        let TypeRef::Named { fq: Some(fq), args, nullable, .. } = &base else { return base };
        let Some(n) = fq.strip_prefix("kotlin.Function") else { return base };
        if n.parse::<u32>().is_err() || args.is_empty() {
            return base;
        }
        let suspend = t.int(1).unwrap_or(0) & 1 == 1;
        let extension = t.msgs(100).iter().any(|a| a.int(1).is_some_and(|i| self.nr.class_fq(i) == "kotlin.ExtensionFunctionType"));
        let mut types: Vec<TypeRef> = args
            .iter()
            .map(|a| match a {
                TypeArg::Type { ty, .. } => ty.clone(),
                TypeArg::Star => TypeRef::named("kotlin.Any").nullable(true),
            })
            .collect();
        let mut ret = types.pop().unwrap();
        if suspend {
            // (P…, Continuation<R>) -> Any?  ==>  suspend (P…) -> R
            if let Some(TypeRef::Named { fq: Some(c), args, .. }) = types.last()
                && &**c == "kotlin.coroutines.Continuation" {
                    if let Some(TypeArg::Type { ty, .. }) = args.first() {
                        ret = ty.clone();
                    }
                    types.pop();
                }
        }
        let receiver = if extension && !types.is_empty() { Some(Box::new(types.remove(0))) } else { None };
        TypeRef::Function { receiver, params: types, ret: Box::new(ret), suspend, nullable: *nullable }
    }

    fn push_type_params(&mut self, params: &[Msg<'r>]) -> Vec<TypeParam> {
        for p in params {
            if let (Some(id), Some(name)) = (p.int(1), p.int(2)) {
                self.type_params.push((id, self.nr.string(name)));
            }
        }
        params
            .iter()
            .map(|p| TypeParam {
                name: p.int(2).map(|n| self.nr.string(n)).unwrap_or_default(),
                bounds: {
                    let mut b: Vec<TypeRef> = p.msgs(5).iter().map(|t| self.ty(t, false)).collect();
                    for id in p.ints(6) {
                        if let Some(t) = self.table.get(id as usize) {
                            b.push(self.ty(t, self.first_nullable >= 0 && id >= self.first_nullable));
                        }
                    }
                    b
                },
                variance: match p.int(4).unwrap_or(2) {
                    0 => Variance::In,
                    1 => Variance::Out,
                    _ => Variance::Invariant,
                },
                reified: p.int(3) == Some(1),
            })
            .collect()
    }

    fn params(&self, ps: &[Msg<'r>]) -> Vec<Param> {
        ps.iter()
            .map(|p| {
                let flags = p.int(1).unwrap_or(0);
                let vararg = self.type_of(p, 4, 6);
                Param {
                    name: p.int(2).map(|n| self.nr.string(n)).unwrap_or_default(),
                    ty: vararg.clone().or_else(|| self.type_of(p, 3, 5)).unwrap_or(TypeRef::Error),
                    has_default: bit(flags, 1),
                    vararg: vararg.is_some(),
                }
            })
            .collect()
    }

    fn function(&self, f: &Msg<'r>, container: &str, origin: &Origin) -> Symbol {
        let mut cx = self.with_table(f.msg(30));
        let name = f.int(2).map(|n| cx.nr.string(n)).unwrap_or_default();
        let flags = f.int(9).or_else(|| f.int(1)).unwrap_or(6);
        let mut s = Symbol::new(SymbolKind::Function, &name, join(container, &name), origin.clone());
        s.visibility = visibility(flags);
        modality(flags, &mut s.modifiers);
        for (b, m) in [(8, Modifiers::OPERATOR), (9, Modifiers::INFIX), (10, Modifiers::INLINE), (11, Modifiers::TAILREC), (12, Modifiers::EXTERNAL), (13, Modifiers::SUSPEND), (14, Modifiers::EXPECT)] {
            if bit(flags, b) {
                s.modifiers.set(m);
            }
        }
        if (flags >> 6) & 0b11 == 1 {
            s.modifiers.set(Modifiers::OVERRIDE);
        }
        s.type_params = cx.push_type_params(&f.msgs(4));
        s.receiver = cx.type_of(f, 5, 8);
        s.params = cx.params(&f.msgs(6));
        s.ty = cx.type_of(f, 3, 7);
        s
    }

    fn property(&self, p: &Msg<'r>, container: &str, origin: &Origin) -> Symbol {
        let mut cx = self.with_table(None);
        let name = p.int(2).map(|n| cx.nr.string(n)).unwrap_or_default();
        let flags = p.int(11).or_else(|| p.int(1)).unwrap_or(518);
        let mut s = Symbol::new(SymbolKind::Property, &name, join(container, &name), origin.clone());
        s.visibility = visibility(flags);
        modality(flags, &mut s.modifiers);
        for (b, m) in [(8, Modifiers::VAR), (11, Modifiers::CONST), (12, Modifiers::LATEINIT), (14, Modifiers::EXTERNAL), (16, Modifiers::EXPECT)] {
            if bit(flags, b) {
                s.modifiers.set(m);
            }
        }
        s.type_params = cx.push_type_params(&p.msgs(4));
        s.receiver = cx.type_of(p, 5, 10);
        s.ty = cx.type_of(p, 3, 9);
        s
    }

    fn type_alias(&self, a: &Msg<'r>, container: &str, origin: &Origin) -> Symbol {
        let mut cx = self.with_table(None);
        let name = a.int(2).map(|n| cx.nr.string(n)).unwrap_or_default();
        let flags = a.int(1).unwrap_or(6);
        let mut s = Symbol::new(SymbolKind::TypeAlias, &name, join(container, &name), origin.clone());
        s.visibility = visibility(flags);
        s.type_params = cx.push_type_params(&a.msgs(3));
        s.ty = cx.type_of(a, 6, 7).or_else(|| cx.type_of(a, 4, 5));
        s
    }
}

fn join(container: &str, name: &str) -> String {
    if container.is_empty() { name.to_string() } else { format!("{container}.{name}") }
}

/// A decoded Kotlin class.
#[derive(Debug, Clone)]
pub struct KClass {
    pub symbol: Symbol,
    pub members: Vec<Symbol>,
    /// Simple names of nested classes.
    pub nested: Vec<String>,
    pub companion: Option<String>,
    pub sealed_subclasses: Vec<String>,
}

/// Decode a `ProtoBuf.Class` message.
pub fn decode_class(class: &Msg, nr: &dyn NameResolver, origin: &Origin) -> KClass {
    let table = class.msg(30);
    let mut cx = Ctx {
        nr,
        table: table.as_ref().map(|t| t.msgs(1)).unwrap_or_default(),
        first_nullable: table.as_ref().and_then(|t| t.int(2)).unwrap_or(-1),
        type_params: Vec::new(),
    };
    let flags = class.int(1).unwrap_or(6);
    let fq = class.int(3).map(|i| nr.class_fq(i)).unwrap_or_default();
    let simple = fq.rsplit('.').next().unwrap_or(&fq).to_string();
    let kind = match (flags >> 6) & 0b111 {
        1 => SymbolKind::Interface,
        2 => SymbolKind::Enum,
        3 => SymbolKind::EnumEntry,
        4 => SymbolKind::Annotation,
        5 | 6 => SymbolKind::Object,
        _ => SymbolKind::Class,
    };
    let mut sym = Symbol::new(kind, &simple, fq.clone(), origin.clone());
    sym.visibility = visibility(flags);
    modality(flags, &mut sym.modifiers);
    if (flags >> 6) & 0b111 == 6 {
        sym.modifiers.set(Modifiers::COMPANION);
    }
    for (b, m) in [(9, Modifiers::INNER), (10, Modifiers::DATA), (11, Modifiers::EXTERNAL), (12, Modifiers::EXPECT), (13, Modifiers::VALUE), (14, Modifiers::FUN_INTERFACE)] {
        if bit(flags, b) {
            sym.modifiers.set(m);
        }
    }
    sym.type_params = cx.push_type_params(&class.msgs(5));
    sym.supertypes = class.msgs(6).iter().map(|t| cx.ty(t, false)).collect();
    for id in class.ints(2) {
        if let Some(t) = cx.table.get(id as usize) {
            sym.supertypes.push(cx.ty(t, cx.first_nullable >= 0 && id >= cx.first_nullable));
        }
    }
    sym.supertypes.retain(|t| t.fq_name() != Some("kotlin.Any"));

    let mut members = Vec::new();
    for c in class.msgs(8) {
        let cflags = c.int(1).unwrap_or(6);
        let mut s = Symbol::new(SymbolKind::Constructor, "<init>", join(&fq, "<init>"), origin.clone());
        s.visibility = visibility(cflags);
        s.params = cx.params(&c.msgs(2));
        s.ty = Some(TypeRef::named(&fq));
        members.push(s);
    }
    for f in class.msgs(9) {
        members.push(cx.function(&f, &fq, origin));
    }
    for p in class.msgs(10) {
        members.push(cx.property(&p, &fq, origin));
    }
    for a in class.msgs(11) {
        members.push(cx.type_alias(&a, &fq, origin));
    }
    for e in class.msgs(13) {
        let name = e.int(1).map(|n| nr.string(n)).unwrap_or_default();
        let mut s = Symbol::new(SymbolKind::EnumEntry, &name, join(&fq, &name), origin.clone());
        s.ty = Some(TypeRef::named(&fq));
        members.push(s);
    }
    KClass {
        symbol: sym,
        members,
        nested: class.ints(7).iter().map(|&i| nr.string(i)).collect(),
        companion: class.int(4).map(|i| nr.string(i)),
        sealed_subclasses: class.ints(16).iter().map(|&i| nr.class_fq(i)).collect(),
    }
}

/// Decode a `ProtoBuf.Package` message into top-level declarations.
pub fn decode_package(pkg: &Msg, nr: &dyn NameResolver, package: &str, origin: &Origin) -> Vec<Symbol> {
    let table = pkg.msg(30);
    let cx = Ctx {
        nr,
        table: table.as_ref().map(|t| t.msgs(1)).unwrap_or_default(),
        first_nullable: table.as_ref().and_then(|t| t.int(2)).unwrap_or(-1),
        type_params: Vec::new(),
    };
    let mut out = Vec::new();
    for f in pkg.msgs(3) {
        out.push(cx.function(&f, package, origin));
    }
    for p in pkg.msgs(4) {
        out.push(cx.property(&p, package, origin));
    }
    for a in pkg.msgs(5) {
        out.push(cx.type_alias(&a, package, origin));
    }
    out
}

pub enum Decoded {
    Class(KClass),
    Package(Vec<Symbol>),
    /// Synthetic classes, multi-file facades: nothing to index.
    Other,
}

/// Decode `@kotlin.Metadata` of a class file.
pub fn decode(meta: &super::classfile::KotlinMetadata, package: &str, archive: &Path, class: &str) -> Option<Decoded> {
    // Synthetic classes (3) and multi-file facades (4, whose d1 lists part names) carry
    // no declarations.
    if !matches!(meta.kind, 1 | 2 | 5) {
        return Some(Decoded::Other);
    }
    let bytes = decode_bytes(&meta.data1);
    let mut pos = 0;
    let len = varint(&bytes, &mut pos)? as usize;
    let types = Msg::parse(bytes.get(pos..pos + len)?)?;
    let body = bytes.get(pos + len..)?;
    let nr = JvmNameResolver::new(&types, &meta.data2);
    let origin = Origin::Library { archive: archive.to_path_buf(), class: class.to_string() };
    match meta.kind {
        1 => {
            let msg = Msg::parse(body)?;
            Some(Decoded::Class(decode_class(&msg, &nr, &origin)))
        }
        2 | 5 => {
            let msg = Msg::parse(body)?;
            let pkg = meta.package_name.as_deref().unwrap_or(package);
            Some(Decoded::Package(decode_package(&msg, &nr, pkg, &origin)))
        }
        _ => Some(Decoded::Other),
    }
}

/// Decode a `.kotlin_builtins` file: (package name, classes, top-level members).
pub fn decode_builtins(data: &[u8]) -> Option<(String, Vec<KClass>, Vec<Symbol>)> {
    // BuiltInsBinaryVersion: int count, then `count` ints (big endian).
    let count = u32::from_be_bytes(data.get(0..4)?.try_into().ok()?) as usize;
    let start = 4 + count * 4;
    let fragment = Msg::parse(data.get(start..)?)?;
    let nr = TableNameResolver::new(&fragment.msg(1)?, &fragment.msg(2).unwrap_or_default());
    let origin = Origin::Builtin;
    let classes: Vec<KClass> = fragment.msgs(4).iter().map(|c| decode_class(c, &nr, &origin)).collect();
    let package = classes.first().map(|c| c.symbol.fq_name.rsplit_once('.').map(|(p, _)| p.to_string()).unwrap_or_default()).unwrap_or_default();
    let members = fragment.msg(3).map(|p| decode_package(&p, &nr, &package, &origin)).unwrap_or_default();
    Some((package, classes, members))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_7to8_roundtrip_layout() {
        // UTF-8 mode marker
        let d1 = vec![vec![0u16, 0x08, 0x96, 0x01]];
        assert_eq!(decode_bytes(&d1), vec![0x08, 0x96, 0x01]);
    }

    #[test]
    fn varints() {
        let mut pos = 0;
        assert_eq!(varint(&[0x96, 0x01], &mut pos), Some(150));
        let m = Msg::parse(&[0x08, 0x96, 0x01, 0x12, 0x02, 0x01, 0x02]).unwrap();
        assert_eq!(m.int(1), Some(150));
        assert_eq!(m.ints(2), vec![1, 2]);
    }
}
