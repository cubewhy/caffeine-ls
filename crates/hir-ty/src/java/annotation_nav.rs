//! Goto-definition inside an annotation's element-value pairs
//! ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7),
//! [§9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1)).
//!
//! An element-value pair `name = value` writes two kinds of reference. The
//! *name* denotes an element of the annotation interface — the method the
//! interface declares under it ([§9.6.1]). The *value* is a
//! `ConditionalExpression` ([§9.7.1]) written in the scope of the declaration
//! that carries the annotation ([§6.5.5.1]): a class literal names its type
//! ([§15.8.2]), a nested annotation names its annotation interface
//! ([§9.7.1]), an enum constant and a constant variable read a field
//! ([§6.5.6]), and an array initializer ([§10.6]) holds more of the same.
//!
//! The references are read from the *syntax tree*, not from the lowered
//! values: the enumeration that backs the element-value checks
//! ([`crate::java::annotation_value`]) keeps the literal forms (an enum
//! constant, a class literal, a nested annotation, an array) out of the
//! expression arena, and the annotations of a written type have no arena at
//! all. One syntax walk therefore covers every occurrence uniformly — a
//! declaration's annotation, a type-use annotation, an annotation inside a
//! body — while the name resolution stays with the layers that own it
//! ([`crate::java::resolve`], [`crate::java::method`]).

use hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use hir_def::jvm::decl::ItemAnnotationRef;
use hir_expand::body::BodyTree;
use hir_expand::name::Name;
use rowan::{SyntaxNode, SyntaxToken, TextRange, TextSize, TokenAtOffset};
use syntax::SourceFile;
use syntax::java::{Lang, SyntaxKind as J, translate_unicode_escapes};
use triomphe::Arc;
use vfs::FileId;

use crate::java::annotation_check::{declaration_annotations, declaration_type_refs};
use crate::java::annotation_value::{NameTarget, ValueCtx, name_target};
use crate::jvm::db::TyDatabase;

use crate::java::method::{InvocationContext, InvocationMode, access_context, pick_method};
use crate::java::range_ctx::range_ctx;
use crate::java::resolve::{
    NameResolution, Resolver, candidate_fqns, resolve_type_name_at, scope_for_file,
};
use crate::java::ty::Ty;
use crate::jvm::member::{FieldData, MethodData};
use hir_def::java::ranges;

/// The declaration an annotation reference denotes, in the shape the IDE's
/// navigation layer turns into a location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotationTarget {
    /// An element-value pair's name: the annotation interface method the name
    /// declares ([JLS §9.6.1]).
    Element(MethodData),
    /// A value's name: the field or enum constant it reads ([§6.5.6]).
    Field(FieldData),
    /// A value's type name: a class literal's type ([§15.8.2]), an enum
    /// qualifier ([§6.5.6.2]), or a nested annotation's annotation interface
    /// ([§9.7.1]).
    Type(Name),
}

/// The declaration the annotation reference at `offset` denotes — the element
/// name of an `ELEMENT_VALUE_PAIR`, or a name inside its value ([JLS §9.7.1]).
/// `None` when the offset is not written inside an annotation, or the written
/// name resolves to nothing.
pub fn annotation_target(
    db: &dyn TyDatabase,
    file: FileId,
    offset: TextSize,
) -> Option<AnnotationTarget> {
    let tree = hir::java_item_tree(db, file);
    let (map, source) = range_ctx(db, file, tree.language)?;
    let root = java_root(&source)?;
    // The innermost annotation the offset is written in: for a nested
    // annotation value that is the nested one, whose pairs are read against
    // the same annotation's item as its enclosing annotation.
    let token = token_at(root, offset)?;
    let annotation = token
        .parent_ancestors()
        .find(|node| matches!(node.kind(), J::ANNOTATION | J::MARKER_ANNOTATION))?;
    let item = annotation_item(&tree, map, &source, &annotation, offset);
    let scope = scope_for_file(db, file);
    let resolver = match item {
        Some(item) => Resolver::for_item(db, file, &tree, item),
        // An annotation outside every item — a package annotation
        // ([§7.4.1]) — resolves in the compilation unit's own scope.
        None => Resolver::for_file(&tree),
    };
    let cx = NavCtx {
        db,
        file,
        item,
        scope: &scope,
        resolver: &resolver,
        bodies: hir::file_body_tree(db, file),
    };
    resolve_in_annotation(&cx, &annotation, offset)
}

