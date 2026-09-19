//! Kotlin reference sites retain their declaring owner while library views load.

use ide::{
    AnalysisHost, Change, Classpath, ClasspathEntry, LibraryFileRef, LibraryId, LibraryInfo,
    LibraryKind, LibrarySources, NavigationTarget, ProjectGraphData, SourceSetId,
};
use ide_db::base_db::{SourceRoot, SourceRootId};
use lsp_test::classfile::{ACC_PUBLIC, build_jar, class_bytes, class_bytes_with_methods};
use rowan::{TextRange, TextSize};
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

const APP: &str = "package app\nimport api.Service\nimport api.Child\nfun use(service: Service, child: Child) {\n    service.send(1)\n    child.send(2)\n    service.total\n}\nfun create() = Service()\n";
const KOTLIN_SERVICE: &str = "package api\nclass Service {\n    @kotlin.jvm.JvmField val total: Int = 0\n    fun send(value: String) {}\n    fun send(value: Int) {}\n}\n";
const JAVA_SERVICE: &str = "package api;\npublic class Service {\n    public int total;\n    public void send(String value) {}\n    public void send(int value) {}\n}\n";

fn source_set() -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(0),
        kind: project_model::SourceSetKind::Main,
    }
}

fn range(text: &str, declaration: &str, name: &str) -> TextRange {
    let start = text.find(declaration).unwrap();
    let start = start + text[start..].find(name).unwrap();
    TextRange::new(
        TextSize::new(start as u32),
        TextSize::new((start + name.len()) as u32),
    )
}

fn offset(text: &str, needle: &str) -> TextSize {
    TextSize::new((text.find(needle).unwrap() + needle.len() - 1) as u32)
}

struct Fixture {
    _dir: tempfile::TempDir,
    host: AnalysisHost,
    app: FileId,
    library: LibraryId,
    workspace: FileSet,
    attached: bool,
}

impl Fixture {
    fn new(attached: bool) -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let abs = |path: &str| AbsPathBuf::assert_utf8(dir.path().join(path));
        let jar = abs("library.jar");
        build_jar(
            jar.as_ref(),
            &[
                (
                    "api/Service.class",
                    class_bytes_with_methods(
                        "api/Service",
                        "java/lang/Object",
                        &["total"],
                        &[
                            ("send", "(Ljava/lang/String;)V", ACC_PUBLIC),
                            ("send", "(I)V", ACC_PUBLIC),
                        ],
                    ),
                ),
                (
                    "api/Child.class",
                    class_bytes("api/Child", "api/Service", &[], &[]),
                ),
                (
                    "kotlin/jvm/JvmField.class",
                    class_bytes("kotlin/jvm/JvmField", "java/lang/Object", &[], &[]),
                ),
            ],
        )
        .unwrap();
        let library = LibraryId::from_file_path(jar.as_ref()).unwrap();
        let app = FileId::from_raw(1);
        let mut workspace = FileSet::default();
        workspace.insert(app, VfsPath::from(abs("src/App.kt")));
        let mut change = Change::default();
        change.change_file(app, Some(APP.to_owned()));
        change.set_roots(vec![
            SourceRoot::new(workspace.clone()),
            SourceRoot::library(FileSet::default()),
        ]);
        let mut graph = ProjectGraphData::default();
        graph
            .libraries
            .insert(library, LibraryInfo::new(LibraryKind::Jar, jar));
        graph.source_sets.insert(
            source_set(),
            triomphe::Arc::new(Classpath {
                entries: vec![ClasspathEntry::Library(library)],
            }),
        );
        graph
            .source_root_to_source_set
            .insert(SourceRootId(0), source_set());
        if attached {
            let archive = abs("library-sources.jar");
            build_jar(
                archive.as_ref(),
                &[("api/Service.kt", KOTLIN_SERVICE.as_bytes().to_vec())],
            )
            .unwrap();
            graph.library_sources.insert(
                library,
                LibrarySources {
                    archive,
                    root: abs("sources"),
                },
            );
            graph.library_source_roots.insert(SourceRootId(1), library);
        } else {
            graph.library_decompiled.insert(library, abs("decompiled"));
            graph
                .library_decompiled_roots
                .insert(SourceRootId(1), library);
        }
        change.set_project_graph(graph);
        let mut host = AnalysisHost::new();
        host.apply_change(change);
        Self {
            _dir: dir,
            host,
            app,
            library,
            workspace,
            attached,
        }
    }

    fn definition(&self, needle: &str) -> Vec<NavigationTarget> {
        self.host
            .snapshot()
            .goto_definition(self.app, offset(APP, needle))
            .unwrap()
    }

    fn pending(&self, needle: &str) -> Vec<LibraryFileRef> {
        self.host
            .snapshot()
            .pending_library_files(self.app, offset(APP, needle))
            .unwrap()
    }

    fn materialize(&mut self, pending: &LibraryFileRef) -> FileId {
        let path = match pending {
            LibraryFileRef::Source { path, .. } | LibraryFileRef::Decompile { path, .. } => path,
        };
        let file = FileId::from_raw(1000);
        let mut root = FileSet::default();
        root.insert(file, VfsPath::from(path.clone()));
        let mut change = Change::default();
        change.change_file(
            file,
            Some(
                if self.attached {
                    KOTLIN_SERVICE
                } else {
                    JAVA_SERVICE
                }
                .to_owned(),
            ),
        );
        change.set_roots(vec![
            SourceRoot::new(self.workspace.clone()),
            SourceRoot::library(root),
        ]);
        self.host.apply_change(change);
        file
    }
}

