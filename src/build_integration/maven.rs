use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tempfile::{Builder, NamedTempFile};
use tokio::process::Command;

use crate::index::{ClasspathId, ModuleId};

use super::detection::{BuildWatchInterest, DetectedBuildTool, DetectedBuildToolKind};
use super::model::{
    JavaToolchainInfo, ModelFidelity, ModelFreshness, SourceRootId, WorkspaceModelProvenance,
    WorkspaceModelSnapshot, WorkspaceModule, WorkspaceRoot, WorkspaceRootKind, WorkspaceSourceRoot,
};
use super::progress::ImportProgress;
use super::tool::{BuildToolImportRequest, BuildToolIntegration, BuildToolLabels};

// This implementation does NOT parse pom.xml manually. Here's why:
//
// Maven's dependency resolution is extremely complex and includes:
// - Parent POM inheritance (e.g., spring-boot-starter-parent)
// - BOM (Bill of Materials) imports
// - Dependency management sections
// - Property interpolation (${spring.version}, ${project.version}, etc.)
// - Profile activation (based on OS, JDK version, properties, etc.)
// - Transitive dependency resolution
// - Dependency exclusions and version conflicts
// - Plugin-contributed dependencies
//
// Attempting to manually parse and resolve these would require reimplementing
// Maven's entire dependency resolution engine - a massive undertaking that would
// be error-prone and incomplete.
//
// Instead, we invoke Maven itself via the Groovy Maven plugin, which gives us
// access to Maven's fully-resolved project model (session.projects). This means:
// - Maven has already resolved all parent POMs
// - Maven has already applied dependency management
// - Maven has already interpolated all properties
// - Maven has already resolved transitive dependencies
// - We get the actual JARs that Maven downloaded
//
// This is the same approach used by IntelliJ IDEA and Eclipse.
//
// If Maven execution fails, we cannot provide accurate dependency information.
// Manual POM parsing would give incorrect results for real-world projects.

const MAVEN_MODEL_BEGIN: &str = "JAVA_ANALYZER_MODEL_BEGIN";
const MAVEN_MODEL_END: &str = "JAVA_ANALYZER_MODEL_END";
const MAVEN_EXPORT_SCRIPT_LEGACY: &str = include_str!("maven/export.legacy.groovy");
const MAVEN_EXPORT_SCRIPT_MODERN: &str = include_str!("maven/export.modern.groovy");

#[derive(Debug, Clone)]
pub struct MavenVersion {
    pub raw: String,
    pub major: Option<u32>,
    pub minor: Option<u32>,
    pub patch: Option<u32>,
}

impl MavenVersion {
    fn parse(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let parts = raw
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .find(|part| part.chars().any(|ch| ch.is_ascii_digit()))
            .unwrap_or("")
            .split('.')
            .filter_map(|part| part.parse::<u32>().ok())
            .collect::<Vec<_>>();

        Self {
            raw,
            major: parts.first().copied(),
            minor: parts.get(1).copied(),
            patch: parts.get(2).copied(),
        }
    }

    pub fn major_or_default(&self, default: u32) -> u32 {
        self.major.unwrap_or(default)
    }

    pub fn supports_modern_export(&self) -> bool {
        self.major.unwrap_or(0) >= 3 && self.minor.unwrap_or(0) >= 6
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MavenExportStrategyKind {
    Legacy,
    Modern,
}

#[derive(Debug, Clone, Copy)]
pub struct MavenExportStrategy {
    pub kind: MavenExportStrategyKind,
    pub script: &'static str,
}

impl MavenExportStrategyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MavenExportStrategyKind::Legacy => "legacy-groovy-script",
            MavenExportStrategyKind::Modern => "modern-groovy-script",
        }
    }
}

