//! Insta snapshot tests for the inlay hints of `ide` (see [`ide::inlay_hints`]).

use triomphe::Arc;

use ide::{
    Analysis, AnalysisHost, Change, Classpath, ClasspathEntry, InlayHint, InlayHintKind,
    InlayHintsConfig, LibraryId, LibraryInfo, LibraryKind, ProjectGraphData, SourceSetId,
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

/// A workspace file plus one dependency jar holding `com.example.Lib` with the
/// given members. The jar's members are *library* members: no classfile
/// without a `MethodParameters` attribute records parameter names, which is
/// exactly the case the parameter-name hints refuse to guess at.
fn library_file(text: &str, lib_methods: &[(&str, usize)]) -> Fixture {
    let dir = tempfile::TempDir::new().unwrap();
    let base = dir.path().to_path_buf();
    let jar = base.join("lib/deps.jar");
    let class = class_bytes("com/example/Lib", "java/lang/Object", &[], lib_methods);
    build_jar(&jar, &[("com/example/Lib.class", class)]).unwrap();

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
    change.set_roots(vec![SourceRoot::new(file_set)]);

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

    let mut host = AnalysisHost::new();
    change.set_project_graph(data);
    host.apply_change(change);

    Fixture {
        _dir: Some(dir),
        host,
        file,
        text: text.to_owned(),
    }
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

const VAR_OMITTED: &str = r#"package com.example;

class Text {
    static Text text() {
        return null;
    }
}

class Sample {
    void omitted() {
        var i = 1;
        var s = "a" + text();
        var o = new Text();
        var a = new int[] { 1 };
        var c = (Text) text();
        Text typed = text();
    }
}
"#;

#[test]
fn var_type_omitted() {
    let fixture = test_file(VAR_OMITTED);

    // Nothing to render: every initializer either describes its own type or is
    // no `var` declaration at all.
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
