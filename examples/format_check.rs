//! Format every Kotlin file under a directory (without writing) and check that only
//! whitespace changes, that formatting is idempotent, and how many lines would change.
//!
//! cargo run --release --example format_check -- <dir> [show-diffs] [indent-width]

use kotlin_lsp::analysis::ide::format::format;
use kotlin_lsp::syntax::parse;

fn apply(text: &str, edits: &[(kotlin_lsp::syntax::TextRange, String)]) -> String {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start()));
    let mut out = text.to_string();
    for (r, t) in edits {
        out.replace_range(usize::from(r.start())..usize::from(r.end()), &t);
    }
    out
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: format_check <dir>");
    let show: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    // Indent width (default 4).
    let unit = " ".repeat(std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(4));
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(&dir)];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                if !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.') || n == "build") {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|e| e == "kt") {
                files.push(p);
            }
        }
    }
    files.sort();
    let (mut lines, mut changed, mut bad, mut unstable, mut shown) = (0, 0, 0, 0, 0);
    let mut worst: Vec<(usize, std::path::PathBuf)> = Vec::new();
    let t = std::time::Instant::now();
    for p in &files {
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        let root = parse(&text, false).syntax();
        let out = apply(&text, &format(&root, &text, &unit, None));
        let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        if strip(&out) != strip(&text) {
            bad += 1;
            println!("NON-WHITESPACE CHANGE: {}", p.display());
        }
        let again = apply(&out, &format(&parse(&out, false).syntax(), &out, &unit, None));
        if again != out {
            unstable += 1;
            println!("NOT IDEMPOTENT: {}", p.display());
            for (i, (x, y)) in out.lines().zip(again.lines()).enumerate().filter(|(_, (x, y))| x != y).take(3) {
                println!("{:5} 1st {x:?}\n{:5} 2nd {y:?}", i + 1, i + 1);
            }
        }
        let (a, b): (Vec<&str>, Vec<&str>) = (text.lines().collect(), out.lines().collect());
        let n = a.iter().zip(&b).filter(|(x, y)| x.trim_end() != y.trim_end()).count();
        lines += a.len();
        changed += n;
        worst.push((n, p.clone()));
        if n > 0 && shown < show {
            shown += 1;
            println!("--- {}", p.display());
            for (i, (x, y)) in a.iter().zip(&b).enumerate().filter(|(_, (x, y))| x.trim_end() != y.trim_end()).take(6) {
                println!("{:5} - {x}\n{:5} + {y}", i + 1, i + 1);
            }
        }
    }
    worst.sort_by_key(|w| std::cmp::Reverse(w.0));
    println!("{} files, {lines} lines, {changed} lines re-indented, {bad} with non-whitespace changes, {unstable} not idempotent, in {:?}", files.len(), t.elapsed());
    for (n, p) in worst.iter().take(5) {
        println!("  {n:5} {}", p.display());
    }
}
