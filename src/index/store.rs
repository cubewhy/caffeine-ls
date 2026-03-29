mod artifact;
mod ids;
mod lmdb;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::index::{ArchiveClassStub, IndexedArchiveData, IndexedJavaModule};

pub use ids::{ArtifactId, FileId, ModuleNodeId, StringId, SymbolId, WorkspaceId};
pub use lmdb::{INDEX_SCHEMA_VERSION, LmdbIndexStore, shared_store_root};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    Jar,
    JdkModulesImage,
    SourceZip,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactFingerprint {
    pub content_hash: u64,
    pub byte_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactSource {
    pub source_path: PathBuf,
    pub kind: ArtifactKind,
    pub fingerprint: ArtifactFingerprint,
}

impl ArtifactSource {
    pub fn from_path(path: &Path, kind: ArtifactKind) -> Result<Self> {
        Ok(Self {
            source_path: path.to_path_buf(),
            kind,
            fingerprint: fingerprint_path(path)?,
        })
    }

    pub fn synthetic(
        source_path: impl Into<PathBuf>,
        kind: ArtifactKind,
        fingerprint: ArtifactFingerprint,
    ) -> Self {
        Self {
            source_path: source_path.into(),
            kind,
            fingerprint,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactMetadata {
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    pub source_path: String,
    pub content_hash: u64,
    pub byte_len: u64,
    pub stored_at_unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredArtifact {
    pub metadata: ArtifactMetadata,
    pub data: IndexedArchiveData,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredArtifactArchive {
    pub metadata: ArtifactMetadata,
    pub classes: Vec<ArchiveClassStub>,
    pub modules: Vec<IndexedJavaModule>,
}

pub trait IndexStore: Send + Sync {
    fn schema_version(&self) -> u32;
    fn root_path(&self) -> &Path;
}

pub trait ArtifactStore: IndexStore {
    fn load_artifact(&self, source: &ArtifactSource) -> Result<Option<StoredArtifact>>;
    fn load_artifact_by_id(&self, id: ArtifactId) -> Result<Option<StoredArtifact>>;
    fn load_artifact_archive(
        &self,
        source: &ArtifactSource,
    ) -> Result<Option<StoredArtifactArchive>>;
    fn load_artifact_archive_by_id(&self, id: ArtifactId) -> Result<Option<StoredArtifactArchive>>;
    fn store_artifact(
        &self,
        source: &ArtifactSource,
        data: &IndexedArchiveData,
    ) -> Result<StoredArtifact>;
}

pub fn fingerprint_archive_data(data: &IndexedArchiveData) -> Result<ArtifactFingerprint> {
    artifact::fingerprint_payload(data)
}

fn fingerprint_path(path: &Path) -> Result<ArtifactFingerprint> {
    use std::hash::Hasher;
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut byte_len = 0u64;

    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.write(&buf[..read]);
        byte_len += read as u64;
    }

    Ok(ArtifactFingerprint {
        content_hash: hasher.finish(),
        byte_len,
    })
}
