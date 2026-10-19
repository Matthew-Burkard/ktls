//! JVM class file reader (only the parts a language server needs).

pub const ACC_PUBLIC: u16 = 0x0001;
pub const ACC_PRIVATE: u16 = 0x0002;
pub const ACC_PROTECTED: u16 = 0x0004;
pub const ACC_STATIC: u16 = 0x0008;
pub const ACC_FINAL: u16 = 0x0010;
pub const ACC_BRIDGE: u16 = 0x0040;
pub const ACC_VARARGS: u16 = 0x0080;
pub const ACC_INTERFACE: u16 = 0x0200;
pub const ACC_ABSTRACT: u16 = 0x0400;
pub const ACC_SYNTHETIC: u16 = 0x1000;
pub const ACC_ANNOTATION: u16 = 0x2000;
pub const ACC_ENUM: u16 = 0x4000;

#[derive(Debug, Clone, PartialEq)]
pub enum ElementValue {
    Int(i64),
    Float(f64),
    /// String constant as raw UTF-16 code units (Kotlin metadata `d1` packs bytes into chars).
    Str(Vec<u16>),
    Enum { type_desc: String, name: String },
    Class(String),
    Annotation(Annotation),
    Array(Vec<ElementValue>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    /// Type descriptor, e.g. `Lkotlin/Metadata;`.
    pub type_desc: String,
    pub values: Vec<(String, ElementValue)>,
}

impl Annotation {
    pub fn get(&self, name: &str) -> Option<&ElementValue> {
        self.values.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

impl ElementValue {
    pub fn as_string(&self) -> Option<String> {
        match self {
            ElementValue::Str(u) => Some(String::from_utf16_lossy(u)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Member {
    pub access: u16,
    pub name: String,
    pub descriptor: String,
    pub signature: Option<String>,
    pub annotations: Vec<Annotation>,
    /// Per-parameter annotation type descriptors.
    pub param_annotations: Vec<Vec<String>>,
    pub param_names: Vec<String>,
    pub deprecated: bool,
}

impl Member {
    pub fn has_annotation(&self, desc: &str) -> bool {
        self.annotations.iter().any(|a| a.type_desc == desc)
    }
}

#[derive(Debug, Clone)]
pub struct InnerClass {
    pub inner: String,
    pub outer: Option<String>,
    pub simple_name: Option<String>,
    pub access: u16,
}

/// Raw `@kotlin.Metadata` contents.
#[derive(Debug, Clone, Default)]
pub struct KotlinMetadata {
    pub kind: i32,
    pub version: Vec<i32>,
    /// `d1`, as UTF-16 code units (the payload is bit-packed into chars).
    pub data1: Vec<Vec<u16>>,
    pub data2: Vec<String>,
    pub extra_string: Option<String>,
    pub package_name: Option<String>,
    pub extra_int: i32,
}

#[derive(Debug, Clone, Default)]
pub struct ClassFile {
    pub access: u16,
    pub name: String,
    pub super_name: Option<String>,
    pub interfaces: Vec<String>,
    pub signature: Option<String>,
    pub fields: Vec<Member>,
    pub methods: Vec<Member>,
    pub inner_classes: Vec<InnerClass>,
    pub annotations: Vec<Annotation>,
    pub kotlin: Option<KotlinMetadata>,
    pub deprecated: bool,
    /// `SourceFile` attribute (file name only, e.g. `Collections.kt`).
    pub source_file: Option<String>,
}

impl ClassFile {
    /// Simple name and access flags of this class when it is a nested class.
    pub fn inner_info(&self) -> Option<&InnerClass> {
        self.inner_classes.iter().find(|ic| ic.inner == self.name)
    }

    pub fn has_annotation(&self, desc: &str) -> bool {
        self.annotations.iter().any(|a| a.type_desc == desc)
    }
}

#[derive(Debug, Clone)]
enum Const {
    Utf8(Vec<u16>),
    Int(i32),
    Float(f32),
    Long(i64),
    Double(f64),
    Class(u16),
    Str,
    Other,
    Unusable,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

type R<T> = Result<T, &'static str>;

impl<'a> Reader<'a> {
    fn u1(&mut self) -> R<u8> {
        let b = *self.data.get(self.pos).ok_or("eof")?;
        self.pos += 1;
        Ok(b)
    }
    fn u2(&mut self) -> R<u16> {
        let s = self.data.get(self.pos..self.pos + 2).ok_or("eof")?;
        self.pos += 2;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    fn u4(&mut self) -> R<u32> {
        let s = self.data.get(self.pos..self.pos + 4).ok_or("eof")?;
        self.pos += 4;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        let s = self.data.get(self.pos..self.pos + n).ok_or("eof")?;
        self.pos += n;
        Ok(s)
    }
}

/// Decode "modified UTF-8" into UTF-16 code units.
pub fn decode_mutf8(bytes: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i] as u16;
        if b & 0x80 == 0 {
            out.push(b);
            i += 1;
        } else if b & 0xE0 == 0xC0 && i + 1 < bytes.len() {
            out.push(((b & 0x1F) << 6) | (bytes[i + 1] as u16 & 0x3F));
            i += 2;
        } else if b & 0xF0 == 0xE0 && i + 2 < bytes.len() {
            out.push(((b & 0x0F) << 12) | ((bytes[i + 1] as u16 & 0x3F) << 6) | (bytes[i + 2] as u16 & 0x3F));
            i += 3;
        } else {
            out.push(0xFFFD);
            i += 1;
        }
    }
    out
}

struct Pool {
    entries: Vec<Const>,
}

impl Pool {
    fn utf8(&self, idx: u16) -> R<String> {
        match self.entries.get(idx as usize) {
            Some(Const::Utf8(u)) => Ok(String::from_utf16_lossy(u)),
            _ => Err("expected utf8"),
        }
    }
    fn utf16(&self, idx: u16) -> R<Vec<u16>> {
        match self.entries.get(idx as usize) {
            Some(Const::Utf8(u)) => Ok(u.clone()),
            _ => Err("expected utf8"),
        }
    }
    fn class(&self, idx: u16) -> R<String> {
        match self.entries.get(idx as usize) {
            Some(Const::Class(n)) => self.utf8(*n),
            _ => Err("expected class"),
        }
    }
}

pub fn parse(data: &[u8]) -> Result<ClassFile, &'static str> {
    let mut r = Reader { data, pos: 0 };
    if r.u4()? != 0xCAFEBABE {
        return Err("bad magic");
    }
    let _minor = r.u2()?;
    let _major = r.u2()?;
    let count = r.u2()? as usize;
    let mut entries = Vec::with_capacity(count);
    entries.push(Const::Unusable);
    let mut i = 1;
    while i < count {
        let tag = r.u1()?;
        let c = match tag {
            1 => {
                let len = r.u2()? as usize;
                Const::Utf8(decode_mutf8(r.bytes(len)?))
            }
            3 => Const::Int(r.u4()? as i32),
            4 => Const::Float(f32::from_bits(r.u4()?)),
            5 => {
                let hi = r.u4()? as u64;
                let lo = r.u4()? as u64;
                Const::Long(((hi << 32) | lo) as i64)
            }
            6 => {
                let hi = r.u4()? as u64;
                let lo = r.u4()? as u64;
                Const::Double(f64::from_bits((hi << 32) | lo))
            }
            7 => Const::Class(r.u2()?),
            8 => {
                r.u2()?;
                Const::Str
            }
            9..=12 | 17 | 18 => {
                r.u4()?;
                Const::Other
            }
            15 => {
                r.bytes(3)?;
                Const::Other
            }
            16 | 19 | 20 => {
                r.u2()?;
                Const::Other
            }
            _ => return Err("bad constant tag"),
        };
        let wide = matches!(c, Const::Long(_) | Const::Double(_));
        entries.push(c);
        if wide {
            entries.push(Const::Unusable);
            i += 1;
        }
        i += 1;
    }
    let pool = Pool { entries };
    let mut cf = ClassFile { access: r.u2()?, ..Default::default() };
    cf.name = pool.class(r.u2()?)?;
    let sup = r.u2()?;
    cf.super_name = if sup == 0 { None } else { Some(pool.class(sup)?) };
    let n = r.u2()?;
    for _ in 0..n {
        cf.interfaces.push(pool.class(r.u2()?)?);
    }
    for is_method in [false, true] {
        let n = r.u2()?;
        for _ in 0..n {
            let mut m = Member { access: r.u2()?, ..Default::default() };
            m.name = pool.utf8(r.u2()?)?;
            m.descriptor = pool.utf8(r.u2()?)?;
            let attrs = r.u2()?;
            for _ in 0..attrs {
                let name = pool.utf8(r.u2()?)?;
                let len = r.u4()? as usize;
                let body = r.bytes(len)?;
                let mut ar = Reader { data: body, pos: 0 };
                match name.as_str() {
                    "Signature" => m.signature = Some(pool.utf8(ar.u2()?)?),
                    "Deprecated" => m.deprecated = true,
                    "RuntimeVisibleAnnotations" | "RuntimeInvisibleAnnotations" => {
                        let n = ar.u2()?;
                        for _ in 0..n {
                            m.annotations.push(annotation(&mut ar, &pool)?);
                        }
                    }
                    "RuntimeVisibleParameterAnnotations" | "RuntimeInvisibleParameterAnnotations" => {
                        let params = ar.u1()? as usize;
                        if m.param_annotations.len() < params {
                            m.param_annotations.resize(params, Vec::new());
                        }
                        for p in 0..params {
                            let n = ar.u2()?;
                            for _ in 0..n {
                                let a = annotation(&mut ar, &pool)?;
                                m.param_annotations[p].push(a.type_desc);
                            }
                        }
                    }
                    "MethodParameters" if is_method => {
                        let n = ar.u1()?;
                        for _ in 0..n {
                            let idx = ar.u2()?;
                            let _flags = ar.u2()?;
                            m.param_names.push(if idx == 0 { String::new() } else { pool.utf8(idx).unwrap_or_default() });
                        }
                    }
                    _ => {}
                }
            }
            if m.has_annotation("Ljava/lang/Deprecated;") || m.has_annotation("Lkotlin/Deprecated;") {
                m.deprecated = true;
            }
            if is_method { cf.methods.push(m) } else { cf.fields.push(m) }
        }
    }
    let n = r.u2()?;
    for _ in 0..n {
        let name = pool.utf8(r.u2()?)?;
        let len = r.u4()? as usize;
        let body = r.bytes(len)?;
        let mut ar = Reader { data: body, pos: 0 };
        match name.as_str() {
            "Signature" => cf.signature = Some(pool.utf8(ar.u2()?)?),
            "Deprecated" => cf.deprecated = true,
            "SourceFile" => cf.source_file = pool.utf8(ar.u2()?).ok(),
            "InnerClasses" => {
                let n = ar.u2()?;
                for _ in 0..n {
                    let inner = ar.u2()?;
                    let outer = ar.u2()?;
                    let simple = ar.u2()?;
                    let access = ar.u2()?;
                    let Ok(inner) = pool.class(inner) else { continue };
                    cf.inner_classes.push(InnerClass {
                        inner,
                        outer: if outer == 0 { None } else { pool.class(outer).ok() },
                        simple_name: if simple == 0 { None } else { pool.utf8(simple).ok() },
                        access,
                    });
                }
            }
            "RuntimeVisibleAnnotations" | "RuntimeInvisibleAnnotations" => {
                let n = ar.u2()?;
                for _ in 0..n {
                    let a = annotation(&mut ar, &pool)?;
                    if a.type_desc == "Lkotlin/Metadata;" {
                        cf.kotlin = Some(kotlin_metadata(&a));
                    }
                    cf.annotations.push(a);
                }
            }
            _ => {}
        }
    }
    if cf.has_annotation("Ljava/lang/Deprecated;") || cf.has_annotation("Lkotlin/Deprecated;") {
        cf.deprecated = true;
    }
    Ok(cf)
}

fn annotation(r: &mut Reader, pool: &Pool) -> R<Annotation> {
    let type_desc = pool.utf8(r.u2()?)?;
    let n = r.u2()?;
    let mut values = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let name = pool.utf8(r.u2()?)?;
        values.push((name, element_value(r, pool)?));
    }
    Ok(Annotation { type_desc, values })
}

fn element_value(r: &mut Reader, pool: &Pool) -> R<ElementValue> {
    let tag = r.u1()?;
    Ok(match tag {
        b'B' | b'C' | b'I' | b'S' | b'Z' | b'J' => {
            let idx = r.u2()?;
            match pool.entries.get(idx as usize) {
                Some(Const::Int(i)) => ElementValue::Int(*i as i64),
                Some(Const::Long(l)) => ElementValue::Int(*l),
                _ => ElementValue::Int(0),
            }
        }
        b'F' | b'D' => {
            let idx = r.u2()?;
            match pool.entries.get(idx as usize) {
                Some(Const::Float(f)) => ElementValue::Float(*f as f64),
                Some(Const::Double(d)) => ElementValue::Float(*d),
                _ => ElementValue::Float(0.0),
            }
        }
        b's' => ElementValue::Str(pool.utf16(r.u2()?)?),
        b'e' => {
            let type_desc = pool.utf8(r.u2()?)?;
            let name = pool.utf8(r.u2()?)?;
            ElementValue::Enum { type_desc, name }
        }
        b'c' => ElementValue::Class(pool.utf8(r.u2()?)?),
        b'@' => ElementValue::Annotation(annotation(r, pool)?),
        b'[' => {
            let n = r.u2()?;
            let mut items = Vec::with_capacity(n as usize);
            for _ in 0..n {
                items.push(element_value(r, pool)?);
            }
            ElementValue::Array(items)
        }
        _ => return Err("bad element tag"),
    })
}

fn kotlin_metadata(a: &Annotation) -> KotlinMetadata {
    let ints = |v: Option<&ElementValue>| -> Vec<i32> {
        match v {
            Some(ElementValue::Array(items)) => items.iter().filter_map(|i| if let ElementValue::Int(x) = i { Some(*x as i32) } else { None }).collect(),
            _ => Vec::new(),
        }
    };
    let raw = |v: Option<&ElementValue>| -> Vec<Vec<u16>> {
        match v {
            Some(ElementValue::Array(items)) => items.iter().filter_map(|i| if let ElementValue::Str(s) = i { Some(s.clone()) } else { None }).collect(),
            _ => Vec::new(),
        }
    };
    KotlinMetadata {
        kind: match a.get("k") {
            Some(ElementValue::Int(k)) => *k as i32,
            _ => 1,
        },
        version: ints(a.get("mv")),
        data1: raw(a.get("d1")),
        data2: raw(a.get("d2")).iter().map(|u| String::from_utf16_lossy(u)).collect(),
        extra_string: a.get("xs").and_then(ElementValue::as_string),
        package_name: a.get("pn").and_then(ElementValue::as_string),
        extra_int: match a.get("xi") {
            Some(ElementValue::Int(x)) => *x as i32,
            _ => 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutf8() {
        assert_eq!(decode_mutf8(&[0x41, 0xC0, 0x80, 0xE2, 0x82, 0xAC]), vec![0x41, 0, 0x20AC]);
    }
}
