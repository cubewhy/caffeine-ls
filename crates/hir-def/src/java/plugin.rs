//! Java as the declaration layer's registry entry: how a Java file is lowered
//! ([`crate::java::lower::lower_java_source`]), how its model appears to the
//! language-agnostic layers ([`Declarations`]) and the typed accessor Java's
//! own layers read the model with ([`tree`]).

use std::any::Any;

use triomphe::Arc;

use base_db::LanguageKind;
use hir_expand::ast_id_map::AstIdMap;
use vfs::FileId;

use crate::{
    db::DefDatabase,
    item_tree::{FileItemTree, LoweredFile},
    java::{item_tree::ItemTree, lower::lower_java_source},
    lang::{Declarations, LangLowering},
};

pub(crate) struct Java;

pub(crate) static JAVA: Java = Java;

impl LangLowering for Java {
    fn kinds(&self) -> &'static [LanguageKind] {
        &[LanguageKind::Java]
    }

    fn lower(&self, text: &str, map: &AstIdMap) -> LoweredFile {
        let (tree, bodies) = lower_java_source(text, map);
        LoweredFile {
            items: FileItemTree::new(std::sync::Arc::new(Declared(Arc::new(tree)))),
            bodies: Arc::new(bodies),
        }
    }
}

/// The erased view of one Java file's declaration model: the model behind a
/// shared handle, so recovering it in [`tree`] and [`model`] is a refcount
/// bump rather than a copy of the tree.
#[derive(Debug, Clone, PartialEq)]
struct Declared(Arc<ItemTree>);

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

/// The Java declaration model of `file` — the accessor every Java-only layer
/// reads. A file that is not Java has no Java declarations: the model's own
/// "no declarations" answer, an empty tree.
pub fn tree(db: &dyn DefDatabase, file: FileId) -> Arc<ItemTree> {
    let items = crate::db::file_item_tree(db, file);
    items
        .declared()
        .as_any()
        .downcast_ref::<Declared>()
        .map_or_else(
            || Arc::new(ItemTree::default()),
            |declared| Arc::clone(&declared.0),
        )
}

/// The Java declaration model behind a lowered facade, `None` when the file is
/// not Java — the accessor for a consumer that has the facade but no database
/// (`crate::pretty`'s renderers).
pub fn model(items: &FileItemTree) -> Option<&ItemTree> {
    items
        .declared()
        .as_any()
        .downcast_ref::<Declared>()
        .map(|declared| &*declared.0)
}