/// The context every resolution below runs in: the file, the item whose
/// declaration carries the annotation (its scope is the one the names resolve
/// in, [JLS §6.5.5.1]), and the file's body tree.
struct NavCtx<'a> {
    db: &'a dyn TyDatabase,
    file: FileId,
    item: Option<ItemId>,
    scope: &'a hir::ResolutionScope,
    resolver: &'a Resolver,
    bodies: Arc<BodyTree>,
}

/// The Java syntax root of a parsed source; `None` for a non-Java file.
fn java_root(source: &SourceFile) -> Option<&SyntaxNode<Lang>> {
    match source {
        SourceFile::Java(file) => Some(&file.syntax_node),
        SourceFile::Kotlin(_) => None,
    }
}

/// The [`Name`] a piece of *source text* denotes ([JLS §3.3]): the lexer
/// reads a Unicode escape to tokenize but keeps every token's text as
/// written, so a name built from that text is the *translation* of it —
/// `@\u0041nn` is the annotation `Ann`.
fn source_name(text: &str) -> Name {
    Name::new(&translate_unicode_escapes(text))
}

/// The token `offset` falls in, right-biased so an offset at a token's start
/// reads that token (the position an editor puts the caret on).
fn token_at(node: &SyntaxNode<Lang>, offset: TextSize) -> Option<SyntaxToken<Lang>> {
    // `token_at_offset` panics on an offset outside the node, and a request
    // may arrive at the very end of the file.
    if !node.text_range().contains_inclusive(offset) {
        return None;
    }
    match node.token_at_offset(offset) {
        TokenAtOffset::None => None,
        TokenAtOffset::Single(token) => Some(token),
        TokenAtOffset::Between(_, right) => Some(right),
    }
}

/// The item whose declaration carries `annotation`: the item whose lowered
/// annotations contain the node, or — for an annotation inside a body, which
/// the item tree does not carry — the innermost item whose declaration range
/// contains the offset.
fn annotation_item(
    tree: &ItemTree,
    map: &hir_expand::ast_id_map::AstIdMap,
    source: &SourceFile,
    annotation: &SyntaxNode<Lang>,
    offset: TextSize,
) -> Option<ItemId> {
    let range = annotation.text_range();
    let mut items = Vec::new();
    collect_items(tree, map, source, &mut items);
    for &(_, item) in &items {
        if item_annotations(tree.data(item)).iter().any(|carried| {
            ranges::annotation_range(map, source, carried)
                .is_some_and(|carried| carried.contains_range(range))
        }) {
            return Some(item);
        }
    }
    items
        .into_iter()
        .filter(|(declared, _)| declared.contains(offset))
        .min_by_key(|(declared, _)| declared.len())
        .map(|(_, item)| item)
}

/// Every item of the file with its declaration range, in tree order.
fn collect_items(
    tree: &ItemTree,
    map: &hir_expand::ast_id_map::AstIdMap,
    source: &SourceFile,
    out: &mut Vec<(TextRange, ItemId)>,
) {
    fn walk(
        tree: &ItemTree,
        map: &hir_expand::ast_id_map::AstIdMap,
        source: &SourceFile,
        item: ItemId,
        out: &mut Vec<(TextRange, ItemId)>,
    ) {
        if let Some(range) = ranges::item_range(map, source, tree, item) {
            out.push((range, item));
        }
        for &child in tree.data(item).body() {
            walk(tree, map, source, child, out);
        }
    }
    out.clear();
    for &top in &tree.top {
        walk(tree, map, source, top, out);
    }
}

