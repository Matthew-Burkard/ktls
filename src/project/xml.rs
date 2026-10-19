//! A tiny, forgiving XML reader: enough for POMs, Maven metadata and settings.xml.
//!
//! Namespaces are reduced to local names; comments, processing instructions and
//! DOCTYPEs are skipped; CDATA and the standard/character entities are decoded.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    /// Concatenated, trimmed text content of this element (not of its children).
    pub text: String,
}

impl Element {
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str()).filter(|t| !t.is_empty())
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Follow a path of child names.
    pub fn path(&self, path: &[&str]) -> Option<&Element> {
        let mut cur = self;
        for p in path {
            cur = cur.child(p)?;
        }
        Some(cur)
    }
}

/// Parse a document and return its root element.
pub fn parse(text: &str) -> Option<Element> {
    let mut p = XmlParser { s: text.as_bytes(), src: text, i: 0 };
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    while p.i < p.s.len() {
        if p.s[p.i] == b'<' {
            if p.starts_with("<!--") {
                p.skip_past("-->");
            } else if p.starts_with("<![CDATA[") {
                let start = p.i + 9;
                p.skip_past("]]>");
                let end = p.i.saturating_sub(3).max(start);
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&text[start..end]);
                }
            } else if p.starts_with("<?") {
                p.skip_past("?>");
            } else if p.starts_with("<!") {
                p.skip_past(">");
            } else if p.starts_with("</") {
                p.skip_past(">");
                let mut el = stack.pop()?;
                el.text = el.text.trim().to_string();
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => {
                        root = Some(el);
                        break;
                    }
                }
            } else {
                let (el, self_closing) = p.open_tag()?;
                if self_closing {
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(el),
                        None => {
                            root = Some(el);
                            break;
                        }
                    }
                } else {
                    stack.push(el);
                }
            }
        } else {
            let start = p.i;
            while p.i < p.s.len() && p.s[p.i] != b'<' {
                p.i += 1;
            }
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&decode_entities(&text[start..p.i]));
            }
        }
    }
    // Unclosed document: fold the stack.
    if root.is_none() {
        while let Some(mut el) = stack.pop() {
            el.text = el.text.trim().to_string();
            match stack.last_mut() {
                Some(parent) => parent.children.push(el),
                None => root = Some(el),
            }
        }
    }
    root
}

struct XmlParser<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
}

impl XmlParser<'_> {
    fn starts_with(&self, pat: &str) -> bool {
        self.s[self.i..].starts_with(pat.as_bytes())
    }

    fn skip_past(&mut self, pat: &str) {
        match self.src[self.i..].find(pat) {
            Some(off) => self.i += off + pat.len(),
            None => self.i = self.s.len(),
        }
    }

    fn open_tag(&mut self) -> Option<(Element, bool)> {
        self.i += 1; // <
        let name = self.name();
        let mut el = Element { name: local(&name), ..Default::default() };
        loop {
            self.skip_ws();
            if self.i >= self.s.len() {
                return Some((el, true));
            }
            match self.s[self.i] {
                b'/' => {
                    self.skip_past(">");
                    return Some((el, true));
                }
                b'>' => {
                    self.i += 1;
                    return Some((el, false));
                }
                _ => {
                    let key = self.name();
                    if key.is_empty() {
                        self.i += 1;
                        continue;
                    }
                    self.skip_ws();
                    let mut value = String::new();
                    if self.i < self.s.len() && self.s[self.i] == b'=' {
                        self.i += 1;
                        self.skip_ws();
                        if self.i < self.s.len() && (self.s[self.i] == b'"' || self.s[self.i] == b'\'') {
                            let q = self.s[self.i];
                            let start = self.i + 1;
                            let mut j = start;
                            while j < self.s.len() && self.s[j] != q {
                                j += 1;
                            }
                            value = decode_entities(&self.src[start..j]);
                            self.i = (j + 1).min(self.s.len());
                        }
                    }
                    el.attrs.push((local(&key), value));
                }
            }
        }
    }

    fn name(&mut self) -> String {
        let start = self.i;
        while self.i < self.s.len() && !matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/' | b'=') {
            self.i += 1;
        }
        self.src[start..self.i].to_string()
    }

    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
}

fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_string()
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|&i| i < 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pom_like_documents() {
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- comment -->
<project xmlns="http://maven.apache.org/POM/4.0.0" xmlns:xsi="x">
  <modelVersion>4.0.0</modelVersion>
  <name>A &amp; B &#65;</name>
  <description><![CDATA[<raw>]]></description>
  <empty/>
  <dependencies>
    <dependency><groupId>g</groupId><artifactId>a</artifactId></dependency>
    <dependency><groupId>g2</groupId><artifactId>b</artifactId></dependency>
  </dependencies>
  <x:prefixed attr='v'>t</x:prefixed>
</project>"#;
        let root = parse(doc).unwrap();
        assert_eq!(root.name, "project");
        assert_eq!(root.child_text("name"), Some("A & B A"));
        assert_eq!(root.child_text("description"), Some("<raw>"));
        assert!(root.child("empty").is_some());
        assert_eq!(root.child("dependencies").unwrap().children_named("dependency").count(), 2);
        assert_eq!(root.child("prefixed").unwrap().attr("attr"), Some("v"));
    }

    #[test]
    fn tolerates_truncation() {
        let root = parse("<a><b>1</b><c>").unwrap();
        assert_eq!(root.child_text("b"), Some("1"));
    }
}
