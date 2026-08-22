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
    FileText, SourceDatabase, SourceRootId, SourceRootInput,
    salsa::{self, Setter as _},
};
use camino::Utf8PathBuf;
use dashmap::DashMap;
use hir_expand::item_tree::{ItemData, ItemId, ItemTree};
use hir_expand::name::Name;
use lasso::ThreadedRodeo;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use vfs::FileId;

use crate::{
    index::{ClassEntry, LibraryIndex, NameIndex},
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
    /// Monotonic id source for inference variables
    /// ([JLS §18.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.1.1))
    /// created during method invocation type inference ([JLS §18.5.2]). Each
    /// distinct id maps to one [`crate::stubs::Symbol`]-free [`Ty`] in `hir-ty`,
    /// unique for the session so no two inference variables ever collide.
    pub next_infer_var: std::sync::Mutex<u64>,
}

#[salsa::db]
pub trait HirDatabase: SourceDatabase + hir_def::db::DefDatabase {
    fn hir_state(&self) -> &HirState;
}

/// The lowered item tree of a source file (see `hir_def::file_item_tree`).
pub fn file_item_tree(
    db: &dyn HirDatabase,
    file_id: FileId,
) -> Arc<hir_expand::item_tree::ItemTree> {
    hir_def::file_item_tree(db, file_id)
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

/// A class resolved to a specific source declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceClass {
    pub file: FileId,
    pub item: hir_expand::item_tree::ItemId,
}

/// A class resolved either to a library entry or to a source declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Library(ResolvedClass),
    Source(SourceClass),
}

impl Resolved {
    /// The fully qualified name of the resolved class.
    pub fn fqn(&self, db: &dyn HirDatabase) -> String {
        match self {
            Resolved::Library(class) => {
                db.hir_state().interner.resolve(&class.entry.fqn).to_owned()
            }
            Resolved::Source(_) => String::new(),
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
            for entry in &classpath(db, source_set.clone()).entries {
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
/// edits invalidate the symbol set of exactly the changed file.
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
        let (simple, public, range) = match data {
            ItemData::Class(d) | ItemData::Interface(d) => (&d.name, d.modifiers.public, d.range),
            ItemData::Enum(d) => (&d.name, d.modifiers.public, d.range),
            ItemData::Record(d) => (&d.name, d.modifiers.public, d.range),
            ItemData::Annotation(d) => (&d.name, d.modifiers.public, d.range),
            // Enum constants are implicitly `public static final`
            // ([JLS §8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1)).
            ItemData::EnumConstant(d) => (&d.name, true, d.range),
            // A module declaration carries no access modifiers
            // ([JLS §7.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7)).
            ItemData::Module(d) => (&d.name, false, d.range),
            ItemData::Method(d) => (&d.name, d.modifiers.public, d.range),
            ItemData::Field(d) => (&d.name, d.modifiers.public, d.range),
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
            range,
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
            | SourceSymbolKind::EnumConstant => prefix,
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
/// so a single indexed lookup replaces the former linear scan.
fn source_resolve(db: &dyn HirDatabase, source_set: &SourceSetId, fqn: &str) -> Option<Resolved> {
    let graph = ProjectGraph::try_get(db)?;
    let name = Name::new(fqn);
    let index = source_set_symbol_index_query(db, graph, source_set.clone());
    index.resolve_class_fqn(&name).map(|reference| {
        Resolved::Source(SourceClass {
            file: reference.file,
            item: reference.symbol.item,
        })
    })
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
