//! Time IDE requests on a real project: a probe file is added next to the first Kotlin file.
//!
//! cargo run --release --example ide_bench -- <dir> [class-fq…]

use std::sync::Arc;

use ktls::analysis::ide::{hierarchy, imports, inlay, semantic, signature};
use ktls::analysis::{AnalysisHost, Language, Workspace};
use ktls::project::{self, LoadOptions};
use ktls::syntax::{TextRange, TextSize};

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: ide_bench <dir>")).canonicalize().unwrap();
    let opts = LoadOptions::new(ktls::util::default_cache_dir());
    let projects: Vec<_> = project::discover(&[dir]).into_iter().map(|(r, bs)| project::load(&r, bs, &opts)).collect();
    let mut host = AnalysisHost::default();
    host.set_workspace(Workspace::new(projects, project::jdk::find(None, None)));
    host.workspace().warm_classpaths();
    host.load_sources_from_disk();
    let snap = host.snapshot();
    let mut kotlin: Vec<_> = snap.files.values().filter(|f| f.language == Language::Kotlin && !f.path.to_string_lossy().contains("/test/")).map(|f| f.path.clone()).collect();
    kotlin.sort();
    // A main source file of a module with coroutines on the classpath, if any.
    let pick = kotlin.iter().find(|p| snap.file(p).is_some_and(|f| f.text.contains("kotlinx.coroutines"))).unwrap_or(&kotlin[0]);
    let probe = pick.with_file_name("ZzProbe.kt");
    let text = "package zz.probe\n\nfun probe(xs: List<String>) {\n    val d = Duration.ZERO\n    runBlocking { }\n    xs.joinToString(\", \", )\n    println(xs.indexOf())\n}\n";
    host.set_file(probe.clone(), Arc::from(text), Some(1));
    let snap = host.snapshot();
    println!("probe {} {:?}", probe.display(), ktls::analysis::diagnostics::coverage(&snap, &probe));
    for (label, needle) in [("Duration", "Duration"), ("runBlocking", "runBlocking")] {
        let at = TextSize::from(text.find(needle).unwrap() as u32 + 1);
        let t = std::time::Instant::now();
        let fixes = imports::import_fixes(&snap, &probe, TextRange::empty(at));
        println!("import {label}: {:?} {:?}", t.elapsed(), fixes.iter().map(|f| &f.fq).collect::<Vec<_>>());
    }
    for needle in ["\", \", ", "indexOf("] {
        let at = TextSize::from((text.find(needle).unwrap() + needle.len()) as u32);
        let t = std::time::Instant::now();
        let h = signature::signature_help(&snap, &probe, at);
        println!("signature: {:?} {:?}", t.elapsed(), h.map(|h| h.signatures.into_iter().map(|s| s.label).collect::<Vec<_>>()));
    }
    // Completion of a lowercase prefix (also searches library top-level functions) and
    // after a dot (unimported extensions); the first builds the library name index.
    for (label, needle) in [("runBl", "runBlocking"), ("runBl again", "runBlocking"), ("xs.joinTo", "joinToString")] {
        let at = TextSize::from((text.find(needle).unwrap() + 5) as u32);
        let t = std::time::Instant::now();
        let c = ktls::analysis::ide::completion::complete(&snap, &probe, at, ktls::analysis::ide::completion::Options { snippets: true });
        println!("complete {label}: {:?} {} items", t.elapsed(), c.map_or(0, |c| c.items.len()));
    }
    for fq in std::env::args().skip(2) {
        let t = std::time::Instant::now();
        let found = hierarchy::inheritors(&snap, &fq, &|| false);
        println!("inheritors of {fq}: {} in {:?}", found.len(), t.elapsed());
    }
    let t = std::time::Instant::now();
    let mut labels: std::collections::BTreeMap<String, usize> = Default::default();
    let mut slowest = (std::time::Duration::ZERO, None);
    for p in &kotlin {
        let f = snap.file(p).unwrap();
        let t1 = std::time::Instant::now();
        for h in inlay::inlay_hints(&snap, p, TextRange::up_to(TextSize::of(&*f.text)), &inlay::InlayConfig::default()) {
            if std::env::var("SHOW_LABEL").is_ok_and(|l| l == h.label) {
                let start = usize::from(h.offset);
                let line_start = f.text[..start].rfind('\n').map_or(0, |i| i + 1);
                let line_end = f.text[start..].find('\n').map_or(f.text.len(), |i| start + i);
                println!("{}: {}⟨{}⟩{}", p.display(), f.text[line_start..start].trim_start(), h.label, &f.text[start..line_end]);
            }
            *labels.entry(h.label).or_default() += 1;
        }
        if t1.elapsed() > slowest.0 {
            slowest = (t1.elapsed(), Some(p.clone()));
        }
    }
    println!("inlay hints for {} files in {:?} (slowest {:?} {:?}); {} distinct labels", kotlin.len(), t.elapsed(), slowest.0, slowest.1, labels.len());
    let mut top: Vec<_> = labels.into_iter().collect();
    top.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (l, c) in top.iter().take(40) {
        println!("  {c:5} {l}");
    }
    let t = std::time::Instant::now();
    let (mut total, mut idents, mut slowest) = (0, 0, (std::time::Duration::ZERO, None));
    for p in &kotlin {
        let t1 = std::time::Instant::now();
        let toks = semantic::semantic_tokens(&snap, p, None);
        if t1.elapsed() > slowest.0 {
            slowest = (t1.elapsed(), Some(p.clone()));
        }
        total += toks.len();
        let f = snap.file(p).unwrap();
        let root = f.parse().syntax();
        idents += root.descendants_with_tokens().filter(|t| t.kind() == ktls::syntax::SyntaxKind::IDENT).count();
    }
    println!("semantic tokens for {} files in {:?} (slowest {:?} {:?}); {total} tokens, {idents} identifiers", kotlin.len(), t.elapsed(), slowest.0, slowest.1);
    for p in kotlin.iter().take(200) {
        let t = std::time::Instant::now();
        if let Some((range, new)) = imports::organize_imports(&snap, p) {
            let old = &snap.file(p).unwrap().text[usize::from(range.start())..usize::from(range.end())];
            println!("organize {}: {:?}\n--- old\n{old}\n--- new\n{new}", p.display(), t.elapsed());
        }
    }
}
