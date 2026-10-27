//! Import a project, index each module's classpath and decode every class.
//!
//! cargo run --release --example index_classpath -- <dir> [fq.name.to.show ...]

use ktls::classpath::ClasspathIndex;
use ktls::project::{self, LoadOptions, SourceKind};
use ktls::symbols::SymbolKind;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::path::PathBuf::from(&args[0]).canonicalize().unwrap();
    let show: Vec<&String> = args.iter().skip(1).collect();
    let opts = LoadOptions::new(ktls::util::default_cache_dir());
    let jdk = project::jdk::find(None, None);
    println!("JDK: {:?}", jdk.as_ref().map(|j| (&j.home, &j.version)));
    for (root, bs) in project::discover(&[dir]) {
        let p = project::load(&root, bs, &opts);
        for m in &p.modules {
            let cp = m.classpath(SourceKind::Test);
            let t = std::time::Instant::now();
            let idx = ClasspathIndex::build(&cp, jdk.as_ref());
            let build_time = t.elapsed();
            let t = std::time::Instant::now();
            let names: Vec<String> = idx.all_classes().collect();
            let mut classes = 0;
            let mut kotlin = 0;
            let mut members = 0;
            let mut errors = 0;
            let mut error_types = 0;
            let mut packages = std::collections::BTreeSet::new();
            for fq in &names {
                match idx.find_class(fq) {
                    Some(c) => {
                        classes += 1;
                        if c.is_kotlin {
                            kotlin += 1;
                        }
                        members += c.members.len();
                        error_types += c.members.iter().filter(|m| m.ty.as_ref().is_some_and(|t| t.display().contains("<error>"))).count();
                        packages.insert(c.symbol.package().to_string());
                    }
                    None => {
                        // facades are expected to be filtered out
                        if !fq.ends_with("Kt") && !fq.contains("Kt__") {
                            errors += 1;
                            if std::env::var_os("SHOW_UNREADABLE").is_some() {
                                println!("  unreadable: {fq}");
                            }
                        }
                    }
                }
            }
            let mut top_level = 0;
            for p in &packages {
                top_level += idx.package_members(p).len();
            }
            println!(
                "{}: {} jars, index {:?}; decoded {} classes ({} Kotlin), {} members, {} top-level decls in {:?}; {} unreadable, {} <error> types",
                m.name,
                cp.len(),
                build_time,
                classes,
                kotlin,
                members,
                top_level,
                t.elapsed(),
                errors,
                error_types
            );
            // Signature plus deprecation and annotations.
            let describe = |s: &ktls::symbols::Symbol| {
                use ktls::symbols::Modifiers;
                let mut d = s.signature();
                if s.modifiers.has(Modifiers::DEPRECATED) {
                    d.push_str(if s.modifiers.has(Modifiers::DEPRECATED_ERROR) { "  [deprecated: error]" } else { "  [deprecated]" });
                }
                if s.modifiers.has(Modifiers::LOW_PRIORITY) {
                    d.push_str("  [low priority]");
                }
                if s.modifiers.has(Modifiers::HIDDEN) {
                    d.push_str("  [hidden]");
                }
                if !s.annotations.is_empty() {
                    d.push_str(&format!("  @{}", s.annotations.join(" @")));
                }
                d
            };
            for fq in &show {
                if let Some(c) = idx.find_class(fq) {
                    println!("  {}", describe(&c.symbol));
                    for mem in c.members.iter().take(40) {
                        println!("    {}", describe(mem));
                    }
                }
                // Top-level declarations with the same name (factory functions...).
                let (pkg, name) = fq.rsplit_once('.').unwrap();
                for s in idx.lookup(pkg, name).1.iter().filter(|s| s.kind != SymbolKind::Package).take(8) {
                    println!("  {}", describe(s));
                }
            }
        }
    }
}
