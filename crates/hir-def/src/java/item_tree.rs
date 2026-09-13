//! The lowered per-file declaration model ("item tree", after rust-analyzer).
//!
//! Lowering turns a language's CST into this flat, arena-based IR: every
//! top-level type, member, field, enum constant, initializer and module
//! directive gets a stable [`ItemId`]. The *bodies* of methods, initializers,
//! field initializers, enum constant arguments and annotation element defaults
//! are lowered into the per-file [`hir_expand::body::BodyTree`], which lives
//! *beside* the item tree ([`crate::item_tree::LoweredFile`]) rather than
//! inside it: keeping the body content out of the memoized item tree lets
//! salsa backdate the signature-level queries across edits that only touch a
//! method body.
//!
//! The item tree carries **no source offsets**: every lowered declaration
//! anchors itself to its syntax node with a
//! [`FileAstId`](hir_expand::ast_id_map::FileAstId), and the source ranges of
//! items, names, type references and annotations are resolved on demand from
//! the current syntax tree ([`crate::java::ranges`]). The pointer-based ids
//! are a function of the file's *declaration skeleton* only — body content is
//! pruned from the id map — so a body-only edit leaves the tree's value
//! unchanged and salsa backdates every consumer.
//!
//! This is the *Java* declaration layer: the item kinds mirror the Java
//! grammar (classes, interfaces, enums, records, annotation types, modules,
//! methods, fields), and every declaration's source modifiers are carried as
//! [`crate::java::modifiers::JavaModifiers`]. Language-specific method
//! attributes are wrapped in [`MethodExtra::Java`], leaving the JVM-level
//! signature independent of the source language. Kotlin will lower its own
//! item tree against the same JVM substrate.

use hir_expand::{
    arena::Arena,
    ast_id_map::{AstIdMap, FileAstId, node_ptr},
    body::{BodyId, ExprId},
    name::Name,
    span::{AnnotationRef, AnnotationValue, SpannedTypeRef},
};
use rowan::SyntaxNode;
use syntax::java::{Lang, SyntaxKind as J};
use syntax::stub::TypeRef;

use crate::java::modifiers::JavaModifiers;

pub use base_db::LanguageKind;
pub use hir_expand::ids::ItemId;

/// The syntax-node markers of the [`FileAstId`]s stored in the item tree.
/// Zero-sized; they type the id's role without constraining its language.
pub struct ClassDeclNode;
pub struct InterfaceDeclNode;
pub struct EnumDeclNode;
pub struct RecordDeclNode;
pub struct AnnotationTypeDeclNode;
pub struct ModuleDeclNode;
pub struct MethodDeclNode;
pub struct FieldDeclNode;
pub struct DeclaratorNode;
pub struct EnumConstantNode;
pub struct StaticInitNode;
pub struct InstanceInitNode;
pub struct PackageDeclNode;
pub struct ImportDeclNode;
pub struct ComponentNode;
pub struct RequiresDirectiveNode;
pub struct ExportsDirectiveNode;
pub struct TypeNode;
pub struct AnnotationNode;

/// An import of a compilation unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportItem {
    pub name: Name,
    pub is_static: bool,
    pub is_asterisk: bool,
    /// The `IMPORT_DECL` syntax node of the import.
    pub path: FileAstId<ImportDeclNode>,
}

/// The per-file result of lowering.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemTree {
    pub language: LanguageKind,
    pub package: Option<Name>,
    /// The `PACKAGE_DECL` syntax node of every package declaration, in source
    /// order ([JLS §7.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.1):
    /// a compilation unit declares at most one package). More than one entry
    /// is the duplicate-package error the declaration diagnostics report; the
    /// first entry names the package symbol the IDE surfaces above the file's
    /// top-level types.
    pub package_decls: Vec<FileAstId<PackageDeclNode>>,
    pub imports: Vec<ImportItem>,
    pub top: Vec<ItemId>,
    /// The local class/interface/enum/record declarations of the file, in
    /// source order
    /// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3)):
    /// they are not members of any class, so they stay out of every `body()`
    /// and are reachable only through this list and through the body that
    /// declares them.
    pub local_types: Vec<ItemId>,
    /// The declaration each item is nested in: a member's enclosing type-like
    /// declaration, a local type's body-owning declaration. Indexed by item
    /// id — grown in lock-step with `items` by [`ItemTree::alloc`] — and
    /// `None` for a top-level item.
    pub parent: Vec<Option<ItemId>>,
    pub items: Arena<ItemData>,
}

