use arc_swap::ArcSwapOption;
use tokio::sync::RwLock;
use vfs::Vfs;

use crate::config::Config;

#[derive(Default)]
pub struct GlobalState {
    pub config: ArcSwapOption<Option<Config>>,
    pub vfs: RwLock<Vfs>,
}
