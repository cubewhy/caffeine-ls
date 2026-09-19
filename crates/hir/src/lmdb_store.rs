//! Persistent LMDB-backed cache for library stubs.
//!
//! A single LMDB environment (opened at `<config cache dir>/stubs/vN`, see
//! [`crate::db::enable_persistent_stub_cache`]) holds every library. The unnamed
//! database uses fixed 13-byte big-endian composite keys so that all entries
//! of one library form a contiguous, prefix-deletable range:
//!
//! ```text
//! key = [library_id: u64 BE][kind: u8][record_index: u32 BE]
//! ```
//!
//! * kind `KIND_META` (`index = 0`): the per-library metadata blob — the
//!   string table, the tier-1 name index rows and the last-accessed stamp.
//!   It is small (a few MB for the whole JDK) and is loaded eagerly on cold
//!   start.
//! * kind `KIND_RECORD`: one postcard blob with the full member stubs of a
//!   class or module, read on demand through a short read transaction.
//! * kind `KIND_SOURCE_INDEX`: the library's attached-source layout — the
//!   archive entry each top-level type is declared in — so a session never
//!   re-reads the archive's central directory it has read before.
//! * kind `KIND_PARAMS`: the parameter names of one library *member*, as a
//!   session that could read the member's declaring source resolved them.
//!   Fixed 17-byte keys (`[id][kind][hash]`) keep every entry of a library in
//!   one prefix-deletable range like the others.
//!
//! The two latter kinds are keyed on the *identity of the sources* they answer
//! for ([`SourcesStamp`]: the archive's path with its length and modification
//! time, or "no sources at all"), so a rebuilt archive — or a library that
//! gains sources it did not have — misses instead of answering from a layout
//! the session no longer reads. They are what keeps a cold session's inlay
//! hints off the disk rather than re-deriving every name from the archives.
//!
//! Writes happen in a single transaction per library, so a crash mid-write
//! never leaves a partially written library behind. Reads use short-lived
//! transactions and never block writers.

use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use heed::{Database, Env, EnvOpenOptions, types::Bytes};
use parking_lot::Mutex;
use postcard::{from_bytes, to_allocvec};
use rustc_hash::{FxHashSet, FxHasher};
use serde::{Deserialize, Serialize};
use vfs::AbsPath;

use crate::{db::LibraryId, stubs::DiskClassOrModuleRecord};

/// Version of the on-disk layout; bumped on incompatible changes. Also
/// selects the cache directory (`stubs/v{N}`).
///
/// `6`: attached-source indexes include Kotlin classifiers; Java-only layouts
/// and member-name misses derived from them must be reindexed.
pub const CACHE_FORMAT_VERSION: u32 = 6;

/// Libraries untouched for this long are eligible for pruning when they are
/// no longer registered by the running session.
pub const STALE_LIBRARY_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

const KIND_META: u8 = 1;
const KIND_RECORD: u8 = 2;
const KIND_SOURCE_INDEX: u8 = 3;
const KIND_PARAMS: u8 = 4;

const KEY_LEN: usize = 13; // u64 id + u8 kind + u32 index
/// The key length of the records that are keyed by a hash of what they answer
/// for ([`SourcesStamp`], and for [`ParamsBlob`] the member too).
const HASHED_KEY_LEN: usize = 17; // u64 id + u8 kind + u64 hash

/// Virtual size of the memory map. LMDB requires an upper bound up front;
/// the file itself only grows with actual data (sparse on Linux).
const MAP_SIZE: usize = 8 * 1024 * 1024 * 1024;

/// Per-library metadata stored under `KIND_META`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetaBlob {
    /// Layout version of the encoded payload, for belt-and-braces checking
    /// beyond the versioned directory name.
    pub format_version: u32,
    /// Per-library string table. All `u32` names below are indices into
    /// this table.
    pub strings: Vec<String>,
    pub entries: Vec<DiskClassEntry>,
    pub modules: Vec<DiskModuleEntry>,
    /// Unix timestamp (seconds) of the last session that used this entry,
    /// refreshed on load; drives stale-entry pruning.
    pub last_accessed: u64,
}

