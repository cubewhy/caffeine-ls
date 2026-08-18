//! Entry point of lowering and the per-file lowering context.
//!
//! [`LowerCtx`] owns the [`ItemTree`] being built; the language-specific
//! walkers allocate items into it in CST order.

use base_db::LanguageKind;
use hir_expand::item_tree::{ItemData, ItemId, ItemTree};
use syntax::SourceFile;

pub mod java;
pub mod kotlin;

pub struct LowerCtx {
    pub tree: ItemTree,
}

impl LowerCtx {
    pub fn new(language: LanguageKind) -> Self {
        Self {
            tree: ItemTree {
                language,
                ..Default::default()
            },
        }
    }

    pub fn alloc(&mut self, data: ItemData) -> ItemId {
        ItemId(self.tree.items.alloc(data))
    }
}

pub fn lower_source(language: LanguageKind, text: &str) -> ItemTree {
    if language == LanguageKind::Unknown {
        return ItemTree {
            language,
            ..Default::default()
        };
    }

    let parse = syntax::SourceFile::parse(language, text);
    let file = parse.syntax_node(language);

    let mut ctx = LowerCtx::new(language);
    match file {
        SourceFile::Java(file) => java::lower_file(&mut ctx, &file),
        SourceFile::Kotlin(_) => kotlin::lower_file(&mut ctx),
    }
    ctx.tree
}
