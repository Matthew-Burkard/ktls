//! Small shared helpers: URI <-> path conversion, hashing.

use std::path::{Path, PathBuf};

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~:@!$&'()*+,;=".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn path_to_uri(path: &Path) -> lsp_types::Uri {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = if s.starts_with('/') { s } else { format!("/{s}") };
    format!("file://{}", percent_encode_path(&s)).parse().expect("valid file URI")
}

pub fn uri_to_path(uri: &lsp_types::Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://")?;
    // Strip an authority ("file://host/path") – only localhost/empty is meaningful.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(i) => &rest[i..],
        None => return None,
    };
    let decoded = percent_decode(path);
    // Windows drive letters: /C:/x -> C:/x
    if decoded.len() > 3 && decoded.as_bytes()[0] == b'/' && decoded.as_bytes()[2] == b':' {
        return Some(PathBuf::from(&decoded[1..]));
    }
    Some(PathBuf::from(decoded))
}

pub fn short_hash(s: &str) -> String {
    sha1_smol::Sha1::from(s).digest().to_string()[..12].to_string()
}

/// Default cache directory (`$XDG_CACHE_HOME/ktls`).
pub fn default_cache_dir() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    let dir = base.join("ktls");
    // Keep the cache of the server's former name.
    let old = base.join("kotlin-lsp-rust");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip() {
        let p = PathBuf::from("/home/user/my project/Main.kt");
        let uri = path_to_uri(&p);
        assert_eq!(uri.as_str(), "file:///home/user/my%20project/Main.kt");
        assert_eq!(uri_to_path(&uri), Some(p));
        let uri: lsp_types::Uri = "file:///C:/x/y.kt".parse().unwrap();
        assert_eq!(uri_to_path(&uri), Some(PathBuf::from("C:/x/y.kt")));
        assert_eq!(percent_decode("a%2Fb%"), "a/b%");
    }
}
