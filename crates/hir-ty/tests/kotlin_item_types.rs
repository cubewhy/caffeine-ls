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

/// A database with the JDK fixture, a hand-encoded Kotlin stdlib and one
/// Kotlin source root whose classpath carries both.
fn kotlin_fixture(files: &[(&str, &str)]) -> (TestDatabase, FileId) {
    kotlin_fixture_with(files, common::interop_classes())
}

/// [`kotlin_fixture`] with `extra` classes added to the classpath, for the
/// interop fixtures whose Java shapes the JDK fixture does not carry.
fn kotlin_fixture_with(
    files: &[(&str, &str)],
    extra: Vec<ClassSpec<'static>>,
) -> (TestDatabase, FileId) {
    let dir = TempDir::new().unwrap();
    let jdk = jdk_fixture();
    let (stdlib_id, stdlib_path) =
        common::fixture_library(&dir, "kotlin-stdlib.jar", &common::kotlin_stdlib_classes());
    let (extra_id, extra_path) = common::fixture_library(&dir, "java-interop.jar", &extra);

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
    data.libraries.insert(
        extra_id,
        hir::LibraryInfo::new(hir::LibraryKind::Jar, extra_path),
    );
    data.jdk_libraries.push(jdk.lib);
    data.source_sets.insert(
        source_set.clone(),
        Arc::new(hir::Classpath {
            entries: vec![
                hir::ClasspathEntry::Library(jdk.lib),
                hir::ClasspathEntry::Library(stdlib_id),
                hir::ClasspathEntry::Library(extra_id),
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
    // The database outlives the fixtures; their jars are read lazily.
    std::mem::forget(dir);
    jdk.keep_alive();
    (db, FileId::from_raw(1))
}

/// The rendered names and types of the file's declaration items.
fn render_types(db: &TestDatabase, file: FileId) -> String {
    let tree = hir::hir_def::kotlin::plugin::tree(db, file).expect("a Kotlin file");
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
/// The declared type of the fixture's declaration named `name` — for an object
/// literal's class, `<anonymous>`.
fn ty_of(db: &TestDatabase, file: FileId, name: &str) -> Ty {
    let tree = hir::hir_def::kotlin::plugin::tree(db, file).expect("a Kotlin file");
    for (id, data) in tree.items.iter() {
        if data.name().map(|n| n.as_str()) == Some(name) {
            // The declared type, nullability included — the caller strips it
            // only where the case is about the non-null half.
            return hir_ty::kotlin_item_ty(db, file, hir_expand::ids::ItemId(id));
        }
    }
    panic!("no item named {name}")
}

#[test]
fn elvis_result_types_preserve_both_operands() {
    let (db, file) = kotlin_fixture(&[
        (
            "/src/main/kotlin/p/Probe.kt",
            r#"package p
fun choose(x: Any?) = x ?: "fallback"
fun require(x: String?) = x ?: throw Exception()
fun nullable(x: String?) = x ?: null
fun empty() = null ?: "fallback"
fun platform(p: Platform) = p.text() ?: "fallback"
"#,
        ),
        (
            "/src/main/java/p/Platform.java",
            r#"package p; public class Platform { public String text() { return "x"; } }"#,
        ),
    ]);
    for (name, expected) in [
        ("choose", Ty::reference(&db, "kotlin.Any", vec![])),
        ("require", Ty::reference(&db, "kotlin.String", vec![])),
        (
            "nullable",
            Ty::nullable(&db, Ty::reference(&db, "kotlin.String", vec![])),
        ),
        ("empty", Ty::reference(&db, "kotlin.String", vec![])),
        ("platform", Ty::reference(&db, "kotlin.String", vec![])),
    ] {
        assert_eq!(ty_of(&db, file, name), expected, "{name}");
    }
}

/// The resolution scope of the fixture's file, for a subtyping question.
fn scope(db: &TestDatabase, file: FileId) -> hir::ResolutionScope {
    hir::ResolutionScope::SourceSet(
        hir::source_set_for_file(db, file).expect("a mapped source set"),
    )
}

mod subtyping {
    use super::*;

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

/// A `.kts` script's body: its top-level *statements* are the body of the
/// implicit `main`
/// (<https://kotlinlang.org/docs/command-line.html#run-scripts>), which no
/// declaration owns — the lowering records it as the item tree's `script_body`
/// — so the type layer infers it under the file's own key
/// ([`hir_ty::kotlin_script_body_types`]). `kotlinc 2.4.20` compiles the
/// fixture as a script (`kotlinc Script.kts -d out`).
#[test]
fn script_body_types() {
    insta::assert_snapshot!("kotlin_script_body_types", {
        let source = r#"class Greeter(val name: String) {
    fun greet(): String = "Hello, $name!"
}

fun twice(n: Int): Int = n * 2

val greeter = Greeter("script")
val local = twice(21)

println(greeter.greet())
println(local)
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Script.kts", source)]);
        let rendered = render_script_body(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "nothing of the script's body is unresolved or mismatched: {rendered}"
        );
        rendered
    });
}

/// The script body's inferred expressions, one line each: the expression's
/// source range, its type, and the declaration the inference resolved it to —
/// the evidence that the body is inferred *and* that its names resolve.
fn render_script_body(db: &TestDatabase, file: FileId) -> String {
    let bodies = hir::file_body_tree(db, file);
    let types = hir_ty::kotlin_script_body_types(db, file);
    let mut lines: Vec<String> = types
        .exprs
        .iter()
        .map(|(expr, ty)| {
            let range = bodies
                .expr_range(*expr)
                .map(|range| format!("@{range:?}"))
                .unwrap_or_default();
            let resolved = types
                .resolved
                .get(expr)
                .map(|resolved| match resolved {
                    hir_ty::KotlinResolvedMember::Local(local) => {
                        format!("local {}", bodies.local(*local).name)
                    }
                    hir_ty::KotlinResolvedMember::Kotlin { item, .. } => {
                        let tree =
                            hir::hir_def::kotlin::plugin::tree(db, file).expect("a Kotlin file");
                        format!(
                            "item {}",
                            tree.data(*item)
                                .name()
                                .map(|name| name.to_string())
                                .unwrap_or_default()
                        )
                    }
                    other => format!("{other:?}"),
                })
                .unwrap_or_else(|| "-".to_owned());
            format!(
                "e{} {range} {} -> {resolved}",
                expr.0.0,
                hir_ty::display_kotlin(db, *ty)
            )
        })
        .collect();
    for diagnostic in &types.diagnostics {
        lines.push(format!(
            "{}: {}",
            diagnostic.code().as_str(),
            diagnostic.message(db)
        ));
    }
    lines.sort();
    lines.join("\n")
}

/// The inferred types and diagnostics of a fixture's bodies, rendered one
/// line per inferred expression in arena order plus one per error.
fn render_bodies(db: &TestDatabase, file: FileId) -> String {
    let tree = hir::hir_def::kotlin::plugin::tree(db, file).expect("a Kotlin file");
    let mut lines = Vec::new();
    let mut render = |types: &hir_ty::KotlinBodyTypes, lines: &mut Vec<String>| {
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
    };
    for (id, data) in tree.items.iter() {
        let Some(_body) = data.body_id() else {
            continue;
        };
        let types = hir_ty::kotlin_body_types(db, file, hir_expand::ids::ItemId(id));
        render(&types, &mut lines);
    }
    // A `.kts` script's implicit `main` is no item's body: its types come from
    // the file-keyed query.
    if tree.script_body.is_some() {
        let types = hir_ty::kotlin_script_body_types(db, file);
        render(&types, &mut lines);
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

/// The two findings a *failed* call reports, each checked with kotlinc 2.4.20
/// against this exact source first — kotlinc 2.4.20 says
/// `argument type mismatch: actual type is 'String', but 'Int' was expected.`
/// twice, `no value passed for parameter 'b'.`, `no value passed for parameter
/// 'a'.` (and `'b'` again for the same call) and `no value passed for parameter
/// 'y'.`
///
/// A finding is reported only for a call whose candidates this model reads
/// whole (one judgeable candidate, no lambda and no flexible argument), which is
/// why the fixture's two-candidate overload set is not what it pins.
#[test]
fn failed_calls_match_the_compiler_wordings() {
    let source = r#"
fun takesTwo(a: Int, b: String) {}

class Point2(val x: Int, val y: Int)

fun calls() {
    takesTwo("x", "y")
    takesTwo(1)
    takesTwo()
    Point2(1)
    Point2("a", 2)
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    let mismatches = rendered
        .lines()
        .filter(|line| {
            line.contains(
                "argument type mismatch: actual type is 'String', but 'Int' was expected.",
            )
        })
        .count();
    assert_eq!(mismatches, 2, "`takesTwo` and `Point2`: {rendered}");
    for expected in [
        "kotlin.missing-argument: no value passed for parameter 'b'.",
        "kotlin.missing-argument: no value passed for parameter 'a'.",
        "kotlin.missing-argument: no value passed for parameter 'y'.",
    ] {
        assert!(
            rendered.contains(expected),
            "expected {expected:?} in:\n{rendered}"
        );
    }
}

/// A write to a read-only property, checked with kotlinc 2.4.20 against this
/// exact source: `holder.readOnly = 3` is `'val' cannot be reassigned.`, while
/// the `var` beside it compiles.
#[test]
fn a_write_to_a_read_only_property_matches_the_compiler_wording() {
    let source = r#"
class Holder {
    val readOnly: Int = 1
    var writable: Int = 2
}

fun writes(holder: Holder) {
    holder.readOnly = 3
    holder.writable = 4
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    let reassignments = rendered
        .lines()
        .filter(|line| line == &"kotlin.val-reassignment: 'val' cannot be reassigned.")
        .count();
    assert_eq!(reassignments, 1, "only the `val` write: {rendered}");
}

/// The `when` findings, checked with kotlinc 2.4.20 against this exact source:
/// a `when` used as a value over an `Int` is `'when' expression must be
/// exhaustive. Add an 'else' branch.`, a subject-less arm condition of type
/// `Int` is `condition type mismatch: inferred type is 'Int' but 'Boolean' was
/// expected.`, and a subject-less *type test* — which has no type to name — is
/// `condition of type 'Boolean' expected.`
///
/// The last two functions are what the compiler *accepts*, and are why neither
/// the containment condition nor a `when` standing as a statement is a finding
/// here: `when { "a" in "abc" -> 1; else -> 2 }` is a `Boolean` operator
/// expression, and a `when` whose value is not used needs no `else`, however
/// few of its subjects its arms cover.
#[test]
fn when_findings_match_the_compiler_wordings() {
    let source = r#"
fun choose(x: Int): String = when (x) {
    1 -> "one"
    2 -> "two"
}

fun odd(x: Int) = when {
    x -> 1
    else -> 2
}

fun isString() = when {
    is String -> 1
    else -> 2
}

fun contains() = when {
    "a" in "abc" -> 1
    else -> 2
}

fun statement(x: Int) {
    when (x) {
        1 -> println("one")
        2 -> println("two")
    }
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    for expected in [
        "kotlin.non-exhaustive-when: 'when' expression must be exhaustive. Add an 'else' branch.",
        "kotlin.non-boolean-when-condition: condition type mismatch: inferred type is 'Int' but 'Boolean' was expected.",
        "kotlin.non-boolean-when-condition: condition of type 'Boolean' expected.",
    ] {
        assert!(
            rendered.contains(expected),
            "expected {expected:?} in:\n{rendered}"
        );
    }
    assert_eq!(
        rendered.matches("kotlin.").count(),
        3,
        "`contains` and `statement` report nothing: {rendered}"
    );
}

/// Kotlin's own SAM conversion is gated on a **`fun interface`**: a Kotlin
/// lambda converts against one — and against a *Java* functional interface,
/// from source or from a classfile — but not against an ordinary Kotlin
/// interface that merely declares one abstract method
/// (<https://kotlinlang.org/docs/fun-interfaces.html>): kotlinc 2.4.20 reports
/// `cannot infer type for value parameter 'text'` for the `Naming` lambda below
/// and an `argument type mismatch` for the `StringConsumer` one, which is the
/// split asserted here.
///
/// The observable is the lambda's `it` — the parameter the conversion gives it
/// ([KLS
/// `expressions.html#lambda-literals`](https://kotlinlang.org/spec/expressions.html#lambda-literals)):
/// a parameter-less lambda takes the expected type's parameters, and no
/// conversion means no parameter at all, which leaves `it` unresolved. Both
/// interfaces that convert give `it` a `String`, which the fixture's
/// `wantsInt(it)` reports.
#[test]
fn a_kotlin_lambda_converts_only_against_a_fun_interface() {
    let source = r#"
fun interface StringConsumer {
    fun accept(text: String)
}

interface Naming {
    fun name(text: String)
}

fun wantsInt(value: Int) {}

fun takeConsumer(consumer: StringConsumer) {}

fun takeNaming(naming: Naming) {}

fun takeJava(consumer: java.util.function.Consumer<String>) {}

fun use() {
    takeConsumer { wantsInt(it) }
    takeNaming { wantsInt(it) }
    takeJava { wantsInt(it) }
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    assert_eq!(
        rendered
            .matches(
                "kotlin.argument-mismatch: argument type mismatch: actual type is 'String', but 'Int' was expected."
            )
            .count(),
        2,
        "the `fun interface` and the classfile `Consumer` both give `it` a `String`: {rendered}"
    );
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.contains("kotlin."))
            .count(),
        3,
        "the non-`fun` interface's lambda has no parameter at all: {rendered}"
    );
    assert!(
        rendered.contains("kotlin.unresolved-reference: unresolved reference 'it'."),
        "`it` is unbound where no conversion applies: {rendered}"
    );
}

// -- Java and classfile members on a Kotlin receiver --------------------------

/// A `when` entry's guard — the `if <expression>` kotlinc writes between the
/// conditions and the arrow
/// (<https://kotlinlang.org/docs/control-flow.html#when-expressions-and-statements>;
/// KLS 1.9's `whenEntry` has no guard, so the compiler is the reference) — is a
/// `Boolean` inferred in the entry's *narrowed* scope, and it does not make the
/// entry cover its condition.
///
/// kotlinc 2.4.20 reports exactly the two findings below for this fixture: the
/// two guarded entries do not exhaust an `Any` subject, and the second guard is
/// `Cat` — not `Any`, and not unresolved — which is what shows the guard read
/// `x` through its own entry's `is Cat`. The last function's unguarded `else`
/// makes its `when` exhaustive, so it reports nothing.
#[test]
fn a_when_guard_is_boolean_and_sees_the_narrowing() {
    let source = r#"
class Cat(val hungry: Boolean)

fun feed(x: Any): String = when (x) {
    is Cat if x.hungry -> "hungry"
    is Cat if x -> "cat"
}

fun complete(x: Any): String = when (x) {
    is Cat if x.hungry -> "hungry"
    else -> "other"
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    for expected in [
        "kotlin.non-exhaustive-when: 'when' expression must be exhaustive. Add an 'else' branch.",
        "kotlin.non-boolean-when-condition: condition type mismatch: inferred type is 'Cat' but 'Boolean' was expected.",
    ] {
        assert!(
            rendered.contains(expected),
            "expected {expected:?} in:\n{rendered}"
        );
    }
    assert_eq!(
        rendered.matches("kotlin.").count(),
        2,
        "`complete` reports nothing and every guard reads its narrowing: {rendered}"
    );
}

/// The member bridge: what a call, a read or a write on a Java or classfile
/// receiver resolves to.
///
/// Each case is checked with kotlinc 2.4.20 against the same sources first.
mod java_members {
    use super::*;

    /// The fixture's Java class, whose members exercise every shape the bridge
    /// has a rule for: an instance method, a static method, a field, a
    /// getter/setter pair, and a `protected` method for a Kotlin subclass.
    const JAVA_BASE: &str = r#"
package a;

public class JavaBase {
    public int field = 1;

    private boolean dragEnabled;

    public static String hello() {
        return "hello";
    }

    public String instance() {
        return "instance";
    }

    public boolean getDragEnabled() {
        return dragEnabled;
    }

    public void setDragEnabled(boolean value) {
        dragEnabled = value;
    }

    protected String guarded() {
        return "guarded";
    }
}
"#;

    /// A Kotlin file that calls, reads and writes those members.
    const KOTLIN_USE: &str = r#"
package a

fun use(base: JavaBase) {
    val hello: String = JavaBase.hello()
    val instance: String = base.instance()
    val field: Int = base.field
    val guarded: String = guarded(base)
    base.dragEnabled = true
    val enabled: Boolean = base.dragEnabled
}

class Sub : JavaBase() {
    fun call(): String = guarded()
}
"#;

    /// Every shape the bridge covers resolves, with no diagnostic at all:
    /// kotlinc compiles the same sources clean.
    #[test]
    fn a_java_source_member_resolves_without_a_diagnostic() {
        let (db, _) = kotlin_fixture(&[
            ("/src/main/java/a/JavaBase.java", JAVA_BASE),
            ("/src/main/kotlin/a/Use.kt", KOTLIN_USE),
        ]);
        // File 1 is the Java source; the Kotlin one is file 2.
        let rendered = render_bodies(&db, FileId::from_raw(2));
        assert!(
            !rendered.contains("kotlin."),
            "no diagnostic for a resolved Java member: {rendered}"
        );
        // A Java declaration's type is a *platform* type in Kotlin: the Java
        // source's `String instance()` is `String..String?`, which kotlinc
        // accepts both where a `String` and where a `String?` is expected
        // (`val x: String = base.instance()` compiles).
        assert!(
            rendered.contains("String..String?"),
            "a Java source member's type is a platform type: {rendered}"
        );
    }

    /// A classfile class, through the same bridge: the fixture's
    /// `java.util.ArrayList`, whose element type is substituted with the
    /// argument the receiver writes (`String`), and whose `size()` is the
    /// *property* `size` the standard library declares over it — kotlinc
    /// 2.4.20 accepts `val n: Int = a.size` and `val l: Int = list.size`, and
    /// `val s: String = list.get(0)` for an `ArrayList<String>`.
    #[test]
    fn a_classfile_member_resolves_through_the_mapping() {
        let source = r#"
fun use(list: java.util.ArrayList<String>): Int {
    val size: Int = list.size
    val isEmpty: Boolean = list.isEmpty()
    val first: String = list.get(0)
    val created = java.util.ArrayList()
    val createdSize: Int = created.size
    return size
}
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Use.kt", source)]);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "every member of the classfile receiver resolves: {rendered}"
        );

        // The mapping *is* the classifier identity: a `java.util.ArrayList` is
        // a `kotlin.collections.List`, which is what the covariance of the
        // standard library treats as one type
        // (<https://kotlinlang.org/docs/java-interop.html#mapped-types>).
        let scope = hir::ResolutionScope::SourceSet(
            hir::source_set_for_file(&db, file).expect("a mapped source set"),
        );
        let java = Ty::reference(&db, "java.util.ArrayList", Vec::new());
        let kotlin = Ty::reference(&db, "kotlin.collections.List", Vec::new());
        assert!(
            hir_ty::kotlin_subtype(&db, &scope, &java, &kotlin),
            "`java.util.ArrayList` *is* a `kotlin.collections.List`"
        );
    }

    /// A Kotlin *subclass* of a Java class reaches its `protected` members,
    /// exactly as a Java subclass does ([JLS §6.6.2]): the access context of a
    /// Kotlin call site is its enclosing classifier and that classifier's
    /// first supertype.
    #[test]
    fn a_kotlin_subclass_reaches_a_protected_classfile_member() {
        let source = r#"
class Sub : javax.swing.JList() {
    fun call(): String = guarded()
}
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Use.kt", source)]);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "a `protected` classfile member is visible to the subclass: {rendered}"
        );
    }

    /// The synthetic property of a Java getter/setter pair is a Kotlin
    /// property, and a same-named *method* does not take its place: kotlinc
    /// 2.4.20 accepts `j.dragEnabled = true` for a
    /// `getDragEnabled()`/`setDragEnabled(boolean)` pair, and reads the
    /// property — not the `void layout()` method — in `container.layout`.
    #[test]
    fn a_classfile_property_and_a_shadowing_method_both_resolve() {
        let source = r#"
fun use(list: javax.swing.JList) {
    list.dragEnabled = true
    val dragEnabled: Boolean = list.dragEnabled
    val layout: Any = list.layout
    list.layout()
}
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Use.kt", source)]);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "the property and the method both resolve: {rendered}"
        );
    }
}

// -- a Java caller of a Kotlin declaration ------------------------------------

/// The other direction of the bridge: a **Java** lambda and method reference
/// against a Kotlin source interface. A Kotlin `interface` *is* an interface in
/// the classfile, so Java's own SAM rules apply to it exactly as they do to a
/// Java one — which is what the Kotlin layer's JVM view answers for it
/// ([`crate::lang::JvmMemberSource::kind`] and `::abstract_methods`).
///
/// The oracle is **javac**, not kotlinc: kotlinc reads a `.java` source for
/// resolution but does not check its body, so the two-file pair is judged by
/// the Java compiler. For a `Listener` compiled by kotlinc 2.4.20
/// (`javap -p`: `public abstract void onEvent(java.lang.String);`), javac
/// reports exactly one error here:
///
/// ```text
/// Use.java:11: error: incompatible types: String cannot be converted to int
///         call(message -> needsInt(message));
///                                  ^
/// ```
///
/// which *is* the claim: the lambda's parameter is the Kotlin interface's
/// `String` — the conversion resolved — and the method reference on the next
/// line is accepted.
#[test]
fn a_java_lambda_converts_against_a_kotlin_interface() {
    const KOTLIN_INTERFACE: &str = r#"
package a;

interface Listener {
    fun onEvent(message: String)
}
"#;
    const JAVA_USE: &str = r#"
package a;

class Use {
    static void call(Listener listener) {}

    static void needsInt(int value) {}

    static void print(String text) {}

    static void withLambda() {
        call(message -> needsInt(message));
    }

    static void withReference() {
        call(Use::print);
    }
}
"#;
    let files: &[(&str, &str)] = &[
        ("/src/main/java/a/Use.java", JAVA_USE),
        ("/src/main/kotlin/a/Listener.kt", KOTLIN_INTERFACE),
    ];
    let (db, _) = kotlin_fixture(files);
    let rendered = common::render_body_types(&db, files);
    assert!(
        rendered.contains("e0: java.lang.String"),
        "the lambda's parameter is the Kotlin interface's `String`: {rendered}"
    );
    assert!(
        rendered.contains("e2: a.Listener"),
        "the lambda converts against the Kotlin interface: {rendered}"
    );
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.contains("diags:"))
            .count(),
        1,
        "only the mismatched call reports; the method reference resolves: {rendered}"
    );
}

/// A Kotlin source *constructs* a Java source generic class and reads it at the
/// arguments it wrote: `Box<String>("x")` constructs a `Box<String>`, so
/// `box.get()` — the Java class's `T get()` — is `String!`
/// (<https://kotlinlang.org/docs/java-interop.html#generics-in-java-and-kotlin>:
/// a Java type is a platform type in Kotlin). kotlinc 2.4.20 reports exactly
/// one error for this fixture — `initializer type mismatch: expected 'Int',
/// actual 'String!'.` — which is what shows the argument was bound.
#[test]
fn a_kotlin_constructor_call_binds_a_java_source_classs_type_parameter() {
    const JAVA_BOX: &str = r#"
package a;

public class Box<T> {
    public T value;

    public Box(T value) {
        this.value = value;
    }

    public T get() {
        return value;
    }
}
"#;
    const KOTLIN_USE: &str = r#"
package a

fun use(): String {
    val box = Box<String>("x")
    val s: String = box.value
    val wrong: Int = box.get()
    return s
}
"#;
    let (db, _) = kotlin_fixture(&[
        ("/src/main/java/a/Box.java", JAVA_BOX),
        ("/src/main/kotlin/a/Use.kt", KOTLIN_USE),
    ]);
    // File 1 is the Java source; the Kotlin one is file 2.
    let rendered = render_bodies(&db, FileId::from_raw(2));
    let diagnostics: Vec<&str> = rendered
        .lines()
        .filter(|line| line.contains("kotlin."))
        .collect();
    assert_eq!(
        diagnostics,
        vec!["kotlin.type-mismatch: initializer type mismatch: expected 'Int', actual 'String'."],
        "the written `String` binds the Java class's `T`: {rendered}"
    );
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

    /// A function type's receiver is its *first* type argument: the receiver is
    /// the `this` of the function's body, so it takes the position of the first
    /// parameter in `FunctionN<P1, …, PN, R>`
    /// (<https://kotlinlang.org/docs/lambdas.html#function-types>), and `N` is
    /// the number of *parameters*. `List<Int>.() -> Unit` is therefore
    /// `Function1<List<Int>, Unit>` and `Map<String, Int>.(String) -> Int` is
    /// `Function2<Map<String, Int>, String, Int>`; kotlinc 2.4.20 accepts both
    /// declarations.
    #[test]
    fn a_function_types_receiver_is_its_first_type_argument() {
        let source = r#"
val fill: List<Int>.() -> Unit = {}

val look: Map<String, Int>.(String) -> Int = { 0 }
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);

        let fill = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "fill"));
        let TyKind::Reference { name, args, .. } = fill.kind(&db) else {
            panic!("a function type is a reference type");
        };
        assert_eq!(name.to_string(), "kotlin.Function1");
        assert_eq!(
            hir_ty::display_kotlin(&db, args[0]).to_string(),
            "List<Int>",
            "the receiver is the first argument"
        );
        assert_eq!(hir_ty::display_kotlin(&db, args[1]).to_string(), "Unit");

        let look = hir_ty::kotlin_item_ty(&db, file, item_named(&db, file, "look"));
        let TyKind::Reference { name, args, .. } = look.kind(&db) else {
            panic!("a function type is a reference type");
        };
        assert_eq!(
            name.to_string(),
            "kotlin.Function2",
            "`N` counts the parameters, not the receiver"
        );
        assert_eq!(
            hir_ty::display_kotlin(&db, args[0]).to_string(),
            "Map<String, Int>"
        );
        assert_eq!(hir_ty::display_kotlin(&db, args[1]).to_string(), "String");
        assert_eq!(hir_ty::display_kotlin(&db, args[2]).to_string(), "Int");
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
        let tree = hir::hir_def::kotlin::plugin::tree(db, file).expect("a Kotlin file");
        for (id, data) in tree.items.iter() {
            if data.name().map(|n| n.as_str()) == Some(name) {
                return hir_expand::ids::ItemId(id);
            }
        }
        panic!("no item named {name} in the fixture");
    }
}

/// The `val`/`var` rules of an assignment, each checked with kotlinc 2.4.20:
/// `var v = 1; v = 2` compiles, `val c = 1; c = 2` is
/// `'val' cannot be reassigned.`, a `val` *declared* without an initializer may
/// be assigned once (`val d: Int` then `d = 1` compiles, a second write does
/// not), and `val list = mutableListOf<Int>(); list += 1` compiles because a
/// compound assignment is the `plusAssign` convention, not a write.
#[test]
fn the_val_and_var_assignment_rules_match_the_compiler() {
    let source = r#"
fun rules() {
    var v = 1
    v = 2
    val c = 1
    c = 2
    val d: Int
    d = 1
    val e: Int
    e = 1
    e = 2
}

fun compound() {
    val list = mutableListOf<Int>()
    list += 1
}

fun parameter(p: Int) {
    p = 1
}

fun loop(xs: List<Int>) {
    for (x in xs) {
        x = 1
    }
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Use.kt", source)]);
    let rendered = render_bodies(&db, file);
    let reassignments = rendered
        .lines()
        .filter(|line| line.contains("'val' cannot be reassigned."))
        .count();
    assert_eq!(
        reassignments, 4,
        "`c`, the second `e`, the parameter `p` and the loop variable `x` are the writes a `val` refuses: {rendered}"
    );
}

/// A parameter-less lambda takes its parameter from the *expected* function
/// type, and `it` is that parameter ([KLS
/// `type-inference.html#function-literals`](https://kotlinlang.org/spec/type-inference.html#function-literals)):
/// `runWith { it + 1 }` for a `runWith(f: (Int) -> Int)` infers `it` as `Int` —
/// kotlinc 2.4.20 compiles this fixture clean (exit status 0, no diagnostics),
/// including the *nested* lambda whose `it` shadows the outer one.
#[test]
fn a_lambda_parameter_comes_from_the_expected_function_type() {
    let source = r#"
fun runWith(f: (Int) -> Int): Int = f(1)

fun twice(f: (Int) -> Int): Int = f(f(1))

fun use(): Int = runWith { it + 1 }

fun nested(): Int = runWith { twice { it + 1 } + it }
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Use.kt", source)]);
    let rendered = render_bodies(&db, file);
    assert!(
        !rendered.contains("kotlin.unresolved-reference"),
        "`it` is bound to the function type's parameter: {rendered}"
    );
}

// -- anonymous and local classes ---------------------------------------------

/// The fixture both cases below share: an object expression whose own members
/// are used, a local class constructed and called, and `this`/`super` inside a
/// subclass. kotlinc 2.4.20 compiles it clean.
const ANONYMOUS_AND_LOCAL: &str = r#"
interface Runner { fun run(): Int }

fun probe(): Int {
    val anonymous = object : Runner {
        val label: String = "x"
        override fun run(): Int = label.length
    }
    val box: Runner = anonymous
    val label: String = anonymous.label
    val direct: Int = anonymous.run()

    class Counter(val start: Int) {
        fun next(): Int = start + 1
    }
    val counted: Int = Counter(1).next()

    return box.run() + direct + counted + label.length
}

open class Base { fun base(): Int = 1 }
class Sub : Base() {
    fun own(): Int = 2
    fun both(): Int = this.own() + super.base()
}
"#;

/// An object expression's type is the anonymous class its body declares ([KLS
/// `expressions.html#object-literals`](https://kotlinlang.org/spec/expressions.html#object-literals)),
/// and it is a subtype of every supertype the literal writes — which is what
/// `val box: Runner = anonymous` needs. The class has no name in the source (the
/// compiler gives it a positional binary name, `ProbeKt$probe$anonymous$1`),
/// so the item carries `<anonymous>` and the type is identified by the
/// declaration.
#[test]
fn an_object_expression_is_its_anonymous_class() {
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", ANONYMOUS_AND_LOCAL)]);
    let rendered = render_bodies(&db, file);
    assert!(
        !rendered.contains("kotlin."),
        "every name of the fixture resolves: {rendered}"
    );
    assert!(
        rendered.contains(": <anonymous>"),
        "the literal types as its anonymous class: {rendered}"
    );

    // The literal is a `Runner`, and only its declared supertype makes it one:
    // the anonymous class and `Runner` are different classifier identities.
    let scope = scope(&db, file);
    let anonymous = ty_of(&db, file, "<anonymous>");
    let runner = Ty::reference(&db, "Runner", Vec::new());
    assert_ne!(
        hir_ty::display_kotlin(&db, anonymous).to_string(),
        hir_ty::display_kotlin(&db, runner).to_string(),
        "the literal is not its supertype by name"
    );
    assert!(
        hir_ty::kotlin_subtype(&db, &scope, &anonymous, &runner),
        "`object : Runner` makes the literal a `Runner`"
    );
}

/// A local class is identified by its declaration too ([KLS
/// `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)):
/// `Counter(1).next()` resolves the local declaration's constructor and then its
/// own member, and the local class shadows nothing it should not.
#[test]
fn a_local_class_resolves_through_its_declaration() {
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", ANONYMOUS_AND_LOCAL)]);
    let rendered = render_bodies(&db, file);
    assert!(
        rendered.lines().any(|line| line.ends_with(": Counter")),
        "`Counter(1)` types as the local class it constructs: {rendered}"
    );
    let scope = scope(&db, file);
    let counter = ty_of(&db, file, "Counter");
    assert_eq!(
        hir_ty::display_kotlin(&db, counter).to_string(),
        "Counter",
        "a local class types as itself, by simple name"
    );
    let TyKind::Reference { local, .. } = counter.kind(&db) else {
        panic!("a local class is a reference type");
    };
    assert!(
        local.is_some(),
        "a local class is identified by its declaration, not by a canonical name"
    );
    // Its declared supertype is `Any` (it writes none), which the local walk of
    // the subtyping relation answers.
    assert!(
        hir_ty::kotlin_subtype(
            &db,
            &scope,
            &counter,
            &Ty::reference(&db, "kotlin.Any", Vec::new())
        ),
        "a local class is still a `kotlin.Any`"
    );
}

/// A source class's *primary* constructor is not one of its body members — it
/// hangs off the header — and it is what `Holder(1)` resolves to ([KLS
/// `declarations.html#primary-constructor`](https://kotlinlang.org/spec/declarations.html#primary-constructor)).
///
/// The oracle is kotlinc 2.4.20's own wording for the same source:
/// `val h: String = Holder(1)` reports
/// `initializer type mismatch: expected 'String', actual 'Holder'.` — which needs
/// the call to type as the class it constructs, and would not be reported at all
/// for an unresolved call.
#[test]
fn a_primary_constructor_call_resolves_to_the_class_it_constructs() {
    let source = r#"
class Holder(val value: Int)

fun probe(): String {
    val h: String = Holder(1)
    return h
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    assert!(
        rendered.contains("initializer type mismatch: expected 'String', actual 'Holder'."),
        "`Holder(1)` is a `Holder`: {rendered}"
    );
}

/// `this` is the innermost enclosing classifier's type and `super` its first
/// supertype ([KLS
/// `expressions.html#this-expressions`](https://kotlinlang.org/spec/expressions.html#this-expressions),
/// [`#super-forms`](https://kotlinlang.org/spec/expressions.html#super-forms)):
/// inside `Sub`, `this.own()` resolves on `Sub` and `super.base()` on `Base`.
#[test]
fn this_and_super_are_the_enclosing_classifiers() {
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", ANONYMOUS_AND_LOCAL)]);
    let rendered = render_bodies(&db, file);
    assert!(
        rendered.lines().any(|line| line.ends_with(": Sub")),
        "`this` is the enclosing classifier: {rendered}"
    );
    assert!(
        rendered.lines().any(|line| line.ends_with(": Base")),
        "`super` is its first supertype: {rendered}"
    );
}

// -- declaration types inferred from the body ---------------------------------

/// The declarations kotlinc 2.4.20 types without a written type, which is what
/// the inference is asked for: a `val` takes its initializer's type, a function
/// with an expression body that expression's, and every other declaration
/// `kotlin.Unit`. `TODO()` is the standard library's and stays unresolved
/// without a classpath (a recorded gap).
const INFERRED_TYPES: &str = r#"
class Holder(val value: Int) {
    val double = value + value
    val label: String = "x"
    fun read() = double
}

val top = 1
val written: String = "a"
fun expression() = "a"
fun block() { val unused = 1 }
fun unitReturn(): Unit { }

interface Declared {
    fun none()
}

fun <T> lazy(initializer: () -> T): Lazy<T> = TODO()

val delegated by lazy { 1 }
"#;

/// A declaration without a written type is typed by its body: a property by its
/// initializer ([KLS
/// `type-inference.html#local-type-inference`](https://kotlinlang.org/spec/type-inference.html#local-type-inference)),
/// a function with an *expression* body by that expression, and a property by
/// nothing less than the rule the compiler documents for delegation
/// (<https://kotlinlang.org/docs/delegated-properties.html>): `by lazy { 1 }` is
/// an `Int`, because the standard library's `Lazy<T>` contributes its type
/// argument.
///
/// The two shapes the *block* form covers are the ones the item tree has to
/// record: `fun block() { val unused = 1 }` is a `Unit` function although its
/// block's last statement is an expression, and `fun none()` of an interface
/// writes neither body nor type and is `Unit` all the same — kotlinc types both
/// `Unit`.
#[test]
fn declaration_types_are_inferred_from_the_body() {
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", INFERRED_TYPES)]);
    let rendered = render_types(&db, file);
    for expected in [
        "val double: Int",
        "val label: String",
        "val top: Int",
        "val written: String",
        // The inferred *member* type is what a read on it resolves to.
        "fun read: Int",
        "fun expression: String",
        "fun block: Unit",
        "fun unitReturn: Unit",
        "fun none: Unit",
        "val delegated: Int",
    ] {
        assert!(
            rendered.contains(expected),
            "expected {expected:?} in:
{rendered}"
        );
    }
}

/// A declaration that refers to itself — directly, or through a second one — is
/// the error type rather than a panic: the item-type query reaches the
/// inference, the inference resolves the name back to the declaration, and the
/// re-entry is what [`hir_ty`]'s in-flight guard answers `Ty::error` for. Salsa
/// 0.28.2 panics on an unrecovered dependency cycle, so the case is pinned.
#[test]
fn a_self_referential_declaration_is_the_error_type() {
    let source = "val x = x\nval a = b\nval b = a\n";
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_types(&db, file);
    for expected in ["val x: <error>", "val a: <error>", "val b: <error>"] {
        assert!(
            rendered.contains(expected),
            "expected {expected:?} in:\n{rendered}"
        );
    }
}

/// A delegate that is not `Lazy` contributes the return type of the `getValue`
/// operator it declares, with the property's owner as the `thisRef` parameter
/// (<https://kotlinlang.org/docs/delegated-properties.html>) — the fixture is
/// the canonical delegate shape, which kotlinc 2.4.20 compiles clean.
#[test]
fn a_delegate_contributes_its_getvalue_return_type() {
    let source = r#"
import kotlin.reflect.KProperty

class Source {
    operator fun getValue(thisRef: Any?, property: KProperty<*>): String = "s"
}

class Holder {
    val bySource by Source()
}

fun use(): String = Holder().bySource
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_types(&db, file);
    assert!(
        rendered.contains("val bySource: String"),
        "the delegate's `getValue` return type is the property's: {rendered}"
    );
    // The read of it, through the same rule.
    let bodies = render_bodies(&db, file);
    assert!(
        !bodies.contains("kotlin."),
        "nothing of the fixture is unresolved: {bodies}"
    );
}

// -- control flow, operators and receivers ------------------------------------

/// The fixture behind the cases below, each of which kotlinc 2.4.20 compiles
/// clean: a class with the operator conventions it declares, a nullable and an
/// open hierarchy for the smart casts, a function type for the lambdas and a
/// local function that captures its caller's bindings.
const CONTROL_FLOW: &str = r#"
open class Animal
class Dog(val legs: Int) : Animal()
class Cat(val claws: Int) : Animal()

class Span(val from: Int, val to: Int)

class Box(var value: Int) {
    operator fun plus(other: Box): Box = Box(value + other.value)

    operator fun get(index: Int): String = "x"

    operator fun component1(): Int = value

    operator fun rangeTo(other: Int): Span = Span(value, other)

    // A lambda whose parameter is a *receiver* function type: inside it, `this`
    // is the box the call is written on.
    fun applied(block: Box.() -> Unit): Box {
        block()
        return this
    }
}

fun runWith(block: (Int) -> Int): Int = block(1)

fun joins(c: Boolean, a: Animal, n: Int?): Int {
    val joined: Number = if (c) 1 else 2.0
    val animal: Animal = if (c) Dog(4) else Cat(18)
    val tried: Int = try { 1 } catch (e: Exception) { 2 } finally { }
    val narrowed: Int = if (a is Dog) a.legs else 0
    val nonNull: Int = if (n != null) n else 0
    val nonNull2: Int = if (n == null) 0 else n
    val notDog: Boolean = a !is Dog
    val notIn: Boolean = 1 !in listOf(1)
    return joined.hashCode() + animal.hashCode() + tried + narrowed + nonNull + nonNull2 + (if (notDog) 1 else 0) + (if (notIn) 1 else 0)
}

fun arithmetic(): Long {
    val int: Int = 1 + 2
    val long: Long = 1L + 2
    val double: Double = 1 + 2.5
    val float: Float = 1.0f + 2
    val char: Char = 'a' + 1
    val text: String = "a" + 1
    val compared: Boolean = 1 < 2L
    val negated: Int = -int
    var counted = 0
    counted++
    val span: Span = Box(1)..3
    val box: Box = Box(1) + Box(2)
    val indexed: String = Box(3)[0]
    return long + double.toLong() + float.toLong() + char.code + text.length + (if (compared) 1L else 0L) + negated.toLong() + counted.toLong() + span.from.toLong() + box.value.toLong() + indexed.length.toLong()
}

fun lambdas(): Int {
    val one = runWith { it + 1 }
    val two = runWith { v -> v + 2 }
    val receiver = Box(1).applied { value = 2 }
    return one + two + receiver.value
}

fun captures(input: Int): Int {
    val doubled = input * 2
    fun inner(extra: Int): Int = doubled + extra
    class Local(val own: Int) {
        fun total(): Int = own + doubled
    }
    val listener = object : Animal() {
        fun read(): Int = doubled
    }
    return inner(1) + Local(2).total() + listener.read()
}

fun caught(): Int {
    val parsed: Int
    try {
        parsed = 1
    } catch (e: Exception) {
        return e.message?.length ?: 0
    }
    return parsed
}

fun destructured(): Int {
    val (first) = Box(7)
    var total = 0
    for ((name, value) in listOf(1 to 2)) {
        total += name + value
    }
    return first + total
}

fun delegated(): Int {
    val local by lazy { 1 }
    return local
}
"#;

/// The types the control-flow, operator and receiver rules produce, and the
/// claim that nothing of the fixture is unresolved: kotlinc 2.4.20 compiles it
/// clean, whose own answer for `if (c) 1 else 2.0` is `Number & Comparable<*>`
/// — assignable to the `Number` the join of the two branch types is here.
#[test]
fn control_flow_operators_and_receivers_match_the_compiler() {
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", CONTROL_FLOW)]);
    let rendered = render_bodies(&db, file);
    assert!(
        !rendered.contains("kotlin."),
        "nothing of the fixture is unresolved or mismatched: {rendered}"
    );
    // The declared conventions are what the operators resolve to, and the
    // built-in arithmetic is the compiler's own table.
    for expected in [
        ": Long",
        ": Int",
        ": Double",
        ": Float",
        ": Char",
        ": String",
        ": Boolean",
        ": Span",
        ": Box",
    ] {
        assert!(
            rendered.contains(expected),
            "expected a {expected} expression in:\n{rendered}"
        );
    }
}

/// A `for` loop's variable is the element of the `iterator()` convention's
/// `Iterator<T>` ([KLS
/// `control--and-data-flow-analysis.html#for-loops`](https://kotlinlang.org/spec/control--and-data-flow-analysis.html#for-loops)),
/// and a destructuring pattern binds the `componentN` of what it destructures
/// ([KLS
/// `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations)).
#[test]
fn a_destructuring_declaration_binds_the_components() {
    let source = r#"
class Cell(val one: Int, val two: Int) {
    operator fun component1(): Int = one

    operator fun component2(): Int = two
}

fun probe(): Int {
    val (first, second) = Cell(1, 2)
    val sum: Int = first + second
    return sum
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    assert!(
        !rendered.contains("kotlin."),
        "the components are bound: {rendered}"
    );
}

/// A `data class` declares its components through the *compiler*, not in its
/// body, and they are the members a destructuring declaration binds
/// (`componentN`, [KLS
/// `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations))
/// and a `copy` call resolves against
/// ([KLS
/// `declarations.html#data-class-declaration`](https://kotlinlang.org/spec/declarations.html#data-class-declaration),
/// <https://kotlinlang.org/docs/data-classes.html>).
///
/// kotlinc 2.4.20 reports exactly one error for this fixture —
/// `initializer type mismatch: expected 'Int', actual 'String'.` for the
/// deliberately wrong binding — which is what shows `y` is `component2`'s
/// `String` and not the fallback type of a receiver that declares no such
/// member; the named and positional `copy` calls and the reads of their results
/// meanwhile resolve.
#[test]
fn a_data_class_declares_its_components_and_copy() {
    let source = r#"
data class Point(val x: Int, var y: String) {
    val label: String = "p"
}

fun probe(point: Point): String {
    val (x, y) = point
    val wrong: Int = y
    val renamed: Point = point.copy(y = "q")
    val positional: Point = point.copy(2, "q")
    return renamed.label + positional.y + x.toString()
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    let diagnostics: Vec<&str> = rendered
        .lines()
        .filter(|line| line.contains("kotlin."))
        .collect();
    assert_eq!(
        diagnostics,
        vec!["kotlin.type-mismatch: initializer type mismatch: expected 'Int', actual 'String'."],
        "the components and both `copy` calls resolve, and `y` is `String`: {rendered}"
    );
}

/// An `enum class` declares `values()` and `valueOf(String)` through the
/// compiler, not in its body
/// (<https://kotlinlang.org/docs/enum-classes.html#find-enum-constants>): a
/// Kotlin caller writes them on the enum's class exactly as a Java one does,
/// and `values()` is the array of the enum's own type.
///
/// kotlinc 2.4.20 reports exactly one error for this fixture — `initializer
/// type mismatch: expected 'Int', actual 'Color'.` for the deliberately wrong
/// binding — which is what shows `valueOf` answers the enum, while `values()`
/// and the `size` of its result resolve.
#[test]
fn an_enum_declares_its_values_and_value_of() {
    let source = r#"
enum class Color { RED, GREEN }

fun probe(): Int {
    val all = Color.values()
    val wrong: Int = Color.valueOf("RED")
    return all.size
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    let diagnostics: Vec<&str> = rendered
        .lines()
        .filter(|line| line.contains("kotlin."))
        .collect();
    assert_eq!(
        diagnostics,
        vec!["kotlin.type-mismatch: initializer type mismatch: expected 'Int', actual 'Color'."],
        "`values()` and `valueOf` resolve, and `valueOf` answers the enum: {rendered}"
    );
}

/// A `return` whose value the declared return type cannot accept is the
/// compiler's mismatch, worded as an assignment's
/// ([KLS `expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions)):
/// kotlinc 2.4.20 reports `type mismatch: inferred type is 'String' but 'Int'
/// was expected.` for the same source.
#[test]
fn a_return_answers_the_declared_return_type() {
    let source = r#"
fun probe(): Int {
    return "x"
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    assert!(
        rendered.contains("kotlin.type-mismatch"),
        "a `return` of the wrong type is reported: {rendered}"
    );
}

// -- extension members and named arguments ------------------------------------

/// A cast keeps the *written* operator: `as T` is `T` and `as? T` its nullable
/// form ([KLS
/// `expressions.html#cast-expressions`](https://kotlinlang.org/spec/expressions.html#cast-expressions)),
/// and `asExpression`'s operators nest to the left, so a chain is a cast of a
/// cast ([spec: grammar-rule-asExpression]). The inferred property types below
/// are the three claims: the plain cast is the target, the safe cast is its
/// nullable form, and the chain applied its *second* operator to the first
/// cast's result — `Int?`, not `Int`. kotlinc 2.4.20 accepts this fixture,
/// warning only that the chain's `as? Int` can never succeed for a value that
/// was just cast to `String`.
#[test]
fn a_cast_keeps_its_direction_and_its_safety() {
    let source = r#"
class Probe(val item: Any?) {
    val plain = item as String
    val safe = item as? String
    val chained = item as String as? Int
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_types(&db, file);
    for expected in [
        "val plain: String",
        "val safe: String?",
        "val chained: Int?",
    ] {
        assert!(
            rendered.contains(expected),
            "expected {expected:?} in:\n{rendered}"
        );
    }
}

/// A top-level extension is resolved on a receiver of the type it extends as
/// long as no *member* of that name applies, a member wins where both exist
/// ([KLS
/// `overload-resolution.html#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers)),
/// and a *member extension* — one a class declares — is in scope inside it.
///
/// kotlinc 2.4.20 reports exactly the two `initializer type mismatch` errors the
/// fixture writes deliberately — `expected 'String', actual 'Int'.` for `doubled`
/// and `withOffset` — which is what proves the extensions resolved, since a call
/// that resolves to nothing is the error type and no check reports it.
#[test]
fn a_source_extension_resolves_on_its_receiver() {
    let source = r#"
class Box(val value: Int) {
    fun describe(): String = "member"

    fun own(): Int = doubled()

    fun withOffset(): Int = offBy(1)

    fun Box.offBy(offset: Int): Int = value + offset
}

fun Box.doubled(): Int = value * 2

fun Box.describe(): Int = 1

fun probe() {
    val member: String = Box(1).describe()
    val own: Int = Box(4).own()
    val wrong: String = Box(2).doubled()
    val wrong2: String = Box(4).withOffset()
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    let mismatches = rendered
        .lines()
        .filter(|line| line.contains("kotlin.type-mismatch"))
        .count();
    assert_eq!(
        mismatches, 2,
        "the member's `String describe` is chosen over the extension's `Int`, and both extensions resolve: {rendered}"
    );
}

/// An extension another *file* declares is in scope through the file's own
/// package, a star import and an explicit import — the three ways a top-level
/// declaration of the workspace is reached ([KLS
/// `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)).
#[test]
fn an_extension_another_file_declares_is_in_scope() {
    const DECLARATION: &str = r#"
package p

class Box(val value: Int)

fun Box.doubled(): Int = value * 2
"#;
    let cases = [
        // The file's own package.
        (
            "/src/main/kotlin/p/SamePackage.kt",
            "\npackage p\n\nfun probe(): Int = Box(2).doubled()\n",
        ),
        // A star import.
        (
            "/src/main/kotlin/q/Star.kt",
            "\npackage q\n\nimport p.*\n\nfun probe(): Int = Box(2).doubled()\n",
        ),
        // An explicit import of the extension itself.
        (
            "/src/main/kotlin/q/Explicit.kt",
            "\npackage q\n\nimport p.Box\nimport p.doubled\n\nfun probe(): Int = Box(2).doubled()\n",
        ),
    ];
    for (path, use_site) in cases {
        let (db, file) = kotlin_fixture(&[
            ("/src/main/kotlin/p/Declaration.kt", DECLARATION),
            (path, use_site),
        ]);
        // File 1 is the declaration, file 2 the call site.
        let rendered = render_bodies(&db, FileId::from_raw(2));
        assert!(
            !rendered.contains("kotlin."),
            "{path} resolves the extension: {rendered}"
        );
    }
}

/// Named arguments and default values decide applicability ([KLS
/// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)):
/// each written name lands on its parameter, an omitted one must declare a
/// default, and a call that names nothing it can fill is not applicable.
///
/// The oracle is the declared type of each read: a call that resolves to the
/// `Int`-returning `describe` is a mismatch against a `String`, and one that
/// resolves to nothing is the error type, which no check reports — so the count
/// of mismatches is the count of calls that resolved.
#[test]
fn named_arguments_land_on_their_parameters() {
    let source = r#"
class Holder(val value: Int)

fun describe(a: Int, b: String = ""): Int = a

fun probe() {
    val byName: String = describe(a = 1)
    val reordered: String = describe(b = "x", a = 1)
    val omitted: String = describe(2)
    val supplied: String = describe(2, "x")
    val constructed: String = Holder(value = 3)
    val tooMany: String = describe(1, "x", 2)
    val unknownName: String = describe(c = 1)
    val twice: String = describe(a = 1, a = 2)
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let rendered = render_bodies(&db, file);
    let mismatches = rendered
        .lines()
        .filter(|line| line.contains("kotlin.type-mismatch"))
        .count();
    assert_eq!(
        mismatches, 5,
        "the four `describe` calls that resolve by name, position or default and the named constructor do: {rendered}"
    );
}

// -- library top-level declarations and extensions ----------------------------

/// One facade class of the fixture's library: the class a Kotlin library's
/// compiler emits for a file's top-level declarations.
fn facade(
    fqn: &'static str,
    methods: &'static [(&'static str, &'static str)],
    method_sigs: &'static [&'static str],
    method_access: &'static [u16],
) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class: Some("java/lang/Object"),
        interfaces: &[],
        access: 0x0031,
        fields: &[],
        field_access: &[],
        methods,
        method_sigs,
        method_access,
        sig: None,
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        method_deprecations: &[],
        method_defaults: &[],
    }
}

/// A Kotlin library's top-level declarations are the *static members of the
/// `<File>Kt` facade class* of their package
/// (<https://kotlinlang.org/docs/java-interop.html#package-level-functions>),
/// and its extensions are those statics whose **first parameter is the
/// receiver** (<https://kotlinlang.org/docs/java-interop.html#static-methods>).
///
/// kotlinc 2.4.20 compiles the same source clean against the standard library,
/// whose own facades carry `listOf` and `isBlank`; the fixture hand-encodes the
/// two shapes so the claim is pinned without a stdlib on the classpath.
#[test]
fn library_top_level_declarations_and_extensions_resolve() {
    let facades = vec![
        // `public static <T> List<T> listOf(T element)`.
        facade(
            "kotlin/collections/CollectionsKt",
            &[("listOf", "(Ljava/lang/Object;)Ljava/util/List;")],
            &["<T:Ljava/lang/Object;>(TT;)Ljava/util/List<TT;>;"],
            &[0x0009],
        ),
        // `public static boolean isBlank(String)` — the extension
        // `fun String.isBlank(): Boolean`.
        facade(
            "kotlin/text/StringsKt",
            &[("isBlank", "(Ljava/lang/String;)Z")],
            &[""],
            &[0x0009],
        ),
    ];
    let source = r#"
fun probe(): Boolean {
    val list: List<String> = listOf("a")
    val blank: Boolean = "a".isBlank()
    return blank && list.isEmpty()
}
"#;
    let (db, file) = kotlin_fixture_with(&[("/src/main/kotlin/Sample.kt", source)], facades);
    let rendered = render_bodies(&db, file);
    assert!(
        !rendered.contains("kotlin."),
        "the library's top-level function and extension resolve: {rendered}"
    );
}

/// A Kotlin library's *extension* is a facade's static method whose first
/// parameter is the receiver, and the classfile writes the receiver position as
/// a *class type parameter*, often through a wildcard
/// (`Function1<? super T, Unit>` for `T.() -> Unit`)
/// (<https://kotlinlang.org/docs/java-interop.html#static-methods>): the
/// receiver's own type binds it, so `apply { }`'s `this` is the receiver and
/// `let { }`'s `it` its type.
#[test]
fn a_library_extension_binds_its_receiver() {
    let facades = vec![
        // `public static final <T> T apply(T receiver, Function1<? super T, Unit> block)`.
        facade(
            "kotlin/StandardKt",
            &[
                (
                    "apply",
                    "(Ljava/lang/Object;Lkotlin/jvm/functions/Function1;)Ljava/lang/Object;",
                ),
                (
                    "let",
                    "(Ljava/lang/Object;Lkotlin/jvm/functions/Function1;)Ljava/lang/Object;",
                ),
            ],
            &[
                "<T:Ljava/lang/Object;>(TT;Lkotlin/jvm/functions/Function1<-TT;Lkotlin/Unit;>;)TT;",
                "<T:Ljava/lang/Object;R:Ljava/lang/Object;>(TT;Lkotlin/jvm/functions/Function1<-TT;+TR;>;)TR;",
            ],
            &[0x0009, 0x0009],
        ),
        // `public static final boolean isBlank(CharSequence)` — `String`'s
        // extension, whose receiver position is a *class* the argument satisfies
        // without any binding. The fixture's `kotlin.String` declares no
        // `CharSequence` supertype, so the facade takes a `String` there.
        facade(
            "kotlin/text/StringsKt",
            &[
                ("isBlank", "(Ljava/lang/String;)Z"),
                (
                    "forEach",
                    "([Ljava/lang/Object;Lkotlin/jvm/functions/Function1;)V",
                ),
            ],
            &[
                "",
                "<T:Ljava/lang/Object;>([TT;Lkotlin/jvm/functions/Function1<-TT;Lkotlin/Unit;>;)V",
            ],
            &[0x0009, 0x0009],
        ),
    ];
    let source = r#"
fun probe(list: javax.swing.JList, names: Array<String>): Boolean {
    val applied = list.apply { dragEnabled = true }
    val read = "a".let { it.length }
    val blank = "a".isBlank()
    var found = false
    names.forEach { found = it.isNotEmpty() }
    return applied.dragEnabled && blank && found
}
"#;
    // The fixture's own interop classes carry `javax.swing.JList`; the facades
    // are the library beside them.
    let mut extra = common::interop_classes();
    extra.extend(facades);
    let (db, file) = kotlin_fixture_with(&[("/src/main/kotlin/Sample.kt", source)], extra);
    let rendered = render_bodies(&db, file);
    assert!(
        !rendered.contains("kotlin."),
        "the extension's receiver binds the receiver position: {rendered}"
    );
}

/// A lambda written *after* a call's parenthesized argument list is that call's
/// last argument — the *trailing lambda*
/// (<https://kotlinlang.org/docs/lambdas.html#passing-trailing-lambdas>) — so it
/// joins the arguments the call already has, and it binds to the **last**
/// parameter, not to the first one the written arguments left unfilled: the
/// `joinToString(separator, …) { transform }` of the standard library is a
/// function whose *last* parameter is the lambda.
///
/// kotlinc 2.4.20 compiles the fixture clean.
#[test]
fn a_trailing_lambda_is_the_calls_last_argument() {
    let source = r#"
class Panel {
    private fun <T> update(configObject: Any, key: String, newValue: T, setter: (T) -> Unit) {
        setter(newValue)
    }

    private fun bind(currentValue: String, key: String, parentObj: Any = this, setter: (String) -> Unit) {
        setter(currentValue)
    }

    fun use(config: String) {
        update(config, "k", "v") { it.length }
        bind("x", "k") { it.length }
    }
}

fun joined(names: List<String>): String =
    names.joinToString(",") { it }
"#;
    let mut extra = common::interop_classes();
    // `public static final <T> String joinToString(Iterable<? extends T>, CharSequence separator = …, Function1<? super T, String> transform = null)`
    extra.push(facade(
        "kotlin/collections/CollectionsKt",
        &[(
            "joinToString",
            "(Ljava/lang/Iterable;Ljava/lang/CharSequence;Lkotlin/jvm/functions/Function1;)Ljava/lang/String;",
        )],
        &["<T:Ljava/lang/Object;>(Ljava/lang/Iterable<+TT;>;Ljava/lang/CharSequence;Lkotlin/jvm/functions/Function1<-TT;Ljava/lang/String;>;)Ljava/lang/String;"],
        &[0x0009],
    ));
    let (db, file) = kotlin_fixture_with(&[("/src/main/kotlin/Sample.kt", source)], extra);
    let rendered = render_bodies(&db, file);
    assert!(
        !rendered.contains("kotlin."),
        "the trailing lambda joins the call and binds to its last parameter: {rendered}"
    );
}

/// A Kotlin *built-in* classifier has no classfile of its own: the compiler
/// maps it onto a JVM type (KLS
/// `built-in-types-and-their-semantics.html`), and this model reads its members
/// and its supertypes through that JVM type. Three consequences the test pins:
/// a Kotlin *spelling* of a library class (`ArrayList`, `List`) is the same
/// classifier as the classfile it resolves to; `MutableList` and `List` are
/// related by Kotlin's declaration (both erase to `java.util.List`, so the
/// JVM's own hierarchy cannot tell them apart); and the members the *language*
/// declares on the built-ins (`Double.toInt()`) have no classfile at all.
#[test]
fn a_builtin_classifier_resolves_through_its_jvm_class() {
    let source = r#"
import java.util.ArrayList

class Holder {
    fun sizes(list: List<String>, arrayList: ArrayList<String>): Int {
        return list.size + arrayList.size
    }

    fun toInts(value: Double): Int = value.toInt()

    fun asMutable(list: ArrayList<String>): MutableList<String> = list

    fun asReadOnly(list: MutableList<String>): List<String> = list
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let tree = hir::hir_def::kotlin::plugin::tree(&db, file).expect("a Kotlin file");
    let rendered = render_types(&db, file);
    assert!(
        !rendered.contains("<error>"),
        "every written type resolves, built-ins included: {rendered}"
    );
    let bodies = render_bodies(&db, file);
    assert!(
        !bodies.contains("<error>") && !bodies.contains("kotlin."),
        "every expression of the file types: {bodies}"
    );
    let scope = scope(&db, file);
    let string = Ty::reference(
        &db,
        hir_expand::name::Name::new("kotlin.String"),
        Vec::new(),
    );
    let of = |fqn: &str| Ty::reference(&db, hir_expand::name::Name::new(fqn), vec![string]);
    let list = of("kotlin.collections.List");
    let mutable_list = of("kotlin.collections.MutableList");
    let array_list = of("kotlin.collections.ArrayList");
    // `MutableList<String> <: List<String>` — Kotlin's declaration, in one
    // direction only.
    assert!(hir_ty::kotlin_subtype(&db, &scope, &mutable_list, &list));
    assert!(!hir_ty::kotlin_subtype(&db, &scope, &list, &mutable_list));
    // `ArrayList<String>` is both: a `MutableList` by Kotlin's declaration of
    // `ArrayList`, a `List` through the JVM hierarchy of the class it maps
    // onto.
    assert!(hir_ty::kotlin_subtype(
        &db,
        &scope,
        &array_list,
        &mutable_list
    ));
    assert!(hir_ty::kotlin_subtype(&db, &scope, &array_list, &list));
    // `java.util.List` read from a classfile is the *same* classifier as the
    // Kotlin `List` the source writes.
    let java_list = Ty::reference(
        &db,
        hir_expand::name::Name::new("java.util.List"),
        vec![string],
    );
    assert!(hir_ty::kotlin_subtype(&db, &scope, &java_list, &list));
    assert!(hir_ty::kotlin_subtype(&db, &scope, &list, &java_list));
    // `Double.toInt()` is a member the *language* declares on the built-in
    // numeric type: no classfile carries it.
    let double = Ty::reference(
        &db,
        hir_expand::name::Name::new("kotlin.Double"),
        Vec::new(),
    );
    let members = hir_ty::kotlin_declared_members(
        &db,
        &scope,
        &double,
        &hir_expand::name::Name::new("toInt"),
        hir_ty::kotlin::method::CallSite {
            file,
            item: Some(hir_expand::ids::ItemId({
                let mut ids = tree.items.iter().map(|(id, _)| id);
                ids.next().expect("a declaration")
            })),
            receiver: hir_ty::kotlin::method::ReceiverKind::Value,
        },
    );
    assert_eq!(
        members
            .iter()
            .map(|member| hir_ty::display_kotlin(&db, member.ty(&db)).to_string())
            .collect::<Vec<_>>(),
        vec!["Int".to_owned()],
        "`Double.toInt()` is a member the language declares, and is `Int`"
    );
}

/// The `field` of a property's accessor is its *backing field*, and its type is
/// the property's; a setter's untyped `value` parameter is that type too. Both
/// are compiler-made declarations no classfile carries, so a `set(value) { field
/// = value }` pair is where a model that reads only written types breaks.
#[test]
fn a_property_accessor_writes_its_own_backing_field() {
    let source = r#"
class Holder {
    var text: String = ""
        set(value) {
            field = value
        }
        get() = field
}
"#;
    let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    let bodies = render_bodies(&db, file);
    assert!(
        !bodies.contains("<error>") && !bodies.contains("kotlin."),
        "`field` and `value` are the property's own type in both accessors: {bodies}"
    );
    assert_eq!(
        bodies
            .lines()
            .filter(|line| line.ends_with(": String"))
            .count(),
        3,
        "the field read, the field write and the setter's value are `String`: {bodies}"
    );
}

// -- the body inference a Kotlin file's call sites need ------------------------

/// The call-site and subtyping rules the Kotlin body layer resolves against,
/// each confirmed with kotlinc 2.4.20 first.
///
/// A platform type is what a Kotlin file sees a *classfile* type as
/// ([`hir_ty::kotlin::ty::ty_from_java`]), and every one of these fixtures has
/// at least one: a library call's result, a Java getter's return, a facade's
/// function type. kotlinc compiles every fixture here clean.
mod call_sites {
    use super::*;

    /// The type a call is *used* at completes its type arguments: `val list:
    /// MutableList<String> = LinkedList()` constructs a `LinkedList<String>`
    /// ([KLS
    /// `type-inference.html#call-completion`](https://kotlinlang.org/spec/type-inference.html#call-completion)).
    #[test]
    fn an_expected_type_completes_a_constructors_arguments() {
        let source = r#"
import java.util.LinkedList

fun build(): MutableList<String> {
    val list: MutableList<String> = LinkedList()
    return list
}
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "the expected type completes the constructor: {rendered}"
        );
        assert!(
            rendered.contains("LinkedList<String>"),
            "the constructed class takes the expected type's argument: {rendered}"
        );
    }

    /// A call's *written* type argument binds the callee's parameter, and an
    /// argument the call does not write is never replaced by the enclosing
    /// receiver: `mutableListOf<File>()` inside a class body is a
    /// `MutableList<File>`, not a list of the class.
    #[test]
    fn a_call_keeps_the_type_it_was_written_with() {
        let facades = vec![facade(
            "kotlin/collections/CollectionsKt",
            &[
                ("mutableListOf", "()Ljava/util/List;"),
                ("mutableListOf", "([Ljava/lang/Object;)Ljava/util/List;"),
            ],
            &[
                "<T:Ljava/lang/Object;>()Ljava/util/List<TT;>;",
                "<T:Ljava/lang/Object;>([TT;)Ljava/util/List<TT;>;",
            ],
            &[0x0009, 0x0009],
        )];
        let source = r#"
class Container {
    fun build(): List<String> {
        val names = mutableListOf<String>()
        return names
    }
}
"#;
        let mut extra = common::interop_classes();
        extra.extend(facades);
        let (db, file) = kotlin_fixture_with(&[("/src/main/kotlin/Sample.kt", source)], extra);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "the written type argument binds the callee's parameter: {rendered}"
        );
        assert!(
            !rendered.contains("Container"),
            "the enclosing classifier is not the call's type argument: {rendered}"
        );
    }

    /// A *mapped* classfile classifier and its Kotlin classifier are one class
    /// (<https://kotlinlang.org/docs/java-interop.html#mapped-types>): a
    /// `MutableList<String>` written in Kotlin is a `java.util.List` on the
    /// classpath, so the `ArrayList` a Java library answers is one.
    ///
    /// The name `List` has *two* candidates here — the `java.awt.List` the
    /// fixture's star import brings in, which is not generic, and
    /// `kotlin.collections.List` — and `List<String>` is the interface, exactly
    /// as kotlinc 2.4.20 reads it.
    #[test]
    fn a_mapped_classifier_is_its_kotlin_classifier() {
        let mut extra = common::interop_classes();
        extra.push(ClassSpec {
            fqn: "java/awt/List",
            super_class: Some("java/lang/Object"),
            interfaces: &[],
            access: 0x0021,
            fields: &[],
            field_access: &[],
            methods: &[("<init>", "()V")],
            method_sigs: &[""],
            method_access: &[0x0001],
            sig: None,
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            method_deprecations: &[],
            method_defaults: &[],
        });
        let source = r#"
import java.awt.*
import java.util.*

fun read(): List<String> {
    val list: MutableList<String> = ArrayList()
    for (name in list) {
        println(name)
    }
    return list
}
"#;
        let (db, file) = kotlin_fixture_with(&[("/src/main/kotlin/Sample.kt", source)], extra);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "a `List<String>` is the Kotlin interface, not the AWT component: {rendered}"
        );
        assert!(
            !rendered.contains("java.awt.List"),
            "the non-generic candidate is not the one `List<String>` names: {rendered}"
        );
    }

    /// A SAM lambda's implicit parameter comes from the receiver's type
    /// argument, through a *platform* receiver: `optional.map { it.length }`
    /// binds `it` to the `Optional`'s own argument
    /// (<https://kotlinlang.org/docs/java-interop.html#sam-conversions>) — the
    /// receiver is a library call's result, so its type is the `Optional<T>!`
    /// the classfile denotes.
    #[test]
    fn a_sam_lambda_binds_its_parameter_through_a_platform_receiver() {
        let source = r#"
import java.util.Optional

fun length(): Int {
    return Optional.of("a").map { it.length }.get()
}

fun lengthOf(optional: Optional<String>): Int {
    return optional.map { it.length }.get()
}
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "the lambda's `it` is the receiver's type argument: {rendered}"
        );
    }

    /// A function-typed parameter's *receiver* takes the receiver's arguments:
    /// `Box<Method>().use { … }` has a `Method.() -> Unit`, so `isAccessible` is
    /// the `Method`'s ([KLS
    /// `type-system.html#type-containment`](https://kotlinlang.org/spec/type-system.html#type-containment)
    /// substitutes the class's parameters in its members' types).
    #[test]
    fn a_lambda_receiver_takes_the_receivers_arguments() {
        let source = r#"
import javax.swing.JList

class Box<T>(val value: T) {
    fun use(f: T.() -> Unit) {
        f(value)
    }
}

fun use(list: JList<String>) {
    val box = Box<JList<String>>(list)
    box.use { dragEnabled = true }
}
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "the lambda's receiver is the substituted parameter: {rendered}"
        );
    }

    /// A mutable map's entries are *mutable* entries — `MutableMap.entries` is
    /// a `MutableSet<MutableEntry<K, V>>`
    /// (<https://kotlinlang.org/api/core/kotlin-stdlib/kotlin.collections/-mutable-map/>),
    /// and `MutableIterator<MutableEntry<…>>` is a
    /// `MutableIterator<Map.Entry<…>>` because `MutableIterator` is declared
    /// `out`
    /// (<https://kotlinlang.org/api/core/kotlin-stdlib/kotlin.collections/-mutable-iterator/>).
    #[test]
    fn a_mutable_maps_entries_are_mutable_entries() {
        let source = r#"
fun entries(map: MutableMap<String, Int>): MutableIterator<Map.Entry<String, Int>> {
    return map.entries.iterator()
}
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "a mutable entry is an entry: {rendered}"
        );
    }

    /// A `suspend` function answers the type its continuation carries. The
    /// classfile shape is `Object f(…, Continuation<? super T>)`
    /// (<https://kotlinlang.org/docs/java-to-kotlin-interop.html#suspending-functions>),
    /// and the `Object` its signature writes *is* the `T`: a Kotlin caller of
    /// the fixture's `runSuspend { "x" }` has a `String`, not an `Any!`.
    #[test]
    fn a_suspend_functions_return_is_its_continuations_argument() {
        let facades = vec![facade(
            "kotlin/SuspendingKt",
            &[(
                "runSuspend",
                "(Lkotlin/jvm/functions/Function0;Lkotlin/coroutines/Continuation;)Ljava/lang/Object;",
            )],
            &[
                "<T:Ljava/lang/Object;>(Lkotlin/jvm/functions/Function0<+TT;>;Lkotlin/coroutines/Continuation<-TT;>;)Ljava/lang/Object;",
            ],
            &[0x0009],
        )];
        let source = r#"
fun value(): String {
    return runSuspend { "x" }
}
"#;
        let mut extra = common::interop_classes();
        extra.extend(facades);
        let (db, file) = kotlin_fixture_with(&[("/src/main/kotlin/Sample.kt", source)], extra);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "a suspend call answers its continuation's type: {rendered}"
        );
        assert!(
            !rendered.contains("Any"),
            "the erased `Object` the signature writes is not the call's type: {rendered}"
        );
    }

    /// `x::class` is the `KClass` of `x`'s type
    /// (<https://kotlinlang.org/docs/reflection.html#class-references>), and
    /// its `java` property is the classfile's `getJavaClass` — a `@JvmName` the
    /// library's metadata carries and this model's table stands in for.
    #[test]
    fn a_class_reference_is_a_kclass_of_its_receiver() {
        let source = r#"
fun methods(value: Any): Int {
    val declared = value::class.java.declaredMethods
    return declared.size
}
"#;
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let rendered = render_bodies(&db, file);
        assert!(
            !rendered.contains("kotlin."),
            "the class reference reaches the class's members: {rendered}"
        );
        assert!(
            rendered.contains("KClass<Any>"),
            "`value::class` is a `KClass` of the value's type: {rendered}"
        );
    }
}

// -- the declaration-level checks ---------------------------------------------

/// The declaration checks, each confirmed with kotlinc 2.4.20 first: the fixture
/// of every case is a file the compiler rejects with exactly the message the
/// check renders, and `clean` is a file it accepts.
mod decl_checks {
    use super::*;

    /// The findings of one Kotlin file, as `code message` lines.
    fn findings(src: &str) -> Vec<String> {
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", src)]);
        hir_ty::kotlin_class_diagnostics(&db, file)
            .into_iter()
            .map(|diagnostic| format!("{} {}", diagnostic.code().as_str(), diagnostic.message(&db)))
            .collect()
    }

    /// Every rule the checker implements, each on a source kotlinc 2.4.20
    /// rejects with the message asserted.
    #[test]
    fn the_compilers_declaration_findings_are_reported() {
        let source = r#"
open class Open1 {
    open fun h() {}
}

class HidesIt : Open1() {
    fun h() {}
}

open class Final1 {
    fun g() {}
}

class OverridesFinal : Final1() {
    override fun g() {}
}

class Plain

class OverridesNothing : Plain() {
    override fun nothingHere() {}
}

abstract class Base2 {
    abstract fun g(): Int
}

class Missing1 : Base2()

interface I2 {
    fun f()
}

class Missing2 : I2

open class WithRequired(val x: Int)

class NoInit : WithRequired

class Dup {
    fun a(x: Int) {}
    fun a(y: Int) {}
}

data fun badModifier() {}

lateinit val badVal: String

inline val badInline: Int = 1
"#;
        let found = findings(source);
        for expected in [
            "kotlin.needs-override-modifier 'h' hides member of supertype 'Open1' and needs an 'override' modifier.",
            "kotlin.final-member-overridden 'g' in 'Final1' is final and cannot be overridden.",
            "kotlin.overrides-nothing 'nothingHere' overrides nothing.",
            "kotlin.unimplemented-abstract-member class 'Missing1' is not abstract and does not implement abstract base class member:\nfun g(): Int",
            "kotlin.unimplemented-abstract-member class 'Missing2' is not abstract and does not implement abstract member:\nfun f(): Unit",
            "kotlin.supertype-not-initialized this type has a constructor, so it must be initialized here.",
            "kotlin.conflicting-overloads conflicting overloads:\nfun a(x: Int): Unit",
            "kotlin.modifier-not-applicable modifier 'data' is not applicable to 'top level function'.",
            "kotlin.lateinit-on-immutable-property 'lateinit' modifier is allowed only on mutable properties.",
        ] {
            assert!(
                found.iter().any(|finding| finding == expected),
                "expected {expected:?} among {found:#?}"
            );
        }
    }

    /// The same rules on a file kotlinc 2.4.20 accepts: every check stays quiet,
    /// which is what keeps a project's own sources free of false positives.
    #[test]
    fn a_compiler_clean_file_reports_nothing() {
        let source = r#"
open class Base {
    open fun h() {}
}

class Derived : Base() {
    override fun h() {}
}

abstract class Abstract1 {
    abstract fun g(): Int
}

class Concrete : Abstract1() {
    override fun g(): Int = 1
}

interface I {
    fun f()
}

class Impl : I {
    override fun f() {}
}

open class WithDefault(val x: Int = 0)

class UsesDefault : WithDefault()

open class AbstractBase {
    abstract val isEnabled: Boolean
}

class ExtendsIt : AbstractBase() {
    override val isEnabled: Boolean = true
}

class Pairs {
    fun a(x: Int) {}
    fun a(y: String) {}
}

lateinit var ok: String

val okVal: String = ""
"#;
        let found = findings(source);
        assert!(found.is_empty(), "a clean file reports nothing: {found:#?}");
    }

    /// The findings are anchored at the declaration they are about: the name of
    /// the member, or the whole declaration where it has none.
    #[test]
    fn a_finding_carries_the_declarations_range() {
        let source = "open class Final1 { fun g() {} }\nclass OverridesFinal : Final1() { override fun g() {} }\n";
        let (db, file) = kotlin_fixture(&[("/src/main/kotlin/Sample.kt", source)]);
        let findings = hir_ty::kotlin_class_diagnostics(&db, file);
        let ranges: Vec<_> = findings.iter().filter_map(|f| f.range()).collect();
        assert!(!ranges.is_empty(), "a finding is anchored: {findings:#?}");
        for range in ranges {
            let text = source[usize::from(range.start())..usize::from(range.end())].to_owned();
            assert!(
                text == "g" || text.starts_with("class OverridesFinal"),
                "the range covers a declaration: {text:?}"
            );
        }
    }
}

/// The other direction of the generic bridge: a **Java** caller of a Kotlin
/// source generic class reads the members at the arguments its use writes.
///
/// kotlinc 2.4.20's classfile for `class Box<T>(val value: T) { fun get(): T }`
/// carries `public final T get();` — the *erasure* is
/// `()Ljava/lang/Object;`, and the `Signature` attribute's `()TT;` is what
/// instantiates the declaring class's parameter:
///
/// ```text
/// public final class a.Box<T> {
///   private final T value;
///   public a.Box(T);
///   public final T getValue();
///   public final T get();
/// }
/// ```
///
/// A Java caller of `Box<String>` therefore reads `String get()`, which is what
/// javac checks: `int wrong(Box<String> box) { return box.get(); }` is
/// `incompatible types: String cannot be converted to int`.
#[test]
fn a_java_caller_reads_a_kotlin_generic_class_at_its_arguments() {
    const KOTLIN_BOX: &str = r#"
package a

class Box<T>(val value: T) {
    fun get(): T = value
}
"#;
    const JAVA_USE: &str = r#"
package a;

class Use {
    static String read(Box<String> box) {
        return box.get();
    }

    static int wrong(Box<String> box) {
        return box.get();
    }

    static String viaValue(Box<String> box) {
        return box.getValue();
    }
}
"#;
    let files: &[(&str, &str)] = &[
        ("/src/main/java/a/Use.java", JAVA_USE),
        ("/src/main/kotlin/a/Box.kt", KOTLIN_BOX),
    ];
    let (db, _) = kotlin_fixture(files);
    let rendered = common::render_body_types(&db, files);
    assert!(
        rendered.contains("e1: java.lang.String") && rendered.contains("e5: java.lang.String"),
        "the member reads as the argument the use wrote: {rendered}"
    );
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.contains("diags:"))
            .count(),
        1,
        "`read` and `viaValue` are the `String` they return; only `wrong` reports: {rendered}"
    );
    assert!(
        rendered.contains("Incompatible types. Found: 'String', required: 'int'"),
        "`get()` is the `String` of `Box<String>`: {rendered}"
    );
}

