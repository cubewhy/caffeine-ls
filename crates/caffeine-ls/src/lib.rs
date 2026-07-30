pub mod config;
pub mod flags;

pub(crate) mod from_proto;
pub(crate) mod handlers;
// pub(crate) mod jdk;

mod global_state;
mod lsp;
mod main_loop;

pub use global_state::GlobalState;
pub use lsp::capabilities::server_capabilities;
pub use main_loop::main_loop;

use serde::de::DeserializeOwned;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const NAME: &str = env!("CARGO_PKG_NAME");

pub fn from_json<T: DeserializeOwned>(
    what: &'static str,
    json: &serde_json::Value,
) -> anyhow::Result<T> {
    serde_json::from_value(json.clone())
        .map_err(|e| anyhow::format_err!("Failed to deserialize {what}: {e}; {json}"))
}
