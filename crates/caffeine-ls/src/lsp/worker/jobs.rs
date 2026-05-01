use base_db::SourceDatabase;
use tower_lsp::Client;
use triomphe::Arc;

use crate::GlobalState;

pub async fn publish_diagnostics(_client: &Client, state: Arc<GlobalState>, file_id: vfs::FileId) {
    let db = state.db_snapshot().await;

    let content = db.file_text(file_id).text(&db);

    let mut cache_lock = state.syntax_cache.lock().await;
    let syntax_cache = cache_lock.entry(file_id).or_default();

    drop(cache_lock);

    let vfs = state.get_vfs().await;
    let path = vfs.file_path(file_id);

    drop(vfs);

    // TODO: parse and publish diagnostics
}
