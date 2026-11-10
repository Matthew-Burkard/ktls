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

/// How to authenticate to a repository.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Credentials {
    /// HTTP basic authentication (Gradle `PasswordCredentials`, Maven `<server>` username/password).
    Basic { username: String, password: String },
    /// An arbitrary header (Gradle `HttpHeaderCredentials`, Maven `<httpHeaders>`), e.g. a bearer token.
    Header { name: String, value: String },
}

impl Credentials {
    /// The request header carrying these credentials.
    fn header(&self) -> (String, String) {
        match self {
            Credentials::Basic { username, password } => ("Authorization".into(), format!("Basic {}", base64(format!("{username}:{password}").as_bytes()))),
            Credentials::Header { name, value } => (name.clone(), value.clone()),
        }
    }
}

/// Never print secrets (models and repositories are logged).
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::Basic { username, .. } => write!(f, "Basic({username}, ***)"),
            Credentials::Header { name, .. } => write!(f, "Header({name}: ***)"),
        }
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    /// Base URL, always ending in `/`. `file:` URLs are supported.
    pub url: String,
    /// Credentials declared with the repository (Gradle `credentials { }`). Never written to
    /// the model cache; Maven `<server>` credentials are looked up by `id` at request time.
    #[serde(skip)]
    pub credentials: Option<Credentials>,
}

impl Repository {
    pub fn new(id: impl Into<String>, url: &str) -> Self {
        let mut url = url.trim().to_string();
        if !url.ends_with('/') {
            url.push('/');
        }
        Repository { id: id.into(), url, credentials: None }
    }

