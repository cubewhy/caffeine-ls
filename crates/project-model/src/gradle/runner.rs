use crate::gradle::model::{GradleClasspathEntry, GradleWorkspace};
use crate::{
    ClasspathEntry, Library, ProjectData, ProjectId, SdkData, SdkId, SourceSetData, SourceSetKind,
    SyncError, WorkspaceGraph,
};
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;
use triomphe::Arc;
use vfs::AbsPathBuf;

pub fn import_gradle_workspace(
    workspace_root: &Path,
    java_home: &Path,
    log_file: Option<&Path>,
    on_output: &mut (dyn FnMut(String) + Send),
) -> anyhow::Result<GradleWorkspace> {
    let gradlew_path = if cfg!(windows) {
        workspace_root.join("gradlew.bat")
    } else {
        workspace_root.join("gradlew")
    };

    let gradle_cmd = if gradlew_path.exists() {
        gradlew_path.to_string_lossy().into_owned()
    } else {
        "gradle".to_string()
    };

    let selected_script = crate::gradle::script::GRADLE_INIT_SCRIPT;

    let mut init_script = NamedTempFile::new()?;
    init_script.write_all(selected_script.as_bytes())?;
    init_script.flush()?;

    let mut command = Command::new(&gradle_cmd);
    command
        .env("JAVA_HOME", java_home)
        .current_dir(workspace_root)
        .arg("--init-script")
        .arg(init_script.path())
        .arg("exportWorkspaceModel");

    let outcome = crate::run_command_streaming(&mut command, log_file, on_output)?;

    if !outcome.status.success() {
        return Err(SyncError {
            message: format!(
                "Gradle execution failed (exit code {})",
                outcome.status.code().unwrap_or(-1)
            ),
            tail: outcome.tail,
        }
        .into());
    }

    if outcome.model_truncated {
        return Err(SyncError {
            message: "Gradle workspace model JSON exceeded the size limit".to_string(),
            tail: outcome.tail,
        }
        .into());
    }

    match outcome.model_json {
        Some(json_str) => {
            let workspace: GradleWorkspace = serde_json::from_str(json_str.trim())?;
            Ok(workspace)
        }
        None => Err(SyncError {
            message: "Failed to locate structural JSON markers in Gradle output.".to_string(),
            tail: outcome.tail,
        }
        .into()),
    }
}

pub fn build_graph_from_json(workspace: GradleWorkspace) -> WorkspaceGraph {
    let mut graph = WorkspaceGraph::default();

    let mut path_to_project_id = FxHashMap::default();
    let mut jar_to_library_id = FxHashMap::default();
    let mut version_to_sdk_id = FxHashMap::default();
    let mut next_sdk_id = 0u32;

    let abs_workspace_root = workspace
        .projects
        .iter()
        .find(|p| p.path == ":")
        .map(|p| AbsPathBuf::assert_utf8(p.project_dir.clone()))
        .unwrap_or_else(|| {
            workspace
                .projects
                .first()
                .map(|p| AbsPathBuf::assert_utf8(p.project_dir.clone()))
                .unwrap_or_else(|| {
                    AbsPathBuf::assert_utf8(std::env::current_dir().unwrap_or_default())
                })
        });

    // Allocate topology project tokens
    for (idx, project) in workspace.projects.iter().enumerate() {
        let project_id = ProjectId(idx as u32);
        path_to_project_id.insert(project.path.clone(), project_id);
    }

    // Structural translation preserving chronological classpath sorting
    for project in workspace.projects {
        let project_id = *path_to_project_id.get(&project.path).unwrap();
        let abs_project_dir = AbsPathBuf::assert_utf8(project.project_dir.clone());

        let resolved_java_home = project
            .java_home
            .map(|path_str| AbsPathBuf::assert_utf8(PathBuf::from(path_str)));

        let target_sdk = if let Some(version) = project.java_language_version
            && let Some(sdk_home_path) = resolved_java_home
        {
            let sdk_id = *version_to_sdk_id.entry(version.clone()).or_insert_with(|| {
                let id = SdkId(next_sdk_id);
                next_sdk_id += 1;

                let sdk_data = SdkData {
                    id,
                    name: SmolStr::from(format!("JDK {}", version)),
                    version: SmolStr::from(version),
                    home_path: sdk_home_path,
                    exploded_library_paths: Vec::new(),
                };
                graph.sdks.insert(id, Arc::new(sdk_data));
                id
            });
            Some(sdk_id)
        } else {
            tracing::error!("Failed to receive SDK version from Gradle");
            None
        };

        let mut main_source_roots = Vec::new();
        for root in project.source_roots {
            let abs_path = AbsPathBuf::assert_utf8(root);
            main_source_roots.push(abs_path.clone());
            graph
                .source_root_to_owning_set
                .insert(abs_path, (project_id, SourceSetKind::Main));
        }

        let mut test_source_roots = Vec::new();
        for root in project.test_roots {
            let abs_path = AbsPathBuf::assert_utf8(root);
            test_source_roots.push(abs_path.clone());
            graph
                .source_root_to_owning_set
                .insert(abs_path, (project_id, SourceSetKind::Test));
        }

        let mut main_generated_roots = Vec::new();
        for root in project.generated_roots {
            let abs_path = AbsPathBuf::assert_utf8(root);
            main_generated_roots.push(abs_path.clone());
            graph
                .source_root_to_owning_set
                .insert(abs_path, (project_id, SourceSetKind::Main));
        }

        // Shared closure mappings that maintain original list sequence
        let mut map_entries = |raw_entries: Vec<GradleClasspathEntry>| -> Vec<ClasspathEntry> {
            let mut entries = Vec::new();

            if let Some(sdk_id) = target_sdk {
                entries.push(ClasspathEntry::Sdk(sdk_id));
            }

            for raw_entry in raw_entries {
                match raw_entry {
                    GradleClasspathEntry::Project { path, source_set } => {
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
                    GradleClasspathEntry::Jar { path, origin } => {
                        if path.extension().is_some_and(|ext| ext == "jar") {
                            let lib_id =
                                *jar_to_library_id.entry(path.clone()).or_insert_with(|| {
                                    crate::LibraryId::from_file_path(&path)
                                        .expect("failed to hash jar path")
                                });

                            let abs_jar_path = AbsPathBuf::assert_utf8(path);
                            let library = if origin == "coordinate" {
                                Library::readonly(lib_id, abs_jar_path)
                            } else if abs_jar_path.starts_with(&abs_workspace_root) {
                                Library::editable(lib_id, abs_jar_path)
                            } else {
                                Library::readonly(lib_id, abs_jar_path)
                            };
                            graph.library_paths.insert(lib_id, library);
                            entries.push(ClasspathEntry::External(lib_id));
                        }
                    }
                }
            }
            entries
        };

        let main_compile_classpath = map_entries(project.compile_classpath);

        // Setup separate test compile entries ensuring module isolation
        let mut test_compile_classpath = Vec::new();
        if let Some(sdk_id) = target_sdk {
            test_compile_classpath.push(ClasspathEntry::Sdk(sdk_id));
        }

        // Force test contexts to look inside their paired production counterpart first
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
