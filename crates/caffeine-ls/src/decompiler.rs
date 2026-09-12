//! Decompiler backends: turning a library class that ships no sources into
//! Java, on demand.
//!
//! A library class with neither an attached source archive nor a sibling
//! `-sources.jar` has no declaration to navigate to. Rather than leave it a
//! dead end, the server runs a decompiler over the class's bytes and serves the
//! Java it produces exactly like a library source (see
//! [`crate::library_view::LibraryView::Decompiled`]).
//!
//! A backend is a jar the user points the server at plus the argv that jar
//! understands; the registry below is the whole configuration surface. Adding
//! one is a struct and one entry in [`BACKENDS`], because nothing outside this
//! module knows how a decompiler is invoked.
//!
//! The JVM itself lives outside the process: a decompiler run costs a JVM start
//! (1-2 s), which is why this module never writes into the cache — it returns
//! the text it produced, and the caller decides where it belongs.

use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Context;
use camino::Utf8Path;
use ide::LibraryKind;
use jimage_rs::JImage;
use project_model::{LibraryId, WorkspaceGraph};
use rustc_hash::{FxHashMap, FxHashSet};
use vfs::AbsPathBuf;
use zip::ZipArchive;

use crate::{
    library_sources,
    library_view::{self, LibraryView},
};

/// One decompiler: how it is named in the client configuration and how it is
/// run.
///
/// `Send + Sync` is part of the contract: a decompile runs on the task pool
/// (a JVM start must not block the main loop), and the registry is a shared
/// table of these.
pub(crate) trait Backend: Send + Sync {
    /// The id the client selects this backend by, and the directory name its
    /// output is cached under.
    fn id(&self) -> &'static str;

    /// The argv after the JVM executable: `["-jar", <jar>, ...]`.
    fn args(
        &self,
        jar: &Path,
        class_file: &Path,
        externals: &[PathBuf],
        out_dir: &Path,
    ) -> Vec<std::ffi::OsString>;
}

/// [CFR](https://github.com/leibnitz27/cfr), which decompiles every classfile
/// up to the newest JDK and runs on any JVM.
pub(crate) struct Cfr;

/// [Vineflower](https://vineflower.org) (formerly Quiltflower), the modern
/// fork of Fernflower. Its releases from 1.11 on require Java 17 or newer.
pub(crate) struct Vineflower;

/// The registry: adding a backend is a struct plus one entry here.
pub(crate) const BACKENDS: &[&dyn Backend] = &[&Cfr, &Vineflower];

/// The backend registered under `id`.
pub(crate) fn backend(id: &str) -> Option<&'static dyn Backend> {
    BACKENDS.iter().copied().find(|backend| backend.id() == id)
}

/// Whether `id` names a registered backend.
pub(crate) fn is_backend(id: &str) -> bool {
    backend(id).is_some()
}

impl Backend for Cfr {
    fn id(&self) -> &'static str {
        "cfr"
    }

    /// CFR writes the class back under its package path (`--outputdir` is the
    /// root of the tree it recreates) and takes a path-separated classpath in
    /// one `--extraclasspath` argument.
    fn args(
        &self,
        jar: &Path,
        class_file: &Path,
        externals: &[PathBuf],
        out_dir: &Path,
    ) -> Vec<std::ffi::OsString> {
        use std::ffi::OsString;

        let mut args: Vec<OsString> = vec![
            "-jar".into(),
            jar.into(),
            class_file.into(),
            "--outputdir".into(),
            out_dir.into(),
            "--silent".into(),
            "true".into(),
        ];
        if externals.is_empty() {
            return args;
        }
        match std::env::join_paths(externals) {
            Ok(classpath) => {
                args.push("--extraclasspath".into());
                args.push(classpath);
            }
            // `join_paths` refuses a path that contains the separator itself
            // (a `:` in a file name); decompiling without a classpath only
            // costs the readability of platform type names.
            Err(err) => tracing::debug!("{}: unusable decompiler classpath: {err}", self.id()),
        }
        args
    }
}

