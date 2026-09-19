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

#[test]
fn executable_kotlin_java_interop_signatures() {
    let kotlin = r#"package proof
fun answer() = 42
class Probe {
    fun choose(x: Any?) = x ?: "fallback"
    fun require(x: String?) = x ?: throw IllegalArgumentException()
    fun `when`() = 1
}
class Words(vararg val values: String)
class Numbers(vararg val values: Int)
"#;
    let java = r#"package proof;
public class Smoke {
    public static void main(String[] args) {
        Probe p = new Probe();
        Object chosen = p.choose(Integer.valueOf(7));
        String required = p.require("ok");
        String[] words = new Words("a", "b").getValues();
        int[] numbers = new Numbers(3, 4).getValues();
        System.out.println(AppKt.answer() + ":" + chosen + ":" + required
            + ":" + p.when() + ":" + words.length + ":" + numbers[1]);
    }
}
"#;
    let (db, _) = interop_fixture(&[
        ("/src/main/kotlin/proof/app.kt", kotlin),
        ("/src/main/java/proof/Smoke.java", java),
        // Signature-only supplements to the minimal JDK fixture; the two
        // interoperability sources above are identical to the executable oracle.
        (
            "/jdk/java/lang/Integer.java",
            "package java.lang; public final class Integer extends Number { public static native Integer valueOf(int value); public native int intValue(); public native long longValue(); public native float floatValue(); public native double doubleValue(); }",
        ),
        (
            "/jdk/java/lang/System.java",
            "package java.lang; public final class System { public static final java.io.PrintStream out = null; }",
        ),
        (
            "/jdk/java/io/PrintStream.java",
            "package java.io; public class PrintStream { public native void println(String value); }",
        ),
    ]);
    let file = FileId::from_raw(2);
    assert!(hir_ty::class_diagnostics(&db, file).is_empty());
    let tree = hir_def::java::plugin::tree(&db, file);
    let bodies = hir::file_body_tree(&db, file);
    let (item, _) = common::all_items(&tree).into_iter().find(|(_, data)| {
        matches!(data, hir_def::java::item_tree::ItemData::Method(method) if method.name.as_str() == "main")
    }).unwrap();
    let types = hir_ty::body_types(&db, file, item).unwrap();
    assert!(types.diagnostics.is_empty(), "{:?}", types.diagnostics);
    let string = Ty::reference(&db, "java.lang.String", vec![]);
    let int = Ty::primitive(&db, syntax::stub::PrimitiveType::Int);
    for (call, expected) in [
        ("AppKt.answer()", int),
        (
            "p.choose(Integer.valueOf(7))",
            Ty::reference(&db, "java.lang.Object", vec![]),
        ),
        ("p.require(\"ok\")", string),
        ("p.when()", int),
        (
            "new Words(\"a\", \"b\").getValues()",
            Ty::array(&db, string),
        ),
        ("new Numbers(3, 4).getValues()", Ty::array(&db, int)),
    ] {
        let actual = types
            .exprs
            .iter()
            .find_map(|(expr, ty)| {
                let range = bodies.expr_range(*expr)?;
                (&java[range] == call).then_some(*ty)
            })
            .expect(call);
        assert_eq!(actual, expected, "{call}");
    }
}

#[test]
fn java_reads_vararg_property_arrays() {
    let kotlin = r#"package p
class Words(vararg var values: String)
class Numbers(vararg val values: Int)
class Boxed(vararg val values: Int?)
class Bounded<T : Number>(vararg val values: T)
class Exposed(@JvmField vararg val values: String)
"#;
    let java = r#"package p; class Use {
void good(Words w, Numbers n, Boxed b, Bounded raw, Exposed e) {
    String[] words = w.getValues(); int[] numbers = n.getValues();
    Integer[] boxed = b.getValues(); Number[] bounded = raw.getValues();
    String[] exposed = e.values; w.setValues(words);
    Words fresh = new Words("a", "b"); Numbers ints = new Numbers(1, 2);
}
void bad(Words w, Numbers n) { String word = w.getValues(); int number = n.getValues(); }
}"#;
    let (db, _) = interop_fixture(&[
        ("/src/main/kotlin/p/Arrays.kt", kotlin),
        ("/src/main/java/p/Use.java", java),
    ]);
    let file = FileId::from_raw(2);
    let tree = hir_def::java::plugin::tree(&db, file);
    let bodies = hir::file_body_tree(&db, file);
    for (item, data) in common::all_items(&tree) {
        let hir_def::java::item_tree::ItemData::Method(method) = data else {
            continue;
        };
        let types = hir_ty::body_types(&db, file, item).unwrap();
        if method.name.as_str() == "bad" {
            assert_eq!(types.diagnostics.len(), 2, "{:?}", types.diagnostics);
            for diagnostic in &types.diagnostics {
                assert!(matches!(
                    diagnostic,
                    hir_ty::TypeError::IncompatibleTypes { .. }
                ));
                assert!(
                    ["w.getValues()", "n.getValues()"]
                        .contains(&&java[diagnostic.range(&bodies).unwrap()])
                );
            }
        } else {
            assert!(types.diagnostics.is_empty(), "{:?}", types.diagnostics);
        }
    }
}

