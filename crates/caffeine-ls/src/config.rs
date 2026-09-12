use std::{env, fmt, path::PathBuf};

use directories::ProjectDirs;
use ide_db::line_index::WideEncoding;
use lsp_types::{ClientCapabilities, ClientInfo, PositionEncodingKind};
use rustc_hash::FxHashMap;
use vfs::AbsPathBuf;

use crate::{decompiler::Backend, line_index::PositionEncoding};

/// The decompiler used when the client names none. Vineflower is the better
/// reader of modern bytecode; it needs Java 17+ to run, so a client on an older
/// JVM selects `cfr` instead.
const DEFAULT_DECOMPILER: &str = "vineflower";

/// The `decompiler` spelling that disables decompilation.
const NO_DECOMPILER: &str = "none";

#[derive(Debug, Clone)]
pub struct Config {
    pub client_capabilities: ClientCapabilities,
    pub workspace_folders: Vec<AbsPathBuf>,
    pub client_info: Option<ClientInfo>,
    pub client_config: Option<ClientConfig>,
}

impl Config {
    pub fn new(
        client_capabilities: ClientCapabilities,
        workspace_folders: Vec<AbsPathBuf>,
        client_info: Option<ClientInfo>,
        client_config: Option<ClientConfig>,
    ) -> Self {
        Self {
            client_capabilities,
            workspace_folders,
            client_info,
            client_config,
        }
    }

    pub fn get_cache_dir(&self) -> PathBuf {
        self.client_config
            .clone()
            .and_then(|c| c.cache_dir)
            .unwrap_or_else(|| {
                if let Some(proj_dirs) = ProjectDirs::from("org", "cubewhy", "caffeine_ls") {
                    // Linux: ~/.cache/caffeine_ls/
                    // macOS: ~/Library/Caches/org.cubewhy.caffeine_ls/
                    // Win: C:\Users\Alice\AppData\Local\cubewhy\caffeine_ls
                    proj_dirs.cache_dir().to_path_buf()
                } else {
                    // Fallback if no home directory is found (rare, but happens in CI/Docker)
                    std::env::temp_dir().join("caffeine_ls")
                }
            })
    }

    pub fn apply_change(mut self, change: ConfigChange) -> (Self, ConfigErrors, bool) {
        let mut errors = ConfigErrors::default();
        let mut config_changed = false;

        if let Some(delta) = change.client_config_change {
            let mut current_json = serde_json::to_value(
                self.client_config
                    .clone()
                    .unwrap_or_else(|| serde_json::from_str("{}").unwrap()),
            )
            .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));

            merge(&mut current_json, &delta);

            match serde_json::from_value::<ClientConfig>(current_json) {
                Ok(new_client_config) => {
                    if self.client_config.as_ref() != Some(&new_client_config) {
                        config_changed = true;
                    }
                    self.client_config = Some(new_client_config);
                }
                Err(e) => {
                    errors.push(format!("Failed to update config: {}", e));
                }
            }
        }

        (self, errors, config_changed)
    }

    pub fn get_java_home(&self) -> Option<PathBuf> {
        if let Some(java_home) = self
            .client_config
            .as_ref()
            .and_then(|config| config.java_home.clone())
        {
            return Some(java_home);
        }

        // try to locate JAVA_HOME from env variables
        if let Ok(java_home_var) = env::var("JAVA_HOME") {
            let java_home = PathBuf::from(java_home_var);
            if java_home.is_dir() {
                return Some(java_home);
            }
        }

        None
    }

    pub fn main_loop_num_threads(&self) -> usize {
        rayon::current_num_threads()
    }

    /// Whether the build-system sync may download dependency sources.
    pub fn download_sources(&self) -> bool {
        self.client_config
            .as_ref()
            .is_some_and(|c| c.download_sources)
    }

    /// The selected decompiler backend and the jar to run it from, or `None`
    /// when decompilation is off: the id is `none`/empty, names no registered
    /// backend, has no jar configured, or that jar is not a file.
    ///
    /// The pair — and not just the id — is what a configuration change compares
    /// to decide whether the workspace has to be re-probed: pointing the same
    /// backend at another jar invalidates every decompiled file.
    pub fn decompiler_spec(&self) -> Option<(String, PathBuf)> {
        let config = self.client_config.as_ref()?;
        let id = config
            .decompiler
            .clone()
            .unwrap_or_else(|| DEFAULT_DECOMPILER.to_owned());
        if id.is_empty() || id == NO_DECOMPILER {
            return None;
        }
        crate::decompiler::backend(&id)?;
        let jar = config.decompiler_jars.as_ref()?.get(&id)?.clone();
        jar.is_file().then_some((id, jar))
    }

    /// The selected backend and its jar, resolved through the registry.
    /// `None` under the same conditions as [`Self::decompiler_spec`].
    pub(crate) fn decompiler(&self) -> Option<(&'static dyn Backend, PathBuf)> {
        let (id, jar) = self.decompiler_spec()?;
        Some((crate::decompiler::backend(&id)?, jar))
    }

    /// The JVM the decompiler is run with: the configured SDK's `bin/java` when
    /// the client set one (or the environment did) and it exists, else whatever
    /// `java` resolves to on `PATH`.
    pub fn decompiler_java(&self) -> PathBuf {
        if let Some(home) = self.get_java_home() {
            let executable = home
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" });
            if executable.is_file() {
                return executable;
            }
        }
        PathBuf::from("java")
    }

    /// The URI scheme the client serves library views over, `None` when the
    /// client configured none — or configured something the server cannot
    /// emit: an empty scheme, `file` (the scheme a real path already uses), or
    /// one outside the `[a-z0-9+.-]` a URI scheme is made of.
    pub fn library_uri_scheme(&self) -> Option<&str> {
        let scheme = self.client_config.as_ref()?.library_uri_scheme.as_deref()?;
        if scheme.is_empty() || scheme == "file" {
            return None;
        }
        scheme
            .bytes()
            .all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'+' | b'.' | b'-')
            })
            .then_some(scheme)
    }

    pub fn negotiated_encoding(&self) -> PositionEncoding {
        let supported_encodings = self
            .client_capabilities
            .general
            .as_ref()
            .and_then(|caps| caps.position_encodings.as_ref());

        if let Some(encodings) = supported_encodings {
            for enc in encodings {
                if enc == &PositionEncodingKind::UTF8 {
                    return PositionEncoding::Utf8;
                } else if enc == &PositionEncodingKind::UTF16 {
                    return PositionEncoding::Wide(WideEncoding::Utf16);
                } else if enc == &PositionEncodingKind::UTF32 {
                    return PositionEncoding::Wide(WideEncoding::Utf32);
                }
            }
        }

        PositionEncoding::Wide(WideEncoding::Utf16)
    }
}

