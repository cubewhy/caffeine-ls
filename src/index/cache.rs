use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tracing::{debug, warn};

use crate::index::IndexedArchiveData;
use crate::index::store::{
    ArtifactId, ArtifactKind, ArtifactSource, ArtifactStore, LmdbIndexStore, StoredArtifact,
    StoredArtifactArchive, fingerprint_archive_data, shared_store_root,
};

pub fn cache_dir() -> Option<PathBuf> {
    shared_store_root()
}

pub(crate) fn shared_store() -> Option<&'static LmdbIndexStore> {
    static STORE: OnceLock<Option<LmdbIndexStore>> = OnceLock::new();

    STORE
        .get_or_init(|| {
            let root = shared_store_root()?;
            match LmdbIndexStore::open(&root) {
                Ok(store) => Some(store),
                Err(err) => {
                    warn!(path = %root.display(), error = %err, "failed to open index store");
                    None
                }
            }
        })
        .as_ref()
}

pub fn load_cached(source_path: &Path) -> Option<IndexedArchiveData> {
    load_cached_artifact(source_path).map(|artifact| artifact.data)
}

pub fn load_cached_artifact(source_path: &Path) -> Option<StoredArtifact> {
    let source = ArtifactSource::from_path(source_path, detect_artifact_kind(source_path)).ok()?;
    let store = shared_store()?;
    let loaded = store.load_artifact(&source).ok()?;
    if let Some(artifact) = loaded {
        debug!(
            path = %source_path.display(),
            kind = ?artifact.metadata.kind,
            class_count = artifact.data.classes.len(),
            module_count = artifact.data.modules.len(),
            "loaded artifact from LMDB index store"
        );
        return Some(artifact);
    }
    None
}

pub fn save_cache(source_path: &Path, data: &IndexedArchiveData) {
    let _ = save_cached_artifact(source_path, data);
}

pub fn save_cached_artifact(
    source_path: &Path,
    data: &IndexedArchiveData,
) -> Option<StoredArtifact> {
    let store = shared_store()?;
    let Ok(source) = ArtifactSource::from_path(source_path, detect_artifact_kind(source_path))
    else {
        return None;
    };

    match store.store_artifact(&source, data) {
        Ok(artifact) => Some(artifact),
        Err(err) => {
            warn!(
                path = %source_path.display(),
                error = %err,
                "failed to persist artifact into LMDB index store"
            );
            None
        }
    }
}

pub fn load_artifact_by_id(id: ArtifactId) -> Option<StoredArtifact> {
    let store = shared_store()?;
    match store.load_artifact_by_id(id) {
        Ok(artifact) => artifact,
        Err(err) => {
            warn!(artifact_id = id.0, error = %err, "failed to load artifact by id");
            None
        }
    }
}

pub fn load_artifact_archive_by_id(id: ArtifactId) -> Option<StoredArtifactArchive> {
    let store = shared_store()?;
    match store.load_artifact_archive_by_id(id) {
        Ok(artifact) => artifact,
        Err(err) => {
            warn!(
                artifact_id = id.0,
                error = %err,
                "failed to load artifact archive by id"
            );
            None
        }
    }
}

pub fn store_generated_artifact(
    source_name: &str,
    kind: ArtifactKind,
    data: &IndexedArchiveData,
) -> Option<StoredArtifact> {
    let store = shared_store()?;
    let fingerprint = match fingerprint_archive_data(data) {
        Ok(fingerprint) => fingerprint,
        Err(err) => {
            warn!(
                source = source_name,
                error = %err,
                "failed to fingerprint generated artifact payload"
            );
            return None;
        }
    };
    let source = ArtifactSource::synthetic(source_name, kind, fingerprint);

    match store.store_artifact(&source, data) {
        Ok(artifact) => Some(artifact),
        Err(err) => {
            warn!(
                source = source_name,
                kind = ?kind,
                error = %err,
                "failed to persist generated artifact into LMDB index store"
            );
            None
        }
    }
}

fn detect_artifact_kind(path: &Path) -> ArtifactKind {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("modules") => ArtifactKind::JdkModulesImage,
        Some("src.zip") => ArtifactKind::SourceZip,
        _ if path.extension().is_some_and(|ext| ext == "jar") => ArtifactKind::Jar,
        _ => ArtifactKind::Unknown,
    }
}
