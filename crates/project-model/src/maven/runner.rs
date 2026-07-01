use crate::maven::model::{MavenClasspathEntry, MavenWorkspace};
use crate::{
    ClasspathEntry, ProjectData, ProjectId, SdkData, SdkId, SourceSetData, SourceSetKind,
    WorkspaceGraph,
};
use ide_db::symbol::LibraryId;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;
use triomphe::Arc;
use vfs::AbsPathBuf;

pub fn import_maven_workspace(
    workspace_root: &Path,
    java_exec: &Path,
) -> anyhow::Result<MavenWorkspace> {
    let mvnw_path = if cfg!(windows) {
        workspace_root.join("mvnw.cmd")
    } else {
        workspace_root.join("mvnw")
    };

    let maven_cmd = if mvnw_path.exists() {
        mvnw_path.to_string_lossy().into_owned()
    } else {
        "mvn".to_string()
    };

    // Spin up an isolated temp file containing our model extractor
    let mut init_script = NamedTempFile::new()?;
    init_script.write_all(crate::maven::script::MAVEN_EXPORT_INIT_SCRIPT.as_bytes())?;
    init_script.flush()?;

    // Sanitize path separators for cross-platform inline evaluation inside the Maven CLI context
    let escaped_script_path = init_script.path().to_string_lossy().replace('\\', "/");
    let inline_bootstrapper = format!(
        "new GroovyShell(binding).evaluate(new File('{}'))",
        escaped_script_path
    );

    tracing::info!("Executing Maven workspace structure exploration pipeline");

    let output = Command::new(&maven_cmd)
        .env("JAVA_HOME", java_exec)
        .current_dir(workspace_root)
        // Ensure test-scope configurations are computed by driving up to test-compile phase
        .arg("test-compile")
        .arg("org.codehaus.gmavenplus:gmavenplus-plugin:3.0.0:execute")
        .arg(format!("-Dgmavenplus.script={}", inline_bootstrapper))
        // Optimize speed by skipping tests and optional downstream validation tasks
        .arg("-DskipTests=true")
        .arg("-Dmaven.test.skip=false")
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Maven build graph extraction failed:\n{}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let begin_marker = "WORKSPACE_MODEL_BEGIN";
    let end_marker = "WORKSPACE_MODEL_END";

    let json_start = stdout
        .find(begin_marker)
        .map(|idx| idx + begin_marker.len());
    let json_end = stdout.find(end_marker);

    match (json_start, json_end) {
        (Some(start), Some(end)) if start < end => {
            let json_str = stdout[start..end].trim();
            let workspace: MavenWorkspace = serde_json::from_str(json_str)?;
            Ok(workspace)
        }
        _ => {
            tracing::error!("Raw Maven Extraction Output:\n{}", stdout);
            anyhow::bail!("Failed to locate structural JSON boundaries within Maven outputs.");
        }
    }
}

