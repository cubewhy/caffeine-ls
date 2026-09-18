//! The JVM view of a Kotlin declaration: the classfile shape `kotlinc` emits
//! for it, as a caller of another language reads it.
//!
//! Every assertion in this file is a line of `javap -p` output for the file's
//! fixture compiled with kotlinc 2.4.20 (JRE 25.0.4.1), quoted in the test's
//! doc comment; the fixture is compiled beside the test in a scratch directory
//! and the emitted `javap` line is the *only* source of the expected shape.
//! Where the compiler disagrees with KLS, the compiler wins and the deviation
//! is recorded on the rule in `hir_ty::kotlin::jvm_view`.
//!
//! The fixture's Kotlin standard library is hand-encoded
//! ([`kotlin_stdlib_classes`]), like the other `kotlin_*` suites, so the files
//! resolve `Int`, `String` and the `kotlin.jvm` annotations hermetically.

use base_db::{FileChange, SourceRoot, SourceRootId};
use hir::SourceSetId;
use hir::hir_def::kotlin::item_tree::KotlinItemData;
use hir_expand::ids::ItemId;
use hir_ty::kotlin::jvm_view::{
    file_facade_fields, file_facade_members, java_view_fields, java_view_members,
};
use hir_ty::kotlin::method::{CallSite, MemberKind};
use hir_ty::kotlin_declared_members;
use hir_ty::{
    Access, FieldData, InvocationContext, MethodData, Ty, all_methods_for_test, member_set,
};
use tempfile::TempDir;
use triomphe::Arc;
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

mod common;
use common::{TestDatabase, fixture_library, interop_classes, jdk_fixture, kotlin_stdlib_classes};

