//! The language-neutral file item tree: the facade every language-dispatched
//! HIR consumer reads.
//!
//! Each language lowers its own declaration model ([`crate::java::item_tree::ItemTree`],
//! [`crate::kotlin::item_tree::KotlinItemTree`]) and registers it in
//! [`crate::lang`]; the facade is the erased handle to whichever model a file
//! produced ([`crate::lang::Declarations`]). Consumers that must work for *any*
//! file take this type and obtain the language's own tree through that
//! language's accessor ([`crate::java::plugin::tree`] /
//! [`crate::kotlin::plugin::tree`]); consumers that are inherently Java-only
//! take the language's model directly and must not be handed a tree of another
//! language.
//!
//! [`LoweredFile`] pairs the item tree with the per-file body IR
//! ([`hir_expand::body::BodyTree`]), which is language-neutral and therefore
//! shared: the item tree carries no body content and no source offsets, so
//! salsa can backdate it across edits that only touch a body.

use triomphe::Arc;

use base_db::LanguageKind;
use hir_expand::body::BodyTree;

use crate::lang::{self, Declarations};

/// The lowered item tree of one file, in whichever language the file is.
#[derive(Debug, Clone)]
pub struct FileItemTree {
    /// The erased model handle (`std::sync::Arc`: `triomphe::Arc` cannot coerce
    /// to a trait object on stable Rust).
    declarations: std::sync::Arc<dyn Declarations>,
}

impl FileItemTree {
    /// The facade over a language's declaration model, as the lowering of that
    /// language produced it.
    pub(crate) fn new(declarations: std::sync::Arc<dyn Declarations>) -> Self {
        Self { declarations }
    }

    /// The facade of a file whose declarations were not lowered: an
    /// unknown-language file, or a language whose lowering has not landed yet.
    /// The carried language is the *file's*, so language-dispatched consumers
    /// still route correctly.
    pub fn empty(language: LanguageKind) -> Self {
        Self {
            declarations: lang::empty(language),
        }
    }

    /// The language of the file the tree was lowered from.
    pub fn language(&self) -> LanguageKind {
        self.declarations.language()
    }

    /// The erased declaration model, borrowed: the language-agnostic view,
    /// without touching the handle's refcount.
    pub fn declared(&self) -> &dyn Declarations {
        &*self.declarations
    }

    /// The erased declaration model, shared: for a consumer that outlives the
    /// facade it read it from.
    pub fn declarations(&self) -> std::sync::Arc<dyn Declarations> {
        std::sync::Arc::clone(&self.declarations)
    }
}

impl PartialEq for FileItemTree {
    /// The models' equality, not the handles': a body-only edit leaves the
    /// item tree's value equal, which is what lets salsa backdate it across
    /// such an edit (see [`crate::db`]).
    fn eq(&self, other: &Self) -> bool {
        self.language() == other.language() && self.declarations.dyn_eq(&*other.declarations)
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
