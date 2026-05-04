#[derive(Clone, Copy)]
pub enum BuildTool {
    Javac, // or IntelliJ
    Gradle,
    Maven,
}

#[derive(Clone)]
pub struct ProjectWorkspace {
    pub build_tool: BuildTool,
    pub jdk_version: u16,
    pub modules: Vec<ProjectModule>,
}

#[derive(Clone)]
pub struct ProjectModule {
    pub name: String,
    pub source_roots: Vec<String>,
    pub classpath: Vec<String>,
    // TODO: gradle/maven project model
}
