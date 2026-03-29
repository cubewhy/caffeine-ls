use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use heed::byteorder::NativeEndian;
use heed::types::{Bytes, Str, U64};
use heed::{Database, Env, EnvOpenOptions};

use crate::index::IndexedArchiveData;

use super::artifact::{
    ArtifactMetaV1, ArtifactPayloadV1, deserialize_meta, deserialize_payload, serialize_meta,
    serialize_payload,
};
use super::{
    ArtifactId, ArtifactMetadata, ArtifactSource, ArtifactStore, IndexStore, StoredArtifact,
    StoredArtifactArchive,
};

pub const INDEX_SCHEMA_VERSION: u32 = 1;

const DB_META: &str = "meta";
const DB_ARTIFACTS: &str = "artifacts";
const DB_ARTIFACT_BY_HASH: &str = "artifact_by_hash";
const DB_ARTIFACT_PAYLOADS: &str = "artifact_payloads";
const META_SCHEMA_VERSION: &str = "schema_version";
const META_NEXT_ARTIFACT_ID: &str = "next_artifact_id";
const DEFAULT_MAP_SIZE: usize = 1 << 30;

pub struct LmdbIndexStore {
    root: PathBuf,
    env: Env,
    meta: Database<Str, Bytes>,
    artifacts: Database<U64<NativeEndian>, Bytes>,
    artifact_by_hash: Database<Bytes, U64<NativeEndian>>,
    artifact_payloads: Database<U64<NativeEndian>, Bytes>,
}

impl LmdbIndexStore {
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("create index store directory {}", path.display()))?;

        let store = Self::open_unchecked(path)?;
        let mut wtxn = store.env.write_txn().context("open LMDB write txn")?;

        match store.read_schema_version(&wtxn)? {
            Some(version) if version != INDEX_SCHEMA_VERSION => {
                drop(wtxn);
                drop(store);
                std::fs::remove_dir_all(path)
                    .with_context(|| format!("remove incompatible store {}", path.display()))?;
                std::fs::create_dir_all(path)
                    .with_context(|| format!("recreate store {}", path.display()))?;
                let rebuilt = Self::open_unchecked(path)?;
                let mut rebuild_txn = rebuilt
                    .env
                    .write_txn()
                    .context("open LMDB write txn for rebuilt store")?;
                rebuilt.initialize_schema(&mut rebuild_txn)?;
                rebuild_txn
                    .commit()
                    .context("commit rebuilt store schema")?;
                Ok(rebuilt)
            }
            Some(_) => {
                drop(wtxn);
                Ok(store)
            }
            None => {
                store.initialize_schema(&mut wtxn)?;
                wtxn.commit().context("commit initial store schema")?;
                Ok(store)
            }
        }
    }

    fn open_unchecked(path: &Path) -> Result<Self> {
        let env = unsafe {
            EnvOpenOptions::new()
                .max_dbs(16)
                .map_size(DEFAULT_MAP_SIZE)
                .open(path)
        }
        .with_context(|| format!("open LMDB environment {}", path.display()))?;

        let mut wtxn = env.write_txn().context("open LMDB write txn")?;
        let meta = env
            .create_database::<Str, Bytes>(&mut wtxn, Some(DB_META))
            .context("create/open meta database")?;
        let artifacts = env
            .create_database::<U64<NativeEndian>, Bytes>(&mut wtxn, Some(DB_ARTIFACTS))
            .context("create/open artifacts database")?;
        let artifact_by_hash = env
            .create_database::<Bytes, U64<NativeEndian>>(&mut wtxn, Some(DB_ARTIFACT_BY_HASH))
            .context("create/open artifact_by_hash database")?;
        let artifact_payloads = env
            .create_database::<U64<NativeEndian>, Bytes>(&mut wtxn, Some(DB_ARTIFACT_PAYLOADS))
            .context("create/open artifact_payloads database")?;
        wtxn.commit().context("commit LMDB database creation")?;

        Ok(Self {
            root: path.to_path_buf(),
            env,
            meta,
            artifacts,
            artifact_by_hash,
            artifact_payloads,
        })
    }

    fn initialize_schema(&self, wtxn: &mut heed::RwTxn<'_>) -> Result<()> {
        self.meta
            .put(
                wtxn,
                META_SCHEMA_VERSION,
                &INDEX_SCHEMA_VERSION.to_le_bytes(),
            )
            .context("store schema version")?;
        self.meta
            .put(wtxn, META_NEXT_ARTIFACT_ID, &1u64.to_le_bytes())
            .context("store initial artifact id")?;
        Ok(())
    }

    fn read_schema_version(&self, txn: &heed::RwTxn<'_>) -> Result<Option<u32>> {
        self.meta
            .get(txn, META_SCHEMA_VERSION)
            .context("read schema version")?
            .map(decode_u32)
            .transpose()
    }

    fn next_artifact_id(&self, wtxn: &mut heed::RwTxn<'_>) -> Result<ArtifactId> {
        let Some(bytes) = self
            .meta
            .get(wtxn, META_NEXT_ARTIFACT_ID)
            .context("read next artifact id")?
        else {
            bail!("store metadata missing next_artifact_id");
        };

        let current = decode_u64(bytes)?;
        let next = current
            .checked_add(1)
            .context("artifact id counter overflowed")?;
        self.meta
            .put(wtxn, META_NEXT_ARTIFACT_ID, &next.to_le_bytes())
            .context("advance next artifact id")?;
        Ok(ArtifactId(current))
    }

    fn load_by_id_read(
        &self,
        txn: &heed::RoTxn<'_>,
        id: ArtifactId,
    ) -> Result<Option<StoredArtifact>> {
        let Some(meta_bytes) = self
            .artifacts
            .get(txn, &id.0)
            .context("load artifact metadata row")?
        else {
            return Ok(None);
        };
        let Some(payload_bytes) = self
            .artifact_payloads
            .get(txn, &id.0)
            .context("load artifact payload row")?
        else {
            return Ok(None);
        };

        let metadata = deserialize_meta(meta_bytes)?.into_public();
        let payload = deserialize_payload(payload_bytes)?;

        Ok(Some(StoredArtifact {
            metadata,
            data: IndexedArchiveData::from(payload),
        }))
    }

    fn load_archive_by_id_read(
        &self,
        txn: &heed::RoTxn<'_>,
        id: ArtifactId,
    ) -> Result<Option<StoredArtifactArchive>> {
        let Some(meta_bytes) = self
            .artifacts
            .get(txn, &id.0)
            .context("load artifact metadata row")?
        else {
            return Ok(None);
        };
        let Some(payload_bytes) = self
            .artifact_payloads
            .get(txn, &id.0)
            .context("load artifact payload row")?
        else {
            return Ok(None);
        };

        let metadata = deserialize_meta(meta_bytes)?.into_public();
        let payload = deserialize_payload(payload_bytes)?;

        Ok(Some(StoredArtifactArchive {
            metadata,
            classes: payload.classes,
            modules: payload.modules,
        }))
    }

    fn load_by_id_write(
        &self,
        txn: &heed::RwTxn<'_>,
        id: ArtifactId,
    ) -> Result<Option<StoredArtifact>> {
        let Some(meta_bytes) = self
            .artifacts
            .get(txn, &id.0)
            .context("load artifact metadata row")?
        else {
            return Ok(None);
        };
        let Some(payload_bytes) = self
            .artifact_payloads
            .get(txn, &id.0)
            .context("load artifact payload row")?
        else {
            return Ok(None);
        };

        let metadata = deserialize_meta(meta_bytes)?.into_public();
        let payload = deserialize_payload(payload_bytes)?;

        Ok(Some(StoredArtifact {
            metadata,
            data: IndexedArchiveData::from(payload),
        }))
    }
}

