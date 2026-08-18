use crate::maven::model::{MavenClasspathEntry, MavenWorkspace};
use crate::maven::sidecar::{self, SIDECAR_ARTIFACT, SIDECAR_GROUP, SIDECAR_VERSION};
use crate::{
    ClasspathEntry, CommandOutcome, Library, ProjectData, ProjectId, SdkData, SdkId, SourceSetData,
    SourceSetKind, SyncError, WorkspaceGraph,
};
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use triomphe::Arc;
use vfs::AbsPathBuf;

pub fn import_maven_workspace(
    workspace_root: &Path,
    java_exec: &Path,
    log_file: Option<&Path>,
    on_output: &mut (dyn FnMut(String) + Send),
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

    // Extract the embedded sidecar plugin to a tempdir and expose it to Maven
    // as a file:// plugin repository, so the plugin resolves without touching
    // the user's local repository.
    let sidecar_dir = sidecar::ensure_extracted()?;
    let settings_file = sidecar_dir.join("settings.xml");

    tracing::info!("Executing Maven workspace structure exploration pipeline");

    let goal = format!("{SIDECAR_GROUP}:{SIDECAR_ARTIFACT}:{SIDECAR_VERSION}:export-model");

    let build_command = |args: &[&str]| {
        let mut command = Command::new(&maven_cmd);
        command
            .env("JAVA_HOME", java_exec)
            .current_dir(workspace_root)
            .arg("-s")
            .arg(&settings_file)
            .args(args)
            .arg(&goal)
            .arg("-DskipTests=true")
            .arg("-Dmaven.test.skip=false");
        command
    };

    // The primary run compiles the reactor so that inter-module dependencies
    // resolve. Projects with compile errors fail here; retry without the
    // lifecycle phase, because the model only needs the resolved project
    // metadata, not compiled output.
    let mut command = build_command(&["test-compile"]);
    let outcome = crate::run_command_streaming(&mut command, log_file, on_output)?;
    if outcome.status.success() {
        return maven_workspace_from_outcome(outcome);
    }

    let primary_failure = outcome;
    tracing::warn!(
        "Maven test-compile failed (exit code {}); retrying the export goal without compilation",
        primary_failure.status.code().unwrap_or(-1)
    );
    let mut command = build_command(&[]);
    let outcome = crate::run_command_streaming(&mut command, log_file, on_output)?;
    if !outcome.status.success() {
        return Err(SyncError {
            message: format!(
                "Maven build graph extraction failed (exit code {})",
                primary_failure.status.code().unwrap_or(-1)
            ),
            tail: primary_failure.tail,
        }
        .into());
    }

    maven_workspace_from_outcome(outcome)
}

fn maven_workspace_from_outcome(outcome: CommandOutcome) -> anyhow::Result<MavenWorkspace> {
    if outcome.model_truncated {
        return Err(SyncError {
            message: "Maven workspace model JSON exceeded the size limit".to_string(),
            tail: outcome.tail,
        }
        .into());
    }

    match outcome.model_json {
        Some(json_str) => {
            let workspace: MavenWorkspace = serde_json::from_str(json_str.trim())?;
            Ok(workspace)
        }
        None => Err(SyncError {
            message: "Failed to locate structural JSON boundaries within Maven outputs."
                .to_string(),
            tail: outcome.tail,
        }
        .into()),
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

    let abs_workspace_root = workspace
        .projects
        .iter()
        .map(|p| AbsPathBuf::assert_utf8(p.project_dir.clone()))
        .min_by_key(|p| p.as_str().len())
        .unwrap_or_else(|| safe_abs_fallback.clone());

    // Map unique multi-module coordinate strings to topology project ID tokens
    for (idx, project) in workspace.projects.iter().enumerate() {
        let project_id = ProjectId(idx as u32);
        path_to_project_id.insert(project.path.clone(), project_id);
    }

    // Convert raw deserialized data structures into native compiler memory layouts
    for project in workspace.projects {
        let project_id = *path_to_project_id.get(&project.path).unwrap();
        let abs_project_dir = AbsPathBuf::assert_utf8(project.project_dir.clone());

        let resolved_java_home = project
            .java_home
            .map(|path_str| AbsPathBuf::assert_utf8(PathBuf::from(path_str)))
            .unwrap();

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
                    MavenClasspathEntry::Jar { path, origin } => {
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
