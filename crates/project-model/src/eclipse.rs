use crate::{BuildSystem, BuildSystemType, WorkspaceGraph};

mod model;
mod runner;

pub struct EclipseBuildSystem;

impl BuildSystem for EclipseBuildSystem {
    fn name(&self) -> &'static str {
        "Eclipse"
    }

    fn is_applicable(&self, workspace_root: &std::path::Path) -> bool {
        workspace_root.join(".project").exists() && workspace_root.join(".classpath").exists()
    }

    fn sync(
        &self,
        workspace_root: &std::path::Path,
        java_home: &std::path::Path,
    ) -> anyhow::Result<WorkspaceGraph> {
        tracing::info!(
            "Starting workspace sync for Eclipse project at: {}",
            workspace_root.display()
        );

        let (project_name, classpath_model) = runner::import_eclipse_workspace(workspace_root)?;
        let workspace_graph = runner::build_graph_from_eclipse(
            workspace_root,
            &project_name,
            classpath_model,
            java_home,
        );

        tracing::info!(
            "Successfully synchronized Eclipse project structure '{}'",
            project_name
        );

        Ok(workspace_graph)
    }

    fn system_type(&self) -> BuildSystemType {
        BuildSystemType::Eclipse
    }
}
