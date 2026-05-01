use base_db::Files;
use triomphe::Arc;

#[salsa::db]
pub struct RootDatabase {
    storage: salsa::Storage<Self>,
    files: Arc<Files>,
}

#[salsa::db]
impl salsa::Database for RootDatabase {}
