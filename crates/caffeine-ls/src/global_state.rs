use arc_swap::ArcSwapOption;
use ra_ap_vfs::Vfs;
use tokio::sync::RwLock;

use crate::config::Config;

#[derive(Default)]
pub struct GlobalState {
    pub config: ArcSwapOption<Option<Config>>,
    pub vfs: RwLock<Vfs>,
}
