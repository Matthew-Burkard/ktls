//! Parse every `.kt`/`.kts` file in a directory or source jar and report syntax errors.
//!
//! cargo run --release --example parse_corpus -- <dir-or-jar> [max-reported]

use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: parse_corpus <dir-or-jar> [max-reported]");
    let max: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(20);
    let mut files: Vec<(String, String)> = Vec::new();
    if path.ends_with(".jar") || path.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).unwrap();
            let Ok(name) = entry.name().map(|n| n.to_string()) else { continue };
            if name.ends_with(".kt") || name.ends_with(".kts") {
                let mut text = String::new();
                if entry.read_to_string(&mut text).is_ok() {
                    files.push((name, text));
                }
            }
        }
    } else {
        for e in walkdir::WalkDir::new(&path).into_iter().filter_map(Result::ok) {
            let name = e.path().display().to_string();
            if (name.ends_with(".kt") || name.ends_with(".kts"))
                && let Ok(text) = std::fs::read_to_string(e.path()) {
                    files.push((name, text));
                }
        }
    }
    let start = std::time::Instant::now();
    let mut bad = 0;
    let mut bytes = 0;
    let mut reported = 0;
    for (name, text) in &files {
        bytes += text.len();
        let parse = kotlin_lsp::syntax::parse(text, name.ends_with(".kts"));
        assert_eq!(parse.syntax().text().to_string(), *text, "not lossless: {name}");
        if !parse.errors().is_empty() {
            bad += 1;
            if reported < max {
                reported += 1;
                let e = &parse.errors()[0];
                let off: usize = e.range.start().into();
                let line = text[..off].matches('\n').count() + 1;
                let ls = text[..off].rfind('\n').map_or(0, |i| i + 1);
                let le = text[off..].find('\n').map_or(text.len(), |i| off + i);
                println!("{name}:{line}: {} ({} errors)\n    {}", e.message, parse.errors().len(), text[ls..le].trim());
            }
        }
    }
    println!(
        "{} files, {} with errors, {:.1} MB in {:?}",
        files.len(),
        bad,
        bytes as f64 / 1e6,
        start.elapsed()
    );
}
