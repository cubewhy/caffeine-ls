//! The Kotlin item type layer: what a declaration's written type resolves to.
//!
//! The fixture's standard library is hand-encoded ([`kotlin_stdlib_classes`]),
//! like the JDK fixture, so the suite stays hermetic: `String`, `Int`, `Unit`
//! and `List` resolve through the *default imports* against a classpath the
//! test controls, which is exactly the claim these tests make.

use base_db::{FileChange, FileSourceRootInput, SourceDatabase, SourceRoot, SourceRootId};
use hir::SourceSetId;
use hir_ty::Ty;
use tempfile::TempDir;
use triomphe::Arc;
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

mod common;
use common::{ClassSpec, DeprecationSpec, TestDatabase, build_jar, jdk_fixture};

/// A minimal Kotlin standard library: the classifiers the default imports are
/// claimed to provide, with the shapes kotlinc compiles them to (`kotlin.Int`
/// is a class, `kotlin.collections.List` an interface with one type parameter).
fn kotlin_stdlib_classes() -> Vec<ClassSpec<'static>> {
    let class = |fqn: &'static str,
                 super_class: Option<&'static str>,
                 interfaces: &'static [&'static str],
                 access: u16| ClassSpec {
        fqn,
        super_class,
        interfaces,
        access,
        fields: &[],
        field_access: &[],
        methods: &[],
        method_sigs: &[],
        method_access: &[],
        sig: None,
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        method_deprecations: &[],
        method_defaults: &[],
    };
    vec![
        class("kotlin/Any", None, &[], 0x0021),
        class("kotlin/String", Some("kotlin/Any"), &[], 0x0031),
        class("kotlin/Int", Some("kotlin/Number"), &[], 0x0031),
        class("kotlin/Number", Some("kotlin/Any"), &[], 0x0421),
        class("kotlin/Boolean", Some("kotlin/Any"), &[], 0x0031),
        class("kotlin/Unit", Some("kotlin/Any"), &[], 0x0031),
        class("kotlin/Nothing", Some("kotlin/Any"), &[], 0x0031),
        // `interface List<out E>` — an interface, hence `ACC_INTERFACE |
        // ACC_ABSTRACT`.
        class("kotlin/collections/List", Some("kotlin/Any"), &[], 0x0601),
        // `interface Function1<in P1, out R>`.
        class("kotlin/Function1", Some("kotlin/Any"), &[], 0x0601),
        // `interface Iterable<out T>` with `iterator()`.
        ClassSpec {
            methods: &[("iterator", "()Ljava/util/Iterator;")],
            ..class(
                "kotlin/collections/Iterable",
                Some("kotlin/Any"),
                &[],
                0x0601,
            )
        },
    ]
}

/// A library holding `specs`, plus its id.
fn library(
    dir: &TempDir,
    name: &str,
    specs: &[ClassSpec<'static>],
) -> (hir::LibraryId, AbsPathBuf) {
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().join(name)).unwrap();
    build_jar(&path, specs);
    let abs = AbsPathBuf::assert_utf8(path.as_std_path().to_owned());
    (
        hir::LibraryId::from_file_path(path.as_std_path()).unwrap(),
        abs,
    )
}

/// A database with the JDK fixture, a hand-encoded Kotlin stdlib and one
/// Kotlin source root whose classpath carries both.
fn kotlin_fixture(files: &[(&str, &str)]) -> (TestDatabase, FileId) {
    let dir = TempDir::new().unwrap();
    let jdk = jdk_fixture();
    let (stdlib_id, stdlib_path) = library(&dir, "kotlin-stdlib.jar", &kotlin_stdlib_classes());

    let mut db = TestDatabase::default();
    let mut file_set = FileSet::default();
    for (i, (path, _)) in files.iter().enumerate() {
        file_set.insert(
            FileId::from_raw((i + 1) as u32),
            VfsPath::from(AbsPathBuf::assert_utf8((*path).into())),
        );
    }
    let root = SourceRoot::new(file_set);
    let mut change = FileChange::default();
    change.set_roots(vec![root]);
    for (i, (_, text)) in files.iter().enumerate() {
        change.change_file(FileId::from_raw((i + 1) as u32), Some((*text).to_owned()));
    }
    change.apply(&mut db);

    let source_set = SourceSetId {
        project: hir::ProjectId(0),
        kind: hir::SourceSetKind::Main,
    };
    let mut data = hir::ProjectGraphData::default();
    data.libraries.insert(
        jdk.lib,
        hir::LibraryInfo::new(
            hir::LibraryKind::Jar,
            AbsPathBuf::assert_utf8(jdk.jar.as_std_path().to_owned()),
        ),
    );
    data.libraries.insert(
        stdlib_id,
        hir::LibraryInfo::new(hir::LibraryKind::Jar, stdlib_path),
    );
    data.jdk_libraries.push(jdk.lib);
    data.source_sets.insert(
        source_set.clone(),
        Arc::new(hir::Classpath {
            entries: vec![
                hir::ClasspathEntry::Library(jdk.lib),
                hir::ClasspathEntry::Library(stdlib_id),
            ],
        }),
    );
    data.source_root_to_source_set
        .insert(SourceRootId(0), source_set.clone());
    data.source_root_dirs.insert(
        SourceRootId(0),
        AbsPathBuf::assert_utf8(dir.path().to_string_lossy().to_string().into()),
    );
    hir::set_project_graph(&mut db, data);
    std::mem::forget(dir);
    (db, FileId::from_raw(1))
}

