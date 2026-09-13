//! Insta snapshot tests for the inlay hints of `ide` (see [`ide::inlay_hints`]).

use triomphe::Arc;

use ide::{
    Analysis, AnalysisHost, Change, Classpath, ClasspathEntry, InlayHint, InlayHintKind,
    InlayHintsConfig, LibraryId, LibraryInfo, LibraryKind, LibrarySources, ProjectGraphData,
    SourceSetId,
};
use ide_db::base_db::{SourceRoot, SourceRootId};
use insta::assert_snapshot;
use lsp_test::classfile::{build_jar, class_bytes};
use rowan::{TextRange, TextSize};
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

fn main_source_set(project: u32) -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(project),
        kind: project_model::SourceSetKind::Main,
    }
}

struct Fixture {
    /// Keeps a fixture's dependency jar alive for the test's lifetime.
    _dir: Option<tempfile::TempDir>,
    host: AnalysisHost,
    file: FileId,
    text: String,
}

impl Fixture {
    fn analysis(&self) -> Analysis {
        self.host.snapshot()
    }

    /// The byte offset of `needle`'s first character, for its `occurrence`-th
    /// (0-based) occurrence.
    fn offset_start(&self, needle: &str, occurrence: usize) -> TextSize {
        let mut from = 0;
        for _ in 0..occurrence {
            let found = self.text[from..]
                .find(needle)
                .unwrap_or_else(|| panic!("occurrence {occurrence} of {needle:?} not found"));
            from += found + needle.len();
        }
        let idx = self.text[from..]
            .find(needle)
            .unwrap_or_else(|| panic!("occurrence {occurrence} of {needle:?} not found"));
        TextSize::new((from + idx) as u32)
    }

    /// The byte offset just after `needle`'s first occurrence — where a `var`
    /// hint is anchored, since the hint renders the inferred type *after* the
    /// declared name.
    fn offset_end(&self, needle: &str) -> TextSize {
        self.offset_end_at(needle, 0)
    }

    /// [`Self::offset_end`] for `needle`'s `occurrence`-th (0-based) one —
    /// where a hint anchored at the *end* of an expression is anchored.
    fn offset_end_at(&self, needle: &str, occurrence: usize) -> TextSize {
        self.offset_start(needle, occurrence) + TextSize::of(needle)
    }

    /// The file's hints over its whole text, under the default configuration.
    fn hints(&self) -> Vec<InlayHint> {
        self.hints_with(&InlayHintsConfig::default())
    }

    fn hints_with(&self, config: &InlayHintsConfig) -> Vec<InlayHint> {
        let whole = TextRange::up_to(TextSize::of(self.text.as_str()));
        self.analysis()
            .inlay_hints(self.file, whole, config)
            .unwrap()
    }
}

fn test_file(text: &str) -> Fixture {
    let mut host = AnalysisHost::new();
    let mut change = Change::default();
    let mut file_set = FileSet::default();
    let file = FileId::from_raw(1);
    file_set.insert(
        file,
        VfsPath::from(AbsPathBuf::assert_utf8(
            "/src/main/java/com/example/Sample.java".into(),
        )),
    );
    change.change_file(file, Some(text.to_string()));
    change.set_roots(vec![SourceRoot::new(file_set)]);

    let mut data = ProjectGraphData::default();
    data.source_root_to_source_set
        .insert(SourceRootId(0), main_source_set(0));
    data.source_sets.insert(
        main_source_set(0),
        Arc::new(Classpath {
            entries: Vec::new(),
        }),
    );
    change.set_project_graph(data);
    host.apply_change(change);

    Fixture {
        _dir: None,
        host,
        file,
        text: text.to_owned(),
    }
}

/// The library source entry every source-carrying fixture materializes.
const LIB_SOURCE_ENTRY: &str = "com/example/Lib.java";

/// The file id that fixture assigns the materialized library source.
const LIB_SOURCE_FILE: u32 = 1000;

