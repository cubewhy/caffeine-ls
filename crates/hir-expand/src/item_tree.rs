//! The lowered per-file declaration model ("item tree", after rust-analyzer).
//!
//! Lowering turns a language's CST into this flat, arena-based IR: every
//! top-level type, member, field, enum constant, initializer and module
//! directive gets a stable [`ItemId`]. The *bodies* of methods, initializers,
//! field initializers, enum constant arguments and annotation element defaults
//! are lowered into the per-file [`crate::body::BodyTree`], which lives
//! *beside* the item tree ([`LoweredFile`]) rather than inside it: keeping the
//! body content out of the memoized item tree lets salsa backdate the
//! signature-level queries across edits that only touch a method body.

use std::sync::Arc;

use rowan::TextRange;

use crate::{
    arena::{Arena, ArenaId},
    body::{BodyId, BodyTree, ExprId},
    modifiers::Modifiers,
    name::Name,
    span::{NameRef, SpannedTypeRef},
};

pub use base_db::LanguageKind;

/// The id of an item within its owning [`ItemTree`]. Stable across salsa
/// queries; combine with a `FileId` for a workspace-unique id ([`crate::item_loc::ItemLoc`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemId(pub ArenaId);

/// An import of a compilation unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportItem {
    pub name: Name,
    pub is_static: bool,
    pub is_asterisk: bool,
    pub range: TextRange,
}

/// The per-file result of lowering.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemTree {
    pub language: LanguageKind,
    pub package: Option<Name>,
    /// The source range of the package declaration's name, when the file
    /// declares a package; used by the IDE to surface a package symbol above
    /// the file's top-level types.
    pub package_range: Option<TextRange>,
    pub imports: Vec<ImportItem>,
    pub top: Vec<ItemId>,
    pub items: Arena<ItemData>,
}

impl Default for ItemTree {
    fn default() -> Self {
        Self {
            language: LanguageKind::Unknown,
            package: None,
            package_range: None,
            imports: Vec::new(),
            top: Vec::new(),
            items: Arena::default(),
        }
    }
}

impl ItemTree {
    pub fn data(&self, id: ItemId) -> &ItemData {
        self.items.get(id.0)
    }
}

/// The full per-file lowering: the declaration [`ItemTree`] plus the body IR
/// ([`crate::body::BodyTree`]), lowered together in one pass so the body ids
/// stored in the item data line up with the body arenas. Computed by a single
/// salsa query (`hir_def::db::lower_source_query`) and read through its
/// [`file_item_tree`](hir_def::file_item_tree) /
/// [`file_body_tree`](hir_def::file_body_tree) accessors. Because the item
/// tree carries no body content, edits that only change a method body leave
/// its value unchanged, letting salsa backdate signature consumers
/// (`file_symbols_query`, `supertypes_query`, ...) instead of re-running them.
#[derive(Debug, Clone, PartialEq)]
pub struct LoweredFile {
    pub items: Arc<ItemTree>,
    pub bodies: Arc<BodyTree>,
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

impl ItemData {
    /// The source range of the item.
    pub fn range(&self) -> TextRange {
        match self {
            ItemData::Class(d) | ItemData::Interface(d) => d.range,
            ItemData::Enum(d) => d.range,
            ItemData::Record(d) => d.range,
            ItemData::Annotation(d) => d.range,
            ItemData::Module(d) => d.range,
            ItemData::Method(d) => d.range,
            ItemData::Field(d) => d.range,
            ItemData::EnumConstant(d) => d.range,
            ItemData::StaticInit(d) => d.range,
            ItemData::InstanceInit(d) => d.range,
        }
    }

    /// The nested member items of a type item, if any.
    pub fn body(&self) -> &[ItemId] {
        match self {
            ItemData::Class(d) | ItemData::Interface(d) => &d.body,
            ItemData::Enum(d) => &d.body,
            ItemData::Record(d) => &d.body,
            ItemData::Annotation(d) => &d.body,
            _ => &[],
        }
    }

    /// The source range of the item's declared name (the identifier), used by
    /// the IDE to set the LSP `selectionRange`. Initializers are nameless and
    /// fall back to their whole range.
    pub fn name_range(&self) -> TextRange {
        match self {
            ItemData::Class(d) | ItemData::Interface(d) => d.name_range,
            ItemData::Enum(d) => d.name_range,
            ItemData::Record(d) => d.name_range,
            ItemData::Annotation(d) => d.name_range,
            ItemData::Module(d) => d.name_range,
            ItemData::Method(d) => d.name_range,
            ItemData::Field(d) => d.name_range,
            ItemData::EnumConstant(d) => d.name_range,
            ItemData::StaticInit(d) => d.range,
            ItemData::InstanceInit(d) => d.range,
        }
    }

