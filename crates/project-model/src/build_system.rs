use serde::Serialize;

use crate::{GradleBuildSystem, MavenBuildSystem, gradle, workspace::WorkspaceGraph};
use std::path::Path;

#[derive(Debug, Copy, Clone, Serialize)]
pub enum BuildSystemType {
    Gradle,
    Maven,
    Eclipse,
    Idea,
}

impl BuildSystemType {
    pub fn name(&self) -> &'static str {
        match self {
            BuildSystemType::Gradle => "Gradle",
            BuildSystemType::Maven => "Maven",
            BuildSystemType::Eclipse => "Eclipse Classpath",
            BuildSystemType::Idea => "IDEA",
        }
    }
}

/// Represents a tool that can resolve the workspace structure.
pub trait BuildSystem: Send + Sync {
    /// The name of the build system (e.g., "Gradle", "Maven")
    fn name(&self) -> &'static str;

    /// Checks if this build system manages the given directory
    /// (e.g., by looking for build.gradle or pom.xml)
    fn is_applicable(&self, workspace_root: &Path) -> bool;

    /// Executes the tool to build and return the workspace graph.
    fn sync(&self, workspace_root: &Path, java_home: &Path) -> anyhow::Result<WorkspaceGraph>;

    fn system_type(&self) -> BuildSystemType;
}

pub enum ProbeResult {
    None,
    Single(BuildSystemType),
    Ambiguous(Vec<BuildSystemType>),
}

pub fn probe_workspace_layout(root: &Path) -> ProbeResult {
    // Registry of all compilation engines supported by your frontend
    let managers: &[&dyn BuildSystem] = &[&GradleBuildSystem, &MavenBuildSystem];

    // Collect every system that detects its build files
    let detected_systems: Vec<BuildSystemType> = managers
        .iter()
        .filter(|sys| sys.is_applicable(root))
        .map(|sys| sys.system_type())
        .collect();

    match detected_systems.len() {
        0 => ProbeResult::None,
        1 => ProbeResult::Single(detected_systems[0]),
        _ => ProbeResult::Ambiguous(detected_systems),
    }
}

/// Standalone synchronization action, executed only after a choice is locked down.
pub fn sync_specific_build_system(
    system: BuildSystemType,
    root: &std::path::Path,
    java_home: &std::path::Path,
) -> anyhow::Result<crate::WorkspaceGraph> {
    match system {
        BuildSystemType::Gradle => {
            let json = gradle::import_gradle_workspace(root, java_home)?;
            Ok(gradle::build_graph_from_json(json))
        }
        BuildSystemType::Maven => todo!("Implement maven extraction pipeline"),
        BuildSystemType::Eclipse => todo!("Implement eclipse extraction pipeline"),
        BuildSystemType::Idea => todo!("Implement idea extraction pipeline"),
    }
}
