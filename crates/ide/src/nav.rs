//! Navigation over the HIR, dispatched by the file's language:
//! goto-definition, references, hover, and the library-file deferral that
//! feeds them.
//!
//! The surface is language-neutral — [`NavigationTarget`], [`HoverInfo`],
//! [`ReferenceTarget`], [`LibraryFileRef`] — and every request dispatches on
//! the language of the file's item tree: Java (and an `Unknown` file, whose
//! empty item tree answers nothing) to the `java` module, Kotlin to the
//! `kotlin` one.
//!
//! The Java resolution semantics — which declaration a reference denotes, and
//! why — are documented where they are implemented, in the `java` module. What
//! both languages share is the deferral the LSP layer drives with
//! [`LibraryFileRef`]: a reference that resolves into a library declaration
//! whose source is not loaded yet is reported as pending rather than answered,
//! and the LSP layer reads the file — the archive entry, or the class, when
//! the library ships no sources at all — into the database and re-runs the
//! request.

use rowan::{TextRange, TextSize};
use triomphe::Arc;
use vfs::{AbsPathBuf, FileId};

use crate::RootDatabase;
use ide_db::base_db::LanguageKind;

mod java;
mod kotlin;

/// The declaration a reference resolves to: a file and the source range of
/// the declaring construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationTarget {
    pub file: FileId,
    /// The source range of the declaration's own *name* token — the identifier
    /// an editor jumps to and selects. Deliberately not the whole declaration:
    /// a class's range contains every reference to it, so a definition that
    /// covered the whole class would leave the cursor inside its own target,
    /// and a client that treats "already inside the definition" as a no-op
    /// would never move — the usual case for the JDK's sources, where `String`
    /// is written inside `String`.
    pub range: TextRange,
    pub name: String,
}

/// A hover result: a rendered signature or type, plus the documentation of the
/// declaration it describes, rendered as Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInfo {
    pub value: String,
    /// The hovered declaration's documentation, rendered as Markdown; `None`
    /// when the declaration has none, or its language's arm has none.
    pub docs: Option<String>,
}

/// One reference site: a file and the range of the *name token* at the site —
/// the identifier a client highlights, not the expression containing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceTarget {
    pub file: FileId,
    pub range: TextRange,
}

/// A library file a reference resolves into but which is not loaded into the
/// database yet: either the archive entry holding a class's sources, or the
/// class a decompiler has to produce a view of. The LSP layer materializes
/// `path` and re-runs the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryFileRef {
    /// An entry to read out of the archive.
    Source {
        library: hir::LibraryId,
        archive: AbsPathBuf,
        entry: Arc<str>,
        path: AbsPathBuf,
    },
    /// A class to decompile into `path`.
    Decompile {
        library: hir::LibraryId,
        class: Arc<str>,
        path: AbsPathBuf,
    },
}

impl LibraryFileRef {
    /// What makes two refs the same load: the library plus the archive entry,
    /// or the class to decompile.
    fn load_key(&self) -> (hir::LibraryId, Arc<str>) {
        match self {
            LibraryFileRef::Source { library, entry, .. } => (*library, Arc::clone(entry)),
            LibraryFileRef::Decompile { library, class, .. } => (*library, Arc::clone(class)),
        }
    }
}

/// The declarations the reference at `offset` resolves to ([JLS §6.5] in a Java
/// file, nothing at all in a Kotlin one).
pub fn definition(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<NavigationTarget> {
    match hir::file_item_tree(db, file).language() {
        LanguageKind::Kotlin | LanguageKind::KotlinScript => kotlin::definition(db, file, offset),
        // `Unknown` is a file with no source root yet (opened before the
        // workspace loaded) or a non-JVM file; it lowers to an empty item tree,
        // so the Java path finds nothing.
        _ => java::definition(db, file, offset),
    }
}

/// The reference sites of the declaration(s) the reference at `offset` names —
/// the LSP `textDocument/references` result. `include_declaration` adds each
/// declaration's own name token.
pub fn references(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
    include_declaration: bool,
) -> Vec<ReferenceTarget> {
    match hir::file_item_tree(db, file).language() {
        LanguageKind::Kotlin | LanguageKind::KotlinScript => kotlin::references(db, file, offset),
        _ => java::references(db, file, offset, include_declaration),
    }
}

/// The library files the reference at `offset` resolves into but which are
/// not loaded into the database yet, in resolution order. The LSP layer reads
/// each one — an archive entry out of its archive, or a class through the
/// decompiler — into `path` and re-runs the request.
pub fn pending_library_files(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
) -> Vec<LibraryFileRef> {
    match hir::file_item_tree(db, file).language() {
        LanguageKind::Kotlin | LanguageKind::KotlinScript => {
            kotlin::pending_library_files(db, file, offset)
        }
        _ => java::pending_library_files(db, file, offset),
    }
}

/// The hover at `offset`, resolved by the file's language: the declaration a
/// reference names, the type of the expression the offset is inside, or the
/// declaration whose own name the offset is on — nothing for an offset that
/// names nothing.
pub fn hover(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<HoverInfo> {
    match hir::file_item_tree(db, file).language() {
        LanguageKind::Kotlin | LanguageKind::KotlinScript => kotlin::hover(db, file, offset),
        _ => java::hover(db, file, offset),
    }
}

/// The declaration of the class-like type `fqn` names in `file`'s scope — the
/// declaration a click on an inlay hint's type label navigates to. Nothing for
/// a Kotlin file (see [`kotlin`]) and for a name the file's scope cannot
/// resolve to a declaration.
pub(crate) fn class_declaration(
    db: &RootDatabase,
    file: FileId,
    fqn: &str,
) -> Option<NavigationTarget> {
    match hir::file_item_tree(db, file).language() {
        LanguageKind::Kotlin | LanguageKind::KotlinScript => {
            kotlin::class_declaration(db, file, fqn)
        }
        _ => java::class_declaration(db, file, fqn),
    }
}

/// The parameter names the declaration the invocation `method` selected writes
/// ([JLS §8.4.1]), for a caller that has none of its own — the inlay-hint
/// layer's parameter-name hints. `constructor` says whether the invocation is a
/// class instance creation or an explicit constructor invocation ([§15.9],
/// [§8.8.7.1]), which selects a constructor rather than a method of that name.
///
/// `None` for a Kotlin file (nothing resolves: see [`kotlin`]) and for a
/// declaration that is no loaded source one — see
/// [`java::declared_parameter_names`].
pub(crate) fn declared_parameter_names(
    db: &RootDatabase,
    file: FileId,
    method: &hir_ty::MethodData,
    constructor: bool,
) -> Option<Vec<String>> {
    match hir::file_item_tree(db, file).language() {
        LanguageKind::Kotlin | LanguageKind::KotlinScript => None,
        _ => java::declared_parameter_names(db, file, method, constructor),
    }
}

/// The library file that has to be loaded before the invocation `method`'s
/// declaration can be named — the pending source of its declaring class, when
/// there is one. The inlay-hint layer collects these over a request's range and
/// defers through them exactly as goto-definition and hover do, so a library
/// member's parameter names render on the first request rather than only once
/// the declaration's source happens to be open.
///
/// `None` for a Kotlin file and for a member no load can name.
pub(crate) fn pending_parameter_names(
    db: &RootDatabase,
    file: FileId,
    method: &hir_ty::MethodData,
    constructor: bool,
) -> Option<LibraryFileRef> {
    match hir::file_item_tree(db, file).language() {
        LanguageKind::Kotlin | LanguageKind::KotlinScript => None,
        _ => java::pending_parameter_names(db, file, method, constructor),
    }
}
