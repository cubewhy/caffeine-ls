use std::sync::{Arc, OnceLock};

use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use crate::build_integration::SourceRootId;
use crate::index::{
    ArtifactId, ArtifactReaderCache, ArtifactScopeReader, BucketIndex, ClasspathId, ModuleId,
    NameTable,
};

pub type AnalysisContextKey = (ModuleId, ClasspathId, Option<SourceRootId>);

#[derive(Clone)]
pub enum ScopeLayer {
    Overlay(Arc<BucketIndex>),
    Artifact(ArtifactId),
}

/// Immutable scope topology for one analysis context.
///
/// This is intentionally compact: it captures only visibility order and
/// reusable declaration summaries. Expensive semantic joins live in
/// request-scoped query layers like `IndexView`.
pub struct ScopeSnapshot {
    key: AnalysisContextKey,
    layers: SmallVec<ScopeLayer, 8>,
    jar_paths: Vec<Arc<str>>,
    name_table: OnceLock<Arc<NameTable>>,
    artifact_readers: Arc<ArtifactReaderCache>,
}

impl ScopeSnapshot {
    pub fn new(
        module_id: ModuleId,
        classpath: ClasspathId,
        source_root: Option<SourceRootId>,
        layers: SmallVec<ScopeLayer, 8>,
        jar_paths: Vec<Arc<str>>,
        artifact_readers: Arc<ArtifactReaderCache>,
    ) -> Self {
        Self {
            key: (module_id, classpath, source_root),
            layers,
            jar_paths,
            name_table: OnceLock::new(),
            artifact_readers,
        }
    }

    pub fn from_layers(layers: SmallVec<Arc<BucketIndex>, 8>) -> Self {
        let layers = layers
            .into_iter()
            .map(ScopeLayer::Overlay)
            .collect::<SmallVec<ScopeLayer, 8>>();
        Self::new(
            ModuleId::ROOT,
            ClasspathId::Main,
            None,
            layers,
            Vec::new(),
            Arc::new(ArtifactReaderCache::default()),
        )
    }

    pub fn key(&self) -> AnalysisContextKey {
        self.key
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    pub fn layers(&self) -> &[ScopeLayer] {
        &self.layers
    }

    pub fn jar_paths(&self) -> &[Arc<str>] {
        &self.jar_paths
    }

    pub fn jar_count(&self) -> usize {
        self.jar_paths.len()
    }

    pub fn artifact_reader(&self, artifact_id: ArtifactId) -> Option<Arc<ArtifactScopeReader>> {
        self.artifact_readers.get(artifact_id)
    }

    pub fn with_prepended_overlay(&self, overlay: Arc<BucketIndex>) -> Self {
        let mut layers = SmallVec::with_capacity(self.layers.len() + 1);
        layers.push(ScopeLayer::Overlay(overlay));
        layers.extend(self.layers.iter().cloned());
        Self::new(
            self.key.0,
            self.key.1,
            self.key.2,
            layers,
            self.jar_paths.clone(),
            Arc::clone(&self.artifact_readers),
        )
    }

    pub fn build_name_table(&self) -> Arc<NameTable> {
        Arc::clone(self.name_table.get_or_init(|| {
            let mut names = Vec::new();
            let mut seen: FxHashSet<Arc<str>> = Default::default();
            for layer in &self.layers {
                let layer_names = match layer {
                    ScopeLayer::Overlay(bucket) => bucket.exact_match_keys(),
                    ScopeLayer::Artifact(artifact_id) => self
                        .artifact_reader(*artifact_id)
                        .map(|reader| reader.exact_match_keys())
                        .unwrap_or_default(),
                };
                for name in layer_names {
                    if seen.insert(Arc::clone(&name)) {
                        names.push(name);
                    }
                }
            }
            tracing::debug!(
                module = self.key.0.0,
                classpath = ?self.key.1,
                source_root = ?self.key.2.map(|id| id.0),
                layer_count = self.layers.len(),
                name_count = names.len(),
                phase = "scope_snapshot",
                "build NameTable from scope snapshot"
            );
            NameTable::from_names(names)
        }))
    }
}
