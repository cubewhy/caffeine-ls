mod change;
mod input;
mod parse;

pub use change::FileChange;
pub use input::{DepsMap, SourceRoot, SourceRootId};
pub use parse::parse;
pub use salsa;
pub use syntax::LanguageKind;

use rowan::TextSize;
use vfs::{AnchoredPath, FileId, file_set::FileSet};

use std::{hash::BuildHasherDefault, sync::atomic::AtomicUsize};

use dashmap::DashMap;
use rustc_hash::{FxHashSet, FxHasher};
use salsa::{Durability, Setter};
use triomphe::Arc;

#[salsa::input(debug)]
pub struct FileText {
    #[returns(ref)]
    pub text: Arc<str>,
    pub file_id: vfs::FileId,
}

/// The reserved source root for documents that hold text but are not yet
/// claimed by any applied workspace layout. Its file set is empty; it exists
/// only so every file with text has a [`FileSourceRootInput`] salsa input,
/// keeping `file_source_root` total. Attaching the document to a real root
/// later replaces this provisional mapping, which is a salsa change: tracked
/// queries that derived the language from the file's root (see
/// [`file_language_kind_query`]) invalidate and recompute instead of serving a
/// stale result built before the workspace was loaded.
pub const FALLBACK_SOURCE_ROOT: SourceRootId = SourceRootId(u32::MAX);

#[derive(Debug, Default)]
pub struct Files {
    files: Arc<DashMap<vfs::FileId, FileText, BuildHasherDefault<FxHasher>>>,
    source_roots: Arc<DashMap<SourceRootId, SourceRootInput, BuildHasherDefault<FxHasher>>>,
    file_source_roots: Arc<DashMap<vfs::FileId, FileSourceRootInput, BuildHasherDefault<FxHasher>>>,
}

impl Files {
    pub fn file_text(&self, file_id: vfs::FileId) -> FileText {
        match self.files.get(&file_id) {
            Some(text) => *text,
            None => {
                panic!("Unable to fetch file text for `vfs::FileId`: {file_id:?}; this is a bug")
            }
        }
    }

    pub fn set_file_text(&self, db: &mut dyn SourceDatabase, file_id: vfs::FileId, text: &str) {
        self.ensure_file_source_root(db, file_id);
        match self.files.get(&file_id) {
            Some(current) => {
                let current = *current;
                if *current.text(db) == Arc::from(text) {
                    return;
                }
                current.set_text(db).to(Arc::from(text));
            }
            None => {
                let text = FileText::new(db, Arc::from(text), file_id);
                self.files.insert(file_id, text);
            }
        };
    }

    pub fn set_file_text_with_durability(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: vfs::FileId,
        text: &str,
        durability: Durability,
    ) {
        self.ensure_file_source_root(db, file_id);
        match self.files.get(&file_id) {
            Some(current) => {
                let current = *current;
                // Setting the same text again is a no-op: salsa 0.28 does not
                // deduplicate equal writes, and every set is a new revision
                // (which invalidates every memo reading the input).
                if *current.text(db) == Arc::from(text) {
                    return;
                }
                current
                    .set_text(db)
                    .with_durability(durability)
                    .to(Arc::from(text));
            }
            None => {
                let text = FileText::builder(Arc::from(text), file_id)
                    .durability(durability)
                    .new(db);
                self.files.insert(file_id, text);
            }
        };
    }

    /// Source root of the file.
    pub fn source_root(&self, source_root_id: SourceRootId) -> SourceRootInput {
        let source_root = match self.source_roots.get(&source_root_id) {
            Some(source_root) => source_root,
            None => panic!(
                "Unable to fetch `SourceRootInput` with `SourceRootId` ({source_root_id:?}); this is a bug"
            ),
        };

        *source_root
    }

    pub fn set_source_root_with_durability(
        &self,
        db: &mut dyn SourceDatabase,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    ) {
        match self.source_roots.get(&source_root_id) {
            Some(current) => {
                let current = *current;
                // Re-setting an unchanged root is a no-op (see
                // [`Self::set_file_text_with_durability`]).
                if *current.source_root(db) == source_root {
                    return;
                }
                current
                    .set_source_root(db)
                    .with_durability(durability)
                    .to(source_root);
            }
            None => {
                let source_root = SourceRootInput::builder(source_root)
                    .durability(durability)
                    .new(db);
                self.source_roots.insert(source_root_id, source_root);
            }
        };
    }