#[test]
fn java_resolves_canonical_kotlin_names() {
    let kotlin = "package `p`\nclass `Box` { fun `when`(): Int = 1; fun ordinary(): Int = 2; fun use() = ordinary() + `ordinary`() }\nfun answer() = 42";
    let java = "package p; class Use { int run(Box b) { return b.when() + b.ordinary() + AppKt.answer(); } }";
    let (db, source_set) = interop_fixture(&[
        ("/src/main/kotlin/p/app.kt", kotlin),
        ("/src/main/java/p/Use.java", java),
    ]);
    let scope = hir::ResolutionScope::SourceSet(source_set);
    assert!(matches!(
        hir::fqn_resolve(&db, &scope, "p.Box"),
        Some(hir::Resolved::Source(_))
    ));
    assert!(matches!(
        hir::fqn_resolve(&db, &scope, "p.AppKt"),
        Some(hir::Resolved::Facade { .. })
    ));
    assert!(hir::fqn_resolve(&db, &scope, "p.appKt").is_none());
    let file = FileId::from_raw(2);
    let tree = hir_def::java::plugin::tree(&db, file);
    for (item, _) in common::all_items(&tree) {
        if let Some(types) = hir_ty::body_types(&db, file, item) {
            assert!(types.diagnostics.is_empty(), "{:?}", types.diagnostics);
        }
    }
    let file = FileId::from_raw(1);
    let tree = hir_def::kotlin::plugin::tree(&db, file).unwrap();
    let item = tree
        .items
        .iter()
        .find(|(_, data)| data.name().is_some_and(|n| n.as_str() == "use"))
        .unwrap()
        .0;
    let types = hir_ty::kotlin_body_types(&db, file, hir_expand::ids::ItemId(item));
    assert!(types.diagnostics.is_empty(), "{:?}", types.diagnostics);
    assert_eq!(
        hir_ty::kotlin_item_ty(&db, file, hir_expand::ids::ItemId(item)),
        Ty::reference(&db, "kotlin.Int", vec![])
    );
    let ordinary = tree
        .items
        .iter()
        .find(|(_, data)| data.name().is_some_and(|n| n.as_str() == "ordinary"))
        .unwrap()
        .0;
    let bodies = hir::file_body_tree(&db, file);
    for spelling in ["ordinary()", "`ordinary`()"] {
        let target = types
            .resolved
            .iter()
            .find_map(|(expr, target)| {
                let range = bodies.expr_range(*expr)?;
                (&kotlin[range] == spelling).then_some(target)
            })
            .expect(spelling);
        assert_eq!(
            target,
            &hir_ty::KotlinResolvedMember::Kotlin {
                file,
                item: hir_expand::ids::ItemId(ordinary)
            }
        );
    }
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

/// A Java *declaration* type naming a Kotlin class is well-formed: the Java
/// declaration checks resolve `Box<String>` to the Kotlin class beside them and
/// then ask the Java layer a question about it — the number of type parameters
/// it declares ([JLS §4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5)).
/// That class is another language's declaration: it has no Java item, and its
/// item id indexes the Kotlin model's arena, so the Java item tree must never
/// be indexed with it.
///
/// kotlinc 2.4.20 compiles `Box.kt` clean and `javac` compiles the Java half
/// against the Kotlin output, so a Java field of a Kotlin type is a
/// well-formed declaration.
#[test]
fn a_java_declaration_type_naming_a_kotlin_class_is_well_formed() {
    let files = [
        (
            "/src/main/kotlin/a/Box.kt",
            "package a\n\nclass Box<T>(val value: T)\n",
        ),
        (
            "/src/main/java/a/Holder.java",
            "package a;\n\npublic class Holder {\n    Box<String> box;\n}\n",
        ),
    ];
    let (db, _) = interop_fixture(&files);
    let java_file = FileId::from_raw(2);
    let diagnostics = hir_ty::class_diagnostics(&db, java_file);
    assert!(
        diagnostics.is_empty(),
        "a Java field of a Kotlin type is well-formed: {diagnostics:?}"
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
    let scope = hir::ResolutionScope::SourceSet(source_set.clone());
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
    let (db, source_set) = interop_fixture(&files);
    assert_eq!(
        hir::file_facade_class(&db, FileId::from_raw(1)).map(|facade| facade.to_string()),
        Some("SampleKt".to_owned()),
        "the compiler names a facade after the file"
    );
    // The facade is a *source-set class of the file's package*: `a.SampleKt` is
    // what a Java caller names, and the file index files the file under the
    // package it declares.
    let scope = hir::ResolutionScope::SourceSet(source_set);
    assert!(
        matches!(
            hir::fqn_resolve(&db, &scope, "a.SampleKt"),
            Some(hir::Resolved::Facade { .. })
        ),
        "the facade resolves by its fully qualified name"
    );
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        !diagnostics.contains("method run"),
        "the Java body reaches the file's top-level declarations: {diagnostics}"
    );
}

/// The JVM methods `name` names on the Kotlin classifier `fqn`, as the Java
/// layer's member set answers them: the `is_static` flag of each candidate.
fn jvm_method_statics(
    db: &TestDatabase,
    source_set: &hir::SourceSetId,
    fqn: &str,
    name: &str,
) -> Vec<bool> {
    let scope = hir::ResolutionScope::SourceSet(source_set.clone());
    let ctx = hir_ty::InvocationContext::external(&scope);
    let ty = Ty::reference(db, fqn, Vec::new());
    hir_ty::member_set(db, &scope, &ty, name, &ctx)
        .iter()
        .map(|method| method.is_static)
        .collect()
}

/// The JVM methods `name` names on the Kotlin classifier `fqn`, as their
/// parameter type lists — the classfile's own list, read regardless of the
/// caller's access (a `private` constructor is carried all the same).
fn jvm_method_params(
    db: &TestDatabase,
    source_set: &hir::SourceSetId,
    fqn: &str,
    name: &str,
) -> Vec<Vec<String>> {
    let scope = hir::ResolutionScope::SourceSet(source_set.clone());
    let ctx = hir_ty::InvocationContext::external(&scope);
    let ty = Ty::reference(db, fqn, Vec::new());
    hir_ty::member_set_ignoring_access(db, &scope, &ty, name, &ctx)
        .iter()
        .map(|method| {
            method
                .params
                .iter()
                .map(|param| param.display(db).to_string())
                .collect()
        })
        .collect()
}

/// The JVM field `name` of the Kotlin classifier `fqn`, as
/// `(is_static, is_final)` — the shape a Java caller reads.
fn jvm_field_shape(
    db: &TestDatabase,
    source_set: &hir::SourceSetId,
    fqn: &str,
    name: &str,
) -> Option<(bool, bool)> {
    let scope = hir::ResolutionScope::SourceSet(source_set.clone());
    let ctx = hir_ty::InvocationContext::external(&scope);
    let ty = Ty::reference(db, fqn, Vec::new());
    hir_ty::pick_field(db, &scope, &ty, name, &ctx).map(|field| (field.is_static, field.is_final))
}

/// An `object` is reached through its `INSTANCE` and nothing else: its own
/// members are *instance* members of the object's class, and a `@JvmStatic`
/// member — and only that — is also static.
///
/// kotlinc 2.4.20 compiles the fixture clean; `javap -p` reports
/// `INSTANCE`, the instance `getX`/`getY`/`n`, the statics `getSx`/`sn` and the
/// static fields `fx`/`fy`/`CX` on `Obj`, `CX`'s (`const val`) field being
/// `public static final`.
#[test]
fn an_objects_members_are_instance_members() {
    const OBJ_KT: &str = r#"
package a

object Obj {
    val x: Int = 1
    var y: Int = 2
    fun n(): Int = 5

    @JvmStatic
    val sx: Int = 3

    @JvmStatic
    fun sn(): Int = 4

    @JvmField
    val fx: Int = 6

    @JvmField
    var fy: Int = 7

    const val CX: Int = 8
}
"#;
    let files = [
        ("/src/main/kotlin/a/Obj.kt", OBJ_KT),
        (
            "/src/main/java/a/UseObj.java",
            "package a;\n\npublic class UseObj {\n    int run() {\n        Obj obj = Obj.INSTANCE;\n        return obj.getX() + obj.getY() + obj.n() + Obj.getSx() + Obj.sn() + Obj.fx + Obj.fy + Obj.CX;\n    }\n}\n",
        ),
    ];
    let (db, source_set) = interop_fixture(&files);
    assert_eq!(
        jvm_method_statics(&db, &source_set, "a.Obj", "getX"),
        vec![false],
        "`val x` of an object is an instance accessor"
    );
    assert_eq!(
        jvm_method_statics(&db, &source_set, "a.Obj", "n"),
        vec![false]
    );
    assert_eq!(
        jvm_method_statics(&db, &source_set, "a.Obj", "getSx"),
        vec![true],
        "`@JvmStatic val sx` adds the static accessor"
    );
    assert_eq!(
        jvm_method_statics(&db, &source_set, "a.Obj", "sn"),
        vec![true]
    );
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Obj", "x"),
        None,
        "`val x` is an accessor, not a field"
    );
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Obj", "fx"),
        Some((true, true)),
        "an `@JvmField val` of an object is a static final field"
    );
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Obj", "fy"),
        Some((true, false)),
        "an `@JvmField var` of an object is a static non-final field"
    );
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Obj", "CX"),
        Some((true, true)),
        "a `const val` is a static final field"
    );
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        !diagnostics.contains("method run"),
        "the Java body reaches the object's members: {diagnostics}"
    );
}

