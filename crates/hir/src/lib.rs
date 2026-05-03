use rust_asm::constants::*;
use smol_str::SmolStr;
use triomphe::Arc;

use crate::intern::{ClassId, FieldId, MethodId, ModuleId};

pub mod intern;

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub enum TypeRef {
    Primitive(PrimitiveType),
    Reference {
        name: SmolStr,
        generic_args: Vec<TypeRef>,
    },
    Wildcard {
        bound: Option<Box<TypeBound>>,
    },
    TypeVariable(SmolStr),
    Array(Box<TypeRef>),
    Error,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub enum TypeBound {
    Upper(TypeRef), // extends
    Lower(TypeRef), // super
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub enum AnnotationValue {
    String(SmolStr),
    Primitive(PrimitiveValue),
    Class(TypeRef),
    Enum {
        class_type: TypeRef,
        entry_name: SmolStr,
    },
    Annotation(Annotation),
    Array(Vec<AnnotationValue>),
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct Annotation {
    pub annotation_type: TypeRef,
    pub arguments: Vec<(SmolStr, AnnotationValue)>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
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

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
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

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct TypeParameter {
    pub name: SmolStr,
    pub bounds: Vec<TypeRef>,
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct RecordComponent {
    pub name: SmolStr,
    pub component_type: TypeRef,
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClassData<'a> {
    /// JVM Access Flags
    pub flags: u16,
    pub super_class: Option<TypeRef>,
    pub interfaces: Vec<TypeRef>,
    pub type_params: Vec<TypeParameter>,

    pub permitted_subclasses: Vec<TypeRef>,
    pub record_components: Vec<RecordComponent>,

    pub methods: Vec<MethodId<'a>>,
    pub fields: Vec<FieldId<'a>>,
    pub annotations: Vec<Annotation>,

    pub enclosing_class: Option<ClassId<'a>>,
    pub inner_classes: Vec<ClassId<'a>>,

    pub source_file: vfs::FileId,
}

impl<'a> ClassData<'a> {
    pub fn kind(&self) -> ClassKind {
        if (self.flags & ACC_ENUM) != 0 {
            ClassKind::Enum
        } else if (self.flags & ACC_ANNOTATION) != 0 {
            ClassKind::Annotation
        } else if (self.flags & ACC_INTERFACE) != 0 {
            ClassKind::Interface
        } else if self.is_record() {
            ClassKind::Record
        } else {
            ClassKind::Class
        }
    }

    #[inline]
    pub fn is_interface(&self) -> bool {
        (self.flags & ACC_INTERFACE) != 0
    }

    #[inline]
    pub fn is_enum(&self) -> bool {
        (self.flags & ACC_ENUM) != 0
    }

    #[inline]
    pub fn is_record(&self) -> bool {
        !self.record_components.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClassKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParamData {
    pub flags: u16,
    pub name: Option<SmolStr>,
    pub param_type: TypeRef,
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MethodData {
    pub flags: u16,
    pub return_type: TypeRef,
    pub type_params: Vec<TypeParameter>,
    pub throws_list: Vec<TypeRef>,
    pub params: Vec<ParamData>,
    pub annotations: Vec<Annotation>,

    /// The default value of an annotation entry
    pub default_value: Option<AnnotationValue>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FieldData {
    pub flags: u16,
    pub field_type: TypeRef,
    pub annotations: Vec<Annotation>,
    pub constant_value: Option<PrimitiveValue>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModuleData {
    pub name: SmolStr,
    pub flags: u16,
    pub version: Option<SmolStr>,

    pub requires: Vec<ModuleRequires>,
    pub exports: Vec<ModuleExports>,
    pub opens: Vec<ModuleOpens>,
    pub uses: Vec<TypeRef>,
    pub provides: Vec<ModuleProvides>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModuleRequires {
    pub module_name: SmolStr,
    pub flags: u16,
    pub compiled_version: Option<SmolStr>,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct ModuleExports {
    pub package_name: SmolStr,
    pub flags: u16,
    pub to_modules: Vec<SmolStr>,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct ModuleOpens {
    pub package_name: SmolStr,
    pub flags: u16,
    pub to_modules: Vec<SmolStr>,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct ModuleProvides {
    pub service_interface: TypeRef,
    pub with_implementations: Vec<TypeRef>,
}

#[salsa::db]
pub trait HirDatabase: salsa::Database {
    fn file_classes(&self, file_id: vfs::FileId) -> Arc<Vec<ClassId<'_>>>;
    fn file_module(&self, file_id: vfs::FileId) -> Option<Arc<ModuleData>>;

    fn class_data(&self, class_id: ClassId) -> Arc<ClassData<'_>>;
    fn module_data(&self, module_id: ModuleId) -> Arc<ModuleData>;

    fn method_data(&self, method_id: MethodId) -> Arc<MethodData>;
    fn field_data(&self, field_id: FieldId) -> Arc<FieldData>;
}
