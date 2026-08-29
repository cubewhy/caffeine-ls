//! Salsa glue for the stub index and the classpath-aware project model.
//!
//! Libraries are immutable within a session: their id is derived from the
//! archive path and mtime (see `project_model::LibraryId`), so the index is
//! registered once, loaded lazily on first use and then served from salsa's
//! memoization plus the persistent LMDB stub cache.
//!
//! Resolution is scoped by the workspace classpath: a [`ProjectGraph`]
//! salsa input maps every source set to its ordered [`Classpath`], and the
//! resolve queries ([`fqn_resolve`]) search exactly those libraries, in
//! classpath order.

use std::sync::Arc;

use base_db::{
    FileText, SourceDatabase, SourceRootId, SourceRootInput,
    salsa::{self, Setter as _},
};
use camino::Utf8PathBuf;
use dashmap::DashMap;
use hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use hir_expand::name::Name;
use lasso::ThreadedRodeo;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use vfs::{AbsPath, AbsPathBuf, FileId};

use crate::{
    index::{ClassEntry, LibraryIndex, NameIndex},
    lmdb_store::{self, StubStore},
    loader,
    project::{Classpath, ClasspathEntry, LibraryInfo, ProjectGraphData, SourceSetId},
    stubs::{ClassOrModuleRecord, ClassOrModuleStub, Symbol, TypeParameter, TypeRef},
    symbol_index::{SourceSymbol, SourceSymbolIndex, SourceSymbolKind},
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
    /// source root → its resolved build-system base directory (the directory
    /// a classpath looks the root's packages up under), for the package-path
    /// diagnostic ([JLS §7.2.1]). Aligned with [`Self::source_root_to_source_set`].
    #[returns(ref)]
    pub source_root_dirs: FxHashMap<SourceRootId, AbsPathBuf>,
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

/// Session-wide shared state of the stub index: the symbol interner, the
/// per-library indexes and the persistent stub cache.
#[derive(Default)]
pub struct HirState {
    pub interner: ThreadedRodeo,
    pub libraries: DashMap<LibraryId, LibraryState>,
    /// Persistent LMDB-backed stub cache. Starts disabled (memory-only);
    /// server sessions enable it once at startup via
    /// [`enable_persistent_stub_cache`].
    pub stub_store: StubStore,
    /// Monotonic id source for inference variables
    /// ([JLS §18.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.1.1))
    /// created during method invocation type inference ([JLS §18.5.2]). Each
    /// distinct id maps to one [`crate::stubs::Symbol`]-free [`Ty`] in `hir-ty`,
    /// unique for the session so no two inference variables ever collide.
    pub next_infer_var: std::sync::Mutex<u64>,
}

/// The JVM substrate database: the classpath, bytecode and resolved class
/// hierarchy. This is the floor of the `hir` layer — a [`SourceDatabase`]
/// plus the `hir-def` Java file HIR — that owns the session-wide [`HirState`]
/// (the symbol interner, the per-library index cache and the persistent stub
/// cache).
#[salsa::db]
pub trait JvmDatabase: SourceDatabase + hir_def::java::db::JavaDatabase {
    fn hir_state(&self) -> &HirState;
}

/// The Java file HIR database: the Java-specific source queries
/// (`file_symbols`, `source_class_fqn`, package file lists, ...) on top of the
/// JVM substrate.
#[salsa::db]
pub trait JavaDatabase: JvmDatabase {}

/// The Kotlin file HIR database: scaffold for the Kotlin side of the `hir`
/// layer.
#[salsa::db]
pub trait KotlinDatabase: JvmDatabase {}

/// The root database of the `hir` layer, composed from the JVM substrate and
/// the language layers.
#[salsa::db]
pub trait HirDatabase: JavaDatabase + KotlinDatabase {}

/// The lowered item tree of a source file (see `hir_def::file_item_tree`).
pub fn file_item_tree(
    db: &dyn HirDatabase,
    file_id: FileId,
) -> Arc<hir_def::java::item_tree::ItemTree> {
    hir_def::file_item_tree(db, file_id)
}

/// The lowered body tree of a source file (see `hir_def::file_body_tree`).
pub fn file_body_tree(db: &dyn HirDatabase, file_id: FileId) -> Arc<hir_expand::body::BodyTree> {
    hir_def::file_body_tree(db, file_id)
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
        source_root_dirs,
        jdk_libraries,
    } = data;
    match ProjectGraph::try_get(db) {
        Some(graph) => {
            graph.set_libraries(db).to(libraries);
            graph.set_source_sets(db).to(source_sets);
            graph
                .set_source_root_to_source_set(db)
                .to(source_root_to_source_set);
            graph.set_source_root_dirs(db).to(source_root_dirs);
            graph.set_jdk_libraries(db).to(jdk_libraries);
        }
        None => {
            ProjectGraph::new(
                db,
                libraries,
                source_sets,
                source_root_to_source_set,
                source_root_dirs,
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
    let index = ensure_loaded(
        id,
        library.value(),
        &state.interner,
        &state.stub_store,
        &cancel_check,
    );
    Arc::clone(&index.names)
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
    store: &StubStore,
    cancel_check: &dyn Fn(),
) -> Arc<LibraryIndex> {
    {
        let guard = library.index.lock();
        if let Some(index) = guard.as_ref() {
            return index.clone();
        }
    }

    let index = match loader::load_or_build(
        id,
        library.kind,
        &library.archive,
        interner,
        cancel_check,
        store,
    ) {
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
    ensure_loaded(
        id,
        library.value(),
        &state.interner,
        &state.stub_store,
        &|| {},
    );
}

/// Enables the persistent LMDB stub cache for this session, pointing it at
/// the platform default cache directory. Idempotent; later calls after the
/// first use have no effect. Also cleans up leftover pre-LMDB v1 cache
/// files. Returns whether persistent caching could be enabled.
pub fn enable_persistent_stub_cache(db: &dyn HirDatabase) -> bool {
    match lmdb_store::cache_dir() {
        Some(dir) => {
            db.hir_state().stub_store.open_at(dir.clone());
            lmdb_store::remove_legacy_v1_files(&dir);
            true
        }
        None => false,
    }
}

/// Prunes stub-cache entries of unregistered libraries that have gone stale,
/// freeing space for current projects. Intended to run once after library
/// warmup completes.
pub fn prune_stub_cache(db: &dyn HirDatabase) {
    let live: FxHashSet<LibraryId> = registered_libraries(db).into_iter().collect();
    let pruned = db.hir_state().stub_store.prune_stale(&live);
    if pruned > 0 {
        tracing::info!(pruned, "pruned stale stub cache entries");
    }
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

/// A class resolved to a specific source declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceClass {
    pub file: FileId,
    pub item: hir_def::java::item_tree::ItemId,
}

/// A class resolved either to a library entry or to a source declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Library(ResolvedClass),
    Source(SourceClass),
}

impl Resolved {
    /// The fully qualified name of the resolved class. Source classes resolve
    /// to their fully qualified name through the source symbol index; the
    /// empty name is returned when the class has no FQN yet (the unnamed
    /// package / unresolved source).
    pub fn fqn(&self, db: &dyn JvmDatabase) -> hir_def::jvm::fqn::FqName {
        match self {
            Resolved::Library(class) => hir_def::jvm::fqn::FqName::from_str(
                db.hir_state().interner.resolve(&class.entry.fqn),
            ),
            Resolved::Source(_) => hir_def::jvm::fqn::FqName::from_str(""),
        }
    }
}

/// The set of libraries a resolution query may see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionScope {
    /// A workspace source set: its own classes plus its ordered classpath.
    SourceSet(SourceSetId),
    /// An explicit, ordered library list (tests / synthetic scopes).
    Classpath(Vec<LibraryId>),
    /// Only the JDK built-ins (jimage / rt.jar). Used for files that are not
    /// yet mapped to a source set.
    JdkBuiltins,
}

/// Resolves a fully qualified class name within a scope, honoring classpath
/// order: a source set's own classes, then its classpath entries (internal
/// source sets, then libraries), each entry in order.
pub fn fqn_resolve(db: &dyn HirDatabase, scope: &ResolutionScope, fqn: &str) -> Option<Resolved> {
    match scope {
        ResolutionScope::SourceSet(source_set) => {
            if let Some(resolved) = source_resolve(db, source_set, fqn) {
                return Some(resolved);
            }
            let entries = classpath(db, source_set.clone());
            for entry in &entries.entries {
                match entry {
                    ClasspathEntry::SourceSet(internal) => {
                        if let Some(resolved) = source_resolve(db, internal, fqn) {
                            return Some(resolved);
                        }
                    }
                    ClasspathEntry::Library(library) => {
                        if let Some(resolved) =
                            resolve_in_libraries(db, std::slice::from_ref(library), fqn)
                        {
                            return Some(Resolved::Library(resolved));
                        }
                    }
                }
            }
            None
        }
        ResolutionScope::Classpath(libraries) => {
            resolve_in_libraries(db, libraries, fqn).map(Resolved::Library)
        }
        ResolutionScope::JdkBuiltins => {
            resolve_in_libraries(db, &jdk_builtin_libraries(db), fqn).map(Resolved::Library)
        }
    }
}

/// Resolves a fully qualified class name against an ordered library list.
pub fn resolve_in_libraries(
    db: &dyn HirDatabase,
    libraries: &[LibraryId],
    fqn: &str,
) -> Option<ResolvedClass> {
    // Library classes are keyed by their *binary* names, whose nested types
    // join with `$` ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html));
    // source-side references use `.` (`Map.Entry`). Fold rightmost dots into
    // `$` until a lookup hits or no dot remains — package segments never
    // match when folded, so over-folding is indistinguishable from a miss.
    let interner = &db.hir_state().interner;
    let mut candidate = fqn.to_string();
    loop {
        let symbol = interner.get_or_intern(candidate.as_str());
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
        match candidate.rfind('.') {
            Some(dot) => candidate.replace_range(dot..dot + 1, "$"),
            None => return None,
        }
    }
}

/// Best-effort path of `file` for diagnostics, `<no source root>` when the
/// file does not belong to any source root (e.g. opened before the workspace
/// is loaded).
fn debug_path(db: &dyn HirDatabase, file: FileId) -> String {
    db.source_root_for_file(file)
        .and_then(|root| db.source_root(root).source_root(db).path_for_file(&file))
        .map_or_else(|| "<no source root>".to_owned(), |path| path.to_string())
}

/// The indexed declarations ([JLS §7.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.6))
/// of `file` as [`SourceSymbol`]s: class-like types get the package name then
/// each enclosing simple name joined by `.` ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7));
/// members get `EnclosingFqn.simple`. Keyed on the interned [`FileText`] so
/// edits invalidate the symbol set of exactly the changed file. Symbols carry
/// no source ranges (declarations get them from the item tree at IDE time), so
/// an edit that only shifts positions — a whitespace change inside a method
/// body — leaves the symbol set equal and salsa backdates every consumer,
/// keeping the workspace symbol index and resolution caches intact.
#[salsa::tracked(returns(ref))]
fn file_symbols_query(db: &dyn HirDatabase, file: FileText) -> Arc<Vec<SourceSymbol>> {
    let file_id = *file.file_id(db);
    let tree = file_item_tree(db, file_id);
    let symbols = collect_file_symbols(&tree);
    tracing::debug!(
        file_id = ?file_id,
        path = %debug_path(db, file_id),
        top_level_items = tree.top.len(),
        symbol_count = symbols.len(),
        "hir: indexed file declarations",
    );
    Arc::new(symbols)
}

fn collect_file_symbols(tree: &ItemTree) -> Vec<SourceSymbol> {
    fn collect(tree: &ItemTree, id: ItemId, prefix: Option<&Name>, out: &mut Vec<SourceSymbol>) {
        let data = tree.data(id);
        let Some(kind) = SourceSymbolKind::of(data) else {
            // Initializers have no name and are not indexed.
            return;
        };
        let (simple, public) = match data {
            ItemData::Class(d) | ItemData::Interface(d) => (&d.name, d.modifiers.is_public()),
            ItemData::Enum(d) => (&d.name, d.modifiers.is_public()),
            ItemData::Record(d) => (&d.name, d.modifiers.is_public()),
            ItemData::Annotation(d) => (&d.name, d.modifiers.is_public()),
            // Enum constants are implicitly `public static final`
            // ([JLS §8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1)).
            ItemData::EnumConstant(d) => (&d.name, true),
            // A module declaration carries no access modifiers
            // ([JLS §7.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7)).
            ItemData::Module(d) => (&d.name, false),
            ItemData::Method(d) => (&d.name, d.modifiers.is_public()),
            ItemData::Field(d) => (&d.name, d.modifiers.is_public()),
            ItemData::StaticInit(_) | ItemData::InstanceInit(_) => unreachable!(),
        };
        let name = match prefix {
            Some(prefix) => join_name(prefix, simple.as_str()),
            // The unnamed package
            // ([JLS §7.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.2))
            // yields a bare simple name.
            None => match &tree.package {
                Some(package) => join_name(package, simple.as_str()),
                None => simple.clone(),
            },
        };
        out.push(SourceSymbol {
            name: name.clone(),
            item: id,
            kind,
            public,
        });
        if data.body().is_empty() {
            return;
        }
        let child_prefix = match kind {
            // Nested types are indexed under the enclosing FQN
            // ([JLS §8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3));
            // members under `EnclosingFqn.simple`.
            SourceSymbolKind::Class
            | SourceSymbolKind::Interface
            | SourceSymbolKind::Enum
            | SourceSymbolKind::Record
            | SourceSymbolKind::Annotation => Some(&name),
            SourceSymbolKind::Module
            | SourceSymbolKind::Method
            | SourceSymbolKind::Field
            | SourceSymbolKind::EnumConstant
            | SourceSymbolKind::Package => prefix,
        };
        for &child in data.body() {
            collect(tree, child, child_prefix, out);
        }
    }

    let mut out = Vec::new();
    for &top in &tree.top {
        collect(tree, top, None, &mut out);
    }
    out
}

fn join_name(prefix: &Name, suffix: &str) -> Name {
    let mut text = String::with_capacity(prefix.as_str().len() + 1 + suffix.len());
    text.push_str(prefix.as_str());
    text.push('.');
    text.push_str(suffix);
    Name::new(&text)
}

/// The symbols of every file in a source root, tagged with their file. Tracked
/// on the interned [`SourceRootInput`] so file-set changes invalidate it.
#[salsa::tracked(returns(ref))]
fn source_root_symbols_query(
    db: &dyn HirDatabase,
    root: SourceRootInput,
) -> Arc<Vec<(FileId, SourceSymbol)>> {
    let source_root = root.source_root(db);
    let mut out = Vec::new();
    let file_count = source_root.iter().count();
    for file in source_root.iter() {
        for symbol in file_symbols_query(db, db.file_text(file)).iter() {
            out.push((file, symbol.clone()));
        }
    }
    tracing::debug!(
        file_count,
        symbol_count = out.len(),
        "hir: aggregated symbols of a source root",
    );
    Arc::new(out)
}

/// The source symbol index of one source set, scoped to its *own* source
/// roots: it indexes only the declarations defined by that source set
/// ([JLS §7.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7.4)
/// classpath/unnamed-module semantics; see [`fqn_resolve`] for how resolution
/// consults each index in classpath order). Tracked on the [`ProjectGraph`]
/// so graph replacement invalidates it; the per-file symbols are themselves
/// tracked on [`FileText`], so a text edit recomputes only the edited file's
/// symbols before this query re-aggregates.
#[salsa::tracked(returns(ref))]
fn source_set_symbol_index_query(
    db: &dyn HirDatabase,
    _project_graph: ProjectGraph,
    source_set: SourceSetId,
) -> Arc<SourceSymbolIndex> {
    let graph =
        ProjectGraph::try_get(db).unwrap_or_else(|| panic!("no project graph; this is a bug"));
    let mut symbols = Vec::new();
    let mut roots: Vec<SourceRootId> = graph
        .source_root_to_source_set(db)
        .iter()
        .filter(|(_, owner)| **owner == source_set)
        .map(|(root, _)| *root)
        .collect();
    roots.sort();
    let root_count = roots.len();
    for root in roots {
        for (file, symbol) in source_root_symbols_query(db, db.source_root(root)).iter() {
            symbols.push((*file, symbol.clone()));
        }
    }
    let index = SourceSymbolIndex::build(symbols);
    tracing::debug!(
        source_set = ?source_set,
        root_count,
        symbol_count = index.len(),
        "hir: built source set symbol index",
    );
    Arc::new(index)
}

/// The declared package of `file`
/// ([JLS §7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4)),
/// or the empty name for the unnamed package
/// ([JLS §7.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.2)).
/// Tracked on the interned [`FileText`], so a text edit invalidates exactly
/// the edited file's package; files whose declaration name does not move stay
/// memoized.
#[salsa::tracked(returns(clone))]
fn file_package_query(db: &dyn HirDatabase, file: FileText) -> Name {
    let file_id = *file.file_id(db);
    file_item_tree(db, file_id)
        .package
        .clone()
        .unwrap_or_else(|| Name::new(""))
}

/// The files of `root` grouped by declared package
/// ([JLS §7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4)),
/// each group in the root's file-set order. Tracked on the interned
/// [`SourceRootInput`]; the per-file packages ([`file_package_query`]) are
/// themselves tracked on [`FileText`], so a text edit that keeps a file's
/// package leaves this map equal and salsa backdates every consumer instead of
/// re-aggregating the workspace.
#[salsa::tracked(returns(ref))]
fn source_root_package_files_query(
    db: &dyn HirDatabase,
    root: SourceRootInput,
) -> Arc<FxHashMap<Name, Arc<Vec<FileId>>>> {
    let source_root = root.source_root(db);
    let mut out: FxHashMap<Name, Vec<FileId>> = FxHashMap::default();
    for file in source_root.iter() {
        let package = file_package_query(db, db.file_text(file));
        out.entry(package).or_default().push(file);
    }
    Arc::new(
        out.into_iter()
            .map(|(package, files)| (package, Arc::new(files)))
            .collect(),
    )
}

/// The files of `source_set` declaring `package`, most-specific roots first:
/// the source set's own roots sorted by id, each root's files in its file-set
/// order. Tracked on the interned [`ProjectGraph`], so a graph replacement
/// (workspace reload) invalidates it; a *text* edit only re-derives the edited
/// file's package, so — unless the file moved packages — every package's file
/// list is left equal and resolution reads of unrelated packages short-circuit.
#[salsa::tracked(returns(ref))]
fn source_set_package_files_query(
    db: &dyn HirDatabase,
    graph: ProjectGraph,
    source_set: SourceSetId,
    package: Name,
) -> Arc<Vec<FileId>> {
    let mut out = Vec::new();
    let mut roots: Vec<SourceRootId> = graph
        .source_root_to_source_set(db)
        .iter()
        .filter(|(_, owner)| **owner == source_set)
        .map(|(root, _)| *root)
        .collect();
    roots.sort();
    for root in roots {
        if let Some(files) = source_root_package_files_query(db, db.source_root(root)).get(&package)
        {
            out.extend(files.iter().copied());
        }
    }
    Arc::new(out)
}

/// The files of `source_set` whose declared package is `package`
/// ([JLS §7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4));
/// the unnamed package ([§7.4.2]) is the empty name. Unlike the whole-source-set
/// symbol aggregate, this is tracked per (source set, package): a text edit to
/// a file in a different package never invalidates it.
pub fn source_set_package_files(
    db: &dyn HirDatabase,
    source_set: SourceSetId,
    package: &Name,
) -> Arc<Vec<FileId>> {
    let graph =
        ProjectGraph::try_get(db).unwrap_or_else(|| panic!("no project graph; this is a bug"));
    source_set_package_files_query(db, graph, source_set, package.clone()).clone()
}

/// Whether any class of `library` belongs to `package`
/// ([JLS §7.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.2)).
fn library_has_package(db: &dyn HirDatabase, library: LibraryId, package: &str) -> bool {
    let interner = &db.hir_state().interner;
    library_name_index(db, library).has_class_in_package(interner.get_or_intern(package))
}

/// Whether `package` (as written in an on-demand import, [JLS §7.5.2]) is
/// observable within `scope`: the source set's own packages, then each
/// classpath entry in order (internal source sets, then libraries) — the same
/// shadowing order as [`fqn_resolve`]. A package is observable when *any* of
/// its classes resolves; packages never contain `module-info`-only descriptors
/// in the stub index, so the class-index probe is exact.
///
/// The source-side probes read the per-(source set, package) file lists
/// ([`source_set_package_files_query`]), so an edit to a file in another
/// package never invalidates the result.
pub fn package_exists(db: &dyn HirDatabase, scope: &ResolutionScope, package: &str) -> bool {
    let package = Name::new(package);
    match scope {
        ResolutionScope::SourceSet(source_set) => {
            let graph = ProjectGraph::try_get(db)
                .unwrap_or_else(|| panic!("no project graph; this is a bug"));
            if !source_set_package_files_query(db, graph, source_set.clone(), package.clone())
                .is_empty()
            {
                return true;
            }
            let entries = classpath(db, source_set.clone());
            for entry in &entries.entries {
                match entry {
                    ClasspathEntry::SourceSet(internal) => {
                        if !source_set_package_files_query(
                            db,
                            graph,
                            internal.clone(),
                            package.clone(),
                        )
                        .is_empty()
                        {
                            return true;
                        }
                    }
                    ClasspathEntry::Library(library) => {
                        if library_has_package(db, *library, package.as_str()) {
                            return true;
                        }
                    }
                }
            }
            false
        }
        ResolutionScope::Classpath(libraries) => libraries
            .iter()
            .any(|library| library_has_package(db, *library, package.as_str())),
        ResolutionScope::JdkBuiltins => jdk_builtin_libraries(db)
            .iter()
            .any(|library| library_has_package(db, *library, package.as_str())),
    }
}

/// The path segments of `file`'s parent directory, in order, as owned
/// strings — `["src", "com", "example"]` for `/src/com/example/A.java`.
/// `None` when the file has no source root or its path is virtual
/// (unsaved buffers), so a caller cannot anchor the comparison.
pub fn file_path_segments(db: &dyn HirDatabase, file: FileId) -> Option<Arc<Vec<String>>> {
    let root = db.source_root_for_file(file)?;
    let root = db.source_root(root);
    let path = root.source_root(db).path_for_file(&file)?;
    let abs = path.as_path()?;
    Some(Arc::new(dir_segments(abs.parent()?)))
}

/// The package *directory* of `file` relative to its source root, rendered
/// IntelliJ-style for the package-path diagnostic ([JLS §7.2.1]): the
/// parent-directory segments left after the source root's base is stripped,
/// joined with `.` — `org/example` renders as `org.example`.
///
/// The source root's base is recovered as the longest common directory prefix
/// of every file in the root (there is no separate base-dir input in
/// [`SourceRoot`]); a configured root that spans a whole package tree yields
/// exactly the root directory. When no base can be recovered (fewer than two
/// real-backed files) or the file's directory lies outside it, the full
/// slash-joined parent directory is returned as a fallback.
///
/// [JLS §7.2.1]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.2.1
pub fn file_package_dir(db: &dyn HirDatabase, file: FileId) -> Option<String> {
    let root_id = db.source_root_for_file(file)?;
    let root = db.source_root(root_id);
    let path = root.source_root(db).path_for_file(&file)?;
    let abs = path.as_path()?;
    let dir = dir_segments(abs.parent()?);
    // Prefer the base directory the build system resolved for this source
    // root ([JLS §7.2.1]): the root is the exact directory a classpath looks
    // the root's packages up under, so the package is the file's parent
    // relative to it. The file-set heuristic is kept only as a fallback (and
    // the full slash path when no base is recoverable).
    let base = project_graph(db).and_then(|graph| {
        graph
            .source_root_dirs(db)
            .get(&root_id)
            .map(|base| dir_segments(base))
    });
    let base = match base {
        Some(base) => Some(base),
        None => source_root_dir_anchor_query(db, root)
            .clone()
            .map(|v| v.to_vec()),
    };
    match base {
        Some(base) if base.len() < dir.len() && dir[..base.len()] == base[..] => {
            Some(dir[base.len()..].join("."))
        }
        _ => Some(dir.join("/")),
    }
}

/// The normal (path) segments of an absolute path, in order, as owned
/// strings: `["src", "com", "example"]` for `/src/com/example`.
fn dir_segments(path: &AbsPath) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            camino::Utf8Component::Normal(segment) => Some(segment.to_owned()),
            _ => None,
        })
        .collect()
}

