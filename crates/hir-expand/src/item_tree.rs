//! The lowered per-file declaration model ("item tree", after rust-analyzer).
//!
//! Lowering turns a language's CST into this flat, arena-based IR: every
//! top-level type, member, field, enum constant, initializer and module
//! directive gets a stable [`ItemId`]. Method bodies are dropped; only their
//! source range is kept so IDE features can map items back to source.

use rowan::TextRange;
use syntax::stub::{RecordComponentData, TypeParameter, TypeRef};

use crate::{
    arena::{Arena, ArenaId},
    modifiers::Modifiers,
    name::Name,
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
    pub imports: Vec<ImportItem>,
    pub top: Vec<ItemId>,
    pub items: Arena<ItemData>,
}

impl Default for ItemTree {
    fn default() -> Self {
        Self {
            language: LanguageKind::Unknown,
            package: None,
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
    pub modifiers: Modifiers,
    pub super_class: Option<TypeRef<Name>>,
    pub interfaces: Vec<TypeRef<Name>>,
    pub type_params: Vec<TypeParameter<Name>>,
    pub body: Vec<ItemId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumData {
    pub name: Name,
    pub modifiers: Modifiers,
    pub interfaces: Vec<TypeRef<Name>>,
    pub body: Vec<ItemId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordData {
    pub name: Name,
    pub modifiers: Modifiers,
    pub components: Vec<RecordComponentData<Name>>,
    pub interfaces: Vec<TypeRef<Name>>,
    pub type_params: Vec<TypeParameter<Name>>,
    pub body: Vec<ItemId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationData {
    pub name: Name,
    pub modifiers: Modifiers,
    pub body: Vec<ItemId>,
    pub range: TextRange,
}

/// A method signature: type parameters, parameters, return type and thrown
/// exceptions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Signature {
    pub type_params: Vec<TypeParameter<Name>>,
    pub params: Vec<Param>,
    pub ret: Option<TypeRef<Name>>,
    pub throws: Vec<TypeRef<Name>>,
}

/// A formal parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: Name,
    pub ty: TypeRef<Name>,
    pub varargs: bool,
}

/// A method, constructor or annotation element. `is_constructor` is `true` for
/// constructors and compact constructors; annotation elements carry a
/// `default_value` range.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodData {
    pub name: Name,
    pub modifiers: Modifiers,
    pub sig: Signature,
    pub is_constructor: bool,
    pub body: Option<TextRange>,
    pub default_value: Option<TextRange>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldData {
    pub name: Name,
    pub modifiers: Modifiers,
    pub ty: TypeRef<Name>,
    pub initializer: Option<TextRange>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumConstantData {
    pub name: Name,
    pub arguments: Option<TextRange>,
    pub class_body: Option<TextRange>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticInitData {
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceInitData {
    pub range: TextRange,
}

/// A JPMS `module-info.java` declaration, lowered minimally: directives are
/// kept at the level of names / types (flags such as `transitive` are folded
/// into booleans).
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleData {
    pub name: Name,
    pub modifiers: Modifiers,
    /// Whether the module was declared `open`.
    pub is_open: bool,
    pub requires: Vec<ModuleRequires>,
    pub exports: Vec<ModuleExports>,
    pub opens: Vec<ModuleExports>,
    pub uses: Vec<TypeRef<Name>>,
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
    pub service: TypeRef<Name>,
    pub implementations: Vec<TypeRef<Name>>,
}