impl MetaBlob {
    /// Builds a fresh meta blob with the current format version and
    /// `last_accessed` stamp.
    pub fn new(
        strings: Vec<String>,
        entries: Vec<DiskClassEntry>,
        modules: Vec<DiskModuleEntry>,
    ) -> Self {
        Self {
            format_version: CACHE_FORMAT_VERSION,
            strings,
            entries,
            modules,
            last_accessed: unix_now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskClassEntry {
    /// FQN, string-table index.
    pub name: u32,
    /// Package name (possibly empty), string-table index.
    pub package: u32,
    pub kind: crate::stubs::ClassKind,
    pub flags: u16,
    pub super_class: Option<u32>,
    pub interfaces: Vec<u32>,
    /// JPMS module owning this class (string-table index), if modular.
    pub module: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskModuleEntry {
    pub name: u32,
    pub flags: u16,
    pub version: Option<u32>,
}

/// The identity of a library's attached sources: the archive's path with its
/// length and modification time, or a stamp that names none when the library
/// ships no sources at all.
///
/// Everything derived from an archive is cached under it, so a rebuilt archive
/// — or a library that gains sources it did not have — misses rather than
/// answering for a layout the session no longer reads. The path is part of the
/// identity because a library can be re-pointed at another archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcesStamp {
    /// `None` for a library that ships no sources.
    path: Option<String>,
    len: u64,
    mtime: u64,
    /// Whether a decompiler is configured for the library: its output is
    /// another declaring view a member's names can be read from, so an answer
    /// that says "this member records none" is only stable while this stays
    /// `false` for a library that ships no sources.
    decompiles: bool,
}

impl SourcesStamp {
    /// The stamp of `archive` (or of a library that ships none, for `None`),
    /// with whether `decompiles` is configured for the library. An archive
    /// whose metadata cannot be read is stamped source-less: it can answer for
    /// nothing this session, and a session that can read it stamps itself.
    pub fn of(archive: Option<&AbsPath>, decompiles: bool) -> Self {
        let mut stamp = Self {
            path: None,
            len: 0,
            mtime: 0,
            decompiles,
        };
        let Some(archive) = archive else {
            return stamp;
        };
        if let Ok(meta) = std::fs::metadata(archive) {
            stamp.path = Some(archive.as_str().to_owned());
            stamp.len = meta.len();
            stamp.mtime = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|age| age.as_secs())
                .unwrap_or_default();
        }
        stamp
    }

    fn hash(&self) -> u64 {
        let mut hasher = FxHasher::default();
        self.path.hash(&mut hasher);
        self.len.hash(&mut hasher);
        self.mtime.hash(&mut hasher);
        self.decompiles.hash(&mut hasher);
        hasher.finish()
    }
}

/// The library's attached-source layout: the archive entry each top-level type
/// is declared in, with the archive it was read from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceIndexBlob {
    /// Layout version of the encoded payload, for belt-and-braces checking
    /// beyond the versioned directory name.
    pub format_version: u32,
    pub stamp: SourcesStamp,
    /// `(top-level type name, archive entry)` pairs, sorted so that an
    /// unchanged index encodes to unchanged bytes.
    pub entries: Vec<(String, String)>,
}

/// The answer one library *member* was resolved to: the parameter names its
/// declaring source records, or the fact that it records none.
///
/// The key is a hash of the member and the [`SourcesStamp`] it answers for, so
/// an entry is only ever read back for the member it was written for; the
/// fields are carried so that a hash collision is *detected* and answered as a
/// miss rather than as another member's names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamsBlob {
    pub format_version: u32,
    pub class: String,
    pub method: String,
    pub descriptor: String,
    /// The names, in declaration order, or `None` when the member records none
    /// — its library ships no sources, or its source does not declare it.
    pub names: Option<Vec<String>>,
}

struct StoreInner {
    env: Env,
    db: Database<Bytes, Bytes>,
}

/// Handle to the persistent stub cache.
///
/// The store starts disabled and must be pointed at a directory once via
/// [`StubStore::open_at`]; the environment is created lazily on first use,
/// so constructing or cloning a store never touches the filesystem until
/// then. Every method degrades gracefully to a no-op when the store is
/// disabled or failed to open.
///
/// Cheap to clone: all clones share one environment.
#[derive(Clone, Default)]
pub struct StubStore {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    /// `None` = not opened yet; `Some(None)` = disabled / failed.
    inner: OnceLock<Option<Arc<StoreInner>>>,
    /// Where to open the environment; `None` keeps the store memory-only.
    dir: Mutex<Option<PathBuf>>,
}

impl std::fmt::Debug for StubStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.shared.inner.get() {
            Some(Some(_)) => f.write_str("StubStore(open)"),
            Some(None) => f.write_str("StubStore(disabled)"),
            None => f.write_str("StubStore(unopened)"),
        }
    }
}

impl StubStore {
    /// Points the store at `dir`. Must be called before the first use; later
    /// calls are ignored (the environment is already decided).
    pub fn open_at(&self, dir: PathBuf) {
        if self.shared.inner.get().is_some() {
            tracing::debug!("stub cache already initialized; ignoring new cache dir");
            return;
        }
        *self.shared.dir.lock() = Some(dir);
    }

