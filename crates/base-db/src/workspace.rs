use smol_str::SmolStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DependencyScope {
    /// Gradle: api, Maven: compile
    Api,
    /// Gradle: implementation
    Implementation,
    /// Gradle: compileOnly, Maven: provided
    CompileOnly,
    /// Gradle: testImplementation, Maven: test
    Test,
    /// Gradle: runtimeOnly, Maven: runtime
    RuntimeOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dependency {
    pub target: WorkspaceModule,
    pub scope: DependencyScope,

    pub jpms_module_name: Option<SmolStr>,
}

#[salsa::input]
#[derive(Debug)]
pub struct WorkspaceModule {
    #[returns(ref)]
    pub name: String,

    #[returns(ref)]
    pub dependencies: Vec<Dependency>,

    pub jdk_version: u8,
}

#[salsa::input(singleton)]
pub struct Workspace {
    #[returns(ref)]
    pub roots: Vec<SourceRoot>,
}

#[derive(Debug, Clone, Copy)]
pub enum SourceRootKind {
    Source,
    Library,
}

#[salsa::input]
#[derive(Debug)]
pub struct SourceRoot {
    pub kind: SourceRootKind,

    #[returns(ref)]
    pub files: Vec<vfs::FileId>,
}