/// The longest common directory prefix, segment by segment, of every real-file
/// parent directory in the source root — the root's effective base directory
/// ([JLS §7.2.1]). `None` when the root has fewer than two real-backed files
/// (with one file there is nothing to anchor a base against).
#[salsa::tracked(returns(ref))]
fn source_root_dir_anchor_query(
    db: &dyn HirDatabase,
    root: SourceRootInput,
) -> Option<Arc<Vec<String>>> {
    let source_root = root.source_root(db);
    let mut anchor: Option<Vec<String>> = None;
    let mut real_count = 0usize;
    for file in source_root.iter() {
        let Some(path) = source_root.path_for_file(&file) else {
            continue;
        };
        let Some(abs) = path.as_path() else { continue };
        let Some(parent) = abs.parent() else { continue };
        real_count += 1;
        let dir = dir_segments(parent);
        anchor = Some(match anchor {
            None => dir,
            Some(anchor) => common_dir_prefix(&anchor, &dir),
        });
    }
    (real_count > 1).then_some(Arc::new(anchor?))
}

/// The longest common prefix of two path-segment lists.
fn common_dir_prefix(a: &[String], b: &[String]) -> Vec<String> {
    a.iter()
        .zip(b)
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.clone())
        .collect()
}

/// The indexed symbols of a file.
pub fn file_symbols(db: &dyn HirDatabase, file_id: FileId) -> Arc<Vec<SourceSymbol>> {
    let symbols = file_symbols_query(db, db.file_text(file_id));
    tracing::debug!(
        file_id = ?file_id,
        path = %debug_path(db, file_id),
        symbol_count = symbols.len(),
        "hir: requested file symbols",
    );
    symbols.clone()
}

