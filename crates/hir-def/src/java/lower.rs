//! Entry point of Java lowering and the per-file lowering context.
//!
//! [`LowerCtx`] owns the [`ItemTree`] being built; the walker ([`walk`])
//! allocates items into it in CST order, and the body lowering ([`body`])
//! fills the per-file body IR. Lowering is a pure function of the parsed
//! file and its [`AstIdMap`], computed once per file by a salsa query
//! ([`crate::db`]).

use triomphe::Arc;

use base_db::LanguageKind;
use hir_expand::{
    ast_id_map::AstIdMap,
    body::{BodyTree, LabelId, StmtData, StmtId},
    name::Name,
};
use stacksafe::stacksafe;
use syntax::SourceFile;

use super::item_tree::{ItemData, ItemId, ItemTree, LoweredFile};
use crate::java::ranges;

pub(super) mod body;
pub(super) mod walk;

/// The per-file lowering context of the Java walker: owns the [`ItemTree`]
/// and [`BodyTree`] being built, and the file's [`AstIdMap`], from which
/// every declaration's [`FileAstId`](hir_expand::ast_id_map::FileAstId) is
/// resolved. Java-internal; the Kotlin lowering will have its own context.
pub(in crate::java) struct LowerCtx<'a> {
    pub tree: ItemTree,
    pub bodies: BodyTree,
    /// The labels currently in scope, innermost last, so that `break`/`continue`
    /// statements resolve to the [`LabelId`] of their enclosing labeled
    /// statement ([JLS §14.15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.15)).
    pub labels: Vec<(Name, LabelId)>,
    pub map: &'a AstIdMap,
}

