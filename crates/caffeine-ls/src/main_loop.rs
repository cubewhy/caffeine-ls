use lsp_server::Connection;

use crate::{GlobalState, config::Config};

pub fn main_loop(config: Config, connection: Connection) -> anyhow::Result<()> {
    tracing::info!("initial config: {:#?}", config);

    GlobalState::new(connection.sender, config).run(connection.receiver)
}
