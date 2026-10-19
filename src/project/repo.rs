//! Artifact repositories: local caches (Maven `~/.m2`, Gradle `files-2.1`), our own
//! download cache, and remote Maven repositories.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::coords::{Coordinate, ModuleId, Version};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    /// Base URL, always ending in `/`. `file:` URLs are supported.
    pub url: String,
}

impl Repository {
    pub fn new(id: impl Into<String>, url: &str) -> Self {
        let mut url = url.trim().to_string();
        if !url.ends_with('/') {
            url.push('/');
        }
        Repository { id: id.into(), url }
    }

    pub fn maven_central() -> Self {
        Repository::new("central", "https://repo.maven.apache.org/maven2/")
    }

    pub fn google() -> Self {
        Repository::new("google", "https://dl.google.com/dl/android/maven2/")
    }

    pub fn gradle_plugin_portal() -> Self {
        Repository::new("gradlePluginPortal", "https://plugins.gradle.org/m2/")
    }

    fn local_dir(&self) -> Option<PathBuf> {
        self.url.strip_prefix("file://").or_else(|| self.url.strip_prefix("file:")).map(|p| {
            PathBuf::from(super::super::util::percent_decode(p))
        })
    }
}

/// Configuration of artifact lookup.
#[derive(Clone, Debug)]
pub struct RepoConfig {
    /// Directory where downloaded artifacts are stored (Maven layout).
    pub cache_dir: PathBuf,
    pub maven_local: Option<PathBuf>,
    pub gradle_user_home: Option<PathBuf>,
    pub offline: bool,
    /// `mirrorOf` → URL from `~/.m2/settings.xml`.
    pub mirrors: Vec<(String, String)>,
    /// How long a failed remote lookup is remembered.
    pub negative_ttl: Duration,
}

