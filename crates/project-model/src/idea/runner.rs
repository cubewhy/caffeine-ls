use crate::idea::model::{
    IdeaJarDirectory, IdeaLibraryClasses, IdeaLibraryTableXml, IdeaMiscXml, IdeaModuleDoc,
    IdeaModulesXml,
};
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

fn resolve_idea_url(url: &str, workspace_root: &Path, module_dir: &Path) -> Option<PathBuf> {
    let clean_url = url
        .strip_prefix("file://")
        .or_else(|| url.strip_prefix("jar://"))
        .unwrap_or(url);

    let expanded = if clean_url.contains("$MODULE_DIR$") {
        clean_url.replace("$MODULE_DIR$", &module_dir.to_string_lossy())
    } else if clean_url.contains("$PROJECT_DIR$") {
        clean_url.replace("$PROJECT_DIR$", &workspace_root.to_string_lossy())
    } else {
        clean_url.to_string()
    };

    let final_path = expanded.split("!/").next().unwrap_or(&expanded);
    Some(PathBuf::from(final_path))
}

fn scan_dir_for_jars(dir: &Path, recursive: bool, jar_paths: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && recursive {
                scan_dir_for_jars(&path, recursive, jar_paths);
            } else if path.extension().is_some_and(|ext| ext == "jar") {
                jar_paths.push(path);
            }
        }
    }
}

fn extract_jars_from_idea_library(
    classes: Option<&IdeaLibraryClasses>,
    jar_dirs: &[IdeaJarDirectory],
    workspace_root: &Path,
    base_dir: &Path,
) -> Vec<PathBuf> {
    let mut jar_paths = Vec::new();
    if let Some(classes) = classes {
        for root in &classes.roots {
            if let Some(resolved) = resolve_idea_url(&root.url, workspace_root, base_dir)
                && resolved.extension().is_some_and(|ext| ext == "jar")
            {
                jar_paths.push(resolved);
            }
        }
    }
    for jar_dir in jar_dirs {
        if let Some(resolved_dir) = resolve_idea_url(&jar_dir.url, workspace_root, base_dir) {
            let recursive = jar_dir.recursive.as_deref() == Some("true");
            scan_dir_for_jars(&resolved_dir, recursive, &mut jar_paths);
        }
    }
    jar_paths
}

/// New: Reads .idea/misc.xml to find the central <component name="ProjectRootManager"> configuration block
fn probe_project_jdk_version(workspace_root: &Path) -> String {
    let misc_xml_path = workspace_root.join(".idea").join("misc.xml");
    if misc_xml_path.exists()
        && let Ok(content) = fs::read_to_string(misc_xml_path)
        && let Ok(misc_doc) = quick_xml::de::from_str::<IdeaMiscXml>(&content)
    {
        for component in misc_doc.components {
            if component.name == "ProjectRootManager"
                && let Some(raw_ver) = component.language_level
            {
                // Strip "JDK_" text wrappers
                let mut clean_ver = raw_ver.replace("JDK_", "");
                if clean_ver.starts_with("1.") {
                    clean_ver = clean_ver[2..].to_string(); // "1.8" -> "8"
                }
                return clean_ver;
            }
        }
    }
    String::from("17") // Safe historical baseline fallback target configuration
}

fn load_project_level_libraries(workspace_root: &Path) -> FxHashMap<String, Vec<PathBuf>> {
    let mut registry = FxHashMap::default();
    let libraries_dir = workspace_root.join(".idea").join("libraries");

    if let Ok(entries) = fs::read_dir(libraries_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "xml")
                && let Ok(xml_content) = fs::read_to_string(&path)
                && let Ok(table_doc) = quick_xml::de::from_str::<IdeaLibraryTableXml>(&xml_content)
            {
                for lib in table_doc.libraries {
                    let jar_paths = extract_jars_from_idea_library(
                        lib.classes.as_ref(),
                        &lib.jar_directories,
                        workspace_root,
                        workspace_root,
                    );
                    registry.insert(lib.name, jar_paths);
                }
            }
        }
    }
    registry
}

