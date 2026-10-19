//! Gradle version catalogs (`gradle/libs.versions.toml`).

use std::collections::HashMap;

use crate::project::coords::ModuleId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogLibrary {
    pub module: ModuleId,
    pub version: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub versions: HashMap<String, String>,
    pub libraries: HashMap<String, CatalogLibrary>,
    pub bundles: HashMap<String, Vec<String>>,
    pub plugins: HashMap<String, (String, Option<String>)>,
}

/// Aliases are accessed with `-`, `_` and `.` all mapped to `.`.
pub fn normalize(alias: &str) -> String {
    alias.replace(['-', '_'], ".")
}

fn rich_version(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Table(t) => ["strictly", "require", "prefer"]
            .iter()
            .find_map(|k| t.get(*k).and_then(|v| v.as_str()))
            .map(str::to_string),
        _ => None,
    }
}

impl Catalog {
    pub fn parse(text: &str) -> anyhow::Result<Catalog> {
        let table: toml::Table = toml::from_str(text)?;
        let mut cat = Catalog::default();
        if let Some(versions) = table.get("versions").and_then(|v| v.as_table()) {
            for (k, v) in versions {
                if let Some(v) = rich_version(v) {
                    cat.versions.insert(normalize(k), v);
                }
            }
        }
        let version_of = |cat: &Catalog, t: &toml::Table| -> Option<String> {
            match t.get("version") {
                Some(toml::Value::Table(vt)) if vt.contains_key("ref") => {
                    vt.get("ref").and_then(|r| r.as_str()).and_then(|r| cat.versions.get(&normalize(r)).cloned())
                }
                Some(v) => rich_version(v),
                None => None,
            }
        };
        if let Some(libs) = table.get("libraries").and_then(|v| v.as_table()) {
            for (alias, v) in libs {
                let lib = match v {
                    toml::Value::String(s) => {
                        let mut parts = s.splitn(3, ':');
                        let (Some(g), Some(a)) = (parts.next(), parts.next()) else { continue };
                        CatalogLibrary { module: ModuleId::new(g, a), version: parts.next().map(str::to_string) }
                    }
                    toml::Value::Table(t) => {
                        let module = if let Some(m) = t.get("module").and_then(|m| m.as_str()) {
                            let Some((g, a)) = m.split_once(':') else { continue };
                            ModuleId::new(g, a)
                        } else {
                            let (Some(g), Some(a)) = (t.get("group").and_then(|g| g.as_str()), t.get("name").and_then(|n| n.as_str())) else {
                                continue;
                            };
                            ModuleId::new(g, a)
                        };
                        CatalogLibrary { module, version: version_of(&cat, t) }
                    }
                    _ => continue,
                };
                cat.libraries.insert(normalize(alias), lib);
            }
        }
        if let Some(bundles) = table.get("bundles").and_then(|v| v.as_table()) {
            for (alias, v) in bundles {
                if let Some(arr) = v.as_array() {
                    cat.bundles.insert(normalize(alias), arr.iter().filter_map(|x| x.as_str()).map(normalize).collect());
                }
            }
        }
        if let Some(plugins) = table.get("plugins").and_then(|v| v.as_table()) {
            for (alias, v) in plugins {
                let entry = match v {
                    toml::Value::String(s) => match s.split_once(':') {
                        Some((id, ver)) => (id.to_string(), Some(ver.to_string())),
                        None => (s.clone(), None),
                    },
                    toml::Value::Table(t) => {
                        let Some(id) = t.get("id").and_then(|i| i.as_str()) else { continue };
                        (id.to_string(), version_of(&cat, t))
                    }
                    _ => continue,
                };
                cat.plugins.insert(normalize(alias), entry);
            }
        }
        Ok(cat)
    }

    /// Look up an accessor path relative to the catalog name, e.g. `androidx.core.ktx`.
    pub fn library(&self, path: &str) -> Option<&CatalogLibrary> {
        self.libraries.get(path)
    }

    pub fn bundle(&self, path: &str) -> Option<Vec<&CatalogLibrary>> {
        self.bundles.get(path).map(|libs| libs.iter().filter_map(|l| self.libraries.get(l)).collect())
    }

    pub fn plugin(&self, path: &str) -> Option<&(String, Option<String>)> {
        self.plugins.get(path)
    }

    pub fn version(&self, path: &str) -> Option<&String> {
        self.versions.get(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_notations() {
        let cat = Catalog::parse(
            r#"
[versions]
kotlin = "2.0.0"
ktor = { strictly = "2.3.7" }

[libraries]
plain = "com.a:plain:1.0"
ktor-core = { module = "io.ktor:ktor-server-core", version.ref = "ktor" }
kotlin_stdlib = { group = "org.jetbrains.kotlin", name = "kotlin-stdlib", version.ref = "kotlin" }
managed = { module = "x:managed" }

[bundles]
ktor = ["ktor-core", "plain"]

[plugins]
kotlin-jvm = { id = "org.jetbrains.kotlin.jvm", version.ref = "kotlin" }
short = "com.x.plugin:1.2"
"#,
        )
        .unwrap();
        assert_eq!(cat.library("plain").unwrap().version.as_deref(), Some("1.0"));
        assert_eq!(cat.library("ktor.core").unwrap().version.as_deref(), Some("2.3.7"));
        assert_eq!(cat.library("kotlin.stdlib").unwrap().module, ModuleId::new("org.jetbrains.kotlin", "kotlin-stdlib"));
        assert_eq!(cat.library("managed").unwrap().version, None);
        assert_eq!(cat.bundle("ktor").unwrap().len(), 2);
        assert_eq!(cat.plugin("kotlin.jvm").unwrap(), &("org.jetbrains.kotlin.jvm".to_string(), Some("2.0.0".to_string())));
        assert_eq!(cat.plugin("short").unwrap().1.as_deref(), Some("1.2"));
        assert_eq!(cat.version("kotlin").map(String::as_str), Some("2.0.0"));
    }
}
