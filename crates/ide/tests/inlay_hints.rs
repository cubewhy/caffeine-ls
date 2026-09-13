//! Insta snapshot tests for the inlay hints of `ide` (see [`ide::inlay_hints`]).

use triomphe::Arc;

use ide::{
    Analysis, AnalysisHost, Change, Classpath, InlayHint, InlayHintKind, InlayHintsConfig,
    ProjectGraphData, SourceSetId,
};
use ide_db::base_db::{SourceRoot, SourceRootId};
use insta::assert_snapshot;
use rowan::{TextRange, TextSize};
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

fn main_source_set(project: u32) -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(project),
        kind: project_model::SourceSetKind::Main,
    }
}

struct Fixture {
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
        let start: u32 = self.offset_start(needle, 0).into();
        TextSize::new(start + needle.len() as u32)
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
