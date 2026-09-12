//! On-demand source-range resolution for the item tree.
//!
//! The item tree stores no source offsets: every declaration carries the
//! [`FileAstId`] of its syntax node, and this module resolves the source
//! ranges the IDE and the diagnostics need from the *current* syntax tree —
//! the same computations the lowering walker ([`crate::java::lower::walk`])
//! used to perform at lowering time. Each helper mirrors the lowering
//! function its doc-comment names, including every fallback, so the resolved
//! ranges are byte-identical to the ones the item tree used to carry.
//!
//! The helpers are pure functions of `(&AstIdMap, &syntax::SourceFile, …)`;
//! the database-facing wrappers live with the consumers, which fetch the map
//! and the parse first.

use rowan::{SyntaxNode, TextRange};
use syntax::SourceFile;
use syntax::java::{Lang, SyntaxKind as J};

use hir_expand::{
    ast_id_map::{AstIdMap, FileAstId},
    name::Name,
};

use crate::java::item_tree::{
    EnumConstantData, FieldData, ImportItem, ItemAnnotationRef, ItemData, ItemId, ItemTree,
    ItemTypeRef, MethodData, ModuleExports, ModuleRequires, PackageDeclNode, RecordComponent,
};
use crate::java::lower::walk::{
    annotation_name_ref, annotation_value_from, first_token, is, is_element_value, trimmed_text,
    type_from,
};

/// The Java root node of `source`; `None` for a non-Java file.
fn java_root(source: &SourceFile) -> Option<&SyntaxNode<Lang>> {
    match source {
        SourceFile::Java(file) => Some(&file.syntax_node),
        SourceFile::Kotlin(_) => None,
    }
}

/// Resolves the syntax node of `id` from the current tree; `None` when the id
/// is a placeholder (a synthesized reference) or the node is gone.
fn node_of<N>(map: &AstIdMap, source: &SourceFile, id: FileAstId<N>) -> Option<SyntaxNode<Lang>> {
    let root = java_root(source)?;
    let ptr = map.try_get(id)?;
    map.try_to_node(ptr, root)
}

/// The syntax node of a declaration item.
fn item_node(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &ItemTree,
    id: ItemId,
) -> Option<SyntaxNode<Lang>> {
    match tree.data(id) {
        ItemData::Class(data) | ItemData::Interface(data) => node_of(map, source, data.ast),
        ItemData::Enum(data) => node_of(map, source, data.ast),
        ItemData::Record(data) => node_of(map, source, data.ast),
        ItemData::Annotation(data) => node_of(map, source, data.ast),
        ItemData::Module(data) => node_of(map, source, data.ast),
        ItemData::Method(data) => node_of(map, source, data.ast),
        ItemData::Field(data) => node_of(map, source, data.ast),
        ItemData::EnumConstant(data) => node_of(map, source, data.ast),
        ItemData::StaticInit(data) => node_of(map, source, data.ast),
        ItemData::InstanceInit(data) => node_of(map, source, data.ast),
    }
}

/// The first direct-child `IDENTIFIER` token of `node` that is not a
/// restricted *type* name (mirror of `walk::decl_type_identifier`; exclusions
/// per [JLS §3.9]).
fn type_identifier_range(node: &SyntaxNode<Lang>) -> Option<TextRange> {
    node.children_with_tokens().find_map(|element| {
        let token = element.as_token()?;
        (token.kind() == J::IDENTIFIER
            && !matches!(token.text(), "record" | "sealed" | "non-sealed" | "permits"))
        .then(|| token.text_range())
    })
}

/// The first direct-child `IDENTIFIER` token of `node` (mirror of
/// `walk::decl_identifier`).
fn identifier_range(node: &SyntaxNode<Lang>) -> Option<TextRange> {
    first_token(node, J::IDENTIFIER).map(|token| token.text_range())
}

/// The source range of the whole declaration item (mirror of the old
/// `ItemData::range`: the declaration node's range, the full record node
/// included).
pub fn item_range(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &ItemTree,
    id: ItemId,
) -> Option<TextRange> {
    item_node(map, source, tree, id).map(|node| node.text_range())
}

/// The source range of the item's declared name — the identifier the IDE
/// points the LSP `selectionRange` at (mirror of `decl_identifier` /
/// `decl_type_identifier`). Initializers are nameless and fall back to their
/// whole range.
pub fn item_name_range(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &ItemTree,
    id: ItemId,
) -> Option<TextRange> {
    let node = item_node(map, source, tree, id)?;
    // A class-like type declaration excludes the restricted *type* names.
    let name_range = match tree.data(id) {
        ItemData::Class(_)
        | ItemData::Interface(_)
        | ItemData::Enum(_)
        | ItemData::Record(_)
        | ItemData::Annotation(_) => type_identifier_range(&node),
        ItemData::StaticInit(_) | ItemData::InstanceInit(_) => None,
        _ => identifier_range(&node),
    };
    Some(name_range.unwrap_or_else(|| node.text_range()))
}

