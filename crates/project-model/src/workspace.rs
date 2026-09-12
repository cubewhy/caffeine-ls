use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
};

use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use triomphe::Arc;
use vfs::AbsPathBuf;

/// Uniquely identifies an independent module in the workspace
/// (e.g., a Maven Submodule or a Gradle Subproject).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProjectId(pub u32);

/// Uniquely identifies a JDK / Runtime SDK environment.
/// Modern Java workspaces allow different modules to bind to different JDK versions
/// (e.g., a legacy module using Java 8, while a new module uses Java 21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SdkId(pub u32);

/// The Java *source* level a source set is compiled at (`-source N`), plus
/// javac's `--enable-preview` flag. Distinct from the SDK: a project may bind
/// JDK 21 while compiling at `-source 8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JavaLanguageLevel {
    /// The feature release (`8`, `11`, `17`, `21`) — never a `1.x` spelling.
    pub source: u8,
    /// Whether `--enable-preview` is in effect for the source set.
    pub preview: bool,
}

impl JavaLanguageLevel {
    /// javac's own floor (`Source.MIN = JDK8`); levels below it are not modelled.
    pub const MIN: u8 = 8;
    /// The highest release this build knows about.
    pub const MAX: u8 = 28;

    /// Validates a feature release. Out-of-range input yields `None`, which
    /// disables level gating for that source set rather than guessing.
    pub fn new(source: u8, preview: bool) -> Option<Self> {
        (Self::MIN..=Self::MAX)
            .contains(&source)
            .then_some(Self { source, preview })
    }

    /// Parses every spelling the importers export: `8`, `1.8`, `17`, `21.0.5`,
    /// `JDK_17`, `JDK_1_8`, `JDK_14_PREVIEW`, `JavaSE-17`. Anything else (and
    /// any release outside [`Self::MIN`]..=[`Self::MAX`]) yields `None`.
    pub fn parse(raw: &str) -> Option<Self> {
        let mut rest = raw.trim();
        rest = rest.strip_prefix("JavaSE-").unwrap_or(rest);
        rest = rest.strip_prefix("JDK_").unwrap_or(rest);

        let mut preview = false;
        if let Some(head) = rest
            .len()
            .checked_sub("_PREVIEW".len())
            .map(|n| rest.split_at(n))
            .filter(|(_, tail)| tail.eq_ignore_ascii_case("_PREVIEW"))
        {
            rest = head.0;
            preview = true;
        }

        let dotted = rest.replace('_', ".");
        let digits: String = dotted
            .strip_prefix("1.")
            .unwrap_or(&dotted)
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        Self::new(digits.parse().ok()?, preview)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LibraryId(pub u64);

impl std::fmt::Display for LibraryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl LibraryId {
    /// Generate a unique ID for a file based on its path and metadata
    pub fn from_file_path(path: &Path) -> std::io::Result<Self> {
        let metadata = fs::metadata(path)?;
        let modified = metadata.modified()?;

        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);

        format!("{:?}", modified).hash(&mut hasher);

        Ok(Self(hasher.finish()))
    }

    /// Generates a unique ID for a mutable local workspace module.
    /// Hashes ONLY the absolute path. We do not hash the modified time because
    /// active files change constantly, and we handle those via the `ParseCache`.
    pub fn from_project_root(path: &Path) -> Self {
        let mut hasher = DefaultHasher::new();
        let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        "/\\".hash(&mut hasher);
        abs_path.hash(&mut hasher);

        Self(hasher.finish())
    }
}

#[derive(Debug, Clone)]
pub struct Library {
    pub id: LibraryId,
    pub path: AbsPathBuf,
    pub readonly: bool,
}

impl Library {
    pub fn editable(lib_id: LibraryId, path: AbsPathBuf) -> Self {
        Self {
            id: lib_id,
            path,
            readonly: false,
        }
    }

    pub fn readonly(lib_id: LibraryId, path: AbsPathBuf) -> Self {
        Self {
            id: lib_id,
            path,
            readonly: true,
        }
    }
}

/// Describes the type of a SourceSet.
/// A core characteristic of Java projects is that different code scopes within the same module
/// (e.g., production code vs. test code) have completely isolated classpaths and visibilities.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceSetKind {
    /// Production source code (corresponds to Maven/Gradle `main`).
    Main,
    /// Test source code (corresponds to Maven/Gradle `test`).
    Test,
    /// Custom source sets (e.g., Gradle's `integrationTest` or `site`).
    Custom(SmolStr),
}