pub fn import_idea_workspace(
    workspace_root: &Path,
) -> anyhow::Result<Vec<(PathBuf, IdeaModuleDoc)>> {
    let mut modules_metadata = Vec::new();
    let idea_dir = workspace_root.join(".idea");
    let modules_xml_path = idea_dir.join("modules.xml");

    if modules_xml_path.exists() {
        let xml_content = fs::read_to_string(&modules_xml_path)?;
        let modules_doc: IdeaModulesXml = quick_xml::de::from_str(&xml_content)?;

        // Safe scanning across nested components list arrays
        if let Some(manager_comp) = modules_doc
            .components
            .iter()
            .find(|c| c.name == "ProjectModuleManager")
            && let Some(ref modules_list) = manager_comp.modules
        {
            for module_ref in &modules_list.items {
                let relative_iml_path = module_ref
                    .file_path
                    .replace("$PROJECT_DIR$", ".")
                    .replace("$MODULE_DIR$", ".");

                let iml_path = workspace_root.join(relative_iml_path);
                if iml_path.exists() {
                    let iml_content = fs::read_to_string(&iml_path)?;
                    let module_doc: IdeaModuleDoc = quick_xml::de::from_str(&iml_content)?;
                    modules_metadata.push((iml_path, module_doc));
                }
            }
        }
    } else {
        for entry in fs::read_dir(workspace_root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "iml") {
                let iml_content = fs::read_to_string(&path)?;
                let module_doc: IdeaModuleDoc = quick_xml::de::from_str(&iml_content)?;
                modules_metadata.push((path, module_doc));
                break;
            }
        }
    }

    if modules_metadata.is_empty() {
        anyhow::bail!("No structural IntelliJ IDEA module descriptors (.iml) found");
    }

    Ok(modules_metadata)
}