/// A workspace file plus a dependency jar holding `com.example.Lib` with the
/// given members — *library* members, since no classfile without a
/// `MethodParameters` attribute records parameter names.
///
/// `sources` is the state the library's attached sources are in, which is what
/// the parameter-name lookup has to tell apart:
///
/// * `None`: the library ships no sources, so it can never name a member (the
///   state a client that disabled source download leaves its dependencies in);
/// * `Some((text, true))`: the archive carries the class's source and the file
///   is *loaded* — [`hir::LibrarySourceDecl::Loaded`];
/// * `Some((text, false))`: the archive carries it but no session has
///   materialized it — [`hir::LibrarySourceDecl::Pending`].
///
/// `workspace` runs the fixture inside a caller-owned directory instead of a
/// fresh one, which is what two sessions *sharing a library cache* need: a
/// library is identified by its jar's path (and the cache keyed by it). `cache`
/// enables the persistent library cache at that directory.
fn library_fixture(
    workspace: Option<&tempfile::TempDir>,
    text: &str,
    sources: Option<(&str, bool)>,
    lib_methods: &[(&str, usize)],
    cache: Option<&std::path::Path>,
) -> Fixture {
    let dir = workspace
        .map(|_| None)
        .unwrap_or_else(|| Some(tempfile::TempDir::new().unwrap()));
    let dir_path = dir
        .as_ref()
        .map(|dir| dir.path().to_path_buf())
        .unwrap_or_else(|| workspace.expect("one of them").path().to_path_buf());
    let base = dir_path.clone();
    let jar = base.join("lib/deps.jar");
    // A jar already in the workspace is reused: a library's identity is its
    // path *and* its modification time, so two sessions over one workspace only
    // see one library — and so one cache entry — if neither rewrites it.
    if !jar.exists() {
        let class = class_bytes("com/example/Lib", "java/lang/Object", &[], lib_methods);
        build_jar(&jar, &[("com/example/Lib.class", class)]).unwrap();
    }

    let file = FileId::from_raw(1);
    let mut change = Change::default();
    let mut file_set = FileSet::default();
    file_set.insert(
        file,
        VfsPath::from(AbsPathBuf::assert_utf8(
            base.join("src/com/example/App.java"),
        )),
    );
    change.change_file(file, Some(text.to_owned()));

    let mut library_root = FileSet::default();
    let mut sources_archive = None;
    let mut sources_root = None;
    if let Some((source, loaded)) = sources {
        let sources_jar = base.join("lib/deps-sources.jar");
        let lib_root = base.join("sources/deps");
        if !sources_jar.exists() {
            build_jar(
                &sources_jar,
                &[(LIB_SOURCE_ENTRY, source.as_bytes().to_vec())],
            )
            .unwrap();
        }
        if loaded {
            library_root.insert(
                FileId::from_raw(LIB_SOURCE_FILE),
                VfsPath::from(AbsPathBuf::assert_utf8(lib_root.join(LIB_SOURCE_ENTRY))),
            );
            change.change_file(FileId::from_raw(LIB_SOURCE_FILE), Some(source.to_owned()));
        }
        sources_archive = Some(sources_jar);
        sources_root = Some(lib_root);
    }
    change.set_roots(vec![
        SourceRoot::new(file_set),
        SourceRoot::library(library_root),
    ]);

    let library = LibraryId::from_file_path(&jar).unwrap();
    let mut data = ProjectGraphData::default();
    data.libraries.insert(
        library,
        LibraryInfo::new(LibraryKind::Jar, AbsPathBuf::assert_utf8(jar)),
    );
    data.source_sets.insert(
        main_source_set(0),
        Arc::new(Classpath {
            entries: vec![ClasspathEntry::Library(library)],
        }),
    );
    data.source_root_to_source_set
        .insert(SourceRootId(0), main_source_set(0));
    if let (Some(archive), Some(root)) = (sources_archive, sources_root) {
        data.library_sources.insert(
            library,
            LibrarySources {
                archive: AbsPathBuf::assert_utf8(archive),
                root: AbsPathBuf::assert_utf8(root),
            },
        );
        data.library_source_roots.insert(SourceRootId(1), library);
    }

    let mut host = AnalysisHost::new();
    if let Some(cache) = cache {
        assert!(
            host.enable_persistent_stub_cache(cache),
            "the fixture's cache directory must be usable"
        );
    }
    change.set_project_graph(data);
    host.apply_change(change);

    Fixture {
        _dir: dir,
        host,
        file,
        text: text.to_owned(),
    }
}

