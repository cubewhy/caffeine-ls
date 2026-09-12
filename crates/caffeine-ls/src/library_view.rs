//! Where a library view lives on disk and how a path in it spells itself as a
//! URI.
//!
//! The server serves two read-only views of a library: the sources its build
//! system attached (or a sibling `-sources.jar` beside the classpath jar), and
//! the Java a decompiler produced for a class that ships none. Each view owns a
//! directory tree under the cache dir — `<cache>/sources/v1/<library-hex>` and
//! `<cache>/decompile/v1/<backend>/<library-hex>` — and, because the client has
//! to fetch the content of a library file the server answers a definition for,
//! each view also owns a URI spelling:
//!
//! ```text
//! caffeine-ls://<library-hex>/source/<path below the library root>
//! caffeine-ls://<library-hex>/decompiled/<backend>/<path below the library root>
//! ```
//!
//! Both directions live in this module on purpose. Materialization decides
//! where a file is written and the URI decides how it is named to the client;
//! if the two were derived apart, a definition could answer a location the
//! client cannot open.

use std::{fmt::Write as _, path::PathBuf, str::FromStr};

use camino::{Utf8Path, Utf8PathBuf};
use lsp_types::Uri;
use project_model::LibraryId;
use vfs::{AbsPath, AbsPathBuf};

/// Version of the on-disk source cache layout. Bumping it invalidates every
/// previously materialized file (a directory per version).
pub(crate) const SOURCE_FORMAT_VERSION: u32 = 1;

/// Version of the on-disk decompiled-file cache layout. Bumping it invalidates
/// every previously decompiled file (a directory per version).
pub(crate) const DECOMPILE_FORMAT_VERSION: u32 = 1;

/// The two read-only views of a library the server can serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LibraryView {
    /// The library's attached source archive, read entry by entry.
    Source,
    /// The output of the named decompiler backend, written one class at a
    /// time. The backend is part of the layout: switching backends starts from
    /// an empty cache instead of serving the other tool's output.
    Decompiled { backend: String },
}

/// The directory every library root of one view hangs under — what a caller
/// that prunes a view's stale library roots reads the way [`root_dir`] writes
/// it.
pub(crate) fn view_base(cache_dir: &std::path::Path, view: &LibraryView) -> PathBuf {
    match view {
        LibraryView::Source => cache_dir
            .join("sources")
            .join(format!("v{SOURCE_FORMAT_VERSION}")),
        LibraryView::Decompiled { backend } => cache_dir
            .join("decompile")
            .join(format!("v{DECOMPILE_FORMAT_VERSION}"))
            .join(backend),
    }
}

/// The directory a library's view materializes into: `<view base>/<library-hex>`.
/// The one place a view root is computed, so the writer and the URI builder
/// cannot diverge.
pub(crate) fn root_dir(
    cache_dir: &std::path::Path,
    view: &LibraryView,
    library: LibraryId,
) -> PathBuf {
    view_base(cache_dir, view).join(library.to_string())
}

/// Splits a path that lies under a view base into its view, the library's hex
/// id and its path below the library root — the three pieces [`uri`] spells.
/// `None` when the path is outside every view base or names no file in one.
pub(crate) fn relative_to_view(
    cache_dir: &std::path::Path,
    path: &AbsPath,
) -> Option<(LibraryView, String, String)> {
    let path: &Utf8Path = path.as_ref();

    if let Some(tail) = strip(cache_dir, &LibraryView::Source, path) {
        return split_tail(LibraryView::Source, tail);
    }

    // A decompiled path carries one more segment — the backend — between the
    // version directory and the library id.
    let base = cache_dir
        .join("decompile")
        .join(format!("v{DECOMPILE_FORMAT_VERSION}"));
    let tail = path.strip_prefix(Utf8Path::from_path(&base)?).ok()?;
    let (backend, rest) = tail.as_str().split_once('/')?;
    if backend.is_empty() {
        return None;
    }
    split_tail(
        LibraryView::Decompiled {
            backend: backend.to_owned(),
        },
        Utf8Path::new(rest),
    )
}