impl std::fmt::Display for SourceSetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceSetKind::Main => write!(f, "main"),
            SourceSetKind::Test => write!(f, "test"),
            SourceSetKind::Custom(name) => write!(f, "{name}"),
        }
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// Represents a specific Maven/Gradle module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectData {
    pub id: ProjectId,
    pub name: SmolStr,

    /// The physical root directory of the module (containing `build.gradle`, `pom.xml`, or `.iml` files).
    pub root_path: AbsPathBuf,

    /// The target JDK bound to this specific module.
    pub target_sdk: Option<SdkId>,

    /// The Java source level every source set of this project compiles at, when
    /// the build system reported one. `None` disables source-level checks for
    /// the project's files (the fallback for workspaces whose build system
    /// exports nothing — a wrong level would produce false errors everywhere).
    pub language_level: Option<JavaLanguageLevel>,

    /// The release of the platform API every source set of this project
    /// compiles against (`javac --release N`,
    /// [JEP 247](https://openjdk.org/jeps/247)), when the build system
    /// reported one. Distinct from [`Self::language_level`]: `-source 8`
    /// alone does not select a platform view, so `None` disables the
    /// release-view check for the project's files.
    pub release: Option<u8>,

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
    pub library_paths: FxHashMap<LibraryId, Library>,

    /// Library → the source archive (`-sources.jar` / `src.zip`) the build
    /// system located for it. Absent for libraries with no sources attached.
    pub library_sources: FxHashMap<LibraryId, AbsPathBuf>,

    /// Maps a specific source root directory (including generated sources)
    /// directly to its owning Project and specific SourceSet.
    /// This avoids the ambiguity caused by blindly traversing parent directories.
    pub source_root_to_owning_set: FxHashMap<AbsPathBuf, (ProjectId, SourceSetKind)>,
}

impl WorkspaceGraph {
    /// Builds a minimal graph for a workspace without any supported build
    /// system: the workspace root itself is treated as a single source root.
    pub fn plain(root: AbsPathBuf, java_home: Option<PathBuf>) -> Self {
        let mut graph = WorkspaceGraph::default();

        let sdk = if let Some(home_path) = java_home {
            let id = SdkId(0);
            graph.sdks.insert(
                id,
                Arc::new(SdkData {
                    id,
                    name: "jdk".into(),
                    version: Default::default(),
                    home_path: AbsPathBuf::assert_utf8(home_path),
                    exploded_library_paths: Vec::new(),
                }),
            );
            Some(id)
        } else {
            None
        };

        let project = ProjectData {
            id: ProjectId(0),
            name: SmolStr::from("workspace"),
            root_path: root.clone(),
            target_sdk: sdk,
            language_level: None,
            release: None,
            source_sets: FxHashMap::from_iter([(
                SourceSetKind::Main,
                SourceSetData {
                    kind: SourceSetKind::Main,
                    source_roots: vec![root.clone()],
                    generated_source_roots: Vec::new(),
                    compile_classpath: Vec::new(),
                    runtime_classpath: Vec::new(),
                },
            )]),
        };
        graph.projects.insert(ProjectId(0), Arc::new(project));
        graph
            .source_root_to_owning_set
            .insert(root, (ProjectId(0), SourceSetKind::Main));

        graph
    }
}

// impl WorkspaceGraph {
//     /// Precisely resolves which project and which source set (`Main` or `Test`) a file belongs to.
//     /// This determines which dependencies the file can see in the LSP, and whether test framework calls are permitted.
//     pub fn resolve_source_set_for_path(
//         &self,
//         file_path: &AbsPathBuf,
//     ) -> Option<(Arc<ProjectData>, SourceSetKind)> {
//         // Walk up the directory tree to match an exactly registered Source Root
//         for ancestor in file_path.ancestors() {
//             if let Ok(abs_ancestor) = AbsPathBuf::try_from(ancestor.to_path_buf())
//                 && let Some((project_id, source_set_kind)) =
//                     self.source_root_to_owning_set.get(&abs_ancestor)
//                 && let Some(project) = self.projects.get(project_id)
//             {
//                 return Some((project.clone(), source_set_kind.clone()));
//             }
//         }
//         None
//     }
// }

#[cfg(test)]
mod tests {
    use super::JavaLanguageLevel;

    fn parsed(raw: &str) -> Option<(u8, bool)> {
        JavaLanguageLevel::parse(raw).map(|level| (level.source, level.preview))
    }

    #[test]
    fn parses_every_importer_spelling() {
        assert_eq!(parsed("8"), Some((8, false)));
        assert_eq!(parsed("1.8"), Some((8, false)));
        // A `1.8u202` distribution string keeps only the leading release.
        assert_eq!(parsed("1.8u202"), Some((8, false)));
        assert_eq!(parsed("17"), Some((17, false)));
        assert_eq!(parsed("21.0.5"), Some((21, false)));
        assert_eq!(parsed("JDK_17"), Some((17, false)));
        assert_eq!(parsed("JDK_1_8"), Some((8, false)));
        assert_eq!(parsed("JavaSE-17"), Some((17, false)));
        assert_eq!(parsed("  JavaSE-21.0.1  "), Some((21, false)));
        assert_eq!(parsed("JDK_14_PREVIEW"), Some((14, true)));
    }

    #[test]
    fn rejects_unknown_and_out_of_range_levels() {
        assert_eq!(parsed(""), None);
        assert_eq!(parsed("jdk"), None);
        // Below javac's `Source.MIN`.
        assert_eq!(parsed("1.7"), None);
        // Above `JavaLanguageLevel::MAX`.
        assert_eq!(parsed("29"), None);
    }
}