impl Backend for Vineflower {
    fn id(&self) -> &'static str {
        "vineflower"
    }

    /// Vineflower always writes `<Outer>.java` at the destination root — hence
    /// `--folder` — and takes one `--add-external=` argument per classpath
    /// entry.
    fn args(
        &self,
        jar: &Path,
        class_file: &Path,
        externals: &[PathBuf],
        out_dir: &Path,
    ) -> Vec<std::ffi::OsString> {
        use std::ffi::OsString;

        let mut args: Vec<OsString> = vec!["-jar".into(), jar.into(), "--folder".into()];
        for external in externals {
            let mut flag = OsString::from("--add-external=");
            flag.push(external);
            args.push(flag);
        }
        args.push(class_file.into());
        args.push(out_dir.into());
        args
    }
}

/// Creates `<cache>/decompile/v1/<backend>/<library-hex>/` for every library
/// the decompiler can be asked about, deletes the roots of libraries no longer
/// on the classpath, and drops the output of every other backend — switching
/// backends re-decompiles from scratch instead of accumulating an unbounded
/// cache. Mirrors `library_sources::prepare_roots`.
pub(crate) fn prepare_roots(
    cache_dir: &Path,
    backend_id: &str,
    libraries: &[LibraryId],
) -> FxHashMap<LibraryId, AbsPathBuf> {
    let view = LibraryView::Decompiled {
        backend: backend_id.to_owned(),
    };
    prune_backends(cache_dir, backend_id);

    let mut out: FxHashMap<LibraryId, AbsPathBuf> = FxHashMap::default();
    for &library in libraries {
        let root = library_view::root_dir(cache_dir, &view, library);
        if let Err(err) = fs::create_dir_all(&root) {
            tracing::warn!(library = %library, "failed to create decompiler root: {err}");
            continue;
        }
        out.insert(library, AbsPathBuf::assert_utf8(root));
    }

    let live: FxHashSet<LibraryId> = out.keys().copied().collect();
    library_sources::prune_roots(&library_view::view_base(cache_dir, &view), &live);
    out
}

/// Removes the output of every backend but the selected one. A failure to
/// remove is logged, not fatal.
fn prune_backends(cache_dir: &Path, backend_id: &str) {
    let base = cache_dir
        .join("decompile")
        .join(format!("v{}", library_view::DECOMPILE_FORMAT_VERSION));
    let Ok(entries) = fs::read_dir(&base) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() == backend_id {
            continue;
        }
        if let Err(err) = fs::remove_dir_all(entry.path()) {
            tracing::warn!(
                dir = %entry.path().display(),
                "failed to prune decompiler cache: {err}"
            );
        }
    }
}

/// Every library the decompiler can be asked about: every classpath library the
/// build system reported, plus each SDK's class archive (whose classes are
/// decompilable too — a JDK distribution without `src.zip` has no other source
/// of truth). Order is deterministic, so a reload assigns the same roots.
pub(crate) fn decompilable_libraries(graph: &WorkspaceGraph) -> Vec<LibraryId> {
    let mut out: FxHashSet<LibraryId> = graph.library_paths.keys().copied().collect();
    for sdk in graph.sdks.values() {
        if let Some((id, _kind, _path)) = library_sources::sdk_class_archive(sdk) {
            out.insert(id);
        }
    }
    let mut out: Vec<LibraryId> = out.into_iter().collect();
    out.sort_by_key(|library| library.0);
    out
}

