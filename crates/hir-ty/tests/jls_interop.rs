//! The Java↔Kotlin interop of the type layer, in the JLS suite's fixture style:
//! one source set holding Java and Kotlin files, checked the way the Java
//! suites check a class — a Java body calling, reading and extending the Kotlin
//! declarations beside it.
//!
//! Every Kotlin fixture here compiles with kotlinc 2.4.20 (JRE 21.0.11) and
//! every Java fixture with `javac` from the same JDK; the *shapes* the tests
//! assert — `getX()`, `INSTANCE`, `FooKt.bar()`, a `const val` field — are the
//! ones `javap -p` reports for the compiled Kotlin half.

use hir_ty::Ty;
use vfs::FileId;

mod common;
use common::{
    TestDatabase, fixture_library, jdk_fixture, kotlin_stdlib_classes,
    render_body_diagnostic_spans, render_body_types,
};

use base_db::{FileChange, SourceDatabase, SourceRoot, SourceRootId};
use hir::LibraryKind;
use tempfile::TempDir;
use triomphe::Arc;
use vfs::{AbsPathBuf, VfsPath, file_set::FileSet};

/// The Kotlin declaration the Java fixtures use.
const UTIL_KT: &str = r#"
@file:JvmName("Facade")

package a

const val LIMIT: Int = 3

class Point(val x: Int) {
    var y: Int = 0

    val isZero: Boolean
        get() = x == 0

    fun move(dx: Int): Int = x + dx

    open fun describe(): String = "point"
}

object Util {
    fun name(): String = "util"
}

class Circle : Point(1) {
    override fun describe(): String = "circle"
}

fun topLevel(seed: Int): Int = seed
"#;

/// A Java file naming those declarations through the JVM shapes the compiler
/// emits for them. `javac` compiles it against the Kotlin output.
const USE_JAVA: &str = r#"
package a;

public class Use {
    int run(Point point) {
        point.getX();
        point.setY(2);
        point.isZero();
        point.move(1);
        point.describe();
        Object name = Util.INSTANCE.name();
        Object limit = Facade.LIMIT;
        Object top = Facade.topLevel(1);
        Point circle = new Circle();
        return 0;
    }
}
"#;

/// A source set holding `files` (Java and Kotlin), compiled against the JDK
/// fixture and a hand-encoded Kotlin standard library — the one source set of a
/// mixed JVM project, which is where Java sees Kotlin and Kotlin sees Java.
fn interop_fixture(files: &[(&str, &str)]) -> (TestDatabase, hir::SourceSetId) {
    let dir = TempDir::new().unwrap();
    let jdk = jdk_fixture();
    let (stdlib, stdlib_path) =
        fixture_library(&dir, "kotlin-stdlib.jar", &kotlin_stdlib_classes());

    let mut db = TestDatabase::new();
    let mut file_set = FileSet::default();
    for (i, (path, _)) in files.iter().enumerate() {
        file_set.insert(
            vfs::FileId::from_raw((i + 1) as u32),
            VfsPath::from(AbsPathBuf::assert_utf8((*path).into())),
        );
    }
    let mut change = FileChange::default();
    change.set_roots(vec![SourceRoot::new(file_set)]);
    for (i, (_, text)) in files.iter().enumerate() {
        change.change_file(
            vfs::FileId::from_raw((i + 1) as u32),
            Some((*text).to_owned()),
        );
    }
    change.apply(&mut db);

    let source_set = hir::SourceSetId {
        project: hir::ProjectId(0),
        kind: hir::SourceSetKind::Main,
    };
    let mut data = hir::ProjectGraphData::default();
    let jdk_info = hir::LibraryInfo::new(
        LibraryKind::Jar,
        AbsPathBuf::assert_utf8(jdk.jar.as_std_path().to_owned()),
    );
    data.libraries.insert(jdk.lib, jdk_info);
    data.libraries
        .insert(stdlib, hir::LibraryInfo::new(LibraryKind::Jar, stdlib_path));
    data.jdk_libraries.push(jdk.lib);
    data.source_sets.insert(
        source_set.clone(),
        Arc::new(hir::Classpath {
            entries: vec![
                hir::ClasspathEntry::Library(jdk.lib),
                hir::ClasspathEntry::Library(stdlib),
            ],
        }),
    );
    data.source_root_to_source_set
        .insert(SourceRootId(0), source_set.clone());
    hir::set_project_graph(&mut db, data);
    // The database outlives the fixtures; their jars are read lazily.
    std::mem::forget(dir);
    jdk.keep_alive();
    (db, source_set)
}