/// `<library-hex>/<path below the library root>` → its two halves, rejecting a
/// root-relative path that names no file.
fn split_tail(view: LibraryView, tail: &Utf8Path) -> Option<(LibraryView, String, String)> {
    let (library, rel) = tail.as_str().split_once('/')?;
    if !is_library_hex(library) || rel.is_empty() {
        return None;
    }
    Some((view, library.to_owned(), rel.to_owned()))
}

/// The `Utf8Path` below the base of `view`, when `path` lies under it.
fn strip<'a>(
    cache_dir: &std::path::Path,
    view: &LibraryView,
    path: &'a Utf8Path,
) -> Option<&'a Utf8Path> {
    path.strip_prefix(Utf8Path::from_path(&view_base(cache_dir, view))?)
        .ok()
}

/// The URI a file of `view` is served under:
/// `caffeine-ls://<library-hex>/source/<rel>` or
/// `caffeine-ls://<library-hex>/decompiled/<backend>/<rel>`. `None` when the
/// scheme or the assembled text is not a URI.
pub(crate) fn uri(scheme: &str, view: &LibraryView, library_hex: &str, rel: &str) -> Option<Uri> {
    let mut text = format!("{scheme}://{library_hex}");
    match view {
        LibraryView::Source => text.push_str("/source/"),
        LibraryView::Decompiled { backend } => {
            write!(text, "/decompiled/{backend}/").ok()?;
        }
    }
    text.push_str(rel);
    Uri::from_str(&text).ok()
}

/// The inverse of [`uri`]: the cache path the URI names. `None` when the scheme
/// differs, a segment is empty or `..`, the library segment is not a library id
/// in hex, or `is_backend` rejects the backend segment — a URI is
/// client-supplied input, so nothing about it is trusted.
pub(crate) fn view_path(
    cache_dir: &std::path::Path,
    scheme: &str,
    uri: &Uri,
    is_backend: impl Fn(&str) -> bool,
) -> Option<AbsPathBuf> {
    if uri.scheme() != scheme {
        return None;
    }
    let library_hex = uri.host_str()?;
    if !is_library_hex(library_hex) {
        return None;
    }

    let mut segments = uri.path_segments()?;
    let view = match segments.next()? {
        "source" => LibraryView::Source,
        "decompiled" => {
            let backend = segments.next()?;
            if !is_backend(backend) {
                return None;
            }
            LibraryView::Decompiled {
                backend: backend.to_owned(),
            }
        }
        _ => return None,
    };

    let rel: Vec<&str> = segments.collect();
    if rel.is_empty()
        || rel
            .iter()
            .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return None;
    }

    let library = LibraryId(u64::from_str_radix(library_hex, 16).ok()?);
    let mut path = Utf8PathBuf::from_path_buf(root_dir(cache_dir, &view, library)).ok()?;
    for segment in rel {
        path.push(segment);
    }
    AbsPathBuf::try_from(path).ok()
}