/// A `companion object`'s `const val` and `@JvmField` properties are static
/// fields of the *enclosing* class — the same placement a `@JvmStatic` member's
/// method gets — while a plain companion property keeps its accessor on the
/// companion itself.
///
/// kotlinc 2.4.20 compiles the fixture clean; `javap -p` reports `cf`, `CC` and
/// `cv` on `Holder` (the first two `final`) and `getCs`/`csn` as its statics.
#[test]
fn a_companions_fields_are_the_enclosing_classs_statics() {
    const HOLDER_KT: &str = r#"
package a

class Holder {
    companion object {
        const val CC: Int = 1

        @JvmField
        val cf: Int = 2

        @JvmField
        var cv: Int = 3

        val plain: Int = 4

        @JvmStatic
        val cs: Int = 5

        @JvmStatic
        fun csn(): Int = 6
    }
}
"#;
    let files = [
        ("/src/main/kotlin/a/Holder.kt", HOLDER_KT),
        (
            "/src/main/java/a/UseHolder.java",
            "package a;\n\npublic class UseHolder {\n    int run() {\n        Holder.Companion companion = Holder.Companion;\n        return Holder.CC + Holder.cf + Holder.cv + Holder.getCs() + Holder.csn() + companion.getPlain();\n    }\n}\n",
        ),
    ];
    let (db, source_set) = interop_fixture(&files);
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Holder", "CC"),
        Some((true, true))
    );
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Holder", "cf"),
        Some((true, true))
    );
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Holder", "cv"),
        Some((true, false))
    );
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Holder", "plain"),
        None,
        "a plain companion property is not a field of the enclosing class"
    );
    assert_eq!(
        jvm_method_statics(&db, &source_set, "a.Holder", "getCs"),
        vec![true]
    );
    assert_eq!(
        jvm_method_statics(&db, &source_set, "a.Holder", "csn"),
        vec![true]
    );
    // The companion's own class keeps *instance* members — the enclosing class
    // is where `@JvmStatic` puts the statics — and carries no fields at all,
    // which is what `Holder.Companion.getPlain()` (an instance call on the
    // `Companion` field) reads.
    assert_eq!(
        jvm_method_statics(&db, &source_set, "a.Holder.Companion", "getPlain"),
        vec![false]
    );
    assert_eq!(
        jvm_method_statics(&db, &source_set, "a.Holder.Companion", "csn"),
        vec![false]
    );
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Holder.Companion", "CC"),
        None
    );
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        !diagnostics.contains("method run"),
        "the Java body reaches the companion's members: {diagnostics}"
    );
}