/// The classfile bytes of `class_path` (a binary path such as
/// `com/example/Foo.class`) out of a jar or a JDK jimage.
pub(crate) fn read_class_bytes(
    archive: &Path,
    kind: LibraryKind,
    class_path: &str,
) -> anyhow::Result<Vec<u8>> {
    match kind {
        LibraryKind::Jar => {
            let file = File::open(archive)
                .with_context(|| format!("failed to open {}", archive.display()))?;
            let mut zip = ZipArchive::new(file)
                .with_context(|| format!("invalid archive {}", archive.display()))?;
            let mut entry = zip
                .by_name(class_path)
                .with_context(|| format!("no entry {class_path} in {}", archive.display()))?;
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        LibraryKind::Jimage => read_jimage_class(archive, class_path),
    }
}

/// The classfile bytes of `class_path` out of a JDK jimage. The resource name
/// is `<module>/<path>`, and the lookup key is the same name with a leading
/// slash — the shape `hir::loader::parse_jimage` reads stubs with.
fn read_jimage_class(archive: &Path, class_path: &str) -> anyhow::Result<Vec<u8>> {
    let archive_path = Utf8Path::from_path(archive)
        .ok_or_else(|| anyhow::anyhow!("archive path is not UTF-8: {}", archive.display()))?;
    let jimage = JImage::open(archive_path)
        .with_context(|| format!("failed to open jimage {archive_path}"))?;
    let names = jimage
        .resource_names()
        .context("failed to list jimage resources")?;
    let Some((module, path)) = names
        .iter()
        .map(|resource| resource.get_full_name())
        .find(|(_, path)| *path == class_path)
    else {
        anyhow::bail!("no entry {class_path} in {archive_path}");
    };
    let lookup = format!("/{module}/{path}");
    jimage
        .find_resource(&lookup)
        .with_context(|| format!("failed to read {lookup} from {archive_path}"))?
        .map(|bytes| bytes.into_owned())
        .ok_or_else(|| anyhow::anyhow!("no entry {lookup} in {archive_path}"))
}

/// Runs `backend` on one class and returns the Java it produced.
///
/// `class_binary_name` is the outermost class's binary name ([JVMS §4.2]),
/// e.g. `com.example.Foo`. Nothing is written into the cache: the decompiler
/// gets a private temporary directory, reads the class out of `archive` (with
/// `externals`, plus `archive` itself, as its classpath) and the produced text
/// is handed back.
pub(crate) fn decompile(
    backend: &dyn Backend,
    java: &Path,
    jar: &Path,
    archive: &Path,
    kind: LibraryKind,
    class_binary_name: &str,
    externals: &[PathBuf],
) -> anyhow::Result<String> {
    let class_path = format!("{}.class", class_binary_name.replace('.', "/"));
    let bytes = read_class_bytes(archive, kind, &class_path)?;

    let tmp = tempfile::TempDir::new().context("failed to create a decompiler workspace")?;
    // The tools disagree on where they put the result (CFR recreates the
    // package path, Vineflower writes `<Outer>.java` at the root), so the class
    // is written under its own simple name and the output is located by
    // extension, never by a constructed name.
    let outer = class_binary_name
        .rsplit('.')
        .next()
        .unwrap_or(class_binary_name);
    let class_file = tmp.path().join(format!("{outer}.class"));
    fs::write(&class_file, &bytes)
        .with_context(|| format!("failed to write {}", class_file.display()))?;
    let out_dir = tmp.path().join("out");
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;

    let output = Command::new(java)
        .args(backend.args(jar, &class_file, externals, &out_dir))
        .output()
        .with_context(|| {
            format!(
                "failed to run the {} decompiler from {}",
                backend.id(),
                jar.display()
            )
        })?;
    if !output.status.success() {
        anyhow::bail!(
            "the {} decompiler exited with {}: {}",
            backend.id(),
            output.status,
            stderr_tail(&output.stderr)
        );
    }

    let produced = first_java_file(&out_dir)?.ok_or_else(|| {
        anyhow::anyhow!(
            "the {} decompiler produced no Java file: {}",
            backend.id(),
            stderr_tail(&output.stderr)
        )
    })?;
    fs::read_to_string(&produced).with_context(|| format!("failed to read {}", produced.display()))
}

/// The first `*.java` file below `root`, in path order, or `None` when the
/// decompiler wrote none.
fn first_java_file(root: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "java"))
        .collect();
    found.sort();
    Ok(found.into_iter().next())
}

/// The tail of a tool's stderr, for an error that is readable without dumping a
/// whole stack trace: the useful part of a JVM failure is at the end.
fn stderr_tail(stderr: &[u8]) -> String {
    const TAIL: usize = 2048;
    let start = stderr.len().saturating_sub(TAIL);
    String::from_utf8_lossy(&stderr[start..]).trim().to_owned()
}
