use std::collections::HashMap;

use tokio::sync::{Mutex, MutexGuard, RwLockReadGuard};

use arc_swap::ArcSwapOption;
use ide_db::RootDatabase;
use tokio::sync::RwLock;
use vfs::Vfs;

use crate::config::Config;

#[derive(Default)]
pub struct GlobalState {
    pub config: ArcSwapOption<Option<Config>>,
    pub vfs: RwLock<Vfs>,
    pub db: Mutex<RootDatabase>,
    pub syntax_cache: Mutex<HashMap<vfs::FileId, rowan::NodeCache>>,
}

impl GlobalState {
    pub async fn get_vfs(&self) -> RwLockReadGuard<'_, Vfs> {
        self.vfs.read().await
    }

    pub async fn db_snapshot(&self) -> RootDatabase {
        let db = self.db.lock().await;
        db.clone()
    }

    pub async fn lock_db(&self) -> MutexGuard<'_, RootDatabase> {
        self.db.lock().await
    }
}