impl Default for ItemTree {
    fn default() -> Self {
        Self {
            language: LanguageKind::Unknown,
            package: None,
            package_decls: Vec::new(),
            imports: Vec::new(),
            top: Vec::new(),
            local_types: Vec::new(),
            parent: Vec::new(),
            items: Arena::default(),
        }
    }
}

impl ItemTree {
    pub fn data(&self, id: ItemId) -> &ItemData {
        self.items.get(id.0)
    }

    /// Allocates an item, keeping [`Self::parent`] aligned with the arena. A
    /// direct `items.alloc` would desynchronize the two; allocate through
    /// here.
    pub fn alloc(&mut self, data: ItemData) -> ItemId {
        let id = ItemId(self.items.alloc(data));
        self.parent.push(None);
        id
    }

    /// The declaration `item` is nested in; `None` for a top-level item.
    pub fn parent_of(&self, item: ItemId) -> Option<ItemId> {
        self.parent.get(item.0.0 as usize).copied().flatten()
    }

    /// The local type declarations nested in `owner`, in source order.
    pub fn local_types_of(&self, owner: ItemId) -> impl Iterator<Item = ItemId> + '_ {
        self.local_types
            .iter()
            .copied()
            .filter(move |item| self.parent_of(*item) == Some(owner))
    }

    /// Whether `item` is a local type declaration
    /// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3)).
    pub fn is_local_type(&self, item: ItemId) -> bool {
        self.local_types.contains(&item)
    }

    /// The id viewed as a class-like type id, if the item is a class,
    /// interface, enum, record or annotation type.
    pub fn as_class(&self, id: ItemId) -> Option<ClassId> {
        self.data(id).is_type().then_some(ClassId(id))
    }

    /// The id viewed as a method id, if the item is a method, constructor or
    /// annotation element.
    pub fn as_method(&self, id: ItemId) -> Option<MethodId> {
        self.data(id).is_method().then_some(MethodId(id))
    }

    /// The id viewed as a field id, if the item is a field.
    pub fn as_field(&self, id: ItemId) -> Option<FieldId> {
        self.data(id).is_field().then_some(FieldId(id))
    }

    /// The declaration data of a method item.
    ///
    /// # Panics
    /// If `id` was not produced by [`Self::as_method`] (or does not name a
    /// method item at all).
    pub fn method(&self, id: MethodId) -> &MethodData {
        match self.data(id.0) {
            ItemData::Method(data) => data,
            _ => panic!("MethodId for non-method item: {id:?}"),
        }
    }

    /// The declaration data of a field item.
    ///
    /// # Panics
    /// If `id` was not produced by [`Self::as_field`] (or does not name a
    /// field item at all).
    pub fn field(&self, id: FieldId) -> &FieldData {
        match self.data(id.0) {
            ItemData::Field(data) => data,
            _ => panic!("FieldId for non-field item: {id:?}"),
        }
    }
}

/// A lowered declaration or member.
#[derive(Debug, Clone, PartialEq)]
pub enum ItemData {
    Class(ClassData),
    Interface(ClassData),
    Enum(EnumData),
    Record(RecordData),
    Annotation(AnnotationData),
    Module(ModuleData),
    Method(MethodData),
    Field(FieldData),
    EnumConstant(EnumConstantData),
    StaticInit(StaticInitData),
    InstanceInit(InstanceInitData),
}

/// The kind of a lowered item ([JLS §7.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.6)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
    Module,
    Method,
    Field,
    EnumConstant,
    StaticInit,
    InstanceInit,
}

/// The id of a class-like item (a class, interface, enum, record or
/// annotation type) within its owning [`ItemTree`]. A typed view of an
/// [`ItemId`]; the bare id is recoverable through the tuple field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClassId(pub ItemId);

/// The id of a method item (a method, constructor or annotation element).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MethodId(pub ItemId);

/// The id of a field item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FieldId(pub ItemId);

impl ItemData {
    /// The kind of the item.
    pub fn kind(&self) -> ItemKind {
        match self {
            ItemData::Class(_) => ItemKind::Class,
            ItemData::Interface(_) => ItemKind::Interface,
            ItemData::Enum(_) => ItemKind::Enum,
            ItemData::Record(_) => ItemKind::Record,
            ItemData::Annotation(_) => ItemKind::Annotation,
            ItemData::Module(_) => ItemKind::Module,
            ItemData::Method(_) => ItemKind::Method,
            ItemData::Field(_) => ItemKind::Field,
            ItemData::EnumConstant(_) => ItemKind::EnumConstant,
            ItemData::StaticInit(_) => ItemKind::StaticInit,
            ItemData::InstanceInit(_) => ItemKind::InstanceInit,
        }
    }

