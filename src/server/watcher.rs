//! Server-side file watching, for clients that do not watch files for the server (Neovim
//! on Linux does not offer `workspace/didChangeWatchedFiles`): changes made by other
//! programs (`git checkout`, formatters, code generators) reach the analysis.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use lsp_types::FileChangeType;
use notify::{EventKind, RecursiveMode, Watcher as _};

use super::Task;

pub struct Watcher {
    inner: notify::RecommendedWatcher,
    /// Watched paths and whether recursively.
    watched: HashSet<(PathBuf, bool)>,
}

impl Watcher {
    /// Start a watcher that reports changes of Kotlin, Java and build files as
    /// [`Task::FilesChanged`].
    pub fn start(tasks: Sender<Task>) -> Option<Watcher> {
        let handler = move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            let changes = changes(&event);
            if !changes.is_empty() {
                let _ = tasks.send(Task::FilesChanged(changes));
            }
        };
        match notify::recommended_watcher(handler) {
            Ok(inner) => Some(Watcher { inner, watched: HashSet::new() }),
            Err(e) => {
                log::warn!("file watcher unavailable: {e}");
                None
            }
        }
    }

    /// Watch exactly these directories: `recursive` ones with their subdirectories (source
    /// roots), the others alone (directories of build files).
    pub fn watch(&mut self, recursive: &[PathBuf], flat: &[PathBuf]) {
        let wanted: HashSet<(PathBuf, bool)> = recursive.iter().map(|p| (p.clone(), true)).chain(flat.iter().map(|p| (p.clone(), false))).filter(|(p, _)| p.is_dir()).collect();
        for (path, _) in self.watched.difference(&wanted) {
            let _ = self.inner.unwatch(path);
        }
        for (path, rec) in wanted.difference(&self.watched) {
            let mode = if *rec { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
            if let Err(e) = self.inner.watch(path, mode) {
                log::warn!("cannot watch {}: {e}", path.display());
            }
        }
        self.watched = wanted;
    }
}

/// The relevant files of an event and how they changed.
fn changes(event: &notify::Event) -> Vec<(PathBuf, FileChangeType)> {
    let kind = match event.kind {
        EventKind::Create(_) => FileChangeType::CREATED,
        EventKind::Modify(_) => FileChangeType::CHANGED,
        EventKind::Remove(_) => FileChangeType::DELETED,
        _ => return Vec::new(),
    };
    event
        .paths
        .iter()
        .filter(|p| relevant(p))
        // Renames report both names: the old one no longer exists.
        .map(|p| (p.clone(), if kind != FileChangeType::DELETED && !p.exists() { FileChangeType::DELETED } else { kind }))
        .collect()
}

fn relevant(path: &Path) -> bool {
    super::is_build_file(path) || crate::analysis::Language::of(path).is_some()
}