/// [`library_fixture`] over a fresh workspace, with a source-less library.
fn library_file(text: &str, lib_methods: &[(&str, usize)]) -> Fixture {
    library_fixture(None, text, None, lib_methods, None)
}

/// [`library_fixture`] in a fresh workspace, with the library's source attached.
fn lib_source_file(
    text: &str,
    source: &str,
    lib_methods: &[(&str, usize)],
    loaded: bool,
    cache: Option<&std::path::Path>,
) -> Fixture {
    library_fixture(None, text, Some((source, loaded)), lib_methods, cache)
}

/// `kind @offset label` per hint, one row each — the label as the client
/// renders it (every part concatenated).
fn render_hints(hints: &[InlayHint]) -> String {
    hints
        .iter()
        .map(|hint| {
            let label: String = hint.label.iter().map(|part| part.value.as_str()).collect();
            format!("{:?} @{:?} {}", hint.kind, hint.offset, label)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const VAR_LOCAL: &str = r#"package com.example;

class Text {
    int length() {
        return 0;
    }
}

class Box<T> {
    T value() {
        return null;
    }
}

class Sample {
    static Text text() {
        return null;
    }

    static int count() {
        return 0;
    }

    static Box<Text> box() {
        return null;
    }

    void locals() {
        var t = text();
        var n = count();
        var b = box();
        var s = t.length() > 0 ? text() : text();
    }
}
"#;

#[test]
fn var_type_local() {
    let fixture = test_file(VAR_LOCAL);
    let hints = fixture.hints();

    // The hint sits exactly after the declared name, `var t: Text = ...`.
    assert_eq!(hints[0].offset, fixture.offset_end("var t"));
    assert_eq!(hints[0].kind, InlayHintKind::Type);
    assert_eq!(hints[1].offset, fixture.offset_end("var n"));
    assert_eq!(hints[2].offset, fixture.offset_end("var b"));
    assert_eq!(hints[3].offset, fixture.offset_end("var s"));

    assert_snapshot!("var_type_local", render_hints(&hints));
}

const VAR_FORMS: &str = r#"package com.example;

class Text {
    static Text open() {
        return null;
    }
}

class Sample {
    void forms(Text[] items) {
        for (var item : items) {
            item.length();
        }
        try (var r = Text.open()) {
            r.length();
        }
    }
}
"#;

#[test]
fn var_type_forms() {
    let fixture = test_file(VAR_FORMS);
    let hints = fixture.hints();

    assert_eq!(hints[0].offset, fixture.offset_end("for (var item"));
    assert_eq!(hints[1].offset, fixture.offset_end("try (var r"));

    assert_snapshot!("var_type_forms", render_hints(&hints));
}

const VAR_INITIALIZERS: &str = r#"package com.example;

class Text {
    int length() {
        return 0;
    }
}

class Sample {
    static Text text() {
        return null;
    }

    void initializers() {
        var e = "";
        var i = 1;
        var t = new Text();
        var a = new int[] { 1 };
        var c = (Text) text();
        var n = 1 + 2;
    }
}
"#;

#[test]
fn var_type_self_describing_initializers() {
    let fixture = test_file(VAR_INITIALIZERS);
    let hints = fixture.hints();

    // The hint renders the type the *compiler* inferred, whatever the
    // initializer says — `var e = "";` is the case the hint exists for, and
    // `int`/`Text` beside `1`/`new Text()` confirm the inference just as well.
    assert_eq!(hints.len(), 6);
    assert_eq!(hints[0].offset, fixture.offset_end("var e"));
    assert_eq!(hints[0].kind, InlayHintKind::Type);
    assert_eq!(hints[1].offset, fixture.offset_end("var i"));
    assert_eq!(hints[2].offset, fixture.offset_end("var t"));
    assert_eq!(hints[3].offset, fixture.offset_end("var a"));
    assert_eq!(hints[4].offset, fixture.offset_end("var c"));
    assert_eq!(hints[5].offset, fixture.offset_end("var n"));

    assert_snapshot!(
        "var_type_self_describing_initializers",
        render_hints(&hints)
    );
}

const VAR_OMITTED: &str = r#"package com.example;

class Text {
    int length() {
        return 0;
    }

    static Text text() {
        return null;
    }

    void omitted(Text[] items) {
        Text typed = text();
        var z = null;
        if (items instanceof Text[] arr) {
            arr.length();
        }
    }
}
"#;

#[test]
fn var_type_omitted() {
    let fixture = test_file(VAR_OMITTED);

    // Nothing to render: a written type is no `var` declaration at all, the
    // null type names no type the source could write ([JLS §4.1]), and a
    // pattern binding states its type in the pattern itself ([§14.30.1]).
    assert_eq!(render_hints(&fixture.hints()), "");
}

/// A configuration with every category off renders nothing, and each flag
/// gates only its own category.
#[test]
fn var_type_disabled_by_config() {
    let fixture = test_file(VAR_LOCAL);
    let config = InlayHintsConfig {
        var_types: false,
        ..InlayHintsConfig::default()
    };

    assert_eq!(render_hints(&fixture.hints_with(&config)), "");
    assert_eq!(
        render_hints(&fixture.hints_with(&InlayHintsConfig::default())),
        render_hints(&fixture.hints())
    );
}

/// A range that contains no hint's offset answers nothing, even though the
/// file has hints.
#[test]
fn var_type_outside_range() {
    let fixture = test_file(VAR_LOCAL);
    let analysis = fixture.analysis();
    let start = fixture.offset_start("void locals()", 0);
    let range = TextRange::new(start, start + TextSize::new(4));

    let hints = analysis
        .inlay_hints(fixture.file, range, &InlayHintsConfig::default())
        .unwrap();
    assert!(hints.is_empty(), "{hints:?}");
}

const LAMBDA_TYPES: &str = r#"package com.example;

class Text {
    int length() {
        return 0;
    }
}

interface Mapper<T, R> {
    R apply(T value);
}

interface Combiner<T> {
    T combine(T a, T b);
}

class Sample {
    static Text text() {
        return null;
    }

    void types() {
        Mapper<Text, Text> m = s -> s;
        Mapper<Text, Text> mv = (var v) -> v;
        Combiner<Text> c = (a, b) -> a;
        Combiner<Text> p = (x, y) -> text();
    }
}
"#;

#[test]
fn lambda_parameter_types() {
    let fixture = test_file(LAMBDA_TYPES);
    let hints = fixture.hints();

    // The type renders *before* the parameter's name: `(Text s) -> ...`.
    assert_eq!(hints[0].offset, fixture.offset_start("s -> s;", 0));
    assert_eq!(hints[0].kind, InlayHintKind::Type);
    assert_eq!(hints[1].offset, fixture.offset_start("v) -> v;", 0));
    assert_eq!(hints[2].offset, fixture.offset_start("a, b) -> a;", 0));
    assert_eq!(hints[3].offset, fixture.offset_start("b) -> a;", 0));
    assert_eq!(hints[4].offset, fixture.offset_start("x, y) -> text();", 0));
    assert_eq!(hints[5].offset, fixture.offset_start("y) -> text();", 0));

    assert_snapshot!("lambda_parameter_types", render_hints(&hints));
}

const LAMBDA_SUPER_WILDCARD: &str = r#"package com.example;

class Text {
    int length() {
        return 0;
    }
}

interface Sink<T> {
    void accept(T value);
}

class Sample {
    static Text text() {
        return null;
    }

    void consume() {
        Sink<? super Text> sink = value -> { };
    }
}
"#;

#[test]
fn lambda_parameter_type_super_wildcard() {
    let fixture = test_file(LAMBDA_SUPER_WILDCARD);
    let hints = fixture.hints();

    assert_eq!(hints[0].offset, fixture.offset_start("value -> ", 0));

    // A captured `? super Text` contributes its *lower* bound, so the
    // parameter renders `Text`, not `? super Text` ([JLS §5.1.10]).
    assert_snapshot!("lambda_parameter_type_super_wildcard", render_hints(&hints));
}

const LAMBDA_OMITTED: &str = r#"package com.example;

class Text {
    int length() {
        return 0;
    }
}

interface Mapper<T, R> {
    R apply(T value);
}

interface Task {
    void run();
}

class Sample {
    static Text text() {
        return null;
    }

    void omitted() {
        Mapper<Text, Text> declared = (Text s) -> s;
        var untargeted = () -> 1;
        Task task = () -> { };
    }
}
"#;

#[test]
fn lambda_parameter_type_omitted() {
    let fixture = test_file(LAMBDA_OMITTED);

    // A parameter that writes its own type states it, an untargeted lambda has
    // no SAM to read, and a parameterless lambda declares none.
    assert_eq!(render_hints(&fixture.hints()), "");
}

const PARAMETER_UNCLEAR: &str = r#"package com.example;

class Text {
    int count;
}

class Sample {
    int field;

    static void foo(int size, int count, Text other, int sum, int delta) {
    }

    void run(Text b, int c) {
        foo(1, 2, null, b.count + c, -1);
    }
}
"#;

#[test]
fn parameter_names_unclear_arguments() {
    let fixture = test_file(PARAMETER_UNCLEAR);
    let hints = fixture.hints();

    // Every hint renders the name *before* its argument, `foo(size: 1, ...)`.
    assert_eq!(hints.len(), 5);
    assert_eq!(hints[0].offset, fixture.offset_start("1, 2, null", 0));
    assert_eq!(hints[0].kind, InlayHintKind::Parameter);
    assert_eq!(hints[1].offset, fixture.offset_start("2, null", 0));
    assert_eq!(hints[2].offset, fixture.offset_start("null, b.count", 0));
    assert_eq!(hints[3].offset, fixture.offset_start("b.count + c", 0));
    assert_eq!(hints[4].offset, fixture.offset_start("-1);", 0));

    assert_snapshot!("parameter_names_unclear_arguments", render_hints(&hints));
}

const PARAMETER_OMITTED: &str = r#"package com.example;

class Text {
    int count;
}

class Sample {
    static void setSize(int size) {
    }

    static void setField(Text config) {
    }

    static void add(int arg0, int arg1) {
    }

    static void pick(int x, double y) {
    }

    static void pick(double x, int y) {
    }

    static void tick(Text t) {
    }

    Text config;

    void run(Text config, int size) {
        setSize(size);
        setField(config);
        add(1, 2);
        tick(this.config);
        pick(1, 1);
    }
}
"#;

#[test]
fn parameter_names_omitted() {
    let fixture = test_file(PARAMETER_OMITTED);

    // Each call's hint is suppressed by its own rule: the argument repeats the
    // parameter's name (`setSize(size)`, `setField(config)`), every parameter
    // name is numbered (`arg0, arg1`), the argument is not an unclear one
    // (`this.config`), or the invocation selected no declaration at all
    // (`pick(1, 1)` ties, [JLS §15.12.2.5]).
    assert_eq!(render_hints(&fixture.hints()), "");
}

const PARAMETER_LIBRARY: &str = r#"package com.example;

class App {
    static void own(int value) {
    }

    void run(Lib lib) {
        lib.from(1);
        own(1);
    }
}
"#;

#[test]
fn parameter_names_library_member_omitted() {
    let fixture = library_file(PARAMETER_LIBRARY, &[("from", 1)]);
    let hints = fixture.hints();

    // The library member records no parameter names, so it gets no hint while
    // the source method beside it does.
    assert_eq!(hints.len(), 1);
    // The second `1);` — `own(1)`'s argument, after the library call's.
    assert_eq!(hints[0].offset, fixture.offset_start("1);", 1));
    assert_snapshot!("parameter_names_library_member", render_hints(&hints));
}

const PARAMETER_OPTIONAL: &str = r#"package java.util;

class Text {
}

class Optional<T> {
    static <T> Optional<T> empty() {
        return null;
    }
}

class Sample {
    static void use(Optional<Text> o) {
    }

    void run() {
        use(Optional.empty());
    }
}
"#;

/// `java.util.Optional.empty()` is unclear regardless of its owner's shape: the
/// empty container says nothing about what it is passed for. The fixture
/// declares `java.util.Optional` itself — the rule's test is the owner's
/// *canonical name*, which a source declaration carries exactly as a classpath
/// one does ([JLS §6.7]).
#[test]
fn parameter_names_optional_empty() {
    let fixture = test_file(PARAMETER_OPTIONAL);
    let hints = fixture.hints();

    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].offset, fixture.offset_start("Optional.empty()", 0));
    assert_snapshot!("parameter_names_optional_empty", render_hints(&hints));
}

const PARAMETER_VARARGS: &str = r#"package com.example;

class Text {
}

class Sample {
    static void printf(Text format, int... args) {
    }

    static Text text() {
        return null;
    }

    void run() {
        printf(text(), 1, 2);
    }
}
"#;

#[test]
fn parameter_names_varargs() {
    let fixture = test_file(PARAMETER_VARARGS);
    let hints = fixture.hints();

    // One hint for the whole trailing group, at its first element, named after
    // the varargs formal ([JLS §8.4.1]) — not one hint per element.
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].offset, fixture.offset_start("1, 2);", 0));
    assert_eq!(hints[0].kind, InlayHintKind::Parameter);
    assert_snapshot!("parameter_names_varargs", render_hints(&hints));
}