    /// Whether persistence is available (used by tests).
    pub fn is_enabled(&self) -> bool {
        self.ensure_open().is_some()
    }

    fn ensure_open(&self) -> Option<&Arc<StoreInner>> {
        self.shared
            .inner
            .get_or_init(|| {
                let dir = self.shared.dir.lock().clone()?;
                match Self::try_open(&dir) {
                    Ok(inner) => Some(inner),
                    Err(err) => {
                        tracing::error!(
                            dir = %dir.display(),
                            "failed to open stub cache; continuing without persistent tier-2: {err:#}"
                        );
                        None
                    }
                }
            })
            .as_ref()
    }

    fn try_open(dir: &Path) -> anyhow::Result<Arc<StoreInner>> {
        std::fs::create_dir_all(dir)?;
        // SAFETY: the environment is used from multiple threads but never
        // across fork(); all accesses go through short-lived transactions.
        let env = unsafe { EnvOpenOptions::new().map_size(MAP_SIZE).open(dir) }?;
        let mut wtxn = env.write_txn()?;
        let db = env.create_database::<Bytes, Bytes>(&mut wtxn, None)?;
        wtxn.commit()?;
        Ok(Arc::new(StoreInner { env, db }))
    }

    /// Atomically writes (or replaces) the full cache entry of `library`:
    /// the meta blob plus one record blob per class/module.
    pub fn write_library(
        &self,
        library: LibraryId,
        meta: &MetaBlob,
        records: &[DiskClassOrModuleRecord],
    ) -> anyhow::Result<()> {
        let Some(store) = self.ensure_open() else {
            anyhow::bail!("stub cache is disabled");
        };
        let mut wtxn = store.env.write_txn()?;
        // Replace the whole range so stale records from a previous build of
        // the same library cannot survive a rebuild with fewer entries.
        delete_library_range(&store.db, &mut wtxn, library)?;
        store.db.put(
            &mut wtxn,
            &meta_key(library),
            &to_allocvec(meta)
                .map_err(|err| anyhow::anyhow!("failed to serialize meta blob: {err}"))?,
        )?;
        for (idx, record) in records.iter().enumerate() {
            let idx = u32::try_from(idx).expect("record count fits u32");
            let payload = to_allocvec(record)
                .map_err(|err| anyhow::anyhow!("failed to serialize stub record {idx}: {err}"))?;
            store
                .db
                .put(&mut wtxn, &record_key(library, idx), &payload)?;
        }
        wtxn.commit()?;
        Ok(())
    }

    /// Loads and decodes the meta blob of `library`; `None` when absent,
    /// corrupt or written by another format version.
    pub fn read_meta(&self, library: LibraryId) -> Option<MetaBlob> {
        let store = self.ensure_open()?;
        let rtxn = store.env.read_txn().ok()?;
        let bytes = store.db.get(&rtxn, &meta_key(library)).ok()??;
        decode_meta(bytes).ok()
    }

    /// Runs `f` on the raw bytes of record `idx` of `library`, inside a
    /// short-lived read transaction. `None` when the store is unavailable,
    /// there is no such record, or `f` returns `None`.
    pub fn with_record_bytes<R>(
        &self,
        library: LibraryId,
        idx: u32,
        f: impl FnOnce(&[u8]) -> Option<R>,
    ) -> Option<R> {
        let store = self.ensure_open()?;
        let rtxn = store.env.read_txn().ok()?;
        let bytes = store.db.get(&rtxn, &record_key(library, idx)).ok()??;
        f(bytes)
    }

    /// Refreshes `last_accessed` for each given library that exists in the
    /// store (single write transaction; silently skipped when the store is
    /// unavailable or its writer lock is held elsewhere).
    pub fn touch_libraries(&self, libraries: impl IntoIterator<Item = LibraryId>) {
        let Some(store) = self.ensure_open() else {
            return;
        };
        let Ok(mut wtxn) = store.env.write_txn() else {
            return;
        };
        let now = unix_now();
        for library in libraries {
            let Some(bytes) = store.db.get(&wtxn, &meta_key(library)).ok().flatten() else {
                continue;
            };
            let Ok(meta) = decode_meta(bytes) else {
                continue;
            };
            let mut updated = meta.clone();
            updated.last_accessed = now;
            if let Ok(payload) = to_allocvec(&updated) {
                let _ = store.db.put(&mut wtxn, &meta_key(library), &payload);
            }
        }
        wtxn.commit().ok();
    }

