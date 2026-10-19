//! JDK discovery.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JdkClasses {
    /// `lib/modules` jimage (JDK 9+).
    JImage(PathBuf),
    /// `jmods/*.jmod`.
    Jmods(PathBuf),
    /// `jre/lib/rt.jar` (JDK 8).
    RtJar(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jdk {
    pub home: PathBuf,
    /// `JAVA_VERSION` from the `release` file, e.g. `17.0.2`.
    pub version: String,
    pub classes: JdkClasses,
    pub sources: Option<PathBuf>,
}

impl Jdk {
    pub fn major(&self) -> u32 {
        let v = self.version.strip_prefix("1.").unwrap_or(&self.version);
        v.split(|c: char| !c.is_ascii_digit()).next().and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    pub fn from_home(home: &Path) -> Option<Jdk> {
        let home = home.canonicalize().ok()?;
        let release = std::fs::read_to_string(home.join("release")).unwrap_or_default();
        let version = release
            .lines()
            .find_map(|l| l.strip_prefix("JAVA_VERSION="))
            .map(|v| v.trim_matches('"').to_string())
            .unwrap_or_default();
        let classes = if home.join("lib/modules").is_file() {
            JdkClasses::JImage(home.join("lib/modules"))
        } else if home.join("jmods").is_dir() {
            JdkClasses::Jmods(home.join("jmods"))
        } else if home.join("jre/lib/rt.jar").is_file() {
            JdkClasses::RtJar(home.join("jre/lib/rt.jar"))
        } else if home.join("lib/rt.jar").is_file() {
            JdkClasses::RtJar(home.join("lib/rt.jar"))
        } else {
            return None;
        };
        let sources = [home.join("lib/src.zip"), home.join("src.zip")].into_iter().find(|p| p.is_file());
        Some(Jdk { home, version, classes, sources })
    }
}

/// Candidate JDK homes, most preferred first.
fn candidates(explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = explicit {
        out.push(p.to_path_buf());
    }
    if let Some(h) = std::env::var_os("JAVA_HOME") {
        out.push(PathBuf::from(h));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let java = dir.join("java");
            if java.is_file()
                && let Ok(real) = java.canonicalize()
                    && let Some(home) = real.parent().and_then(|bin| bin.parent()) {
                        out.push(home.to_path_buf());
                    }
        }
    }
    let home = dirs::home_dir();
    let mut roots: Vec<PathBuf> = vec![PathBuf::from("/usr/lib/jvm"), PathBuf::from("/Library/Java/JavaVirtualMachines")];
    if let Some(h) = &home {
        roots.push(h.join(".sdkman/candidates/java"));
        roots.push(h.join(".gradle/jdks"));
        roots.push(h.join(".jdks"));
    }
    for r in roots {
        if let Ok(entries) = std::fs::read_dir(&r) {
            let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            dirs.sort();
            dirs.reverse();
            for d in dirs {
                out.push(d.join("Contents/Home"));
                out.push(d);
            }
        }
    }
    out
}

/// Find a JDK, preferring `explicit`, then one matching `wanted_major`.
pub fn find(explicit: Option<&Path>, wanted_major: Option<u32>) -> Option<Jdk> {
    let mut first = None;
    for c in candidates(explicit) {
        if let Some(jdk) = Jdk::from_home(&c) {
            if explicit.is_some_and(|e| e == c) {
                return Some(jdk);
            }
            if wanted_major.is_none_or(|w| jdk.major() == w) {
                return Some(jdk);
            }
            first.get_or_insert(jdk);
        }
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_versions() {
        let mk = |v: &str| Jdk { home: PathBuf::new(), version: v.into(), classes: JdkClasses::RtJar(PathBuf::new()), sources: None };
        assert_eq!(mk("1.8.0_292").major(), 8);
        assert_eq!(mk("17.0.2").major(), 17);
        assert_eq!(mk("25").major(), 25);
    }

    #[test]
    fn finds_some_jdk_on_this_machine() {
        // Not an assertion on CI machines without Java; just exercise the code.
        if let Some(jdk) = find(None, None) {
            assert!(jdk.major() >= 8);
        }
    }
}
