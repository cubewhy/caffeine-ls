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
//! loaded file, its parameter names.

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
    db::{ProjectGraph, file_symbols},
    hir_def::java::item_tree::ItemId,
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
    /// The archive entry declaring the class `fqn`, or `None`.
    ///
    /// `fqn` may arrive in the binary spelling (`pkg.Outer$Inner`,
    /// [JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2)):
    /// `$` is folded to `.` and then the longest prefix the index knows is
    /// taken, which lands nested types on their outer compilation unit
    /// (`java.util.Map.Entry` → `java/util/Map.java`). The lookup is also how
    /// a **method/field** owner is resolved: a member is looked for in its
    /// owning class's file.
    fn lookup(&self, fqn: &str) -> Option<&SmolStr> {
        let mut candidate = fqn.replace('$', ".");
        loop {
            if let Some(entry) = self.entries.get(candidate.as_str()) {
                return Some(entry);
            }
            match candidate.rfind('.') {
                Some(dot) => candidate.truncate(dot),
                None => return None,
            }
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

/// Builds the source index of `library` by reading the central directory of
/// its source archive. `None` when the library has no attached sources or the
/// archive cannot be read.
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
    match build_index(&archive) {
        Ok(index) => Some(Arc::new(index)),
        Err(err) => {
            tracing::warn!(library = %library, archive = %archive, "failed to index library sources: {err:#}");
            None
        }
    }
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

/// The archive entry declaring the class `fqn`, or `None` when the library has
/// no sources or its archive does not lay the type out under its own name.
pub fn library_source_entry(
    db: &dyn HirDatabase,
    library: LibraryId,
    fqn: &str,
) -> Option<Arc<str>> {
    let index = library_source_index(db, library)?;
    index.lookup(fqn).map(|entry| Arc::from(entry.as_str()))
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

/// Where a library class is declared in the library's sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibrarySourceDecl {
    /// The declaring file is loaded into the database; `item` is the
    /// declaration inside it.
    Loaded { file: FileId, item: ItemId },
    /// The archive entry that still has to be materialized (and loaded) before
    /// the declaration can be answered.
    Pending { entry: Arc<str>, path: AbsPathBuf },
}

/// Where the library class `fqn` is declared in its library's sources.
///
/// `None` when the library is unknown, has no sources, the entry name does not
/// match a prefix of `fqn`, or the materialized file — though present in the
/// source root — declares no class-like symbol of that name (an anonymous
/// class's `pkg.Outer$1` landing on `pkg/Outer.java` is not a declaration of
/// `pkg.Outer$1`).
pub fn library_source_decl(
    db: &dyn HirDatabase,
    library: LibraryId,
    fqn: &str,
) -> Option<LibrarySourceDecl> {
    let sources = library_sources(db, library)?;
    let index = library_source_index(db, library)?;
    let root_id = library_source_root(db, library)?;
    let dotted = fqn.replace('$', ".");

    // Walk the prefixes the index knows: a nested type lands on its outer
    // compilation unit, and a file whose top-level type does not match keeps
    // the walk going.
    let mut candidate = dotted.clone();
    loop {
        if let Some(entry) = index.entries.get(candidate.as_str()) {
            let path = sources.root.join(relative_entry(entry));
            let source_root = db.source_root(root_id).source_root(db);
            let Some(&file) = source_root.file_for_path(&VfsPath::from(path.clone())) else {
                return Some(LibrarySourceDecl::Pending {
                    entry: Arc::from(entry.as_str()),
                    path,
                });
            };
            if let Some(item) = class_symbol(db, file, &dotted) {
                return Some(LibrarySourceDecl::Loaded { file, item });
            }
        }
        match candidate.rfind('.') {
            Some(dot) => candidate.truncate(dot),
            None => return None,
        }
    }
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

/// The library whose sources are held by the source root `file` belongs to.
///
/// Reads the file→source-root input *before* consulting the project graph, for
/// the reason documented on [`crate::db::source_set_for_file`]: a query
/// memoized before the workspace load must re-derive once the graph exists.
pub fn library_source_for_file(db: &dyn HirDatabase, file: FileId) -> Option<LibraryId> {
    let root_id = db.source_root_for_file(file)?;
    let graph = ProjectGraph::try_get(db)?;
    graph.library_source_roots(db).get(&root_id).copied()
}

/// The source root id holding `library`'s sources.
fn library_source_root(db: &dyn HirDatabase, library: LibraryId) -> Option<SourceRootId> {
    let graph = ProjectGraph::try_get(db)?;
    graph
        .library_source_roots(db)
        .iter()
        .find_map(|(root, owner)| (*owner == library).then_some(*root))
}
