use crate::eclipse::model::{EclipseClasspath, EclipseProjectDescription};
use crate::{
    ClasspathEntry, ProjectData, ProjectId, SdkData, SdkId, SourceSetData, SourceSetKind,
    WorkspaceGraph,
};
use ide_db::symbol::LibraryId;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use std::fs;
use std::path::{Path, PathBuf};
use triomphe::Arc;
use vfs::AbsPathBuf;

pub fn import_eclipse_workspace(
    workspace_root: &Path,
) -> anyhow::Result<(String, EclipseClasspath)> {
    let project_file_path = workspace_root.join(".project");
    let classpath_file_path = workspace_root.join(".classpath");

    if !project_file_path.exists() || !classpath_file_path.exists() {
        anyhow::bail!(
            "Missing required Eclipse descriptor metadata files (.project or .classpath)"
        );
    }

    let project_xml = fs::read_to_string(project_file_path)?;
    let project_desc: EclipseProjectDescription = quick_xml::de::from_str(&project_xml)?;

    let classpath_xml = fs::read_to_string(classpath_file_path)?;
    let classpath: EclipseClasspath = quick_xml::de::from_str(&classpath_xml)?;

    Ok((project_desc.name, classpath))
}

pub fn build_graph_from_eclipse(
    workspace_root: &Path,
    project_name: &str,
    classpath: EclipseClasspath,
    java_home: &Path,
) -> WorkspaceGraph {
    let mut graph = WorkspaceGraph::default();

    let project_id = ProjectId(0);
    let mut jar_to_library_id = FxHashMap::default();

    let abs_workspace_root =
        AbsPathBuf::try_from(workspace_root.to_path_buf()).unwrap_or_else(|_| {
            AbsPathBuf::assert_utf8(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
        });

    let resolved_java_home = AbsPathBuf::try_from(java_home.to_path_buf())
        .unwrap_or_else(|_| abs_workspace_root.clone());

    let mut source_roots: Vec<AbsPathBuf> = Vec::new();
    let mut test_roots: Vec<AbsPathBuf> = Vec::new();
    let mut main_compile_classpath = Vec::new();
    let mut java_version = String::from("17");

    for entry in &classpath.entries {
        if entry.kind == "con"
            && entry
                .path
                .contains("org.eclipse.jdt.launching.JRE_CONTAINER")
            && let Some(idx) = entry.path.rfind('/')
        {
            let ver_str = &entry.path[idx + 1..];
            java_version = ver_str.replace("JavaSE-", "");
            if java_version.starts_with("1.") {
                java_version = java_version[2..].to_string();
            }
        }
    }

    let sdk_id = SdkId(0);
    let sdk_data = SdkData {
        id: sdk_id,
        name: SmolStr::from(format!("JDK {}", java_version)),
        version: SmolStr::from(java_version),
        home_path: resolved_java_home,
        exploded_library_paths: Vec::new(),
    };
    graph.sdks.insert(sdk_id, Arc::new(sdk_data));
    main_compile_classpath.push(ClasspathEntry::Sdk(sdk_id));

    for entry in classpath.entries {
        match entry.kind.as_str() {
            "src" => {
                if entry.path.starts_with('/') {
                    main_compile_classpath.push(ClasspathEntry::Internal {
                        project_id,
                        source_set: SourceSetKind::Main,
                    });
                } else {
                    let resolved_path = workspace_root.join(&entry.path);
                    let abs_src_path = AbsPathBuf::assert_utf8(resolved_path);

                    if entry.path.to_lowercase().contains("test") {
                        test_roots.push(abs_src_path.clone());
                        graph
                            .source_root_to_owning_set
                            .insert(abs_src_path, (project_id, SourceSetKind::Test));
                    } else {
                        source_roots.push(abs_src_path.clone());
                        graph
                            .source_root_to_owning_set
                            .insert(abs_src_path, (project_id, SourceSetKind::Main));
                    }
                }
            }
            "lib" => {
                let jar_path = PathBuf::from(&entry.path);
                let target_path = if jar_path.is_absolute() {
                    jar_path
                } else {
                    workspace_root.join(jar_path)
                };

                if target_path.extension().is_some_and(|ext| ext == "jar") {
                    let lib_id =
                        *jar_to_library_id
                            .entry(target_path.clone())
                            .or_insert_with(|| {
                                LibraryId::from_jar_path(&target_path)
                                    .expect("failed to hash jar path")
                            });

                    if let Ok(abs_jar_path) = AbsPathBuf::try_from(target_path) {
                        graph.library_paths.insert(lib_id, abs_jar_path);
                    }
                    main_compile_classpath.push(ClasspathEntry::External(lib_id));
                }
            }
            _ => {}
        }
    }

    let mut test_compile_classpath = vec![ClasspathEntry::Sdk(sdk_id)];
    test_compile_classpath.push(ClasspathEntry::Internal {
        project_id,
        source_set: SourceSetKind::Main,
    });
    test_compile_classpath.extend(main_compile_classpath.clone());

    let main_source_set = SourceSetData {
        kind: SourceSetKind::Main,
        source_roots,
        generated_source_roots: Vec::new(),
        compile_classpath: main_compile_classpath.clone(),
        runtime_classpath: main_compile_classpath,
        jpms_module_name: None,
    };

    let test_source_set = SourceSetData {
        kind: SourceSetKind::Test,
        source_roots: test_roots,
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
        name: SmolStr::from(project_name),
        root_path: abs_workspace_root,
        target_sdk: Some(sdk_id),
        source_sets,
    };

    graph.projects.insert(project_id, Arc::new(project_data));
    graph
}
