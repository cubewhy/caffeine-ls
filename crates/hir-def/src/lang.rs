//! The language registry of the declaration layer: one registration per
//! language for how a file's text becomes its declaration model, plus the
//! erased view of that model for the layers that must work for *any* file
//! (IntelliJ: `PsiFile`, and the file element type that produces it).
//!
//! A language is a [`LangLowering`] implementation plus a [`Declarations`]
//! model; the layers above ask the registry instead of naming a language, and
//! a model is cast back to its concrete type only inside that language's own
//! module ([`crate::java::plugin`], [`crate::kotlin::plugin`]).
//!
//! A kind is answered per *layer*: Kotlin's lowering answers for
//! [`LanguageKind::Kotlin`] and leaves [`LanguageKind::KotlinScript`] to
//! nobody, because a script's top-level statements declare no file item to
//! hang off — while the Kotlin entries of the layers that do treat a script as
//! Kotlin (its syntax, its type layer, its IDE features) answer for it.

use std::any::Any;
use std::sync::Arc;

use base_db::LanguageKind;
use hir_expand::ast_id_map::AstIdMap;
use vfs::FileId;

use crate::{db::DefDatabase, item_tree::LoweredFile};

/// The lowered declarations of one file, in whichever language (IntelliJ:
/// `PsiFile`).
pub trait Declarations: std::fmt::Debug + Send + Sync {
    /// The language of the file the declarations were lowered from.
    fn language(&self) -> LanguageKind;
    /// The concrete declaration model, for the language's own module only.
    fn as_any(&self) -> &dyn Any;
    /// Structural equality with another file's model. The item-tree query
    /// backdates an edit that only changes a body on this being `true`
    /// ([`crate::db`]), so it must compare the models, not their addresses.
    fn dyn_eq(&self, other: &dyn Declarations) -> bool;
}

/// The lowering of one language (IntelliJ: the file element type).
pub trait LangLowering: Sync {
    /// The kinds this implementation answers for.
    fn kinds(&self) -> &'static [LanguageKind];
    /// Lowers `text`, anchoring every declaration to its syntax node through
    /// `map`.
    fn lower(&self, text: &str, map: &AstIdMap) -> LoweredFile;
}

/// Every registered language, in lookup order.
static LANGUAGES: &[&dyn LangLowering] =
    &[&crate::java::plugin::JAVA, &crate::kotlin::plugin::KOTLIN];

/// The lowering of a file of `kind`, `None` for a kind no language lowers: an
/// unknown-language file, and Kotlin's `.kts` script.
pub fn lowering(kind: LanguageKind) -> Option<&'static dyn LangLowering> {
    LANGUAGES
        .iter()
        .copied()
        .find(|language| language.kinds().contains(&kind))
}

/// The declaration model of a file no language lowered.
#[derive(Debug, Clone, Copy, PartialEq)]
struct EmptyDeclarations(LanguageKind);

impl Declarations for EmptyDeclarations {
    fn language(&self) -> LanguageKind {
        self.0
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn dyn_eq(&self, other: &dyn Declarations) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

/// The empty declaration model of a file of `kind` — the model's own "no
/// declarations" answer. It still carries the file's language, so
/// language-dispatched consumers route correctly.
///
/// The erased handle is a `std::sync::Arc`: `triomphe::Arc` (which the crate
/// uses for the models themselves) cannot coerce to a trait object on stable
/// Rust, and the handle is cloned once per consumer rather than per access.
pub fn empty(kind: LanguageKind) -> Arc<dyn Declarations> {
    Arc::new(EmptyDeclarations(kind))
}

/// The declaration model of one file, for a layer that reads any language.
pub fn declarations(db: &dyn DefDatabase, file: FileId) -> Arc<dyn Declarations> {
    crate::db::file_item_tree(db, file).declarations()
}
