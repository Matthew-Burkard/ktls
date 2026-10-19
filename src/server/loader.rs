//! Loads the workspace model on a background thread.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::Sender;
use rayon::prelude::*;

use super::Task;
use super::config::Config;
use crate::analysis::{FileData, Language, Workspace};
use crate::project::{self, Project};

pub struct Loaded {
    pub generation: u64,
    pub workspace: Workspace,
    /// Source files read from disk (summaries already computed).
    pub files: Vec<Arc<FileData>>,
    /// Human readable notes about problems (incomplete modules etc).
    pub notes: Vec<String>,
}

pub enum ProgressEvent {
    Begin(String),
    Report(String),
    End(String),
}

/// Load everything; `force` ignores cached models.
pub fn load(folders: Vec<PathBuf>, config: Config, generation: u64, force: bool, tasks: Sender<Task>) {
    let progress_tasks = tasks.clone();
    let report = move |msg: &str| {
        let _ = progress_tasks.send(Task::Progress(ProgressEvent::Report(msg.to_string())));
    };
    let _ = tasks.send(Task::Progress(ProgressEvent::Begin("Importing Kotlin project".into())));
    let start = std::time::Instant::now();

    let roots = project::discover(&folders);
    log::info!("discovered {} project root(s): {:?}", roots.len(), roots);
    let progress: project::repo::ProgressFn = Arc::new(report.clone());
    let opts = config.load_options(Some(progress));
    let mut projects: Vec<Project> = Vec::new();
    for (root, bs) in &roots {
        if !force
            && let Some(p) = project::load_cached(root, &opts) {
                log::info!("using cached model for {}", root.display());
                projects.push(p);
                continue;
            }
        report(&format!("Reading {}", root.display()));
        projects.push(project::load(root, *bs, &opts));
    }

    let wanted_java = projects
        .iter()
        .flat_map(|p| p.modules.iter())
        .filter_map(|m| m.java_release.as_deref())
        .filter_map(|v| v.trim_start_matches("1.").parse::<u32>().ok())
        .max();
    let jdk = project::jdk::find(config.jdk_home.as_deref(), wanted_java);
    let mut notes = Vec::new();
    match &jdk {
        Some(j) => log::info!("using JDK {} at {}", j.version, j.home.display()),
        None => notes.push("No JDK found: Java library classes are unavailable (set kotlin.jdkHome)".to_string()),
    }
    for p in &projects {
        for prob in &p.problems {
            notes.push(format!("{}: {prob}", p.root.display()));
        }
        for m in &p.modules {
            if !m.complete && !m.source_roots.is_empty() {
                let first = m.problems.first().cloned().unwrap_or_default();
                notes.push(format!("module {}: {}{}", m.name, first, if m.problems.len() > 1 { format!(" (+{} more)", m.problems.len() - 1) } else { String::new() }));
            }
        }
    }

    report("Indexing libraries");
    let workspace = Workspace::new(projects, jdk).with_cache(config.cache_dir(), config.project.offline);
    workspace.warm_classpaths();

    report("Indexing sources");
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    for (root, _) in workspace.source_roots() {
        for e in walkdir::WalkDir::new(root).into_iter().flatten() {
            let p = e.path();
            if e.file_type().is_file() && Language::of(p).is_some() && seen.insert(p.to_path_buf()) {
                paths.push(p.to_path_buf());
            }
        }
    }
    let files: Vec<Arc<FileData>> = paths
        .par_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            let fd = Arc::new(FileData::new(p.clone(), text.into(), None)?);
            fd.summary();
            Some(fd)
        })
        .collect();
    let modules = workspace.modules().count();
    let msg = format!("Indexed {} modules and {} source files in {:.1?}", modules, files.len(), start.elapsed());
    log::info!("{msg}");
    let _ = tasks.send(Task::Loaded(Box::new(Loaded { generation, workspace, files, notes })));
    let _ = tasks.send(Task::Progress(ProgressEvent::End(msg)));
}