impl IndexStore for LmdbIndexStore {
    fn schema_version(&self) -> u32 {
        INDEX_SCHEMA_VERSION
    }

    fn root_path(&self) -> &Path {
        &self.root
    }
}

impl ArtifactStore for LmdbIndexStore {
    fn load_artifact(&self, source: &ArtifactSource) -> Result<Option<StoredArtifact>> {
        let rtxn = self.env.read_txn().context("open LMDB read txn")?;
        let key = encode_hash_key(source);
        let artifact_id = self
            .artifact_by_hash
            .get(&rtxn, key.as_slice())
            .context("lookup artifact by hash")?
            .map(ArtifactId);

        match artifact_id {
            Some(id) => self.load_by_id_read(&rtxn, id),
            None => Ok(None),
        }
    }

    fn load_artifact_archive(
        &self,
        source: &ArtifactSource,
    ) -> Result<Option<StoredArtifactArchive>> {
        let rtxn = self.env.read_txn().context("open LMDB read txn")?;
        let key = encode_hash_key(source);
        let artifact_id = self
            .artifact_by_hash
            .get(&rtxn, key.as_slice())
            .context("lookup artifact by hash")?
            .map(ArtifactId);

        match artifact_id {
            Some(id) => self.load_archive_by_id_read(&rtxn, id),
            None => Ok(None),
        }
    }

    fn load_artifact_by_id(&self, id: ArtifactId) -> Result<Option<StoredArtifact>> {
        let rtxn = self.env.read_txn().context("open LMDB read txn")?;
        self.load_by_id_read(&rtxn, id)
    }

