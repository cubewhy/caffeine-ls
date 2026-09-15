//! The Kotlin item type layer: what a declaration's written type resolves to.
//!
//! The fixture's standard library is hand-encoded ([`kotlin_stdlib_classes`]),
//! like the JDK fixture, so the suite stays hermetic: `String`, `Int`, `Unit`
//! and `List` resolve through the *default imports* against a classpath the
//! test controls, which is exactly the claim these tests make.

use base_db::{FileChange, FileSourceRootInput, SourceDatabase, SourceRoot, SourceRootId};
use hir::SourceSetId;
use hir_ty::{Ty, TyKind};
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
        // `abstract class Enum<E>`, the implicit supertype of an `enum class`.
        class("kotlin/Enum", Some("kotlin/Any"), &[], 0x0421),
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

// -- platform types, mapped classifiers and the resolution gaps --------------

/// The Java↔Kotlin boundary of the type model.
///
/// Every case is checked against kotlinc 2.4.20 (JRE 21.0.11) first, with the
/// probe fixture `J.java` +
///
/// ```kotlin
/// typealias Handler<T> = (T) -> Unit
/// abstract class Box<T>(val v: T) : List<T>
/// enum class Direction { NORTH }
/// class Outer { class Inner }
/// val ints: List<Int> = listOf()
/// val handler: Handler<Int> = {}
/// val direction: Enum<Direction> = Direction.NORTH
/// fun <T : Number> bounded(t: T): T = t
/// fun takesInts(x: List<Int>) {}
/// fun takesStrings(x: List<String>) {}
/// fun takesNumbers(x: List<Number>) {}
/// fun boxProbe(boxed: Box<Int>) { takesInts(boxed); takesNumbers(boxed) }
/// fun probes() {
///     val a: String = J.javaMethod()      // a Java `String javaMethod()`
///     val b: String? = J.javaMethod()
///     val c: List<Number> = ints
///     val f: String? = null
///     val h: Number = bounded(1)
///     val i: Outer.Inner = Outer.Inner()
/// }
/// ```
///
/// which compiles clean:
///
/// ```text
/// $ JAVA_HOME=/home/cubewhy/.jdks/temurin-21.0.11 kotlinc -d out J.java Ok.kt
/// (exit status 0, no diagnostics)
/// ```
///
/// and a `Bad.kt` beside it whose three probes the compiler rejects with
/// exactly these messages:
///
/// ```text
/// Bad.kt:2:18: error: argument type mismatch: actual type is 'Box<Int>', but 'List<String>' was expected.
///     takesStrings(boxed)
///                  ^^^^^
/// Bad.kt:6:21: error: null cannot be a value of a non-null type 'String'.
///     val g: String = null
///                     ^^^^
/// Bad.kt:10:13: error: type arguments are not allowed for type parameters.
///     val x: T<Int>? = null
///             ^^^^^
/// ```
mod interop_types {
    use super::*;
    use hir_ty::kotlin::ty::ty_from_java;

    fn scope(db: &TestDatabase, file: FileId) -> hir::ResolutionScope {
        hir::ResolutionScope::SourceSet(
            hir::source_set_for_file(db, file).expect("a mapped source set"),
        )
    }