/// A **Java** caller resolves a Kotlin enum's generated statics: `values()` is
/// the enum's array and `valueOf(String)` an entry of it, exactly as it is for
/// a Java enum (the classfile's `public static Color[] values();` and
/// `public static Color valueOf(java.lang.String);`,
/// [`crate::kotlin::jvm_view`]). `red.name()` is `java.lang.Enum`'s own member,
/// reached through the enum's supertype, which the same walk answers.
#[test]
fn a_java_caller_resolves_a_kotlin_enums_values_and_value_of() {
    const KOTLIN_ENUM: &str = r#"
package a

enum class Color { RED, GREEN }
"#;
    const JAVA_USE: &str = r#"
package a;

class Use {
    static String name() {
        Color red = Color.valueOf("RED");
        return red.name();
    }

    static int wrong() {
        int all = Color.values();
        return all;
    }
}
"#;
    let files: &[(&str, &str)] = &[
        ("/src/main/java/a/Use.java", JAVA_USE),
        ("/src/main/kotlin/a/Color.kt", KOTLIN_ENUM),
    ];
    let (db, _) = kotlin_fixture(files);
    let rendered = common::render_body_types(&db, files);
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.contains("diags:"))
            .count(),
        1,
        "`valueOf` and `name()` resolve; only the deliberately wrong binding reports: {rendered}"
    );
    assert!(
        rendered.contains("Incompatible types. Found: 'Color[]', required: 'int'"),
        "`values()` is the enum's array: {rendered}"
    );
}

