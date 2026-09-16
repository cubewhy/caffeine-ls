//! Lazy library-source index: archive entry names → canonical type names, and
//! the lookup that turns a resolved class or member owner into the source
//! location that declares it.
//!
//! A library's source archive is indexed once per library, on the first
//! request that resolves into it, and only its **central directory** is read:
//! entry names are collected, the ~250 MB of JDK `src.zip` text is never
//! decompressed. The index holds `canonical top-level type name → entry name`
//! (~1 MB for a JDK `src.zip`), so a class resolves to its compilation unit
//! without the archive ever being resident.
//!
//! The merge is one-directional: the classfile stub stays the source of truth
//! for resolution, typing and flags. Sources contribute exactly the
//! *location* of a declaration ([`library_source_decl`]) and, through the
//! loaded file, its parameter names. A library that ships no archive
//! contributes no location at all — unless a decompiler is configured for it,
//! in which case the class's decompiled output is that location instead (see
//! [`LibrarySourceDecl::Decompiled`]).

use std::fs::File;

use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use triomphe::Arc;
use vfs::{AbsPath, AbsPathBuf, FileId, VfsPath};
use zip::ZipArchive;

use base_db::SourceRootId;
use camino::Utf8Path;

use crate::{
    HirDatabase,
    db::{HirState, ProjectGraph, file_symbols},
    hir_def::jvm::ids::ItemId,
    lmdb_store::{self, ParamsBlob, SourceIndexBlob, SourcesStamp, StubStore},
    project::LibrarySources,
    symbol_index::SourceSymbolKind,
};
use project_model::LibraryId;

/// The archive entry names of one library's sources, keyed by the canonical
/// name of the top-level type each entry declares.
///
/// Not public: reached through the free functions below.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LibrarySourceIndex {
    entries: FxHashMap<SmolStr, SmolStr>,
}

impl LibrarySourceIndex {
    /// The index a persisted layout holds.
    fn from_blob(blob: SourceIndexBlob) -> Self {
        Self {
            entries: blob
                .entries
                .into_iter()
                .map(|(name, entry)| (SmolStr::new(name), SmolStr::new(entry)))
                .collect(),
        }
    }
}

/// The path of an archive entry below the source root: the module prefix of a
/// JDK 9+ `src.zip` entry is stripped. Those entries are
/// `<module>/<package path>/X.java`, and a module name always contains a `.`
/// while a package segment never does — so a leading segment carrying a dot is
/// the module and is dropped. Shared by the index and [`library_source_path`],
/// so the two always agree.
fn relative_entry(entry: &str) -> &str {
    match entry.split_once('/') {
        Some((head, rest)) if head.contains('.') => rest,
        _ => entry,
    }
}

/// The source index of `library`: the persistent tier when a previous session
/// indexed the *same* archive, the archive's central directory otherwise.
/// `None` when the library has no attached sources or the archive cannot be
/// read.
#[salsa::tracked(returns(ref))]
fn library_source_index_query(
    db: &dyn HirDatabase,
    project_graph: ProjectGraph,
    library: LibraryId,
) -> Option<Arc<LibrarySourceIndex>> {
    let archive = project_graph
        .library_sources(db)
        .get(&library)?
        .archive
        .clone();
    let stamp = SourcesStamp::of(Some(&archive), library_decompiles(db, library));
    // Indexing a JDK `src.zip` walks ~25k central-directory entries; a session
    // that already did it — the workspace-load index stage warms it through
    // [`warm_library_sources`], and a previous session wrote it to the cache —
    // hands the layout over instead (see [`crate::lmdb_store`]).
    let store = &db.hir_state().stub_store;
    if let Some(blob) = store.read_source_index(library, &stamp) {
        tracing::debug!(library = %library, entries = blob.entries.len(), "library sources indexed from cache");
        return Some(Arc::new(LibrarySourceIndex::from_blob(blob)));
    }
    index_archive(store, library, &stamp, &archive).map(Arc::new)
}

/// Builds (and persists) the attached-source layout of the library `library`
/// from `archive`, outside any database snapshot: the scan must not block the
/// main loop's next write (see [`crate::warmup_library`]).
///
/// The workspace-load index stage runs this for every library that ships
/// sources, so the first request that resolves a member through one reads the
/// layout out of the cache rather than the archive's central directory.
pub fn warm_library_sources(
    state: &HirState,
    library: LibraryId,
    archive: &AbsPath,
    decompiles: bool,
) {
    let stamp = SourcesStamp::of(Some(archive), decompiles);
    let store = &state.stub_store;
    if store.read_source_index(library, &stamp).is_some() {
        return;
    }
    index_archive(store, library, &stamp, archive);
}

