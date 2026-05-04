use base_db::SourceDatabase;
use smol_str::SmolStr;

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
    Annotation(AnnotationSignature),
    Array(Vec<AnnotationValue>),
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct AnnotationSignature {
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
    pub annotations: Vec<AnnotationSignature>,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct RecordComponent {
    pub name: SmolStr,
    pub component_type: TypeRef,
    pub annotations: Vec<AnnotationSignature>,
}

#[salsa::tracked]
pub struct ClassSignature<'db> {
    /// JVM Access Flags
    pub flags: u16,
    pub super_class: Option<TypeRef>,
    pub interfaces: Vec<TypeRef>,
    pub type_params: Vec<TypeParameter>,

    pub permitted_subclasses: Vec<TypeRef>,
    pub record_components: Vec<RecordComponent>,

    pub methods: Vec<MethodSignature<'db>>,
    pub fields: Vec<FieldSignature<'db>>,
    pub annotations: Vec<AnnotationSignature>,

    pub enclosing_class: Option<ClassSignature<'db>>,
    pub inner_classes: Vec<ClassSignature<'db>>,

    pub source_file: vfs::FileId,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClassKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

#[salsa::tracked]
pub struct ParamSignature<'db> {
    pub flags: u16,
    pub name: Option<SmolStr>,
    pub param_type: TypeRef,
    pub annotations: Vec<AnnotationSignature>,
}

#[salsa::tracked]
pub struct MethodSignature<'db> {
    pub flags: u16,
    pub return_type: TypeRef,
    pub type_params: Vec<TypeParameter>,
    pub throws_list: Vec<TypeRef>,
    pub params: Vec<ParamSignature<'db>>,
    pub annotations: Vec<AnnotationSignature>,

    /// The default value of an annotation entry
    pub default_value: Option<AnnotationValue>,
}

#[salsa::tracked]
pub struct FieldSignature<'db> {
    pub flags: u16,
    pub field_type: TypeRef,
    pub annotations: Vec<AnnotationSignature>,
    pub constant_value: Option<PrimitiveValue>,
}

#[salsa::tracked]
pub struct Module<'db> {
    pub name: SmolStr,
    pub flags: u16,
    pub version: Option<SmolStr>,

    pub requires: Vec<ModuleRequires<'db>>,
    pub exports: Vec<ModuleExports<'db>>,
    pub opens: Vec<ModuleOpens<'db>>,
    pub uses: Vec<TypeRef>,
    pub provides: Vec<ModuleProvides<'db>>,
}

#[salsa::tracked]
pub struct ModuleRequires<'db> {
    pub module_name: SmolStr,
    pub flags: u16,
    pub compiled_version: Option<SmolStr>,
}

#[salsa::tracked]
pub struct ModuleExports<'db> {
    pub package_name: SmolStr,
    pub flags: u16,
    pub to_modules: Vec<SmolStr>,
}

#[salsa::tracked]
pub struct ModuleOpens<'db> {
    pub package_name: SmolStr,
    pub flags: u16,
    pub to_modules: Vec<SmolStr>,
}

#[salsa::tracked]
pub struct ModuleProvides<'db> {
    pub service_interface: TypeRef,
    pub with_implementations: Vec<TypeRef>,
}

#[salsa::db]
pub trait HirDatabase: SourceDatabase + salsa::Database {}
