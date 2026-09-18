//! Kotlin as the declaration layer's registry entry: how a Kotlin file is
//! lowered ([`crate::kotlin::lower::lower_kotlin_source`]), how its model
//! appears to the language-agnostic layers ([`Declarations`]) and the typed
//! accessor Kotlin's own layers read the model with ([`tree`]).

use std::any::Any;

use triomphe::Arc;

use base_db::LanguageKind;
use hir_expand::ast_id_map::AstIdMap;
use vfs::FileId;

use crate::{
    db::DefDatabase,
    item_tree::{FileItemTree, LoweredFile},
    kotlin::{item_tree::KotlinItemTree, lower::lower_kotlin_source},
    lang::{Declarations, LangLowering},
};

pub(crate) struct Kotlin;

pub(crate) static KOTLIN: Kotlin = Kotlin;

impl LangLowering for Kotlin {
    fn kinds(&self) -> &'static [LanguageKind] {
        // A `.kts` script lowers with the same walk as a `.kt` file, plus the
        // body of its implicit `main`
        // (<https://kotlinlang.org/docs/command-line.html#run-scripts>).
        &[LanguageKind::Kotlin, LanguageKind::KotlinScript]
    }

    fn lower(&self, kind: LanguageKind, text: &str, map: &AstIdMap) -> LoweredFile {
        let (tree, bodies) = lower_kotlin_source(kind, text, map);
        LoweredFile {
            items: FileItemTree::new(std::sync::Arc::new(Declared(Arc::new(tree)))),
            bodies: Arc::new(bodies),
        }
    }
}

/// The erased view of one Kotlin file's declaration model: the model behind a
/// shared handle, so recovery in [`tree`] and [`model`] is a refcount bump
/// rather than a copy of the tree.
#[derive(Debug, Clone, PartialEq)]
struct Declared(Arc<KotlinItemTree>);

impl Declarations for Declared {
    fn language(&self) -> LanguageKind {
        self.0.language
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn dyn_eq(&self, other: &dyn Declarations) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

/// The Kotlin declaration model of `file`, `None` when the file is not Kotlin
/// — a `.kts` script included, whose model carries its top-level declarations
/// and the [`KotlinItemTree::script_body`] of its implicit `main`.
pub fn tree(db: &dyn DefDatabase, file: FileId) -> Option<Arc<KotlinItemTree>> {
    let items = crate::db::file_item_tree(db, file);
    items
        .declared()
        .as_any()
        .downcast_ref::<Declared>()
        .map(|declared| Arc::clone(&declared.0))
}

/// The Kotlin declaration model behind a lowered facade, `None` when the file
/// is not Kotlin — the accessor for a consumer that has the facade but no
/// database (`crate::pretty`'s renderers).
pub fn model(items: &FileItemTree) -> Option<&KotlinItemTree> {
    items
        .declared()
        .as_any()
        .downcast_ref::<Declared>()
        .map(|declared| &*declared.0)
}
