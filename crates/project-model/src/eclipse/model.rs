use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename = "projectDescription")]
pub struct EclipseProjectDescription {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "classpath")]
pub struct EclipseClasspath {
    #[serde(rename = "classpathentry", default)]
    pub entries: Vec<EclipseClasspathEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct EclipseClasspathEntry {
    #[serde(rename = "@kind")]
    pub kind: String, // "src", "lib", "con", "output"

    #[serde(rename = "@path")]
    pub path: String,

    #[serde(rename = "@combiningaccessrules")]
    pub combine_access_rules: Option<String>,
}
