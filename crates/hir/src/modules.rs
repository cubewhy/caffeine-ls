//! JPMS (Java Platform Module System) support.
//!
//! Module descriptors are parsed from compiled artifacts at load time:
//! modular jars contribute their single `module-info.class`, and the JDK
//! jimage contributes one descriptor per module. This module turns those
//! descriptors into first-class salsa queries (per-library module graphs)
//! plus a source-set-scoped aggregate module path and readability/visibility
//! helpers.
//!
//! Sources without a `module-info.java` behave as the *unnamed module*
//! (classpath semantics): they read every module and every exported
//! package. Named-module enforcement for source files is a follow-up.

use std::sync::Arc;

use rust_asm::constants::ACC_TRANSITIVE;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    db::{
        HirDatabase, LibraryId, ProjectGraph, ResolutionScope, classpath_libraries, fqn_resolve,
        library_name_index, module_record,
    },
    project::SourceSetId,
    stubs::{ClassOrModuleStub, ModuleStub, Symbol},
};

/// A module descriptor resolved to a specific library and index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDescriptor {
    pub library: LibraryId,
    pub module_idx: u32,
    pub stub: Arc<ModuleStub<Symbol>>,
}

impl ModuleDescriptor {
    pub fn name(&self) -> Symbol {
        self.stub.name
    }

    pub fn flags(&self) -> u16 {
        self.stub.flags
    }
}

/// The JPMS module graph of a single library: module descriptors plus the
/// package → owning-module(s) map derived from the class entries.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ModuleGraph {
    modules: FxHashMap<Symbol, Arc<ModuleStub<Symbol>>>,
    package_to_modules: FxHashMap<Symbol, Vec<Symbol>>,
}

impl ModuleGraph {
    fn new(
        modules: FxHashMap<Symbol, Arc<ModuleStub<Symbol>>>,
        package_to_modules: FxHashMap<Symbol, Vec<Symbol>>,
    ) -> Self {
        Self {
            modules,
            package_to_modules,
        }
    }

    pub fn module(&self, name: Symbol) -> Option<&Arc<ModuleStub<Symbol>>> {
        self.modules.get(&name)
    }

    pub fn len(&self) -> usize {
        self.modules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Symbol, &Arc<ModuleStub<Symbol>>)> {
        self.modules.iter()
    }

    /// The module names owning `package` in this library.
    pub fn modules_for_package(&self, package: Symbol) -> &[Symbol] {
        self.package_to_modules
            .get(&package)
            .map_or(&[], Vec::as_slice)
    }

    /// Raw access to the package → owners map (used by the workspace
    /// aggregate).
    pub fn package_map(&self) -> &FxHashMap<Symbol, Vec<Symbol>> {
        &self.package_to_modules
    }

    /// The packages belonging to `module`: those named in its descriptor's
    /// `exports`/`opens` directives plus every package with a class in this
    /// library owned by the module.
    pub fn packages_of_module(&self, module: Symbol) -> Vec<Symbol> {
        let mut packages: FxHashSet<Symbol> = FxHashSet::default();
        if let Some(stub) = self.modules.get(&module) {
            packages.extend(stub.exports.iter().map(|e| e.package_name));
            packages.extend(stub.opens.iter().map(|o| o.package_name));
        }
        for (package, owners) in &self.package_to_modules {
            if owners.contains(&module) {
                packages.insert(*package);
            }
        }
        packages.into_iter().collect()
    }
}

/// The aggregate module path of a source set: module name → descriptor (the
/// first declaration on the classpath wins), plus package → owning module
/// descriptors.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WorkspaceModuleGraph {
    /// module name → descriptor. The first declaration wins; duplicates are
    /// logged as warnings.
    pub modules: FxHashMap<Symbol, ModuleDescriptor>,
    /// package → owning module descriptors.
    package_to_modules: FxHashMap<Symbol, Vec<ModuleDescriptor>>,
}

impl WorkspaceModuleGraph {
    pub fn module(&self, name: Symbol) -> Option<&ModuleDescriptor> {
        self.modules.get(&name)
    }

    pub fn len(&self) -> usize {
        self.modules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Symbol, &ModuleDescriptor)> {
        self.modules.iter()
    }