/// The rendered names and types of the file's declaration items.
fn render_types(db: &TestDatabase, file: FileId) -> String {
    let tree = hir::file_item_tree(db, file);
    let tree = tree.as_kotlin().expect("a Kotlin file").clone();
    let mut lines = Vec::new();
    for (id, data) in tree.items.iter() {
        if data.name().is_none()
            && !matches!(
                data,
                hir_def::kotlin::item_tree::KotlinItemData::Accessor(_)
            )
        {
            continue;
        }
        let ty = hir_ty::kotlin_item_ty(db, file, hir_expand::ids::ItemId(id));
        lines.push(format!(
            "{} {}: {}",
            data.label(),
            data.name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "_".to_owned()),
            hir_ty::display_kotlin(db, ty)
        ));
    }
    lines.join("\n")
}

fn check(src: &str) -> String {
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", src)]);
    render_types(&db, file)
}

#[test]
fn declared_types_resolve_through_the_default_imports() {
    insta::assert_snapshot!(
        "kotlin_item_types_declared",
        check(
            r#"
class Sample(val x: String, val y: String?, val z: List<Int>) {
    fun f(a: Int): String = ""

    var fn: (Int) -> String = { "" }

    val p: List<*> = listOf()

    val o: List<out Number> = listOf()

    val nested: List<List<String>> = listOf()
}
"#
        )
    );
}

/// A name that resolves to nothing is the error type — kotlinc reports
/// `unresolved reference 'Missing'` for the same source — and resolving it must
/// not panic (the resolution walks the classpath and the file's scopes).
#[test]
fn an_unresolved_reference_is_the_error_type() {
    let rendered = check("val broken: Missing = TODO()\n");
    assert!(
        rendered.contains("val broken: <error>"),
        "an unresolved type renders as the error type: {rendered}"
    );
}

/// A type parameter in scope wins over any declaration of the same name, and
/// `T?` keeps the parameter while adding nullability (KLS
/// `declarations.html#type-parameters`).
#[test]
fn type_parameters_resolve_in_their_own_scope() {
    insta::assert_snapshot!(
        "kotlin_item_types_type_parameters",
        check(
            r#"
class Box<T : Any> {
    val value: T? = null

    fun <R> map(f: (T) -> R): R = TODO()
}
"#
        )
    );
}

// -- subtyping and member resolution ----------------------------------------

/// The subtyping and overload cases, each confirmed with kotlinc 2.4.20 first.
///
/// The oracle is the probe (`val probe: String = <expr>` reports
/// `initializer type mismatch: expected 'X', actual 'Y'`) and the clean
/// compile: kotlinc accepts
/// `class L<out T>(val v: T); val a: L<Number> = L(1); val b: L<Any> = a` and
/// rejects `val probe: Any = (null as String?)`, which is what these cases
/// assert through [`hir_ty::kotlin_subtype`].
mod subtyping {
    use super::*;

    /// A database plus the `Ty` of a type written in the fixture, for a
    /// subtyping question.
    fn ty_of(db: &TestDatabase, file: FileId, name: &str) -> Ty {
        let tree = hir::file_item_tree(db, file);
        let tree = tree.as_kotlin().expect("a Kotlin file").clone();
        for (id, data) in tree.items.iter() {
            if data.name().map(|n| n.as_str()) == Some(name) {
                // The declared type, nullability included — the caller strips
                // it only where the case is about the non-null half.
                return hir_ty::kotlin_item_ty(db, file, hir_expand::ids::ItemId(id));
            }
        }
        panic!("no item named {name}")
    }

    fn scope(db: &TestDatabase, file: FileId) -> hir::ResolutionScope {
        hir::ResolutionScope::SourceSet(
            hir::source_set_for_file(db, file).expect("a mapped source set"),
        )
    }