    /// Whether the item is a class-like type declaration (a class, interface,
    /// enum, record or annotation type).
    pub fn is_type(&self) -> bool {
        matches!(
            self,
            ItemData::Class(_)
                | ItemData::Interface(_)
                | ItemData::Enum(_)
                | ItemData::Record(_)
                | ItemData::Annotation(_)
        )
    }

    /// Whether the item is a method, constructor or annotation element.
    pub fn is_method(&self) -> bool {
        matches!(self, ItemData::Method(_))
    }

    /// Whether the item is a field.
    pub fn is_field(&self) -> bool {
        matches!(self, ItemData::Field(_))
    }

    /// The declaration data, if the item is a method, constructor or
    /// annotation element.
    pub fn as_method(&self) -> Option<&MethodData> {
        match self {
            ItemData::Method(data) => Some(data),
            _ => None,
        }
    }

    /// The declaration data, if the item is a field.
    pub fn as_field(&self) -> Option<&FieldData> {
        match self {
            ItemData::Field(data) => Some(data),
            _ => None,
        }
    }

    /// The declared name of a declaration item, if it has one (an initializer
    /// block declares no name).
    pub fn name(&self) -> Option<&Name> {
        match self {
            ItemData::Class(data) | ItemData::Interface(data) => Some(&data.name),
            ItemData::Enum(data) => Some(&data.name),
            ItemData::Record(data) => Some(&data.name),
            ItemData::Annotation(data) => Some(&data.name),
            ItemData::Module(data) => Some(&data.name),
            ItemData::Method(data) => Some(&data.name),
            ItemData::Field(data) => Some(&data.name),
            ItemData::EnumConstant(data) => Some(&data.name),
            ItemData::StaticInit(_) | ItemData::InstanceInit(_) => None,
        }
    }

    /// The nested member items of a type item, if any.
    pub fn body(&self) -> &[ItemId] {
        match self {
            ItemData::Class(d) | ItemData::Interface(d) => &d.body,
            ItemData::Enum(d) => &d.body,
            ItemData::Record(d) => &d.body,
            ItemData::Annotation(d) => &d.body,
            // §8.9.1/[§15.9.1]: an enum constant's class body declares
            // members of the anonymous class the constant denotes.
            ItemData::EnumConstant(d) => &d.body,
            _ => &[],
        }
    }

    /// A display label used by [`crate::java::pretty::pretty_print`].
    pub fn label(&self) -> &'static str {
        match self {
            ItemData::Class(_) => "class",
            ItemData::Interface(_) => "interface",
            ItemData::Enum(_) => "enum",
            ItemData::Record(_) => "record",
            ItemData::Annotation(_) => "@interface",
            ItemData::Module(_) => "module",
            ItemData::Method(_) => "method",
            ItemData::Field(_) => "field",
            ItemData::EnumConstant(_) => "constant",
            ItemData::StaticInit(_) => "static block",
            ItemData::InstanceInit(_) => "instance block",
        }
    }
}

/// A declaration-side source type reference: the lowered [`TypeRef<Name>`]
/// plus the reference names it contains (depth-first, in the same order the
/// source-spanned form keeps) and the syntax node of the type itself, from
/// which the per-name source ranges are re-derived on demand
/// ([`crate::java::ranges::type_ref_occurrences`]). The item tree stores no
/// source offsets; `node` is a [`FileAstId`] into the file's
/// [`AstIdMap`](hir_expand::ast_id_map::AstIdMap).
#[derive(Debug, Clone, PartialEq)]
pub struct ItemTypeRef {
    pub ty: TypeRef<Name>,
    /// The reference names of `ty`, depth-first, names only. The source
    /// ranges are computed from `node` by [`crate::java::ranges`].
    pub refs: Vec<Name>,
    /// The type-use annotations of the type
    /// ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4),
    /// `int @Nullable []`, `List<@NonNull T>`), in the same (flattened,
    /// depth-first) order the ranged [`SpannedTypeRef`] keeps. The annotation
    /// names also appear in [`ItemTypeRef::refs`], so they resolve like any
    /// type name.
    pub type_use_annotations: Vec<ItemAnnotationRef>,
    /// The `TYPE` syntax node of the type (or the `QUALIFIED_NAME` node for a
    /// module directive's service / implementation reference).
    pub node: FileAstId<TypeNode>,
}

impl std::ops::Deref for ItemTypeRef {
    type Target = TypeRef<Name>;

