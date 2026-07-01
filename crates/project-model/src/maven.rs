use crate::{BuildSystem, BuildSystemType};

pub struct MavenBuildSystem;

impl BuildSystem for MavenBuildSystem {
    fn name(&self) -> &'static str {
        "Maven"
    }

    fn is_applicable(&self, workspace_root: &std::path::Path) -> bool {
        workspace_root.join("pom.xml").exists()
    }

    fn sync(
        &self,
        workspace_root: &std::path::Path,
        java_home: &std::path::Path,
    ) -> anyhow::Result<crate::WorkspaceGraph> {
        anyhow::bail!("not implemented yet")
    }

    fn system_type(&self) -> BuildSystemType {
        BuildSystemType::Maven
    }
}
