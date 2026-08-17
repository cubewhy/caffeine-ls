//! Salsa glue for the stub index.
//!
//! Libraries are immutable within a session: their id is derived from the
//! archive path and mtime (see `project_model::LibraryId`), so the index is
//! registered once, loaded lazily on first use and then served from salsa's
//! memoization plus the per-library LRU member cache.

use std::sync::Arc;

use base_db::{
    SourceDatabase,
    salsa::{self, Setter as _},
};
use camino::Utf8PathBuf;
use dashmap::DashMap;
use lasso::ThreadedRodeo;
use parking_lot::Mutex;
use rustc_hash::FxHashSet;

use crate::{
    index::{ClassEntry, LibraryIndex, NameIndex},
    loader,
    stubs::{ClassOrModuleRecord, Symbol},
};
/// Identifies a library archive (jar or JDK jimage). Hashed from the path
/// and mtime, so content changes produce a new id and invalidate the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LibraryId(pub u64);

impl std::fmt::Display for LibraryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LibraryKind {
    Jar,
    Jimage,
}

/// The set of registered libraries. Changes when the workspace model is
/// (re)loaded.
#[salsa::input(singleton, debug)]
pub struct RegisteredLibraries {
    #[returns(ref)]
    pub libraries: FxHashSet<LibraryId>,
}

/// Per-library state: registration data plus the lazily built index.
pub struct LibraryState {
    kind: LibraryKind,
    archive: Utf8PathBuf,
    index: Mutex<Option<Arc<LibraryIndex>>>,
}

/// Session-wide shared state of the stub index: the symbol interner and the
/// per-library indexes.
#[derive(Default)]
pub struct HirState {
    pub interner: ThreadedRodeo,
    pub libraries: DashMap<LibraryId, LibraryState>,
}

#[salsa::db]
pub trait HirDatabase: SourceDatabase {
    fn hir_state(&self) -> &HirState;
}

/// Registers a library with the database. Loading is deferred until the
/// first query.
pub fn register_library(
    db: &mut dyn HirDatabase,
    id: LibraryId,
    kind: LibraryKind,
    archive: Utf8PathBuf,
) {
    db.hir_state().libraries.insert(
        id,
        LibraryState {
            kind,
            archive,
            index: Mutex::new(None),
        },
    );

    let libraries = match RegisteredLibraries::try_get(db) {
        Some(registered) => {
            let mut set = registered.libraries(db).clone();
            set.insert(id);
            set
        }
        None => FxHashSet::from_iter([id]),
    };
    match RegisteredLibraries::try_get(db) {
        Some(registered) => {
            registered.set_libraries(db).to(libraries);
        }
        None => {
            RegisteredLibraries::new(db, libraries);
        }
    }
}

/// The registered libraries, in unspecified order.
pub fn registered_libraries(db: &dyn HirDatabase) -> Vec<LibraryId> {
    RegisteredLibraries::try_get(db)
        .map(|registered| registered.libraries(db).iter().copied().collect())
        .unwrap_or_default()
}

/// The tier-1 name index of a library: loaded from the on-disk cache or
/// built by parsing the archive.
#[salsa::tracked(returns(ref))]
fn library_name_index_query(
    db: &dyn HirDatabase,
    _registered: RegisteredLibraries,
    id: LibraryId,
) -> Arc<NameIndex> {
    let state = db.hir_state();
    let library = state
        .libraries
        .get(&id)
        .unwrap_or_else(|| panic!("library {id:?} is not registered; this is a bug"));
    let cancel_check = || db.unwind_if_revision_cancelled();
    ensure_loaded(id, library.value(), &state.interner, &cancel_check)
        .names
        .clone()
}

/// The tier-1 name index of a registered library.
pub fn library_name_index(db: &dyn HirDatabase, id: LibraryId) -> Arc<NameIndex> {
    let registered = RegisteredLibraries::try_get(db)
        .unwrap_or_else(|| panic!("no libraries registered; this is a bug"));
    library_name_index_query(db, registered, id).clone()
}

fn ensure_loaded(
    id: LibraryId,
    library: &LibraryState,
    interner: &ThreadedRodeo,
    cancel_check: &dyn Fn(),
) -> Arc<LibraryIndex> {
    {
        let guard = library.index.lock();
        if let Some(index) = guard.as_ref() {
            return index.clone();
        }
    }

    let index =
        match loader::load_or_build(id, library.kind, &library.archive, interner, cancel_check) {
            Ok(index) => Arc::new(index),
            Err(err) => {
                tracing::error!(library = %id, "failed to index library: {err:#}");
                Arc::new(LibraryIndex::empty(
                    id,
                    library.kind,
                    library.archive.clone(),
                ))
            }
        };
    *library.index.lock() = Some(index.clone());
    index
}

/// Builds the tier-1 index of a library outside of salsa, so a long parse is
/// not cancelled by revision bumps applied while the workspace loads. The
/// result is stored in the per-library cache; a subsequent salsa query sees
/// it and returns without re-parsing. Errors are logged and the empty index
/// is cached so the failure is not re-attempted on every query.
pub fn warmup_library(db: &dyn HirDatabase, id: LibraryId) {
    let state = db.hir_state();
    let Some(library) = state.libraries.get(&id) else {
        return;
    };
    ensure_loaded(id, library.value(), &state.interner, &|| {});
}

fn library_index(db: &dyn HirDatabase, id: LibraryId) -> Option<Arc<LibraryIndex>> {
    library_name_index(db, id);
    let state = db.hir_state();
    let library = state.libraries.get(&id)?;
    library.value().index.lock().clone()
}

/// A class resolved to a specific library entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedClass {
    pub library: LibraryId,
    pub entry_idx: u32,
    pub entry: ClassEntry,
}

/// Resolves a fully qualified class name against all registered libraries.
pub fn fqn_resolve(db: &dyn HirDatabase, fqn: &str) -> Option<ResolvedClass> {
    let symbol = db.hir_state().interner.get_or_intern(fqn);
    for library in registered_libraries(db) {
        let index = library_name_index(db, library);
        if let Some((entry_idx, entry)) = index.lookup(symbol) {
            return Some(ResolvedClass {
                library,
                entry_idx,
                entry: entry.clone(),
            });
        }
    }
    None
}

/// The direct super class and interfaces of a class, as FQN symbols.
pub fn super_types(_db: &dyn HirDatabase, resolved: &ResolvedClass) -> Vec<Symbol> {
    let mut out = Vec::new();
    if let Some(super_class) = resolved.entry.super_class {
        out.push(super_class);
    }
    out.extend(resolved.entry.interfaces.iter().copied());
    out
}

/// Tier-2 access: the full member stubs of a class.
pub fn class_record(
    db: &dyn HirDatabase,
    resolved: &ResolvedClass,
) -> Option<Arc<ClassOrModuleRecord>> {
    let interner = &db.hir_state().interner;
    let index = library_index(db, resolved.library)?;
    index.class_record(interner, resolved.entry_idx)
}

/// Tier-2 access: a module record by module index.
pub fn module_record(
    db: &dyn HirDatabase,
    library: LibraryId,
    module_idx: u32,
) -> Option<Arc<ClassOrModuleRecord>> {
    let interner = &db.hir_state().interner;
    let index = library_index(db, library)?;
    index.module_record(interner, module_idx)
}