    pub fn file_source_root(
        &self,
        db: &dyn SourceDatabase,
        id: vfs::FileId,
    ) -> FileSourceRootInput {
        let file_source_root = match self.file_source_roots.get(&id) {
            Some(file_source_root) => file_source_root,
            None => panic!(
                "Unable to get `FileSourceRootInput` with `vfs::FileId` ({id:?}, path: {}); this is a bug",
                self.path_for_file(db, id)
                    .map_or_else(|| "<unknown>".to_owned(), |path| path.to_string()),
            ),
        };
        *file_source_root
    }

    /// Non-panicking variant of [`Files::file_source_root`]: the source root
    /// of `id`, or `None` when the file does not belong to any root (e.g. a
    /// file opened before the workspace is loaded).
    pub fn file_source_root_id(
        &self,
        db: &dyn SourceDatabase,
        id: vfs::FileId,
    ) -> Option<SourceRootId> {
        self.file_source_roots
            .get(&id)
            .map(|entry| *entry)
            .map(|input| *input.source_root_id(db))
    }

    /// The registered source-root ids, including the catch-all fallback root
    /// that files outside the workspace attach to.
    pub fn source_root_ids(&self) -> impl Iterator<Item = SourceRootId> + '_ {
        self.source_roots.iter().map(|entry| *entry.key())
    }

    /// Ensures `file_id` has a [`FileSourceRootInput`] salsa input, so
    /// [`SourceDatabase::file_source_root`] is total for every file that holds
    /// text. Documents not yet claimed by an applied source root are
    /// attributed to [`FALLBACK_SOURCE_ROOT`]; attaching them to a real root
    /// later (see [`Self::set_file_source_root_with_durability`]) is a salsa
    /// change that invalidates the tracked [`file_language_kind_query`].
    fn ensure_file_source_root(&self, db: &mut dyn SourceDatabase, file_id: vfs::FileId) {
        if self.file_source_roots.get(&file_id).is_some() {
            return;
        }
        if !self.source_roots.contains_key(&FALLBACK_SOURCE_ROOT) {
            let root = SourceRootInput::builder(Arc::new(SourceRoot::new(FileSet::default())))
                .durability(Durability::LOW)
                .new(db);
            self.source_roots.insert(FALLBACK_SOURCE_ROOT, root);
        }
        let file_root = FileSourceRootInput::builder(FALLBACK_SOURCE_ROOT)
            .durability(Durability::LOW)
            .new(db);
        self.file_source_roots.insert(file_id, file_root);
    }

    fn path_for_file(&self, db: &dyn SourceDatabase, id: vfs::FileId) -> Option<vfs::VfsPath> {
        for source_root in &*self.source_roots {
            let source_root = *source_root.value();
            if let Some(path) = source_root.source_root(db).path_for_file(&id) {
                return Some(path.clone());
            }
        }
        None
    }

    pub fn set_file_source_root_with_durability(
        &self,
        db: &mut dyn SourceDatabase,
        id: vfs::FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        match self.file_source_roots.get(&id) {
            Some(current) => {
                let current = *current;
                // Re-setting an unchanged mapping is a no-op (see
                // [`Self::set_file_text_with_durability`]).
                if *current.source_root_id(db) == source_root_id {
                    return;
                }
                current
                    .set_source_root_id(db)
                    .with_durability(durability)
                    .to(source_root_id);
            }
            None => {
                let file_source_root = FileSourceRootInput::builder(source_root_id)
                    .durability(durability)
                    .new(db);
                self.file_source_roots.insert(id, file_source_root);
            }
        };
    }
}

#[salsa::db]
pub trait SourceDatabase: salsa::Database {
    /// Text of the file.
    fn file_text(&self, file_id: vfs::FileId) -> FileText;

    fn set_file_text(&mut self, file_id: vfs::FileId, text: &str);