fn merge(a: &mut serde_json::Value, b: &serde_json::Value) {
    match (a, b) {
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            for (k, v) in b {
                merge(a.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (a, b) => *a = b.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct ClientConfig {
    pub cache_dir: Option<PathBuf>,
    pub java_home: Option<PathBuf>,
    /// Let the build-system sync download dependency sources.
    #[serde(default, alias = "downloadSources")]
    pub download_sources: bool,
    /// Selected decompiler backend id, one of `decompiler::BACKENDS` or
    /// `"none"`. Absent means [`DEFAULT_DECOMPILER`]; an id whose jar is not
    /// configured disables decompilation for that session.
    #[serde(default)]
    pub decompiler: Option<String>,
    /// backend id → the jar to run it from.
    #[serde(default)]
    pub decompiler_jars: Option<FxHashMap<String, PathBuf>>,
    /// URI scheme the client serves library views over; absent keeps the
    /// materialized files' own `file://` URIs.
    #[serde(default)]
    pub library_uri_scheme: Option<String>,
}

#[derive(Debug, Default)]
pub struct ConfigChange {
    client_config_change: Option<serde_json::Value>,
}

impl ConfigChange {
    pub fn change_client_config(&mut self, json: serde_json::Value) {
        self.client_config_change = Some(json);
    }
}

#[derive(Debug, Default)]
pub struct ConfigErrors(Vec<String>);

impl ConfigErrors {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn push(&mut self, msg: String) {
        self.0.push(msg);
    }
}

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Configuration errors:\n{}", self.0.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration built the way the server builds one: from the client's
    /// initialization options.
    fn with_client_config(json: serde_json::Value) -> Config {
        let mut change = ConfigChange::default();
        change.change_client_config(json);
        Config::new(ClientCapabilities::default(), Vec::new(), None, None)
            .apply_change(change)
            .0
    }

    fn existing_file(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, b"a jar the server only checks for existence").unwrap();
        path
    }

    #[test]
    fn the_default_backend_is_vineflower() {
        let dir = tempfile::tempdir().unwrap();
        let jar = existing_file(&dir, "vineflower.jar");
        let config =
            with_client_config(serde_json::json!({ "decompiler_jars": { "vineflower": jar } }));

        assert_eq!(
            config.decompiler_spec(),
            Some(("vineflower".to_owned(), jar))
        );
        assert_eq!(
            config.decompiler().map(|(backend, _)| backend.id()),
            Some("vineflower")
        );
    }

    #[test]
    fn decompilation_is_off_without_a_usable_backend() {
        let dir = tempfile::tempdir().unwrap();
        let cfr = existing_file(&dir, "cfr.jar");

        for client_config in [
            // Explicitly disabled.
            serde_json::json!({ "decompiler": "none", "decompiler_jars": { "cfr": cfr } }),
            // An id nobody registered.
            serde_json::json!({ "decompiler": "jd", "decompiler_jars": { "jd": cfr } }),
            // A registered backend with no jar configured.
            serde_json::json!({ "decompiler": "cfr" }),
            // A jar that is not there — a clone without `cargo xtask prepare`.
            serde_json::json!({
                "decompiler": "cfr",
                "decompiler_jars": { "cfr": dir.path().join("missing.jar") },
            }),
            // The default backend with no jar either.
            serde_json::json!({ "decompiler_jars": { "cfr": cfr } }),
        ] {
            let config = with_client_config(client_config.clone());
            assert_eq!(config.decompiler_spec(), None, "{client_config}");
            assert!(config.decompiler().is_none(), "{client_config}");
        }
    }

    #[test]
    fn the_view_scheme_is_only_what_a_uri_can_spell() {
        let scheme_of = |scheme: serde_json::Value| {
            with_client_config(serde_json::json!({ "library_uri_scheme": scheme }))
                .library_uri_scheme()
                .map(str::to_owned)
        };

        assert_eq!(
            scheme_of(serde_json::json!("caffeine-ls")).as_deref(),
            Some("caffeine-ls")
        );
        // `file` is the scheme of a real path, an underscore is outside a URI
        // scheme's `[a-z0-9+.-]` ([RFC 3986] §3.1), and the client may send
        // nothing at all: none of them can be emitted.
        assert_eq!(scheme_of(serde_json::json!("file")), None);
        assert_eq!(scheme_of(serde_json::json!("caffeine_ls")), None);
        assert_eq!(scheme_of(serde_json::json!("Caffeine-LS")), None);
        assert_eq!(scheme_of(serde_json::json!("")), None);
        assert_eq!(scheme_of(serde_json::json!(null)), None);
    }
}