    #[test]
    fn nullability_rules_match_the_compiler() {
        let (db, file) = kotlin_fixture(&[(
            "/src/main/kotlin/Sample.kt",
            "val plain: String = \"\"\nval opt: String? = null\nval any: Any? = null\nval anyPlain: Any = 1 as Any\n",
        )]);
        let scope = scope(&db, file);
        let string = ty_of(&db, file, "plain");
        let optional = ty_of(&db, file, "opt");
        let any = ty_of(&db, file, "any");
        let any_plain = ty_of(&db, file, "anyPlain");

        // `String <: String?` — the probe: `val probe: String? = x` compiles.
        let string_opt = Ty::nullable(&db, string);
        assert!(hir_ty::kotlin_subtype(&db, &scope, &string, &string_opt));
        // A nullable value is *not* a non-null one — the probe:
        // `val probe: String = x` reports `actual 'String?'`.
        assert!(!hir_ty::kotlin_subtype(&db, &scope, &string_opt, &string));
        // `String? <: Any?` but not `<: Any` — kotlinc:
        // `null cannot be a value of a non-null type 'Any'`. `any` is declared
        // `Any?`, so the first assertion says a nullable type is a subtype of
        // the nullable root while the second says it is not a subtype of the
        // non-null one.
        assert!(any.is_nullable(&db));
        assert!(hir_ty::kotlin_subtype(&db, &scope, &string_opt, &any));
        assert!(!hir_ty::kotlin_subtype(
            &db,
            &scope,
            &string_opt,
            &any_plain
        ));
        assert!(hir_ty::kotlin_subtype(&db, &scope, &string, &any));
        let _ = optional;
    }

    #[test]
    fn declaration_site_variance_matches_the_compiler() {
        let source = "class L<out T>(val v: T)\nclass Inv<T>(val v: T)\nval a: L<Int> = L(1)\nval b: L<Any> = L(1)\nval c: Inv<Int> = Inv(1)\nval d: Inv<Any> = Inv(1)\n";
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let scope = scope(&db, file);
        let int_l = ty_of(&db, file, "a");
        let any_l = ty_of(&db, file, "b");
        let int_inv = ty_of(&db, file, "c");
        let any_inv = ty_of(&db, file, "d");

        // kotlinc: `val b: L<Any> = a` compiles (covariant parameter), while
        // `val d: Inv<Any> = c` fails with `initializer type mismatch`.
        assert!(
            hir_ty::kotlin_subtype(&db, &scope, &int_l, &any_l),
            "L<Int> <: L<Any> for an `out T` parameter"
        );
        assert!(
            !hir_ty::kotlin_subtype(&db, &scope, &int_inv, &any_inv),
            "Inv<Int> !<: Inv<Any> for an invariant parameter"
        );
    }
}

// -- body inference ---------------------------------------------------------

/// The inferred types and diagnostics of a fixture's bodies, rendered one
/// line per inferred expression in arena order plus one per error.
fn render_bodies(db: &TestDatabase, file: FileId) -> String {
    let tree = hir::file_item_tree(db, file);
    let tree = tree.as_kotlin().expect("a Kotlin file").clone();
    let mut lines = Vec::new();
    for (id, data) in tree.items.iter() {
        let Some(body) = data.body_id() else {
            continue;
        };
        let _ = body;
        let types = hir_ty::kotlin_body_types(db, file, hir_expand::ids::ItemId(id));
        for (expr, ty) in types.exprs.iter() {
            lines.push(format!(
                "e{}: {}",
                expr.0.0,
                hir_ty::display_kotlin(db, *ty)
            ));
        }
        for diagnostic in &types.diagnostics {
            lines.push(format!(
                "{}: {}",
                diagnostic.code().as_str(),
                diagnostic.message(db)
            ));
        }
    }
    lines.sort();
    lines.join("\n")
}

/// Every fixture here is a source kotlinc 2.4.20 either accepts (exit 0, no
/// output) or rejects with exactly the message the diagnostic renders.
#[test]
fn inferred_expression_types_match_the_compiler() {
    insta::assert_snapshot!("kotlin_infer_expression_types", {
        // Source-declared members only: a *library* member is not
        // collected yet (see `hir_ty::kotlin::method`), and this fixture
        // pins the inference, not that gap.
        let source = r#"
class Holder(val value: Int)

fun compute(holder: Holder): String {
    val first = holder.value
    val name: String = "x"
    val optional: String? = name
    val asserted = optional!!
    val chosen = optional ?: name
    val text = "value is $first"
    val returned = if (first > 0) first else 0
    val sum = first + 1
    return name
}
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "the diagnostics of a clean fixture: {rendered}"
        );
        rendered
    });
}

/// The negative cases, each of which kotlinc rejects with the message the
/// diagnostic renders:
///
/// * `val x: String = null` →
///   `null cannot be a value of a non-null type 'String'.`
/// * `val x: String = listOf(1, 2)` →
///   `initializer type mismatch: expected 'String', actual 'List<Int>'.`
/// * `x` unresolved → `unresolved reference 'x'.`
#[test]
fn diagnostics_match_the_compiler_wordings() {
    let source = r#"
fun broken() {
    val a: String = null
    val b: String = missing()
    val c: String = 1
    undefinedName
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    for expected in [
        "kotlin.nullability-mismatch: null cannot be a value of a non-null type 'String'.",
        "kotlin.unresolved-reference: unresolved reference 'undefinedName'.",
        "kotlin.type-mismatch: initializer type mismatch: expected 'String', actual 'Int'.",
    ] {
        assert!(
            rendered.contains(expected),
            "expected {expected:?} in:\n{rendered}"
        );
    }
}