/// Whether `hex` is a library id as [`LibraryId`]'s `Display` writes it: 16
/// lowercase hex digits.
fn is_library_hex(hex: &str) -> bool {
    hex.len() == 16
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const LIBRARY: LibraryId = LibraryId(0x0123_4567_89ab_cdef);
    const HEX: &str = "0123456789abcdef";
    const SCHEME: &str = "caffeine-ls";

    fn cache_dir() -> &'static Path {
        Path::new("/cache")
    }

    /// The path of one file of a view, for the round-trip assertions.
    fn file(view: &LibraryView, rel: &str) -> AbsPathBuf {
        AbsPathBuf::assert_utf8(root_dir(cache_dir(), view, LIBRARY).join(rel))
    }

    #[test]
    fn source_view_round_trips_through_its_uri() {
        let view = LibraryView::Source;
        let path = file(&view, "com/example/Foo.java");

        let (found, hex, rel) = relative_to_view(cache_dir(), &path).unwrap();
        assert_eq!(found, view);
        assert_eq!(hex, HEX);
        assert_eq!(rel, "com/example/Foo.java");

        let uri = uri(SCHEME, &found, &hex, &rel).unwrap();
        assert_eq!(
            uri.as_str(),
            "caffeine-ls://0123456789abcdef/source/com/example/Foo.java"
        );

        let back = view_path(cache_dir(), SCHEME, &uri, |_| false).unwrap();
        assert_eq!(back, path);
    }

    #[test]
    fn decompiled_view_round_trips_through_its_uri() {
        let view = LibraryView::Decompiled {
            backend: "cfr".to_owned(),
        };
        let path = file(&view, "java/util/Map.java");

        let (found, hex, rel) = relative_to_view(cache_dir(), &path).unwrap();
        assert_eq!(found, view);
        assert_eq!(hex, HEX);
        assert_eq!(rel, "java/util/Map.java");

        let uri = uri(SCHEME, &found, &hex, &rel).unwrap();
        assert_eq!(
            uri.as_str(),
            "caffeine-ls://0123456789abcdef/decompiled/cfr/java/util/Map.java"
        );

        let back = view_path(cache_dir(), SCHEME, &uri, |backend| backend == "cfr").unwrap();
        assert_eq!(back, path);
    }

    #[test]
    fn relative_to_view_rejects_paths_outside_a_view() {
        // A workspace file, the cache root itself and a view base with no file
        // below it name no view.
        for path in [
            "/src/Main.java",
            "/cache",
            "/cache/sources/v1",
            "/cache/sources/v1/0123456789abcdef",
            "/cache/decompile/v1/cfr",
            "/cache/decompile/v1/cfr/0123456789abcdef",
            // A library segment that is not a library id.
            "/cache/sources/v1/not-a-library/com/example/Foo.java",
        ] {
            assert!(
                relative_to_view(cache_dir(), AbsPath::assert(Utf8Path::new(path))).is_none(),
                "{path} names no library view"
            );
        }
    }

    #[test]
    fn view_path_rejects_unservable_uris() {
        let servable = |uri: &str| {
            let uri = Uri::from_str(uri).unwrap();
            view_path(cache_dir(), SCHEME, &uri, |backend| backend == "cfr")
        };

        // A file of the decompiled view of a backend nobody registered.
        assert!(servable("caffeine-ls://0123456789abcdef/decompiled/jd/1.java").is_none());
        // A path that tries to climb out of the cache directory: the URI parser
        // folds dot segments away before `view_path` sees them (for `..` and
        // for `%2e%2e` alike), and what is left names no view. `view_path`
        // refuses a literal `..` on top of that, because a URI is
        // client-supplied input and the parsing rules above are not this
        // module's to rely on.
        assert!(servable("caffeine-ls://0123456789abcdef/source/../../etc/passwd").is_none());
        assert!(servable("caffeine-ls://0123456789abcdef/source/%2e%2e/etc/passwd").is_none());
        // Another client's scheme, or this one without a library.
        assert!(servable("file://0123456789abcdef/source/com/example/Foo.java").is_none());
        assert!(servable("caffeine-ls:///source/com/example/Foo.java").is_none());
        // A library segment that is not a library id, and an unknown view.
        assert!(servable("caffeine-ls://0123456789abcde/source/com/example/Foo.java").is_none());
        assert!(servable("caffeine-ls://0123456789ABCDEF/source/com/example/Foo.java").is_none());
        assert!(servable("caffeine-ls://0123456789abcdef/classes/com/example/Foo.java").is_none());
        // No file below the library root.
        assert!(servable("caffeine-ls://0123456789abcdef/source/").is_none());

        // The servable shape still resolves.
        assert!(servable("caffeine-ls://0123456789abcdef/source/com/example/Foo.java").is_some());
    }
}
