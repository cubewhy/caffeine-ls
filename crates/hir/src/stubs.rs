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
//! footprint small. Member-level data is only kept for classes that are
//! actually requested (see [`crate::index::LibraryIndex`]).

use lasso::{Spur, ThreadedRodeo};
use rust_asm::constants::{ACC_ANNOTATION, ACC_ENUM, ACC_INTERFACE};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::hash::Hash;

pub type Symbol = Spur;

pub use syntax::stub::{PrimitiveType, PrimitiveValue};

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum ClassKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

impl ClassKind {
    /// Classifies a class by its JVM access flags.
    pub fn from_flags(flags: u16) -> ClassKind {
        if flags & ACC_INTERFACE != 0 {
            if flags & ACC_ANNOTATION != 0 {
                ClassKind::Annotation
            } else {
                ClassKind::Interface
            }
        } else if flags & ACC_ENUM != 0 {
            ClassKind::Enum
        } else {
            ClassKind::Class
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum TypeRef<N> {
    Primitive(PrimitiveType),
    Reference {
        /// The dot FQN of the reference type.
        name: N,
        generic_args: Vec<TypeRef<N>>,
    },
    Wildcard {
        bound: Option<Box<TypeBound<N>>>,
    },
    TypeVariable(N),
    Array(Box<TypeRef<N>>),
    Error,
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

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum TypeBound<N> {
    /// `extends`
    Upper(TypeRef<N>),
    /// `super`
    Lower(TypeRef<N>),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct TypeParameter<N> {
    pub name: N,
    pub bounds: Vec<TypeRef<N>>,
    pub annotations: Vec<AnnotationSig<N>>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct AnnotationSig<N> {
    pub annotation_type: TypeRef<N>,
    pub arguments: Vec<(N, AnnotationValue<N>)>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
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

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ParamData<N> {
    pub flags: u16,
    pub name: Option<N>,
    pub param_type: TypeRef<N>,
    pub annotations: Vec<AnnotationSig<N>>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct MethodData<N> {
    pub flags: u16,
    pub name: N,
    pub return_type: TypeRef<N>,
    pub type_params: Vec<TypeParameter<N>>,
    pub throws_list: Vec<TypeRef<N>>,
    pub params: Vec<ParamData<N>>,
    pub annotations: Vec<AnnotationSig<N>>,

    /// The default value of an annotation entry.
    pub default_value: Option<AnnotationValue<N>>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct FieldData<N> {
    pub flags: u16,
    pub field_type: TypeRef<N>,
    pub annotations: Vec<AnnotationSig<N>>,
    pub constant_value: Option<AnnotationValue<N>>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct RecordComponentData<N> {
    pub name: N,
    pub component_type: TypeRef<N>,
    pub annotations: Vec<AnnotationSig<N>>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ClassData<N> {
    /// The fully qualified name (e.g. `java.lang.String`).
    pub fqn: N,
    /// The simple name (or `Outer$Inner` for nested classes).
    pub name: N,
    /// JVM access flags.
    pub flags: u16,
    pub kind: ClassKind,
    pub super_class: Option<TypeRef<N>>,
    pub interfaces: Vec<TypeRef<N>>,
    pub type_params: Vec<TypeParameter<N>>,
    pub methods: Vec<MethodData<N>>,
    pub fields: Vec<FieldData<N>>,
    pub permitted_subclasses: Vec<TypeRef<N>>,
    pub record_components: Vec<RecordComponentData<N>>,
    pub annotations: Vec<AnnotationSig<N>>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ModuleRequires<N> {
    pub module_name: N,
    pub flags: u16,
    pub compiled_version: Option<N>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ModuleExports<N> {
    pub package_name: N,
    pub flags: u16,
    pub to_modules: Vec<N>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ModuleOpens<N> {
    pub package_name: N,
    pub flags: u16,
    pub to_modules: Vec<N>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ModuleProvides<N> {
    pub service_interface: TypeRef<N>,
    pub with_implementations: Vec<TypeRef<N>>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ModuleData<N> {
    pub name: N,
    pub flags: u16,
    pub version: Option<N>,
    pub requires: Vec<ModuleRequires<N>>,
    pub exports: Vec<ModuleExports<N>>,
    pub opens: Vec<ModuleOpens<N>>,
    pub uses: Vec<TypeRef<N>>,
    pub provides: Vec<ModuleProvides<N>>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum ClassOrModule<N> {
    Class(ClassData<N>),
    Module(ModuleData<N>),
}

/// In-memory instantiations.
pub type ClassRecord = ClassData<Symbol>;
pub type ModuleRecord = ModuleData<Symbol>;
pub type ClassOrModuleRecord = ClassOrModule<Symbol>;

/// On-disk instantiations (string-table indices).
pub type DiskClassRecord = ClassData<u32>;
pub type DiskModuleRecord = ModuleData<u32>;
pub type DiskClassOrModuleRecord = ClassOrModule<u32>;

/// Builds the per-library string table while converting the
/// [`Symbol`]-based stubs produced by `syntax::ClassParser` into the
/// on-disk `u32`-based representation.
pub struct StubStringTable<'a> {
    interner: &'a ThreadedRodeo,
    strings: Vec<String>,
    str_to_idx: FxHashMap<String, u32>,
    symbol_to_idx: FxHashMap<Symbol, u32>,
}

impl<'a> StubStringTable<'a> {
    pub fn new(interner: &'a ThreadedRodeo) -> Self {
        Self {
            interner,
            strings: Vec::new(),
            str_to_idx: FxHashMap::default(),
            symbol_to_idx: FxHashMap::default(),
        }
    }

    pub fn into_strings(self) -> Vec<String> {
        self.strings
    }

    /// Interns a raw string into the table, returning its index.
    pub fn intern_str(&mut self, s: &str) -> u32 {
        if let Some(&idx) = self.str_to_idx.get(s) {
            return idx;
        }
        let idx = self.strings.len() as u32;
        self.strings.push(s.to_owned());
        self.str_to_idx.insert(s.to_owned(), idx);
        idx
    }

    /// Maps an already-interned symbol to a string-table index.
    pub fn symbol(&mut self, s: Symbol) -> u32 {
        if let Some(&idx) = self.symbol_to_idx.get(&s) {
            return idx;
        }
        let resolved = self.interner.resolve(&s);
        let idx = self.intern_str(resolved);
        self.symbol_to_idx.insert(s, idx);
        idx
    }

    pub fn type_ref(&mut self, t: &AstTypeRef) -> TypeRef<u32> {
        match t {
            AstTypeRef::Primitive(p) => TypeRef::Primitive(*p),
            AstTypeRef::Reference { name, generic_args } => TypeRef::Reference {
                name: self.symbol(*name),
                generic_args: generic_args.iter().map(|arg| self.type_ref(arg)).collect(),
            },
            AstTypeRef::Wildcard { bound } => TypeRef::Wildcard {
                bound: bound.as_ref().map(|b| Box::new(self.type_bound(b))),
            },
            AstTypeRef::TypeVariable(v) => TypeRef::TypeVariable(self.symbol(*v)),
            AstTypeRef::Array(inner) => TypeRef::Array(Box::new(self.type_ref(inner))),
            AstTypeRef::Error => TypeRef::Error,
        }
    }

    pub fn type_bound(&mut self, b: &AstTypeBound) -> TypeBound<u32> {
        match b {
            AstTypeBound::Upper(t) => TypeBound::Upper(self.type_ref(t)),
            AstTypeBound::Lower(t) => TypeBound::Lower(self.type_ref(t)),
        }
    }

    pub fn type_parameter(&mut self, p: &AstTypeParameter) -> TypeParameter<u32> {
        TypeParameter {
            name: self.symbol(p.name),
            bounds: p.bounds.iter().map(|b| self.type_ref(b)).collect(),
            annotations: p.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    pub fn annotation(&mut self, a: &AstAnnotationSig) -> AnnotationSig<u32> {
        AnnotationSig {
            annotation_type: self.type_ref(&a.annotation_type),
            arguments: a
                .arguments
                .iter()
                .map(|(name, value)| (self.symbol(*name), self.annotation_value(value)))
                .collect(),
        }
    }

    pub fn annotation_value(&mut self, v: &AstAnnotationValue) -> AnnotationValue<u32> {
        match v {
            AstAnnotationValue::String(s) => AnnotationValue::String(self.symbol(*s)),
            AstAnnotationValue::Primitive(p) => AnnotationValue::Primitive(*p),
            AstAnnotationValue::Class(t) => AnnotationValue::Class(self.type_ref(t)),
            AstAnnotationValue::Enum {
                class_type,
                entry_name,
            } => AnnotationValue::Enum {
                class_type: self.type_ref(class_type),
                entry_name: self.symbol(*entry_name),
            },
            AstAnnotationValue::Annotation(a) => AnnotationValue::Annotation(self.annotation(a)),
            AstAnnotationValue::Array(values) => AnnotationValue::Array(
                values
                    .iter()
                    .map(|value| self.annotation_value(value))
                    .collect(),
            ),
        }
    }

    pub fn param(&mut self, p: &AstParamData) -> ParamData<u32> {
        ParamData {
            flags: p.flags,
            name: p.name.map(|n| self.symbol(n)),
            param_type: self.type_ref(&p.param_type),
            annotations: p.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    pub fn method(&mut self, m: &AstMethodStub) -> MethodData<u32> {
        MethodData {
            flags: m.flags,
            name: self.symbol(m.name),
            return_type: self.type_ref(&m.return_type),
            type_params: m
                .type_params
                .iter()
                .map(|tp| self.type_parameter(tp))
                .collect(),
            throws_list: m.throws_list.iter().map(|t| self.type_ref(t)).collect(),
            params: m.params.iter().map(|p| self.param(p)).collect(),
            annotations: m.annotations.iter().map(|a| self.annotation(a)).collect(),
            default_value: m.default_value.as_ref().map(|v| self.annotation_value(v)),
        }
    }

    pub fn field(&mut self, f: &AstFieldStub) -> FieldData<u32> {
        FieldData {
            flags: f.flags,
            field_type: self.type_ref(&f.field_type),
            annotations: f.annotations.iter().map(|a| self.annotation(a)).collect(),
            constant_value: f.constant_value.as_ref().map(|v| self.annotation_value(v)),
        }
    }

    pub fn record_component(&mut self, r: &AstRecordComponentData) -> RecordComponentData<u32> {
        RecordComponentData {
            name: self.symbol(r.name),
            component_type: self.type_ref(&r.component_type),
            annotations: r.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    /// Converts a class stub, attaching its fully qualified name.
    pub fn class(&mut self, c: &AstClassStub, fqn: u32) -> ClassData<u32> {
        ClassData {
            fqn,
            name: self.symbol(c.name),
            flags: c.flags,
            kind: ClassKind::from_flags(c.flags),
            super_class: c.super_class.as_ref().map(|t| self.type_ref(t)),
            interfaces: c.interfaces.iter().map(|t| self.type_ref(t)).collect(),
            type_params: c
                .type_params
                .iter()
                .map(|tp| self.type_parameter(tp))
                .collect(),
            methods: c.methods.iter().map(|m| self.method(m)).collect(),
            fields: c.fields.iter().map(|f| self.field(f)).collect(),
            permitted_subclasses: c
                .permitted_subclasses
                .iter()
                .map(|t| self.type_ref(t))
                .collect(),
            record_components: c
                .record_components
                .iter()
                .map(|rc| self.record_component(rc))
                .collect(),
            annotations: c.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    pub fn module(&mut self, m: &AstModuleStub) -> ModuleData<u32> {
        ModuleData {
            name: self.symbol(m.name),
            flags: m.flags,
            version: m.version.map(|v| self.symbol(v)),
            requires: m
                .requires
                .iter()
                .map(|r| ModuleRequires {
                    module_name: self.symbol(r.module_name),
                    flags: r.flags,
                    compiled_version: r.compiled_version.map(|v| self.symbol(v)),
                })
                .collect(),
            exports: m
                .exports
                .iter()
                .map(|e| ModuleExports {
                    package_name: self.symbol(e.package_name),
                    flags: e.flags,
                    to_modules: e.to_modules.iter().map(|&n| self.symbol(n)).collect(),
                })
                .collect(),
            opens: m
                .opens
                .iter()
                .map(|o| ModuleOpens {
                    package_name: self.symbol(o.package_name),
                    flags: o.flags,
                    to_modules: o.to_modules.iter().map(|&n| self.symbol(n)).collect(),
                })
                .collect(),
            uses: m.uses.iter().map(|t| self.type_ref(t)).collect(),
            provides: m
                .provides
                .iter()
                .map(|p| ModuleProvides {
                    service_interface: self.type_ref(&p.service_interface),
                    with_implementations: p
                        .with_implementations
                        .iter()
                        .map(|t| self.type_ref(t))
                        .collect(),
                })
                .collect(),
        }
    }
}

/// Resolves `u32` string-table indices back into [`Symbol`]s using a
/// library's string table and the session interner.
pub struct DiskResolver<'a> {
    strings: &'a [String],
    interner: &'a ThreadedRodeo,
}

impl<'a> DiskResolver<'a> {
    pub fn new(strings: &'a [String], interner: &'a ThreadedRodeo) -> Self {
        Self { strings, interner }
    }

    pub fn symbol(&self, idx: u32) -> Symbol {
        self.interner.get_or_intern(&self.strings[idx as usize])
    }

    pub fn type_ref(&self, t: &TypeRef<u32>) -> TypeRef<Symbol> {
        match t {
            TypeRef::Primitive(p) => TypeRef::Primitive(*p),
            TypeRef::Reference { name, generic_args } => TypeRef::Reference {
                name: self.symbol(*name),
                generic_args: generic_args.iter().map(|arg| self.type_ref(arg)).collect(),
            },
            TypeRef::Wildcard { bound } => TypeRef::Wildcard {
                bound: bound.as_ref().map(|b| Box::new(self.type_bound(b))),
            },
            TypeRef::TypeVariable(v) => TypeRef::TypeVariable(self.symbol(*v)),
            TypeRef::Array(inner) => TypeRef::Array(Box::new(self.type_ref(inner))),
            TypeRef::Error => TypeRef::Error,
        }
    }

    pub fn type_bound(&self, b: &TypeBound<u32>) -> TypeBound<Symbol> {
        match b {
            TypeBound::Upper(t) => TypeBound::Upper(self.type_ref(t)),
            TypeBound::Lower(t) => TypeBound::Lower(self.type_ref(t)),
        }
    }

    pub fn type_parameter(&self, p: &TypeParameter<u32>) -> TypeParameter<Symbol> {
        TypeParameter {
            name: self.symbol(p.name),
            bounds: p.bounds.iter().map(|b| self.type_ref(b)).collect(),
            annotations: p.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    pub fn annotation(&self, a: &AnnotationSig<u32>) -> AnnotationSig<Symbol> {
        AnnotationSig {
            annotation_type: self.type_ref(&a.annotation_type),
            arguments: a
                .arguments
                .iter()
                .map(|(name, value)| (self.symbol(*name), self.annotation_value(value)))
                .collect(),
        }
    }

    pub fn annotation_value(&self, v: &AnnotationValue<u32>) -> AnnotationValue<Symbol> {
        match v {
            AnnotationValue::String(s) => AnnotationValue::String(self.symbol(*s)),
            AnnotationValue::Primitive(p) => AnnotationValue::Primitive(*p),
            AnnotationValue::Class(t) => AnnotationValue::Class(self.type_ref(t)),
            AnnotationValue::Enum {
                class_type,
                entry_name,
            } => AnnotationValue::Enum {
                class_type: self.type_ref(class_type),
                entry_name: self.symbol(*entry_name),
            },
            AnnotationValue::Annotation(a) => AnnotationValue::Annotation(self.annotation(a)),
            AnnotationValue::Array(values) => AnnotationValue::Array(
                values
                    .iter()
                    .map(|value| self.annotation_value(value))
                    .collect(),
            ),
        }
    }

    pub fn param(&self, p: &ParamData<u32>) -> ParamData<Symbol> {
        ParamData {
            flags: p.flags,
            name: p.name.map(|n| self.symbol(n)),
            param_type: self.type_ref(&p.param_type),
            annotations: p.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    pub fn method(&self, m: &MethodData<u32>) -> MethodData<Symbol> {
        MethodData {
            flags: m.flags,
            name: self.symbol(m.name),
            return_type: self.type_ref(&m.return_type),
            type_params: m
                .type_params
                .iter()
                .map(|tp| self.type_parameter(tp))
                .collect(),
            throws_list: m.throws_list.iter().map(|t| self.type_ref(t)).collect(),
            params: m.params.iter().map(|p| self.param(p)).collect(),
            annotations: m.annotations.iter().map(|a| self.annotation(a)).collect(),
            default_value: m.default_value.as_ref().map(|v| self.annotation_value(v)),
        }
    }

    pub fn field(&self, f: &FieldData<u32>) -> FieldData<Symbol> {
        FieldData {
            flags: f.flags,
            field_type: self.type_ref(&f.field_type),
            annotations: f.annotations.iter().map(|a| self.annotation(a)).collect(),
            constant_value: f.constant_value.as_ref().map(|v| self.annotation_value(v)),
        }
    }

    pub fn record_component(&self, r: &RecordComponentData<u32>) -> RecordComponentData<Symbol> {
        RecordComponentData {
            name: self.symbol(r.name),
            component_type: self.type_ref(&r.component_type),
            annotations: r.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    pub fn class(&self, c: &ClassData<u32>) -> ClassData<Symbol> {
        ClassData {
            fqn: self.symbol(c.fqn),
            name: self.symbol(c.name),
            flags: c.flags,
            kind: c.kind,
            super_class: c.super_class.as_ref().map(|t| self.type_ref(t)),
            interfaces: c.interfaces.iter().map(|t| self.type_ref(t)).collect(),
            type_params: c
                .type_params
                .iter()
                .map(|tp| self.type_parameter(tp))
                .collect(),
            methods: c.methods.iter().map(|m| self.method(m)).collect(),
            fields: c.fields.iter().map(|f| self.field(f)).collect(),
            permitted_subclasses: c
                .permitted_subclasses
                .iter()
                .map(|t| self.type_ref(t))
                .collect(),
            record_components: c
                .record_components
                .iter()
                .map(|rc| self.record_component(rc))
                .collect(),
            annotations: c.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    pub fn module(&self, m: &ModuleData<u32>) -> ModuleData<Symbol> {
        ModuleData {
            name: self.symbol(m.name),
            flags: m.flags,
            version: m.version.map(|v| self.symbol(v)),
            requires: m
                .requires
                .iter()
                .map(|r| ModuleRequires {
                    module_name: self.symbol(r.module_name),
                    flags: r.flags,
                    compiled_version: r.compiled_version.map(|v| self.symbol(v)),
                })
                .collect(),
            exports: m
                .exports
                .iter()
                .map(|e| ModuleExports {
                    package_name: self.symbol(e.package_name),
                    flags: e.flags,
                    to_modules: e.to_modules.iter().map(|&n| self.symbol(n)).collect(),
                })
                .collect(),
            opens: m
                .opens
                .iter()
                .map(|o| ModuleOpens {
                    package_name: self.symbol(o.package_name),
                    flags: o.flags,
                    to_modules: o.to_modules.iter().map(|&n| self.symbol(n)).collect(),
                })
                .collect(),
            uses: m.uses.iter().map(|t| self.type_ref(t)).collect(),
            provides: m
                .provides
                .iter()
                .map(|p| ModuleProvides {
                    service_interface: self.type_ref(&p.service_interface),
                    with_implementations: p
                        .with_implementations
                        .iter()
                        .map(|t| self.type_ref(t))
                        .collect(),
                })
                .collect(),
        }
    }

    pub fn class_or_module(&self, c: &ClassOrModule<u32>) -> ClassOrModule<Symbol> {
        match c {
            ClassOrModule::Class(class) => ClassOrModule::Class(self.class(class)),
            ClassOrModule::Module(module) => ClassOrModule::Module(self.module(module)),
        }
    }
}

// Aliases for the `syntax::stub` types produced by `syntax::ClassParser`.
use syntax::stub::{
    AnnotationSig as AstAnnotationSig, AnnotationValue as AstAnnotationValue,
    ClassStub as AstClassStub, FieldStub as AstFieldStub, MethodStub as AstMethodStub,
    ModuleStub as AstModuleStub, ParamData as AstParamData,
    RecordComponentData as AstRecordComponentData, TypeBound as AstTypeBound,
    TypeParameter as AstTypeParameter, TypeRef as AstTypeRef,
};

#[cfg(test)]
mod tests {
    use lasso::ThreadedRodeo;
    use syntax::stub::{
        AnnotationSig as AstAnnotationSig, AnnotationValue as AstAnnotationValue,
        ClassStub as AstClassStub, FieldStub as AstFieldStub, MethodStub as AstMethodStub,
        PrimitiveType as AstPrimitiveType, PrimitiveValue as AstPrimitiveValue,
        TypeParameter as AstTypeParameter, TypeRef as AstTypeRef,
    };

    use super::*;

    fn class_stub(interner: &ThreadedRodeo) -> AstClassStub {
        AstClassStub {
            name: interner.get_or_intern("String"),
            flags: 0x0021, // ACC_PUBLIC | ACC_SUPER
            super_class: Some(AstTypeRef::Reference {
                name: interner.get_or_intern("java.lang.Object"),
                generic_args: Vec::new(),
            }),
            interfaces: vec![AstTypeRef::Reference {
                name: interner.get_or_intern("java.lang.CharSequence"),
                generic_args: Vec::new(),
            }],
            type_params: vec![AstTypeParameter {
                name: interner.get_or_intern("T"),
                bounds: Vec::new(),
                annotations: Vec::new(),
            }],
            permitted_subclasses: Vec::new(),
            record_components: Vec::new(),
            methods: vec![AstMethodStub {
                flags: 0x0001, // ACC_PUBLIC
                name: interner.get_or_intern("length"),
                return_type: AstTypeRef::Primitive(AstPrimitiveType::Int),
                type_params: Vec::new(),
                throws_list: Vec::new(),
                params: Vec::new(),
                annotations: Vec::new(),
                default_value: None,
            }],
            fields: vec![AstFieldStub {
                flags: 0x0001,
                field_type: AstTypeRef::Reference {
                    name: interner.get_or_intern("int"),
                    generic_args: Vec::new(),
                },
                annotations: Vec::new(),
                constant_value: Some(AstAnnotationValue::Primitive(AstPrimitiveValue::Int(42))),
            }],
            annotations: vec![AstAnnotationSig {
                annotation_type: AstTypeRef::Reference {
                    name: interner.get_or_intern("java.lang.Deprecated"),
                    generic_args: Vec::new(),
                },
                arguments: Vec::new(),
            }],
        }
    }

    #[test]
    fn string_table_round_trip() {
        let interner = ThreadedRodeo::default();
        let stub = class_stub(&interner);
        let fqn = interner.get_or_intern("java.lang.String");

        let mut table = StubStringTable::new(&interner);
        let fqn_idx = table.symbol(fqn);
        let disk = table.class(&stub, fqn_idx);

        // All symbols in the stub must have been added to the string table.
        let strings = table.into_strings();
        assert!(strings.contains(&"java.lang.String".to_string()));
        assert!(strings.contains(&"java.lang.Object".to_string()));
        assert!(strings.contains(&"length".to_string()));

        // And the disk record resolves back to the original symbols.
        let resolver = DiskResolver::new(&strings, &interner);
        let back: ClassRecord = resolver.class(&disk);
        assert_eq!(back.fqn, fqn);
        assert_eq!(back.name, stub.name);
        assert_eq!(back.kind, ClassKind::Class);
        assert_eq!(
            back.super_class
                .as_ref()
                .and_then(|t| t.as_reference_name()),
            Some(&interner.get_or_intern("java.lang.Object"))
        );
        assert_eq!(back.methods.len(), 1);
        assert_eq!(back.methods[0].name, stub.methods[0].name);
        assert_eq!(
            back.fields[0].constant_value,
            Some(AnnotationValue::Primitive(PrimitiveValue::Int(42)))
        );
        assert_eq!(back.annotations.len(), 1);
    }

    #[test]
    fn string_table_deduplicates() {
        let interner = ThreadedRodeo::default();
        let mut table = StubStringTable::new(&interner);

        let a = table.intern_str("java.lang.String");
        let b = table.intern_str("java.lang.String");
        assert_eq!(a, b);

        let spur = interner.get_or_intern("java.lang.String");
        assert_eq!(table.symbol(spur), a);

        assert_eq!(table.into_strings().len(), 1);
    }

    #[test]
    fn class_kind_from_flags() {
        assert_eq!(ClassKind::from_flags(0x0200), ClassKind::Interface);
        assert_eq!(ClassKind::from_flags(0x2200), ClassKind::Annotation); // interface | annotation
        assert_eq!(ClassKind::from_flags(0x4000), ClassKind::Enum);
        assert_eq!(ClassKind::from_flags(0x0021), ClassKind::Class);
    }
}
