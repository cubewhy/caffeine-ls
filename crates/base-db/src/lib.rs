use std::hash::BuildHasherDefault;

use dashmap::DashMap;
use rustc_hash::FxHasher;
use triomphe::Arc;

pub struct FileText(String);

pub struct Files {
    files: Arc<DashMap<vfs::FileId, FileText, BuildHasherDefault<FxHasher>>>,
}
