use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

pub type Symbol = SmolStr;

#[derive(Clone, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
pub enum TypeRef {
    Primitive(PrimitiveType),
    Reference {
        /// The dot fqn of the reference type
        name: Symbol,
        generic_args: Vec<TypeRef>,
    },
    Wildcard {
        bound: Option<Box<TypeBound>>,
    },
    TypeVariable(Symbol),
    Array(Box<TypeRef>),
    Error,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
pub enum TypeBound {
    Upper(TypeRef), // extends
    Lower(TypeRef), // super
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Hash)]
pub enum AnnotationValue {
    String(Symbol),
    Primitive(PrimitiveValue),
    Class(TypeRef),
    Enum {
        class_type: TypeRef,
        entry_name: Symbol,
    },
    Annotation(AnnotationSig),
    Array(Vec<AnnotationValue>),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Hash, Debug)]
pub struct AnnotationSig {
    pub annotation_type: TypeRef,
    pub arguments: Vec<(Symbol, AnnotationValue)>,
}

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
    Void,
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
    Void,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash, Deserialize, Serialize)]
pub struct TypeParameter {
    pub name: Symbol,
    pub bounds: Vec<TypeRef>,
    pub annotations: Vec<AnnotationSig>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct RecordComponentData {
    pub name: Symbol,
    pub component_type: TypeRef,
    pub annotations: Vec<AnnotationSig>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ClassStub {
    // TODO: add fqn and inner_class_of
    pub name: Symbol,
    /// JVM Access Flags
    pub flags: u16,
    pub super_class: Option<TypeRef>,
    pub interfaces: Vec<TypeRef>,
    pub type_params: Vec<TypeParameter>,

    pub permitted_subclasses: Vec<TypeRef>,
    pub record_components: Vec<RecordComponentData>,

    pub methods: Vec<MethodStub>,
    pub fields: Vec<FieldStub>,
    pub annotations: Vec<AnnotationSig>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub enum ClassKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ParamData {
    pub flags: u16,
    pub name: Option<Symbol>,
    pub param_type: TypeRef,
    pub annotations: Vec<AnnotationSig>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct MethodStub {
    pub flags: u16,
    pub name: Symbol,
    pub return_type: TypeRef,
    pub type_params: Vec<TypeParameter>,
    pub throws_list: Vec<TypeRef>,
    pub params: Vec<ParamData>,
    pub annotations: Vec<AnnotationSig>,

    /// The default value of an annotation entry
    pub default_value: Option<AnnotationValue>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct FieldStub {
    pub flags: u16,
    pub field_type: TypeRef,
    pub annotations: Vec<AnnotationSig>,
    pub constant_value: Option<AnnotationValue>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleStub {
    pub name: Symbol,
    pub flags: u16,
    pub version: Option<Symbol>,

    pub requires: Vec<ModuleRequires>,
    pub exports: Vec<ModuleExports>,
    pub opens: Vec<ModuleOpens>,
    pub uses: Vec<TypeRef>,
    pub provides: Vec<ModuleProvides>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub enum ClassOrModuleStub {
    Class(ClassStub),
    Module(ModuleStub),
}

impl ClassOrModuleStub {
    pub fn fqn(&self) -> Symbol {
        match self {
            Self::Class(class_data) => class_data.name.clone(),
            Self::Module(module_data) => module_data.name.clone(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleRequires {
    pub module_name: Symbol,
    pub flags: u16,
    pub compiled_version: Option<Symbol>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleExports {
    pub package_name: Symbol,
    pub flags: u16,
    pub to_modules: Vec<Symbol>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleOpens {
    pub package_name: Symbol,
    pub flags: u16,
    pub to_modules: Vec<Symbol>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleProvides {
    pub service_interface: TypeRef,
    pub with_implementations: Vec<TypeRef>,
}
