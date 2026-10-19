//! Maven coordinates and version ordering.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

/// `group:artifact` — the identity used for conflict resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModuleId {
    pub group: String,
    pub artifact: String,
}

impl ModuleId {
    pub fn new(group: impl Into<String>, artifact: impl Into<String>) -> Self {
        ModuleId { group: group.into(), artifact: artifact.into() }
    }
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.group, self.artifact)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Coordinate {
    pub group: String,
    pub artifact: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    /// File extension / packaging type (`jar`, `aar`, `pom`, `klib`...).
    #[serde(default = "default_ext")]
    pub ext: String,
}

fn default_ext() -> String {
    "jar".into()
}

impl Coordinate {
    pub fn new(group: &str, artifact: &str, version: &str) -> Self {
        Coordinate {
            group: group.into(),
            artifact: artifact.into(),
            version: version.into(),
            classifier: None,
            ext: "jar".into(),
        }
    }

    pub fn module(&self) -> ModuleId {
        ModuleId::new(self.group.clone(), self.artifact.clone())
    }

    /// Parse Gradle/Maven short notation `group:artifact[:version[:classifier]][@ext]`.
    pub fn parse(s: &str) -> Option<Coordinate> {
        let s = s.trim();
        let (body, ext) = match s.rsplit_once('@') {
            Some((b, e)) if !e.contains(':') => (b, Some(e)),
            _ => (s, None),
        };
        let parts: Vec<&str> = body.split(':').collect();
        if parts.len() < 2 || parts.iter().take(2).any(|p| p.is_empty()) {
            return None;
        }
        Some(Coordinate {
            group: parts[0].into(),
            artifact: parts[1].into(),
            version: parts.get(2).map(|v| v.to_string()).unwrap_or_default(),
            classifier: parts.get(3).filter(|c| !c.is_empty()).map(|c| c.to_string()),
            ext: ext.unwrap_or("jar").into(),
        })
    }

    /// Path of a file of this artifact inside a Maven-layout repository.
    pub fn repo_path(&self, file_version: &str, classifier: Option<&str>, ext: &str) -> String {
        let mut name = format!("{}-{}", self.artifact, file_version);
        if let Some(c) = classifier {
            name.push('-');
            name.push_str(c);
        }
        name.push('.');
        name.push_str(ext);
        format!("{}/{}/{}/{}", self.group.replace('.', "/"), self.artifact, self.version, name)
    }

    pub fn file_name(&self, classifier: Option<&str>, ext: &str) -> String {
        let mut name = format!("{}-{}", self.artifact, self.version);
        if let Some(c) = classifier {
            name.push('-');
            name.push_str(c);
        }
        format!("{name}.{ext}")
    }
}

impl fmt::Display for Coordinate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.group, self.artifact, self.version)?;
        if let Some(c) = &self.classifier {
            write!(f, ":{c}")?;
        }
        if self.ext != "jar" {
            write!(f, "@{}", self.ext)?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ versions

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Int(u64),
    Str(String),
    /// A nested list started by `-` (Maven's ListItem).
    List(Vec<Item>),
}

/// Maven `ComparableVersion` ordering (also a very good approximation of Gradle's).
#[derive(Debug, Clone)]
pub struct Version {
    raw: String,
    items: Vec<Item>,
}