/// A name that Kotlin declares is resolvable from Java, through the JVM shape
/// the compiler gives it: `point.getX()`, `point.setY(2)`, `Util.INSTANCE` and
/// the `@file:JvmName("Facade")` facade's statics.
#[test]
fn a_java_body_resolves_a_kotlin_declaration() {
    let files = [
        ("/src/main/kotlin/a/Util.kt", UTIL_KT),
        ("/src/main/java/a/Use.java", USE_JAVA),
    ];
    let (db, _) = interop_fixture(&files);
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    // The Java body reports *nothing*: every Kotlin declaration it names
    // resolves through the JVM shape the compiler gives it — the accessors of
    // `val x`/`var y`/`val isZero`, the `object`'s `INSTANCE`, the
    // `@file:JvmName("Facade")` facade's statics, and `Circle`'s inherited
    // constructor.
    assert!(
        !diagnostics.contains("method run"),
        "every Kotlin declaration resolves from Java: {diagnostics}"
    );
    // The *types* are the Kotlin ones in their JVM form: `getX()` is `int`,
    // `isZero()` a `boolean` and the constructor call a `Circle`.
    let types = render_body_types(&db, &files);
    for expected in ["method run(a.Point): int", "int", "boolean", "Circle"] {
        assert!(
            types.contains(expected),
            "the JVM view's types reach the Java body ({expected:?}): {types}"
        );
    }
}

/// The same fixture, at the *type* level: the Java layer must key a Kotlin
/// class and answer its member set without touching the empty Java item tree
/// (which is what `ClassKey::of` used to index).
#[test]
fn the_java_layer_answers_for_a_kotlin_class() {
    let files = [
        ("/src/main/kotlin/a/Util.kt", UTIL_KT),
        ("/src/main/java/a/Use.java", USE_JAVA),
    ];
    let (db, _) = interop_fixture(&files);
    let scope = hir::ResolutionScope::SourceSet(
        hir::source_set_for_file(&db, FileId::from_raw(1)).unwrap(),
    );
    let point = Ty::reference(&db, "a.Point", Vec::new());
    let ctx = hir_ty::InvocationContext::external(&scope);
    let methods: Vec<String> = hir_ty::member_set(&db, &scope, &point, "move", &ctx)
        .iter()
        .map(|method| method.display(&db).to_string())
        .collect();
    assert!(
        methods.iter().any(|method| method.contains("move")),
        "the JVM view of a Kotlin class answers: {methods:?}"
    );
    // Two Kotlin declarations the JVM view must *not* expose as fields: `val x`
    // is a getter, and a `var y` a getter/setter pair.
    assert!(
        hir_ty::pick_field(&db, &scope, &point, "x", &ctx).is_none(),
        "`val x` is an accessor, not a field"
    );
    assert!(
        hir_ty::pick_field(&db, &scope, &point, "y", &ctx).is_none(),
        "`var y` is an accessor pair, not a field"
    );
    let accessors: Vec<String> = ["getX", "setY", "isZero"]
        .iter()
        .flat_map(|name| {
            hir_ty::member_set(&db, &scope, &point, name, &ctx)
                .iter()
                .map(|method| method.display(&db).to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        accessors.len(),
        3,
        "the JVM accessors are what a Java caller names: {accessors:?}"
    );
}

/// A top-level declaration of *another* Kotlin file is resolvable: a property
/// or a function the file's own package or an import names is the one the
/// workspace's symbol index holds under that fully qualified name ([KLS
/// `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)).
///
/// kotlinc compiles the pair clean (the two files are one module).
#[test]
fn a_kotlin_file_uses_another_files_top_level_declaration() {
    let files = [
        (
            "/src/main/kotlin/a/Decls.kt",
            "package a\n\nval LIMIT: Int = 3\n\nfun doubled(seed: Int): Int = seed * 2\n",
        ),
        (
            "/src/main/kotlin/b/Use.kt",
            "package b\n\nimport a.LIMIT\nimport a.doubled\n\nfun use(): Int = doubled(LIMIT)\n\nval local: Int = doubled(1)\n",
        ),
    ];
    let (db, _) = interop_fixture(&files);
    let file = FileId::from_raw(2);
    let tree = hir::hir_def::kotlin::plugin::tree(&db, file).expect("a Kotlin file");
    for (id, data) in tree.items.iter() {
        if data.body_id().is_none() {
            continue;
        }
        let types = hir_ty::kotlin_body_types(&db, file, hir_expand::ids::ItemId(id));
        for diagnostic in &types.diagnostics {
            panic!(
                "an imported declaration resolves: {} {}",
                diagnostic.code().as_str(),
                diagnostic.message(&db)
            );
        }
    }
}

/// The `kotlin.jvm` annotations are recognized by the name their application
/// *resolves* to, never by the last segment the source wrote: the qualified
/// `@kotlin.jvm.JvmName("…")` and an aliased
/// `import kotlin.jvm.JvmName as JN` are both the standard library's
/// annotation, so the classfile names the member — and its accessor —
/// accordingly.
///
/// kotlinc 2.4.20 compiles the Kotlin file clean, and `javap -p` reports the
/// classfile members `renamed()` and `qualifiedGetter()`.
#[test]
fn a_qualified_or_aliased_jvm_annotation_names_the_member() {
    let files = [
        (
            "/src/main/kotlin/a/Renamed.kt",
            "package a\n\nimport kotlin.jvm.JvmName as JN\n\nclass Renamed {\n    @JN(\"renamed\")\n    fun m(): Int = 1\n\n    @get:kotlin.jvm.JvmName(\"qualifiedGetter\")\n    val v: Int = 0\n}\n",
        ),
        (
            "/src/main/java/a/UseRenamed.java",
            "package a;\n\npublic class UseRenamed {\n    int run(Renamed renamed) {\n        renamed.renamed();\n        return renamed.qualifiedGetter();\n    }\n}\n",
        ),
    ];
    let (db, source_set) = interop_fixture(&files);
    let scope = hir::ResolutionScope::SourceSet(source_set);
    let ctx = hir_ty::InvocationContext::external(&scope);
    let renamed = Ty::reference(&db, "a.Renamed", Vec::new());
    for name in ["renamed", "qualifiedGetter"] {
        assert!(
            !hir_ty::member_set(&db, &scope, &renamed, name, &ctx).is_empty(),
            "the annotation resolved to the library's, so {name} is the JVM member"
        );
    }
    assert!(
        hir_ty::member_set(&db, &scope, &renamed, "m", &ctx).is_empty(),
        "`@JN` renamed the declaration, so its Kotlin name is not a JVM member"
    );
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        !diagnostics.contains("method run"),
        "the Java body names the renamed members: {diagnostics}"
    );
}

/// A `JvmName` the *file* declares is not the standard library's annotation:
/// an annotation application names a type, and classifiers resolve by scope, so
/// the file's own `annotation class JvmName` shadows `kotlin.jvm.JvmName` and
/// its applications rename nothing — the member and the facade keep the Kotlin
/// names the classfile then carries.
///
/// kotlinc 2.4.20 compiles the file clean; `javap -p` reports `m()` and the
/// facade `ShadowKt`, never `x()` or `Wrong`.
#[test]
fn a_shadowing_jvm_name_renames_nothing() {
    const SHADOW_KT: &str = r#"
@file:JvmName("Wrong")

package a

@Target(AnnotationTarget.FILE, AnnotationTarget.FUNCTION)
annotation class JvmName(val value: String)

class Holder {
    @JvmName("x")
    fun m(): Int = 1
}

fun top(seed: Int): Int = seed
"#;
    let files = [
        ("/src/main/kotlin/a/Shadow.kt", SHADOW_KT),
        (
            "/src/main/java/a/UseShadow.java",
            "package a;\n\npublic class UseShadow {\n    int run(Holder holder) {\n        return holder.m() + ShadowKt.top(1);\n    }\n}\n",
        ),
    ];
    let (db, source_set) = interop_fixture(&files);
    let file = FileId::from_raw(1);
    assert_eq!(
        hir::file_facade_class(&db, file).map(|facade| facade.to_string()),
        Some("ShadowKt".to_owned()),
        "the file's own `JvmName` is not the library's, so the facade is the file's stem"
    );
    let scope = hir::ResolutionScope::SourceSet(source_set);
    let ctx = hir_ty::InvocationContext::external(&scope);
    let holder = Ty::reference(&db, "a.Holder", Vec::new());
    assert!(
        !hir_ty::member_set(&db, &scope, &holder, "m", &ctx).is_empty(),
        "the shadowing annotation renames nothing"
    );
    assert!(
        hir_ty::member_set(&db, &scope, &holder, "x", &ctx).is_empty(),
        "the shadowing annotation is not the library's, so `x` names no member"
    );
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        !diagnostics.contains("method run"),
        "the Java body reaches the Kotlin names: {diagnostics}"
    );
}

