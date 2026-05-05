use rustc_hash::FxHashMap;

use base_db::{
    SourceDatabase,
    workspace::{Library, Module, WorkspaceGraph},
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

pub mod bytecode;

#[derive(Clone, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
pub enum TypeRef {
    Primitive(PrimitiveType),
    Reference {
        /// The dot fqn of the reference type
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

#[derive(Clone, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
pub enum TypeBound {
    Upper(TypeRef), // extends
    Lower(TypeRef), // super
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Hash)]
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Hash, Debug)]
pub struct AnnotationSignature {
    pub annotation_type: TypeRef,
    pub arguments: Vec<(SmolStr, AnnotationValue)>,
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
    pub name: SmolStr,
    pub bounds: Vec<TypeRef>,
    pub annotations: Vec<AnnotationSignature>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct RecordComponentData {
    pub name: SmolStr,
    pub component_type: TypeRef,
    pub annotations: Vec<AnnotationSignature>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ClassData {
    pub name: SmolStr,
    /// JVM Access Flags
    pub flags: u16,
    pub super_class: Option<TypeRef>,
    pub interfaces: Vec<TypeRef>,
    pub type_params: Vec<TypeParameter>,

    pub permitted_subclasses: Vec<TypeRef>,
    pub record_components: Vec<RecordComponentData>,

    pub methods: Vec<MethodData>,
    pub fields: Vec<FieldData>,
    pub annotations: Vec<AnnotationSignature>,
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
    pub name: Option<SmolStr>,
    pub param_type: TypeRef,
    pub annotations: Vec<AnnotationSignature>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct MethodData {
    pub flags: u16,
    pub name: SmolStr,
    pub return_type: TypeRef,
    pub type_params: Vec<TypeParameter>,
    pub throws_list: Vec<TypeRef>,
    pub params: Vec<ParamData>,
    pub annotations: Vec<AnnotationSignature>,

    /// The default value of an annotation entry
    pub default_value: Option<AnnotationValue>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct FieldData {
    pub flags: u16,
    pub field_type: TypeRef,
    pub annotations: Vec<AnnotationSignature>,
    pub constant_value: Option<AnnotationValue>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
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

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub enum ClassOrModuleData {
    Class(ClassData),
    Module(ModuleData),
}

impl ClassOrModuleData {
    pub fn fqn(&self) -> SmolStr {
        match self {
            ClassOrModuleData::Class(class_data) => class_data.name.clone(),
            ClassOrModuleData::Module(module_data) => module_data.name.clone(),
        }
    }
}

#[salsa::tracked]
pub struct ClassSignature<'db> {
    pub name: SmolStr,

    pub data: ClassData,
    pub source_file: vfs::FileId,
}

#[salsa::tracked]
pub struct ModuleSignature<'db> {
    pub name: SmolStr,
    pub data: ModuleData,

    pub physical_module: Module,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleRequires {
    pub module_name: SmolStr,
    pub flags: u16,
    pub compiled_version: Option<SmolStr>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleExports {
    pub package_name: SmolStr,
    pub flags: u16,
    pub to_modules: Vec<SmolStr>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleOpens {
    pub package_name: SmolStr,
    pub flags: u16,
    pub to_modules: Vec<SmolStr>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub struct ModuleProvides {
    pub service_interface: TypeRef,
    pub with_implementations: Vec<TypeRef>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorkspaceClassIndex {
    pub fqn_to_file: FxHashMap<SmolStr, vfs::FileId>,
}

#[salsa::input]
pub struct GlobalLibraryCache {
    /// Maps the physical Library ID (from base-db) to its parsed ClassData.
    #[returns(ref)]
    pub parsed_classes: FxHashMap<Library, FxHashMap<SmolStr, ClassData>>,
}

#[salsa::tracked]
pub fn build_workspace_index(
    db: &dyn HirDatabase,
    workspace: WorkspaceGraph,
) -> WorkspaceClassIndex {
    let mut index = FxHashMap::default();

    // TODO: compute class signature

    // for &file_id in workspace.file_to_module(db).keys() {
    //     let class_signature = compute_class_signature(db, file_id);
    //
    //     index.insert(class_signature.name(db).clone(), file_id);
    // }

    WorkspaceClassIndex { fqn_to_file: index }
}

#[salsa::db]
pub trait HirDatabase: SourceDatabase + salsa::Database {}