/// Builds `archive`'s layout and persists it under `stamp`, logging an archive
/// it cannot read. `None` in that case.
fn index_archive(
    store: &StubStore,
    library: LibraryId,
    stamp: &SourcesStamp,
    archive: &AbsPath,
) -> Option<LibrarySourceIndex> {
    let index = match build_index(archive) {
        Ok(index) => index,
        Err(err) => {
            tracing::warn!(library = %library, archive = %archive, "failed to index library sources: {err:#}");
            return None;
        }
    };
    let mut entries: Vec<(String, String)> = index
        .entries
        .iter()
        .map(|(name, entry)| (name.to_string(), entry.to_string()))
        .collect();
    // Sorted, so an unchanged index encodes to unchanged bytes.
    entries.sort();
    let blob = SourceIndexBlob {
        format_version: lmdb_store::CACHE_FORMAT_VERSION,
        stamp: stamp.clone(),
        entries,
    };
    if let Err(err) = store.write_source_index(library, &blob) {
        tracing::debug!(library = %library, "failed to persist library source index: {err:#}");
    }
    tracing::debug!(
        library = %library,
        entries = index.entries.len(),
        "library sources indexed and cached"
    );
    Some(index)
}

/// The parameter names a previous session resolved for the library member
/// `(class, method, descriptor)` — the *persistent* tier of the merge below.
///
/// `None` when the cache holds no answer for this session's sources: nothing
/// was ever resolved for that member, or what was resolved belongs to different
/// sources ([`SourcesStamp`]). A member that was resolved to *no* names answers
/// [`CachedMemberParams::NoNames`] — the answer a source-less library carries,
/// and the reason a cold session never probes such a library again.
pub fn cached_member_params(
    db: &dyn HirDatabase,
    library: LibraryId,
    class: &str,
    method: &str,
    descriptor: &str,
) -> Option<CachedMemberParams> {
    let stamp = sources_stamp(db, library);
    let blob = db
        .hir_state()
        .stub_store
        .read_params(library, &stamp, class, method, descriptor)?;
    Some(match blob.names {
        Some(names) => CachedMemberParams::Names(names),
        None => CachedMemberParams::NoNames,
    })
}

/// Persists the answer for one library member, for the sessions that read the
/// same sources. Best-effort: a disabled or failing cache only costs the next
/// session the work of resolving the member again.
pub fn cache_member_params(
    db: &dyn HirDatabase,
    library: LibraryId,
    class: &str,
    method: &str,
    descriptor: &str,
    names: Option<&[String]>,
) {
    let stamp = sources_stamp(db, library);
    let blob = ParamsBlob {
        format_version: lmdb_store::CACHE_FORMAT_VERSION,
        class: class.to_owned(),
        method: method.to_owned(),
        descriptor: descriptor.to_owned(),
        names: names.map(<[String]>::to_vec),
    };
    if let Err(err) = db
        .hir_state()
        .stub_store
        .write_params(library, &stamp, &blob)
    {
        tracing::debug!(library = %library, "failed to persist a member's parameter names: {err:#}");
    }
}

/// What a previous session resolved for one library member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachedMemberParams {
    /// The parameter names its declaring source records, in declaration order.
    Names(Vec<String>),
    /// The member records none: its library ships no sources at all, or the
    /// source that declares its class declares no such member.
    NoNames,
}

impl CachedMemberParams {
    /// The names, or `None` for [`CachedMemberParams::NoNames`] — the shape the
    /// caller's own answer takes.
    pub fn into_names(self) -> Option<Vec<String>> {
        match self {
            CachedMemberParams::Names(names) => Some(names),
            CachedMemberParams::NoNames => None,
        }
    }
}

/// The stamp of `library`'s attached sources, as the cache keys them.
fn sources_stamp(db: &dyn HirDatabase, library: LibraryId) -> SourcesStamp {
    let archive = library_sources(db, library).map(|sources| sources.archive);
    SourcesStamp::of(archive.as_deref(), library_decompiles(db, library))
}