    pub fn with_credentials(mut self, credentials: Option<Credentials>) -> Self {
        self.credentials = credentials;
        self
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
    /// Mirrors from `~/.m2/settings.xml`.
    pub mirrors: Vec<Mirror>,
    /// Maven `<server>` credentials by repository / mirror id.
    pub servers: HashMap<String, Credentials>,
    /// How long a failed remote lookup is remembered.
    pub negative_ttl: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mirror {
    pub id: String,
    pub mirror_of: String,
    pub url: String,
}

impl RepoConfig {
    pub fn from_environment(cache_dir: PathBuf, offline: bool) -> RepoConfig {
        let home = dirs::home_dir();
        let m2 = home.as_ref().map(|h| h.join(".m2"));
        let settings = m2.as_ref().map(|m| m.join("settings.xml")).and_then(|p| std::fs::read_to_string(p).ok());
        let MavenSettings { local_repository, mirrors, servers } = settings.as_deref().map(parse_maven_settings).unwrap_or_default();
        let maven_local = local_repository.or_else(|| m2.map(|m| m.join("repository")));
        let gradle_user_home = std::env::var_os("GRADLE_USER_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".gradle")));
        RepoConfig { cache_dir, maven_local, gradle_user_home, offline, mirrors, servers, negative_ttl: Duration::from_secs(6 * 3600) }
    }
}

#[derive(Default)]
struct MavenSettings {
    local_repository: Option<PathBuf>,
    mirrors: Vec<Mirror>,
    servers: HashMap<String, Credentials>,
}

/// Extract `<localRepository>`, `<mirrors>` and `<servers>` from a Maven settings.xml.
fn parse_maven_settings(xml: &str) -> MavenSettings {
    let Some(root) = super::xml::parse(xml) else { return MavenSettings::default() };
    let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
    // `${user.home}` and `${env.NAME}`, the interpolations commonly used for secrets.
    let interpolate = |s: &str| -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let Some(len) = rest[start..].find('}') else { break };
            let key = &rest[start + 2..start + len];
            match key.strip_prefix("env.") {
                Some(var) => out.push_str(&std::env::var(var).unwrap_or_default()),
                None if key == "user.home" => out.push_str(&home),
                None => out.push_str(&rest[start..start + len + 1]),
            }
            rest = &rest[start + len + 1..];
        }
        out.push_str(rest);
        out.trim().to_string()
    };
    let local_repository = root.child_text("localRepository").map(|s| PathBuf::from(interpolate(s)));
    let mirrors = root
        .child("mirrors")
        .map(|m| {
            m.children_named("mirror")
                .filter_map(|m| {
                    let mirror_of = m.child_text("mirrorOf")?.to_string();
                    let url = m.child_text("url")?.to_string();
                    Some(Mirror { id: m.child_text("id").unwrap_or(&mirror_of).to_string(), mirror_of, url })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut servers = HashMap::new();
    for s in root.child("servers").into_iter().flat_map(|s| s.children_named("server")) {
        let Some(id) = s.child_text("id") else { continue };
        let header = s.path(&["configuration", "httpHeaders"]).and_then(|h| {
            h.children_named("property").find_map(|p| Some((interpolate(p.child_text("name")?), interpolate(p.child_text("value")?))))
        });
        let credentials = match (s.child_text("username").map(interpolate), s.child_text("password").map(interpolate), header) {
            (_, Some(password), _) if password.starts_with('{') && password.ends_with('}') => {
                log::warn!("settings.xml server {id}: encrypted passwords are not supported");
                continue;
            }
            (Some(username), Some(password), _) => Credentials::Basic { username, password },
            (_, _, Some((name, value))) => Credentials::Header { name, value },
            _ => continue,
        };
        servers.insert(id.to_string(), credentials);
    }
    MavenSettings { local_repository, mirrors, servers }
}

pub type ProgressFn = Arc<dyn Fn(&str) + Send + Sync>;

pub struct RepoSystem {
    pub config: RepoConfig,
    agent: ureq::Agent,
    memo: Mutex<HashMap<String, Option<PathBuf>>>,
    /// One lock per path being looked up, so concurrent resolutions never download twice.
    inflight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    versions_memo: Mutex<HashMap<(ModuleId, Vec<Repository>), Vec<String>>>,
    /// Snapshot metadata path and repositories → timestamped file version.
    snapshot_memo: Mutex<HashMap<String, Option<String>>>,
    downloads: AtomicUsize,
    progress: Option<ProgressFn>,
    /// Repositories that refused a request (401/403): id → (status, credentials were sent).
    denied: Mutex<std::collections::BTreeMap<String, (u16, bool)>>,
    /// Repositories that answered with something other than the requested file (a login
    /// page instead of a jar): id → (file name, what was wrong).
    invalid: Mutex<std::collections::BTreeMap<String, (String, String)>>,
}

/// How long cached SNAPSHOT `maven-metadata.xml` is trusted before asking the repository again.
const SNAPSHOT_METADATA_TTL: Duration = Duration::from_secs(3600);

/// Outcome of a remote request.
enum Fetch {
    Downloaded,
    NotFound,
    /// 401/403: an authentication problem, not proof the file doesn't exist.
    Denied(u16),
    /// A successful response whose body is not the file (an HTML login or proxy error page).
    Invalid(String),
}

impl RepoSystem {
    pub fn new(config: RepoConfig, progress: Option<ProgressFn>) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .timeout_connect(Some(Duration::from_secs(10)))
            .http_status_as_error(true)
            .user_agent("ktls/0.1")
            .build()
            .into();
        RepoSystem {
            config,
            agent,
            memo: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            versions_memo: Mutex::new(HashMap::new()),
            snapshot_memo: Mutex::new(HashMap::new()),
            downloads: AtomicUsize::new(0),
            progress,
            denied: Mutex::new(Default::default()),
            invalid: Mutex::new(Default::default()),
        }
    }

    pub fn download_count(&self) -> usize {
        self.downloads.load(Ordering::Relaxed)
    }

    /// Repositories that refused access or sent something other than the requested files, as
    /// user-facing problems.
    pub fn access_problems(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .denied
            .lock()
            .iter()
            .map(|(id, (status, sent))| {
                if *sent {
                    format!("Repository {id} rejected the configured credentials (HTTP {status})")
                } else {
                    format!("Repository {id} requires authentication (HTTP {status}): add credentials to the build script, gradle.properties or ~/.m2/settings.xml")
                }
            })
            .collect();
        out.extend(self.invalid.lock().iter().map(|(id, (file, why))| {
            format!("Repository {id} sent {why} instead of {file}: check the repository URL and credentials")
        }));
        out
    }

    /// The URL to request for `repo` (applying mirrors) and the credentials to send.
    fn endpoint<'a>(&'a self, repo: &'a Repository) -> (String, Option<&'a Credentials>) {
        for m in &self.config.mirrors {
            let applies = m.mirror_of == "*"
                || m.mirror_of.split(',').any(|id| id.trim() == repo.id)
                || (m.mirror_of.starts_with("external:") && !repo.url.contains("localhost") && !repo.url.starts_with("file:"));
            if applies {
                let mut u = m.url.clone();
                if !u.ends_with('/') {
                    u.push('/');
                }
                return (u, self.config.servers.get(&m.id));
            }
        }
        (repo.url.clone(), repo.credentials.as_ref().or_else(|| self.config.servers.get(&repo.id)))
    }

    /// Download `rel` from `repo` into `dest`, remembering authentication failures.
    fn fetch(&self, repo: &Repository, rel: &str, dest: &Path) -> anyhow::Result<Fetch> {
        let (base, credentials) = self.endpoint(repo);
        let url = format!("{base}{rel}");
        let result = self.download(&url, dest, credentials);
        if let Ok(Fetch::Denied(status)) = result {
            log::warn!("{url}: HTTP {status} ({})", if credentials.is_some() { "credentials rejected" } else { "no credentials configured" });
            self.denied.lock().insert(repo.id.clone(), (status, credentials.is_some()));
        }
        if let Ok(Fetch::Invalid(why)) = &result {
            log::warn!("{url}: the response is {why}, not the requested file");
            let file = rel.rsplit('/').next().unwrap_or(rel).to_string();
            self.invalid.lock().entry(repo.id.clone()).or_insert((file, why.clone()));
        }
        result
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
            // Older versions saved whatever the server sent; drop a cached login page.
            match content_problem(&cached) {
                None => return Some(cached),
                Some(why) => {
                    log::warn!("discarding cached {}: it is {why}", cached.display());
                    let _ = std::fs::remove_file(&cached);
                }
            }
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
        let mut uncertain = false;
        for repo in repos.iter().filter(|r| r.local_dir().is_none()) {
            match self.fetch(repo, rel, &cached) {
                Ok(Fetch::Downloaded) => return Some(cached),
                Ok(Fetch::NotFound) => continue,
                Ok(Fetch::Denied(_) | Fetch::Invalid(_)) => uncertain = true,
                Err(e) => {
                    log::warn!("download failed {}{rel}: {e}", repo.url);
                    uncertain = true;
                }
            }
        }
        // Only remember definite misses (404s), never network or authentication failures.
        if uncertain {
            return None;
        }
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::write(&marker, b"");
        }
        None
    }

    /// Err for transport problems.
    fn download(&self, url: &str, dest: &Path, credentials: Option<&Credentials>) -> anyhow::Result<Fetch> {
        let resp = match self.get(url, credentials) {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(404 | 410)) => return Ok(Fetch::NotFound),
            Err(ureq::Error::StatusCode(s @ (401 | 403))) => return Ok(Fetch::Denied(s)),
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
        if let Some(why) = content_problem_as(&tmp, dest) {
            let _ = std::fs::remove_file(&tmp);
            return Ok(Fetch::Invalid(why));
        }
        std::fs::rename(&tmp, dest)?;
        self.downloads.fetch_add(1, Ordering::Relaxed);
        Ok(Fetch::Downloaded)
    }

    /// GET `url`. With credentials, redirects are followed here so the credential header (which
    /// may be a custom one, unlike `Authorization` that ureq guards) is only sent to the original
    /// host: Artifactory and Nexus commonly redirect downloads to blob storage elsewhere.
    fn get(&self, url: &str, credentials: Option<&Credentials>) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let Some(c) = credentials else { return self.agent.get(url).call() };
        let (name, value) = c.header();
        let origin: ureq::http::Uri = url.parse().map_err(|_| ureq::Error::BadUri(url.to_string()))?;
        let mut current = origin.clone();
        for _ in 0..10 {
            let mut req = self.agent.get(&current).config().max_redirects(0).max_redirects_will_error(false).build();
            if may_send_credentials(&origin, &current) {
                req = req.header(&name, &value);
            }
            let resp = req.call()?;
            if !resp.status().is_redirection() {
                return Ok(resp);
            }
            let location = resp.headers().get("location").and_then(|l| l.to_str().ok()).ok_or(ureq::Error::RedirectFailed)?;
            current = redirect_target(&current, location).ok_or(ureq::Error::RedirectFailed)?;
        }
        Err(ureq::Error::TooManyRedirects)
    }

    /// Resolve the timestamped file version of a remote SNAPSHOT.
    fn snapshot_file_version(&self, coord: &Coordinate, repos: &[Repository]) -> Option<String> {
        // Local repositories store plain -SNAPSHOT names.
        if let Some(m2) = &self.config.maven_local
            && m2.join(coord.repo_path(&coord.version, None, "pom")).is_file() {
                return None;
            }
        let rel = format!("{}/{}/{}/maven-metadata.xml", coord.group.replace('.', "/"), coord.artifact, coord.version);
        let memo_key = format!("snapshot|{rel}|{}", repos.iter().map(|r| r.url.as_str()).collect::<Vec<_>>().join(","));
        if let Some(hit) = self.snapshot_memo.lock().get(&memo_key) {
            return hit.clone();
        }
        let lock = self.inflight.lock().entry(memo_key.clone()).or_default().clone();
        let _guard = lock.lock();
        if let Some(hit) = self.snapshot_memo.lock().get(&memo_key) {
            return hit.clone();
        }
        let found = self.lookup_snapshot(coord, &rel, repos);
        self.snapshot_memo.lock().insert(memo_key.clone(), found.clone());
        self.inflight.lock().remove(&memo_key);
        found
    }

    /// The latest SNAPSHOT build listed by the first repository that has one. New builds are
    /// published under the same metadata path, so cached metadata is refreshed after
    /// `SNAPSHOT_METADATA_TTL` (and kept when the refresh fails).
    fn lookup_snapshot(&self, coord: &Coordinate, rel: &str, repos: &[Repository]) -> Option<String> {
        let parse = |p: &Path| -> Option<String> {
            let xml = std::fs::read_to_string(p).ok()?;
            let root = super::xml::parse(&xml)?;
            let snap = root.child("versioning")?.child("snapshot")?;
            let ts = snap.child_text("timestamp")?;
            let build = snap.child_text("buildNumber")?;
            Some(format!("{}-{}-{}", coord.version.trim_end_matches("-SNAPSHOT"), ts, build))
        };
        for repo in repos {
            let path = match repo.local_dir() {
                Some(dir) => dir.join(rel),
                None => {
                    let cached = self.config.cache_dir.join(format!("metadata/{}/{rel}", sanitize(&repo.id)));
                    if !self.config.offline && !(cached.is_file() && is_recent(&cached, SNAPSHOT_METADATA_TTL)) {
                        let _ = self.fetch(repo, rel, &cached);
                    }
                    cached
                }
            };
            if let Some(v) = parse(&path) {
                return Some(v);
            }
        }
        None
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
                let _ = self.fetch(repo, &rel, &cached);
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

/// Credentials go only to the host they were configured for, and never over a downgrade to http.
fn may_send_credentials(origin: &ureq::http::Uri, target: &ureq::http::Uri) -> bool {
    origin.host() == target.host() && origin.port_u16() == target.port_u16() && (target.scheme_str() == Some("https") || origin.scheme_str() == target.scheme_str())
}

/// Resolve a `Location` header (absolute, or relative to the current URL's origin or path).
fn redirect_target(current: &ureq::http::Uri, location: &str) -> Option<ureq::http::Uri> {
    if location.contains("://") {
        return location.parse().ok();
    }
    let origin = format!("{}://{}", current.scheme_str()?, current.authority()?);
    let path = if let Some(rest) = location.strip_prefix("//") {
        return format!("{}://{rest}", current.scheme_str()?).parse().ok();
    } else if location.starts_with('/') {
        location.to_string()
    } else {
        let dir = current.path().rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        format!("{dir}/{location}")
    };
    format!("{origin}{path}").parse().ok()
}

/// What is wrong with the file at `path` if it is not what its name says: archives must be
/// zips, and nothing may be an HTML page (repositories behind SSO answer 200 with a login page).
fn content_problem(path: &Path) -> Option<String> {
    content_problem_as(path, path)
}

/// Like `content_problem`, for content at `path` that will be stored as `name`.
fn content_problem_as(path: &Path, name: &Path) -> Option<String> {
    use std::io::Read;
    let mut head = Vec::with_capacity(512);
    std::fs::File::open(path).ok()?.take(512).read_to_end(&mut head).ok()?;
    let text = String::from_utf8_lossy(&head);
    let start = text.trim_start_matches('\u{feff}').trim_start().to_ascii_lowercase();
    let html = start.starts_with("<!doctype html") || start.starts_with("<html") || start.starts_with("<head") || start.starts_with("<body");
    if html {
        return Some("a web page (a login page or proxy error?)".into());
    }
    if crate::classpath::is_archive(name) && !(head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06")) {
        return Some(if head.is_empty() { "an empty file".into() } else { "not a zip archive".into() });
    }
    None
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
                servers: HashMap::new(),
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
        let s = parse_maven_settings(
            "<settings><localRepository>/opt/m2</localRepository><mirrors><mirror><id>m</id><mirrorOf>*</mirrorOf><url>https://nexus/repo</url></mirror></mirrors></settings>",
        );
        assert_eq!(s.local_repository, Some(PathBuf::from("/opt/m2")));
        assert_eq!(s.mirrors, vec![Mirror { id: "m".into(), mirror_of: "*".into(), url: "https://nexus/repo".into() }]);
    }

    #[test]
    fn settings_xml_servers() {
        // SAFETY: tests touching this variable don't run concurrently with readers of it.
        unsafe { std::env::set_var("KTLS_TEST_ARTIFACTORY_TOKEN", "s3cret") };
        let s = parse_maven_settings(
            "<settings><servers>\
               <server><id>jfrog</id><username>me</username><password>${env.KTLS_TEST_ARTIFACTORY_TOKEN}</password></server>\
               <server><id>gitlab</id><configuration><httpHeaders><property><name>Private-Token</name><value>tok</value></property></httpHeaders></configuration></server>\
               <server><id>enc</id><username>me</username><password>{COQLCE6DU6GtcS5P=}</password></server>\
             </servers></settings>",
        );
        assert_eq!(s.servers.get("jfrog"), Some(&Credentials::Basic { username: "me".into(), password: "s3cret".into() }));
        assert_eq!(s.servers.get("gitlab"), Some(&Credentials::Header { name: "Private-Token".into(), value: "tok".into() }));
        assert!(!s.servers.contains_key("enc"));
    }

    #[test]
    fn credentials_for_mirrors_and_repositories() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sys = system(tmp.path(), true);
        let basic = Credentials::Basic { username: "u".into(), password: "p".into() };
        sys.config.servers.insert("private".into(), basic.clone());
        // By repository id (Maven <repository><id>).
        let r = Repository::new("private", "https://example.jfrog.io/artifactory/libs");
        assert_eq!(sys.endpoint(&r), ("https://example.jfrog.io/artifactory/libs/".to_string(), Some(&basic)));
        // Declared with the repository (Gradle credentials { }).
        let own = Credentials::Header { name: "Authorization".into(), value: "Bearer t".into() };
        let g = Repository::new("https://x/", "https://x/").with_credentials(Some(own.clone()));
        assert_eq!(sys.endpoint(&g).1, Some(&own));
        // A mirror uses its own server entry.
        sys.config.mirrors.push(Mirror { id: "private".into(), mirror_of: "central".into(), url: "https://example.jfrog.io/central".into() });
        assert_eq!(sys.endpoint(&Repository::maven_central()), ("https://example.jfrog.io/central/".to_string(), Some(&basic)));
        assert_eq!(format!("{basic:?}"), "Basic(u, ***)");
    }

    #[test]
    fn credentials_stay_on_their_host() {
        let u = |s: &str| s.parse::<ureq::http::Uri>().unwrap();
        let origin = u("https://example.jfrog.io/artifactory/libs/a.jar");
        assert!(may_send_credentials(&origin, &u("https://example.jfrog.io/artifactory/api/x")));
        assert!(!may_send_credentials(&origin, &u("https://bucket.s3.amazonaws.com/a.jar?sig=1")));
        assert!(!may_send_credentials(&origin, &u("http://example.jfrog.io/a.jar")));
        assert!(!may_send_credentials(&origin, &u("https://example.jfrog.io:8443/a.jar")));
        assert_eq!(redirect_target(&origin, "/b/c.jar"), Some(u("https://example.jfrog.io/b/c.jar")));
        assert_eq!(redirect_target(&origin, "c.jar"), Some(u("https://example.jfrog.io/artifactory/libs/c.jar")));
        assert_eq!(redirect_target(&origin, "https://cdn.example.com/x"), Some(u("https://cdn.example.com/x")));
        assert_eq!(redirect_target(&origin, "//cdn.example.com/x"), Some(u("https://cdn.example.com/x")));
    }

    /// A tiny HTTP server: `handler(request head) -> response`; returns its base URL and the
    /// request heads it saw.
    fn serve(handler: impl Fn(&str) -> String + Send + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                while reader.read_line(&mut head).is_ok_and(|n| n > 2) {}
                log.lock().push(head.clone());
                let _ = (&stream).write_all(handler(&head).as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}/"), seen)
    }

    fn respond(status: &str, extra: &str, body: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}", body.len())
    }