    fn deref(&self) -> &TypeRef<Name> {
        &self.ty
    }
}

impl ItemTypeRef {
    /// Converts a source-spanned type reference (as lowered by
    /// [`crate::java::lower::walk`]) into its range-free item form, keeping
    /// the reference-name order, the type-use annotations and the resolved
    /// syntax-node id of the type.
    pub fn from_spanned(spanned: SpannedTypeRef, node: &SyntaxNode<Lang>, map: &AstIdMap) -> Self {
        let node_id = map
            .ast_id(&node_ptr(node))
            .unwrap_or_else(FileAstId::placeholder);
        // The type-use annotations of the whole type subtree, in the same
        // depth-first order `type_from` flattens them.
        let annotation_ids = type_use_annotation_ids(node, map);
        let type_annotations = spanned
            .type_use_annotations
            .into_iter()
            .zip(annotation_ids)
            .map(|(annotation, id)| {
                ItemAnnotationRef::from_parts(annotation, id, &mut None.into_iter(), map)
            })
            .collect();
        Self {
            ty: spanned.ty,
            refs: spanned
                .refs
                .into_iter()
                .map(|reference| reference.name)
                .collect(),
            type_use_annotations: type_annotations,
            node: node_id,
        }
    }

    /// A type reference synthesized during lowering (a missing or error
    /// type), naming no syntax node. Its occurrence list is empty, and range
    /// resolution of its placeholder id never happens.
    pub fn synthetic(ty: TypeRef<Name>) -> Self {
        Self {
            ty,
            refs: Vec::new(),
            type_use_annotations: Vec::new(),
            node: FileAstId::placeholder(),
        }
    }
}

/// A declaration-side annotation with its element-value arguments
/// ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7),
/// [§9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1)) —
/// the range-free twin of [`AnnotationRef`]. The (possibly qualified)
/// annotation name is [`ItemAnnotationRef::name`] and the annotation's syntax
/// node is [`ItemAnnotationRef::node`], from which the name's source range is
/// re-derived on demand.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemAnnotationRef {
    pub name: Name,
    /// The element-value pairs of the argument list ([§9.7.1]), in source
    /// order. Empty for a marker annotation (`@Foo`).
    pub args: Vec<ItemAnnotationArg>,
    /// The `ANNOTATION`/`MARKER_ANNOTATION` syntax node of the annotation.
    pub node: FileAstId<AnnotationNode>,
}

impl ItemAnnotationRef {
    /// Converts a source-spanned annotation (as lowered by
    /// [`crate::java::lower::walk`]) into its range-free item form, resolving
    /// the syntax-node ids of the annotation and of every nested annotation
    /// in its argument values.
    pub fn from_spanned(spanned: AnnotationRef, node: &SyntaxNode<Lang>, map: &AstIdMap) -> Self {
        let node_id = map
            .ast_id(&node_ptr(node))
            .unwrap_or_else(FileAstId::placeholder);
        // The nested annotations (the element-value annotations of the
        // argument list, depth-first) in the same order the ranged walk
        // produces them.
        let nested = annotation_node_ids(node, map);
        Self::from_parts(spanned, node_id, &mut nested.into_iter(), map)
    }

    fn from_parts(
        spanned: AnnotationRef,
        node: FileAstId<AnnotationNode>,
        nested: &mut dyn Iterator<Item = FileAstId<AnnotationNode>>,
        map: &AstIdMap,
    ) -> Self {
        let args = spanned
            .args
            .into_iter()
            .map(|arg| {
                let value = match arg.value {
                    AnnotationValue::Literal(literal) => ItemAnnotationValue::Literal(literal),
                    AnnotationValue::EnumConstant { qualifier, member } => {
                        ItemAnnotationValue::EnumConstant { qualifier, member }
                    }
                    AnnotationValue::ClassLit(ty) => ItemAnnotationValue::ClassLit(Box::new(
                        ItemTypeRef::from_spanned_impl(ty, map),
                    )),
                    AnnotationValue::Annotation(inner) => {
                        let id = nested.next().unwrap_or_else(FileAstId::placeholder);
                        ItemAnnotationValue::Annotation(Box::new(ItemAnnotationRef::from_parts(
                            *inner, id, nested, map,
                        )))
                    }
                    AnnotationValue::Array(values) => ItemAnnotationValue::Array(
                        values
                            .into_iter()
                            .map(|value| convert_annotation_value(value, nested, map))
                            .collect(),
                    ),
                    AnnotationValue::Expr(expr) => ItemAnnotationValue::Expr(expr),
                    AnnotationValue::Unresolved { text } => {
                        ItemAnnotationValue::Unresolved { text }
                    }
                };
                ItemAnnotationArg {
                    name: arg.name,
                    value,
                }
            })
            .collect();
        Self {
            name: spanned.name.name,
            args,
            node,
        }
    }
}

