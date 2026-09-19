//! Entry point of the Kotlin lowering and its per-file context.
//!
//! [`LowerCtx`] owns the [`KotlinItemTree`] being built; the walker
//! ([`walk`]) allocates items into it in CST order, and the body lowering
//! ([`body`]) fills the per-file body IR while the walker visits the bodies.
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

/// Escaped and ordinary identifiers denote the same name; syntax ranges stay raw.
/// https://kotlinlang.org/spec/syntax-and-grammar.html#identifiers
pub(super) fn identifier_text(text: &str) -> &str {
    text.strip_prefix('`')
        .and_then(|inner| inner.strip_suffix('`'))
        .unwrap_or(text)
}

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
    pub fn new(language: LanguageKind, map: &'a AstIdMap) -> LowerCtx<'a> {
        LowerCtx {
            tree: KotlinItemTree {
                language,
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

/// The *owner* the walker is given for a body no declaration owns: the body of
/// a `.kts` script's implicit `main`
/// (<https://kotlinlang.org/docs/command-line.html#run-scripts>), which the
/// file itself declares ([`KotlinItemTree::script_body`]).
///
/// The walker threads the declaration whose body it is lowering through every
/// statement and expression, and *uses* it at exactly two points — the
/// [`Body::owner`](hir_expand::body::Body::owner) of an allocated body and the
/// parent of a local declaration ([`walk::record_local`]). Both read this
/// value as "no owning declaration": a script's body records `None`, and a
/// local declaration its statements declare keeps a `None` parent, because
/// neither is nested in anything. The value therefore never reaches the item
/// tree, and no item is ever allocated with it — a script declares no item for
/// itself.
pub(in crate::kotlin) const NO_OWNER: ItemId = ItemId(hir_expand::arena::ArenaId(u32::MAX));

/// Lowers a Kotlin file into its declaration model plus body IR, anchoring
/// every declaration to its syntax node through `map`. The language-dispatched
/// entry point is [`crate::lower::lower_source`], which reaches this one
/// through [`crate::kotlin::plugin::KOTLIN`]; this one parses the text with the
/// production `language` names — `kotlinFile` for
/// [`LanguageKind::Kotlin`], `script` for a `.kts`
/// [`LanguageKind::KotlinScript`].
///
/// A script's top-level *declarations* are the file's top-level declarations
/// and its top-level *statements* are the body of the implicit `main` the
/// compiler wraps them in
/// (<https://kotlinlang.org/docs/command-line.html#run-scripts>), recorded as
/// [`KotlinItemTree::script_body`]. A `.kt` file declares no such body.
pub fn lower_kotlin_source(
    language: LanguageKind,
    text: &str,
    map: &AstIdMap,
) -> (KotlinItemTree, BodyTree) {
    let parse = syntax::SourceFile::parse(language, text);
    let file = parse.syntax_node(language);
    let SourceFile::Kotlin(file) = &file else {
        unreachable!("SourceFile::parse(Kotlin) yields a Kotlin file")
    };

    let mut ctx = LowerCtx::new(language, map);
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
