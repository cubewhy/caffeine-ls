use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
};

use dashmap::DashMap;
use heed::{
    Database, Env, EnvOpenOptions,
    types::{Bytes, Str, U32},
};
use lasso::ThreadedRodeo;
use syntax::{ClassStub, Symbol};
use triomphe::Arc;
use vfs::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LibraryId(pub u64);

impl LibraryId {
    /// Generate a unique ID for a JAR file based on its path and metadata
    pub fn from_jar_path(path: &Path) -> std::io::Result<Self> {
        let metadata = fs::metadata(path)?;
        let modified = metadata.modified()?;

        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);

        format!("{:?}", modified).hash(&mut hasher);

        Ok(Self(hasher.finish()))
    }

    /// Generates a unique ID for a mutable local workspace module.
    /// Hashes ONLY the absolute path. We do not hash the modified time because
    /// active files change constantly, and we handle those via the `ParseCache`.
    pub fn from_project_root(path: &Path) -> Self {
        let mut hasher = DefaultHasher::new();
        let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        "/\\".hash(&mut hasher);
        abs_path.hash(&mut hasher);

        Self(hasher.finish())
    }
}

// The ScopedSymbol is our universal key for both Memory and Disk
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopedSymbol {
    pub lib_id: LibraryId,
    pub symbol: Symbol,
}

/// Manages multi-layered persistent symbol indexing for JVM-family languages using LMDB (`heed`).
pub struct GlobalSymbolIndex {
    /// Root directory hosting global immutable caches for external `.jar` and JDK artifacts.
    global_cache_dir: PathBuf,

    /// LMDB environment mapping active local workspace source states.
    workspace_env: Env,

    /// Primary Workspace DB: Maps FQN (`&str`) to serialized `ClassStub` (`[u8]`).
    workspace_fqn_to_stub: Database<Str, Bytes>,

    /// Secondary Workspace DB: Maps `FileId` (`u32`) to its defined FQNs (`Vec<String>`).
    /// Crucial for sweeping stale stubs when a file is modified or deleted.
    workspace_file_to_fqns: Database<U32<heed::byteorder::NativeEndian>, Bytes>,

    /// Thread-safe active mapping of read-only attached dependencies (JARs/JDK).
    /// Prevents concurrent file locking issues, especially on Windows environments.
    attached_libraries: DashMap<LibraryId, (Env, Database<Str, Bytes>)>,
}

impl GlobalSymbolIndex {
    /// Initializes a new high-performance JVM symbol index.
    pub fn new(global_cache_dir: impl AsRef<Path>, project_root: &Path) -> Self {
        let global_dir = global_cache_dir.as_ref().to_path_buf();
        fs::create_dir_all(&global_dir).expect("Failed to create global cache directory");

        let local_db_dir = project_root.join(".caffeine");
        fs::create_dir_all(&local_db_dir).expect("Failed to create project-local storage");

        // Initialize the local mutable workspace workspace environment
        let workspace_env = unsafe {
            EnvOpenOptions::new()
                .map_size(1024 * 1024 * 1024) // 1 GB virtual address space allocation
                .max_dbs(3)
                .open(&local_db_dir)
                .expect("Failed to open Workspace LMDB environment")
        };

        // Open or create internal databases atomically within a transaction
        let mut wtxn = workspace_env
            .write_txn()
            .expect("Failed to open write transaction");
        let workspace_fqn_to_stub = workspace_env
            .create_database(&mut wtxn, Some("workspace_fqn_to_stub"))
            .expect("Failed to initialize FQN database");
        let workspace_file_to_fqns = workspace_env
            .create_database(&mut wtxn, Some("workspace_file_to_fqns"))
            .expect("Failed to initialize File mapping database");
        wtxn.commit()
            .expect("Failed to commit database initializations");

        Self {
            global_cache_dir: global_dir,
            workspace_env,
            workspace_fqn_to_stub,
            workspace_file_to_fqns,
            attached_libraries: DashMap::new(),
        }
    }

    /// Updates or increments all class stubs belonging to a local source file.
    /// Safely purges any previous classes that were eliminated in the latest modification.
    pub fn update_workspace_file(
        &self,
        rodeo: &ThreadedRodeo,
        file_id: FileId,
        stubs: Vec<ClassStub>,
    ) {
        let mut wtxn = self
            .workspace_env
            .write_txn()
            .expect("Failed to open write txn");
        let raw_file_id = file_id.0;

        // Step 1: Clean up previous stale symbols emitted by this file
        if let Some(old_bytes) = self
            .workspace_file_to_fqns
            .get(&wtxn, &raw_file_id)
            .unwrap()
            && let Ok(old_fqns) = postcard::from_bytes::<Vec<String>>(old_bytes)
        {
            for old_fqn in old_fqns {
                self.workspace_fqn_to_stub
                    .delete(&mut wtxn, &old_fqn)
                    .unwrap();
            }
        }

        // Shortcut: If file was cleared or contains no classes, prune tracking entries entirely
        if stubs.is_empty() {
            self.workspace_file_to_fqns
                .delete(&mut wtxn, &raw_file_id)
                .unwrap();
            wtxn.commit().unwrap();
            return;
        }

        // Step 2: Ingest the newly provided class stubs
        let mut tracking_fqns = Vec::with_capacity(stubs.len());
        for stub in stubs {
            let fqn_str = rodeo.resolve(&stub.name);
            tracking_fqns.push(fqn_str.to_string());
            let serialized_stub =
                postcard::to_allocvec(&stub).expect("Failed to serialize ClassStub");

            self.workspace_fqn_to_stub
                .put(&mut wtxn, fqn_str, &serialized_stub)
                .unwrap();
        }

        // Step 3: Refresh the reverse-lookup mapping index
        let serialized_tracking = postcard::to_allocvec(&tracking_fqns).unwrap();
        self.workspace_file_to_fqns
            .put(&mut wtxn, &raw_file_id, &serialized_tracking)
            .unwrap();

        wtxn.commit()
            .expect("Failed to commit workspace file update");
    }