pub fn build_graph_from_idea(
    workspace_root: &Path,
    modules: Vec<(PathBuf, IdeaModuleDoc)>,
    java_home: &Path,
) -> WorkspaceGraph {
    let mut graph = WorkspaceGraph::default();

    let mut module_name_to_project_id = FxHashMap::default();
    let mut jar_to_library_id = FxHashMap::default();

    let abs_workspace_root = AbsPathBuf::try_from(workspace_root.to_path_buf())
        .unwrap_or_else(|_| AbsPathBuf::assert_utf8(std::env::current_dir().unwrap_or_default()));

    let resolved_java_home = AbsPathBuf::try_from(java_home.to_path_buf())
        .unwrap_or_else(|_| abs_workspace_root.clone());

    let global_project_libraries = load_project_level_libraries(workspace_root);

    // Resolve accurate version signatures dynamically using misc.xml settings
    let project_jdk_version = probe_project_jdk_version(workspace_root);

    for (idx, (iml_path, _)) in modules.iter().enumerate() {
        if let Some(stem) = iml_path.file_stem().and_then(|s| s.to_str()) {
            let project_id = ProjectId(idx as u32);
            module_name_to_project_id.insert(String::from(stem), project_id);
        }
    }

    let sdk_id = SdkId(0);
    let sdk_data = SdkData {
        id: sdk_id,
        name: SmolStr::from(format!("JDK {}", project_jdk_version)),
        version: SmolStr::from(project_jdk_version),
        home_path: resolved_java_home,
        exploded_library_paths: Vec::new(),
    };
    graph.sdks.insert(sdk_id, Arc::new(sdk_data));

    for (iml_path, doc) in modules {
        let module_name = iml_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let project_id = *module_name_to_project_id.get(module_name).unwrap();

        let module_dir = iml_path.parent().unwrap_or(workspace_root);
        let abs_project_dir = AbsPathBuf::assert_utf8(module_dir.to_path_buf());

        let mut source_roots = Vec::new();
        let mut test_roots = Vec::new();
        let mut generated_roots = Vec::new();
        let mut explicit_jar_entries = Vec::new();
        let mut internal_project_dependencies = Vec::new();

        for component in doc.components {
            if component.name == "NewModuleRootManager" {
                for content in component.contents {
                    for src_folder in content.source_folders {
                        if let Some(resolved_path) =
                            resolve_idea_url(&src_folder.url, workspace_root, module_dir)
                        {
                            let abs_src_path = AbsPathBuf::assert_utf8(resolved_path);

                            if src_folder.generated.as_deref() == Some("true") {
                                generated_roots.push(abs_src_path.clone());
                                graph
                                    .source_root_to_owning_set
                                    .insert(abs_src_path, (project_id, SourceSetKind::Main));
                            } else if src_folder.is_test_source.as_deref() == Some("true") {
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
                }

                for order_entry in component.order_entries {
                    match order_entry.entry_type.as_str() {
                        "module" => {
                            if let Some(dep_name) = order_entry.module_name
                                && let Some(&target_pid) = module_name_to_project_id.get(&dep_name)
                            {
                                internal_project_dependencies.push(ClasspathEntry::Internal {
                                    project_id: target_pid,
                                    source_set: SourceSetKind::Main,
                                });
                            }
                        }
                        "library" => {
                            if let Some(lib_name) = order_entry.name
                                && let Some(jar_paths) = global_project_libraries.get(&lib_name)
                            {
                                for jar_path in jar_paths {
                                    let lib_id = *jar_to_library_id
                                        .entry(jar_path.clone())
                                        .or_insert_with(|| {
                                            LibraryId::from_jar_path(jar_path)
                                                .expect("failed to hash jar path")
                                        });
                                    if let Ok(abs_jar_path) = AbsPathBuf::try_from(jar_path.clone())
                                    {
                                        graph.library_paths.insert(lib_id, abs_jar_path);
                                    }
                                    explicit_jar_entries.push(ClasspathEntry::External(lib_id));
                                }
                            }
                        }
                        "module-library" => {
                            if let Some(lib) = order_entry.inline_library {
                                let jar_paths = extract_jars_from_idea_library(
                                    lib.classes.as_ref(),
                                    &lib.jar_directories,
                                    workspace_root,
                                    module_dir,
                                );
                                for jar_path in jar_paths {
                                    let lib_id = *jar_to_library_id
                                        .entry(jar_path.clone())
                                        .or_insert_with(|| {
                                            LibraryId::from_jar_path(&jar_path)
                                                .expect("failed to hash jar path")
                                        });
                                    if let Ok(abs_jar_path) = AbsPathBuf::try_from(jar_path) {
                                        graph.library_paths.insert(lib_id, abs_jar_path);
                                    }
                                    explicit_jar_entries.push(ClasspathEntry::External(lib_id));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        let mut main_classpath = vec![ClasspathEntry::Sdk(sdk_id)];
        main_classpath.extend(internal_project_dependencies.clone());
        main_classpath.extend(explicit_jar_entries.clone());

        let mut test_classpath = vec![ClasspathEntry::Sdk(sdk_id)];
        test_classpath.push(ClasspathEntry::Internal {
            project_id,
            source_set: SourceSetKind::Main,
        });
        test_classpath.extend(internal_project_dependencies);
        test_classpath.extend(explicit_jar_entries);

        let main_source_set = SourceSetData {
            kind: SourceSetKind::Main,
            source_roots,
            generated_source_roots: generated_roots,
            compile_classpath: main_classpath.clone(),
            runtime_classpath: main_classpath,
            jpms_module_name: None,
        };

        let test_source_set = SourceSetData {
            kind: SourceSetKind::Test,
            source_roots: test_roots,
            generated_source_roots: Vec::new(),
            compile_classpath: test_classpath.clone(),
            runtime_classpath: test_classpath,
            jpms_module_name: None,
        };

        let mut source_sets = FxHashMap::default();
        source_sets.insert(SourceSetKind::Main, main_source_set);
        source_sets.insert(SourceSetKind::Test, test_source_set);

        let project_data = ProjectData {
            id: project_id,
            name: SmolStr::from(module_name),
            root_path: abs_project_dir,
            target_sdk: Some(sdk_id),
            source_sets,
        };

        graph.projects.insert(project_id, Arc::new(project_data));
    }

    graph
}