impl<'a> LowerCtx<'a> {
    pub fn new(language: LanguageKind, map: &'a AstIdMap) -> LowerCtx<'a> {
        LowerCtx {
            tree: ItemTree {
                language,
                ..Default::default()
            },
            bodies: BodyTree::default(),
            labels: Vec::new(),
            map,
        }
    }

    pub fn alloc(&mut self, data: ItemData) -> ItemId {
        self.tree.alloc(data)
    }
}

/// Lowers `text` for `language` into the file's item tree plus body IR,
/// anchoring every declaration to its syntax node through `map`.
///
/// Kotlin files produce an empty item tree for now; the Kotlin CST is parsed
/// but not yet lowered ([`crate::kotlin::lower`]).
pub fn lower_source(language: LanguageKind, text: &str, map: &AstIdMap) -> LoweredFile {
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

    let mut ctx = LowerCtx::new(language, map);
    match &file {
        SourceFile::Java(file) => walk::lower_file(&mut ctx, file),
        SourceFile::Kotlin(_) => {
            // TODO(kotlin): lower the Kotlin CST into an item tree on top of
            // the JVM substrate; see crate::kotlin::lower.
        }
    }
    // The range arenas are allocated lock-step with the expr/local arenas;
    // assert the alignment so a direct allocation cannot silently
    // desynchronize them.
    debug_assert_eq!(ctx.bodies.expr_ranges.len(), ctx.bodies.exprs.len());
    debug_assert_eq!(ctx.bodies.expr_name_ranges.len(), ctx.bodies.exprs.len());
    debug_assert_eq!(ctx.bodies.local_ranges.len(), ctx.bodies.locals.len());
    debug_assert_eq!(ctx.bodies.local_name_ranges.len(), ctx.bodies.locals.len());
    debug_assert_eq!(ctx.tree.parent.len(), ctx.tree.items.len());

    let LowerCtx {
        mut tree, bodies, ..
    } = ctx;
    record_nesting(&mut tree, &bodies, &file, map);
    LoweredFile {
        items: Arc::new(tree),
        bodies: Arc::new(bodies),
    }
}

/// Records, after the walk, the two structural relations the item tree does
/// not get for free:
///
/// - every item's *parent*: a member's enclosing type-like declaration, and a
///   local type declaration's body-owning declaration ([§14.3]);
/// - the file's `local_types` list, in source order.
///
/// Both are derived from the tree itself plus the body IR — nothing here
/// depends on source *offsets* except the sort of `local_types` (which is a
/// function of the declaration structure, so it is stable across body edits).
fn record_nesting(tree: &mut ItemTree, bodies: &BodyTree, source: &SourceFile, map: &AstIdMap) {
    // Every local type declaration is declared by a statement of some body;
    // its parent is that body's owner.
    let mut local_types = Vec::new();
    for (_, body) in bodies.bodies.iter() {
        let Some(owner) = body.owner else {
            continue;
        };
        let first = local_types.len();
        for stmt in &body.stmts {
            collect_local_decls(bodies, *stmt, &mut local_types);
        }
        for item in &local_types[first..] {
            tree.parent[item.0.0 as usize] = Some(owner);
        }
    }
    // The bodies are visited in allocation order, which interleaves a local
    // declaration with the declarations nested in the local types declared
    // before it; sorting by source position restores the reader's order.
    local_types.sort_by_key(|item| {
        ranges::item_range(map, source, tree, *item)
            .map(|range| range.start())
            .unwrap_or_default()
    });
    tree.local_types = local_types;

    // A member's parent is the declaration that holds it — a top-level item
    // has none. Local type declarations are not members of anything, so they
    // seed the walk alongside the top-level items rather than being reached
    // through a `body()`.
    let mut stack: Vec<ItemId> = tree
        .top
        .iter()
        .chain(tree.local_types.iter())
        .copied()
        .collect();
    while let Some(id) = stack.pop() {
        let members = tree.data(id).body().to_vec();
        for member in members {
            tree.parent[member.0.0 as usize] = Some(id);
            stack.push(member);
        }
    }
}

/// Collects the local type declarations declared by `stmt` and its nested
/// statements, in statement order: a local declaration is a block statement
/// ([§14.3]), so it is found by walking the statement forms that contain
/// statement lists.
#[stacksafe]
fn collect_local_decls(bodies: &BodyTree, stmt: StmtId, out: &mut Vec<ItemId>) {
    match bodies.stmt(stmt) {
        StmtData::LocalClass { item } => out.push(*item),
        StmtData::Block(inner) | StmtData::DeclGroup(inner) => {
            for stmt in inner {
                collect_local_decls(bodies, *stmt, out);
            }
        }
        StmtData::Labeled { stmt, .. } => collect_local_decls(bodies, *stmt, out),
        StmtData::If { then, els, .. } => {
            collect_local_decls(bodies, *then, out);
            if let Some(els) = els {
                collect_local_decls(bodies, *els, out);
            }
        }
        StmtData::While { body, .. } | StmtData::DoWhile { body, .. } => {
            collect_local_decls(bodies, *body, out)
        }
        StmtData::For { init, body, .. } => {
            for stmt in init {
                collect_local_decls(bodies, *stmt, out);
            }
            collect_local_decls(bodies, *body, out);
        }
        StmtData::ForEach { body, .. } => collect_local_decls(bodies, *body, out),
        StmtData::Switch { arms, .. } => {
            for arm in arms {
                for stmt in &arm.body {
                    collect_local_decls(bodies, *stmt, out);
                }
            }
        }
        StmtData::Synchronized { body, .. } => collect_local_decls(bodies, *body, out),
        StmtData::Try {
            body,
            catches,
            finally,
            ..
        } => {
            collect_local_decls(bodies, *body, out);
            for catch in catches {
                collect_local_decls(bodies, catch.body, out);
            }
            if let Some(finally) = finally {
                collect_local_decls(bodies, *finally, out);
            }
        }
        StmtData::Empty
        | StmtData::Decl { .. }
        | StmtData::Expr(_)
        | StmtData::Return(_)
        | StmtData::Throw(_)
        | StmtData::Break(_)
        | StmtData::Continue(_)
        | StmtData::Yield(_)
        | StmtData::Assert { .. }
        | StmtData::Missing => {}
    }
}
