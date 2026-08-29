//! Entry point of lowering and the per-file lowering context.
//!
//! [`LowerCtx`] owns the [`ItemTree`] being built; the language-specific
//! walkers allocate items into it in CST order.

use std::sync::Arc;

use base_db::LanguageKind;
use hir_expand::{
    body::{BodyTree, LabelId},
    item_tree::{ItemData, ItemId, ItemTree, LoweredFile},
    name::Name,
};
use syntax::SourceFile;

pub mod java;
pub mod kotlin;

pub struct LowerCtx {
    pub tree: ItemTree,
    pub bodies: BodyTree,
    /// The labels currently in scope, innermost last, so that `break`/`continue`
    /// statements resolve to the [`LabelId`] of their enclosing labeled
    /// statement ([JLS §14.15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.15)).
    pub labels: Vec<(Name, LabelId)>,
}

impl LowerCtx {
    pub fn new(language: LanguageKind) -> Self {
        Self {
            tree: ItemTree {
                language,
                ..Default::default()
            },
            bodies: BodyTree::default(),
            labels: Vec::new(),
        }
    }

    pub fn alloc(&mut self, data: ItemData) -> ItemId {
        ItemId(self.tree.items.alloc(data))
    }
}

pub fn lower_source(language: LanguageKind, text: &str) -> LoweredFile {
    if language == LanguageKind::Unknown {
        return LoweredFile {
            items: Arc::new(ItemTree {
                language,
                ..Default::default()
            }),
            bodies: Arc::default(),
        };
    }

    let parse = syntax::SourceFile::parse(language, text);
    let file = parse.syntax_node(language);

    let mut ctx = LowerCtx::new(language);
    match file {
        SourceFile::Java(file) => java::lower_file(&mut ctx, &file),
        SourceFile::Kotlin(_) => kotlin::lower_file(&mut ctx),
    }
    // The range arenas are allocated lock-step with the expr/local arenas;
    // assert the alignment so a direct allocation cannot silently
    // desynchronize them.
    debug_assert_eq!(ctx.bodies.expr_ranges.len(), ctx.bodies.exprs.len());
    debug_assert_eq!(ctx.bodies.local_ranges.len(), ctx.bodies.locals.len());
    LoweredFile {
        items: Arc::new(ctx.tree),
        bodies: Arc::new(ctx.bodies),
    }
}
