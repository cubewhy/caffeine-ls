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
    pub java_language_preview: Option<bool>,
    /// The explicit `maven.compiler.release` of the project, when it set one
    /// (`javac --release N`, [JEP 247](https://openjdk.org/jeps/247)).
    /// `maven.compiler.source` alone does not select a platform view, so it
    /// never fills this.
    pub java_release: Option<u8>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An export whose compile roots carry the Kotlin directory the export adds
    /// when the Kotlin plugin did not register it (`addConventionalRoot`).
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
          "resource_roots": [],
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
        let workspace: MavenWorkspace =
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
