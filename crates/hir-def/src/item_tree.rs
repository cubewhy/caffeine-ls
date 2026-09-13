//! The language-neutral file item tree: the facade every language-dispatched
//! HIR consumer reads.
//!
//! Each language lowers its own declaration model — Java's
//! [`crate::java::item_tree::ItemTree`] today, Kotlin's alongside it — and the
//! facade hides which one a file produced behind [`FileItemTree`]. Consumers
//! that must work for *any* file take this type: the language-dispatched entry
//! points of `ide` ([`crate::db::file_item_tree`]'s callers) and the file-level
//! queries. Consumers that are inherently Java-only take the language's own
//! tree through the typed accessor (`hir::java_item_tree`) and must not be
//! handed a tree of another language.
//!
//! [`LoweredFile`] pairs the item tree with the per-file body IR
//! ([`hir_expand::body::BodyTree`]), which is language-neutral and therefore
//! shared: the item tree carries no body content and no source offsets, so
//! salsa can backdate it across edits that only touch a body.

use triomphe::Arc;

use base_db::LanguageKind;
use hir_expand::body::BodyTree;

use crate::java;
use crate::kotlin;

/// The lowered item tree of one file, in whichever language the file is.
#[derive(Debug, Clone, PartialEq)]
pub enum FileItemTree {
    /// A file whose declarations were not lowered: an unknown-language file,
    /// or a language whose lowering has not landed yet. The carried language is
    /// the *file's*, so language-dispatched consumers still route correctly.
    Empty(LanguageKind),
    /// A Java file's declaration model.
    Java(Arc<java::item_tree::ItemTree>),
    /// A Kotlin file's declaration model.
    Kotlin(Arc<kotlin::item_tree::KotlinItemTree>),
}

impl FileItemTree {
    /// The language of the file the tree was lowered from.
    pub fn language(&self) -> LanguageKind {
        match self {
            FileItemTree::Empty(language) => *language,
            FileItemTree::Java(tree) => tree.language,
            FileItemTree::Kotlin(tree) => tree.language,
        }
    }

    /// The Java declaration model of the file, or `None` when the file is not
    /// Java (or has no lowered items).
    pub fn as_java(&self) -> Option<&Arc<java::item_tree::ItemTree>> {
        match self {
            FileItemTree::Empty(_) | FileItemTree::Kotlin(_) => None,
            FileItemTree::Java(tree) => Some(tree),
        }
    }

    /// The Kotlin declaration model of the file, or `None` when the file is
    /// not Kotlin (or has no lowered items — a `.kts` script).
    pub fn as_kotlin(&self) -> Option<&Arc<kotlin::item_tree::KotlinItemTree>> {
        match self {
            FileItemTree::Empty(_) | FileItemTree::Java(_) => None,
            FileItemTree::Kotlin(tree) => Some(tree),
        }
    }
}

/// The full per-file lowering: the declaration [`FileItemTree`] plus the body
/// IR ([`hir_expand::body::BodyTree`]), lowered together in one pass so the
/// body ids stored in the item data line up with the body arenas. Computed by
/// two salsa queries (`hir_def::db::item_tree_query` /
/// `hir_def::db::body_tree_query`) and read through their
/// [`file_item_tree`](crate::db::file_item_tree) /
/// [`file_body_tree`](crate::db::file_body_tree) accessors. Because the item
/// tree carries no body content, edits that only change a method body leave
/// its value unchanged, letting salsa backdate signature consumers
/// (`file_symbols_query`, `supertypes_query`, ...) instead of re-running them.
#[derive(Debug, Clone, PartialEq)]
pub struct LoweredFile {
    pub items: FileItemTree,
    pub bodies: Arc<BodyTree>,
}

/// The lowercase name of a language, as the snapshot renderers spell it.
pub fn language_name(language: LanguageKind) -> &'static str {
    match language {
        LanguageKind::Java => "java",
        LanguageKind::Kotlin | LanguageKind::KotlinScript => "kotlin",
        LanguageKind::Unknown => "unknown",
    }
}