    #[test]
    fn downloads_with_credentials() {
        // Another host (by name) that must never see the credentials.
        let (blob, blob_seen) = serve(|_| respond("200 OK", "", "PK\u{3}\u{4}blob"));
        let blob = blob.replace("127.0.0.1", "localhost");
        let (base, _) = serve(move |head| {
            let authorized = head.lines().any(|l| l.eq_ignore_ascii_case("authorization: Basic dTpw"));
            match head.split_whitespace().nth(1).unwrap_or("") {
                _ if !authorized => respond("401 Unauthorized", "WWW-Authenticate: Basic\r\n", ""),
                "/g/a/1/a-1.jar" => respond("200 OK", "", "PK\u{3}\u{4}jar"),
                "/g/b/1/b-1.jar" => respond("302 Found", "Location: /real/b-1.jar\r\n", ""),
                "/real/b-1.jar" => respond("200 OK", "", "PK\u{3}\u{4}b"),
                "/g/c/1/c-1.jar" => respond("302 Found", &format!("Location: {blob}c-1.jar\r\n"), ""),
                _ => respond("404 Not Found", "", ""),
            }
        });
        let tmp = tempfile::tempdir().unwrap();
        let read = |p: Option<PathBuf>| p.map(|p| std::fs::read_to_string(p).unwrap().trim_start_matches("PK\u{3}\u{4}").to_string());

        // Without credentials: denied, reported, and not remembered as missing.
        let anonymous = system(tmp.path(), false);
        let repo = Repository::new("private", &base);
        assert_eq!(anonymous.artifact(&Coordinate::new("g", "a", "1"), None, "jar", std::slice::from_ref(&repo)), None);
        assert!(!tmp.path().join("cache/g/a/1/a-1.jar.missing").exists());
        assert_eq!(anonymous.access_problems().len(), 1);
        assert!(anonymous.access_problems()[0].contains("requires authentication"), "{:?}", anonymous.access_problems());

        let mut sys = system(tmp.path(), false);
        sys.config.servers.insert("private".into(), Credentials::Basic { username: "u".into(), password: "p".into() });
        let repos = [repo];
        assert_eq!(read(sys.artifact(&Coordinate::new("g", "a", "1"), None, "jar", &repos)), Some("jar".into()));
        assert_eq!(read(sys.artifact(&Coordinate::new("g", "b", "1"), None, "jar", &repos)), Some("b".into()));
        assert_eq!(read(sys.artifact(&Coordinate::new("g", "c", "1"), None, "jar", &repos)), Some("blob".into()));
        assert!(blob_seen.lock().iter().all(|h| !h.to_ascii_lowercase().contains("authorization")), "{:?}", blob_seen.lock());
        assert!(sys.access_problems().is_empty());
    }

