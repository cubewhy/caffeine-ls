//! The JVM declaration stubs produced by the classfile readers: the
//! "item tree" of a compiled library class, with no bodies and no source
//! locations ([JVMS §4](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html)).
//! Re-exported from [`syntax::stub`], which defines them once and shares them
//! with `hir-expand`, the `hir` indexer and `hir-ty`.

pub use syntax::stub::{
    AnnotationSig, AnnotationValue, ClassKind, ClassOrModuleStub, ClassStub, FieldStub, MethodStub,
    ModuleExports, ModuleOpens, ModuleProvides, ModuleRequires, ModuleStub, ParamData,
    RecordComponentData, Symbol, TypeParameter,
};