    /// Deletes every cache entry whose library is not in `live` and whose
    /// `last_accessed` stamp is older than [`STALE_LIBRARY_TTL`], plus every
    /// unregistered library with a corrupt meta blob. Returns the number of
    /// pruned libraries. Best-effort: skipped entirely when the store is
    /// unavailable or its writer lock is held elsewhere.
    pub fn prune_stale(&self, live: &FxHashSet<LibraryId>) -> usize {
        let Some(store) = self.ensure_open() else {
            return 0;
        };
        let cutoff = unix_now().saturating_sub(STALE_LIBRARY_TTL.as_secs());
        let Ok(rtxn) = store.env.read_txn() else {
            return 0;
        };
        let mut seen: Option<LibraryId> = None;
        let mut stale = Vec::new();
        let Ok(mut iter) = store.db.iter(&rtxn) else {
            return 0;
        };
        while let Some(Ok((key, value))) = iter.next() {
            if key.len() != KEY_LEN || key[8] != KIND_META {
                continue;
            }
            let library = LibraryId(u64::from_be_bytes(key[..8].try_into().unwrap()));
            if seen == Some(library) {
                continue;
            }
            seen = Some(library);
            // Keys sort by [id][kind][index], so the first KIND_META entry
            // of a library is its meta blob.
            let prune = match decode_meta(value) {
                Ok(meta) => meta.last_accessed < cutoff && !live.contains(&library),
                Err(()) => !live.contains(&library),
            };
            if prune {
                stale.push(library);
            }
        }
        drop(iter);
        drop(rtxn);

        if stale.is_empty() {
            return 0;
        }
        let Ok(mut wtxn) = store.env.write_txn() else {
            return 0;
        };
        let mut pruned = 0;
        for library in stale {
            match delete_library_range(&store.db, &mut wtxn, library) {
                Ok(count) if count > 0 => {
                    tracing::debug!(
                        library = %library,
                        entries = count,
                        "pruned stale stub cache entry"
                    );
                    pruned += 1;
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(library = %library, "failed to prune stub cache entry: {err:#}");
                    break;
                }
            }
        }
        wtxn.commit().ok();
        pruned
    }

    /// Deletes all data of one library (best-effort, single transaction).
    pub fn clear_library(&self, library: LibraryId) {
        let Some(store) = self.ensure_open() else {
            return;
        };
        if let Ok(mut wtxn) = store.env.write_txn()
            && delete_library_range(&store.db, &mut wtxn, library).is_ok()
        {
            wtxn.commit().ok();
        }
    }

    /// The attached-source layout of `library` as a previous session indexed
    /// it, `None` when it was never indexed or was indexed from different
    /// sources ([`SourcesStamp`]).
    pub fn read_source_index(
        &self,
        library: LibraryId,
        stamp: &SourcesStamp,
    ) -> Option<SourceIndexBlob> {
        let store = self.ensure_open()?;
        let rtxn = store.env.read_txn().ok()?;
        let bytes = store
            .db
            .get(&rtxn, &hashed_key(library, KIND_SOURCE_INDEX, stamp.hash()))
            .ok()??;
        decode(bytes)
    }

    /// Stores the attached-source layout of `library`, replacing any layout
    /// cached for another stamp of it.
    pub fn write_source_index(
        &self,
        library: LibraryId,
        blob: &SourceIndexBlob,
    ) -> anyhow::Result<()> {
        let Some(store) = self.ensure_open() else {
            anyhow::bail!("stub cache is disabled");
        };
        let mut wtxn = store.env.write_txn()?;
        // A library has one layout at a time: drop the ones cached for other
        // stamps so a re-indexed archive does not accumulate entries.
        delete_hashed_kind(&store.db, &mut wtxn, library, KIND_SOURCE_INDEX)?;
        let payload = to_allocvec(blob)
            .map_err(|err| anyhow::anyhow!("failed to serialize source index blob: {err}"))?;
        store.db.put(
            &mut wtxn,
            &hashed_key(library, KIND_SOURCE_INDEX, blob.stamp.hash()),
            &payload,
        )?;
        wtxn.commit()?;
        Ok(())
    }

    /// The parameter names a previous session resolved for the member
    /// `(class, method, descriptor)` of `library`, `None` when it resolved
    /// nothing for it — including when it resolved that the member records
    /// none (the returned blob's `names` is then `None`).
    pub fn read_params(
        &self,
        library: LibraryId,
        stamp: &SourcesStamp,
        class: &str,
        method: &str,
        descriptor: &str,
    ) -> Option<ParamsBlob> {
        let store = self.ensure_open()?;
        let rtxn = store.env.read_txn().ok()?;
        let bytes = store
            .db
            .get(
                &rtxn,
                &params_key(library, stamp, class, method, descriptor),
            )
            .ok()??;
        let blob: ParamsBlob = decode(bytes)?;
        // The key is a hash, so the member it answers for is checked before the
        // answer is used: a collision is a miss, never another member's names.
        (blob.class == class && blob.method == method && blob.descriptor == descriptor)
            .then_some(blob)
    }