/// The source range of a record's component list — its parameter declaration
/// `(int x, int y)`, the outline's selection for the record (mirror of
/// `lower_record`'s `components_range`; the component list is the fallback of
/// the name range).
pub fn record_components_range(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &ItemTree,
    id: ItemId,
) -> Option<TextRange> {
    let node = item_node(map, source, tree, id)?;
    let name_range = type_identifier_range(&node).unwrap_or_else(|| node.text_range());
    Some(
        node.children()
            .find(|child| is(child, J::FORMAL_PARAMETERS))
            .map(|params| params.text_range())
            .unwrap_or(name_range),
    )
}

/// The source range of a record declaration *header*: from the `record`
/// keyword through the closing `)` of the component list (and any
/// `implements` clause), excluding the body `{ ... }` — the declaration's
/// "definition" (verbatim mirror of `lower_record`'s `header_end`
/// computation).
pub fn record_header_range(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &ItemTree,
    id: ItemId,
) -> Option<TextRange> {
    let node = item_node(map, source, tree, id)?;
    let name_end = type_identifier_range(&node)
        .unwrap_or_else(|| node.text_range())
        .end();
    let header_end = node
        .children()
        .find(|child| is(child, J::FORMAL_PARAMETERS))
        .map(|params| params.text_range().end())
        .or_else(|| {
            node.children()
                .find(|child| is(child, J::IMPLEMENTS_CLAUSE))
                .map(|clause| clause.text_range().end())
        })
        .unwrap_or(name_end);
    Some(TextRange::new(node.text_range().start(), header_end))
}

/// The source range of an import declaration (`ImportItem`'s old `range`:
/// the whole `IMPORT_DECL` node, mirroring `lower_import`).
pub fn import_name_range(
    map: &AstIdMap,
    source: &SourceFile,
    import: &ImportItem,
) -> Option<TextRange> {
    node_of(map, source, import.path).map(|node| node.text_range())
}

/// The identifier segments of an import declaration with their source ranges, in
/// written order (`import static a.b.C.m;` → `a`, `b`, `C`, `m`); the `*` of an
/// on-demand import is not a segment (mirror of `lower_import`'s name walk,
/// `crate::java::lower::walk::lower_import`).
pub fn import_segments(
    map: &AstIdMap,
    source: &SourceFile,
    import: &ImportItem,
) -> Vec<(String, TextRange)> {
    let Some(node) = node_of(map, source, import.path) else {
        return Vec::new();
    };
    let Some(path) = node.children().find(|child| is(child, J::IMPORT_PATH)) else {
        return Vec::new();
    };
    let segments: Vec<(String, TextRange)> = path
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() == J::IDENTIFIER)
        .map(|token| (token.text().to_owned(), token.text_range()))
        .collect();
    // `lower_import` strips only a trailing `.*` from the path, so the joined
    // segments are exactly the lowered name for both import forms.
    debug_assert_eq!(
        segments
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join("."),
        import.name.as_str()
    );
    segments
}

/// The source range of a package declaration's name — the `QUALIFIED_NAME`
/// child of the `PACKAGE_DECL` (mirror of `lower_package`).
pub fn package_name_range(
    map: &AstIdMap,
    source: &SourceFile,
    decl: FileAstId<PackageDeclNode>,
) -> Option<TextRange> {
    let node = node_of(map, source, decl)?;
    node.children()
        .find(|child| is(child, J::QUALIFIED_NAME))
        .map(|child| child.text_range())
}

/// The source range of a whole record component declaration (`T name`, or
/// `T... name` for a varargs component) — the record accessor's outline
/// selection (mirror of `component_from`'s `range`).
pub fn component_range(
    map: &AstIdMap,
    source: &SourceFile,
    component: &RecordComponent,
) -> Option<TextRange> {
    node_of(map, source, component.ast).map(|node| node.text_range())
}

/// The source range of a field's initializer: from the end of the `=` token
/// to the end of its declarator (mirror of `lower_field_decl`).
pub fn field_initializer_range(
    map: &AstIdMap,
    source: &SourceFile,
    field: &FieldData,
) -> Option<TextRange> {
    let node = node_of(map, source, field.ast)?;
    let equal = first_token(&node, J::EQUAL)?;
    Some(TextRange::new(
        equal.text_range().end(),
        node.text_range().end(),
    ))
}

/// The source range of an annotation element's default value: from the end of
/// the `default` keyword to the start of the closing `;` (or the end of the
/// declaration) — mirror of `lower_annotation_element`'s `token_range_after`.
pub fn method_default_value_range(
    map: &AstIdMap,
    source: &SourceFile,
    method: &MethodData,
) -> Option<TextRange> {
    let node = node_of(map, source, method.ast)?;
    let start = first_token(&node, J::DEFAULT_KW)?.text_range().end();
    let end = node
        .children_with_tokens()
        .filter_map(|element| element.as_token().cloned())
        .find(|token| token.kind() == J::SEMICOLON)
        .map_or_else(
            || node.text_range().end(),
            |token| token.text_range().start(),
        );
    Some(TextRange::new(start, end))
}

