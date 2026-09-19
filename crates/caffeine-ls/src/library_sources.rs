//! Locating, preparing and reading library (and JDK) source archives.
//!
//! A library's sources are never extracted wholesale and never loaded eagerly:
//! a JDK `src.zip` holds ~25k compilation units (~250 MB of text), so keeping
//! them resident — or even on disk — is the cost this design refuses to pay.
//! Each library's archive is indexed once by the analysis layer (entry names
//! for Java, declared classifiers for Kotlin), and an individual file is written
//! under `<cache_dir>/sources/v1/<library-id-hex>/` and loaded into the
//! VFS/database **only when a request resolves into it**.
//!
//! This module owns the cache side of that protocol: which archive belongs to
//! which library, the root each library materializes into, and the one-file
//! read/write pair the LSP layer drives.

use std::{
    fs::{self, File},
    io::Read,
    path::Path,
};

use ide::{LibraryId, LibraryKind, LibrarySources};
use project_model::{SdkData, WorkspaceGraph};
use rustc_hash::{FxHashMap, FxHashSet};
use vfs::AbsPathBuf;
use zip::ZipArchive;

use crate::library_view::{self, LibraryView};

/// The SDK's platform class archive: `lib/modules` (jimage), then the legacy
/// `lib/rt.jar`, then the pre-JDK-9 `jre/lib/rt.jar`, first existing wins —
/// with the library id derived from its path, so the SDK's archive identity is
/// computed in exactly one place.
pub(crate) fn sdk_class_archive(sdk: &SdkData) -> Option<(LibraryId, LibraryKind, AbsPathBuf)> {
    let candidates = [
        (
            sdk.home_path.join("lib").join("modules"),
            LibraryKind::Jimage,
        ),
        (sdk.home_path.join("lib").join("rt.jar"), LibraryKind::Jar),
        (
            sdk.home_path.join("jre").join("lib").join("rt.jar"),
            LibraryKind::Jar,
        ),
    ];
    let (path, kind) = candidates.into_iter().find(|(path, _)| exists(path))?;
    let id = LibraryId::from_file_path(path.as_ref()).ok()?;
    Some((id, kind, path))
}

/// The JDK's source archive (`lib/src.zip` for JDK 9+, `src.zip` for a few
/// distributions), first existing file.
pub(crate) fn sdk_source_archive(sdk: &SdkData) -> Option<AbsPathBuf> {
    [
        sdk.home_path.join("lib").join("src.zip"),
        sdk.home_path.join("src.zip"),
    ]
    .into_iter()
    .find(exists)
}

/// `<dir>/<stem>-sources.jar` beside a classpath jar, when that file exists.
pub(crate) fn sibling_sources_jar(jar: &Path) -> Option<AbsPathBuf> {
    let stem = jar.file_stem()?.to_str()?;
    let candidate = jar.with_file_name(format!("{stem}-sources.jar"));
    candidate
        .is_file()
        .then(|| AbsPathBuf::assert_utf8(candidate))
}

/// Library → the source archive to index: the importer-reported attachment
/// when the build system resolved one, else the sibling `-sources.jar` on
/// disk; plus every SDK's class archive paired with its `src.zip`.
pub(crate) fn collect_archives(graph: &WorkspaceGraph) -> FxHashMap<LibraryId, AbsPathBuf> {
    let mut out: FxHashMap<LibraryId, AbsPathBuf> = FxHashMap::default();
    for (id, library) in &graph.library_paths {
        let sources = graph.library_sources.get(id).cloned().or_else(|| {
            let path: &Path = library.path.as_ref();
            sibling_sources_jar(path)
        });
        if let Some(sources) = sources {
            out.insert(*id, sources);
        }
    }
    for sdk in graph.sdks.values() {
        if let Some((id, _kind, _path)) = sdk_class_archive(sdk)
            && let Some(sources) = sdk_source_archive(sdk)
        {
            out.insert(id, sources);
        }
    }
    out
}

/// Creates a materialization root per library with sources (empty directories:
/// a library whose sources are never read costs one directory) and deletes the
/// directories of libraries no longer on the classpath.
pub(crate) fn prepare_roots(
    cache_dir: &Path,
    archives: &FxHashMap<LibraryId, AbsPathBuf>,
) -> FxHashMap<LibraryId, LibrarySources> {
    let view = LibraryView::Source;
    let mut out: FxHashMap<LibraryId, LibrarySources> = FxHashMap::default();
    for (id, archive) in archives {
        let root = library_view::root_dir(cache_dir, &view, *id);
        if let Err(err) = fs::create_dir_all(&root) {
            tracing::warn!(library = %id, "failed to create library source root: {err}");
            continue;
        }
        out.insert(
            *id,
            LibrarySources {
                archive: archive.clone(),
                root: AbsPathBuf::assert_utf8(root),
            },
        );
    }
    let live: FxHashSet<LibraryId> = out.keys().copied().collect();
    prune_roots(&library_view::view_base(cache_dir, &view), &live);
    out
}

/// Removes the materialized files of libraries that are no longer on the
/// classpath, under a view's base directory. A failure to remove is logged, not
/// fatal — mirrors `ide::LibraryWarmup::prune`.
pub(crate) fn prune_roots(base: &Path, live: &FxHashSet<LibraryId>) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let is_live = live.iter().any(|id| id.to_string() == name);
        if !is_live && let Err(err) = fs::remove_dir_all(entry.path()) {
            tracing::warn!(
                dir = %entry.path().display(),
                "failed to prune library source cache: {err}"
            );
        }
    }
}

/// Reads one archive entry's bytes.
pub(crate) fn read_entry(archive: &Path, entry: &str) -> anyhow::Result<Vec<u8>> {
    let file = File::open(archive)
        .map_err(|e| anyhow::anyhow!("failed to open {}: {e}", archive.display()))?;
    let mut zip = ZipArchive::new(file)
        .map_err(|e| anyhow::anyhow!("invalid source archive {}: {e}", archive.display()))?;
    let mut entry = zip
        .by_name(entry)
        .map_err(|e| anyhow::anyhow!("no entry {entry} in {}: {e}", archive.display()))?;
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Writes `bytes` into `root` at `relative`, creating the parent directory. A
/// file that already exists is left as it is: idempotent, and a
/// re-materialization after a stale cache entry is impossible because
/// `LibraryId` hashes the archive's path and mtime, so a changed archive is a
/// different library and thus a different root.
pub(crate) fn materialize(root: &Path, relative: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let path = root.join(relative);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, bytes)?;
    Ok(())
}

/// Whether the absolute path exists.
fn exists(path: &AbsPathBuf) -> bool {
    let path: &Path = path.as_ref();
    fs::metadata(path).is_ok()
}