impl Version {
    pub fn parse(s: &str) -> Version {
        Version { raw: s.to_string(), items: parse_items(&s.to_lowercase()) }
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn is_snapshot(&self) -> bool {
        self.raw.ends_with("-SNAPSHOT")
    }

    /// Not a pre-release (alpha, beta, rc, milestone, snapshot, dev, eap).
    pub fn is_stable(&self) -> bool {
        let l = self.raw.to_lowercase();
        !["alpha", "beta", "rc", "-m", "milestone", "snapshot", "dev", "eap", "preview", "pre"]
            .iter()
            .any(|q| l.contains(q))
    }
}

fn qualifier_rank(s: &str) -> (i32, &str) {
    // Order: alpha < beta < milestone < rc/cr < snapshot < "" (release) < sp < anything else
    match s {
        "alpha" | "a" => (0, ""),
        "beta" | "b" => (1, ""),
        "milestone" | "m" => (2, ""),
        "rc" | "cr" => (3, ""),
        "snapshot" => (4, ""),
        "" | "ga" | "final" | "release" => (5, ""),
        "sp" => (6, ""),
        other => (7, other),
    }
}

fn parse_items(s: &str) -> Vec<Item> {
    // Maven semantics: '.' separates items, while '-' and digit/letter transitions
    // start a nested sub-list.
    let mut segments: Vec<Vec<(String, bool)>> = vec![Vec::new()];
    let mut cur = String::new();
    let mut cur_digit = false;
    fn flush(segments: &mut [Vec<(String, bool)>], cur: &mut String, digit: bool) {
        if !cur.is_empty() {
            segments.last_mut().unwrap().push((std::mem::take(cur), digit));
        }
    }
    for c in s.chars() {
        match c {
            '.' | '_' | '+' => flush(&mut segments, &mut cur, cur_digit),
            '-' => {
                flush(&mut segments, &mut cur, cur_digit);
                segments.push(Vec::new());
            }
            _ => {
                let d = c.is_ascii_digit();
                if !cur.is_empty() && d != cur_digit {
                    flush(&mut segments, &mut cur, cur_digit);
                    segments.push(Vec::new());
                }
                cur_digit = d;
                cur.push(c);
            }
        }
    }
    flush(&mut segments, &mut cur, cur_digit);

    let to_item = |(tok, digit): (String, bool)| {
        if digit {
            Item::Int(tok.parse().unwrap_or(u64::MAX))
        } else {
            Item::Str(match tok.as_str() {
                "a" => "alpha".into(),
                "b" => "beta".into(),
                "m" => "milestone".into(),
                _ => tok,
            })
        }
    };
    let mut acc: Vec<Item> = Vec::new();
    for seg in segments.into_iter().rev() {
        let mut list: Vec<Item> = seg.into_iter().map(to_item).collect();
        if !acc.is_empty() {
            list.push(Item::List(acc));
        }
        normalize(&mut list);
        acc = list;
    }
    acc
}

fn is_null(item: &Item) -> bool {
    match item {
        Item::Int(0) => true,
        Item::Int(_) => false,
        Item::Str(s) => qualifier_rank(s).0 == 5,
        Item::List(l) => l.is_empty(),
    }
}

/// Remove trailing "null" items (0, "", "final", "ga"), looking through sub-lists.
fn normalize(items: &mut Vec<Item>) {
    let mut i = items.len();
    while i > 0 {
        i -= 1;
        if is_null(&items[i]) {
            items.remove(i);
        } else if !matches!(items[i], Item::List(_)) {
            break;
        }
    }
}

fn cmp_item(a: Option<&Item>, b: Option<&Item>) -> Ordering {
    use Item::*;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (Some(Int(x)), None) => {
            if *x == 0 {
                Ordering::Equal
            } else {
                Ordering::Greater
            }
        }
        (None, Some(Int(y))) => {
            if *y == 0 {
                Ordering::Equal
            } else {
                Ordering::Less
            }
        }
        (Some(Str(s)), None) => qualifier_rank(s).cmp(&qualifier_rank("")),
        (None, Some(Str(s))) => qualifier_rank("").cmp(&qualifier_rank(s)),
        (Some(List(l)), None) => cmp_list(l, &[]),
        (None, Some(List(l))) => cmp_list(&[], l),
        (Some(Int(x)), Some(Int(y))) => x.cmp(y),
        (Some(Int(_)), Some(_)) => Ordering::Greater,
        (Some(Str(_)), Some(Int(_))) => Ordering::Less,
        (Some(Str(x)), Some(Str(y))) => qualifier_rank(x).cmp(&qualifier_rank(y)),
        (Some(Str(_)), Some(List(_))) => Ordering::Less,
        (Some(List(_)), Some(Int(_))) => Ordering::Less,
        (Some(List(_)), Some(Str(_))) => Ordering::Greater,
        (Some(List(x)), Some(List(y))) => cmp_list(x, y),
    }
}

fn cmp_list(a: &[Item], b: &[Item]) -> Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let o = cmp_item(a.get(i), b.get(i));
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Version {}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        cmp_list(&self.items, &other.items)
    }
}

/// A version requirement as written in a build file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionReq {
    /// No version given (must come from a BOM / dependency management).
    None,
    /// A plain (soft) version: `1.2.3`.
    Exact(String),
    /// A Maven range such as `[1.0,2.0)` (possibly a union `[1,2),[3,4)`).
    Range(Vec<Range>),
    /// Gradle prefix version: `1.2.+`.
    Prefix(String),
    /// `latest.release` / `latest.integration` / `+`.
    Latest { allow_unstable: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub lower: Option<(String, bool)>,
    pub upper: Option<(String, bool)>,
}

impl VersionReq {
    pub fn parse(s: &str) -> VersionReq {
        let s = s.trim();
        if s.is_empty() {
            return VersionReq::None;
        }
        if s == "+" || s == "latest.integration" {
            return VersionReq::Latest { allow_unstable: true };
        }
        if s == "latest.release" {
            return VersionReq::Latest { allow_unstable: false };
        }
        if let Some(prefix) = s.strip_suffix('+') {
            return VersionReq::Prefix(prefix.to_string());
        }
        if s.starts_with('[') || s.starts_with('(') {
            let mut ranges = Vec::new();
            let mut rest = s;
            while let Some(start) = rest.find(['[', '(']) {
                let Some(end) = rest[start..].find([']', ')']).map(|e| e + start) else { break };
                let body = &rest[start + 1..end];
                let lower_incl = rest.as_bytes()[start] == b'[';
                let upper_incl = rest.as_bytes()[end] == b']';
                let (lo, hi) = match body.split_once(',') {
                    Some((lo, hi)) => (lo.trim(), hi.trim()),
                    None => (body.trim(), body.trim()),
                };
                ranges.push(Range {
                    lower: (!lo.is_empty()).then(|| (lo.to_string(), lower_incl)),
                    upper: (!hi.is_empty()).then(|| (hi.to_string(), upper_incl)),
                });
                rest = &rest[end + 1..];
            }
            if !ranges.is_empty() {
                return VersionReq::Range(ranges);
            }
        }
        VersionReq::Exact(s.to_string())
    }

