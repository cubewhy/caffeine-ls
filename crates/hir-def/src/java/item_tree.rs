//! The lowered per-file declaration model ("item tree", after rust-analyzer).
//!
//! Lowering turns a language's CST into this flat, arena-based IR: every
//! top-level type, member, field, enum constant, initializer and module
//! directive gets a stable [`ItemId`]. The *bodies* of methods, initializers,
//! field initializers, enum constant arguments and annotation element defaults
//! are lowered into the per-file [`hir_expand::body::BodyTree`], which lives
//! *beside* the item tree ([`LoweredFile`]) rather than inside it: keeping the
//! body content out of the memoized item tree lets salsa backdate the
//! signature-level queries across edits that only touch a method body.
//!
//! This is the *Java* declaration layer: the item kinds mirror the Java
//! grammar (classes, interfaces, enums, records, annotation types, modules,
//! methods, fields), and every declaration's source modifiers are carried as
//! [`crate::java::modifiers::JavaModifiers`]. Language-specific method
//! attributes are wrapped in [`MethodExtra::Java`], leaving the JVM-level
//! signature independent of the source language. Kotlin will lower its own
//! item tree against the same JVM substrate.

use triomphe::Arc;

use hir_expand::{
    arena::Arena,
    body::{BodyId, BodyTree, ExprId},
    name::Name,
    span::{AnnotationRef, SpannedTypeRef},
};
use rowan::TextRange;

use crate::java::modifiers::JavaModifiers;

pub use base_db::LanguageKind;
pub use hir_expand::ids::ItemId;

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
    /// The source range of every package declaration's name, in source order
    /// ([JLS §7.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.1):
    /// a compilation unit declares at most one package). More than one entry
    /// is the duplicate-package error the declaration diagnostics report.
    pub package_decl_ranges: Vec<TextRange>,
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
            package_decl_ranges: Vec::new(),
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

/// The full per-file lowering: the declaration [`ItemTree`] plus the body IR
/// ([`hir_expand::body::BodyTree`]), lowered together in one pass so the body
/// ids stored in the item data line up with the body arenas. Computed by a
/// single salsa query (`hir_def::db::lower_source_query`) and read through its
/// [`file_item_tree`](crate::db::file_item_tree) /
/// [`file_body_tree`](crate::db::file_body_tree) accessors. Because the item
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

/// A class or interface declaration (they share the same layout).
#[derive(Debug, Clone, PartialEq)]
pub struct ClassData {
    pub name: Name,
    pub name_range: TextRange,
    pub modifiers: JavaModifiers,
    /// The annotation references of the declaration, in source order
    /// ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7)),
    /// decoupled from the modifier flags.
    pub annotations: Vec<AnnotationRef>,
    pub super_class: Option<SpannedTypeRef>,
    pub interfaces: Vec<SpannedTypeRef>,
    /// The permitted direct subclasses of a `sealed` class or interface
    /// ([§8.1.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.1.2)),
    /// from its `permits` clause; empty when the declaration has none (the
    /// permitted set is then the same-module direct subclasses).
    pub permits: Vec<SpannedTypeRef>,
    pub type_params: Vec<TypeParam>,
    pub body: Vec<ItemId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumData {
    pub name: Name,
    pub name_range: TextRange,
    pub modifiers: JavaModifiers,
    pub annotations: Vec<AnnotationRef>,
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
    pub modifiers: JavaModifiers,
    pub annotations: Vec<AnnotationRef>,
    pub components: Vec<RecordComponent>,
    pub interfaces: Vec<SpannedTypeRef>,
    /// The permitted direct subclasses of a `sealed` record
    /// ([§8.1.1.2]), from its `permits` clause.
    pub permits: Vec<SpannedTypeRef>,
    pub type_params: Vec<TypeParam>,
    pub body: Vec<ItemId>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationData {
    pub name: Name,
    pub name_range: TextRange,
    pub modifiers: JavaModifiers,
    pub annotations: Vec<AnnotationRef>,
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

/// The Java-specific attributes of a [`MethodData`]: constructor-ness, the
/// lowered body and the annotation element default.
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
    /// The source range of an annotation element's default value
    /// ([JLS §9.6.1]).
    pub default_value: Option<TextRange>,
    /// The lowered default-value expression of an annotation element.
    pub default_expr: Option<ExprId>,
}

/// A method, constructor or annotation element.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodData {
    pub name: Name,
    pub name_range: TextRange,
    pub modifiers: JavaModifiers,
    pub annotations: Vec<AnnotationRef>,
    pub sig: Signature,
    /// The language-specific attributes of the declaration (Java constructor
    /// / body / annotation default, Kotlin attributes later).
    pub extra: MethodExtra,
    pub range: TextRange,
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

    /// The source range of an annotation element's default value.
    pub fn default_value(&self) -> Option<TextRange> {
        match &self.extra {
            MethodExtra::Java(java) => java.default_value,
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
    pub name_range: TextRange,
    pub modifiers: JavaModifiers,
    pub annotations: Vec<AnnotationRef>,
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
    pub modifiers: JavaModifiers,
    pub annotations: Vec<AnnotationRef>,
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
    /// The source range of the required module name.
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleExports {
    pub package: Name,
    pub to: Vec<Name>,
    /// The source range of the exported/opened package name.
    pub package_range: TextRange,
    /// The source ranges of the `to` module names, parallel to `to`.
    pub to_ranges: Vec<TextRange>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleProvides {
    pub service: SpannedTypeRef,
    pub implementations: Vec<SpannedTypeRef>,
}

/// A declared type parameter of a class or method
/// ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
/// with source-spanned bounds and the annotations on the type-parameter
/// declaration ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4)).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: Name,
    pub bounds: Vec<SpannedTypeRef>,
    pub annotations: Vec<AnnotationRef>,
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
    pub annotations: Vec<AnnotationRef>,
}
