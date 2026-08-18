//! Compact, serializable declaration stubs for JVM classes.
//!
//! The stub data types themselves are defined generically over the name
//! representation `N` in [`syntax::stub`] and re-exported here. This module
//! adds the conversions between the two instantiations:
//!
//! * `N = [`Symbol`]` (a [`lasso::Spur`] into a session-wide interner) is the
//!   in-memory representation;
//! * `N = u32` (an index into a per-library string table) is the on-disk
//!   representation used by the persistent cache.
//!
//! Member-level data is only kept for classes that are actually requested
//! (see [`crate::index::LibraryIndex`]).

use lasso::ThreadedRodeo;
use rustc_hash::FxHashMap;

pub use syntax::stub::{
    AnnotationSig, AnnotationValue, ClassKind, ClassOrModuleStub, ClassStub, FieldStub, MethodStub,
    ModuleExports, ModuleOpens, ModuleProvides, ModuleRequires, ModuleStub, ParamData,
    PrimitiveType, PrimitiveValue, RecordComponentData, Symbol, TypeBound, TypeParameter, TypeRef,
};

/// In-memory instantiations.
pub type ClassRecord = ClassStub<Symbol>;
pub type ModuleRecord = ModuleStub<Symbol>;
pub type ClassOrModuleRecord = ClassOrModuleStub<Symbol>;

