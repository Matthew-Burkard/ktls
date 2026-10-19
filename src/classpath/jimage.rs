//! Reader for the JDK runtime image (`lib/modules`, "jimage" format).

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Only the index (header, tables, strings) is kept in memory; resources are read
/// on demand with positioned reads.
pub struct JImage {
    file: Option<File>,
    /// The index; when built from bytes in tests, the whole image.
    data: Vec<u8>,
    little_endian: bool,
    table_length: usize,
    offsets_start: usize,
    locations_start: usize,
    strings_start: usize,
    index_size: usize,
}

const HEADER_SIZE: usize = 7 * 4;
const MAGIC: u32 = 0xCAFE_DADA;

#[derive(Default, Debug)]
struct Location {
    module: u64,
    parent: u64,
    base: u64,
    extension: u64,
    offset: u64,
    compressed: u64,
    uncompressed: u64,
}

impl JImage {
    pub fn open(path: &Path) -> anyhow::Result<JImage> {
        let mut file = File::open(path)?;
        let mut header = vec![0u8; HEADER_SIZE];
        file.read_exact(&mut header)?;
        let probe = Self::from_bytes(header.clone(), true)?;
        let mut index = vec![0u8; probe.index_size];
        read_at(&file, &mut index, 0)?;
        let mut img = Self::from_bytes(index, false)?;
        img.file = Some(file);
        Ok(img)
    }

    fn from_bytes(data: Vec<u8>, header_only: bool) -> anyhow::Result<JImage> {
        anyhow::ensure!(data.len() >= HEADER_SIZE, "jimage too small");
        let le = u32::from_le_bytes(data[0..4].try_into().unwrap()) == MAGIC;
        let be = u32::from_be_bytes(data[0..4].try_into().unwrap()) == MAGIC;
        anyhow::ensure!(le || be, "not a jimage file");
        let mut img = JImage { file: None, data, little_endian: le, table_length: 0, offsets_start: 0, locations_start: 0, strings_start: 0, index_size: 0 };
        let version = img.u32_at(4);
        anyhow::ensure!(version >> 16 == 1, "unsupported jimage version {}.{}", version >> 16, version & 0xFFFF);
        img.table_length = img.u32_at(16) as usize;
        let locations_size = img.u32_at(20) as usize;
        let strings_size = img.u32_at(24) as usize;
        img.offsets_start = HEADER_SIZE + img.table_length * 4;
        img.locations_start = img.offsets_start + img.table_length * 4;
        img.strings_start = img.locations_start + locations_size;
        img.index_size = img.strings_start + strings_size;
        anyhow::ensure!(header_only || img.index_size <= img.data.len(), "truncated jimage");
        Ok(img)
    }