const CHAIN_TYPES: &str = r#"package com.example;

class Text {
}

class Count {
}

class List<T> {
}

class Stream<T> {
    Stream<T> filter() {
        return this;
    }

    Stream<Count> map() {
        return null;
    }

    List<T> collect() {
        return null;
    }
}

class Sample {
    Stream<Text> stream() {
        return null;
    }

    void run() {
        List<Text> result = stream()
                .filter()
                .map()
                .collect();
    }
}
"#;

#[test]
fn method_chain_types() {
    let fixture = test_file(CHAIN_TYPES);
    let hints = fixture.hints();

    // The chain's own type — the collected `List<Text>` — is what the
    // expression already reads as, so the outermost call gets nothing; the
    // run of `Stream<Text>` calls is annotated once, at its innermost element.
    assert_eq!(hints.len(), 2);
    // The second `stream()`: the first is the method declaration.
    assert_eq!(hints[0].offset, fixture.offset_end_at("stream()", 1));
    assert_eq!(hints[0].kind, InlayHintKind::Type);
    assert_eq!(hints[1].offset, fixture.offset_end(".map()"));

    assert_snapshot!("method_chain_types", render_hints(&hints));
}

const CHAIN_OMITTED: &str = r#"package com.example;

class Text {
}

class Count {
}

