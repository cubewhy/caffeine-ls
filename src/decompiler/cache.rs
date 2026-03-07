use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::Context;

pub struct DecompilerCache {
    pub root: PathBuf,
}

impl DecompilerCache {
    pub fn new(root: PathBuf) -> Self {
        if !root.exists() {
            std::fs::create_dir_all(&root).ok();
        }
        Self { root }
    }

    /// Generate a unique hash path based on bytecode
    pub fn resolve(&self, internal_name: &str, bytes: &[u8]) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        // Add the class name as a salt to prevent conflicts between classes with the same name in different packages (although internal_name already includes the package name).
        internal_name.hash(&mut hasher);
        bytes.hash(&mut hasher);
        let hash = hasher.finish();

        // {root}/{internal_name}/{simple_name}__{hash}.java
        let folder = self.root.join(internal_name);
        let simple_name = internal_name
            .rsplit_once("/")
            .unwrap_or(("", internal_name))
            .1;

        if !folder.exists() {
            std::fs::create_dir_all(&folder).ok();
        }

        folder.join(format!("{simple_name}__{:08x}.java", hash))
    }

    pub fn cleanup_stale(&self, internal_name: &str, current_file: &Path) {
        let folder = self.root.join(internal_name);
        if let Ok(entries) = std::fs::read_dir(folder) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path != current_file {
                    tracing::debug!(stale_file = ?path, "Cleaning up old cache version");
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}
