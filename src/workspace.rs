use anyhow::Result;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::Url;
use tracing::{error, info, warn};

use crate::index::{ClassMetadata, WorkspaceIndex, index_jar};
use crate::index::{IndexScope, ModuleId};
use document::DocumentStore;

pub mod document;

pub struct Workspace {
    pub documents: DocumentStore,
    pub index: Arc<RwLock<WorkspaceIndex>>,
    loaded_jars: RwLock<Vec<PathBuf>>,
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            documents: DocumentStore::new(),
            index: Arc::new(RwLock::new(WorkspaceIndex::new())),
            loaded_jars: RwLock::new(Vec::new()),
        }
    }

    pub fn scope_for_uri(&self, _uri: &Url) -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    pub async fn load_jar_async(&self, path: PathBuf) -> Result<()> {
        let path_clone = path.clone();
        let classes = tokio::task::spawn_blocking(move || {
            info!(path = %path_clone.display(), "indexing jar");
            index_jar(&path_clone)
        })
        .await??;

        info!(
            path = %path.display(),
            count = classes.len(),
            "jar indexed"
        );

        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        self.index.write().await.add_jar_classes(scope, classes);
        self.loaded_jars.write().await.push(path);
        Ok(())
    }

    /// Scan the directory and load all JAR files concurrently
    pub async fn load_jars_from_dir(&self, dir: PathBuf) {
        // let index_arc = Arc::clone(&self.index);

        let result = tokio::task::spawn_blocking(move || {
            let entries: Vec<PathBuf> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "jar"))
                .collect();

            info!(dir = %dir.display(), count = entries.len(), "found jars, parsing in parallel");

            let all_classes: Vec<ClassMetadata> = entries
                .par_iter()
                .flat_map(|jar| match index_jar(jar) {
                    Ok(classes) => {
                        info!(path = %jar.display(), count = classes.len(), "jar indexed");
                        classes
                    }
                    Err(e) => {
                        warn!(path = %jar.display(), error = %e, "failed");
                        vec![]
                    }
                })
                .collect();

            all_classes
        })
        .await;

        match result {
            Ok(classes) => {
                info!(total = classes.len(), "all jars parsed, writing to index");
                let scope = IndexScope {
                    module: ModuleId::ROOT,
                };
                self.index.write().await.add_jar_classes(scope, classes);
            }
            Err(e) => error!(error = %e, "jar indexing panicked"),
        }
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}