/// Whether a decompiler is configured for `library`: its output is a declaring
/// view a member's names can be read from once it is produced, which is what
/// makes a "records none" answer conditional on it.
pub fn library_decompiles(db: &dyn HirDatabase, library: LibraryId) -> bool {
    ProjectGraph::try_get(db)
        .is_some_and(|graph| graph.library_decompiled(db).contains_key(&library))
}

fn build_index(archive: &AbsPath) -> anyhow::Result<LibrarySourceIndex> {
    let path: &Utf8Path = archive.as_ref();
    let file = File::open(path).map_err(|e| anyhow::anyhow!("failed to open {path}: {e}"))?;
    let zip =
        ZipArchive::new(file).map_err(|e| anyhow::anyhow!("invalid source archive {path}: {e}"))?;

    let mut entries: FxHashMap<SmolStr, SmolStr> = FxHashMap::default();
    // Only the central directory is read: `name_for_index` never decompresses
    // an entry. Entries are visited in the archive's own order, so the first
    // spelling of a name wins — deterministic, and a name repeated across JDK
    // modules resolves to its first module.
    for idx in 0..zip.len() {
        let Some(name) = zip.name_for_index(idx) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".java") else {
            continue;
        };
        // A JDK 9+ `src.zip` entry (`java.base/java/lang/String.java`) drops
        // its module segment before the `module-info` check.
        let stem = relative_entry(stem);
        if stem == "module-info" {
            continue;
        }
        entries
            .entry(SmolStr::new(stem.replace('/', ".")))
            .or_insert_with(|| SmolStr::new(name));
    }
    Ok(LibrarySourceIndex { entries })
}

/// The source index of a registered library, if it has attached sources.
fn library_source_index(
    db: &dyn HirDatabase,
    library: LibraryId,
) -> Option<Arc<LibrarySourceIndex>> {
    let graph = ProjectGraph::try_get(db)?;
    library_source_index_query(db, graph, library).clone()
}

/// A library's attached sources, `None` when it has none.
pub fn library_sources(db: &dyn HirDatabase, library: LibraryId) -> Option<LibrarySources> {
    ProjectGraph::try_get(db)?
        .library_sources(db)
        .get(&library)
        .cloned()
}

/// The absolute path the entry is materialized at below `library`'s source
/// root. `None` when the library has no attached sources.
pub fn library_source_path(
    db: &dyn HirDatabase,
    library: LibraryId,
    entry: &str,
) -> Option<AbsPathBuf> {
    let sources = library_sources(db, library)?;
    Some(sources.root.join(relative_entry(entry)))
}

/// Where a library class is declared in a view of its library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibrarySourceDecl {
    /// The declaring file is loaded into the database; `item` is the
    /// declaration inside it.
    Loaded { file: FileId, item: ItemId },
    /// The archive entry that still has to be materialized (and loaded) before
    /// the declaration can be answered.
    Pending { entry: Arc<str>, path: AbsPathBuf },
    /// The class has no source at all; its declaring view is what the
    /// decompiler produces for `class`, which has to be run before the
    /// declaration can be answered.
    Decompiled { class: Arc<str>, path: AbsPathBuf },
}

/// Where the library class `fqn` is declared.
///
/// Sources win over decompilation, structurally: the source lookup runs first
/// and its answer is returned as it is, so a library that ships sources never
/// pays for a JVM start.
///
/// `fqn` is the class's *binary* name ([JVMS §4.2]), so a nested type is
/// `pkg.Outer$Inner`.
///
/// `None` when the library is unknown, has neither sources nor a decompiler,
/// the archive holds no compilation unit for the class's outermost type, or
/// the unit — though present in the source root — declares no class-like
/// symbol of that name (an anonymous class's `pkg.Outer$1` lands on
/// `pkg/Outer.java` but is not a declaration there).
pub fn library_source_decl(
    db: &dyn HirDatabase,
    library: LibraryId,
    fqn: &str,
) -> Option<LibrarySourceDecl> {
    if let Some(decl) = source_decl(db, library, fqn) {
        return Some(decl);
    }
    decompiled_decl(db, library, fqn)
}