/// A nested `object`'s `INSTANCE` is the *object's* field, not its enclosing
/// class's: `class Outer { object Nested }` compiles to `Outer$Nested` with the
/// static field `INSTANCE`, beside an `Outer` that carries none — so a Java
/// caller writes `Outer.Nested.INSTANCE`.
///
/// kotlinc 2.4.20 compiles the fixture clean; `javap -p` reports
/// `public static final a.Outer$Nested INSTANCE` on `Outer$Nested` only.
#[test]
fn a_nested_objects_instance_is_its_own_field() {
    let files = [
        (
            "/src/main/kotlin/a/Outer.kt",
            "package a\n\nclass Outer {\n    object Nested {\n        val x: Int = 1\n    }\n}\n",
        ),
        (
            "/src/main/java/a/UseOuter.java",
            "package a;\n\npublic class UseOuter {\n    int run() {\n        Outer.Nested nested = Outer.Nested.INSTANCE;\n        return nested.getX();\n    }\n}\n",
        ),
    ];
    let (db, source_set) = interop_fixture(&files);
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Outer", "INSTANCE"),
        None,
        "the enclosing class carries no INSTANCE"
    );
    assert_eq!(
        jvm_field_shape(&db, &source_set, "a.Outer.Nested", "INSTANCE"),
        Some((true, true)),
        "the object's own class carries INSTANCE"
    );
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        !diagnostics.contains("method run"),
        "the Java body reaches the nested object: {diagnostics}"
    );
}

/// `@JvmOverloads` generates one JVM method per parameter that declares a
/// default value — not one per *trailing* default: each generated list holds
/// the parameters before that parameter plus the parameters after it that
/// declare none, so `f(a: String = "x", b: Int, c: Long = 1L)` gains
/// `f(String, int)` *and* `f(int)`, whose single parameter is `b`.
///
/// The same rule applies to a constructor. kotlinc 2.4.20 compiles the fixture
/// clean, and `javap -p` reports exactly these parameter lists.
#[test]
fn jvm_overloads_generates_a_method_per_default() {
    const OVERLOADS_KT: &str = r#"
package a

@JvmOverloads
fun f(a: String = "x", b: Int, c: Long = 1L): String = a

@JvmOverloads
fun g(a: Int, b: Int = 1): Int = a + b

class G @JvmOverloads constructor(val a: String = "x", val b: Int, val c: Long = 1L)
"#;
    let files = [
        ("/src/main/kotlin/a/Overloads.kt", OVERLOADS_KT),
        (
            "/src/main/java/a/UseOverloads.java",
            "package a;\n\npublic class UseOverloads {\n    String run() {\n        G whole = new G(\"y\", 1, 2L);\n        G dropped = new G(\"y\", 1);\n        G shortest = new G(1);\n        return OverloadsKt.f(\"y\", 1, 2L) + OverloadsKt.f(\"y\", 1) + OverloadsKt.f(1) + OverloadsKt.g(1, 2) + OverloadsKt.g(1) + whole.getA() + dropped.getB() + shortest.getC();\n    }\n}\n",
        ),
    ];
    let (db, source_set) = interop_fixture(&files);
    assert_eq!(
        jvm_method_params(&db, &source_set, "a.OverloadsKt", "f"),
        vec![
            vec![
                "java.lang.String".to_owned(),
                "int".to_owned(),
                "long".to_owned()
            ],
            vec!["java.lang.String".to_owned(), "int".to_owned()],
            vec!["int".to_owned()],
        ],
        "each default yields one method, in declaration order"
    );
    assert_eq!(
        jvm_method_params(&db, &source_set, "a.OverloadsKt", "g"),
        vec![
            vec!["int".to_owned(), "int".to_owned()],
            vec!["int".to_owned()]
        ]
    );
    assert_eq!(
        jvm_method_params(&db, &source_set, "a.G", "G"),
        vec![
            vec![
                "java.lang.String".to_owned(),
                "int".to_owned(),
                "long".to_owned()
            ],
            vec!["java.lang.String".to_owned(), "int".to_owned()],
            vec!["int".to_owned()],
        ],
        "@JvmOverloads applies to a constructor exactly as to a function"
    );
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        !diagnostics.contains("method run"),
        "the Java body reaches every generated overload: {diagnostics}"
    );
}