impl RepoConfig {
    pub fn from_environment(cache_dir: PathBuf, offline: bool) -> RepoConfig {
        let home = dirs::home_dir();
        let m2 = home.as_ref().map(|h| h.join(".m2"));
        let settings = m2.as_ref().map(|m| m.join("settings.xml")).and_then(|p| std::fs::read_to_string(p).ok());
        let (mut maven_local, mirrors) = settings.as_deref().map(parse_maven_settings).unwrap_or_default();
        if maven_local.is_none() {
            maven_local = m2.map(|m| m.join("repository"));
        }
        let gradle_user_home = std::env::var_os("GRADLE_USER_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".gradle")));
        RepoConfig { cache_dir, maven_local, gradle_user_home, offline, mirrors, negative_ttl: Duration::from_secs(6 * 3600) }
    }
}

/// Extract `<localRepository>` and `<mirrors>` from a Maven settings.xml.
fn parse_maven_settings(xml: &str) -> (Option<PathBuf>, Vec<(String, String)>) {
    let tree = super::xml::parse(xml);
    let Some(root) = tree else { return (None, Vec::new()) };
    let local = root.child_text("localRepository").map(|s| {
        let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
        PathBuf::from(s.replace("${user.home}", &home))
    });
    let mirrors = root
        .child("mirrors")
        .map(|m| {
            m.children_named("mirror")
                .filter_map(|m| Some((m.child_text("mirrorOf")?.to_string(), m.child_text("url")?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    (local, mirrors)
}

pub type ProgressFn = Arc<dyn Fn(&str) + Send + Sync>;

pub struct RepoSystem {
    pub config: RepoConfig,
    agent: ureq::Agent,
    memo: Mutex<HashMap<String, Option<PathBuf>>>,
    /// One lock per path being looked up, so concurrent resolutions never download twice.
    inflight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    versions_memo: Mutex<HashMap<(ModuleId, Vec<Repository>), Vec<String>>>,
    downloads: AtomicUsize,
    progress: Option<ProgressFn>,
}

impl RepoSystem {
    pub fn new(config: RepoConfig, progress: Option<ProgressFn>) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .timeout_connect(Some(Duration::from_secs(10)))
            .http_status_as_error(true)
            .user_agent("kotlin-lsp-rust/0.1")
            .build()
            .into();
        RepoSystem {
            config,
            agent,
            memo: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            versions_memo: Mutex::new(HashMap::new()),
            downloads: AtomicUsize::new(0),
            progress,
        }
    }

    pub fn download_count(&self) -> usize {
        self.downloads.load(Ordering::Relaxed)
    }

    fn effective_url(&self, repo: &Repository) -> String {
        for (mirror_of, url) in &self.config.mirrors {
            let applies = mirror_of == "*"
                || mirror_of.split(',').any(|m| m.trim() == repo.id)
                || (mirror_of.starts_with("external:") && !repo.url.contains("localhost") && !repo.url.starts_with("file:"));
            if applies {
                let mut u = url.clone();
                if !u.ends_with('/') {
                    u.push('/');
                }
                return u;
            }
        }
        repo.url.clone()
    }

    /// Locate (downloading if necessary) a file of `coord`.
    pub fn artifact(&self, coord: &Coordinate, classifier: Option<&str>, ext: &str, repos: &[Repository]) -> Option<PathBuf> {
        let file_version = if coord.version.ends_with("-SNAPSHOT") {
            self.snapshot_file_version(coord, repos).unwrap_or_else(|| coord.version.clone())
        } else {
            coord.version.clone()
        };
        let rel = coord.repo_path(&file_version, classifier, ext);
        let file_name = rel.rsplit('/').next().unwrap().to_string();
        // A SNAPSHOT may be stored locally under its non-timestamped name.
        let local_name = coord.file_name(classifier, ext);
        self.file(&rel, Some((coord, &[file_name, local_name])), repos)
    }

    /// Fetch a file by its path relative to the repository root.
    pub fn file(&self, rel: &str, gradle: Option<(&Coordinate, &[String])>, repos: &[Repository]) -> Option<PathBuf> {
        let memo_key = format!("{rel}|{}", repos.iter().map(|r| r.url.as_str()).collect::<Vec<_>>().join(","));
        if let Some(hit) = self.memo.lock().get(&memo_key) {
            return hit.clone();
        }
        let lock = self.inflight.lock().entry(memo_key.clone()).or_default().clone();
        let _guard = lock.lock();
        // Another thread may have finished the lookup while we waited.
        if let Some(hit) = self.memo.lock().get(&memo_key) {
            return hit.clone();
        }
        let found = self.lookup(rel, gradle, repos);
        self.memo.lock().insert(memo_key.clone(), found.clone());
        self.inflight.lock().remove(&memo_key);
        found
    }

    fn lookup(&self, rel: &str, gradle: Option<(&Coordinate, &[String])>, repos: &[Repository]) -> Option<PathBuf> {
        // 1. Maven local repository.
        if let Some(m2) = &self.config.maven_local {
            let p = m2.join(rel);
            if p.is_file() {
                return Some(p);
            }
        }
        // 2. Gradle's module cache: files-2.1/<group>/<artifact>/<version>/<sha1>/<file>
        if let (Some(home), Some((coord, names))) = (&self.config.gradle_user_home, gradle) {
            let dir = home.join("caches/modules-2/files-2.1").join(&coord.group).join(&coord.artifact).join(&coord.version);
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    for name in names {
                        let p = e.path().join(name);
                        if p.is_file() {
                            return Some(p);
                        }
                    }
                }
            }
        }
        // 3. file: repositories.
        for repo in repos {
            if let Some(dir) = repo.local_dir() {
                let p = dir.join(rel);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        // 4. Our download cache.
        let cached = self.config.cache_dir.join(rel);
        if cached.is_file() {
            return Some(cached);
        }
        if self.config.offline {
            return None;
        }
        // 5. Remote repositories (remember misses for a while).
        let marker = self.config.cache_dir.join(format!("{rel}.missing"));
        if let Ok(meta) = std::fs::metadata(&marker) {
            let fresh = meta.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).is_some_and(|age| age < self.config.negative_ttl);
            if fresh {
                return None;
            }
        }
        let mut transport_error = false;
        for repo in repos.iter().filter(|r| r.local_dir().is_none()) {
            let url = format!("{}{}", self.effective_url(repo), rel);
            match self.download(&url, &cached) {
                Ok(true) => return Some(cached),
                Ok(false) => continue,
                Err(e) => {
                    log::warn!("download failed {url}: {e}");
                    transport_error = true;
                }
            }
        }
        // Only remember definite misses (404s), never network failures.
        if transport_error {
            return None;
        }
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::write(&marker, b"");
        }
        None
    }

    /// Ok(false) for "not found", Err for transport problems.
    fn download(&self, url: &str, dest: &Path) -> anyhow::Result<bool> {
        let resp = match self.agent.get(url).call() {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(404 | 403 | 410)) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if let Some(p) = &self.progress {
            p(&format!("Downloading {}", url.rsplit('/').next().unwrap_or(url)));
        }
        let parent = dest.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let tmp = parent.join(format!(
            ".{}.{}-{}.part",
            dest.file_name().unwrap().to_string_lossy(),
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let copied = (|| -> std::io::Result<()> {
            let mut out = std::fs::File::create(&tmp)?;
            let mut reader = resp.into_body().into_reader();
            std::io::copy(&mut reader, &mut out)?;
            out.flush()
        })();
        if let Err(e) = copied {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        std::fs::rename(&tmp, dest)?;
        self.downloads.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    /// Resolve the timestamped file version of a remote SNAPSHOT.
    fn snapshot_file_version(&self, coord: &Coordinate, repos: &[Repository]) -> Option<String> {
        // Local repositories store plain -SNAPSHOT names.
        if let Some(m2) = &self.config.maven_local
            && m2.join(coord.repo_path(&coord.version, None, "pom")).is_file() {
                return None;
            }
        let rel = format!("{}/{}/{}/maven-metadata.xml", coord.group.replace('.', "/"), coord.artifact, coord.version);
        let path = self.file(&rel, None, repos)?;
        let xml = std::fs::read_to_string(path).ok()?;
        let root = super::xml::parse(&xml)?;
        let snap = root.child("versioning")?.child("snapshot")?;
        let ts = snap.child_text("timestamp")?;
        let build = snap.child_text("buildNumber")?;
        Some(format!("{}-{}-{}", coord.version.trim_end_matches("-SNAPSHOT"), ts, build))
    }

    /// All versions of a module known to local caches and remote metadata.
    pub fn list_versions(&self, module: &ModuleId, repos: &[Repository]) -> Vec<String> {
        let key = (module.clone(), repos.to_vec());
        if let Some(v) = self.versions_memo.lock().get(&key) {
            return v.clone();
        }
        let mut versions: Vec<String> = Vec::new();
        let group_path = module.group.replace('.', "/");
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(m2) = &self.config.maven_local {
            dirs.push(m2.join(&group_path).join(&module.artifact));
        }
        if let Some(home) = &self.config.gradle_user_home {
            dirs.push(home.join("caches/modules-2/files-2.1").join(&module.group).join(&module.artifact));
        }
        for d in dirs {
            if let Ok(entries) = std::fs::read_dir(d) {
                versions.extend(entries.flatten().filter(|e| e.path().is_dir()).filter_map(|e| e.file_name().into_string().ok()));
            }
        }
        let rel = format!("{group_path}/{}/maven-metadata.xml", module.artifact);
        for repo in repos.iter().filter(|r| r.local_dir().is_none()) {
            // Metadata is per repository (they list different versions) and changes over time.
            let cached = self.config.cache_dir.join(format!("metadata/{}/{rel}", sanitize(&repo.id)));
            let fresh = cached.is_file() && is_recent(&cached, Duration::from_secs(3600));
            if !self.config.offline && !fresh {
                let _ = self.download(&format!("{}{}", self.effective_url(repo), rel), &cached);
            }
            let path = cached.is_file().then_some(cached);
            if let Some(xml) = path.and_then(|p| std::fs::read_to_string(p).ok())
                && let Some(root) = super::xml::parse(&xml)
                    && let Some(vs) = root.child("versioning").and_then(|v| v.child("versions")) {
                        versions.extend(vs.children_named("version").map(|v| v.text.clone()));
                    }
        }
        versions.sort_by(|a, b| Version::parse(a).cmp(&Version::parse(b)));
        versions.dedup();
        self.versions_memo.lock().insert(key, versions.clone());
        versions
    }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn is_recent(p: &Path, ttl: Duration) -> bool {
    std::fs::metadata(p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age < ttl)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system(dir: &Path, offline: bool) -> RepoSystem {
        RepoSystem::new(
            RepoConfig {
                cache_dir: dir.join("cache"),
                maven_local: Some(dir.join("m2")),
                gradle_user_home: Some(dir.join("gradle")),
                offline,
                mirrors: vec![],
                negative_ttl: Duration::from_secs(60),
            },
            None,
        )
    }

    #[test]
    fn finds_artifacts_in_local_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let m2_pom = tmp.path().join("m2/org/x/a/1.0/a-1.0.pom");
        std::fs::create_dir_all(m2_pom.parent().unwrap()).unwrap();
        std::fs::write(&m2_pom, "<project/>").unwrap();
        let gradle_jar = tmp.path().join("gradle/caches/modules-2/files-2.1/org.y/b/2.0/abc123/b-2.0.jar");
        std::fs::create_dir_all(gradle_jar.parent().unwrap()).unwrap();
        std::fs::write(&gradle_jar, "jar").unwrap();

        let repo = system(tmp.path(), true);
        let a = Coordinate::new("org.x", "a", "1.0");
        assert_eq!(repo.artifact(&a, None, "pom", &[]), Some(m2_pom));
        let b = Coordinate::new("org.y", "b", "2.0");
        assert_eq!(repo.artifact(&b, None, "jar", &[]), Some(gradle_jar));
        assert_eq!(repo.artifact(&b, Some("sources"), "jar", &[]), None);
        let versions = repo.list_versions(&ModuleId::new("org.y", "b"), &[]);
        assert_eq!(versions, vec!["2.0".to_string()]);
    }

    #[test]
    fn file_repositories() {
        let tmp = tempfile::tempdir().unwrap();
        let jar = tmp.path().join("repo/g/a/1/a-1.jar");
        std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
        std::fs::write(&jar, "x").unwrap();
        let repo = system(tmp.path(), true);
        let r = Repository::new("local", &format!("file://{}", tmp.path().join("repo").display()));
        assert_eq!(repo.artifact(&Coordinate::new("g", "a", "1"), None, "jar", &[r]), Some(jar));
    }

    #[test]
    fn settings_xml() {
        let (local, mirrors) = parse_maven_settings(
            "<settings><localRepository>/opt/m2</localRepository><mirrors><mirror><id>m</id><mirrorOf>*</mirrorOf><url>https://nexus/repo</url></mirror></mirrors></settings>",
        );
        assert_eq!(local, Some(PathBuf::from("/opt/m2")));
        assert_eq!(mirrors, vec![("*".to_string(), "https://nexus/repo".to_string())]);
    }
}