class List<T> {
}

class Stream<T> {
    Stream<T> filter() {
        return this;
    }

    Stream<Count> map() {
        return null;
    }

    List<T> collect() {
        return null;
    }
}

class Sample {
    Stream<Text> stream() {
        return null;
    }

    void singleLine() {
        List<Text> result = stream().filter().map().collect();
    }

    void oneType() {
        Stream<Text> s = stream()
                .filter()
                .filter();
    }

    void twoCalls() {
        Stream<Text> s = stream()
                .filter();
    }
}
"#;

#[test]
fn method_chain_omitted() {
    let fixture = test_file(CHAIN_OMITTED);

    // No chain qualifies: a single-line chain breaks no element onto its own
    // line, a chain whose calls all return one type has nothing to say, and a
    // two-call chain keeps only one element once its outermost call is dropped.
    assert_eq!(render_hints(&fixture.hints()), "");
}

/// The deferred detail of the first hint the file renders.
fn resolve_hint(fixture: &Fixture, index: usize) -> (InlayHint, ide::InlayHintDetail) {
    let analysis = fixture.analysis();
    let whole = TextRange::up_to(TextSize::of(fixture.text.as_str()));
    let hints = analysis
        .inlay_hints(fixture.file, whole, &InlayHintsConfig::default())
        .unwrap();
    let hint = hints[index].clone();
    let detail = analysis
        .inlay_hint_resolve(
            fixture.file,
            hint.offset,
            hint.kind,
            &InlayHintsConfig::default(),
        )
        .unwrap()
        .expect("the hint resolves");
    (hint, detail)
}

