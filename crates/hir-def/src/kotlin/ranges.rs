//! Kotlin declaration ranges, resolved on demand.
//!
//! The item tree stores no source offsets: every declaration anchors itself to
//! its syntax node with a [`FileAstId`], and the range of that node is
//! resolved against the current syntax tree here ([`item_range`]). Because the
//! anchors are a function of the declaration skeleton only, a body-only edit
//! leaves every range resolving to the same *declaration* node.
//!
//! This module currently carries the helpers the lowering's snapshot renderer
//! needs; the navigation-oriented helpers (name, type-reference, annotation
//! and parameter ranges) land with the Kotlin navigation layer.

use rowan::TextRange;
use syntax::SourceFile;

use hir_expand::ast_id_map::{AstIdMap, FileAstId};

use super::item_tree::{ItemId, KotlinItemTree};

/// The source range of the file's root node, if `source` is a Kotlin file —
/// the root every anchor is resolved against. `None` for another language, so
/// a caller cannot accidentally resolve a Kotlin id against a Java tree.
pub fn kotlin_root<'a>(
    source: &'a SourceFile,
) -> Option<&'a rowan::SyntaxNode<syntax::kotlin::Lang>> {
    match source {
        SourceFile::Kotlin(file) => Some(&file.syntax_node),
        SourceFile::Java(_) => None,
    }
}

/// The source range of the syntax node `id` anchored.
pub fn ast_node_range<N>(
    map: &AstIdMap,
    source: &SourceFile,
    id: FileAstId<N>,
) -> Option<TextRange> {
    let root = kotlin_root(source)?;
    let ptr = map.try_get(id)?;
    map.try_to_node(ptr, root).map(|node| node.text_range())
}

/// The source range of the declaration item `id` refers to — the whole
/// declaration, from its first modifier or annotation to its last token.
pub fn item_range(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &KotlinItemTree,
    id: ItemId,
) -> Option<TextRange> {
    let ptr = tree.data(id).ast_id(map)?;
    let root = kotlin_root(source)?;
    map.try_to_node(ptr, root).map(|node| node.text_range())
}