    /// Stores the answer for one member of `library`.
    pub fn write_params(
        &self,
        library: LibraryId,
        stamp: &SourcesStamp,
        blob: &ParamsBlob,
    ) -> anyhow::Result<()> {
        let Some(store) = self.ensure_open() else {
            anyhow::bail!("stub cache is disabled");
        };
        let mut wtxn = store.env.write_txn()?;
        let payload = to_allocvec(blob)
            .map_err(|err| anyhow::anyhow!("failed to serialize member params blob: {err}"))?;
        store.db.put(
            &mut wtxn,
            &params_key(library, stamp, &blob.class, &blob.method, &blob.descriptor),
            &payload,
        )?;
        wtxn.commit()?;
        Ok(())
    }
}

fn decode_meta(bytes: &[u8]) -> Result<MetaBlob, ()> {
    let meta: MetaBlob = from_bytes(bytes).map_err(|_| ())?;
    if meta.format_version != CACHE_FORMAT_VERSION {
        return Err(());
    }
    Ok(meta)
}

/// Decodes a payload written by the current layout; `None` for a foreign or
/// corrupt one.
fn decode<T>(bytes: &[u8]) -> Option<T>
where
    T: serde::de::DeserializeOwned + HasFormatVersion,
{
    let blob: T = from_bytes(bytes).ok()?;
    (blob.format_version() == CACHE_FORMAT_VERSION).then_some(blob)
}

/// The layout version a cache payload carries.
trait HasFormatVersion {
    fn format_version(&self) -> u32;
}

impl HasFormatVersion for SourceIndexBlob {
    fn format_version(&self) -> u32 {
        self.format_version
    }
}

impl HasFormatVersion for ParamsBlob {
    fn format_version(&self) -> u32 {
        self.format_version
    }
}

fn delete_library_range(
    db: &Database<Bytes, Bytes>,
    wtxn: &mut heed::RwTxn<'_>,
    library: LibraryId,
) -> anyhow::Result<usize> {
    // BE ids make `[id, ..] .. [id+1, ..]` exactly the range of the
    // library's entries, including carry across kind/index bytes.
    let start = meta_key(library);
    let end = meta_key(LibraryId(library.0.wrapping_add(1)));
    let range = (
        std::ops::Bound::Included(start.as_slice()),
        std::ops::Bound::Excluded(end.as_slice()),
    );
    Ok(db.delete_range(wtxn, &range)?)
}

fn key(library: LibraryId, kind: u8, idx: u32) -> [u8; KEY_LEN] {
    let mut key = [0; KEY_LEN];
    key[..8].copy_from_slice(&library.0.to_be_bytes());
    key[8] = kind;
    key[9..].copy_from_slice(&idx.to_be_bytes());
    key
}

fn meta_key(library: LibraryId) -> [u8; KEY_LEN] {
    key(library, KIND_META, 0)
}

fn record_key(library: LibraryId, idx: u32) -> [u8; KEY_LEN] {
    key(library, KIND_RECORD, idx)
}

fn hashed_key(library: LibraryId, kind: u8, hash: u64) -> [u8; HASHED_KEY_LEN] {
    let mut key = [0; HASHED_KEY_LEN];
    key[..8].copy_from_slice(&library.0.to_be_bytes());
    key[8] = kind;
    key[9..].copy_from_slice(&hash.to_be_bytes());
    key
}

/// The key of one member's cached answer: the member and the sources it was
/// answered for, hashed — the blob it reads back carries the member, so a
/// collision answers as a miss.
fn params_key(
    library: LibraryId,
    stamp: &SourcesStamp,
    class: &str,
    method: &str,
    descriptor: &str,
) -> [u8; HASHED_KEY_LEN] {
    let mut hasher = FxHasher::default();
    class.hash(&mut hasher);
    method.hash(&mut hasher);
    descriptor.hash(&mut hasher);
    hashed_key(library, KIND_PARAMS, stamp.hash() ^ hasher.finish())
}

