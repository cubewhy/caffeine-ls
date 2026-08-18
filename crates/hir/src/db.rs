//! Salsa glue for the stub index and the classpath-aware project model.
//!
//! Libraries are immutable within a session: their id is derived from the
//! archive path and mtime (see `project_model::LibraryId`), so the index is
//! registered once, loaded lazily on first use and then served from salsa's
//! memoization plus the per-library LRU member cache.
//!
//! Resolution is scoped by the workspace classpath: a [`ProjectGraph`]
//! salsa input maps every source set to its ordered [`Classpath`], and the
//! resolve queries ([`fqn_resolve`]) search exactly those libraries, in
//! classpath order.

use std::sync::Arc;

use base_db::{
    SourceDatabase, SourceRootId,
    salsa::{self, Setter as _},
};
use camino::Utf8PathBuf;
use dashmap::DashMap;
use lasso::ThreadedRodeo;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use vfs::FileId;

use crate::{
    index::{ClassEntry, LibraryIndex, NameIndex},
    loader,
    project::{Classpath, LibraryInfo, ProjectGraphData, SourceSetId},
    stubs::{ClassOrModuleRecord, Symbol},
};
pub use project_model::LibraryId;

/// Identifies a library archive (jar or JDK jimage). Hashed from the path
/// and mtime, so content changes produce a new id and invalidate the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LibraryKind {
    Jar,
    Jimage,
}

/// The classpath-aware workspace model: every source set's ordered classpath
/// plus the registry of loaded libraries. Set once per workspace (re)load;
/// replacing it invalidates every resolve query.
#[salsa::input(singleton, debug)]
pub struct ProjectGraph {
    /// id → metadata for every library reachable from some source set.
    #[returns(ref)]
    pub libraries: FxHashMap<LibraryId, LibraryInfo>,
    /// source set → its (build-tool flattened, ordered) compile classpath.
    #[returns(ref)]
    pub source_sets: FxHashMap<SourceSetId, Arc<Classpath>>,
    /// source root → owning source set. `SourceRootId`s are assigned by
    /// `FileChange::apply` in the order roots were set, so the driver must
    /// keep this map aligned with that order.
    #[returns(ref)]
    pub source_root_to_source_set: FxHashMap<SourceRootId, SourceSetId>,
    /// JDK built-in libraries (jimage / rt.jar), in registration order.
    #[returns(ref)]
    pub jdk_libraries: Vec<LibraryId>,
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

/// Applies a workspace project graph, replacing the previous one. Libraries
/// no longer reachable are dropped from the per-library index cache.
pub fn set_project_graph(db: &mut dyn HirDatabase, data: ProjectGraphData) {
    let state = db.hir_state();

    // Prune libraries that are no longer reachable, then register the new set.
    let live: rustc_hash::FxHashSet<LibraryId> = data.libraries.keys().copied().collect();
    state.libraries.retain(|id, _| live.contains(id));
    for (id, info) in &data.libraries {
        state.libraries.entry(*id).or_insert_with(|| LibraryState {
            kind: info.kind,
            archive: Utf8PathBuf::from(info.path.as_str()),
            index: Mutex::new(None),
        });
    }

    let ProjectGraphData {
        libraries,
        source_sets,
        source_root_to_source_set,
        jdk_libraries,
    } = data;
    match ProjectGraph::try_get(db) {
        Some(graph) => {
            graph.set_libraries(db).to(libraries);
            graph.set_source_sets(db).to(source_sets);
            graph
                .set_source_root_to_source_set(db)
                .to(source_root_to_source_set);
            graph.set_jdk_libraries(db).to(jdk_libraries);
        }
        None => {
            ProjectGraph::new(
                db,
                libraries,
                source_sets,
                source_root_to_source_set,
                jdk_libraries,
            );
        }
    }
}

/// The current project graph, if the workspace has been loaded.
pub fn project_graph(db: &dyn HirDatabase) -> Option<ProjectGraph> {
    ProjectGraph::try_get(db)
}

/// The registered libraries (those reachable from some source set), in
/// unspecified order.
pub fn registered_libraries(db: &dyn HirDatabase) -> Vec<LibraryId> {
    ProjectGraph::try_get(db)
        .map(|graph| graph.libraries(db).keys().copied().collect())
        .unwrap_or_default()
}

/// The JDK built-in libraries (jimage / rt.jar), in registration order.
pub fn jdk_builtin_libraries(db: &dyn HirDatabase) -> Vec<LibraryId> {
    ProjectGraph::try_get(db)
        .map(|graph| graph.jdk_libraries(db).clone())
        .unwrap_or_default()
}

/// The source set owning `file_id`, if the file belongs to a source root.
pub fn source_set_for_file(db: &dyn HirDatabase, file_id: FileId) -> Option<SourceSetId> {
    let graph = ProjectGraph::try_get(db)?;
    let root_id = db.source_root_for_file(file_id)?;
    graph.source_root_to_source_set(db).get(&root_id).cloned()
}

/// The ordered classpath of a source set. Unknown source sets yield an empty
/// classpath.
pub fn classpath(db: &dyn HirDatabase, source_set: SourceSetId) -> Arc<Classpath> {
    ProjectGraph::try_get(db)
        .and_then(|graph| graph.source_sets(db).get(&source_set).cloned())
        .unwrap_or_default()
}

/// The libraries of a source set's classpath, in classpath order.
pub fn classpath_libraries(db: &dyn HirDatabase, source_set: SourceSetId) -> Vec<LibraryId> {
    classpath(db, source_set).libraries().collect()
}

/// The tier-1 name index of a library: loaded from the on-disk cache or
/// built by parsing the archive.
#[salsa::tracked(returns(ref))]
fn library_name_index_query(
    db: &dyn HirDatabase,
    _project_graph: ProjectGraph,
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
    let project_graph =
        ProjectGraph::try_get(db).unwrap_or_else(|| panic!("no project graph; this is a bug"));
    library_name_index_query(db, project_graph, id).clone()
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

/// The set of libraries a resolution query may see.
#[derive(Debug, Clone)]
pub enum ResolutionScope<'a> {
    /// A workspace source set: its ordered classpath.
    SourceSet(SourceSetId),
    /// An explicit, ordered library list (tests / synthetic scopes).
    Classpath(&'a [LibraryId]),
    /// Only the JDK built-ins (jimage / rt.jar). Used for files that are not
    /// yet mapped to a source set.
    JdkBuiltins,
}

/// Resolves a fully qualified class name within a scope, honoring classpath
/// order: the first library containing the name wins.
pub fn fqn_resolve(
    db: &dyn HirDatabase,
    scope: &ResolutionScope<'_>,
    fqn: &str,
) -> Option<ResolvedClass> {
    let libraries: Vec<LibraryId> = match scope {
        ResolutionScope::SourceSet(source_set) => classpath_libraries(db, source_set.clone()),
        ResolutionScope::Classpath(libraries) => libraries.to_vec(),
        ResolutionScope::JdkBuiltins => jdk_builtin_libraries(db),
    };
    resolve_in_libraries(db, &libraries, fqn)
}

/// Resolves a fully qualified class name against an ordered library list.
pub fn resolve_in_libraries(
    db: &dyn HirDatabase,
    libraries: &[LibraryId],
    fqn: &str,
) -> Option<ResolvedClass> {
    let symbol = db.hir_state().interner.get_or_intern(fqn);
    for &library in libraries {
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