    fn load_artifact_archive_by_id(&self, id: ArtifactId) -> Result<Option<StoredArtifactArchive>> {
        let rtxn = self.env.read_txn().context("open LMDB read txn")?;
        self.load_archive_by_id_read(&rtxn, id)
    }

    fn store_artifact(
        &self,
        source: &ArtifactSource,
        data: &IndexedArchiveData,
    ) -> Result<StoredArtifact> {
        let key = encode_hash_key(source);
        let mut wtxn = self.env.write_txn().context("open LMDB write txn")?;

        if let Some(existing_id) = self
            .artifact_by_hash
            .get(&wtxn, key.as_slice())
            .context("lookup existing artifact by hash")?
            .map(ArtifactId)
            && let Some(existing) = self.load_by_id_write(&wtxn, existing_id)?
        {
            wtxn.commit().context("commit LMDB dedupe transaction")?;
            return Ok(existing);
        }

        let id = self.next_artifact_id(&mut wtxn)?;
        let metadata = ArtifactMetadata {
            id,
            kind: source.kind,
            source_path: source.source_path.to_string_lossy().into_owned(),
            content_hash: source.fingerprint.content_hash,
            byte_len: source.fingerprint.byte_len,
            stored_at_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        let meta_row = ArtifactMetaV1::from_public(&metadata);
        let payload_row = ArtifactPayloadV1::from(data);
        let meta_bytes = serialize_meta(&meta_row)?;
        let payload_bytes = serialize_payload(&payload_row)?;

        self.artifacts
            .put(&mut wtxn, &id.0, meta_bytes.as_slice())
            .context("write artifact metadata row")?;
        self.artifact_payloads
            .put(&mut wtxn, &id.0, payload_bytes.as_slice())
            .context("write artifact payload row")?;
        self.artifact_by_hash
            .put(&mut wtxn, key.as_slice(), &id.0)
            .context("write artifact hash index row")?;
        wtxn.commit().context("commit artifact write transaction")?;

        Ok(StoredArtifact {
            metadata,
            data: data.clone(),
        })
    }
}

pub fn shared_store_root() -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("java-analyzer")
            .join("index-v1")
            .join("shared"),
    )
}

fn encode_hash_key(source: &ArtifactSource) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0] = match source.kind {
        super::ArtifactKind::Jar => 1,
        super::ArtifactKind::JdkModulesImage => 2,
        super::ArtifactKind::SourceZip => 3,
        super::ArtifactKind::Unknown => 255,
    };
    out[1..].copy_from_slice(&source.fingerprint.content_hash.to_le_bytes());
    out
}

fn decode_u32(bytes: &[u8]) -> Result<u32> {
    let slice: [u8; 4] = bytes
        .try_into()
        .context("expected 4-byte little-endian integer")?;
    Ok(u32::from_le_bytes(slice))
}

fn decode_u64(bytes: &[u8]) -> Result<u64> {
    let slice: [u8; 8] = bytes
        .try_into()
        .context("expected 8-byte little-endian integer")?;
    Ok(u64::from_le_bytes(slice))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::index::{ClassMetadata, ClassOrigin, IndexedArchiveData};

    fn temp_store() -> (TempDir, LmdbIndexStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = LmdbIndexStore::open(dir.path()).expect("open store");
        (dir, store)
    }

    fn write_artifact_file(dir: &TempDir, name: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).expect("write artifact file");
        path
    }

    fn demo_archive() -> IndexedArchiveData {
        IndexedArchiveData {
            classes: vec![ClassMetadata {
                package: Some(Arc::from("demo")),
                name: Arc::from("Example"),
                internal_name: Arc::from("demo/Example"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Jar(Arc::from("file:///demo/example.jar")),
            }],
            modules: vec![],
        }
    }

    #[test]
    fn roundtrips_artifact_payloads_through_lmdb() {
        let (_dir, store) = temp_store();
        let artifact_dir = tempfile::tempdir().expect("artifact tempdir");
        let path = write_artifact_file(&artifact_dir, "demo.jar", b"jar-bytes");
        let source =
            super::super::ArtifactSource::from_path(&path, super::super::ArtifactKind::Jar)
                .expect("fingerprint artifact");
        let archive = demo_archive();

        let stored = store
            .store_artifact(&source, &archive)
            .expect("store artifact");
        let loaded = store
            .load_artifact(&source)
            .expect("load artifact")
            .expect("artifact present");

        assert_eq!(stored.metadata.id, loaded.metadata.id);
        assert_eq!(loaded.metadata.kind, super::super::ArtifactKind::Jar);
        assert_eq!(loaded.data, archive);
    }
}
