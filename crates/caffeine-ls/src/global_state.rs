use parking_lot::{Mutex, MutexGuard, RwLock, RwLockReadGuard};

use arc_swap::ArcSwapOption;
use ide_db::RootDatabase;
use triomphe::Arc;
use vfs::Vfs;

use crate::config::Config;

pub struct GlobalState {
    pub config: ArcSwapOption<Option<Config>>,
    pub vfs: Arc<RwLock<Vfs>>,
    pub db: Mutex<RootDatabase>,
}

impl GlobalState {
    pub fn get_vfs(&self) -> RwLockReadGuard<'_, Vfs> {
        self.vfs.read()
    }

    pub fn db_snapshot(&self) -> RootDatabase {
        let db = self.db.lock();
        db.clone()
    }

    pub fn lock_db(&self) -> MutexGuard<'_, RootDatabase> {
        self.db.lock()
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        let vfs = Arc::new(RwLock::new(Vfs::default()));
        Self {
            config: Default::default(),
            vfs: vfs.clone(),
            db: Mutex::new(RootDatabase::new(vfs)),
        }
    }
}