/// A primary constructor whose parameters *all* declare defaults also gets the
/// compiler's parameterless `<init>`, at that constructor's own access — "on the
/// JVM, if all primary constructor parameters have default values, the compiler
/// implicitly provides a parameterless constructor that uses those default
/// values"
/// (<https://kotlinlang.org/docs/classes.html#constructors>) — while a
/// constructor with only *some* defaults gains nothing, and a `private` primary
/// constructor gains nothing either (the compiler's own refinement of the
/// documented rule).
///
/// kotlinc 2.4.20 compiles the fixture clean; `javap -p` reports
/// `AllDefault(int, int)` + `AllDefault()`, `Partial(int, int)` alone and
/// `Hidden(int)` alone.
#[test]
fn an_all_defaults_constructor_gains_a_parameterless_one() {
    const DEFAULTS_KT: &str = r#"
package a

class AllDefault(val a: Int = 0, val b: Int = 0)

class Partial(val a: Int = 0, val b: Int)

class Hidden private constructor(val a: Int = 0)

class Overloaded @JvmOverloads constructor(val a: Int = 0)
"#;
    let files = [
        ("/src/main/kotlin/a/Defaults.kt", DEFAULTS_KT),
        (
            "/src/main/java/a/UseDefaults.java",
            "package a;\n\npublic class UseDefaults {\n    int run() {\n        AllDefault implicit = new AllDefault();\n        AllDefault whole = new AllDefault(1, 2);\n        Partial partial = new Partial(1, 2);\n        Overloaded overloaded = new Overloaded();\n        return implicit.getA() + whole.getB() + partial.getA() + overloaded.getA();\n    }\n}\n",
        ),
    ];
    let (db, source_set) = interop_fixture(&files);
    assert_eq!(
        jvm_method_params(&db, &source_set, "a.AllDefault", "AllDefault"),
        vec![vec!["int".to_owned(), "int".to_owned()], vec![]],
        "an all-defaults primary constructor gains the parameterless one"
    );
    assert_eq!(
        jvm_method_params(&db, &source_set, "a.Partial", "Partial"),
        vec![vec!["int".to_owned(), "int".to_owned()]],
        "a partially defaulted constructor gains nothing"
    );
    assert_eq!(
        jvm_method_params(&db, &source_set, "a.Hidden", "Hidden"),
        vec![vec!["int".to_owned()]],
        "a private primary constructor gains nothing"
    );
    assert_eq!(
        jvm_method_params(&db, &source_set, "a.Overloaded", "Overloaded"),
        vec![vec!["int".to_owned()], vec![]],
        "@JvmOverloads's own parameterless list is not duplicated"
    );
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        !diagnostics.contains("method run"),
        "the Java body reaches the implicit constructor: {diagnostics}"
    );
}

/// The JVM methods `name` names on the Kotlin classifier `fqn`, as their
/// `throws` clauses — the classfile's `Exceptions` attribute.
fn jvm_method_throws(
    db: &TestDatabase,
    source_set: &hir::SourceSetId,
    fqn: &str,
    name: &str,
) -> Vec<Vec<String>> {
    let scope = hir::ResolutionScope::SourceSet(source_set.clone());
    let ctx = hir_ty::InvocationContext::external(&scope);
    let ty = Ty::reference(db, fqn, Vec::new());
    hir_ty::member_set(db, &scope, &ty, name, &ctx)
        .iter()
        .map(|method| {
            method
                .throws
                .iter()
                .map(|thrown| thrown.display(db).to_string())
                .collect()
        })
        .collect()
}

/// `@Throws(IOException::class)` is what gives a Java caller a checked
/// exception to discharge: Kotlin's own exceptions are unchecked, and the
/// annotation is the only thing that writes the classfile's `Exceptions`
/// attribute
/// (<https://kotlinlang.org/docs/java-interop.html#checked-exceptions>).
///
/// kotlinc 2.4.20 compiles the fixture clean; `javap -p` reports the declared
/// `throws java.io.IOException` on the facade's `read`, and `javac` accepts a
/// Java body only where the liability is declared or caught.
#[test]
fn a_java_caller_must_discharge_a_thrown_exception() {
    const THROWS_KT: &str = r#"
package a

@Throws(java.io.IOException::class)
fun read(path: String): String = path

class Reader {
    @get:Throws(java.io.IOException::class)
    val name: String = "reader"
}
"#;
    const USE_THROWS_JAVA: &str = r#"
package a;

import java.io.IOException;

public class UseThrows {
    int handled() throws IOException {
        return ThrowsKt.read("x").length();
    }

    int unhandled() {
        return ThrowsKt.read("y").length();
    }

    int accessor() throws IOException {
        Reader reader = new Reader();
        return reader.getName().length();
    }
}
"#;
    let files = [
        ("/src/main/kotlin/a/Throws.kt", THROWS_KT),
        ("/src/main/java/a/UseThrows.java", USE_THROWS_JAVA),
    ];
    let (db, source_set) = interop_fixture(&files);
    assert_eq!(
        jvm_method_throws(&db, &source_set, "a.ThrowsKt", "read"),
        vec![vec!["java.io.IOException".to_owned()]],
        "the classfile's throws clause is the annotation's class literals"
    );
    assert_eq!(
        jvm_method_throws(&db, &source_set, "a.Reader", "getName"),
        vec![vec!["java.io.IOException".to_owned()]],
        "a getter declares the exceptions `@get:Throws` writes"
    );
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        diagnostics.contains("method unhandled"),
        "a Java body that neither catches nor declares the exception is reported: {diagnostics}"
    );
    assert!(
        !diagnostics.contains("method handled") && !diagnostics.contains("method accessor"),
        "a declared or caught liability is discharged: {diagnostics}"
    );
}