    /// A display label used by [`crate::pretty::pretty_print`].
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

/// A class or interface declaration (they share the same layout).
#[derive(Debug, Clone, PartialEq)]
pub struct ClassData {
    pub name: Name,
    pub name_range: TextRange,
    pub modifiers: Modifiers,
    pub super_class: Option<SpannedTypeRef>,
    pub interfaces: Vec<SpannedTypeRef>,
    pub type_params: Vec<TypeParam>,
    pub body: Vec<ItemId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumData {
    pub name: Name,
    pub name_range: TextRange,
    pub modifiers: Modifiers,
    pub interfaces: Vec<SpannedTypeRef>,
    pub body: Vec<ItemId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordData {
    pub name: Name,
    pub name_range: TextRange,
    /// The source range of the component list — the record's parameter
    /// declaration `(int x, int y)`. The record's outline selection points
    /// here (its "definition").
    pub components_range: TextRange,
    /// The source range of the record declaration *header*: from the `record`
    /// keyword through the closing `)` of the component list (and any
    /// `implements` clause), excluding the body `{ ... }`. This is the
    /// declaration's "definition" — what the outline should point at.
    pub header_range: TextRange,
    pub modifiers: Modifiers,
    pub components: Vec<RecordComponent>,
    pub interfaces: Vec<SpannedTypeRef>,
    pub type_params: Vec<TypeParam>,
    pub body: Vec<ItemId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationData {
    pub name: Name,
    pub name_range: TextRange,
    pub modifiers: Modifiers,
    pub body: Vec<ItemId>,
    pub range: TextRange,
}

/// A method signature: type parameters, parameters, return type and thrown
/// exceptions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Signature {
    pub type_params: Vec<TypeParam>,
    pub params: Vec<Param>,
    pub ret: Option<SpannedTypeRef>,
    pub throws: Vec<SpannedTypeRef>,
}

/// A formal parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: Name,
    pub ty: SpannedTypeRef,
    pub varargs: bool,
}

/// A method, constructor or annotation element. `is_constructor` is `true` for
/// constructors and compact constructors; annotation elements carry a
/// `default_value` range and expression.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodData {
    pub name: Name,
    pub name_range: TextRange,
    pub modifiers: Modifiers,
    pub sig: Signature,
    pub is_constructor: bool,
    pub body: Option<BodyId>,
    pub default_value: Option<TextRange>,
    pub default_expr: Option<ExprId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldData {
    pub name: Name,
    pub name_range: TextRange,
    pub modifiers: Modifiers,
    pub ty: SpannedTypeRef,
    pub initializer: Option<TextRange>,
    pub initializer_expr: Option<ExprId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumConstantData {
    pub name: Name,
    pub name_range: TextRange,
    pub arguments: Option<TextRange>,
    pub argument_exprs: Vec<ExprId>,
    pub class_body: Option<TextRange>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticInitData {
    pub body: Option<BodyId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceInitData {
    pub body: Option<BodyId>,
    pub range: TextRange,
}

/// A JPMS `module-info.java` declaration, lowered minimally: directives are
/// kept at the level of names / types (flags such as `transitive` are folded
/// into booleans).
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleData {
    pub name: Name,
    pub name_range: TextRange,
    pub modifiers: Modifiers,
    /// Whether the module was declared `open`.
    pub is_open: bool,
    pub requires: Vec<ModuleRequires>,
    pub exports: Vec<ModuleExports>,
    pub opens: Vec<ModuleExports>,
    pub uses: Vec<SpannedTypeRef>,
    pub provides: Vec<ModuleProvides>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleRequires {
    pub name: Name,
    pub transitive: bool,
    pub statik: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleExports {
    pub package: Name,
    pub to: Vec<Name>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleProvides {
    pub service: SpannedTypeRef,
    pub implementations: Vec<SpannedTypeRef>,
}

/// A declared type parameter of a class or method
/// ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
/// with source-spanned bounds and the annotations on the type-parameter
/// declaration ([JLS §9.7.4]).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: Name,
    pub bounds: Vec<SpannedTypeRef>,
    pub annotations: Vec<NameRef>,
}

/// A record component declaration `T name`
/// ([JLS §8.10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.1)),
/// with the source-spanned component type and the annotations on the
/// component declaration ([JLS §9.7.4]).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordComponent {
    pub name: Name,
    /// The source range of the component's declared name (the identifier).
    pub name_range: TextRange,
    /// The full source range of the component declaration (`T name`, or
    /// `T... name` for a varargs component). The record accessor's outline
    /// selection points here.
    pub range: TextRange,
    pub ty: SpannedTypeRef,
    /// The source range of the component's declared type.
    pub ty_range: TextRange,
    /// Whether the component was declared varargs (`String... names`).
    pub varargs: bool,
    pub annotations: Vec<NameRef>,
}