/// A Kotlin file that writes no `@file:JvmName` still has a facade class — its
/// stem with `Kt` appended — and a Java caller reaches the file's top-level
/// declarations through it
/// (<https://kotlinlang.org/docs/java-interop.html#package-level-functions>).
///
/// kotlinc 2.4.20 compiles `Sample.kt` to `SampleKt`, and `javac` compiles the
/// Java file against it.
#[test]
fn a_java_caller_reaches_a_default_facade() {
    let files = [
        (
            "/src/main/kotlin/a/Sample.kt",
            "package a\n\nfun doubled(seed: Int): Int = seed * 2\n\nconst val LIMIT: Int = 3\n",
        ),
        (
            "/src/main/java/a/UseSample.java",
            "package a;\n\npublic class UseSample {\n    int run() {\n        return SampleKt.doubled(SampleKt.LIMIT);\n    }\n}\n",
        ),
    ];
    let (db, _) = interop_fixture(&files);
    assert_eq!(
        hir::file_facade_class(&db, FileId::from_raw(1)).map(|facade| facade.to_string()),
        Some("SampleKt".to_owned()),
        "the compiler names a facade after the file"
    );
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        !diagnostics.contains("method run"),
        "the Java body reaches the file's top-level declarations: {diagnostics}"
    );
}
