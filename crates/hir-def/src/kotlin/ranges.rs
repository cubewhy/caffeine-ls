//! Kotlin declaration ranges, resolved on demand.
//!
//! The item tree stores no source offsets: every declaration anchors itself to
//! its syntax node with a [`FileAstId`], and the ranges the IDE and the
//! diagnostics need are resolved against the *current* syntax tree here. Each
//! helper mirrors the lowering function its doc-comment names.
//!
//! Because the anchors are a function of the declaration skeleton only, a
//! body-only edit leaves every range resolving to the same *declaration*
//! node.
//!
//! The navigation-oriented helpers (type-reference, annotation and parameter
//! ranges) land with the Kotlin navigation layer; this module currently
//! carries what the symbol surface and the snapshot renderer need.

use rowan::{NodeOrToken, SyntaxNode, TextRange};
use syntax::SourceFile;
use syntax::kotlin::{Lang, SyntaxKind as K};

use hir_expand::ast_id_map::{AstIdMap, FileAstId};

use super::item_tree::{ItemId, KotlinItemTree, PackageHeaderNode};

/// The Kotlin root node of `source`; `None` for a non-Kotlin file, so a
/// caller cannot accidentally resolve a Kotlin id against a Java tree.
pub fn kotlin_root(source: &SourceFile) -> Option<&SyntaxNode<Lang>> {
    match source {
        SourceFile::Kotlin(file) => Some(&file.syntax_node),
        SourceFile::Java(_) => None,
    }
}

/// Resolves the syntax node of `id` from the current tree; `None` when the id
/// is a placeholder (a synthesized reference) or the node is gone.
fn node_of<N>(map: &AstIdMap, source: &SourceFile, id: FileAstId<N>) -> Option<SyntaxNode<Lang>> {
    let root = kotlin_root(source)?;
    let ptr = map.try_get(id)?;
    map.try_to_node(ptr, root)
}

/// The source range of the syntax node `id` anchored.
pub fn ast_node_range<N>(
    map: &AstIdMap,
    source: &SourceFile,
    id: FileAstId<N>,
) -> Option<TextRange> {
    node_of(map, source, id).map(|node| node.text_range())
}

/// The source range of the declared package name (the qualified name after
/// the `package` keyword).
pub fn package_name_range(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &KotlinItemTree,
) -> Option<TextRange> {
    node_of::<PackageHeaderNode>(map, source, tree.package_header?)?
        .children()
        .find(|child| child.kind() == K::QUALIFIED_NAME)
        .map(|name| name.text_range())
}

/// The syntax node of a declaration item.
fn item_node(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &KotlinItemTree,
    id: ItemId,
) -> Option<SyntaxNode<Lang>> {
    let ptr = tree.data(id).ast_id(map)?;
    let root = kotlin_root(source)?;
    map.try_to_node(ptr, root)
}

/// The source range of the declaration item `id` refers to — the whole
/// declaration, from its first modifier or annotation to its last token.
pub fn item_range(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &KotlinItemTree,
    id: ItemId,
) -> Option<TextRange> {
    item_node(map, source, tree, id).map(|node| node.text_range())
}

/// The source range of the item's declared name — the identifier the IDE
/// selects and renames.
///
/// A constructor, accessor and `init` block declare no name of their own, so
/// the range of the keyword that introduces them stands in; an unnamed
/// companion object selects its `object` keyword.
pub fn item_name_range(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &KotlinItemTree,
    id: ItemId,
) -> Option<TextRange> {
    let node = item_node(map, source, tree, id)?;
    let data = tree.data(id);
    match node.kind() {
        K::CLASS_DECL | K::OBJECT_DECL => identifier_after_keyword(&node),
        K::COMPANION_OBJECT => {
            identifier_after_keyword(&node).or_else(|| token_range(&node, K::OBJECT_KW))
        }
        K::FUNCTION_DECL | K::TYPE_ALIAS | K::ENUM_ENTRY => first_identifier(&node),
        K::PROPERTY_DECL | K::CLASS_PARAMETER => {
            variable_name_range(&node, data.name().map(|name| name.as_str()))
        }
        K::PRIMARY_CONSTRUCTOR => token_range(&node, K::IDENTIFIER).or_else(|| {
            node.children()
                .find(|child| child.kind() == K::CLASS_PARAMETERS)
                .map(|parameters| parameters.text_range())
        }),
        K::SECONDARY_CONSTRUCTOR => token_range(&node, K::IDENTIFIER),
        K::GETTER | K::SETTER | K::ANONYMOUS_INITIALIZER => first_identifier(&node),
        _ => None,
    }
}

/// The first direct-child `IDENTIFIER` token of `node` that follows the
/// classifier keyword — so a companion object's leading `companion` is never
/// mistaken for its name.
fn identifier_after_keyword(node: &SyntaxNode<Lang>) -> Option<TextRange> {
    let mut after_keyword = false;
    for element in node.children_with_tokens() {
        let NodeOrToken::Token(token) = element else {
            continue;
        };
        match token.kind() {
            K::CLASS_KW | K::INTERFACE_KW | K::OBJECT_KW => after_keyword = true,
            K::IDENTIFIER if after_keyword => return Some(token.text_range()),
            _ => {}
        }
    }
    None
}

/// The first direct-child `IDENTIFIER` token of `node`.
fn first_identifier(node: &SyntaxNode<Lang>) -> Option<TextRange> {
    token_range(node, K::IDENTIFIER)
}

/// The source range of the first direct-child token of `kind`.
fn token_range(node: &SyntaxNode<Lang>, kind: K) -> Option<TextRange> {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| token.kind() == kind)
        .map(|token| token.text_range())
}

/// The name range of a `propertyDeclaration`/`classParameter`: the identifier
/// of the variable declaration that declares `name`.
///
/// A destructuring declaration (`val (a, b) = pair`) declares one property per
/// bound name and both share one node, so the name selects among them.
fn variable_name_range(node: &SyntaxNode<Lang>, name: Option<&str>) -> Option<TextRange> {
    let declarations: Vec<SyntaxNode<Lang>> = match node
        .children()
        .find(|child| child.kind() == K::MULTI_VARIABLE_DECLARATION)
    {
        Some(multi) => multi
            .children()
            .filter(|child| child.kind() == K::VARIABLE_DECLARATION)
            .collect(),
        None => node
            .children()
            .filter(|child| child.kind() == K::VARIABLE_DECLARATION)
            .collect(),
    };
    let range_of = |declaration: &SyntaxNode<Lang>| {
        declaration
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .find(|token| matches!(token.kind(), K::IDENTIFIER | K::UNDERSCORE))
            .map(|token| token.text_range())
    };
    if let Some(name) = name {
        for declaration in &declarations {
            if let Some(range) = range_of(declaration)
                && declaration
                    .children_with_tokens()
                    .filter_map(NodeOrToken::into_token)
                    .any(|token| token.kind() == K::IDENTIFIER && token.text() == name)
            {
                return Some(range);
            }
        }
    }
    // A class parameter carries its name directly (`val x: Int`).
    if let Some(name) = name
        && let Some(token) = node
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .find(|token| token.kind() == K::IDENTIFIER && token.text() == name)
    {
        return Some(token.text_range());
    }
    declarations.first().and_then(&range_of)
}