/// On-disk instantiations (string-table indices).
pub type DiskClassRecord = ClassStub<u32>;
pub type DiskModuleRecord = ModuleStub<u32>;
pub type DiskClassOrModuleRecord = ClassOrModuleStub<u32>;

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

    pub fn type_ref(&mut self, t: &TypeRef<Symbol>) -> TypeRef<u32> {
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

    pub fn type_bound(&mut self, b: &TypeBound<Symbol>) -> TypeBound<u32> {
        match b {
            TypeBound::Upper(t) => TypeBound::Upper(self.type_ref(t)),
            TypeBound::Lower(t) => TypeBound::Lower(self.type_ref(t)),
        }
    }

    pub fn type_parameter(&mut self, p: &TypeParameter<Symbol>) -> TypeParameter<u32> {
        TypeParameter {
            name: self.symbol(p.name),
            bounds: p.bounds.iter().map(|b| self.type_ref(b)).collect(),
            annotations: p.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    pub fn annotation(&mut self, a: &AnnotationSig<Symbol>) -> AnnotationSig<u32> {
        AnnotationSig {
            annotation_type: self.type_ref(&a.annotation_type),
            arguments: a
                .arguments
                .iter()
                .map(|(name, value)| (self.symbol(*name), self.annotation_value(value)))
                .collect(),
        }
    }

    pub fn annotation_value(&mut self, v: &AnnotationValue<Symbol>) -> AnnotationValue<u32> {
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

    pub fn param(&mut self, p: &ParamData<Symbol>) -> ParamData<u32> {
        ParamData {
            flags: p.flags,
            name: p.name.map(|n| self.symbol(n)),
            param_type: self.type_ref(&p.param_type),
            annotations: p.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    pub fn method(&mut self, m: &MethodStub<Symbol>) -> MethodStub<u32> {
        MethodStub {
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

    pub fn field(&mut self, f: &FieldStub<Symbol>) -> FieldStub<u32> {
        FieldStub {
            flags: f.flags,
            field_type: self.type_ref(&f.field_type),
            annotations: f.annotations.iter().map(|a| self.annotation(a)).collect(),
            constant_value: f.constant_value.as_ref().map(|v| self.annotation_value(v)),
        }
    }

    pub fn record_component(
        &mut self,
        r: &RecordComponentData<Symbol>,
    ) -> RecordComponentData<u32> {
        RecordComponentData {
            name: self.symbol(r.name),
            component_type: self.type_ref(&r.component_type),
            varargs: r.varargs,
            annotations: r.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    /// Converts a class stub, attaching its fully qualified name.
    pub fn class(&mut self, c: &ClassStub<Symbol>) -> ClassStub<u32> {
        ClassStub {
            fqn: self.symbol(c.fqn),
            name: self.symbol(c.name),
            flags: c.flags,
            is_record: c.is_record,
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

    pub fn module(&mut self, m: &ModuleStub<Symbol>) -> ModuleStub<u32> {
        ModuleStub {
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

    pub fn method(&self, m: &MethodStub<u32>) -> MethodStub<Symbol> {
        MethodStub {
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

    pub fn field(&self, f: &FieldStub<u32>) -> FieldStub<Symbol> {
        FieldStub {
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
            varargs: r.varargs,
            annotations: r.annotations.iter().map(|a| self.annotation(a)).collect(),
        }
    }

    pub fn class(&self, c: &ClassStub<u32>) -> ClassStub<Symbol> {
        ClassStub {
            fqn: self.symbol(c.fqn),
            name: self.symbol(c.name),
            flags: c.flags,
            is_record: c.is_record,
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

    pub fn module(&self, m: &ModuleStub<u32>) -> ModuleStub<Symbol> {
        ModuleStub {
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

    pub fn class_or_module(&self, c: &ClassOrModuleStub<u32>) -> ClassOrModuleStub<Symbol> {
        match c {
            ClassOrModuleStub::Class(class) => ClassOrModuleStub::Class(self.class(class)),
            ClassOrModuleStub::Module(module) => ClassOrModuleStub::Module(self.module(module)),
        }
    }
}

#[cfg(test)]
mod tests {
    use lasso::ThreadedRodeo;

    use super::*;

    fn class_stub(interner: &ThreadedRodeo) -> ClassStub<Symbol> {
        ClassStub {
            fqn: interner.get_or_intern("java.lang.String"),
            name: interner.get_or_intern("String"),
            flags: 0x0021, // ACC_PUBLIC | ACC_SUPER
            is_record: false,
            super_class: Some(TypeRef::Reference {
                name: interner.get_or_intern("java.lang.Object"),
                generic_args: Vec::new(),
            }),
            interfaces: vec![TypeRef::Reference {
                name: interner.get_or_intern("java.lang.CharSequence"),
                generic_args: Vec::new(),
            }],
            type_params: vec![TypeParameter {
                name: interner.get_or_intern("T"),
                bounds: Vec::new(),
                annotations: Vec::new(),
            }],
            permitted_subclasses: Vec::new(),
            record_components: Vec::new(),
            methods: vec![MethodStub {
                flags: 0x0001, // ACC_PUBLIC
                name: interner.get_or_intern("length"),
                return_type: TypeRef::Primitive(PrimitiveType::Int),
                type_params: Vec::new(),
                throws_list: Vec::new(),
                params: Vec::new(),
                annotations: Vec::new(),
                default_value: None,
            }],
            fields: vec![FieldStub {
                flags: 0x0001,
                field_type: TypeRef::Reference {
                    name: interner.get_or_intern("int"),
                    generic_args: Vec::new(),
                },
                annotations: Vec::new(),
                constant_value: Some(AnnotationValue::Primitive(PrimitiveValue::Int(42))),
            }],
            annotations: vec![AnnotationSig {
                annotation_type: TypeRef::Reference {
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
        let disk = table.class(&stub);

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
        assert_eq!(back.is_record, stub.is_record);
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
        assert_eq!(ClassKind::from_flags(0x0200, false), ClassKind::Interface);
        assert_eq!(ClassKind::from_flags(0x2200, false), ClassKind::Annotation); // interface | annotation
        assert_eq!(ClassKind::from_flags(0x4000, false), ClassKind::Enum);
        assert_eq!(ClassKind::from_flags(0x0021, false), ClassKind::Class);
        assert_eq!(ClassKind::from_flags(0x0031, true), ClassKind::Record); // final class + Record attribute
        assert_eq!(ClassKind::from_flags(0x0200, true), ClassKind::Interface); // interface wins over record
    }
}