    /// Whether this requirement needs the list of available versions.
    pub fn is_dynamic(&self) -> bool {
        matches!(self, VersionReq::Range(_) | VersionReq::Prefix(_) | VersionReq::Latest { .. })
    }

    pub fn matches(&self, v: &str) -> bool {
        let ver = Version::parse(v);
        match self {
            VersionReq::None => true,
            VersionReq::Exact(e) => Version::parse(e) == ver,
            VersionReq::Prefix(p) => v.starts_with(p),
            VersionReq::Latest { allow_unstable } => *allow_unstable || ver.is_stable(),
            VersionReq::Range(ranges) => ranges.iter().any(|r| {
                let lo_ok = r.lower.as_ref().is_none_or(|(l, incl)| {
                    let l = Version::parse(l);
                    if *incl { ver >= l } else { ver > l }
                });
                let hi_ok = r.upper.as_ref().is_none_or(|(h, incl)| {
                    let h = Version::parse(h);
                    if *incl { ver <= h } else { ver < h }
                });
                lo_ok && hi_ok
            }),
        }
    }

    /// Pick the best version from `available`.
    pub fn select<'a>(&self, available: impl IntoIterator<Item = &'a str>) -> Option<String> {
        available
            .into_iter()
            .filter(|v| self.matches(v))
            .filter(|v| matches!(self, VersionReq::Latest { allow_unstable: true }) || Version::parse(v).is_stable() || !matches!(self, VersionReq::Prefix(_) | VersionReq::Latest { .. }))
            .max_by(|a, b| Version::parse(a).cmp(&Version::parse(b)))
            .map(str::to_string)
    }

    /// Lower bound used when we cannot list versions (offline): a reasonable guess.
    pub fn fallback(&self) -> Option<String> {
        match self {
            VersionReq::Exact(v) => Some(v.clone()),
            VersionReq::Range(r) => r
                .iter()
                .filter_map(|r| r.upper.as_ref().filter(|(_, incl)| *incl).or(r.lower.as_ref()))
                .map(|(v, _)| v.clone())
                .next(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lt(a: &str, b: &str) {
        assert!(Version::parse(a) < Version::parse(b), "{a} < {b}");
    }

    #[test]
    fn ordering() {
        lt("1.0", "1.1");
        lt("1.9", "1.10");
        lt("1.0-alpha1", "1.0-beta1");
        lt("1.0-beta1", "1.0-rc1");
        lt("1.0-rc1", "1.0-SNAPSHOT");
        lt("1.0-SNAPSHOT", "1.0");
        lt("1.0", "1.0-sp1");
        lt("1.0", "1.0.1");
        lt("2.0.0-Beta2", "2.0.0-RC1");
        lt("2.0.0-RC1", "2.0.0");
        lt("1.7.3", "1.8.0-RC");
        lt("31.1-android", "32.0.0-jre");
        assert_eq!(Version::parse("1.0"), Version::parse("1.0.0"));
        assert_eq!(Version::parse("1.0"), Version::parse("1-0"));
    }

    #[test]
    fn coordinates() {
        let c = Coordinate::parse("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.7.3").unwrap();
        assert_eq!(c.version, "1.7.3");
        let c = Coordinate::parse("g:a:1.0:sources@zip").unwrap();
        assert_eq!(c.classifier.as_deref(), Some("sources"));
        assert_eq!(c.ext, "zip");
        let c = Coordinate::parse("g:a").unwrap();
        assert_eq!(c.version, "");
        assert!(Coordinate::parse("nope").is_none());
        assert_eq!(
            Coordinate::new("org.x", "y", "1").repo_path("1", None, "pom"),
            "org/x/y/1/y-1.pom"
        );
    }

    #[test]
    fn ranges() {
        let r = VersionReq::parse("[1.0,2.0)");
        assert!(r.matches("1.0"));
        assert!(r.matches("1.9.9"));
        assert!(!r.matches("2.0"));
        assert_eq!(r.select(["0.9", "1.0", "1.5", "2.0"]).as_deref(), Some("1.5"));
        let r = VersionReq::parse("[1.5]");
        assert!(r.matches("1.5") && !r.matches("1.6"));
        let p = VersionReq::parse("1.2.+");
        assert_eq!(p.select(["1.2.1", "1.2.10", "1.3.0"]).as_deref(), Some("1.2.10"));
        assert_eq!(VersionReq::parse("latest.release").select(["1.0", "2.0-beta"]).as_deref(), Some("1.0"));
    }
}