/// One element-value pair `name = value` of an annotation
/// ([JLS §9.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.1),
/// [§9.7.1]) — the range-free twin of [`hir_expand::span::AnnotationArg`].
/// The value's source range is re-derived on demand from the owning
/// annotation's syntax node ([`crate::java::ranges::annotation_arg_value_range`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ItemAnnotationArg {
    /// The element name of the pair; `value` for the implicit single-argument
    /// form ([§9.7.1]).
    pub name: Name,
    pub value: ItemAnnotationValue,
}

/// The value of an annotation element ([JLS §9.7.1]) — the range-free twin of
/// [`hir_expand::span::AnnotationValue`].
#[derive(Debug, Clone, PartialEq)]
pub enum ItemAnnotationValue {
    /// A constant literal ([JLS §15.28]) — a primitive, string or text-block
    /// literal.
    Literal(hir_expand::body::Literal),
    /// An enum constant ([§8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1)):
    /// `Type.CONSTANT` with its qualifier, or a bare `CONSTANT`, whose
    /// declaring type is inferred from the element's type ([§9.7.1]).
    EnumConstant {
        qualifier: Option<Name>,
        member: Name,
    },
    /// A class literal `Foo.class` ([§15.8.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.2)).
    ClassLit(Box<ItemTypeRef>),
    /// A nested annotation ([§9.7.1]).
    Annotation(Box<ItemAnnotationRef>),
    /// An array initializer `{ v1, v2 }` ([§10.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.6)).
    Array(Vec<ItemAnnotationValue>),
    /// An element value that is not one of the literal forms above — a unary,
    /// binary, conditional, parenthesized, cast or `null` expression — as an
    /// expression of the file's body tree ([JLS §9.7.1]: the value is a
    /// `ConditionalExpression`).
    Expr(hir_expand::body::ExprId),
    /// An element value whose expression arena is unavailable, kept as its raw
    /// source text — the annotation of a *written type* ([§9.7.4]), which has
    /// no owning declaration to lower the value against.
    Unresolved { text: String },
}

fn convert_annotation_value(
    value: AnnotationValue,
    nested: &mut dyn Iterator<Item = FileAstId<AnnotationNode>>,
    map: &AstIdMap,
) -> ItemAnnotationValue {
    match value {
        AnnotationValue::Literal(literal) => ItemAnnotationValue::Literal(literal),
        AnnotationValue::EnumConstant { qualifier, member } => {
            ItemAnnotationValue::EnumConstant { qualifier, member }
        }
        AnnotationValue::ClassLit(ty) => {
            ItemAnnotationValue::ClassLit(Box::new(ItemTypeRef::from_spanned_impl(ty, map)))
        }
        AnnotationValue::Annotation(inner) => {
            let id = nested.next().unwrap_or_else(FileAstId::placeholder);
            ItemAnnotationValue::Annotation(Box::new(ItemAnnotationRef::from_parts(
                *inner, id, nested, map,
            )))
        }
        AnnotationValue::Array(values) => ItemAnnotationValue::Array(
            values
                .into_iter()
                .map(|value| convert_annotation_value(value, nested, map))
                .collect(),
        ),
        AnnotationValue::Expr(expr) => ItemAnnotationValue::Expr(expr),
        AnnotationValue::Unresolved { text } => ItemAnnotationValue::Unresolved { text },
    }
}

impl ItemTypeRef {
    /// The range-free conversion of a type reference with *no* resolvable
    /// syntax node — a class literal's type inside an annotation element
    /// value ([§15.8.2]). Its node is a placeholder; only the `TypeRef` and
    /// the reference-name order are ever read.
    fn from_spanned_impl(spanned: SpannedTypeRef, map: &AstIdMap) -> Self {
        Self {
            ty: spanned.ty,
            refs: spanned
                .refs
                .into_iter()
                .map(|reference| reference.name)
                .collect(),
            type_use_annotations: spanned
                .type_use_annotations
                .into_iter()
                .map(|annotation| {
                    ItemAnnotationRef::from_parts(
                        annotation,
                        FileAstId::placeholder(),
                        &mut None.into_iter(),
                        map,
                    )
                })
                .collect(),
            node: FileAstId::placeholder(),
        }
    }
}