/// A Java caller of a Kotlin `@Deprecated` declaration reads it as deprecated:
/// kotlinc 2.4.20 writes the classfile's `Deprecated` attribute
/// ([JVMS §4.7.15](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.15))
/// *and* the runtime-visible annotation, at every level —
///
/// ```text
///   public final int f();
///     Deprecated: true
///         kotlin.Deprecated(
///           message="use g"
/// ```
///
/// (`javap -v -p` for `class Legacy { @Deprecated("use g") fun f(): Int = 1 }`)
/// — so javac reports a Java call of it ([JLS §9.6.4.6]) and this layer must
/// too, with the same sentence it renders for a deprecated Java member. The
/// exempt cases of the section apply unchanged: a use inside a declaration
/// that is itself deprecated is not reported.
#[test]
fn a_java_caller_reads_a_kotlin_declarations_deprecation() {
    const LEGACY_KT: &str = r#"
package a

class Legacy {
    @Deprecated("use g")
    fun f(): Int = 1

    fun g(): Int = 2
}

@Deprecated("use Fresh")
class Old {
    fun n(): Int = 3
}
"#;
    const USE_LEGACY_JAVA: &str = r#"
package a;

public class UseLegacy {
    int run() {
        return new Legacy().f();
    }

    int fine() {
        return new Legacy().g();
    }

    @Deprecated
    int exempt() {
        return new Legacy().f();
    }

    int older(Old old) {
        return old.n();
    }
}
"#;
    let files = [
        ("/src/main/kotlin/a/Legacy.kt", LEGACY_KT),
        ("/src/main/java/a/UseLegacy.java", USE_LEGACY_JAVA),
    ];
    let (db, _) = interop_fixture(&files);
    let diagnostics = render_body_diagnostic_spans(&db, &files);
    assert!(
        diagnostics.contains("deprecated-use: f() in Legacy has been deprecated"),
        "a Java call to a deprecated Kotlin member is reported: {diagnostics}"
    );
    assert!(
        !diagnostics.contains("method fine") && !diagnostics.contains("method exempt"),
        "a Kotlin member that declares no `@Deprecated` reports nothing, and a use inside a \
         deprecated declaration is exempt: {diagnostics}"
    );
    // The *class*'s own deprecation reaches the declaration-position reference
    // too — the parameter's type is a reference javac reports, which is a
    // declaration diagnostic rather than a body one.
    let decls = hir_ty::class_diagnostics(&db, FileId::from_raw(2));
    assert!(
        decls.iter().any(|diag| matches!(
            diag,
            hir_ty::DeclDiagnostic::DeprecatedUse {
                api: hir_ty::java::deprecation::DeprecatedApi::Class { name, .. },
                ..
            } if name.as_str() == "Old"
        )),
        "a Java declaration type naming a deprecated Kotlin class is reported: {decls:?}"
    );
}

#[test]
fn java_reads_elvis_inferred_return_types() {
    let kotlin = r#"package p
class Probe {
    fun choose(x: Any?) = x ?: "fallback"
    fun require(x: String?) = x ?: throw Exception()
    fun answer() = 42
    fun action() { }
    fun unitValue(): Unit? = null
}
"#;
    let java = r#"package p;
class Use {
    void run(Probe p, Object x) {
        Object chosen = p.choose(x);
        String required = p.require("ok");
        int answer = p.answer();
        p.action();
        kotlin.Unit unit = p.unitValue();
        String invalid = p.choose(x);
    }
}"#;
    let (db, source_set) = interop_fixture(&[
        ("/src/main/kotlin/p/Probe.kt", kotlin),
        ("/src/main/java/p/Use.java", java),
    ]);
    let scope = hir::ResolutionScope::SourceSet(source_set);
    let ctx = hir_ty::InvocationContext::external(&scope);
    let probe = Ty::reference(&db, "p.Probe", vec![]);
    let expected = [
        ("choose", Ty::reference(&db, "java.lang.Object", vec![])),
        ("require", Ty::reference(&db, "java.lang.String", vec![])),
        (
            "answer",
            Ty::primitive(&db, syntax::stub::PrimitiveType::Int),
        ),
        ("action", Ty::void(&db)),
        ("unitValue", Ty::reference(&db, "kotlin.Unit", vec![])),
    ];
    for (name, ret) in &expected {
        let methods = hir_ty::member_set(&db, &scope, &probe, name, &ctx);
        assert_eq!(methods.len(), 1, "{name}");
        assert_eq!(methods[0].ret, *ret, "{name}");
    }
    let file = FileId::from_raw(2);
    let tree = hir_def::java::plugin::tree(&db, file);
    let bodies = hir::file_body_tree(&db, file);
    let (item, _) = common::all_items(&tree).into_iter().find(|(_, data)| {
        matches!(data, hir_def::java::item_tree::ItemData::Method(m) if m.name.as_str() == "run")
    }).unwrap();
    let types = hir_ty::body_types(&db, file, item).unwrap();
    assert_eq!(types.diagnostics.len(), 1, "{:?}", types.diagnostics);
    let diagnostic = &types.diagnostics[0];
    assert!(matches!(
        diagnostic,
        hir_ty::TypeError::IncompatibleTypes { .. }
    ));
    let range = diagnostic.range(&bodies).unwrap();
    assert_eq!(
        usize::from(range.start()),
        java.rfind("p.choose(x)").unwrap()
    );
    assert_eq!(&java[range], "p.choose(x)");
    for (name, ret) in expected {
        let call = match name {
            "choose" => "p.choose(x)",
            "require" => "p.require(\"ok\")",
            "answer" => "p.answer()",
            "action" => "p.action()",
            _ => "p.unitValue()",
        };
        let actual = types
            .exprs
            .iter()
            .find_map(|(expr, ty)| {
                let range = bodies.expr_range(*expr)?;
                (&java[range] == call).then_some(*ty)
            })
            .expect(call);
        assert_eq!(actual, ret, "{call}");
    }
}