    /// A classfile reference type is a *platform* type: `java.lang.String` is
    /// `kotlin.String!` — the flexible type `String..String?` — and a value of
    /// it is usable both where a `String` and where a `String?` is expected.
    /// kotlinc accepts `val a: String = javaMethod()` and
    /// `val b: String? = javaMethod()` for a Java `String javaMethod()`.
    #[test]
    fn a_classfile_type_becomes_a_platform_type() {
        let (db, file) = kotlin_fixture(&[(
            "/src/main/kotlin/Sample.kt",
            "val plain: String = \"\"\nval optional: String? = null\n",
        )]);
        let scope = scope(&db, file);
        let plain = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "plain"));
        let optional = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "optional"));

        let java = ty_from_java(&db, Ty::reference(&db, "java.lang.String", Vec::new()));
        assert_eq!(
            hir_ty::display_kotlin(&db, java).to_string(),
            "String..String?",
            "`java.lang.String` is the platform type `String!`"
        );
        assert!(
            hir_ty::kotlin_subtype(&db, &scope, &java, &plain),
            "a platform `String!` is usable as `String`"
        );
        assert!(
            hir_ty::kotlin_subtype(&db, &scope, &java, &optional),
            "a platform `String!` is usable as `String?`"
        );
        // The mapping is part of the conversion: the *lower* half is
        // `kotlin.String`, not `java.lang.String`.
        assert_eq!(
            hir_ty::display_kotlin(
                &db,
                ty_from_java(&db, Ty::primitive(&db, syntax::stub::PrimitiveType::Int))
            )
            .to_string(),
            "Int",
            "`int` denotes `kotlin.Int`"
        );
    }

    /// The standard-library variance the classfile cannot carry comes from the
    /// mapped-variance table: `interface List<out E>`.
    ///
    /// kotlinc accepts `val c: List<Number> = ints` for a `List<Int>` — `List`
    /// is declared `List<out E>` — and a `Box<Int> : List<T>` is accepted
    /// where `List<Int>` and `List<Number>` are expected but rejected for
    /// `List<String>` (`argument type mismatch: actual type is 'Box<Int>', but
    /// 'List<String>' was expected.`).
    #[test]
    fn the_mapped_variance_table_makes_the_standard_library_covariant() {
        let source = "val ints: List<Int> = listOf()\nval numbers: List<Number> = listOf()\nval anys: List<Any> = listOf()\n";
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let scope = scope(&db, file);
        let ints = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "ints"));
        let numbers = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "numbers"));
        let anys = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "anys"));
        assert!(
            hir_ty::kotlin_subtype(&db, &scope, &ints, &numbers),
            "List<Int> <: List<Number> for the `out E` of kotlin.collections.List"
        );
        assert!(
            hir_ty::kotlin_subtype(&db, &scope, &ints, &anys),
            "List<Int> <: List<Any>"
        );
    }

    /// A supertype list is declared over the class's own parameters, so a
    /// receiver's arguments are substituted into it: `class Box<T> : List<T>`
    /// makes `Box<Int> <: List<Int>` — kotlinc accepts `takesInts(boxed)` for
    /// a `Box<Int>`. The substitution is what the *covariance* of `List` then
    /// widens to `List<Number>`, while `List<String>` stays out of reach.
    #[test]
    fn a_source_supertype_list_is_substituted_with_the_receivers_arguments() {
        let source = "class Box<T>(val v: T) : List<T>\nval box: Box<Int> = TODO()\nval ints: List<Int> = TODO()\nval numbers: List<Number> = TODO()\nval strings: List<String> = TODO()\n";
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let scope = scope(&db, file);
        let boxed = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "box"));
        let ints = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "ints"));
        let numbers = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "numbers"));
        assert!(
            hir_ty::kotlin_subtype(&db, &scope, &boxed, &ints),
            "Box<Int> <: List<Int>"
        );
        let strings = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "strings"));
        assert!(
            !hir_ty::kotlin_subtype(&db, &scope, &boxed, &strings),
            "Box<Int> !<: List<String>: the substituted argument is Int"
        );
    }

    /// The null type is a subtype of every nullable type and of nothing else:
    /// kotlinc accepts `val f: String? = null` and rejects
    /// `val g: String = null` with
    /// `null cannot be a value of a non-null type 'String'.`
    #[test]
    fn null_is_a_subtype_of_every_nullable_type_only() {
        let source = "val optional: String? = null\nval plain: String = \"\"\n";
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let scope = scope(&db, file);
        let null = Ty::null(&db);
        let optional = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "optional"));
        let plain = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "plain"));
        assert!(optional.is_nullable(&db));
        assert!(hir_ty::kotlin_subtype(&db, &scope, &null, &optional));
        assert!(
            !hir_ty::kotlin_subtype(&db, &scope, &null, &plain),
            "`null` is not a value of a non-null type"
        );
    }

    /// An unresolved name is the *error* type, and the error type absorbs every
    /// subtyping question: a name that could not be resolved must not *also*
    /// report a mismatch downstream. kotlinc reports the unresolved reference
    /// and nothing else for `val x: String = missing()`.
    #[test]
    fn the_error_type_absorbs_subtyping_in_both_positions() {
        let (db, file) = kotlin_fixture(&[(
            "/src/main/kotlin/Sample.kt",
            "val x: String = \"\"\nval missing: Missing = TODO()\n",
        )]);
        let scope = scope(&db, file);
        let string = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "x"));
        let missing = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "missing"));
        assert!(
            missing.is_error(&db),
            "an unresolved name is the error type"
        );
        assert!(hir_ty::kotlin_subtype(&db, &scope, &missing, &string));
        assert!(hir_ty::kotlin_subtype(&db, &scope, &string, &missing));
    }

    /// A type parameter reaches its declared bounds: `fun <T : Number>
    /// bounded(t: T): T` compiles and passes `T` where a `Number` is expected
    /// (`val h: Number = bounded(1)`), which is what `T <: U` means
    /// ([KLS
    /// `type-system.html#type-parameters`](https://kotlinlang.org/spec/type-system.html#type-parameters)).
    #[test]
    fn a_type_parameter_is_bounded_by_its_declared_bounds() {
        let source =
            "class Box<T : Number>(val v: T)\nval n: Number = TODO()\nval s: String = \"\"\n";
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let scope = scope(&db, file);
        let number = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "n"));
        let string = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "s"));
        let param = item_named(&db, file, "v");
        let param_ty = hir_ty::kotlin_item_ty(&db, file, param);
        assert!(
            hir_ty::kotlin_subtype(&db, &scope, &param_ty, &number),
            "`T : Number` makes T a subtype of Number"
        );
        assert!(!hir_ty::kotlin_subtype(&db, &scope, &param_ty, &string));
    }

    /// A `typealias` is expanded with its arguments, not named: `Handler<Int>`
    /// *is* `(Int) -> Unit`, which is what kotlinc's `val handler: Handler<Int>
    /// = {}` accepts ([KLS
    /// `declarations.html#type-alias`](https://kotlinlang.org/spec/declarations.html#type-alias)).
    #[test]
    fn a_type_alias_is_expanded_with_its_arguments() {
        let source = "typealias Handler<T> = (T) -> Unit\ntypealias Name = String\nval h: Handler<Int> = {}\nval n: Name = \"\"\n";
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let h = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "h"));
        let n = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "n"));
        assert_eq!(hir_ty::display_kotlin(&db, h).to_string(), "(Int) -> Unit");
        assert_eq!(hir_ty::display_kotlin(&db, n).to_string(), "String");
    }

    /// A type parameter is not a classifier and takes no type arguments ([KLS
    /// `type-system.html#classifier-types`](https://kotlinlang.org/spec/type-system.html#classifier-types)):
    /// `type arguments are not allowed for type parameters.` (kotlinc 2.4.20),
    /// and `T<Int>` must not silently resolve to `T`, which a classifier
    /// lookup without this guard would do.
    #[test]
    fn a_type_parameter_with_type_arguments_is_the_error_type() {
        let (db, file) = kotlin_fixture(&[(
            "/src/main/kotlin/Sample.kt",
            "class Box<T>(val v: T<Int>)\n",
        )]);
        let v = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "v"));
        assert!(v.is_error(&db), "`T<Int>` is an error: {v:?}");
    }

    /// An `enum class` has the implicit supertype `kotlin.Enum<E>` ([KLS
    /// `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)),
    /// with itself as the argument — kotlinc accepts
    /// `val direction: Enum<Direction> = Direction.NORTH`.
    #[test]
    fn an_enum_class_has_the_implicit_enum_supertype() {
        let (db, file) = kotlin_fixture(&[(
            "/src/main/kotlin/Sample.kt",
            "enum class Direction { NORTH }\n",
        )]);
        let direction = item_named(&db, file, "Direction");
        let supertypes: Vec<String> = hir_ty::kotlin_supertypes(&db, file, direction)
            .iter()
            .map(|ty| hir_ty::display_kotlin(&db, *ty).to_string())
            .collect();
        assert!(
            supertypes.iter().any(|name| name == "Enum<Direction>"),
            "an enum is a subtype of `Enum<Direction>`: {supertypes:?}"
        );
    }

    /// A nested classifier resolves through its enclosing declaration, and a
    /// name written inside the enclosing class finds the nested one first —
    /// kotlinc accepts `val i: Outer.Inner = Outer.Inner()`.
    #[test]
    fn a_nested_classifier_resolves_through_its_enclosing_declaration() {
        let source = "class Outer {\n    class Inner\n    val v: Inner = TODO()\n}\nval w: Outer.Inner? = null\n";
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let v = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "v"));
        let w = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "w"));
        // The *canonical* name says which classifier resolved — the display
        // renders simple names, which a nested and a top-level class share.
        assert_eq!(reference_name(&db, &v), "Outer.Inner");
        assert_eq!(
            reference_name(&db, &w.strip_nullability(&db)),
            "Outer.Inner",
            "`Outer.Inner?` names the same classifier"
        );
    }

    /// A Kotlin class is named by the *source symbol index*, so the Java layer
    /// can key it — and resolving it must not index the empty Java item tree
    /// (`ClassKey::of_ty` used to).
    #[test]
    fn the_java_layer_keys_a_kotlin_class_by_its_name() {
        let (db, file) = kotlin_fixture(&[(
            "/src/main/kotlin/Util.kt",
            "package a\n\nclass Util(val n: Int)\n",
        )]);
        let util = item_named(&db, file, "Util");
        assert_eq!(
            hir::source_class_fqn(&db, file, util).map(|name| name.to_string()),
            Some("a.Util".to_owned())
        );
        let ty = Ty::reference(&db, "a.Util", Vec::new());
        let key = hir_ty::ClassKey::of_ty(&db, &ty).expect("a reference type");
        assert_eq!(
            hir_ty::ClassKey::display_name(&key, &db).to_string(),
            "a.Util"
        );
    }

    /// The canonical name of a reference type, for the assertions that must
    /// tell two same-named classifiers apart.
    fn reference_name(db: &TestDatabase, ty: &Ty) -> String {
        match ty.kind(db) {
            TyKind::Reference { name, .. } => name.to_string(),
            other => panic!("not a reference type: {other:?}"),
        }
    }

    /// The item id of the declaration named `name` in the fixture's file.
    fn item_named(db: &TestDatabase, file: FileId, name: &str) -> hir_expand::ids::ItemId {
        let tree = hir::file_item_tree(db, file);
        let tree = tree.as_kotlin().expect("a Kotlin file").clone();
        for (id, data) in tree.items.iter() {
            if data.name().map(|n| n.as_str()) == Some(name) {
                return hir_expand::ids::ItemId(id);
            }
        }
        panic!("no item named {name} in the fixture");
    }
}
