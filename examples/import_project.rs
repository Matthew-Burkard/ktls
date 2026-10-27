//! Import a Gradle/Maven project and print the resulting model.
//!
//! cargo run --release --example import_project -- <dir> [--tooling] [--offline] [--deps]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::path::PathBuf::from(args.iter().find(|a| !a.starts_with("--")).expect("usage: import_project <dir>"));
    let dir = dir.canonicalize().unwrap();
    let mut opts = ktls::project::LoadOptions::new(ktls::util::default_cache_dir());
    opts.offline = args.iter().any(|a| a == "--offline");
    opts.allow_tooling = args.iter().any(|a| a == "--tooling");
    opts.gradle_mode = if opts.allow_tooling { ktls::project::GradleMode::Tooling } else { ktls::project::GradleMode::Static };
    opts.progress = Some(std::sync::Arc::new(|m: &str| eprintln!("  .. {m}")));
    let show_deps = args.iter().any(|a| a == "--deps");

    let start = std::time::Instant::now();
    for (root, bs) in ktls::project::discover(&[dir]) {
        let p = ktls::project::load(&root, bs, &opts);
        println!("project {} ({:?}, {:?}) in {:?}", p.root.display(), p.build_system, p.source, start.elapsed());
        for prob in &p.problems {
            println!("  ! {prob}");
        }
        for m in &p.modules {
            println!(
                "  module {} complete={} kotlin={:?} deps={} main_cp={} test_cp={}",
                m.name,
                m.complete,
                m.kotlin,
                m.dependencies.len(),
                m.classpath(ktls::project::SourceKind::Main).len(),
                m.classpath(ktls::project::SourceKind::Test).len()
            );
            for r in &m.source_roots {
                println!("    src {:?} {} {}", r.kind, r.source_set, r.path.display());
            }
            for d in &m.module_dependencies {
                println!("    -> {} ({:?})", d.module, d.scope);
            }
            if show_deps {
                let mut deps: Vec<_> = m.dependencies.iter().collect();
                deps.sort_by(|a, b| a.coord.cmp(&b.coord));
                for d in deps {
                    println!("    dep {} {:?} {}", d.coord, d.scope, if d.file.is_some() { "" } else { "(no file)" });
                }
            }
            for prob in &m.problems {
                println!("    ! {prob}");
            }
        }
    }
}