    /// The modules owning `package` on this module path.
    pub fn modules_for_package(&self, package: Symbol) -> &[ModuleDescriptor] {
        self.package_to_modules
            .get(&package)
            .map_or(&[], Vec::as_slice)
    }
}

/// Index of a module within its library's `NameIndex`.
fn module_index(db: &dyn HirDatabase, library: LibraryId, name: Symbol) -> Option<u32> {
    library_name_index(db, library)
        .module(name)
        .map(|(idx, _)| idx)
}

/// The module graph of a library, memoized. Loads every tier-2 module
/// descriptor once per (library, revision).
#[salsa::tracked(returns(ref))]
fn module_graph_query(
    db: &dyn HirDatabase,
    _project_graph: ProjectGraph,
    library: LibraryId,
) -> Arc<ModuleGraph> {
    let names = library_name_index(db, library);

    let mut modules: FxHashMap<Symbol, Arc<ModuleStub<Symbol>>> = FxHashMap::default();
    for (idx, _entry) in names.modules().iter().enumerate() {
        let module_idx = idx as u32;
        if let Some(record) = module_record(db, library, module_idx)
            && let ClassOrModuleStub::Module(module) = record.as_ref()
        {
            modules.insert(module.name, Arc::new(module.clone()));
        }
    }

    // Derive package → owning module(s) from the class entries. This yields
    // the module's full package set (not just the exported ones).
    let mut package_to_modules: FxHashMap<Symbol, Vec<Symbol>> = FxHashMap::default();
    for entry in names.entries() {
        if let Some(module) = entry.module {
            let owners = package_to_modules.entry(entry.package).or_default();
            if !owners.contains(&module) {
                owners.push(module);
            }
        }
    }

    Arc::new(ModuleGraph::new(modules, package_to_modules))
}

/// The module graph of a registered library.
pub fn module_graph(db: &dyn HirDatabase, library: LibraryId) -> Arc<ModuleGraph> {
    let project_graph =
        ProjectGraph::try_get(db).unwrap_or_else(|| panic!("no project graph; this is a bug"));
    module_graph_query(db, project_graph, library).clone()
}

/// The full descriptor of `module` in `library`, if it exists.
pub fn module_descriptor(
    db: &dyn HirDatabase,
    library: LibraryId,
    module: Symbol,
) -> Option<Arc<ModuleStub<Symbol>>> {
    module_graph(db, library).module(module).cloned()
}

/// Resolves a fully qualified class name to its owning module descriptor,
/// within a resolution scope.
pub fn module_for_class(
    db: &dyn HirDatabase,
    scope: &ResolutionScope<'_>,
    fqn: &str,
) -> Option<ModuleDescriptor> {
    let resolved = fqn_resolve(db, scope, fqn)?;
    let module = resolved.entry.module?;
    let stub = module_descriptor(db, resolved.library, module)?;
    Some(ModuleDescriptor {
        library: resolved.library,
        module_idx: module_index(db, resolved.library, module)?,
        stub,
    })
}

/// The module(s) in `library` owning `package`.
pub fn modules_for_package(
    db: &dyn HirDatabase,
    library: LibraryId,
    package: Symbol,
) -> Vec<ModuleDescriptor> {
    module_graph(db, library)
        .modules_for_package(package)
        .iter()
        .filter_map(|name| {
            let stub = module_descriptor(db, library, *name)?;
            Some(ModuleDescriptor {
                library,
                module_idx: module_index(db, library, *name)?,
                stub,
            })
        })
        .collect()
}

/// Whether `module` exports `package`, either unqualified or (if
/// `to_module` is given) qualified to that module.
pub fn is_package_exported(
    module: &ModuleStub<Symbol>,
    package: Symbol,
    to_module: Option<Symbol>,
) -> bool {
    module.exports.iter().any(|export| {
        if export.package_name != package {
            return false;
        }
        if export.to_modules.is_empty() {
            return true;
        }
        match to_module {
            Some(to) => export.to_modules.contains(&to),
            None => false,
        }
    })
}

/// The module names directly required by `module`.
pub fn required_modules(module: &ModuleStub<Symbol>) -> Vec<Symbol> {
    module.requires.iter().map(|r| r.module_name).collect()
}