#[test]
fn inlay_hint_resolve_var_type() {
    let fixture = test_file(VAR_LOCAL);
    let (hint, detail) = resolve_hint(&fixture, 0);

    // The tooltip and the edit both render the *canonical* name: an accepted
    // edit inserts a type name that is valid wherever it lands ([JLS §6.5.5]),
    // where the label renders the simple name a reader expects.
    assert_eq!(detail.hint, hint);
    assert_eq!(detail.tooltip, "com.example.Text");
    assert_eq!(detail.edits.len(), 1);
    let edit = &detail.edits[0];
    assert_eq!(
        &fixture.text[edit.range.start().into()..edit.range.end().into()],
        "var"
    );
    assert_eq!(edit.new_text, "com.example.Text");

    // The part that rendered the class name carries it, so a client knows the
    // clickable span; the `": "` before it names nothing.
    assert_eq!(hint.label[0].class, None);
    assert_eq!(hint.label[1].class.as_deref(), Some("com.example.Text"));
    assert_eq!(hint.label[1].value, "Text");

    // The class the label named has a declaration the label navigates to.
    let target = fixture
        .analysis()
        .class_definition(fixture.file, "com.example.Text")
        .unwrap()
        .expect("the fixture declares com.example.Text");
    assert_eq!(target.name, "Text");
}