/// Every annotation the item tree anchored on `data` — its modifier
/// annotations ([`declaration_annotations`]), the annotations of its record
/// components, formal parameters and type parameters, and the type-use
/// annotations of its declaration type references ([JLS §9.7.4]).
fn item_annotations(data: &ItemData) -> Vec<&ItemAnnotationRef> {
    let mut out = declaration_annotations(data);
    match data {
        ItemData::Class(d) | ItemData::Interface(d) => {
            for param in &d.type_params {
                out.extend(param.annotations.iter());
            }
        }
        ItemData::Record(d) => {
            for param in &d.type_params {
                out.extend(param.annotations.iter());
            }
            for component in &d.components {
                out.extend(component.annotations.iter());
            }
        }
        ItemData::Method(d) => {
            for param in &d.sig.type_params {
                out.extend(param.annotations.iter());
            }
            for param in &d.sig.params {
                out.extend(param.annotations.iter());
            }
        }
        _ => {}
    }
    for tyref in declaration_type_refs(data) {
        out.extend(tyref.type_use_annotations.iter());
    }
    out
}

/// The reference at `offset` inside the annotation `annotation`: its own name
/// (a nested annotation's type), or a pair's name or value.
fn resolve_in_annotation(
    cx: &NavCtx<'_>,
    annotation: &SyntaxNode<Lang>,
    offset: TextSize,
) -> Option<AnnotationTarget> {
    // §9.7.1: `@ TypeName (...)`. The name is a type name; a *nested*
    // annotation's name is resolved from here (an outer one's is answered by
    // the declaration-side step, which enumerates every annotation name).
    if let Some(name) = annotation
        .children()
        .find(|child| child.kind() == J::QUALIFIED_NAME)
        && name.text_range().contains(offset)
    {
        return type_name_target(cx, &name, offset);
    }
    let list = annotation
        .children()
        .find(|child| child.kind() == J::ANNOTATION_ARGUMENT_LIST)?;
    resolve_in_arg_list(cx, annotation, &list, offset)
}

/// The reference at `offset` inside the argument list of `annotation`.
fn resolve_in_arg_list(
    cx: &NavCtx<'_>,
    annotation: &SyntaxNode<Lang>,
    list: &SyntaxNode<Lang>,
    offset: TextSize,
) -> Option<AnnotationTarget> {
    for child in list.children() {
        if !child.text_range().contains(offset) {
            continue;
        }
        if child.kind() == J::ELEMENT_VALUE_PAIR {
            // §9.6.1: the pair's name is an element of the annotation
            // interface — the method of that name. The value identifier is
            // wrapped in a `LITERAL` node, so a direct identifier token of the
            // pair is always its name.
            if let Some(name_token) = child
                .children_with_tokens()
                .filter_map(|element| element.into_token())
                .find(|token| token.kind() == J::IDENTIFIER)
                && name_token.text_range().contains(offset)
            {
                let annotation_name = annotation_name(annotation)?;
                return element_target(cx, &annotation_name, &source_name(name_token.text()));
            }
            let value = child
                .children()
                .find(|child| child.text_range().contains(offset))?;
            return resolve_value(cx, &value, offset);
        }
        // §9.7.1: the single-element form `(v)` — the value stands alone.
        return resolve_value(cx, &child, offset);
    }
    None
}

/// The value `offset` falls in: a nested annotation or an array initializer
/// is descended into ([§9.7.1], [§10.6]), everything else is a name.
fn resolve_value(
    cx: &NavCtx<'_>,
    node: &SyntaxNode<Lang>,
    offset: TextSize,
) -> Option<AnnotationTarget> {
    match node.kind() {
        J::ANNOTATION | J::MARKER_ANNOTATION => resolve_in_annotation(cx, node, offset),
        J::ARRAY_INITIALIZER => {
            let child = node
                .children()
                .find(|child| child.text_range().contains(offset))?;
            resolve_value(cx, &child, offset)
        }
        _ => resolve_expression_value(cx, node, offset),
    }
}

