//! Persistent LMDB-backed cache for library stubs.
//!
//! A single LMDB environment (opened at
//! `$XDG_CACHE_HOME/caffeine-ls/stubs/vN`) holds every library. The unnamed
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
//!
//! Writes happen in a single transaction per library, so a crash mid-write
//! never leaves a partially written library behind. Reads use short-lived
//! transactions and never block writers.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use heed::{Database, Env, EnvOpenOptions, types::Bytes};
use parking_lot::Mutex;
use postcard::{from_bytes, to_allocvec};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};

use crate::{db::LibraryId, stubs::DiskClassOrModuleRecord};

/// Version of the on-disk layout; bumped on incompatible changes. Also
/// selects the cache directory (`stubs/v{N}`).
pub const CACHE_FORMAT_VERSION: u32 = 1;

/// Libraries untouched for this long are eligible for pruning when they are
/// no longer registered by the running session.
pub const STALE_LIBRARY_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

const KIND_META: u8 = 1;
const KIND_RECORD: u8 = 2;

const KEY_LEN: usize = 13; // u64 id + u8 kind + u32 index

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

struct StoreInner {
    env: Env,
    db: Database<Bytes, Bytes>,
}

/// Handle to the persistent stub cache.
///
/// The store starts disabled and must be pointed at a directory once via
/// [`StubStore::open_at`] (or [`StubStore::open_default_cache_dir`]); the
/// environment is created lazily on first use, so constructing or cloning a
/// store never touches the filesystem until then. Every method degrades
/// gracefully to a no-op when the store is disabled or failed to open.
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

    /// Points the store at the platform default cache directory. Returns
    /// whether such a directory could be determined.
    pub fn open_default_cache_dir(&self) -> bool {
        match cache_dir() {
            Some(dir) => {
                self.open_at(dir);
                true
            }
            None => false,
        }
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
}

fn decode_meta(bytes: &[u8]) -> Result<MetaBlob, ()> {
    let meta: MetaBlob = from_bytes(bytes).map_err(|_| ())?;
    if meta.format_version != CACHE_FORMAT_VERSION {
        return Err(());
    }
    Ok(meta)
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Platform-specific cache directory for library stubs
/// (`$XDG_CACHE_HOME/caffeine-ls/stubs/vN`, falling back to `~/.cache` or
/// `%LOCALAPPDATA%`).
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))?;
    Some(
        base.join("caffeine-ls")
            .join("stubs")
            .join(format!("v{CACHE_FORMAT_VERSION}")),
    )
}

/// Removes leftover `{id}.names` / `{id}.stubs` files of the pre-LMDB v1
/// cache layout. Best-effort; called once when the persistent store opens.
pub fn remove_legacy_v1_files(cache_dir: &Path) {
    let Some(parent) = cache_dir.parent() else {
        return;
    };
    let legacy = parent.join(format!("v{}", CACHE_FORMAT_VERSION - 1));
    let Ok(entries) = std::fs::read_dir(&legacy) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let stem = name
            .strip_suffix(".names")
            .or_else(|| name.strip_suffix(".stubs"));
        let is_legacy_entry = stem
            .is_some_and(|stem| stem.len() == 16 && stem.chars().all(|c| c.is_ascii_hexdigit()));
        if is_legacy_entry && std::fs::remove_file(&path).is_ok() {
            tracing::debug!(file = %path.display(), "removed legacy v1 stub cache file");
        }
    }
    // Remove the emptied directory too (fails silently when non-empty).
    let _ = std::fs::remove_dir(&legacy);
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
        // Its records are gone with it.
        assert!(store.with_record_bytes(corrupt, 0, |_| Some(())).is_none());
    }

    #[test]
    fn clear_library_drops_everything() {
        let (_dir, store) = temp_store();
        let library = LibraryId(123);
        store
            .write_library(library, &sample_meta(0), &[sample_record()])
            .unwrap();

        store.clear_library(library);
        assert!(store.read_meta(library).is_none());
        assert!(store.with_record_bytes(library, 0, |_| Some(())).is_none());
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
