use std::fmt;

use lsp_types::{ClientCapabilities, ClientInfo};
use vfs::AbsPathBuf;

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

    pub fn main_loop_num_threads(&self) -> usize {
        rayon::current_num_threads()
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

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClientConfig {}

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