/// A database with the JDK fixture, the hand-encoded Kotlin stdlib and one
/// Kotlin source root whose classpath carries both — the shape
/// `kotlin_item_types.rs`'s `kotlin_fixture` builds.
fn fixture(files: &[(&str, &str)]) -> (TestDatabase, FileId) {
    let dir = TempDir::new().unwrap();
    let jdk = jdk_fixture();
    let (stdlib_id, stdlib_path) =
        fixture_library(&dir, "kotlin-stdlib.jar", &kotlin_stdlib_classes());
    let (interop_id, interop_path) = fixture_library(&dir, "java-interop.jar", &interop_classes());

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
        interop_id,
        hir::LibraryInfo::new(hir::LibraryKind::Jar, interop_path),
    );
    data.jdk_libraries.push(jdk.lib);
    data.source_sets.insert(
        source_set.clone(),
        Arc::new(hir::Classpath {
            entries: vec![
                hir::ClasspathEntry::Library(jdk.lib),
                hir::ClasspathEntry::Library(stdlib_id),
                hir::ClasspathEntry::Library(interop_id),
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

/// The scope a *Java* caller of these declarations reads them from: Java and
/// Kotlin of one module share a source set, and the Java layer's *external*
/// invocation context is the one `jls_interop.rs` gives a Java body.
fn java_scope(db: &TestDatabase, file: FileId) -> hir::ResolutionScope {
    hir::ResolutionScope::SourceSet(hir::source_set_for_file(db, file).expect("a source set"))
}

/// The item of the classifier `name` in `file`.
fn class_item(db: &TestDatabase, file: FileId, name: &str) -> ItemId {
    let tree = hir::hir_def::kotlin::plugin::tree(db, file).expect("a Kotlin file");
    for (id, data) in tree.items.iter() {
        if let KotlinItemData::Class(class) = data
            && class.name.as_str() == name
        {
            return ItemId(id);
        }
    }
    panic!("no classifier {name}");
}

/// The JVM methods of the classifier `class` in `file` whose JVM name is one of
/// `names`, each rendered the way `javap -p` prints its declaration line:
/// access, `static`, `final`, `abstract`, the return type, the name and the
/// erased parameter list.
fn methods(db: &TestDatabase, file: FileId, class: &str, names: &[&str]) -> Vec<String> {
    let source = hir::SourceClass {
        file,
        item: class_item(db, file, class),
    };
    let mut out = Vec::new();
    for name in names {
        out.extend(
            java_view_members(db, source, name)
                .iter()
                .map(|m| method_line(db, m)),
        );
    }
    out
}

fn method_line(db: &TestDatabase, method: &MethodData) -> String {
    let mut line = access(method.access).to_owned();
    if method.is_static {
        line.push_str(" static");
    }
    // `javap -p` prints `default` for an interface member that declares a body
    // — the modifier the compiler emits, which no source keyword spells.
    if method.declaring_interface && !method.abstract_ && !method.is_static {
        line.push_str(" default");
    }
    if method.is_final {
        line.push_str(" final");
    }
    if method.abstract_ {
        line.push_str(" abstract");
    }
    line.push(' ');
    line.push_str(&method.ret.display(db).to_string());
    line.push(' ');
    line.push_str(&method.name);
    line.push('(');
    for (i, param) in method.params.iter().enumerate() {
        if i > 0 {
            line.push_str(", ");
        }
        line.push_str(&param.display(db).to_string());
    }
    line.push(')');
    line
}

fn access(access: Access) -> &'static str {
    match access {
        Access::Public => "public",
        Access::Protected => "protected",
        Access::Package => "",
        Access::Private => "private",
    }
}

/// The fixture every test below asserts against — one declaration per
/// behaviour. It is compiled by the test's own oracle, kotlinc 2.4.20 (JRE
/// 25.0.4.1), and the `javap -p` output quoted in each test's doc comment is
/// that compilation's.
const SHAPES_KT: &str = r#"
package m6

interface Iface {
    fun withBody(): Int = 1
    fun noBody(): Int
    val prop: Int
    val propWithBody: Int get() = 2
}

interface Plain { fun f(): Int = 1 }

annotation class Ann(val x: Int, val y: String)

enum class Colour { RED, GREEN; fun f(): Int = 1 }

object Single { fun f(): Int = 1 }

class Normal { fun f(): Int = 1 }

fun String.twice(): String = this + this

class Outer { fun String.memberExt(): Int = length }

abstract class Base {
    abstract fun must(): Int
    open fun maybe(): Int = 1
    fun sealed_(): Int = 2
    abstract val ap: Int
}

open class Base2 { open fun o(): Int = 1 }
class Derived : Base2() { override fun o(): Int = 3 }

open class Open { fun f(): Int = 1 }

class AllDefaults(val a: Int = 0)
class Empty

class Props {
    var v: Int = 0
    val r: Int = 1
    var isOn: Boolean = false
}

const val NAME = "renamed"
@JvmName(NAME) fun topFn(): Int = 1
@get:JvmName("getRenamed") val topVal: Int = 2
@set:JvmName("writeTopVar") var topVar: Int = 3
const val CONST = 4
"#;

fn shapes() -> (TestDatabase, FileId) {
    fixture(&[("/src/main/kotlin/m6/Shapes.kt", SHAPES_KT)])
}

/// The Kotlin standard library the fixture resolves against carries the
/// `kotlin.jvm` annotations, so `@JvmName` is the standard library's own.
///
/// kotlinc 2.4.20, `javap -p m6.Iface`:
///
/// ```text
/// public interface m6.Iface {
///   public default int withBody();
///   public abstract int noBody();
///   public abstract int getProp();
///   public default int getPropWithBody();
/// }
/// ```
///
/// The `default` and the `abstract` pair is the whole rule: a member of an
/// interface carries `ACC_ABSTRACT` exactly when it declares no body, and a
/// member that declares one is the classfile's `default` method — neither
/// abstract nor final, because a member of an interface is `open`
/// (<https://kotlinlang.org/docs/interfaces.html>).
#[test]
fn an_interface_member_is_abstract_only_without_a_body() {
    let (db, file) = shapes();
    assert_eq!(
        methods(
            &db,
            file,
            "Iface",
            &["withBody", "noBody", "getProp", "getPropWithBody"]
        ),
        vec![
            // `public default int withBody();`
            "public default int withBody()",
            // `public abstract int noBody();`
            "public abstract int noBody()",
            // `public abstract int getProp();` — a `val` member with no
            // initializer and no accessor is an abstract getter.
            "public abstract int getProp()",
            // `public default int getPropWithBody();` — the declared accessor
            // is the body the classfile carries.
            "public default int getPropWithBody()",
        ]
    );
    // A Java caller reads the same flags: the declaration-level enumeration a
    // Java check uses ([JLS §9.8]'s functional-interface walk) answers for a
    // Kotlin interface, and reports the body-less member abstract.
    let scope = java_scope(&db, file);
    let iface = Ty::reference(&db, "m6.Iface", Vec::new());
    let ctx = InvocationContext::external(&scope);
    let named = |name: &str| {
        member_set(&db, &scope, &iface, name, &ctx)
            .into_iter()
            .map(|method| (method.abstract_, method.is_final))
            .collect::<Vec<_>>()
    };
    assert_eq!(named("noBody"), vec![(true, false)]);
    assert_eq!(named("withBody"), vec![(false, false)]);
}

/// Which modality makes a member `abstract` and which makes it `final`.
/// kotlinc 2.4.20, `javap -p`:
///
/// ```text
/// public abstract class m6.Base {
///   public abstract int must();
///   public int maybe();
///   public final int sealed_();
///   public abstract int getAp();
/// }
/// public class m6.Base2 { public int o(); }
/// public final class m6.Derived extends m6.Base2 { public int o(); }
/// public class m6.Open { public final int f(); }
/// ```
///
/// The written modality is what a declaration *departs* from: a member that
/// names none is `final` in a `class`, an `open class` and an `abstract class`
/// alike, `open` is neither flag, `abstract` is `ACC_ABSTRACT`, and an
/// `override` that names no modality is `open` by default
/// ([KLS `inheritance.html#overriding`](https://kotlinlang.org/spec/inheritance.html#overriding))
/// — even in a class the compiler finalizes (`Derived`).
#[test]
fn a_members_abstract_flag_and_finality_come_from_its_context() {
    let (db, file) = shapes();
    assert_eq!(
        methods(&db, file, "Base", &["must", "maybe", "sealed_", "getAp"]),
        vec![
            // `public abstract int must();`
            "public abstract int must()",
            // `public int maybe();` — `open` sets neither flag.
            "public int maybe()",
            // `public final int sealed_();`
            "public final int sealed_()",
            // `public abstract int getAp();`
            "public abstract int getAp()",
        ]
    );
    // The default modality is `final` whatever the class's own modality is.
    assert_eq!(methods(&db, file, "Base2", &["o"]), vec!["public int o()"]);
    assert_eq!(
        methods(&db, file, "Open", &["f"]),
        vec!["public final int f()"]
    );
    // An `override` that names no modality is `open`, even in a class the
    // compiler finalizes.
    assert_eq!(
        methods(&db, file, "Derived", &["o"]),
        vec!["public int o()"]
    );
}
