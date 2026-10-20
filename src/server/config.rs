//! Server settings (`initializationOptions` / `workspace/configuration` section `kotlin`).

use std::path::PathBuf;

use serde::Deserialize;

use crate::project::GradleMode;
use crate::project::repo::Repository;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[derive(Default)]
pub struct Config {
    /// JDK to index (defaults to JAVA_HOME / `java` on PATH / common install locations).
    pub jdk_home: Option<PathBuf>,
    /// Where downloaded artifacts and cached models live.
    pub cache_dir: Option<PathBuf>,
    pub project: ProjectConfig,
    pub diagnostics: DiagnosticsConfig,
    /// Lint rules: `{ "enabled": true, "rules": { "<RuleId or category>": "warning" | "off" | ... } }`.
    pub lint: crate::analysis::lints::LintConfig,
    /// `{ "types": true, "lambdaParameters": true, "returnTypes": true, "parameterNames": "literals" | "all" | "none" }`.
    pub inlay_hints: crate::analysis::ide::inlay::InlayConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ProjectConfig {
    pub gradle_mode: GradleMode,
    /// Allow running the project's build tool (Gradle wrapper) to obtain an exact model.
    /// Off by default because it executes code from the project.
    pub allow_build_tool_execution: bool,
    /// Never access the network; use local caches only.
    pub offline: bool,
    /// Additional Maven repositories (URLs).
    pub extra_repositories: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DiagnosticsConfig {
    pub enabled: bool,
    /// Delay before diagnostics are recomputed after an edit.
    pub debounce_ms: u64,
}


impl Default for ProjectConfig {
    fn default() -> Self {
        ProjectConfig { gradle_mode: GradleMode::Auto, allow_build_tool_execution: false, offline: false, extra_repositories: Vec::new() }
    }
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        DiagnosticsConfig { enabled: true, debounce_ms: 200 }
    }
}

impl Config {
    /// Accepts either `{ "kotlin": { ... } }` or the inner object.
    pub fn from_json(value: &serde_json::Value) -> Config {
        let inner = value.get("kotlin").unwrap_or(value);
        if inner.is_null() {
            return Config::default();
        }
        match serde_json::from_value::<Config>(inner.clone()) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("invalid configuration ({e}); using defaults");
                Config::default()
            }
        }
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.cache_dir.clone().unwrap_or_else(crate::util::default_cache_dir)
    }

    pub fn load_options(&self, progress: Option<crate::project::repo::ProgressFn>) -> crate::project::LoadOptions {
        let mut o = crate::project::LoadOptions::new(self.cache_dir());
        o.offline = self.project.offline;
        o.gradle_mode = self.project.gradle_mode;
        o.allow_tooling = self.project.allow_build_tool_execution;
        o.progress = progress;
        o.extra_repositories = self.project.extra_repositories.iter().map(|u| Repository::new(u.clone(), u)).collect();
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_settings() {
        let c = Config::from_json(&serde_json::json!({
            "kotlin": { "jdkHome": "/opt/jdk", "project": { "gradleMode": "tooling", "allowBuildToolExecution": true }, "diagnostics": { "debounceMs": 50 } }
        }));
        assert_eq!(c.jdk_home, Some(PathBuf::from("/opt/jdk")));
        assert_eq!(c.project.gradle_mode, GradleMode::Tooling);
        assert!(c.project.allow_build_tool_execution);
        assert!(c.diagnostics.enabled);
        assert_eq!(c.diagnostics.debounce_ms, 50);
        assert_eq!(Config::from_json(&serde_json::json!(null)), Config::default());
        let c = Config::from_json(&serde_json::json!({ "lint": { "rules": { "UnusedImport": "error", "naming": "off" } } }));
        assert!(c.lint.enabled);
        assert_eq!(c.lint.level(&crate::analysis::lints::UNUSED_IMPORT), crate::analysis::lints::Level::Error);
        assert_eq!(c.lint.level(&crate::analysis::lints::CLASS_NAME), crate::analysis::lints::Level::Off);
        let c = Config::from_json(&serde_json::json!({ "inlayHints": { "parameterNames": "all", "types": false } }));
        assert_eq!(c.inlay_hints.parameter_names, crate::analysis::ide::inlay::ParameterNames::All);
        assert!(!c.inlay_hints.types && c.inlay_hints.return_types);
    }
}
