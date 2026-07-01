use crate::idea::runner::{build_graph_from_idea, import_idea_workspace};
use crate::{BuildSystem, BuildSystemType, WorkspaceGraph};

mod model;
mod runner;

pub struct IdeaBuildSystem;

impl BuildSystem for IdeaBuildSystem {
    fn name(&self) -> &'static str {
        "IntelliJ IDEA"
    }

    fn is_applicable(&self, workspace_root: &std::path::Path) -> bool {
        // Validation check for either modular directory environments or fallback individual iml descriptors
        if workspace_root.join(".idea").join("modules.xml").exists() {
            return true;
        }

        // Single root folder scan optimization
        if let Ok(entries) = std::fs::read_dir(workspace_root) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|ext| ext == "iml") {
                    return true;
                }
            }
        }
        false
    }

    fn sync(
        &self,
        workspace_root: &std::path::Path,
        java_home: &std::path::Path,
    ) -> anyhow::Result<WorkspaceGraph> {
        tracing::info!(
            "Starting semantic sync for IntelliJ IDEA project layout at: {}",
            workspace_root.display()
        );

        let modules_metadata = import_idea_workspace(workspace_root)?;
        let workspace_graph = build_graph_from_idea(workspace_root, modules_metadata, java_home);

        tracing::info!(
            "Successfully completed IntelliJ IDEA build graph mapping. Tracked submodules: {}",
            workspace_graph.projects.len()
        );

        Ok(workspace_graph)
    }

    fn system_type(&self) -> BuildSystemType {
        BuildSystemType::Idea
    }
}