/// A Java *static* member is reached through its declaring **classifier** and
/// nowhere else: kotlinc 2.4.20 reports `unresolved reference 'stat' on
/// receiver of type 'N'` for `n.stat()` and keeps `N.stat()` — the class-name
/// form Java interop is written in
/// (<https://kotlinlang.org/docs/java-interop.html#static-methods>) — while
/// `Sub.stat()`, where `Sub` is a Kotlin subclass of the Java class, is
/// unresolved too (`error: unresolved reference 'stat'`).
///
/// The receiver's *kind* is what this layer reads: a name that is a local,
/// parameter or lambda parameter shadows the classifier it spells ([KLS
/// `scopes-and-identifiers.html#scopes-and-identifiers`](https://kotlinlang.org/spec/scopes-and-identifiers.html#scopes-and-identifiers)),
/// so `val N = other; N.stat()` is the value receiver a static cannot be
/// reached through.
#[test]
fn a_java_static_member_is_reached_only_through_its_class() {
    const JAVA_N: &str = r#"
package a;

public class N {
    public static int stat() {
        return 1;
    }

    public int inst() {
        return 2;
    }
}
"#;
    const KOTLIN_USE: &str = r#"
package a

class Sub : N()

fun statOnClass(): Int = N.stat()

fun statOnValue(n: N): Int = n.stat()

fun instOnValue(n: N): Int = n.inst()

fun statOnSubclass(): Int = Sub.stat()

fun statOnLocal(other: N): Int {
    val N = other
    return N.stat()
}
"#;
    let (db, _) = kotlin_fixture(&[
        ("/src/main/java/a/N.java", JAVA_N),
        ("/src/main/kotlin/a/Use.kt", KOTLIN_USE),
    ]);
    // File 1 is the Java source, the Kotlin one file 2.
    let file = FileId::from_raw(2);
    let types = |name: &str| -> Vec<String> {
        let tree = hir::hir_def::kotlin::plugin::tree(&db, file).expect("a Kotlin file");
        let item = tree
            .items
            .iter()
            .find_map(|(id, data)| match data {
                hir_def::kotlin::item_tree::KotlinItemData::Function(function)
                    if function.name.as_str() == name =>
                {
                    Some(hir_expand::ids::ItemId(id))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no function {name}"));
        let body = hir_ty::kotlin_body_types(&db, file, item);
        let mut lines: Vec<String> = body
            .exprs
            .iter()
            .map(|(expr, ty)| format!("e{}: {}", expr.0.0, hir_ty::display_kotlin(&db, *ty)))
            .collect();
        lines.sort();
        lines
    };
    // `N.stat()` — the declaring class's own static.
    let on_class = types("statOnClass");
    assert_eq!(
        on_class
            .iter()
            .filter(|line| line.ends_with(": Int"))
            .count(),
        1,
        "`N.stat()` is the `Int` static: {on_class:?}"
    );
    assert!(
        !on_class.iter().any(|line| line.contains("<error>")),
        "`N.stat()` resolves: {on_class:?}"
    );
    // `n.stat()` — a static through a value receiver.
    let on_value = types("statOnValue");
    assert!(
        on_value.iter().any(|line| line.contains("<error>")),
        "`n.stat()` is an unresolved reference: {on_value:?}"
    );
    let body = {
        let tree = hir::hir_def::kotlin::plugin::tree(&db, file).expect("a Kotlin file");
        let item = tree
            .items
            .iter()
            .find_map(|(id, data)| match data {
                hir_def::kotlin::item_tree::KotlinItemData::Function(function)
                    if function.name.as_str() == "statOnValue" =>
                {
                    Some(hir_expand::ids::ItemId(id))
                }
                _ => None,
            })
            .expect("statOnValue");
        hir_ty::kotlin_body_types(&db, file, item)
    };
    assert!(
        !body
            .resolved
            .values()
            .any(|member| matches!(member, hir_ty::KotlinResolvedMember::Java(_))),
        "a static reached through a value records no Java member: {:?}",
        body.resolved
    );
    // `n.inst()` — the instance member still resolves.
    let on_instance = types("instOnValue");
    assert!(
        !on_instance.iter().any(|line| line.contains("<error>")),
        "`n.inst()` resolves: {on_instance:?}"
    );
    // `Sub.stat()` — a Kotlin subclass inherits none of its Java supertype's
    // statics.
    let on_subclass = types("statOnSubclass");
    assert!(
        on_subclass.iter().any(|line| line.contains("<error>")),
        "`Sub.stat()` is an unresolved reference: {on_subclass:?}"
    );
    // A local that spells the class name shadows it.
    let on_local = types("statOnLocal");
    assert!(
        on_local.iter().any(|line| line.contains("<error>")),
        "a local named like the class is a value: {on_local:?}"
    );
}