#[test]
fn inlay_hint_resolve_lambda_parameter() {
    let fixture = test_file(LAMBDA_SUPER_WILDCARD);
    let (hint, detail) = resolve_hint(&fixture, 0);

    // A concise parameter gets the type *inserted* before its name: the empty
    // range at the parameter's own start, which is what the LSP spec's
    // insertion edit is.
    assert_eq!(detail.tooltip, "com.example.Text");
    assert_eq!(detail.edits.len(), 1);
    let edit = &detail.edits[0];
    assert!(edit.range.is_empty());
    assert_eq!(edit.range.start(), hint.offset);
    assert_eq!(edit.new_text, "com.example.Text ");
}

#[test]
fn inlay_hint_resolve_missing_hint() {
    let fixture = test_file(VAR_LOCAL);

    // No hint is anchored there any more — the answer is `None`, not a guess.
    assert_eq!(
        fixture
            .analysis()
            .inlay_hint_resolve(
                fixture.file,
                fixture.offset_start("class Sample", 0),
                InlayHintKind::Type,
                &InlayHintsConfig::default(),
            )
            .unwrap(),
        None
    );
}

#[test]
fn inlay_hint_resolve_method_parameter_name() {
    let fixture = test_file(PARAMETER_VARARGS);
    let (_, detail) = resolve_hint(&fixture, 0);

    // A parameter hint's tooltip is the selected declaration's signature, and
    // it has nothing to accept — a name is not source.
    assert_eq!(
        detail.tooltip,
        "com.example.Sample.printf(com.example.Text, int[])"
    );
    assert!(detail.edits.is_empty());
    assert_eq!(detail.hint.label[0].value, "...args:");
    assert_eq!(detail.hint.label[0].class, None);
}

const LIB_SOURCE: &str = r#"package com.example;

public class Lib {
    public void from(int count) {
    }
}
"#;

const LIB_SOURCE_CALLER: &str = r#"package com.example;

class App {
    void run(Lib lib) {
        lib.from(1);
    }
}
"#;

#[test]
fn parameter_names_library_member_from_source() {
    let fixture = lib_source_file(LIB_SOURCE_CALLER, LIB_SOURCE, &[("from", 1)], true, None);
    let hints = fixture.hints();

    // The classfile records no parameter name; the *loaded* source of the
    // declaring class does, and it is the same declaration the merged hover
    // reads — `lib.from(count: 1)`.
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].offset, fixture.offset_end("lib.from("));
    assert_eq!(hints[0].kind, InlayHintKind::Parameter);
    let hover = fixture
        .analysis()
        .hover(fixture.file, fixture.offset_start("from", 0))
        .unwrap()
        .expect("the invocation's declaration hovers");
    assert!(
        hover.value.contains("count"),
        "the hint and the hover name the same parameter: {}",
        hover.value
    );

    assert_snapshot!(
        "parameter_names_library_member_from_source",
        render_hints(&hints)
    );
}

const LIB_SOURCE_STALE: &str = r#"package com.example;

public class Lib {
    public void from() {
    }
}
"#;

#[test]
fn parameter_names_library_member_arity_mismatch_omitted() {
    // A library source that no longer lines up with its classfile — the stale
    // jar every edit-then-build cycle produces — is not guessed at: the hint
    // needs a name for *every* parameter the selected member declares.
    let fixture = lib_source_file(
        LIB_SOURCE_CALLER,
        LIB_SOURCE_STALE,
        &[("from", 1)],
        true,
        None,
    );
    assert_eq!(render_hints(&fixture.hints()), "");
}

