pub use hir_expand;

pub mod db;
pub mod disk;
pub mod index;
pub mod loader;
pub mod modules;
pub mod project;
pub mod stubs;
pub mod symbol_index;

pub use db::{
    ClassGenericInfo, HirDatabase, HirState, LibraryId, LibraryKind, ProjectGraph, ResolutionScope,
    Resolved, ResolvedClass, SourceClass, class_generic_info, class_record, classpath,
    classpath_libraries, file_item_tree, file_symbols, fqn_resolve, jdk_builtin_libraries,
    library_name_index, module_record, project_graph, registered_libraries, resolve_in_libraries,
    set_project_graph, source_class_fqn, source_set_for_file, source_set_symbols, super_types,
    warmup_library,
};
pub use index::{ClassEntry, LibraryIndex, ModuleEntry, NameIndex};
pub use modules::{
    ModuleDescriptor, ModuleGraph, WorkspaceModuleGraph, is_package_exported, is_package_visible,
    is_package_visible_from_unnamed, module_descriptor, module_for_class, module_graph,
    module_graph_for_source_set, modules_for_package, readable_modules, required_modules,
};
pub use project::{Classpath, ClasspathEntry, LibraryInfo, ProjectGraphData, SourceSetId};
pub use project_model::{ProjectId, SourceSetKind};
pub use stubs::{
    ClassKind, ClassOrModuleRecord, ClassOrModuleStub, ClassRecord, ClassStub, FieldStub,
    MethodStub, ModuleRecord, ModuleStub, ParamData, PrimitiveType, PrimitiveValue, Symbol,
    TypeRef,
};
pub use symbol_index::{SourceSymbol, SourceSymbolIndex, SourceSymbolKind, SourceSymbolRef};