fn assert_materialization(attached: bool) {
    let mut fixture = Fixture::new(attached);
    // The inherited call must request Service, not its receiver class Child.
    let sites = [
        "service.send",
        "child.send",
        "service.total",
        "service: Service",
        "= Service",
    ];
    let pending = fixture.pending(sites[0]);
    assert_eq!(pending.len(), 1, "{pending:?}");
    match &pending[0] {
        LibraryFileRef::Source { library, entry, .. } => {
            assert!(attached);
            assert_eq!(*library, fixture.library);
            assert_eq!(entry.as_ref(), "api/Service.kt");
        }
        LibraryFileRef::Decompile { library, class, .. } => {
            assert!(!attached);
            assert_eq!(*library, fixture.library);
            assert_eq!(class.as_ref(), "api.Service");
        }
    }
    for site in sites {
        assert!(fixture.definition(site).is_empty(), "{site}");
        assert_eq!(fixture.pending(site), pending, "{site}");
    }
    let file = fixture.materialize(&pending[0]);
    let text = if attached {
        KOTLIN_SERVICE
    } else {
        JAVA_SERVICE
    };
    let method = if attached {
        "fun send(value: Int)"
    } else {
        "void send(int value)"
    };
    for (site, declaration, name) in [
        ("service.send", method, "send"),
        ("child.send", method, "send"),
        ("service.total", "total", "total"),
        ("service: Service", "class Service", "Service"),
        ("= Service", "class Service", "Service"),
    ] {
        assert_eq!(
            fixture.definition(site),
            vec![NavigationTarget {
                file,
                range: range(text, declaration, name),
                name: name.to_owned()
            }],
            "{site}"
        );
        assert!(fixture.pending(site).is_empty(), "{site}");
    }
}

#[test]
fn kotlin_library_source_members_and_classes_materialize() {
    assert_materialization(true);
}

#[test]
fn kotlin_library_decompiled_members_and_classes_materialize() {
    assert_materialization(false);
}

#[test]
fn cross_file_kotlin_targets_use_the_declaring_tree() {
    let caller = "package app\nimport api.remote\nimport api.answer\nfun call() { remote() }\nfun read() = answer\n";
    // The target item ids exceed the caller's item count: reading the caller's
    // arena either chooses an unrelated name or indexes outside its bounds.
    let target = "package api\nfun padding0() {}\nfun padding1() {}\nfun padding2() {}\nfun padding3() {}\nfun remote() {}\nval answer = 42\n";
    let host = workspace(&[("/src/App.kt", caller), ("/src/Api.kt", target)]);
    for (site, declaration, name) in [
        ("{ remote", "fun remote", "remote"),
        ("= answer", "val answer", "answer"),
    ] {
        assert_eq!(
            host.snapshot()
                .goto_definition(FileId::from_raw(1), offset(caller, site))
                .unwrap(),
            vec![NavigationTarget {
                file: FileId::from_raw(2),
                range: range(target, declaration, name),
                name: name.to_owned()
            }]
        );
    }
}

#[test]
fn kotlin_default_import_type_navigates_to_its_declaration() {
    let caller = "package app\nfun use(task: Runnable) {}\n";
    let target = "package java.lang; public interface Runnable {}";
    let host = workspace(&[("/src/App.kt", caller), ("/src/Runnable.java", target)]);
    assert_eq!(
        host.snapshot()
            .goto_definition(FileId::from_raw(1), offset(caller, "Runnable"))
            .unwrap(),
        vec![NavigationTarget {
            file: FileId::from_raw(2),
            range: range(target, "interface Runnable", "Runnable"),
            name: "Runnable".to_owned(),
        }],
    );
}

fn workspace(sources: &[(&str, &str)]) -> AnalysisHost {
    let mut files = FileSet::default();
    let mut change = Change::default();
    for (index, &(path, text)) in sources.iter().enumerate() {
        let file = FileId::from_raw(index as u32 + 1);
        files.insert(file, VfsPath::from(AbsPathBuf::assert_utf8(path.into())));
        change.change_file(file, Some(text.to_owned()));
    }
    change.set_roots(vec![SourceRoot::new(files)]);
    let mut graph = ProjectGraphData::default();
    graph
        .source_root_to_source_set
        .insert(SourceRootId(0), source_set());
    graph.source_sets.insert(
        source_set(),
        triomphe::Arc::new(Classpath {
            entries: Vec::new(),
        }),
    );
    change.set_project_graph(graph);
    let mut host = AnalysisHost::new();
    host.apply_change(change);
    host
}
