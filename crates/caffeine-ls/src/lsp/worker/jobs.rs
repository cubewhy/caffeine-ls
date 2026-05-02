use base_db::SourceDatabase;
use tower_lsp::Client;
use triomphe::Arc;

use crate::GlobalState;

pub async fn publish_diagnostics(_client: &Client, state: Arc<GlobalState>, file_id: vfs::FileId) {
    let db = state.db_snapshot().await;

    let Some(_parse_result) = db.parse_node(file_id) else {
        return;
    };

    // TODO: publish diagnostics
}
