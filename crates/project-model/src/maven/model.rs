use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct MavenWorkspace {
    pub workspace_name: String,
    pub projects: Vec<MavenProject>,
}

#[derive(Debug, Deserialize)]
pub struct MavenProject {
    pub path: String,
    pub name: String,
    pub project_dir: PathBuf,
    pub source_roots: Vec<PathBuf>,
    pub test_roots: Vec<PathBuf>,
    pub resource_roots: Vec<PathBuf>,
    pub generated_roots: Vec<PathBuf>,
    pub compile_classpath: Vec<MavenClasspathEntry>,
    pub test_classpath: Vec<MavenClasspathEntry>,
    pub java_language_version: Option<String>,
    pub java_home: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum MavenClasspathEntry {
    #[serde(rename = "project")]
    Project { path: String, source_set: String },
    #[serde(rename = "jar")]
    Jar {
        path: PathBuf,
        origin: String, // 'coordinate' or 'flat-file'
    },
}
