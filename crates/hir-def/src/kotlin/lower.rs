//! Entry point of the Kotlin lowering and its per-file context.
//!
//! [`LowerCtx`] owns the [`KotlinItemTree`] being built; the walker
//! ([`walk`]) allocates items into it in CST order, and the body lowering
//! ([`body`], landing with the Kotlin body IR) fills the per-file body IR.
//! Lowering is a pure function of the parsed file and its [`AstIdMap`],
//! computed once per file by a salsa query ([`crate::db`]).
//!
//! # Reference
//!
//! The lowering follows the KLS *Kotlin/Core* grammar, cited per production in
//! [`walk`]:
//!
//! * `syntax-and-grammar.html` — the productions themselves
//!   (`classDeclaration`, `objectDeclaration`, `companionObject`,
//!   `functionDeclaration`, `propertyDeclaration`, `variableDeclaration`,
//!   `multiVariableDeclaration`, `getter`, `setter`, `typeAlias`,
//!   `typeParameters`, `typeParameter`, `delegationSpecifiers`,
//!   `packageHeader`, `importHeader`);
//! * `declarations.html` — the declaration rules (classifier kinds,
//!   visibility, type-parameter variance, extension declarations, constant and
//!   late-initialized properties, delegated properties);
//! * `packages-and-imports.html` — packages and imports.

use base_db::LanguageKind;
use hir_expand::{ast_id_map::AstIdMap, body::BodyTree};
use syntax::SourceFile;

use crate::kotlin::item_tree::{ItemId, KotlinItemData, KotlinItemTree};

pub(super) mod body;
pub(super) mod walk;

/// The per-file lowering context of the Kotlin walker: owns the
/// [`KotlinItemTree`] and [`BodyTree`] being built, and the file's
/// [`AstIdMap`], from which every declaration's
/// [`FileAstId`](hir_expand::ast_id_map::FileAstId) is resolved.
pub(in crate::kotlin) struct LowerCtx<'a> {
    pub tree: KotlinItemTree,
    pub bodies: BodyTree,
    pub map: &'a AstIdMap,
}

impl<'a> LowerCtx<'a> {
    pub fn new(map: &'a AstIdMap) -> LowerCtx<'a> {
        LowerCtx {
            tree: KotlinItemTree {
                language: LanguageKind::Kotlin,
                ..Default::default()
            },
            bodies: BodyTree::default(),
            map,
        }
    }

    pub fn alloc(&mut self, data: KotlinItemData) -> ItemId {
        self.tree.alloc(data)
    }
}

/// Lowers a Kotlin file into its declaration model plus body IR, anchoring
/// every declaration to its syntax node through `map`. The language-dispatched
/// entry point is [`crate::lower::lower_source`], which reaches this one
/// through [`crate::kotlin::plugin::KOTLIN`]; this one parses the text as
/// Kotlin.
///
/// `.kts` scripts are *not* lowered: a script's top-level statements have no
/// file item to hang off (see [`crate::lower::lower_source`]).
pub fn lower_kotlin_source(text: &str, map: &AstIdMap) -> (KotlinItemTree, BodyTree) {
    let parse = syntax::SourceFile::parse(LanguageKind::Kotlin, text);
    let file = parse.syntax_node(LanguageKind::Kotlin);
    let SourceFile::Kotlin(file) = &file else {
        unreachable!("SourceFile::parse(Kotlin) yields a Kotlin file")
    };

    let mut ctx = LowerCtx::new(map);
    walk::lower_file(&mut ctx, file);
    // The range arenas are allocated lock-step with the expr/local arenas;
    // assert the alignment so a direct allocation cannot silently
    // desynchronize them. (Both are empty until the body lowering lands.)
    debug_assert_eq!(ctx.bodies.expr_ranges.len(), ctx.bodies.exprs.len());
    debug_assert_eq!(ctx.bodies.local_ranges.len(), ctx.bodies.locals.len());
    debug_assert_eq!(ctx.tree.parent.len(), ctx.tree.items.len());

    let LowerCtx {
        mut tree, bodies, ..
    } = ctx;
    record_nesting(&mut tree);
    (tree, bodies)
}

/// Records, after the walk, the structural relation the item tree does not get
/// for free: every item's *parent*. A *local* declaration — a local class,
/// function, type alias or object literal ([KLS
/// `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration))
/// — records its own parent as the body walker lowers it, because only the
/// body knows which declaration declares it.
///
/// The tree stores each declaration's members as a `body` list, each property's
/// accessors as an `accessors` list, and a classifier's primary constructor in
/// its own field; the reverse edges are filled here, from those lists alone —
/// nothing here depends on source *offsets*, so a body-only edit leaves the
/// relation (and therefore the tree's value) unchanged.
fn record_nesting(tree: &mut KotlinItemTree) {
    let mut parents: Vec<(ItemId, ItemId)> = Vec::new();
    for (id, data) in tree.items.iter() {
        let parent = ItemId(id);
        for &child in data.body() {
            parents.push((child, parent));
        }
        if let KotlinItemData::Class(class) = data
            && let Some(constructor) = class.primary_constructor
        {
            parents.push((constructor, parent));
        }
        if let KotlinItemData::Property(property) = data {
            for &accessor in &property.accessors {
                parents.push((accessor, parent));
            }
        }
    }
    for (child, parent) in parents {
        tree.parent[child.0.0 as usize] = Some(parent);
    }
}