/// A second session — a fresh analysis host over the same library cache — that
/// has *not* materialized the declaring source still names the member: what it
/// reads is the first session's answer, not the archive.
///
/// Both sessions run in one workspace, because a library is identified by its
/// jar's path and the cache is keyed by it.
#[test]
fn parameter_names_member_from_a_previous_session() {
    let workspace = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let fixture = |loaded: bool, cache: Option<&std::path::Path>| {
        library_fixture(
            Some(&workspace),
            LIB_SOURCE_CALLER,
            Some((LIB_SOURCE, loaded)),
            &[("from", 1)],
            cache,
        )
    };

    // The session that resolves: the declaring source is loaded, so the names
    // come from it — and are persisted.
    let resolved = fixture(true, Some(cache.path()));
    assert_eq!(render_hints(&resolved.hints()), "Parameter @75 count:");
    drop(resolved);

    // The same session with the source *not* loaded answers nothing: the
    // archive entry exists, but no session has read the file it names.
    let cold = fixture(false, None);
    assert_eq!(
        render_hints(&cold.hints()),
        "",
        "without a cache, a pending archive entry names nothing"
    );
    drop(cold);

    // The next session over the same cache answers the name anyway.
    let remembered = fixture(false, Some(cache.path()));
    assert_eq!(render_hints(&remembered.hints()), "Parameter @75 count:");
}

/// The pending state of a library member's names is *reported* so the hint
/// layer can materialize the declaring source and render the name on the first
/// request. A call whose arguments would render no hint anyway — and a library
/// that ships no sources at all — collects nothing, so a request never
/// materializes a source no hint waits on.
#[test]
fn library_member_pending_source_is_collected_for_names() {
    let deferred = |fixture: &Fixture| {
        let whole = TextRange::up_to(TextSize::of(fixture.text.as_str()));
        fixture
            .analysis()
            .inlay_hint_pending_library_files(fixture.file, whole, &InlayHintsConfig::default())
            .unwrap()
    };

    // The declaring source exists in the archive but no session loaded it, and
    // the literal argument is one whose role a reader cannot infer: the hint
    // waits on the file, so the request defers on it.
    let fixture = lib_source_file(LIB_SOURCE_CALLER, LIB_SOURCE, &[("from", 1)], false, None);
    let pending = deferred(&fixture);
    assert_eq!(pending.len(), 1, "{pending:#?}");
    let ide::LibraryFileRef::Source { entry, path, .. } = &pending[0] else {
        panic!("a sourced library defers on its archive entry: {pending:#?}");
    };
    assert_eq!(entry.as_ref(), "com/example/Lib.java");
    assert!(path.to_string().ends_with("com/example/Lib.java"), "{path}");

    // A variable argument speaks for itself: the call renders no hint, so its
    // source is never materialized for it.
    let clear = "package com.example;\n\nclass App {\n    void run(Lib lib, int size) {\n        lib.from(size);\n    }\n}\n";
    let fixture = lib_source_file(clear, LIB_SOURCE, &[("from", 1)], false, None);
    assert!(deferred(&fixture).is_empty());

    // A library that ships no sources can never name the member: there is no
    // file to load.
    let fixture = library_file(LIB_SOURCE_CALLER, &[("from", 1)]);
    assert!(deferred(&fixture).is_empty());
}

/// A library that ships no sources at all records that it names nothing — and
/// the record is keyed on the sources it was made for, so attaching sources to
/// the same library is a fresh question rather than a cached "no".
#[test]
fn parameter_names_sourceless_library_answer_is_invalidated_by_new_sources() {
    let workspace = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();

    // A session with no sources attached: the library can name nothing.
    let sourceless = library_fixture(
        Some(&workspace),
        LIB_SOURCE_CALLER,
        None,
        &[("from", 1)],
        Some(cache.path()),
    );
    assert_eq!(render_hints(&sourceless.hints()), "");
    drop(sourceless);

    // The same library with sources attached, in a later session: the cached
    // answer belongs to the source-less library, so it is not reused.
    let sourced = library_fixture(
        Some(&workspace),
        LIB_SOURCE_CALLER,
        Some((LIB_SOURCE, true)),
        &[("from", 1)],
        Some(cache.path()),
    );
    assert_eq!(render_hints(&sourced.hints()), "Parameter @75 count:");
}
