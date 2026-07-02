use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum GradleClasspathEntry {
    #[serde(rename = "project")]
    Project { path: String, source_set: String },
    #[serde(rename = "jar")]
    Jar {
        path: PathBuf,
        origin: String, // 'coordinate' or 'flat-file'
    },
}

#[derive(Debug, Deserialize)]
pub struct GradleProject {
    pub path: String,
    pub name: String,
    pub project_dir: PathBuf,
    pub source_roots: Vec<PathBuf>,
    pub test_roots: Vec<PathBuf>,
    pub resource_roots: Vec<PathBuf>,
    pub generated_roots: Vec<PathBuf>,
    pub compile_classpath: Vec<GradleClasspathEntry>,
    pub test_classpath: Vec<GradleClasspathEntry>,
    pub java_language_version: Option<String>,
    pub java_home: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GradleWorkspace {
    pub workspace_name: String,
    pub projects: Vec<GradleProject>,
}