/// The name the identifier token at `offset` writes, read as a type name
/// ([§6.5.2]): a class literal's type ([§15.8.2]), a nested annotation's
/// interface ([§9.7.1]), or the qualifier of a qualified value name.
fn resolve_expression_value(
    cx: &NavCtx<'_>,
    node: &SyntaxNode<Lang>,
    offset: TextSize,
) -> Option<AnnotationTarget> {
    let token = token_at(node, offset)?;
    if token.kind() != J::IDENTIFIER {
        return None;
    }
    let chain = name_chain(&token);
    let tokens = name_tokens(&chain);
    let index = tokens
        .iter()
        .position(|candidate| candidate.text_range() == token.text_range())?;
    let prefix = join(&tokens[..=index]);
    // §15.8.2: the name before `.class` is a type name.
    if chain.kind() == J::CLASS_LITERAL {
        return type_target(cx, &prefix);
    }
    // A non-final segment of a qualified name is the type — or the member type
    // enclosing the next segment — the name is read through ([§6.5.2]).
    if index + 1 < tokens.len() {
        return type_target(cx, &prefix).or_else(|| field_target(cx, None, &source_name(&prefix)));
    }
    // The last segment: the whole name may be a type (`Outer.Inner`), a static
    // field of the qualifier's type ([§6.5.6.2]), or a simple name read as a
    // field of an enclosing declaration ([§6.5.6.1]).
    if let Some((qualifier, member)) = prefix.rsplit_once('.') {
        if let Some(target) = type_target(cx, &prefix) {
            return Some(target);
        }
        return field_target(cx, Some(&source_name(qualifier)), &source_name(member));
    }
    field_target(cx, None, &source_name(&prefix)).or_else(|| type_target(cx, &prefix))
}

/// The type name written by the chain node `node`, up to and including the
/// identifier `offset` falls on — the longest prefix ending at the token,
/// which is the declaration the caret names.
fn type_name_target(
    cx: &NavCtx<'_>,
    node: &SyntaxNode<Lang>,
    offset: TextSize,
) -> Option<AnnotationTarget> {
    let token = token_at(node, offset)?;
    if token.kind() != J::IDENTIFIER {
        return None;
    }
    let chain = name_chain(&token);
    let tokens = name_tokens(&chain);
    let index = tokens
        .iter()
        .position(|candidate| candidate.text_range() == token.text_range())?;
    type_target(cx, &join(&tokens[..=index]))
}

