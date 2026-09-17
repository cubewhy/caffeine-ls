//! Compact, serializable declaration stubs for JVM classes.
//!
//! All data types are generic over the name representation `N`:
//!
//! * `N = [`Symbol`]` (a [`lasso::Spur`] into a session-wide interner) is the
//!   in-memory representation;
//! * `N = u32` (an index into a per-library string table) is the on-disk
//!   representation used by the persistent cache.
//!
//! The stub IR is the "item tree" of caffeine-ls (see rust-analyzer's
//! `item_tree`): it stores declarations (classes, members and type
//! references) but no bodies and no source locations, keeping the memory
//! footprint small.

use lasso::Spur;
use rust_asm::constants::{ACC_ANNOTATION, ACC_ENUM, ACC_INTERFACE};
use serde::{Deserialize, Serialize};

pub type Symbol = Spur;

#[derive(Clone, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
pub enum TypeRef<N> {
    Primitive(PrimitiveType),
    Reference {
        /// The dot fqn of the reference type
        name: N,
        generic_args: Vec<TypeRef<N>>,
    },
    Wildcard {
        bound: Option<Box<TypeBound<N>>>,
    },
    TypeVariable(N),
    Array(Box<TypeRef<N>>),
    Error,
    /// A Kotlin nullable type `T?` ([KLS
    /// `type-system.html#nullable-types`](https://kotlinlang.org/spec/type-system.html#nullable-types)).
    ///
    /// A Kotlin *source* type only: nullability is not a JVM concept, so the
    /// classfile reader never produces one (a compiled Kotlin type carries its
    /// nullability in `@Metadata`, not in the descriptor). The variants are
    /// declared *after* [`TypeRef::Error`] so the encoding of every existing
    /// variant — and of every persisted library stub — is unchanged.
    Nullable(Box<TypeRef<N>>),
    /// A Kotlin definitely-non-nullable type `T & Any` ([KLS
    /// `type-system.html#intersection-types`](https://kotlinlang.org/spec/type-system.html#intersection-types)).
    /// Kotlin source only, like [`TypeRef::Nullable`].
    DefinitelyNonNull(Box<TypeRef<N>>),
    /// The `void` type ([JLS §4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.3)).
    ///
    /// `void` is a type but *not* a primitive type
    /// ([§4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.2)
    /// lists only `boolean` and the numeric types), so it gets its own kind
    /// rather than a [`PrimitiveType`] variant: no `void` value exists, and
    /// only a method's result type and the class literal `void.class` name it.
    /// It is the classfile's `V` ([JVMS §4.3.2]) and, like the other variants
    /// added after [`TypeRef::Error`], declared last so every persisted
    /// encoding of the variants before it is unchanged.
    Void,
}

impl<N> TypeRef<N> {
    /// The referenced class name, if this is a (possibly generic) reference
    /// type.
    pub fn as_reference_name(&self) -> Option<&N> {
        match self {
            TypeRef::Reference { name, .. } => Some(name),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
pub enum TypeBound<N> {
    Upper(TypeRef<N>), // extends
    Lower(TypeRef<N>), // super
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Hash)]
pub enum AnnotationValue<N> {
    String(N),
    Primitive(PrimitiveValue),
    Class(TypeRef<N>),
    Enum {
        class_type: TypeRef<N>,
        entry_name: N,
    },
    Annotation(AnnotationSig<N>),
    Array(Vec<AnnotationValue<N>>),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Hash, Debug)]
pub struct AnnotationSig<N> {
    pub annotation_type: TypeRef<N>,
    pub arguments: Vec<(N, AnnotationValue<N>)>,
}

/// A primitive constant value ([JVMS §4.4]): the kind the classfile records
/// for a `ConstantValue` attribute or an annotation's `element_value`.
///
/// `void` is absent — it is not a primitive ([JLS §4.2]), no constant of it
/// exists, and an annotation interface element's return type may not be it
/// ([§9.6.1]).
#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Hash)]
pub enum PrimitiveValue {
    Int(i32),
    Long(i64),
    Float(u32),
    Double(u64),
    Boolean(bool),
    Byte(i8),
    Char(u16),
    Short(i16),
}

impl PrimitiveValue {
    #[inline]
    pub fn float(val: f32) -> Self {
        Self::Float(val.to_bits())
    }

    #[inline]
    pub fn double(val: f64) -> Self {
        Self::Double(val.to_bits())
    }

    #[inline]
    pub fn get_float(&self) -> Option<f32> {
        if let Self::Float(bits) = self {
            Some(f32::from_bits(*bits))
        } else {
            None
        }
    }