/// The source range of an enum constant's argument list (`RED(255, 0, 0)`'s
/// `(255, 0, 0)`) — the `ARGUMENT_LIST` child of the constant (mirror of
/// `enum_body_members`).
pub fn enum_constant_arguments_range(
    map: &AstIdMap,
    source: &SourceFile,
    constant: &EnumConstantData,
) -> Option<TextRange> {
    let node = node_of(map, source, constant.ast)?;
    node.children()
        .find(|child| is(child, J::ARGUMENT_LIST))
        .map(|list| list.text_range())
}

/// The source range of an enum constant's constant class body (`BLUE { ... }`
/// — `CLASS_BODY` child of the constant, mirror of `enum_body_members`).
pub fn enum_constant_class_body_range(
    map: &AstIdMap,
    source: &SourceFile,
    constant: &EnumConstantData,
) -> Option<TextRange> {
    let node = node_of(map, source, constant.ast)?;
    node.children()
        .find(|child| is(child, J::CLASS_BODY))
        .map(|body| body.text_range())
}

/// The reference names of a declaration-side type reference with the source
/// range of each, in `ItemTypeRef::refs` order. For a `TYPE` node the names
/// and ranges are re-derived with the exact walk of lowering's `type_from`
/// (same syntax node, same revision — identical output); a module directive's
/// `QUALIFIED_NAME` node yields its own range; an unresolvable (placeholder)
/// node yields all `None`.
pub fn type_ref_occurrences(
    map: &AstIdMap,
    source: &SourceFile,
    tyref: &ItemTypeRef,
) -> Vec<(Name, Option<TextRange>)> {
    let node = node_of(map, source, tyref.node);
    let mut occurrences: Vec<(Name, Option<TextRange>)> = match &node {
        Some(node) if node.kind() == J::QUALIFIED_NAME => {
            vec![(Name::new(&trimmed_text(node)), Some(node.text_range()))]
        }
        Some(node) => type_from(node)
            .refs
            .into_iter()
            .map(|reference| (reference.name, reference.range))
            .collect(),
        None => tyref.refs.iter().map(|name| (name.clone(), None)).collect(),
    };
    // The names are authoritative (they were lowered from this node); keep
    // them, and drop any occurrence whose range walk produced a surplus.
    occurrences.truncate(tyref.refs.len());
    tyref
        .refs
        .iter()
        .cloned()
        .zip(occurrences)
        .map(|(name, (_, range))| (name, range))
        .collect()
}

/// The source range of an annotation's (possibly qualified) name — the first
/// `QUALIFIED_NAME` descendant of its syntax node (mirror of
/// `annotation_name_ref`).
pub fn annotation_name_range(
    map: &AstIdMap,
    source: &SourceFile,
    annotation: &ItemAnnotationRef,
) -> Option<TextRange> {
    let node = node_of(map, source, annotation.node)?;
    annotation_name_ref(&node).and_then(|name| name.range)
}

/// The source range of an annotation element-value pair's *value* expression
/// (the `arg.range` of the spanned lowering): the value syntax node of the
/// `arg_idx`-th argument of `annotation`'s argument list, found by replaying
/// `annotation_args_from`'s walk (same node, same order, same filters).
pub fn annotation_arg_value_range(
    map: &AstIdMap,
    source: &SourceFile,
    annotation: &ItemAnnotationRef,
    arg_idx: usize,
) -> Option<TextRange> {
    let node = node_of(map, source, annotation.node)?;
    let list = node
        .children()
        .find(|child| is(child, J::ANNOTATION_ARGUMENT_LIST))?;
    let mut seen = 0usize;
    for child in list.children() {
        let value = if is(&child, J::ELEMENT_VALUE_PAIR) {
            child.children().find(is_element_value)
        } else if is_element_value(&child) {
            Some(child)
        } else {
            None
        };
        let Some(value) = value else { continue };
        // `annotation_args_from` skips a value that fails to parse.
        if annotation_value_from(&value).is_none() {
            continue;
        }
        if seen == arg_idx {
            return Some(value.text_range());
        }
        seen += 1;
    }
    None
}

/// The source range of a `requires` directive's required module name — the
/// `QUALIFIED_NAME` child, falling back to the whole directive (mirror of
/// `requires_from`).
pub fn requires_name_range(
    map: &AstIdMap,
    source: &SourceFile,
    requires: &ModuleRequires,
) -> Option<TextRange> {
    let node = node_of(map, source, requires.ast)?;
    Some(
        node.children()
            .find(|child| is(child, J::QUALIFIED_NAME))
            .map(|child| child.text_range())
            .unwrap_or_else(|| node.text_range()),
    )
}

/// The source range of an `exports`/`opens` directive's package name — the
/// first `QUALIFIED_NAME` child, falling back to the whole directive (mirror
/// of `package_exports_from`).
pub fn module_exports_package_range(
    map: &AstIdMap,
    source: &SourceFile,
    export: &ModuleExports,
) -> Option<TextRange> {
    let node = node_of(map, source, export.ast)?;
    Some(
        node.children()
            .find(|child| is(child, J::QUALIFIED_NAME))
            .map(|child| child.text_range())
            .unwrap_or_else(|| node.text_range()),
    )
}