/// Deletes every record of one kind of a library, whatever its hashed stamp —
/// the records of the layout being replaced.
fn delete_hashed_kind(
    db: &Database<Bytes, Bytes>,
    wtxn: &mut heed::RwTxn<'_>,
    library: LibraryId,
    kind: u8,
) -> anyhow::Result<usize> {
    let start = hashed_key(library, kind, 0);
    // The range of one library's records of this kind: the id and kind bytes
    // fix the space, the hashed stamp ranges over all of it.
    let mut end = start;
    end[8] = kind + 1;
    let range = (
        std::ops::Bound::Included(start.as_slice()),
        std::ops::Bound::Excluded(end.as_slice()),
    );
    Ok(db.delete_range(wtxn, &range)?)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stubs::{ClassOrModuleStub, ModuleStub};

    fn temp_store() -> (tempfile::TempDir, StubStore) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StubStore::default();
        store.open_at(dir.path().to_owned());
        (dir, store)
    }

    fn sample_meta(last_accessed: u64) -> MetaBlob {
        MetaBlob {
            format_version: CACHE_FORMAT_VERSION,
            strings: vec!["java.lang.Object".to_owned()],
            entries: Vec::new(),
            modules: vec![DiskModuleEntry {
                name: 0,
                flags: 0x8000,
                version: None,
            }],
            last_accessed,
        }
    }

    fn sample_record() -> DiskClassOrModuleRecord {
        ClassOrModuleStub::Module(ModuleStub {
            name: 0,
            flags: 0x8000,
            version: None,
            requires: Vec::new(),
            exports: Vec::new(),
            opens: Vec::new(),
            uses: Vec::new(),
            provides: Vec::new(),
        })
    }

    #[test]
    fn disabled_store_is_a_no_op() {
        let store = StubStore::default();
        assert!(!store.is_enabled());
        assert!(store.read_meta(LibraryId(1)).is_none());
        assert_eq!(store.with_record_bytes(LibraryId(1), 0, |_| Some(())), None);
        store
            .write_library(LibraryId(1), &sample_meta(0), &[])
            .unwrap_err();
        let stamp = SourcesStamp::of(None, false);
        assert!(store.read_source_index(LibraryId(1), &stamp).is_none());
        assert!(
            store
                .read_params(LibraryId(1), &stamp, "A", "m", "()V")
                .is_none()
        );
        store
            .write_source_index(
                LibraryId(1),
                &SourceIndexBlob {
                    format_version: CACHE_FORMAT_VERSION,
                    stamp: stamp.clone(),
                    entries: Vec::new(),
                },
            )
            .unwrap_err();
        store
            .write_params(
                LibraryId(1),
                &stamp,
                &ParamsBlob {
                    format_version: CACHE_FORMAT_VERSION,
                    class: "A".to_owned(),
                    method: "m".to_owned(),
                    descriptor: "()V".to_owned(),
                    names: None,
                },
            )
            .unwrap_err();
        store.touch_libraries([LibraryId(1)]);
        assert_eq!(store.prune_stale(&FxHashSet::default()), 0);
        store.clear_library(LibraryId(1));
    }

    #[test]
    fn write_then_read_round_trip() {
        let (_dir, store) = temp_store();
        let library = LibraryId(0xdeadbeef);
        let meta = sample_meta(42);

        store
            .write_library(library, &meta, &[sample_record(), sample_record()])
            .unwrap();

        assert_eq!(store.read_meta(library).unwrap(), meta);
        let decoded = store
            .with_record_bytes(library, 1, |bytes| {
                from_bytes::<DiskClassOrModuleRecord>(bytes).ok()
            })
            .unwrap();
        assert_eq!(decoded, sample_record());
        assert!(store.with_record_bytes(library, 2, |_| Some(())).is_none());
    }

    #[test]
    fn rewrite_replaces_previous_records() {
        let (_dir, store) = temp_store();
        let library = LibraryId(7);

        store
            .write_library(
                library,
                &sample_meta(0),
                &[sample_record(), sample_record(), sample_record()],
            )
            .unwrap();
        store
            .write_library(library, &sample_meta(1), &[sample_record()])
            .unwrap();

        assert!(store.with_record_bytes(library, 0, |_| Some(())).is_some());
        assert!(store.with_record_bytes(library, 1, |_| Some(())).is_none());
    }

    /// The stamp of a file of `len` zero bytes: what distinguishes one archive
    /// from another is its path with its length and modification time.
    fn stamped(dir: &tempfile::TempDir, name: &str, len: usize) -> SourcesStamp {
        let path = dir.path().join(name);
        std::fs::write(&path, vec![0u8; len]).unwrap();
        SourcesStamp::of(Some(&vfs::AbsPathBuf::assert_utf8(path)), false)
    }

    fn source_index(stamp: &SourcesStamp) -> SourceIndexBlob {
        SourceIndexBlob {
            format_version: CACHE_FORMAT_VERSION,
            stamp: stamp.clone(),
            entries: vec![
                (
                    "com.example.Foo".to_owned(),
                    "com/example/Foo.java".to_owned(),
                ),
                (
                    "com.example.Bar".to_owned(),
                    "com/example/Bar.java".to_owned(),
                ),
            ],
        }
    }

    fn params(names: Option<&[&str]>) -> ParamsBlob {
        ParamsBlob {
            format_version: CACHE_FORMAT_VERSION,
            class: "com/example/Foo".to_owned(),
            method: "greet".to_owned(),
            descriptor: "(Ljava/lang/String;)V".to_owned(),
            names: names.map(|names| names.iter().map(|name| (*name).to_owned()).collect()),
        }
    }

    #[test]
    fn source_index_is_keyed_on_the_archive_it_was_read_from() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StubStore::default();
        store.open_at(dir.path().to_owned());
        let library = LibraryId(0xabcd);

        let stamp = stamped(&dir, "deps-sources.jar", 16);
        store
            .write_source_index(library, &source_index(&stamp))
            .unwrap();
        assert_eq!(
            store
                .read_source_index(library, &stamp)
                .unwrap()
                .entries
                .len(),
            2
        );

        // The same archive re-indexed after a rebuild is another archive: the
        // length (and the path) are part of what is keyed on.
        let rebuilt = stamped(&dir, "deps-sources.jar", 32);
        assert_ne!(rebuilt, stamp);
        assert!(store.read_source_index(library, &rebuilt).is_none());

        // And a re-index replaces the previous layout rather than accumulating
        // one per build.
        store
            .write_source_index(library, &source_index(&rebuilt))
            .unwrap();
        assert_eq!(
            store
                .read_source_index(library, &rebuilt)
                .unwrap()
                .entries
                .len(),
            2
        );
        let other = tempfile::TempDir::new().unwrap();
        assert!(
            store
                .read_source_index(library, &stamped(&other, "x.jar", 16))
                .is_none()
        );
    }

    #[test]
    fn java_only_source_index_is_a_cache_miss() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StubStore::default();
        store.open_at(dir.path().to_owned());
        let library = LibraryId(7);
        let stamp = stamped(&dir, "kotlin-sources.jar", 16);
        let mut old_index = source_index(&stamp);
        old_index.format_version = 5;
        store.write_source_index(library, &old_index).unwrap();

        assert!(store.read_source_index(library, &stamp).is_none());
        let current = source_index(&stamp);
        store.write_source_index(library, &current).unwrap();
        assert_eq!(store.read_source_index(library, &stamp), Some(current));
    }

    #[test]
    fn member_params_round_trip_including_the_records_none_answer() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StubStore::default();
        store.open_at(dir.path().to_owned());
        let library = LibraryId(5);
        let stamp = stamped(&dir, "deps-sources.jar", 8);

        let named = params(Some(&["count", "text"]));
        store.write_params(library, &stamp, &named).unwrap();
        assert_eq!(
            store
                .read_params(
                    library,
                    &stamp,
                    &named.class,
                    &named.method,
                    &named.descriptor
                )
                .unwrap()
                .names,
            Some(vec!["count".to_owned(), "text".to_owned()])
        );

        // A member resolved to *no* names is an answer too: it is what keeps a
        // source-less library from being probed again.
        let none = params(None);
        store.write_params(library, &stamp, &none).unwrap();
        assert_eq!(
            store
                .read_params(library, &stamp, &none.class, &none.method, "()V")
                .map(|blob| blob.names),
            None,
            "another member of the same class is not this one's answer"
        );
        store
            .write_params(
                library,
                &stamp,
                &ParamsBlob {
                    descriptor: "()V".to_owned(),
                    names: None,
                    ..none.clone()
                },
            )
            .unwrap();
        assert_eq!(
            store
                .read_params(library, &stamp, &none.class, &none.method, "()V")
                .unwrap()
                .names,
            None
        );

        // Another archive means another answer.
        let other = stamped(&dir, "other-sources.jar", 8);
        assert!(
            store
                .read_params(
                    library,
                    &other,
                    &named.class,
                    &named.method,
                    &named.descriptor
                )
                .is_none()
        );
    }

    #[test]
    fn reopen_sees_committed_records() {
        let dir = tempfile::TempDir::new().unwrap();
        let library = LibraryId(0xfeed);
        let stamp = stamped(&dir, "deps-sources.jar", 4);
        {
            let store = StubStore::default();
            store.open_at(dir.path().to_owned());
            store
                .write_source_index(library, &source_index(&stamp))
                .unwrap();
            let named = params(Some(&["count"]));
            store.write_params(library, &stamp, &named).unwrap();
        }
        let reopened = StubStore::default();
        reopened.open_at(dir.path().to_owned());
        assert_eq!(
            reopened
                .read_source_index(library, &stamp)
                .unwrap()
                .entries
                .len(),
            2
        );
        assert_eq!(
            reopened
                .read_params(
                    library,
                    &stamp,
                    "com/example/Foo",
                    "greet",
                    "(Ljava/lang/String;)V"
                )
                .unwrap()
                .names,
            Some(vec!["count".to_owned()])
        );
    }

    #[test]
    fn reopen_sees_committed_data() {
        let dir = tempfile::TempDir::new().unwrap();
        let library = LibraryId(0xcafe);
        {
            let store = StubStore::default();
            store.open_at(dir.path().to_owned());
            store
                .write_library(library, &sample_meta(5), &[sample_record()])
                .unwrap();
        }
        let reopened = StubStore::default();
        reopened.open_at(dir.path().to_owned());
        assert_eq!(reopened.read_meta(library).unwrap().last_accessed, 5);
    }

    #[test]
    fn touch_refreshes_last_accessed() {
        let (_dir, store) = temp_store();
        let library = LibraryId(9);
        store.write_library(library, &sample_meta(1), &[]).unwrap();

        store.touch_libraries([library, LibraryId(999)]);
        assert!(store.read_meta(library).unwrap().last_accessed > 1);
    }

    #[test]
    fn prune_removes_only_stale_unregistered_libraries() {
        let (_dir, store) = temp_store();
        let old = LibraryId(1);
        let fresh = LibraryId(2);
        let live_old = LibraryId(3);
        let corrupt = LibraryId(4);

        store
            .write_library(old, &sample_meta(0), &[sample_record()])
            .unwrap();
        let stamp = SourcesStamp::of(None, false);
        store
            .write_source_index(old, &source_index(&stamp))
            .unwrap();
        store
            .write_params(old, &stamp, &params(Some(&["count"])))
            .unwrap();
        store
            .write_library(fresh, &sample_meta(unix_now()), &[])
            .unwrap();
        store.write_library(live_old, &sample_meta(0), &[]).unwrap();
        // A library whose meta does not decode (wrong format version here).
        let mut broken = sample_meta(0);
        broken.format_version = 99;
        store
            .write_library(corrupt, &broken, &[sample_record()])
            .unwrap();

        let live = FxHashSet::from_iter([live_old]);
        assert_eq!(store.prune_stale(&live), 2);

        assert!(store.read_meta(old).is_none());
        assert!(store.read_meta(fresh).is_some());
        assert!(store.read_meta(live_old).is_some());
        assert!(store.read_meta(corrupt).is_none());
        // Its records are gone with it — the source layout and the members'
        // names included, which is what keeps the growth of the cache bounded
        // by the libraries the session actually uses.
        assert!(store.with_record_bytes(corrupt, 0, |_| Some(())).is_none());
        assert!(store.read_source_index(old, &stamp).is_none());
        assert!(
            store
                .read_params(
                    old,
                    &stamp,
                    "com/example/Foo",
                    "greet",
                    "(Ljava/lang/String;)V"
                )
                .is_none()
        );
    }

    #[test]
    fn clear_library_drops_everything() {
        let (_dir, store) = temp_store();
        let library = LibraryId(123);
        store
            .write_library(library, &sample_meta(0), &[sample_record()])
            .unwrap();
        let stamp = SourcesStamp::of(None, false);
        store
            .write_source_index(library, &source_index(&stamp))
            .unwrap();
        store
            .write_params(library, &stamp, &params(Some(&["count"])))
            .unwrap();

        store.clear_library(library);
        assert!(store.read_meta(library).is_none());
        assert!(store.with_record_bytes(library, 0, |_| Some(())).is_none());
        assert!(store.read_source_index(library, &stamp).is_none());
        assert!(
            store
                .read_params(
                    library,
                    &stamp,
                    "com/example/Foo",
                    "greet",
                    "(Ljava/lang/String;)V"
                )
                .is_none()
        );
    }

    #[test]
    fn clone_shares_the_open_environment() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StubStore::default();
        store.open_at(dir.path().to_owned());
        let clone = store.clone();

        clone
            .write_library(LibraryId(3), &sample_meta(0), &[])
            .unwrap();
        assert!(store.read_meta(LibraryId(3)).is_some());

        let unopened = StubStore::default();
        assert!(!unopened.clone().is_enabled());
    }

    #[test]
    fn key_layout_is_big_endian_prefix_ordered() {
        let low = meta_key(LibraryId(1));
        let high = meta_key(LibraryId(2));
        assert!(low < high);
        assert_eq!(low.len(), KEY_LEN);
        assert_eq!(low[8], KIND_META);
        assert_eq!(record_key(LibraryId(1), 5)[8], KIND_RECORD);
        // Record keys sort after the meta key of the same library...
        assert!(meta_key(LibraryId(1)) < record_key(LibraryId(1), 0));
        // ...but before any key of the next library.
        assert!(record_key(LibraryId(1), u32::MAX) < meta_key(LibraryId(2)));
    }
}