/// The `ANNOTATION`/`MARKER_ANNOTATION` descendant nodes of `node` that carry
/// a name, depth-first — the same set and order the ranged annotation walk
/// produces for nested annotation element values. (`rowan`'s
/// `descendants()` includes `node` itself; the annotation node is its own
/// outermost annotation, not one of its values.)
fn annotation_node_ids(node: &SyntaxNode<Lang>, map: &AstIdMap) -> Vec<FileAstId<AnnotationNode>> {
    let self_range = node.text_range();
    node.descendants()
        .filter(|descendant| {
            descendant.text_range() != self_range
                && matches!(descendant.kind(), J::ANNOTATION | J::MARKER_ANNOTATION)
                && annotation_has_name(descendant)
        })
        .map(|descendant| {
            map.ast_id(&node_ptr(&descendant))
                .unwrap_or_else(FileAstId::placeholder)
        })
        .collect()
}

/// Whether the annotation node names a qualified name (an annotation without
/// a name is skipped by lowering's `annotation_ref`, so ids must skip it too
/// to stay aligned).
fn annotation_has_name(node: &SyntaxNode<Lang>) -> bool {
    node.descendants().any(|d| d.kind() == J::QUALIFIED_NAME)
}

/// The ids of every type-use annotation of `node`'s type subtree, in the
/// depth-first order `type_from` flattens `SpannedTypeRef::type_use_annotations`:
/// this node's own `MODIFIER_LIST`/`DIMENSIONS`/`DIMENSION` annotations, then
/// each generic argument type's, recursively. Wildcard bounds contribute none
/// (a wildcard's structured annotation list is empty).
fn type_use_annotation_ids(
    node: &SyntaxNode<Lang>,
    map: &AstIdMap,
) -> Vec<FileAstId<AnnotationNode>> {
    let mut out = Vec::new();
    for child in node.children() {
        if !matches!(
            child.kind(),
            J::MODIFIER_LIST | J::DIMENSIONS | J::DIMENSION
        ) {
            continue;
        }
        for annotation in child.descendants() {
            if matches!(annotation.kind(), J::ANNOTATION | J::MARKER_ANNOTATION)
                && annotation_has_name(&annotation)
            {
                out.push(
                    map.ast_id(&node_ptr(&annotation))
                        .unwrap_or_else(FileAstId::placeholder),
                );
            }
        }
    }
    for arguments in node
        .children()
        .filter(|child| child.kind() == J::TYPE_ARGUMENTS)
    {
        for argument in arguments
            .children()
            .filter(|child| child.kind() == J::TYPE_ARGUMENT)
        {
            if let Some(ty) = argument.children().find(|child| child.kind() == J::TYPE) {
                out.extend(type_use_annotation_ids(&ty, map));
            }
        }
    }
    out
}