    /// Complete removal of a workspace source file (e.g., file unlinked or deleted).
    pub fn remove_file(&self, file_id: FileId) {
        let mut wtxn = self
            .workspace_env
            .write_txn()
            .expect("Failed to open write txn");
        let raw_file_id = file_id.0;

        // Corrected from .remove() to sequential .get() and .delete() calls
        if let Some(old_bytes) = self
            .workspace_file_to_fqns
            .get(&wtxn, &raw_file_id)
            .unwrap()
        {
            if let Ok(old_fqns) = postcard::from_bytes::<Vec<String>>(old_bytes) {
                for old_fqn in old_fqns {
                    self.workspace_fqn_to_stub
                        .delete(&mut wtxn, &old_fqn)
                        .unwrap();
                }
            }
            self.workspace_file_to_fqns
                .delete(&mut wtxn, &raw_file_id)
                .unwrap();
        }
        wtxn.commit()
            .expect("Failed to commit file sweeping transaction");
    }

    /// Attaches an unalterable binary archive dependency into the server session.
    /// Deploys a Shadow-Write & Atomic Move pattern to maintain absolute consistency under concurrency.
    pub fn attach_library(
        &self,
        rodeo: &ThreadedRodeo,
        lib_id: LibraryId,
        jar_path: &Path,
        parse_factory: impl Fn(&Path) -> Vec<ClassStub>,
    ) {
        if self.attached_libraries.contains_key(&lib_id) {
            return; // Target compilation block is already actively mapped
        }

        let db_file_name = format!("{}.db", lib_id.0);
        let final_db_path = self.global_cache_dir.join(&db_file_name);

        if !final_db_path.exists() {
            // Shadow construction step to handle caching gaps safely without write locks
            let tmp_db_path = self.global_cache_dir.join(format!("{}.db.tmp", lib_id.0));
            let _ = fs::create_dir_all(&tmp_db_path);

            let tmp_env = unsafe {
                EnvOpenOptions::new()
                    .map_size(1024 * 1024 * 1024 * 4) // 4 GB dynamic allocation pool for heavy libraries/JDK
                    .max_dbs(1)
                    .open(&tmp_db_path)
                    .expect("Failed to spawn shadow compilation environment")
            };

            let mut wtxn = tmp_env.write_txn().unwrap();
            let tmp_db: Database<Str, Bytes> = tmp_env.create_database(&mut wtxn, None).unwrap();

            // Execute processing callback context via parsing thread pool
            let stubs = parse_factory(jar_path);
            for stub in stubs {
                let bytes =
                    postcard::to_allocvec(&stub).expect("Failed to serialize external stub");
                let fqn_str = rodeo.resolve(&stub.name);
                tmp_db.put(&mut wtxn, fqn_str, &bytes).unwrap();
            }
            wtxn.commit().unwrap();

            // Sync and detach to free memory maps before moving file handles
            tmp_env.prepare_for_closing().wait();

            // Atomic fallback deployment (highly durable and safe across all OS filesystems)
            if fs::rename(&tmp_db_path, &final_db_path).is_err() {
                let _ = fs::remove_dir_all(&tmp_db_path);
            }
        }

        // Initialize and bind a permanently Immutable, Zero-Lock Read-Only handle
        let ro_env = unsafe {
            EnvOpenOptions::new()
                .flags(heed::EnvFlags::READ_ONLY)
                .open(&final_db_path)
                .expect("Failed to load read-only global DB view")
        };
        let rtxn = ro_env.read_txn().unwrap();
        let ro_db = ro_env
            .open_database(&rtxn, None)
            .unwrap()
            .expect("DB missing");

        drop(rtxn);

        self.attached_libraries.insert(lib_id, (ro_env, ro_db));
    }

    /// Resolves class metadata across all active layers with zero-copy reference mechanics.
    pub fn resolve_class(&self, fqn: &str) -> Option<Arc<ClassStub>> {
        // Step 1: Query the hyper-volatile Project Local Storage layer
        let local_rtxn = self
            .workspace_env
            .read_txn()
            .expect("Failed to open read txn");
        if let Some(bytes) = self.workspace_fqn_to_stub.get(&local_rtxn, fqn).unwrap() {
            let stub: ClassStub = postcard::from_bytes(bytes).ok()?;
            return Some(Arc::new(stub));
        }

        // Step 2: Query the attached external libraries layer (Guava, Spring, JDK modules, etc.)
        for entry in self.attached_libraries.iter() {
            let (env, db) = entry.value();
            let rtxn = env.read_txn().ok()?;
            if let Some(bytes) = db.get(&rtxn, fqn).unwrap() {
                let stub: ClassStub = postcard::from_bytes(bytes).ok()?;
                return Some(Arc::new(stub));
            }
        }

        None
    }
}
