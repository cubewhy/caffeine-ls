use index::symbol::LibraryId;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use triomphe::Arc;
use vfs::AbsPathBuf;

/// Uniquely identifies an independent module in the workspace
/// (e.g., a Maven Submodule or a Gradle Subproject).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectId(pub u32);

/// Uniquely identifies a JDK / Runtime SDK environment.
/// Modern Java workspaces allow different modules to bind to different JDK versions
/// (e.g., a legacy module using Java 8, while a new module uses Java 21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SdkId(pub u32);

/// Describes the type of a SourceSet.
/// A core characteristic of Java projects is that different code scopes within the same module
/// (e.g., production code vs. test code) have completely isolated classpaths and visibilities.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SourceSetKind {
    /// Production source code (corresponds to Maven/Gradle `main`).
    Main,
    /// Test source code (corresponds to Maven/Gradle `test`).
    Test,
    /// Custom source sets (e.g., Gradle's `integrationTest` or `site`).
    Custom(SmolStr),
}

/// Describes a precisely resolved classpath entry.
/// Modern build tools typically perform dynamic version conflict resolution before exporting data to the LSP.
/// Therefore, the entries here are flattened and deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClasspathEntry {
    /// A dependency on the compilation output of another specific source set within the workspace.
    /// E.g., Project A's `Test` source set depends on Project B's `Main` source set output.
    Internal {
        project_id: ProjectId,
        source_set: SourceSetKind,
    },
    /// A dependency on an external compiled artifact (e.g., a JAR downloaded from Maven Central).
    External(LibraryId),
    /// A dependency on the core JDK standard library (e.g., `java.base`).
    Sdk(SdkId),
}

/// A SourceSet is a first-class citizen in the Java LSP model.
/// It represents the minimal boundary unit for Java compilation, indexing, and error diagnostics.
#[derive(Debug, Clone)]
pub struct SourceSetData {
    pub kind: SourceSetKind,

    /// Hand-written source root directories.
    /// E.g., `src/main/java`, `src/main/kotlin`.
    pub source_roots: Vec<AbsPathBuf>,

    /// Source directories automatically generated at build time by annotation processors
    /// (e.g., Lombok, MapStruct, APT) or code generation tools (Protobuf, Avro).
    /// The LSP needs to track them separately because they are typically read-only and
    /// re-indexing is triggered by external file system events.
    pub generated_source_roots: Vec<AbsPathBuf>,

    /// The full classpath required to compile this source set
    /// (corresponds to Gradle's `compileClasspath` / Maven's `compile` + `provided`).
    /// When editing a file under this source set, the LSP's autocompletion and type inference
    /// must rely solely on this list.
    pub compile_classpath: Vec<ClasspathEntry>,

    /// The full classpath required to run this source set (corresponds to Gradle's `runtimeClasspath` / Maven's `runtime`).
    /// Used to support LSP-initiated actions like Debug/Run tests.
    pub runtime_classpath: Vec<ClasspathEntry>,

    /// If this source set defines a JPMS (Java 9+ Module System) module, this field records
    /// its module name (the name declared in `module-info.java`).
    /// Used to enforce strict module visibility checks (`exports` / `requires`).
    pub jpms_module_name: Option<SmolStr>,
}

/// Represents a specific Maven/Gradle module.
#[derive(Debug, Clone)]
pub struct ProjectData {
    pub id: ProjectId,
    pub name: SmolStr,

    /// The physical root directory of the module (containing `build.gradle`, `pom.xml`, or `.iml` files).
    pub root_path: AbsPathBuf,

    /// The target JDK bound to this specific module.
    pub target_sdk: Option<SdkId>,

    /// All source sets contained within the module (typically contains at least `Main` and `Test`).
    pub source_sets: FxHashMap<SourceSetKind, SourceSetData>,
}

/// Represents the configuration and metadata of a JDK.
#[derive(Debug, Clone)]
pub struct SdkData {
    pub id: SdkId,
    pub name: SmolStr,
    pub version: SmolStr,
    /// The home directory of the JDK (`JAVA_HOME`).
    pub home_path: AbsPathBuf,
    /// Physical paths to the core JDK libraries (`rt.jar` for Java 8, or modular files under the `jmods` directory for Java 9+).
    pub exploded_library_paths: Vec<AbsPathBuf>,
}

/// The complete compilation and dependency graph of the entire workspace.
#[derive(Default, Debug, Clone)]
pub struct WorkspaceGraph {
    pub projects: FxHashMap<ProjectId, Arc<ProjectData>>,
    pub sdks: FxHashMap<SdkId, Arc<SdkData>>,
    pub library_paths: FxHashMap<LibraryId, AbsPathBuf>,

    /// Maps a specific source root directory (including generated sources)
    /// directly to its owning Project and specific SourceSet.
    /// This avoids the ambiguity caused by blindly traversing parent directories.
    pub source_root_to_owning_set: FxHashMap<AbsPathBuf, (ProjectId, SourceSetKind)>,
}

impl WorkspaceGraph {
    /// Precisely resolves which project and which source set (`Main` or `Test`) a file belongs to.
    /// This determines which dependencies the file can see in the LSP, and whether test framework calls are permitted.
    pub fn resolve_source_set_for_path(
        &self,
        file_path: &AbsPathBuf,
    ) -> Option<(Arc<ProjectData>, SourceSetKind)> {
        // Walk up the directory tree to match an exactly registered Source Root
        for ancestor in file_path.ancestors() {
            if let Ok(abs_ancestor) = AbsPathBuf::try_from(ancestor.to_path_buf())
                && let Some((project_id, source_set_kind)) =
                    self.source_root_to_owning_set.get(&abs_ancestor)
                && let Some(project) = self.projects.get(project_id)
            {
                return Some((project.clone(), source_set_kind.clone()));
            }
        }
        None
    }

    /// Backward compatibility interface: resolves only the `Project` the file belongs to.
    pub fn resolve_project_for_path(&self, file_path: &AbsPathBuf) -> Option<Arc<ProjectData>> {
        self.resolve_source_set_for_path(file_path)
            .map(|(project, _)| project)
    }
}