/// The aggregate module path of a source set: every JPMS module declared by a
/// library on the source set's classpath, plus the package → module mapping.
/// Deduplicates repeated libraries (same id on a classpath) while preserving
/// classpath order.
#[salsa::tracked(returns(ref))]
fn source_set_module_graph_query(
    db: &dyn HirDatabase,
    _project_graph: ProjectGraph,
    source_set: SourceSetId,
) -> Arc<WorkspaceModuleGraph> {
    Arc::new(workspace_module_graph_for_libraries(
        db,
        &classpath_libraries(db, source_set),
    ))
}

/// The module path of a source set (its classpath-scoped module graph).
pub fn module_graph_for_source_set(
    db: &dyn HirDatabase,
    source_set: SourceSetId,
) -> Arc<WorkspaceModuleGraph> {
    let project_graph =
        ProjectGraph::try_get(db).unwrap_or_else(|| panic!("no project graph; this is a bug"));
    source_set_module_graph_query(db, project_graph, source_set).clone()
}

/// Builds the aggregate module path over an ordered library list. First
/// declaration of a module name wins; a duplicate in a later library is
/// logged and ignored.
fn workspace_module_graph_for_libraries(
    db: &dyn HirDatabase,
    libraries: &[LibraryId],
) -> WorkspaceModuleGraph {
    let mut modules: FxHashMap<Symbol, ModuleDescriptor> = FxHashMap::default();
    let mut package_to_modules: FxHashMap<Symbol, Vec<ModuleDescriptor>> = FxHashMap::default();
    let mut seen: FxHashSet<LibraryId> = FxHashSet::default();

    for &library in libraries {
        if !seen.insert(library) {
            continue;
        }
        let graph = module_graph(db, library);
        for (name, stub) in graph.iter() {
            let name = *name;
            if modules.contains_key(&name) {
                tracing::warn!(
                    module = ?name,
                    duplicate = ?library,
                    "duplicate JPMS module; keeping the first declaration"
                );
                continue;
            }
            let Some(module_idx) = module_index(db, library, name) else {
                continue;
            };
            modules.insert(
                name,
                ModuleDescriptor {
                    library,
                    module_idx,
                    stub: stub.clone(),
                },
            );
        }

        for (package, owners) in graph.package_map() {
            let entry = package_to_modules.entry(*package).or_default();
            for owner in owners {
                if let Some(descriptor) = modules.get(owner)
                    && !entry.iter().any(|d| d.stub.name == *owner)
                {
                    entry.push(descriptor.clone());
                }
            }
        }
    }

    WorkspaceModuleGraph {
        modules,
        package_to_modules,
    }
}

/// The modules readable from `from`, honoring `requires transitive`
/// (readability graph per JLS §7.7.2).
pub fn readable_modules(
    workspace: &WorkspaceModuleGraph,
    from: &ModuleDescriptor,
) -> FxHashSet<Symbol> {
    let mut readable = FxHashSet::default();
    let mut queue: Vec<Symbol> = vec![from.stub.name];
    while let Some(current) = queue.pop() {
        if !readable.insert(current) {
            continue;
        }
        let Some(descriptor) = workspace.modules.get(&current) else {
            continue;
        };
        for require in &descriptor.stub.requires {
            let target = require.module_name;
            // The starting module reads its direct requires regardless of
            // modifier; deeper modules only propagate `requires transitive`.
            if current == from.stub.name || require.flags & ACC_TRANSITIVE != 0 {
                queue.push(target);
            }
        }
    }
    readable
}

/// Whether `package` is visible from `from` (a named module): the owning
/// module must be readable and the package exported to `from`. A module's
/// own packages are always visible to it.
pub fn is_package_visible(
    workspace: &WorkspaceModuleGraph,
    from: &ModuleDescriptor,
    package: Symbol,
) -> bool {
    let readable = readable_modules(workspace, from);
    workspace.modules_for_package(package).iter().any(|owner| {
        if owner.stub.name == from.stub.name {
            return true;
        }
        readable.contains(&owner.stub.name)
            && is_package_exported(&owner.stub, package, Some(from.stub.name))
    })
}

