use std::path::Path;

use crate::{BuildSystem, BuildSystemType, WorkspaceGraph};

pub use runner::build_graph_from_json;

mod model;
pub(crate) mod progress;
mod runner;
mod script;

pub struct GradleBuildSystem;

impl BuildSystem for GradleBuildSystem {
    fn name(&self) -> &'static str {
        "Gradle"
    }

    fn is_applicable(&self, workspace_root: &Path) -> bool {
        workspace_root.join("build.gradle").exists()
            || workspace_root.join("build.gradle.kts").exists()
            || workspace_root.join("settings.gradle").exists()
    }

    fn support_logging(&self) -> bool {
        true
    }

    fn sync(
        &self,
        workspace_root: &Path,
        java_home: &Path,
        log_file: Option<&Path>,
        on_output: &mut (dyn FnMut(String) + Send),
    ) -> anyhow::Result<WorkspaceGraph> {
        self.sync_with_progress(workspace_root, java_home, log_file, on_output, &mut |_| {})
    }

    fn sync_with_progress(
        &self,
        workspace_root: &Path,
        java_home: &Path,
        log_file: Option<&Path>,
        on_output: &mut (dyn FnMut(String) + Send),
        on_progress: &mut (dyn FnMut(crate::SyncProgress) + Send),
    ) -> anyhow::Result<WorkspaceGraph> {
        let gradle_json = runner::import_gradle_workspace(
            workspace_root,
            java_home,
            log_file,
            on_output,
            on_progress,
        )?;
        let graph = build_graph_from_json(gradle_json);

        Ok(graph)
    }

    fn system_type(&self) -> BuildSystemType {
        BuildSystemType::Gradle
    }
}
