use crate::{BuildSystem, BuildSystemType};

mod model;
pub(crate) mod progress;
mod runner;
mod sidecar;

pub struct MavenBuildSystem;

impl BuildSystem for MavenBuildSystem {
    fn name(&self) -> &'static str {
        "Maven"
    }

    fn is_applicable(&self, workspace_root: &std::path::Path) -> bool {
        workspace_root.join("pom.xml").exists()
    }

    fn support_logging(&self) -> bool {
        true
    }

    fn sync(
        &self,
        workspace_root: &std::path::Path,
        java_home: &std::path::Path,
        log_file: Option<&std::path::Path>,
        on_output: &mut (dyn FnMut(String) + Send),
    ) -> anyhow::Result<crate::WorkspaceGraph> {
        self.sync_with_progress(workspace_root, java_home, log_file, on_output, &mut |_| {})
    }

    fn sync_with_progress(
        &self,
        workspace_root: &std::path::Path,
        java_home: &std::path::Path,
        log_file: Option<&std::path::Path>,
        on_output: &mut (dyn FnMut(String) + Send),
        on_progress: &mut (dyn FnMut(crate::SyncProgress) + Send),
    ) -> anyhow::Result<crate::WorkspaceGraph> {
        let maven_workspace = runner::import_maven_workspace(
            workspace_root,
            java_home,
            log_file,
            on_output,
            on_progress,
        )?;
        let workspace_graph = runner::build_graph_from_maven_json(maven_workspace);

        tracing::info!(
            "Successfully synchronized Maven workspace '{}' with {} tracked sub-projects",
            workspace_graph.projects.len(), // Assuming projects is accessible via a map/collection len
            workspace_graph.projects.len()
        );

        Ok(workspace_graph)
    }

    fn system_type(&self) -> BuildSystemType {
        BuildSystemType::Maven
    }
}