/// The source symbol index of a source set, scoped to its own source roots.
pub fn source_set_symbols(db: &dyn HirDatabase, source_set: SourceSetId) -> Arc<SourceSymbolIndex> {
    let graph =
        ProjectGraph::try_get(db).unwrap_or_else(|| panic!("no project graph; this is a bug"));
    let index = source_set_symbol_index_query(db, graph, source_set.clone());
    tracing::debug!(
        source_set = %source_set,
        symbol_count = index.len(),
        index_empty = index.is_empty(),
        "hir: requested source set symbol index",
    );
    index.clone()
}

/// Resolves `fqn` against the source symbol index of `source_set`'s own
/// source roots. Types are indexed under their fully qualified name
/// ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)),
/// and a declaration's FQN is its declared package joined with its (possibly
/// nested) type path — so the declaring file's package is always a `.`-prefix
/// of the FQN. Rather than consulting one per-source-set aggregate (which
/// salsa must re-derive on *any* file edit, cascading the recompute into every
/// file's type inference), this probes only the files of the FQN's prefix
/// packages ([`source_set_package_files_query`]) and scans each candidate's
/// per-file symbols ([`file_symbols_query`]) — both tracked per file/package,
/// so an edit to a file in an unrelated package leaves every resolver here
/// memoized.
fn source_resolve(db: &dyn HirDatabase, source_set: &SourceSetId, fqn: &str) -> Option<Resolved> {
    let graph = ProjectGraph::try_get(db)?;
    for package in package_prefixes(fqn) {
        let files = source_set_package_files_query(db, graph, source_set.clone(), package);
        for &file in files.iter() {
            for symbol in file_symbols_query(db, db.file_text(file)).iter() {
                if symbol.name.as_str() == fqn
                    && matches!(
                        symbol.kind,
                        SourceSymbolKind::Class
                            | SourceSymbolKind::Interface
                            | SourceSymbolKind::Enum
                            | SourceSymbolKind::Record
                            | SourceSymbolKind::Annotation
                            | SourceSymbolKind::Module
                    )
                {
                    return Some(Resolved::Source(SourceClass {
                        file,
                        item: symbol.item,
                    }));
                }
            }
        }
    }
    None
}

