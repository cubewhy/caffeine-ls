use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectedBuildToolKind {
    Gradle,
    Maven,
}

#[derive(Debug, Clone)]
pub struct BuildWatchInterest {
    pub file_names: Vec<&'static str>,
}

impl BuildWatchInterest {
    pub fn matches_path(&self, path: &Path) -> bool {
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };

        self.file_names
            .iter()
            .any(|candidate| candidate == &file_name)
    }
}

#[derive(Debug, Clone)]
pub struct DetectedBuildTool {
    pub kind: DetectedBuildToolKind,
    pub root: PathBuf,
    pub watch_interest: BuildWatchInterest,
}