pub fn build_graph_from_maven_json(workspace: MavenWorkspace) -> WorkspaceGraph {
    let mut graph = WorkspaceGraph::default();

    let mut path_to_project_id = FxHashMap::default();
    let mut jar_to_library_id = FxHashMap::default();
    let mut version_to_sdk_id = FxHashMap::default();
    let mut next_sdk_id = 0u32;

    let current_abs_dir = std::env::current_dir().unwrap_or_else(|_| {
        if cfg!(windows) {
            PathBuf::from(r"C:\")
        } else {
            PathBuf::from("/")
        }
    });
    let safe_abs_fallback = AbsPathBuf::assert_utf8(current_abs_dir);

    // Map unique multi-module coordinate strings to topology project ID tokens
    for (idx, project) in workspace.projects.iter().enumerate() {
        let project_id = ProjectId(idx as u32);
        path_to_project_id.insert(project.path.clone(), project_id);
    }

    // Convert raw deserialized data structures into native compiler memory layouts
    for project in workspace.projects {
        let project_id = *path_to_project_id.get(&project.path).unwrap();
        let abs_project_dir = AbsPathBuf::try_from(project.project_dir.clone())
            .unwrap_or_else(|_| safe_abs_fallback.clone());

        let resolved_java_home = project
            .java_home
            .and_then(|path_str| AbsPathBuf::try_from(PathBuf::from(path_str)).ok())
            .unwrap_or_else(|| safe_abs_fallback.clone());

        let target_sdk = if let Some(version) = project.java_language_version {
            let sdk_id = *version_to_sdk_id.entry(version.clone()).or_insert_with(|| {
                let id = SdkId(next_sdk_id);
                next_sdk_id += 1;

                let sdk_data = SdkData {
                    id,
                    name: SmolStr::from(format!("JDK {}", version)),
                    version: SmolStr::from(version),
                    home_path: resolved_java_home,
                    exploded_library_paths: Vec::new(),
                };
                graph.sdks.insert(id, Arc::new(sdk_data));
                id
            });
            Some(sdk_id)
        } else {
            None
        };

        let mut main_source_roots = Vec::new();
        for root in project.source_roots {
            if let Ok(abs_path) = AbsPathBuf::try_from(root) {
                main_source_roots.push(abs_path.clone());
                graph
                    .source_root_to_owning_set
                    .insert(abs_path, (project_id, SourceSetKind::Main));
            }
        }

        let mut test_source_roots = Vec::new();
        for root in project.test_roots {
            if let Ok(abs_path) = AbsPathBuf::try_from(root) {
                test_source_roots.push(abs_path.clone());
                graph
                    .source_root_to_owning_set
                    .insert(abs_path, (project_id, SourceSetKind::Test));
            }
        }

        let mut main_generated_roots = Vec::new();
        for root in project.generated_roots {
            if let Ok(abs_path) = AbsPathBuf::try_from(root) {
                main_generated_roots.push(abs_path.clone());
                graph
                    .source_root_to_owning_set
                    .insert(abs_path, (project_id, SourceSetKind::Main));
            }
        }

        let mut map_entries = |raw_entries: Vec<MavenClasspathEntry>| -> Vec<ClasspathEntry> {
            let mut entries = Vec::new();

            if let Some(sdk_id) = target_sdk {
                entries.push(ClasspathEntry::Sdk(sdk_id));
            }

            for raw_entry in raw_entries {
                match raw_entry {
                    MavenClasspathEntry::Project { path, source_set } => {
                        if let Some(&target_id) = path_to_project_id.get(&path) {
                            let set_kind = match source_set.as_str() {
                                "main" => SourceSetKind::Main,
                                "test" => SourceSetKind::Test,
                                custom => SourceSetKind::Custom(SmolStr::from(custom)),
                            };
                            entries.push(ClasspathEntry::Internal {
                                project_id: target_id,
                                source_set: set_kind,
                            });
                        }
                    }
                    MavenClasspathEntry::Jar { path } => {
                        if path.extension().is_some_and(|ext| ext == "jar") {
                            let lib_id =
                                *jar_to_library_id.entry(path.clone()).or_insert_with(|| {
                                    LibraryId::from_jar_path(&path)
                                        .expect("failed to hash jar path")
                                });

                            if let Ok(abs_jar_path) = AbsPathBuf::try_from(path) {
                                graph.library_paths.insert(lib_id, abs_jar_path);
                            }
                            entries.push(ClasspathEntry::External(lib_id));
                        }
                    }
                }
            }
            entries
        };

        let main_compile_classpath = map_entries(project.compile_classpath);

        let mut test_compile_classpath = Vec::new();
        if let Some(sdk_id) = target_sdk {
            test_compile_classpath.push(ClasspathEntry::Sdk(sdk_id));
        }
        test_compile_classpath.push(ClasspathEntry::Internal {
            project_id,
            source_set: SourceSetKind::Main,
        });
        test_compile_classpath.extend(map_entries(project.test_classpath));

        let main_source_set = SourceSetData {
            kind: SourceSetKind::Main,
            source_roots: main_source_roots,
            generated_source_roots: main_generated_roots,
            compile_classpath: main_compile_classpath.clone(),
            runtime_classpath: main_compile_classpath,
            jpms_module_name: None,
        };

        let test_source_set = SourceSetData {
            kind: SourceSetKind::Test,
            source_roots: test_source_roots,
            generated_source_roots: Vec::new(),
            compile_classpath: test_compile_classpath.clone(),
            runtime_classpath: test_compile_classpath,
            jpms_module_name: None,
        };

        let mut source_sets = FxHashMap::default();
        source_sets.insert(SourceSetKind::Main, main_source_set);
        source_sets.insert(SourceSetKind::Test, test_source_set);

        let project_data = ProjectData {
            id: project_id,
            name: SmolStr::from(project.name),
            root_path: abs_project_dir,
            target_sdk,
            source_sets,
        };

        graph.projects.insert(project_id, Arc::new(project_data));
    }

    graph
}