    const ZIP: &str = "PK\u{5}\u{6}\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

    #[test]
    fn web_pages_are_not_saved_as_artifacts() {
        let page = "<!DOCTYPE html><html><body>Sign in</body></html>";
        let (base, _) = serve(move |head| match head.split_whitespace().nth(1).unwrap_or("") {
            "/g/a/1/a-1.jar" | "/g/a/1/a-1.pom" => respond("200 OK", "Content-Type: text/html\r\n", page),
            "/g/b/1/b-1.jar" => respond("200 OK", "", "not a zip"),
            "/g/c/1/c-1.jar" => respond("200 OK", "", ZIP),
            _ => respond("404 Not Found", "", ""),
        });
        let tmp = tempfile::tempdir().unwrap();
        let repos = [Repository::new("private", &base)];
        let sys = system(tmp.path(), false);
        let a = Coordinate::new("g", "a", "1");
        assert_eq!(sys.artifact(&a, None, "jar", &repos), None);
        assert_eq!(sys.artifact(&a, None, "pom", &repos), None);
        assert_eq!(sys.artifact(&Coordinate::new("g", "b", "1"), None, "jar", &repos), None);
        // Neither saved nor remembered as missing: the next load asks again.
        for f in ["a-1.jar", "a-1.pom", "a-1.jar.missing"] {
            assert!(!tmp.path().join("cache/g/a/1").join(f).exists(), "{f}");
        }
        let problems = sys.access_problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("sent a web page") && problems[0].contains("a-1.jar"), "{problems:?}");