/// The `.`-prefix packages of a fully qualified name, most specific first: a
/// declaration whose FQN is `fqn` lives in a file whose declared package is
/// one of these (the package segments come first, the type path — possibly
/// nested — last). A bare simple name has only the unnamed package (`""`).
fn package_prefixes(fqn: &str) -> Vec<Name> {
    let mut out = Vec::new();
    let mut end = fqn.len();
    while let Some(dot) = fqn[..end].rfind('.') {
        out.push(Name::new(&fqn[..dot]));
        end = dot;
    }
    out.push(Name::new(""));
    out
}

/// The fully qualified name
/// ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7))
/// of a source class declaration. Preserves the classic semantics — only
/// class-like items, excluding `module-info` — because `hir-ty` uses it to
/// compute enclosing classes for access control
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
pub fn source_class_fqn(db: &dyn HirDatabase, file: FileId, item: ItemId) -> Option<Name> {
    let symbols = file_symbols_query(db, db.file_text(file));
    symbols
        .iter()
        .find(|symbol| {
            symbol.item == item
                && matches!(
                    symbol.kind,
                    SourceSymbolKind::Class
                        | SourceSymbolKind::Interface
                        | SourceSymbolKind::Enum
                        | SourceSymbolKind::Record
                        | SourceSymbolKind::Annotation
                )
        })
        .map(|symbol| symbol.name.clone())
}