/// A class or interface declaration (they share the same layout).
#[derive(Debug, Clone, PartialEq)]
pub struct ClassData {
    pub name: Name,
    pub modifiers: JavaModifiers,
    /// The annotation references of the declaration, in source order
    /// ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7)),
    /// decoupled from the modifier flags.
    pub annotations: Vec<ItemAnnotationRef>,
    pub super_class: Option<ItemTypeRef>,
    pub interfaces: Vec<ItemTypeRef>,
    /// The permitted direct subclasses of a `sealed` class or interface
    /// ([§8.1.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.1.2)),
    /// from its `permits` clause; empty when the declaration has none (the
    /// permitted set is then the same-module direct subclasses).
    pub permits: Vec<ItemTypeRef>,
    pub type_params: Vec<TypeParam>,
    pub body: Vec<ItemId>,
    /// The `CLASS_DECL`/`INTERFACE_DECL` syntax node of the declaration.
    pub ast: FileAstId<ClassDeclNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumData {
    pub name: Name,
    pub modifiers: JavaModifiers,
    pub annotations: Vec<ItemAnnotationRef>,
    pub interfaces: Vec<ItemTypeRef>,
    pub body: Vec<ItemId>,
    /// The `ENUM_DECL` syntax node of the declaration.
    pub ast: FileAstId<EnumDeclNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordData {
    pub name: Name,
    pub modifiers: JavaModifiers,
    pub annotations: Vec<ItemAnnotationRef>,
    pub components: Vec<RecordComponent>,
    pub interfaces: Vec<ItemTypeRef>,
    /// The permitted direct subclasses of a `sealed` record
    /// ([§8.1.1.2]), from its `permits` clause.
    pub permits: Vec<ItemTypeRef>,
    pub type_params: Vec<TypeParam>,
    pub body: Vec<ItemId>,
    /// The `RECORD_DECL` syntax node of the declaration. The outline's
    /// component list and declaration-header ranges are derived from it
    /// ([`crate::java::ranges::record_components_range`] /
    /// [`crate::java::ranges::record_header_range`]).
    pub ast: FileAstId<RecordDeclNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationData {
    pub name: Name,
    pub modifiers: JavaModifiers,
    pub annotations: Vec<ItemAnnotationRef>,
    pub body: Vec<ItemId>,
    /// The `ANNOTATION_TYPE_DECL` syntax node of the declaration.
    pub ast: FileAstId<AnnotationTypeDeclNode>,
}

/// A method signature: type parameters, parameters, return type and thrown
/// exceptions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Signature {
    pub type_params: Vec<TypeParam>,
    pub params: Vec<Param>,
    pub ret: Option<ItemTypeRef>,
    pub throws: Vec<ItemTypeRef>,
}

/// A formal parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: Name,
    pub ty: ItemTypeRef,
    pub varargs: bool,
    /// The annotation modifiers of the parameter declaration
    /// ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4),
    /// `void m(@A int p)`, `void m(@A String... p)`), in source order. The
    /// type annotations of `ty` are separate: they annotate the *type*
    /// ([§9.7.4]).
    pub annotations: Vec<ItemAnnotationRef>,
}

/// The language-specific attributes of a method declaration, abstracted out of
/// the JVM-level [`MethodData`] core so the substrate stays language-neutral:
/// a Kotlin method will carry `MethodExtra::Kotlin(...)` with its own
/// attributes instead of these Java ones.
#[derive(Debug, Clone, PartialEq)]
pub enum MethodExtra {
    /// A Java method, constructor or annotation element
    /// ([JLS §8.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4),
    /// [§8.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8),
    /// [§9.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.1)).
    Java(MethodExtraJava),
}

/// The Java-specific attributes of a [`MethodData`]: constructor-ness and the
/// lowered body (the annotation element default's *range* is derived from the
/// method's syntax node on demand; its lowered expression lives here).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MethodExtraJava {
    /// Whether the method is a constructor or compact constructor
    /// ([JLS §8.8]).
    pub is_constructor: bool,
    /// Whether the constructor is a record *compact* constructor
    /// ([JLS §8.10.4]): one whose parameter list is the record's component
    /// list, written as `record R(int x) { R { … } }`. A compact constructor
    /// declares no formal parameters, so its signature in [`Signature::params`]
    /// is empty; the component list supplies the *implicit* parameters that
    /// the compact body assigns. `false` for ordinary constructors (including
    /// a genuine zero-argument one) and methods.
    pub is_compact_constructor: bool,
    /// The lowered body of the method, if it declares one.
    pub body: Option<BodyId>,
    /// The lowered default-value expression of an annotation element.
    pub default_expr: Option<ExprId>,
}

/// A method, constructor or annotation element.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodData {
    pub name: Name,
    pub modifiers: JavaModifiers,
    pub annotations: Vec<ItemAnnotationRef>,
    pub sig: Signature,
    /// The language-specific attributes of the declaration (Java constructor
    /// / body / annotation default, Kotlin attributes later).
    pub extra: MethodExtra,
    /// The `METHOD_DECL`/`CONSTRUCTOR_DECL`/`COMPACT_CONSTRUCTOR_DECL`/
    /// `ANNOTATION_TYPE_ELEMENT_DECL` syntax node of the declaration (one
    /// marker for all four kinds; the id is its position in the id map).
    pub ast: FileAstId<MethodDeclNode>,
}

impl MethodData {
    /// Whether the method is a Java constructor or compact constructor
    /// ([JLS §8.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8)).
    pub fn is_constructor(&self) -> bool {
        matches!(&self.extra, MethodExtra::Java(java) if java.is_constructor)
    }

    /// Whether the constructor is a record *compact* constructor
    /// ([JLS §8.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.4)):
    /// one whose parameter list is the record's component list. Its declared
    /// formal-parameter list is empty (the signature is the component list),
    /// which matters wherever a constructor's arity is derived.
    pub fn is_compact_constructor(&self) -> bool {
        matches!(
            &self.extra,
            MethodExtra::Java(java) if java.is_compact_constructor
        )
    }

    /// The lowered body of the method, if it declares one.
    pub fn body(&self) -> Option<BodyId> {
        match &self.extra {
            MethodExtra::Java(java) => java.body,
        }
    }