/// Whether `package` is visible from the unnamed module (plain classpath
/// semantics): packages from non-modular archives are always accessible, and
/// module packages are accessible when exported (unqualified or qualified).
pub fn is_package_visible_from_unnamed(workspace: &WorkspaceModuleGraph, package: Symbol) -> bool {
    let owners = workspace.modules_for_package(package);
    if owners.is_empty() {
        // Classpath (unnamed module) package: always accessible.
        return true;
    }
    owners
        .iter()
        .any(|owner| is_package_exported(&owner.stub, package, None))
}

#[cfg(test)]
mod tests {
    use lasso::ThreadedRodeo;

    use super::*;
    use crate::stubs::{ModuleExports, ModuleRequires};

    fn module_stub(
        interner: &ThreadedRodeo,
        name: &str,
        requires: &[(&str, u16)],
        exports: &[(&str, u16, Vec<&str>)],
    ) -> Arc<ModuleStub<Symbol>> {
        Arc::new(ModuleStub {
            name: interner.get_or_intern(name),
            flags: 0,
            version: None,
            requires: requires
                .iter()
                .map(|(module, flags)| ModuleRequires {
                    module_name: interner.get_or_intern(module),
                    flags: *flags,
                    compiled_version: None,
                })
                .collect(),
            exports: exports
                .iter()
                .map(|(package, flags, to)| ModuleExports {
                    package_name: interner.get_or_intern(package),
                    flags: *flags,
                    to_modules: to.iter().map(|m| interner.get_or_intern(m)).collect(),
                })
                .collect(),
            opens: Vec::new(),
            uses: Vec::new(),
            provides: Vec::new(),
        })
    }

    fn owner(stub: &Arc<ModuleStub<Symbol>>, idx: u64) -> ModuleDescriptor {
        ModuleDescriptor {
            library: LibraryId(idx),
            module_idx: 0,
            stub: stub.clone(),
        }
    }

    #[test]
    fn export_semantics() {
        let interner = ThreadedRodeo::default();
        let stub = module_stub(
            &interner,
            "com.example.app",
            &[("java.base", ACC_TRANSITIVE)],
            &[
                ("com.example.api", 0, Vec::new()),
                ("com.example.internal", 0, vec!["com.example.consumer"]),
            ],
        );

        let api = interner.get_or_intern("com.example.api");
        let internal = interner.get_or_intern("com.example.internal");
        let consumer = interner.get_or_intern("com.example.consumer");
        let other = interner.get_or_intern("com.example.other");

        // Unqualified export: visible to everyone.
        assert!(is_package_exported(&stub, api, None));
        assert!(is_package_exported(&stub, api, Some(other)));
        // Qualified export: only the listed module.
        assert!(!is_package_exported(&stub, internal, None));
        assert!(!is_package_exported(&stub, internal, Some(other)));
        assert!(is_package_exported(&stub, internal, Some(consumer)));
        // Non-exported package.
        assert!(!is_package_exported(
            &stub,
            interner.get_or_intern("com.example.hidden"),
            None
        ));
    }

    #[test]
    fn readable_closure_honors_transitive() {
        let interner = ThreadedRodeo::default();
        let app = owner(
            &module_stub(
                &interner,
                "app",
                &[("lib", 0), ("trans", ACC_TRANSITIVE)],
                &[],
            ),
            1,
        );
        let lib = module_stub(&interner, "lib", &[("deep", 0)], &[]);
        let trans = module_stub(&interner, "trans", &[("deep", 0)], &[]);
        let deep = module_stub(&interner, "deep", &[], &[]);

        let workspace = WorkspaceModuleGraph {
            modules: FxHashMap::from_iter([
                (app.stub.name, app.clone()),
                (lib.name, owner(&lib, 2)),
                (trans.name, owner(&trans, 3)),
                (deep.name, owner(&deep, 4)),
            ]),
            package_to_modules: FxHashMap::default(),
        };

        let readable = readable_modules(&workspace, &app);
        let s = |name: &str| interner.get_or_intern(name);
        // app reads its direct requires `lib` and `trans`. A transitive edge
        // propagates only the target module itself (JLS §7.7.2): `deep` is
        // plain-required by `lib`/`trans`, so it stays out of app's read set.
        assert!(readable.contains(&s("app")));
        assert!(readable.contains(&s("lib")));
        assert!(readable.contains(&s("trans")));
        assert!(!readable.contains(&s("deep")));
    }

