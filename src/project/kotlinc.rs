//! A Kotlin compiler installation (`kotlinc`): the standard library for folders without a
//! build file, which are compiled with it directly.

use std::path::{Path, PathBuf};

/// The installation's `kotlin-stdlib.jar`: of `explicit` (a Kotlin home) when given,
/// otherwise of `KOTLIN_HOME`, `kotlinc` on `PATH`, or SDKMAN's current Kotlin.
pub fn stdlib(explicit: Option<&Path>) -> Option<PathBuf> {
    match explicit {
        Some(home) => stdlib_of(home),
        None => candidates().iter().find_map(|h| stdlib_of(h)),
    }
}

/// `lib/kotlin-stdlib.jar` of a Kotlin home (Homebrew keeps it under `libexec`).
fn stdlib_of(home: &Path) -> Option<PathBuf> {
    [home.join("lib/kotlin-stdlib.jar"), home.join("libexec/lib/kotlin-stdlib.jar")].into_iter().find(|j| j.is_file())
}

/// Candidate Kotlin homes, most preferred first.
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = std::env::var_os("KOTLIN_HOME") {
        out.push(PathBuf::from(h));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            // `<home>/bin/kotlinc`, usually reached through a symlink.
            if let Ok(real) = dir.join("kotlinc").canonicalize()
                && let Some(home) = real.parent().and_then(Path::parent)
            {
                out.push(home.to_path_buf());
            }
        }
    }
    if let Some(h) = dirs::home_dir() {
        out.push(h.join(".sdkman/candidates/kotlin/current"));
    }
    out
}

/// Jars of a folder without a build file, passed to `kotlinc -cp` by convention: the
/// `*.jar` files directly in `lib/` and `libs/`.
pub fn local_jars(root: &Path) -> Vec<PathBuf> {
    let mut jars: Vec<PathBuf> = ["lib", "libs"]
        .iter()
        .filter_map(|d| std::fs::read_dir(root.join(d)).ok())
        .flat_map(|entries| entries.flatten().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "jar"))
        .collect();
    jars.sort();
    jars
}