    /// The lowered default-value expression of an annotation element.
    pub fn default_expr(&self) -> Option<ExprId> {
        match &self.extra {
            MethodExtra::Java(java) => java.default_expr,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldData {
    pub name: Name,
    pub modifiers: JavaModifiers,
    pub annotations: Vec<ItemAnnotationRef>,
    pub ty: ItemTypeRef,
    /// Whether the declarator has an explicit initializer (an `=` sign).
    /// Distinct from `initializer_expr`, which is `None` when the `=` exists
    /// but its expression failed to lower.
    pub has_initializer: bool,
    pub initializer_expr: Option<ExprId>,
    /// The `VARIABLE_DECLARATOR` syntax node of the field.
    pub ast: FileAstId<DeclaratorNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumConstantData {
    pub name: Name,
    pub argument_exprs: Vec<ExprId>,
    /// The members of the constant's class body, in source order — the
    /// ordinary class body an enum constant may carry
    /// ([§8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1),
    /// [§15.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.1)):
    /// the constant denotes an anonymous class that extends the enum, so its
    /// fields, methods and initializers are members of that class, declared
    /// *inside* the constant. Empty for a constant without a class body.
    pub body: Vec<ItemId>,
    /// The `ENUM_CONSTANT` syntax node of the constant; its argument list and
    /// constant class body ranges are derived from it on demand.
    pub ast: FileAstId<EnumConstantNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticInitData {
    pub body: Option<BodyId>,
    /// The `STATIC_INITIALIZER` syntax node of the initializer.
    pub ast: FileAstId<StaticInitNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceInitData {
    pub body: Option<BodyId>,
    /// The `INSTANCE_INITIALIZER` syntax node of the initializer.
    pub ast: FileAstId<InstanceInitNode>,
}

/// A JPMS `module-info.java` declaration, lowered minimally: directives are
/// kept at the level of names / types (flags such as `transitive` are folded
/// into booleans).
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleData {
    pub name: Name,
    pub modifiers: JavaModifiers,
    pub annotations: Vec<ItemAnnotationRef>,
    /// Whether the module was declared `open`.
    pub is_open: bool,
    pub requires: Vec<ModuleRequires>,
    pub exports: Vec<ModuleExports>,
    pub opens: Vec<ModuleExports>,
    pub uses: Vec<ItemTypeRef>,
    pub provides: Vec<ModuleProvides>,
    /// The `MODULE_DECL` syntax node of the declaration.
    pub ast: FileAstId<ModuleDeclNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleRequires {
    pub name: Name,
    pub transitive: bool,
    pub statik: bool,
    /// The `REQUIRES_DIRECTIVE` syntax node of the directive; the required
    /// module name's range is derived from it on demand.
    pub ast: FileAstId<RequiresDirectiveNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleExports {
    pub package: Name,
    pub to: Vec<Name>,
    /// The `EXPORTS_DIRECTIVE`/`OPENS_DIRECTIVE` syntax node of the directive.
    pub ast: FileAstId<ExportsDirectiveNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleProvides {
    pub service: ItemTypeRef,
    pub implementations: Vec<ItemTypeRef>,
}

/// A declared type parameter of a class or method
/// ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
/// with source-spanned bounds and the annotations on the type-parameter
/// declaration ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4)).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: Name,
    pub bounds: Vec<ItemTypeRef>,
    pub annotations: Vec<ItemAnnotationRef>,
}

impl TypeParam {
    /// The declaring-scope marker of this type parameter ([JLS §6.4.1],
    /// [§8.4.4]): a class/interface/enum/record type parameter (`"c"`) or a
    /// method/constructor type parameter (`"m"`). A method parameter shadows
    /// a same-named class parameter and the two are *distinct* type
    /// variables, so the resolution of a name to a variable must know which
    /// scope the innermost declaration belongs to.
    pub fn scope_kind(&self, method: bool) -> &'static str {
        if method { "m" } else { "c" }
    }
}

/// A record component declaration `T name`
/// ([JLS §8.10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.1)),
/// with the source-spanned component type and the annotations on the
/// component declaration ([JLS §9.7.4]).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordComponent {
    pub name: Name,
    /// The `FORMAL_PARAMETER`/`SPREAD_PARAMETER` syntax node of the component
    /// declaration; the component's full, name and type ranges are derived
    /// from it on demand.
    pub ast: FileAstId<ComponentNode>,
    pub ty: ItemTypeRef,
    /// Whether the component was declared varargs (`String... names`).
    pub varargs: bool,
    pub annotations: Vec<ItemAnnotationRef>,
}