#[test]
fn java_respects_kotlin_accessor_visibility() {
    use salsa::Setter;

    let kotlin = r#"package p
open class Access {
    lateinit var ready: String
    var hidden: String = ""
        private set
    var custom: String = ""
        private set(value) { field = value }
    var guarded: String = ""
        protected set
    var local: String = ""
        internal set
}
object Obj { lateinit var ready: String }
class Holder { companion object { lateinit var ready: String } }
"#;
    let subclass = r#"package q;
import p.Access;
import p.Obj;
import p.Holder;
class Sub extends Access {
    void good(Access a) {
        a.ready = "ready";
        String ready = a.ready;
        a.setReady(ready);
        String fromGetter = a.getReady();
        String hidden = a.getHidden();
        String custom = a.getCustom();
        a.setLocal$m("module");
        String local = a.getLocal();
        Obj.ready = "object";
        String object = Obj.ready;
        Holder.ready = "companion";
        String companion = Holder.ready;
        this.setGuarded("protected");
    }
    void missing(Access a) { a.setHidden("x"); }
    void privateSetter(Access a) { a.setCustom("x"); }
    void protectedReceiver(Access a) { a.setGuarded("x"); }
    void unmangled(Access a) { a.setLocal("x"); }
    void companionOwner() { Holder.Companion.ready = "x"; }
}"#;
    let outside = r#"package q;
import p.Access;
class Outside {
    void protectedSetter(Access a) { a.setGuarded("x"); }
}"#;
    let (mut db, source_set) = interop_fixture(&[
        ("/src/main/kotlin/p/Access.kt", kotlin),
        ("/src/main/java/q/Sub.java", subclass),
        ("/src/main/java/q/Outside.java", outside),
    ]);
    // Configure only this compilation, leaving the shared fixture's defaults intact.
    let graph = hir::project_graph(&db).unwrap();
    let mut module_names = graph.module_names(&db).clone();
    module_names.insert(source_set, hir_expand::name::Name::new("m"));
    graph.set_module_names(&mut db).to(module_names);

    let string = Ty::reference(&db, "java.lang.String", vec![]);
    for (file, java) in [
        (FileId::from_raw(2), subclass),
        (FileId::from_raw(3), outside),
    ] {
        let declarations = hir_ty::class_diagnostics(&db, file);
        assert!(declarations.is_empty(), "{declarations:?}");
        let tree = hir_def::java::plugin::tree(&db, file);
        let bodies = hir::file_body_tree(&db, file);
        for (item, data) in common::all_items(&tree) {
            let hir_def::java::item_tree::ItemData::Method(method) = data else {
                continue;
            };
            let types = hir_ty::body_types(&db, file, item).unwrap();
            if method.name.as_str() == "good" {
                assert!(types.diagnostics.is_empty(), "{:?}", types.diagnostics);
                for (expression, owner, is_static) in [
                    ("a.ready", "p.Access", false),
                    ("Obj.ready", "p.Obj", true),
                    ("Holder.ready", "p.Holder", true),
                ] {
                    let selected = types
                        .resolved
                        .iter()
                        .find_map(|(expr, selected)| {
                            let range = bodies.expr_range(*expr)?;
                            (&java[range] == expression).then_some(selected)
                        })
                        .expect(expression);
                    let hir_ty::ResolvedMember::Field(field) = selected else {
                        panic!("{expression}: {selected:?}");
                    };
                    assert_eq!(
                        field.owner.as_ty(&db, vec![]),
                        Ty::reference(&db, owner, vec![])
                    );
                    assert_eq!(field.ty, string, "{expression}");
                    assert_eq!(field.is_static, is_static, "{expression}");
                    assert!(!field.is_final, "{expression}");
                }
                for (expression, name) in [
                    ("a.setReady(ready)", "setReady"),
                    ("a.getReady()", "getReady"),
                    ("a.getHidden()", "getHidden"),
                    ("a.getCustom()", "getCustom"),
                    ("a.setLocal$m(\"module\")", "setLocal$m"),
                    ("a.getLocal()", "getLocal"),
                    ("this.setGuarded(\"protected\")", "setGuarded"),
                ] {
                    let selected = types
                        .resolved
                        .iter()
                        .find_map(|(expr, selected)| {
                            let range = bodies.expr_range(*expr)?;
                            (&java[range] == expression).then_some(selected)
                        })
                        .expect(expression);
                    assert!(
                        matches!(selected, hir_ty::ResolvedMember::Method(method)
                        if method.name == name),
                        "{expression}: {selected:?}"
                    );
                }
                continue;
            }

            assert_eq!(
                types.diagnostics.len(),
                1,
                "{}: {:?}",
                method.name,
                types.diagnostics
            );
            let diagnostic = &types.diagnostics[0];
            let (expression, name) = match method.name.as_str() {
                "missing" | "unmangled" => {
                    let (expression, name) = if method.name.as_str() == "missing" {
                        ("a.setHidden(\"x\")", "setHidden")
                    } else {
                        ("a.setLocal(\"x\")", "setLocal")
                    };
                    assert!(
                        matches!(diagnostic, hir_ty::TypeError::NoSuchMethod { name: actual, .. }
                        if actual.as_str() == name),
                        "{diagnostic:?}"
                    );
                    assert!(
                        !types.resolved.iter().any(|(expr, _)| {
                            bodies
                                .expr_range(*expr)
                                .is_some_and(|range| &java[range] == expression)
                        }),
                        "an omitted method must not resolve: {expression}"
                    );
                    (expression, name)
                }
                "privateSetter" | "protectedReceiver" | "protectedSetter" => {
                    let (expression, name, access) = if method.name.as_str() == "privateSetter" {
                        ("a.setCustom(\"x\")", "setCustom", "private")
                    } else {
                        ("a.setGuarded(\"x\")", "setGuarded", "protected")
                    };
                    assert!(
                        matches!(diagnostic, hir_ty::TypeError::IllegalAccess {
                        kind: hir_ty::java::diagnostics::IllegalAccessKind::Method,
                        name: actual, access: actual_access, ..
                    } if actual.as_str() == name && *actual_access == access),
                        "{diagnostic:?}"
                    );
                    (expression, name)
                }
                "companionOwner" => {
                    assert!(
                        matches!(diagnostic, hir_ty::TypeError::NoSuchField { name, .. }
                        if name.as_str() == "ready"),
                        "{diagnostic:?}"
                    );
                    ("Holder.Companion.ready", "ready")
                }
                name => panic!("unexpected method {name}"),
            };
            let range = diagnostic.range(&bodies).unwrap();
            assert_eq!(&java[range], name);
            let start = java.find(expression).unwrap() + expression.find(name).unwrap();
            assert_eq!(usize::from(range.start()), start);
        }
    }
}

