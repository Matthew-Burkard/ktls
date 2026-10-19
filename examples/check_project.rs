//! Import a project, load all sources and run diagnostics on every Kotlin file.
//!
//! cargo run --release --example check_project -- <dir> [max-shown] [code]

use std::collections::BTreeMap;

use kotlin_lsp::analysis::diagnostics::{Coverage, file_diagnostics};
use kotlin_lsp::analysis::{AnalysisHost, Language, Workspace};
use kotlin_lsp::project::{self, LoadOptions};
use rayon::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::path::PathBuf::from(&args[0]).canonicalize().unwrap();
    let max: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    // Only show diagnostics with this code.
    let only = args.get(2).cloned();
    let t = std::time::Instant::now();
    let opts = LoadOptions::new(kotlin_lsp::util::default_cache_dir());
    let projects: Vec<_> = project::discover(&[dir]).into_iter().map(|(r, bs)| project::load(&r, bs, &opts)).collect();
    for p in &projects {
        for m in &p.modules {
            if !m.complete {
                println!("incomplete module {}: {:?}", m.name, m.problems.iter().take(3).collect::<Vec<_>>());
            }
        }
    }
    let jdk = project::jdk::find(None, None);
    let mut host = AnalysisHost::default();
    host.set_workspace(Workspace::new(projects, jdk));
    host.workspace().warm_classpaths();
    let n = host.load_sources_from_disk();
    let snap = host.snapshot();
    println!("loaded {} modules, {n} files, indexed in {:?}", snap.workspace.modules().count(), t.elapsed());
    let t = std::time::Instant::now();
    let files: Vec<_> = snap.files.values().filter(|f| f.language == Language::Kotlin).cloned().collect();
    let results: Vec<_> = files.par_iter().map(|f| (f.clone(), file_diagnostics(&snap, &f.path))).collect();
    let mut by_code: BTreeMap<&str, usize> = BTreeMap::new();
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    let mut full = 0;
    let mut shown = 0;
    for (f, (diags, cov)) in &results {
        if *cov == Coverage::Full {
            full += 1;
        }
        for d in diags {
            *by_code.entry(d.code).or_default() += 1;
            let text = &f.text[usize::from(d.range.start())..usize::from(d.range.end())];
            *names.entry(format!("{} {text}", d.code)).or_default() += 1;
            if shown < max && only.as_deref().is_none_or(|c| c == d.code) {
                shown += 1;
                let line = f.text[..usize::from(d.range.start())].matches('\n').count() + 1;
                println!("{}:{line}: {} [{}]", f.path.display(), d.message, d.code);
            }
        }
    }
    println!("{} Kotlin files ({full} fully checked) in {:?}: {by_code:?}", files.len(), t.elapsed());
    let mut top: Vec<_> = names.into_iter().collect();
    top.sort_by_key(|a| std::cmp::Reverse(a.1));
    for (n, c) in top.into_iter().take(25) {
        println!("  {c:5} {n}");
    }
}