/// Where the library class `fqn` is declared in its library's source archive.
///
/// A class is declared by the compilation unit of its *outermost* enclosing
/// type: [JLS §7.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.6)
/// names a compilation unit after the top-level type it declares, and a nested
/// type's binary name spells the enclosing types before the first `$`
/// ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2)).
/// So the archive index — keyed by the top-level type each entry declares — is
/// looked up by that prefix; a dotted spelling is never walked back segment by
/// segment, which would let a package name answer for a nested type.
fn source_decl(db: &dyn HirDatabase, library: LibraryId, fqn: &str) -> Option<LibrarySourceDecl> {
    let sources = library_sources(db, library)?;
    let index = library_source_index(db, library)?;
    let root_id = library_source_root(db, library)?;

    let top_level = fqn.split('$').next().unwrap_or(fqn);
    let entry = index.entries.get(top_level)?;
    let path = sources.root.join(relative_entry(entry));
    let source_root = db.source_root(root_id).source_root(db);
    let Some(&file) = source_root.file_for_path(&VfsPath::from(path.clone())) else {
        return Some(LibrarySourceDecl::Pending {
            entry: Arc::from(entry.as_str()),
            path,
        });
    };
    class_symbol(db, file, &fqn.replace('$', "."))
        .map(|item| LibrarySourceDecl::Loaded { file, item })
}

/// Where the library class `fqn` is declared in the library's decompiled
/// output, which materializes one file per compilation unit at the path the
/// class's *outermost* enclosing class names.
///
/// `None` when no decompiler is configured for the library (the feature
/// switch), or when the produced file — already loaded — declares no class-like
/// symbol of that name: a backend may drop a nested type, and answering the
/// outer class would jump to a location that does not declare the reference.
fn decompiled_decl(
    db: &dyn HirDatabase,
    library: LibraryId,
    fqn: &str,
) -> Option<LibrarySourceDecl> {
    let graph = ProjectGraph::try_get(db)?;
    let root = graph.library_decompiled(db).get(&library)?.clone();
    // `Resolved::fqn` hands out binary names ([JVMS §4.2]), and a nested type
    // is produced together with the outermost class that encloses it.
    let outer = fqn.split('$').next().unwrap_or(fqn);
    let path = root.join(format!("{}.java", outer.replace('.', "/")));

    let root_id = library_decompiled_root(db, library)?;
    let source_root = db.source_root(root_id).source_root(db);
    let Some(&file) = source_root.file_for_path(&VfsPath::from(path.clone())) else {
        return Some(LibrarySourceDecl::Decompiled {
            class: Arc::from(outer),
            path,
        });
    };
    class_symbol(db, file, &fqn.replace('$', "."))
        .map(|item| LibrarySourceDecl::Loaded { file, item })
}

/// The class-like symbol of `file` whose canonical name is `fqn`.
fn class_symbol(db: &dyn HirDatabase, file: FileId, fqn: &str) -> Option<ItemId> {
    file_symbols(db, file)
        .iter()
        .find(|symbol| {
            symbol.name.as_str() == fqn
                && matches!(
                    symbol.kind,
                    SourceSymbolKind::Class
                        | SourceSymbolKind::Interface
                        | SourceSymbolKind::Enum
                        | SourceSymbolKind::Record
                        | SourceSymbolKind::Annotation
                )
        })
        .map(|symbol| symbol.item)
}

/// The library whose view holds the file of the source root `file` belongs to —
/// its sources, or its decompiled output.
///
/// Reads the file→source-root input *before* consulting the project graph, for
/// the reason documented on [`crate::db::source_set_for_file`]: a query
/// memoized before the workspace load must re-derive once the graph exists.
pub fn library_source_for_file(db: &dyn HirDatabase, file: FileId) -> Option<LibraryId> {
    let root_id = db.source_root_for_file(file)?;
    let graph = ProjectGraph::try_get(db)?;
    graph
        .library_source_roots(db)
        .get(&root_id)
        .or_else(|| graph.library_decompiled_roots(db).get(&root_id))
        .copied()
}

/// The source root id holding `library`'s sources.
fn library_source_root(db: &dyn HirDatabase, library: LibraryId) -> Option<SourceRootId> {
    let graph = ProjectGraph::try_get(db)?;
    graph
        .library_source_roots(db)
        .iter()
        .find_map(|(root, owner)| (*owner == library).then_some(*root))
}

/// The source root id holding `library`'s decompiled output.
fn library_decompiled_root(db: &dyn HirDatabase, library: LibraryId) -> Option<SourceRootId> {
    let graph = ProjectGraph::try_get(db)?;
    graph
        .library_decompiled_roots(db)
        .iter()
        .find_map(|(root, owner)| (*owner == library).then_some(*root))
}