/// The direct super class and interfaces of a class, as FQN symbols. Source
/// classes are handled by `hir-ty` against the item tree; this returns the
/// empty set for them.
pub fn super_types(_db: &dyn HirDatabase, resolved: &Resolved) -> Vec<Symbol> {
    match resolved {
        Resolved::Library(resolved) => {
            let mut out = Vec::new();
            if let Some(super_class) = resolved.entry.super_class {
                out.push(super_class);
            }
            out.extend(resolved.entry.interfaces.iter().copied());
            out
        }
        Resolved::Source(_) => Vec::new(),
    }
}

/// The generic signature of a class as written in its classfile `Signature`
/// attribute ([JVMS §4.7.9.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.9.1)):
/// the declared type parameters plus the superclass and interfaces with their
/// type arguments. Unlike [`super_types`] this carries the type arguments, so
/// parameterized subtyping ([JLS §4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2))
/// can substitute them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassGenericInfo {
    pub type_params: Vec<TypeParameter<Symbol>>,
    pub super_class: Option<TypeRef<Symbol>>,
    pub interfaces: Vec<TypeRef<Symbol>>,
}

/// The generic signature of a resolved class, from its tier-2 class record.
/// `None` when the tier-2 record is unavailable (in-memory-only index) or the
/// class carries no `Signature` attribute. Source classes carry no classfile
/// signature, so `None` is returned for them.
pub fn class_generic_info(db: &dyn HirDatabase, resolved: &Resolved) -> Option<ClassGenericInfo> {
    let resolved = match resolved {
        Resolved::Library(resolved) => resolved,
        Resolved::Source(_) => return None,
    };
    let record = class_record(db, resolved)?;
    let ClassOrModuleStub::Class(class) = record.as_ref() else {
        return None;
    };
    Some(ClassGenericInfo {
        type_params: class.type_params.clone(),
        super_class: class.super_class.clone(),
        interfaces: class.interfaces.clone(),
    })
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
