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
        #[serde(default)]
        sources: Option<PathBuf>,
        #[serde(default)]
        group: Option<String>,
        #[serde(default)]
        artifact: Option<String>,
        #[serde(default)]
        version: Option<String>,
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
    pub java_release: Option<u8>,
    pub java_language_preview: Option<bool>,
    pub java_home: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GradleWorkspace {
    pub workspace_name: String,
    pub projects: Vec<GradleProject>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An export whose `mainSource` carries both a Java and a Kotlin source
    /// directory — the shape the init script produces once a source set has
    /// Kotlin sources (`sourceDirsOf` unions the Java plugin's directories with
    /// the Kotlin plugin's and the conventional `src/main/kotlin`).
    const EXPORT: &str = r#"
    {
      "workspace_name": "app",
      "projects": [
        {
          "path": ":app",
          "name": "app",
          "project_dir": "/w/app",
          "source_roots": [
            "/w/app/src/main/java",
            "/w/app/src/main/kotlin"
          ],
          "test_roots": [
            "/w/app/src/test/java",
            "/w/app/src/test/kotlin"
          ],
          "resource_roots": ["/w/app/src/main/resources"],
          "generated_roots": [],
          "compile_classpath": [],
          "test_classpath": [],
          "java_language_version": null,
          "java_release": null,
          "java_language_preview": null,
          "java_home": null
        }
      ]
    }
    "#;

    #[test]
    fn kotlin_source_roots_survive_the_export() {
        let workspace: GradleWorkspace =
            serde_json::from_str(EXPORT).expect("the export deserializes");
        let project = &workspace.projects[0];
        assert_eq!(
            project.source_roots,
            vec![
                PathBuf::from("/w/app/src/main/java"),
                PathBuf::from("/w/app/src/main/kotlin"),
            ]
        );
        assert_eq!(
            project.test_roots,
            vec![
                PathBuf::from("/w/app/src/test/java"),
                PathBuf::from("/w/app/src/test/kotlin"),
            ]
        );
    }
}