    fn u32_at(&self, pos: usize) -> u32 {
        let b: [u8; 4] = self.data[pos..pos + 4].try_into().unwrap();
        if self.little_endian { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) }
    }

    fn string(&self, offset: u64) -> &str {
        let start = self.strings_start + offset as usize;
        if start >= self.index_size {
            return "";
        }
        let end = self.data[start..self.index_size].iter().position(|&b| b == 0).map_or(self.index_size, |p| start + p);
        std::str::from_utf8(&self.data[start..end]).unwrap_or("")
    }

    fn location(&self, offset: usize) -> Location {
        let mut attrs = [0u64; 8];
        let mut i = self.locations_start + offset;
        while i < self.strings_start {
            let byte = self.data[i];
            i += 1;
            if byte <= 0x7 {
                break;
            }
            let kind = (byte >> 3) as usize;
            let len = (byte & 0x7) as usize + 1;
            let mut value = 0u64;
            for _ in 0..len {
                value = (value << 8) | self.data.get(i).copied().unwrap_or(0) as u64;
                i += 1;
            }
            if kind < attrs.len() {
                attrs[kind] = value;
            }
        }
        Location {
            module: attrs[1],
            parent: attrs[2],
            base: attrs[3],
            extension: attrs[4],
            offset: attrs[5],
            compressed: attrs[6],
            uncompressed: attrs[7],
        }
    }

    fn full_name(&self, loc: &Location) -> String {
        let mut s = String::new();
        if loc.module != 0 {
            s.push('/');
            s.push_str(self.string(loc.module));
            s.push('/');
        }
        if loc.parent != 0 {
            s.push_str(self.string(loc.parent));
            s.push('/');
        }
        s.push_str(self.string(loc.base));
        if loc.extension != 0 {
            s.push('.');
            s.push_str(self.string(loc.extension));
        }
        s
    }

    /// All `.class` entries as `(module, class path within module)`, e.g.
    /// `("java.base", "java/lang/Object.class")`.
    pub fn class_entries(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for i in 0..self.table_length {
            let off = self.u32_at(self.offsets_start + i * 4) as usize;
            let loc = self.location(off);
            if loc.module == 0 || self.string(loc.extension) != "class" {
                continue;
            }
            let module = self.string(loc.module).to_string();
            let mut path = String::new();
            if loc.parent != 0 {
                path.push_str(self.string(loc.parent));
                path.push('/');
            }
            path.push_str(self.string(loc.base));
            path.push_str(".class");
            out.push((module, path));
        }
        out
    }

    /// Find a resource by full name (`/java.base/java/lang/Object.class`).
    fn find(&self, name: &str) -> Option<Location> {
        if self.table_length == 0 {
            return None;
        }
        let index = {
            let h = hash(name, 0x0100_0193) as usize % self.table_length;
            let redirect = self.u32_at(HEADER_SIZE + h * 4) as i32;
            if redirect < 0 {
                (-1 - redirect) as usize
            } else if redirect > 0 {
                hash(name, redirect as u32) as usize % self.table_length
            } else {
                return None;
            }
        };
        let off = self.u32_at(self.offsets_start + index * 4) as usize;
        let loc = self.location(off);
        (self.full_name(&loc) == name).then_some(loc)
    }

    pub fn read(&self, module: &str, path: &str) -> Option<Vec<u8>> {
        let loc = self.find(&format!("/{module}/{path}"))?;
        let start = self.index_size as u64 + loc.offset;
        let size = if loc.compressed == 0 { loc.uncompressed } else { loc.compressed } as usize;
        let mut bytes = vec![0u8; size];
        match &self.file {
            Some(f) => read_at(f, &mut bytes, start).ok()?,
            None => bytes.copy_from_slice(self.data.get(start as usize..start as usize + size)?),
        }
        if loc.compressed == 0 {
            return Some(bytes);
        }
        // A chain of compressed headers; only the "zip" decompressor is supported.
        loop {
            if bytes.len() < 29 || u32::from_le_bytes(bytes[0..4].try_into().ok()?) != 0xCAFE_FAFA {
                return Some(bytes);
            }
            let compressed_size = u64::from_le_bytes(bytes[4..12].try_into().ok()?) as usize;
            let decompressor = u32::from_le_bytes(bytes[20..24].try_into().ok()?) as u64;
            if self.string(decompressor) != "zip" {
                return None;
            }
            let payload = bytes.get(29..29 + compressed_size)?;
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(payload).read_to_end(&mut out).ok()?;
            bytes = out;
        }
    }
}

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_at(f: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = f.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
    }
    Ok(())
}

/// `ImageStringsReader.hashCode`: FNV-1a style over the UTF-8 bytes.
fn hash(s: &str, seed: u32) -> u32 {
    let mut h = seed;
    for b in s.bytes() {
        h = h.wrapping_mul(0x0100_0193) ^ b as u32;
    }
    h & 0x7FFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_local_jdk_if_present() {
        let Some(jdk) = crate::project::jdk::find(None, None) else { return };
        let crate::project::jdk::JdkClasses::JImage(path) = &jdk.classes else { return };
        let img = JImage::open(path).unwrap();
        let entries = img.class_entries();
        assert!(entries.len() > 10_000, "{}", entries.len());
        assert!(entries.iter().any(|(m, p)| m == "java.base" && p == "java/lang/Object.class"));
        let bytes = img.read("java.base", "java/lang/String.class").expect("String.class");
        let cf = crate::classpath::classfile::parse(&bytes).unwrap();
        assert_eq!(cf.name, "java/lang/String");
        assert!(cf.methods.iter().any(|m| m.name == "substring"));
    }
}