    #[inline]
    pub fn get_double(&self) -> Option<f64> {
        if let Self::Double(bits) = self {
            Some(f64::from_bits(*bits))
        } else {
            None
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
pub enum PrimitiveType {
    Int,
    Long,
    Float,
    Double,
    Boolean,
    Byte,
    Char,
    Short,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum ClassKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

impl ClassKind {
    /// Classifies a class by its JVM access flags and the presence of a
    /// `Record` attribute (records carry no distinguishing access flag).
    pub fn from_flags(flags: u16, is_record: bool) -> ClassKind {
        if flags & ACC_INTERFACE != 0 {
            if flags & ACC_ANNOTATION != 0 {
                ClassKind::Annotation
            } else {
                ClassKind::Interface
            }
        } else if flags & ACC_ENUM != 0 {
            ClassKind::Enum
        } else if is_record {
            ClassKind::Record
        } else {
            ClassKind::Class
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Hash, Deserialize, Serialize)]
pub struct TypeParameter<N> {
    pub name: N,
    pub bounds: Vec<TypeRef<N>>,
    pub annotations: Vec<AnnotationSig<N>>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct RecordComponentData<N> {
    pub name: N,
    pub component_type: TypeRef<N>,
    /// Whether the component was declared varargs (`String... names`).
    pub varargs: bool,
    pub annotations: Vec<AnnotationSig<N>>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ClassStub<N> {
    /// The fully qualified name (e.g. `java.lang.String`).
    pub fqn: N,
    /// The simple name (or `Outer$Inner` for nested classes).
    pub name: N,
    /// JVM Access Flags
    pub flags: u16,
    /// Whether the class file carries a `Record` attribute.
    pub is_record: bool,
    /// Whether the class file carries the `Deprecated` attribute
    /// ([JVMS §4.7.15](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.15))
    /// — the marker of an element annotated `@Deprecated`
    /// ([JLS §9.6.4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.6)).
    pub deprecated: bool,
    pub super_class: Option<TypeRef<N>>,
    pub interfaces: Vec<TypeRef<N>>,
    pub type_params: Vec<TypeParameter<N>>,

    pub permitted_subclasses: Vec<TypeRef<N>>,
    pub record_components: Vec<RecordComponentData<N>>,

    pub methods: Vec<MethodStub<N>>,
    pub fields: Vec<FieldStub<N>>,
    pub annotations: Vec<AnnotationSig<N>>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ParamData<N> {
    pub flags: u16,
    pub name: Option<N>,
    pub param_type: TypeRef<N>,
    pub annotations: Vec<AnnotationSig<N>>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct MethodStub<N> {
    pub flags: u16,
    pub name: N,
    /// The classfile descriptor of the method
    /// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6)) —
    /// the identity of a member across a platform release, where the
    /// `Signature` attribute ([JVMS §4.7.9.1]) may not be available.
    pub descriptor: N,
    pub return_type: TypeRef<N>,
    pub type_params: Vec<TypeParameter<N>>,
    pub throws_list: Vec<TypeRef<N>>,
    pub params: Vec<ParamData<N>>,
    pub annotations: Vec<AnnotationSig<N>>,

    /// The default value of an annotation entry
    pub default_value: Option<AnnotationValue<N>>,

    /// Whether the class file carries the `Deprecated` attribute
    /// ([JVMS §4.7.15](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.15))
    /// — the marker of a method annotated `@Deprecated`.
    pub deprecated: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct FieldStub<N> {
    /// The name of the field (from the constant pool).
    pub name: N,
    /// The classfile descriptor of the field
    /// ([JVMS §4.5](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.5)).
    pub descriptor: N,
    pub flags: u16,
    pub field_type: TypeRef<N>,
    pub annotations: Vec<AnnotationSig<N>>,
    pub constant_value: Option<AnnotationValue<N>>,
    /// Whether the class file carries the `Deprecated` attribute
    /// ([JVMS §4.7.15](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.15))
    /// — the marker of a field annotated `@Deprecated`.
    pub deprecated: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleStub<N> {
    pub name: N,
    pub flags: u16,
    pub version: Option<N>,

    pub requires: Vec<ModuleRequires<N>>,
    pub exports: Vec<ModuleExports<N>>,
    pub opens: Vec<ModuleOpens<N>>,
    pub uses: Vec<TypeRef<N>>,
    pub provides: Vec<ModuleProvides<N>>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub enum ClassOrModuleStub<N> {
    Class(ClassStub<N>),
    Module(ModuleStub<N>),
}

impl<N: Copy> ClassOrModuleStub<N> {
    pub fn fqn(&self) -> N {
        match self {
            Self::Class(class_data) => class_data.fqn,
            Self::Module(module_data) => module_data.name,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleRequires<N> {
    pub module_name: N,
    pub flags: u16,
    pub compiled_version: Option<N>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleExports<N> {
    pub package_name: N,
    pub flags: u16,
    pub to_modules: Vec<N>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleOpens<N> {
    pub package_name: N,
    pub flags: u16,
    pub to_modules: Vec<N>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleProvides<N> {
    pub service_interface: TypeRef<N>,
    pub with_implementations: Vec<TypeRef<N>>,
}