/// The names of a name chain in source order: a `LITERAL`/`QUALIFIED_NAME`
/// node's own identifiers, and a `FIELD_ACCESS`'s receiver chain followed by
/// its member.
fn name_tokens(chain: &SyntaxNode<Lang>) -> Vec<SyntaxToken<Lang>> {
    match chain.kind() {
        J::LITERAL | J::QUALIFIED_NAME => chain
            .children_with_tokens()
            .filter_map(|element| element.into_token())
            .filter(|token| token.kind() == J::IDENTIFIER)
            .collect(),
        J::FIELD_ACCESS => {
            let mut out = Vec::new();
            if let Some(receiver) = chain.children().next()
                && is_name_chain(receiver.kind())
            {
                out.extend(name_tokens(&receiver));
            }
            if let Some(member) = chain
                .children_with_tokens()
                .filter_map(|element| element.into_token())
                .find(|token| token.kind() == J::IDENTIFIER)
            {
                out.push(member);
            }
            out
        }
        J::CLASS_LITERAL => chain
            .children()
            .find(|child| is_name_chain(child.kind()))
            .map(|child| name_tokens(&child))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The outermost name-chain node the identifier `token` is written in — the
/// node whose identifiers spell the whole dotted name.
fn name_chain(token: &SyntaxToken<Lang>) -> SyntaxNode<Lang> {
    let mut node = token.parent().expect("a token has a parent");
    while let Some(parent) = node.parent() {
        if is_name_chain(parent.kind()) {
            node = parent;
        } else {
            break;
        }
    }
    node
}

/// Whether `kind` is a node that spells part of a dotted name — a bare
/// identifier (`LITERAL`), a qualification (`FIELD_ACCESS`), a written type
/// name (`CLASS_LITERAL`) or an annotation/import name (`QUALIFIED_NAME`).
fn is_name_chain(kind: J) -> bool {
    matches!(
        kind,
        J::LITERAL | J::FIELD_ACCESS | J::CLASS_LITERAL | J::QUALIFIED_NAME
    )
}

/// The dotted text of `tokens`.
fn join(tokens: &[SyntaxToken<Lang>]) -> String {
    tokens
        .iter()
        .map(|token| token.text())
        .collect::<Vec<_>>()
        .join(".")
}

/// The annotation's written name — the first `QUALIFIED_NAME` descendant of
/// its syntax node (mirror of `annotation_name_ref`).
fn annotation_name(annotation: &SyntaxNode<Lang>) -> Option<Name> {
    annotation
        .descendants()
        .find(|node| node.kind() == J::QUALIFIED_NAME)
        .map(|node| source_name(&node.text().to_string()))
}

/// The element the annotation `annotation` declares under `element`
/// ([JLS §9.6.1]): the method of the annotation interface, found among the
/// interface's own members.
fn element_target(cx: &NavCtx<'_>, annotation: &Name, element: &Name) -> Option<AnnotationTarget> {
    let fqn = candidate_fqns(cx.resolver, annotation)
        .into_iter()
        .find(|candidate| hir::fqn_resolve(cx.db, cx.scope, candidate.as_str()).is_some())?;
    if !is_annotation_interface(cx.db, cx.scope, &fqn) {
        return None;
    }
    let receiver = Ty::reference(cx.db, fqn.as_str(), Vec::new());
    let context = match cx.item {
        Some(item) => access_context(cx.db, cx.file, item),
        None => InvocationContext::external(cx.scope),
    }
    .with_mode(InvocationMode::MethodName);
    let method = pick_method(
        cx.db,
        cx.scope,
        &receiver,
        element.as_str(),
        &[],
        &context,
        None,
    )?;
    Some(AnnotationTarget::Element(method))
}

/// The field a value's name denotes ([JLS §6.5.6.1], [§6.5.6.2], [§7.5.4]),
/// through the same resolution the element-value checks use. A name written
/// outside every item (a package annotation's value) has no declaration scope
/// to read a field from, so it resolves to nothing.
fn field_target(
    cx: &NavCtx<'_>,
    qualifier: Option<&Name>,
    member: &Name,
) -> Option<AnnotationTarget> {
    let item = cx.item?;
    let value = ValueCtx {
        db: cx.db,
        file: cx.file,
        item,
        scope: cx.scope,
        resolver: cx.resolver,
        bodies: &cx.bodies,
    };
    match name_target(&value, qualifier, member) {
        NameTarget::Field(field) => Some(AnnotationTarget::Field(field)),
        NameTarget::NotQualified | NameTarget::Unresolved => None,
    }
}

/// The class the type name `text` written at the annotation's item denotes
/// ([JLS §6.5.5.1]). A name that resolves to a type that is not accessible
/// still denotes it — navigation is not a compile check ([§7.4.3]).
fn type_target(cx: &NavCtx<'_>, text: &str) -> Option<AnnotationTarget> {
    if text.is_empty() {
        return None;
    }
    match resolve_type_name_at(cx.db, cx.file, cx.item, &source_name(text)) {
        NameResolution::Resolved(fqn) | NameResolution::NotAccessible(fqn) => {
            Some(AnnotationTarget::Type(fqn))
        }
        // A local declaration has no canonical name for a navigation target.
        NameResolution::TypeVar
        | NameResolution::ResolvedLocal(_)
        | NameResolution::Ambiguous(_)
        | NameResolution::Unresolved => None,
    }
}

/// Whether `fqn` denotes an annotation interface ([JLS §9.6]) — the only
/// declaration kind that has annotation elements.
fn is_annotation_interface(db: &dyn TyDatabase, scope: &hir::ResolutionScope, fqn: &Name) -> bool {
    match hir::fqn_resolve(db, scope, fqn.as_str()) {
        Some(hir::Resolved::Source(class)) => matches!(
            hir::java_item_tree(db, class.file).data(class.item),
            ItemData::Annotation(_)
        ),
        // A Kotlin file's facade is no annotation interface.
        Some(hir::Resolved::KotlinFacade { .. }) => false,
        Some(hir::Resolved::Library(resolved)) => {
            hir::class_record(db, &resolved).is_some_and(|record| {
                matches!(
                    record.as_ref(),
                    hir::ClassOrModuleRecord::Class(class)
                        if hir::ClassKind::from_flags(class.flags, class.is_record)
                            == hir::ClassKind::Annotation
                )
            })
        }
        None => false,
    }
}