    #[test]
    fn transitive_chain_reads_deep() {
        let interner = ThreadedRodeo::default();
        let app = owner(
            &module_stub(&interner, "app", &[("mid", ACC_TRANSITIVE)], &[]),
            1,
        );
        let mid = module_stub(&interner, "mid", &[("deep", ACC_TRANSITIVE)], &[]);
        let deep = module_stub(&interner, "deep", &[], &[]);

        let workspace = WorkspaceModuleGraph {
            modules: FxHashMap::from_iter([
                (app.stub.name, app.clone()),
                (mid.name, owner(&mid, 2)),
                (deep.name, owner(&deep, 3)),
            ]),
            package_to_modules: FxHashMap::default(),
        };

        let readable = readable_modules(&workspace, &app);
        let s = |name: &str| interner.get_or_intern(name);
        // Chains of `requires transitive` do propagate (JLS §7.7.2 R3 rule).
        assert!(readable.contains(&s("mid")));
        assert!(readable.contains(&s("deep")));
    }

    #[test]
    fn visibility_checks() {
        let interner = ThreadedRodeo::default();
        let app = owner(
            &module_stub(
                &interner,
                "app",
                &[("lib", 0), ("trans", ACC_TRANSITIVE)],
                &[],
            ),
            1,
        );
        let lib = module_stub(
            &interner,
            "lib",
            &[],
            &[("lib.api", 0, Vec::new()), ("lib.internal", 0, vec!["app"])],
        );
        let trans = module_stub(&interner, "trans", &[], &[("trans.api", 0, Vec::new())]);
        let lib_owner = owner(&lib, 2);
        let trans_owner = owner(&trans, 3);

        let mut package_to_modules: FxHashMap<Symbol, Vec<ModuleDescriptor>> = FxHashMap::default();
        for (package, owner) in [
            ("lib.api", &lib_owner),
            ("lib.internal", &lib_owner),
            ("trans.api", &trans_owner),
        ] {
            package_to_modules
                .entry(interner.get_or_intern(package))
                .or_default()
                .push(owner.clone());
        }

        let workspace = WorkspaceModuleGraph {
            modules: FxHashMap::from_iter([
                (app.stub.name, app.clone()),
                (lib.name, lib_owner.clone()),
                (trans.name, trans_owner.clone()),
            ]),
            package_to_modules,
        };

        // app requires lib (non-transitive) and trans (transitive).
        assert!(is_package_visible(
            &workspace,
            &app,
            interner.get_or_intern("lib.api")
        ));
        assert!(is_package_visible(
            &workspace,
            &app,
            interner.get_or_intern("lib.internal")
        ));
        assert!(is_package_visible(
            &workspace,
            &app,
            interner.get_or_intern("trans.api")
        ));

        // Unnamed module sees unqualified exports, but NOT qualified ones
        // (`lib.internal` is only exported to `app`).
        assert!(is_package_visible_from_unnamed(
            &workspace,
            interner.get_or_intern("lib.api")
        ));
        assert!(!is_package_visible_from_unnamed(
            &workspace,
            interner.get_or_intern("lib.internal")
        ));

        // A package with no owner module is invisible.
        assert!(!is_package_visible(
            &workspace,
            &app,
            interner.get_or_intern("nowhere.here")
        ));
    }

    #[test]
    fn own_and_classpath_packages_are_visible() {
        let interner = ThreadedRodeo::default();
        let app = owner(&module_stub(&interner, "app", &[], &[]), 1);
        let mut package_to_modules: FxHashMap<Symbol, Vec<ModuleDescriptor>> = FxHashMap::default();
        package_to_modules
            .entry(interner.get_or_intern("app.self"))
            .or_default()
            .push(app.clone());
        let workspace = WorkspaceModuleGraph {
            modules: FxHashMap::from_iter([(app.stub.name, app.clone())]),
            package_to_modules,
        };

        // A module's own package is visible even though it is not exported.
        assert!(is_package_visible(
            &workspace,
            &app,
            interner.get_or_intern("app.self")
        ));
        // A classpath package (no module owner) is visible from the unnamed
        // module.
        assert!(is_package_visible_from_unnamed(
            &workspace,
            interner.get_or_intern("com.example.plain")
        ));
    }
}