        // A login page cached by an older version is replaced by the real archive.
        let cached = tmp.path().join("cache/g/c/1/c-1.jar");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, page).unwrap();
        let sys = system(tmp.path(), false);
        assert_eq!(sys.artifact(&Coordinate::new("g", "c", "1"), None, "jar", &repos), Some(cached.clone()));
        assert_eq!(std::fs::read_to_string(&cached).unwrap(), ZIP);
    }

    #[test]
    fn snapshot_metadata_is_refreshed() {
        let build = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let served = build.clone();
        let (base, seen) = serve(move |head| {
            let n = served.load(Ordering::SeqCst);
            match head.split_whitespace().nth(1).unwrap_or("") {
                "/g/a/1.0-SNAPSHOT/maven-metadata.xml" => respond(
                    "200 OK",
                    "",
                    &format!("<metadata><versioning><snapshot><timestamp>20261109.10000{n}</timestamp><buildNumber>{n}</buildNumber></snapshot></versioning></metadata>"),
                ),
                p if p == format!("/g/a/1.0-SNAPSHOT/a-1.0-20261109.10000{n}-{n}.jar") => respond("200 OK", "", ZIP),
                _ => respond("404 Not Found", "", ""),
            }
        });
        let tmp = tempfile::tempdir().unwrap();
        let repos = [Repository::new("snapshots", &base)];
        let coord = Coordinate::new("g", "a", "1.0-SNAPSHOT");
        let name = |p: Option<PathBuf>| p.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        assert_eq!(name(system(tmp.path(), false).artifact(&coord, None, "jar", &repos)).as_deref(), Some("a-1.0-20261109.100001-1.jar"));

        // A new build is published; recent metadata is still trusted (no request)…
        build.store(2, Ordering::SeqCst);
        let requests = seen.lock().len();
        assert_eq!(name(system(tmp.path(), false).artifact(&coord, None, "jar", &repos)).as_deref(), Some("a-1.0-20261109.100001-1.jar"));
        assert_eq!(seen.lock().len(), requests);

        // …until it is older than the TTL.
        let cached = tmp.path().join("cache/metadata/snapshots/g/a/1.0-SNAPSHOT/maven-metadata.xml");
        let old = SystemTime::now() - SNAPSHOT_METADATA_TTL - Duration::from_secs(60);
        std::fs::File::options().write(true).open(&cached).unwrap().set_modified(old).unwrap();
        assert_eq!(name(system(tmp.path(), false).artifact(&coord, None, "jar", &repos)).as_deref(), Some("a-1.0-20261109.100002-2.jar"));

        // Offline, stale metadata is used as is.
        std::fs::File::options().write(true).open(&cached).unwrap().set_modified(old).unwrap();
        build.store(3, Ordering::SeqCst);
        assert_eq!(name(system(tmp.path(), true).artifact(&coord, None, "jar", &repos)).as_deref(), Some("a-1.0-20261109.100002-2.jar"));
    }

    #[test]
    fn basic_auth_header() {
        let c = Credentials::Basic { username: "Aladdin".into(), password: "open sesame".into() };
        assert_eq!(c.header().1, "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b""), "");
    }
}