    fn set_file_text_with_durability(
        &mut self,
        file_id: vfs::FileId,
        text: &str,
        durability: Durability,
    );

    /// Contents of the source root.
    fn source_root(&self, id: SourceRootId) -> SourceRootInput;

    fn file_source_root(&self, id: vfs::FileId) -> FileSourceRootInput;

    /// The source root of `file_id`, if it belongs to any source root.
    /// Unlike [`Self::file_source_root`] this never panics, so it is safe to
    /// call for files outside the workspace.
    fn source_root_for_file(&self, file_id: vfs::FileId) -> Option<SourceRootId>;

    fn set_file_source_root_with_durability(
        &mut self,
        id: vfs::FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    );

    /// Source root of the file.
    fn set_source_root_with_durability(
        &mut self,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    );

    fn resolve_path(&self, path: AnchoredPath<'_>) -> Option<FileId> {
        // FIXME: this *somehow* should be platform agnostic...
        let source_root = self.file_source_root(path.anchor);
        let source_root = self.source_root(*source_root.source_root_id(self));
        source_root.source_root(self).resolve_path(path)
    }

    /// The language kind of the file, derived from its source root. The
    /// default is a tracked read (see [`file_language_kind`]), so attaching a
    /// source root to a later-open document invalidates dependent queries
    /// instead of serving a stale `Unknown` result.
    fn file_language_kind(&self, file_id: vfs::FileId) -> Option<LanguageKind>
    where
        Self: Sized,
    {
        file_language_kind(self, file_id)
    }

    #[doc(hidden)]
    fn deps_map(&self) -> Arc<DepsMap>;

    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision);

    fn line_column(&self, file: FileId, offset: TextSize) -> Result<(u32, u32), ()>;
}

static NEXT_NONCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nonce(usize);

impl Default for Nonce {
    #[inline]
    fn default() -> Self {
        Nonce::new()
    }
}

impl Nonce {
    #[inline]
    pub fn new() -> Nonce {
        Nonce(NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }
}

#[salsa::input(debug)]
pub struct SourceRootInput {
    pub source_root: Arc<SourceRoot>,
}

#[salsa::input(debug)]
pub struct FileSourceRootInput {
    pub source_root_id: SourceRootId,
}

/// The interned file id `salsa` key of [`SourceDatabase::file_language_kind`].
#[salsa::interned]
struct InternedFileId {
    #[returns(copy)]
    file_id: vfs::FileId,
}

/// The [`LanguageKind`] of a file, derived from the owning source root's file
/// set. This is a *tracked* read — keyed on the interned file, it touches only
/// the salsa inputs [`SourceDatabase::file_source_root`] and
/// [`SourceDatabase::source_root`] — so attaching a document to a source root
/// (e.g. when the workspace finishes loading over an already-open file)
/// invalidates the result and everything built from it, instead of replaying a
/// stale `Unknown` lowered before the workspace was ready. Returns `None`
/// while the file has no source root; such files lower as
/// [`LanguageKind::Unknown`].
#[salsa::tracked(returns(ref))]
fn file_language_kind_query<'db>(
    db: &'db dyn SourceDatabase,
    file: InternedFileId<'db>,
) -> Option<LanguageKind> {
    let file_id = file.file_id(db);
    let file_root = db.file_source_root(file_id);
    let root = db.source_root(*file_root.source_root_id(db));
    let path = root.source_root(db).path_for_file(&file_id)?;
    Some(LanguageKind::from_path(&path.to_string()))
}

/// The [`LanguageKind`] of a file. Free-function form of a tracked read that
/// also works through `&dyn SourceDatabase` (the trait method requires
/// `Self: Sized`).
pub fn file_language_kind(db: &dyn SourceDatabase, file_id: vfs::FileId) -> Option<LanguageKind> {
    *file_language_kind_query(db, InternedFileId::new(db, file_id))
}

/// The set of "local" (that is, from the current workspace) roots.
/// Files in local roots are assumed to change frequently.
#[salsa::input(singleton, debug)]
pub struct LocalRoots {
    #[returns(ref)]
    pub roots: FxHashSet<SourceRootId>,
}