impl MavenExportStrategy {
    pub fn select(version: &MavenVersion) -> Option<Self> {
        if version.supports_modern_export() {
            Some(Self {
                kind: MavenExportStrategyKind::Modern,
                script: MAVEN_EXPORT_SCRIPT_MODERN,
            })
        } else if version.major_or_default(0) >= 3 {
            Some(Self {
                kind: MavenExportStrategyKind::Legacy,
                script: MAVEN_EXPORT_SCRIPT_LEGACY,
            })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MavenVersionProbe;

impl MavenVersionProbe {
    pub async fn probe(&self, root: &Path, java_home: Option<&Path>) -> Result<MavenVersion> {
        let executable = maven_executable(root);
        tracing::debug!(
            workspace = %root.display(),
            executable = %executable.to_string_lossy(),
            configured_java_home = java_home.map(|path| path.display().to_string()),
            java_home_injected = java_home.is_some(),
            "launching Maven version probe"
        );
        let mut command = Command::new(&executable);
        command
            .current_dir(root)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_maven_java_env(&mut command, java_home)?;
        let output = command.output().await.with_context(|| {
            format!(
                "Failed to execute Maven version probe via {}\n\n\
                 Hint: Ensure Maven is installed and available in PATH, or use a Maven wrapper (mvnw).\n\
                 You can install Maven from: https://maven.apache.org/download.cgi",
                executable.to_string_lossy()
            )
        })?;

        if !output.status.success() {
            anyhow::bail!(
                "Maven version probe failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let stdout =
            String::from_utf8(output.stdout).context("Maven version output was not valid UTF-8")?;
        let raw = stdout
            .lines()
            .find_map(|line| {
                line.strip_prefix("Apache Maven ")
                    .map(|s| s.split_whitespace().next().unwrap_or("").trim())
            })
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .or_else(|| {
                stdout
                    .lines()
                    .find(|line| line.contains("Maven"))
                    .map(|line| line.trim().to_string())
            })
            .ok_or_else(|| anyhow!("could not parse Maven version from --version output"))?;

        Ok(MavenVersion::parse(raw))
    }
}

#[derive(Debug, Clone, Default)]
pub struct MavenDetector;

impl MavenDetector {
    fn watch_interest(&self) -> BuildWatchInterest {
        BuildWatchInterest {
            file_names: vec![
                "pom.xml",             // Main project descriptor
                "settings.xml",        // User/global Maven settings
                ".mvn/maven.config",   // Maven configuration
                ".mvn/jvm.config",     // JVM options for Maven
                ".mvn/extensions.xml", // Maven extensions
                "maven.config",        // Alternative location
            ],
        }
    }

    fn detect(&self, root: &Path) -> Option<DetectedBuildTool> {
        let pom_xml = root.join("pom.xml");
        if pom_xml.exists() {
            Some(DetectedBuildTool {
                kind: DetectedBuildToolKind::Maven,
                root: root.to_path_buf(),
                watch_interest: self.watch_interest(),
            })
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct MavenImportRequest {
    pub root: PathBuf,
    pub generation: u64,
    pub version: MavenVersion,
    pub strategy: MavenExportStrategy,
    pub java_home: Option<PathBuf>,
    pub client: tower_lsp::Client,
}

#[derive(Debug, Clone)]
pub struct ImportedMavenWorkspace {
    pub root: PathBuf,
    pub version: MavenVersion,
    pub export: MavenWorkspaceExport,
    pub generated_at: SystemTime,
}

#[async_trait]
pub trait WorkspaceImporter: Send + Sync {
    type Output;

    async fn import_workspace(&self, request: MavenImportRequest) -> Result<Self::Output>;
}

#[derive(Debug, Clone, Default)]
pub struct MavenImporter;

#[async_trait]
impl WorkspaceImporter for MavenImporter {
    type Output = ImportedMavenWorkspace;

    async fn import_workspace(&self, request: MavenImportRequest) -> Result<Self::Output> {
        // Start progress notification
        let progress = ImportProgress::begin(
            request.client.clone(),
            format!("java-analyzer/maven-import/{}", request.generation),
            "Importing Maven workspace",
            &format!(
                "Running Maven {} to resolve dependencies...",
                request.version.raw
            ),
        )
        .await
        .ok();

        let executable = maven_executable(&request.root);
        let script_file = write_maven_script(request.strategy)?;
        tracing::debug!(
            workspace = %request.root.display(),
            generation = request.generation,
            maven_version = %request.version.raw,
            strategy = %request.strategy.kind.as_str(),
            script_path = %script_file.path().display(),
            configured_java_home = request.java_home.as_ref().map(|path| path.display().to_string()),
            java_home_injected = request.java_home.is_some(),
            "running Maven workspace import"
        );

        if let Some(ref progress) = progress {
            progress
                .report("Executing Maven with Groovy export script...")
                .await;
        }

        let mut command = Command::new(&executable);
        command
            .current_dir(&request.root)
            .env(
                "JAVA_ANALYZER_MAVEN_DEBUG",
                if tracing::enabled!(tracing::Level::DEBUG) {
                    "1"
                } else {
                    "0"
                },
            )
            .arg("-q")
            .arg("org.codehaus.gmaven:groovy-maven-plugin:2.1.1:execute")
            .arg(format!("-Dsource={}", script_file.path().display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_maven_java_env(&mut command, request.java_home.as_deref())?;
        let output = command.output().await.with_context(|| {
            format!(
                "failed to execute Maven importer via {}",
                executable.to_string_lossy()
            )
        })?;

        if !output.status.success() {
            if let Some(progress) = progress {
                progress.finish("Maven import failed").await;
            }

            let stderr = String::from_utf8_lossy(&output.stderr);
            let error_msg = stderr.trim();

            // Provide helpful error messages
            let hint = if error_msg.contains("groovy-maven-plugin") {
                "\n\nHint: The Groovy Maven plugin is required for workspace import. \
                 Please ensure you have internet connectivity to download it, or run 'mvn dependency:resolve' first."
            } else if error_msg.contains("JAVA_HOME") {
                "\n\nHint: Maven requires JAVA_HOME to be set. Please configure your Java installation."
            } else if error_msg.contains("pom.xml") {
                "\n\nHint: Maven could not parse your pom.xml. Please ensure it is valid by running 'mvn validate'."
            } else {
                "\n\nHint: Try running 'mvn clean compile' manually to diagnose the issue."
            };

            bail!(
                "Maven import failed: {}{}\n\nNote: java-analyzer requires Maven to successfully resolve dependencies. \
                 Manual POM parsing is not supported as it cannot handle parent POMs, BOMs, property interpolation, \
                 or transitive dependencies.",
                error_msg,
                hint
            );
        }

        if let Some(ref progress) = progress {
            progress.report("Parsing Maven output...").await;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            tracing::debug!(
                workspace = %request.root.display(),
                stderr = %stderr.trim(),
                "Maven importer debug stderr"
            );
        }

        let stdout = String::from_utf8(output.stdout)
            .context("Maven importer output was not valid UTF-8")?;
        let json = extract_model_json(&stdout)?;
        let export = serde_json::from_str::<MavenWorkspaceExport>(&json)
            .context("failed to parse Maven workspace export")?;

        if let Some(ref progress) = progress {
            progress
                .report(&format!("Found {} Maven module(s)", export.projects.len()))
                .await;
        }

        tracing::debug!(
            workspace = %request.root.display(),
            projects = export.projects.len(),
            maven_version = %request.version.raw,
            strategy = %request.strategy.kind.as_str(),
            "Maven importer produced workspace export"
        );
        for project in &export.projects {
            tracing::info!(
                project = %project.path,
                name = %project.name,
                source_roots = ?project.source_roots,
                test_roots = ?project.test_roots,
                compile_classpath_count = project.compile_classpath.len(),
                test_classpath_count = project.test_classpath.len(),
                compile_classpath = ?project.compile_classpath,
                test_classpath = ?project.test_classpath,
                module_dependencies = ?project.module_dependencies,
                "raw Maven import payload"
            );
        }

        if let Some(progress) = progress {
            progress.finish("Maven import completed successfully").await;
        }

        Ok(ImportedMavenWorkspace {
            root: request.root,
            version: request.version,
            export,
            generated_at: SystemTime::now(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MavenWorkspaceExport {
    pub workspace_name: String,
    pub projects: Vec<MavenProjectExport>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MavenProjectExport {
    pub path: String,
    pub name: String,
    pub project_dir: PathBuf,
    pub source_roots: Vec<PathBuf>,
    pub test_roots: Vec<PathBuf>,
    pub resource_roots: Vec<PathBuf>,
    pub generated_roots: Vec<PathBuf>,
    pub compile_classpath: Vec<PathBuf>,
    pub test_classpath: Vec<PathBuf>,
    pub module_dependencies: Vec<String>,
    pub java_language_version: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MavenWorkspaceNormalizer;

impl MavenWorkspaceNormalizer {
    pub fn normalize(
        &self,
        imported: ImportedMavenWorkspace,
        generation: u64,
    ) -> Result<WorkspaceModelSnapshot> {
        tracing::debug!(
            workspace = %imported.root.display(),
            projects = imported.export.projects.len(),
            maven_version = %imported.version.raw,
            "normalizing imported Maven workspace"
        );
        let module_ids: BTreeMap<Arc<str>, ModuleId> = imported
            .export
            .projects
            .iter()
            .enumerate()
            .map(|(idx, project)| {
                (
                    Arc::<str>::from(project.path.as_str()),
                    ModuleId(idx as u32 + 1),
                )
            })
            .collect();

        let mut fidelity = ModelFidelity::Full;
        let modules = imported
            .export
            .projects
            .iter()
            .enumerate()
            .map(|(module_idx, project)| {
                if project.compile_classpath.is_empty() {
                    tracing::warn!(
                        project = %project.path,
                        "compile classpath is empty; marking fidelity as Partial"
                    );
                    fidelity = ModelFidelity::Partial;
                }

                let mut source_root_id_counter = (module_idx as u32) * 1000;
                let mut roots = Vec::new();

                for source_root in &project.source_roots {
                    source_root_id_counter += 1;
                    roots.push(WorkspaceSourceRoot {
                        id: SourceRootId(source_root_id_counter),
                        path: source_root.clone(),
                        kind: WorkspaceRootKind::Sources,
                        classpath: ClasspathId::Main,
                    });
                }

                for test_root in &project.test_roots {
                    source_root_id_counter += 1;
                    roots.push(WorkspaceSourceRoot {
                        id: SourceRootId(source_root_id_counter),
                        path: test_root.clone(),
                        kind: WorkspaceRootKind::Tests,
                        classpath: ClasspathId::Test,
                    });
                }

                for resource_root in &project.resource_roots {
                    source_root_id_counter += 1;
                    roots.push(WorkspaceSourceRoot {
                        id: SourceRootId(source_root_id_counter),
                        path: resource_root.clone(),
                        kind: WorkspaceRootKind::Resources,
                        classpath: ClasspathId::Main,
                    });
                }

                for generated_root in &project.generated_roots {
                    source_root_id_counter += 1;
                    roots.push(WorkspaceSourceRoot {
                        id: SourceRootId(source_root_id_counter),
                        path: generated_root.clone(),
                        kind: WorkspaceRootKind::Generated,
                        classpath: ClasspathId::Main,
                    });
                }

                let dependency_modules = project
                    .module_dependencies
                    .iter()
                    .filter_map(|dep_path| module_ids.get(dep_path.as_str()).copied())
                    .collect();

                WorkspaceModule {
                    id: ModuleId(module_idx as u32 + 1),
                    name: project.name.clone(),
                    directory: project.project_dir.clone(),
                    roots,
                    compile_classpath: project.compile_classpath.clone(),
                    test_classpath: project.test_classpath.clone(),
                    dependency_modules,
                    java: JavaToolchainInfo {
                        language_version: project.java_language_version.clone(),
                    },
                }
            })
            .collect();

        Ok(WorkspaceModelSnapshot {
            generation,
            root: WorkspaceRoot {
                path: imported.root.clone(),
            },
            name: imported.export.workspace_name,
            modules,
            provenance: WorkspaceModelProvenance {
                tool: DetectedBuildToolKind::Maven,
                tool_version: Some(imported.version.raw),
                imported_at: imported.generated_at,
            },
            freshness: ModelFreshness::Fresh,
            fidelity,
        })
    }
}

#[derive(Clone, Default)]
pub struct MavenIntegration {
    detector: MavenDetector,
    version_probe: MavenVersionProbe,
    importer: MavenImporter,
    normalizer: MavenWorkspaceNormalizer,
}

#[async_trait]
impl BuildToolIntegration for MavenIntegration {
    fn detect(&self, root: &Path) -> Option<DetectedBuildTool> {
        self.detector.detect(root)
    }

    fn watch_interest(&self) -> BuildWatchInterest {
        self.detector.watch_interest()
    }

    fn labels(&self) -> BuildToolLabels {
        BuildToolLabels {
            importing_workspace: "Importing Maven workspace...",
        }
    }

    async fn import_workspace(
        &self,
        request: BuildToolImportRequest,
    ) -> Result<WorkspaceModelSnapshot> {
        let version = self
            .version_probe
            .probe(&request.root, request.java_home.as_deref())
            .await?;

        let strategy = MavenExportStrategy::select(&version)
            .ok_or_else(|| anyhow!("Unsupported Maven version: {}", version.raw))?;

        let imported = self
            .importer
            .import_workspace(MavenImportRequest {
                root: request.root.clone(),
                generation: request.generation,
                version,
                strategy,
                java_home: request.java_home,
                client: request.client,
            })
            .await?;

        self.normalizer.normalize(imported, request.generation)
    }
}

fn maven_executable(root: &Path) -> PathBuf {
    // 1. Check for Maven wrapper: ./mvnw (Unix) or mvnw.cmd (Windows)
    let wrapper_unix = root.join("mvnw");
    if wrapper_unix.exists() {
        return wrapper_unix;
    }

    let wrapper_win = root.join("mvnw.cmd");
    if cfg!(windows) && wrapper_win.exists() {
        return wrapper_win;
    }

    // 2. Check MAVEN_HOME environment variable
    if let Ok(maven_home) = std::env::var("MAVEN_HOME") {
        let mvn = PathBuf::from(maven_home).join("bin").join("mvn");
        if mvn.exists() {
            return mvn;
        }
    }

    // 3. Fall back to PATH
    PathBuf::from(if cfg!(windows) { "mvn.cmd" } else { "mvn" })
}

fn configure_maven_java_env(command: &mut Command, java_home: Option<&Path>) -> Result<()> {
    if let Some(java_home) = java_home {
        command.env("JAVA_HOME", java_home);
    }
    Ok(())
}

fn write_maven_script(strategy: MavenExportStrategy) -> Result<NamedTempFile> {
    let mut file = Builder::new()
        .prefix("java-analyzer-maven-export-")
        .suffix(".groovy")
        .tempfile()
        .context("failed to create temporary Maven export script")?;

    use std::io::Write;
    file.write_all(strategy.script.as_bytes())
        .context("failed to write Maven export script")?;
    file.flush()
        .context("failed to flush Maven export script")?;

    Ok(file)
}

fn extract_model_json(output: &str) -> Result<String> {
    let begin = output
        .find(MAVEN_MODEL_BEGIN)
        .ok_or_else(|| anyhow!("Maven export did not produce model begin marker"))?;
    let end = output
        .find(MAVEN_MODEL_END)
        .ok_or_else(|| anyhow!("Maven export did not produce model end marker"))?;

    if begin >= end {
        bail!("Maven export markers are in wrong order");
    }

    let json_start = begin + MAVEN_MODEL_BEGIN.len();
    let json = &output[json_start..end];

    Ok(json.trim().to_string())
}