#[test]
fn java_cannot_override_final_kotlin_members() {
    let kotlin = r#"package p
open class Base { open fun f(): Int = 1 }
open class OpenMid : Base() { override fun f(): Int = 2 }
open class FinalMid : Base() { final override fun f(): Int = 2 }
"#;
    let open_child = r#"package q;
class OpenChild extends p.OpenMid {
    public int f() { return 3; }
}"#;
    let final_child = r#"package q;
class FinalChild extends p.FinalMid {
    public int f() { return 3; }
}"#;
    let (db, _) = interop_fixture(&[
        ("/src/main/kotlin/p/Overrides.kt", kotlin),
        ("/src/main/java/q/OpenChild.java", open_child),
        ("/src/main/java/q/FinalChild.java", final_child),
    ]);
    let open_file = FileId::from_raw(2);
    let declarations = hir_ty::class_diagnostics(&db, open_file);
    assert!(declarations.is_empty(), "{declarations:?}");

    let final_file = FileId::from_raw(3);
    let declarations = hir_ty::class_diagnostics(&db, final_file);
    assert_eq!(declarations.len(), 1, "{declarations:?}");
    assert!(
        matches!(&declarations[0], hir_ty::DeclDiagnostic::CannotOverrideFinalMethod {
        method, super_owner,
    } if method.as_str() == "f" && super_owner.as_str() == "p.FinalMid"),
        "{declarations:?}"
    );

    // Hierarchy findings are keyed by method name; the diagnostic consumer
    // resolves that key to the offending Java declaration's source range.
    let diagnostics = ide_diagnostics::declaration_diagnostics(&db, final_file);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].code,
        Some(syntax::DiagnosticCode::Java(
            syntax::JavaDiagnosticCode::CannotOverrideFinalMethod,
        ))
    );
    let range = diagnostics[0].range.range;
    assert_eq!(&final_child[range], "public int f() { return 3; }");
    assert_eq!(
        usize::from(range.start()),
        final_child.find("public int f()").unwrap()
    );
    assert!(ide_diagnostics::declaration_diagnostics(&db, open_file).is_empty());

    for file in [open_file, final_file] {
        let tree = hir_def::java::plugin::tree(&db, file);
        for (item, _) in common::all_items(&tree) {
            if let Some(types) = hir_ty::body_types(&db, file, item) {
                assert!(types.diagnostics.is_empty(), "{:?}", types.diagnostics);
            }
        }
    }
}
