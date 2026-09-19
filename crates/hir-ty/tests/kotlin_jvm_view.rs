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
            java_view_members(db, source, &[], name)
                .iter()
                .map(|m| method_line(db, m)),
        );
    }
    out
}

/// [`methods`] for a file's facade class.
fn facade_methods(db: &TestDatabase, file: FileId, names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for name in names {
        out.extend(
            file_facade_members(db, file, name)
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

fn field_line(db: &TestDatabase, field: &FieldData) -> String {
    let mut line = access(field.access).to_owned();
    if field.is_static {
        line.push_str(" static");
    }
    if field.is_final {
        line.push_str(" final");
    }
    line.push(' ');
    line.push_str(&field.ty.display(db).to_string());
    line.push(' ');
    line.push_str(&field.name);
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

fn fields(db: &TestDatabase, file: FileId, class: &str, names: &[&str]) -> Vec<String> {
    let source = hir::SourceClass {
        file,
        item: class_item(db, file, class),
    };
    let mut out = Vec::new();
    for name in names {
        out.extend(
            java_view_fields(db, source, &[], name)
                .iter()
                .map(|f| field_line(db, f)),
        );
    }
    out
}

fn facade_fields(db: &TestDatabase, file: FileId, names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for name in names {
        out.extend(
            file_facade_fields(db, file, name)
                .iter()
                .map(|f| field_line(db, f)),
        );
    }
    out
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

/// The JVM kind of a classifier is the *Kotlin* declaration's, and each kind's
/// members follow it. kotlinc 2.4.20, `javap -p`:
///
/// ```text
/// public interface m6.Plain { public default int f(); }
/// public interface m6.Ann extends java.lang.annotation.Annotation {
///   public abstract int x();
///   public abstract java.lang.String y();
/// }
/// public final class m6.Colour extends java.lang.Enum<m6.Colour> {
///   public static final m6.Colour RED;
///   public static final m6.Colour GREEN;
///   private m6.Colour();
///   public final int f();
/// }
/// public final class m6.Single {
///   public static final m6.Single INSTANCE;
///   private m6.Single();
///   public final int f();
/// }
/// public final class m6.Normal { public m6.Normal(); public final int f(); }
/// ```
///
/// An `annotation class` is an interface in the classfile whose *elements* are
/// named by the property alone (`x()`, not `getX()`); an enum's entries are
/// statics of the enum's own type; an object is a final class whose
/// `INSTANCE` field is the singleton.
#[test]
fn a_classifiers_kind_is_the_kotlin_declaration() {
    let (db, file) = shapes();
    // Every kind's member reports whether the classfile is an interface: true
    // for an `interface` and an `annotation class` (JVMS §4.1's ACC_INTERFACE),
    // false for an `enum class`, an `object` and a `class`.
    for (class, name, is_interface) in [
        ("Plain", "f", true),
        ("Ann", "x", true),
        ("Colour", "f", false),
        ("Single", "f", false),
        ("Normal", "f", false),
    ] {
        let source = hir::SourceClass {
            file,
            item: class_item(&db, file, class),
        };
        let found = java_view_members(&db, source, &[], name);
        assert_eq!(
            found.first().map(|method| method.declaring_interface),
            Some(is_interface),
            "{class}.{name} keeps the Kotlin kind's interface-ness"
        );
    }
    // An annotation class's property is an element named by the property.
    assert_eq!(
        methods(&db, file, "Ann", &["x", "y"]),
        vec![
            // `public abstract int x();`
            "public abstract int x()",
            // `public abstract java.lang.String y();`
            "public abstract java.lang.String y()",
        ]
    );
    assert!(
        methods(&db, file, "Ann", &["getX", "getY"]).is_empty(),
        "an annotation element is not a JavaBeans getter"
    );
    // An interface member with a body is the classfile's default method.
    assert_eq!(
        methods(&db, file, "Plain", &["f"]),
        vec!["public default int f()"]
    );
    // An enum's entries are statics of the enum's own type, and its members
    // are the final members of a final class.
    assert_eq!(
        fields(&db, file, "Colour", &["RED", "GREEN"]),
        vec![
            "public static final m6.Colour RED",
            "public static final m6.Colour GREEN",
        ]
    );
    assert_eq!(
        methods(&db, file, "Colour", &["f"]),
        vec!["public final int f()"]
    );
    // An object is a final class whose members stay *instance* members.
    assert_eq!(
        fields(&db, file, "Single", &["INSTANCE"]),
        vec!["public static final m6.Single INSTANCE"]
    );
    assert_eq!(
        methods(&db, file, "Single", &["f"]),
        vec!["public final int f()"]
    );
    assert_eq!(
        methods(&db, file, "Normal", &["f"]),
        vec!["public final int f()"]
    );
    // A *declaration-level* enumeration of a classifier ([`all_methods`]) —
    // what the JVM layer's declaration walks hand a Java check — names every
    // member the Kotlin declaration contributes, under the JVM names above.
    let scope = java_scope(&db, file);
    let ctx = InvocationContext::external(&scope);
    let iface = Ty::reference(&db, "m6.Iface", Vec::new());
    let all: Vec<String> = all_methods_for_test(&db, &scope, &iface, &ctx)
        .iter()
        .map(|method| method.name.clone())
        .collect();
    for expected in ["noBody", "withBody", "getProp", "getPropWithBody"] {
        assert!(
            all.contains(&expected.to_owned()),
            "a declaration-level enumeration sees {expected}: {all:?}"
        );
    }
    let colour = Ty::reference(&db, "m6.Colour", Vec::new());
    let enum_members: Vec<String> = all_methods_for_test(&db, &scope, &colour, &ctx)
        .iter()
        .map(|method| method.name.clone())
        .collect();
    assert!(
        enum_members.contains(&"f".to_owned()),
        "a declaration-level enumeration sees the enum's own member: {enum_members:?}"
    );
}

/// An extension function's *receiver* is its first parameter, and a top-level
/// one is a static of the file's facade. kotlinc 2.4.20, `javap -p`:
///
/// ```text
/// public final class m6.ShapesKt {
///   public static final java.lang.String twice(java.lang.String);
/// }
/// public final class m6.Outer {
///   public m6.Outer();
///   public final int memberExt(java.lang.String);
/// }
/// ```
///
/// (`https://kotlinlang.org/docs/java-to-kotlin-interop.html#extension-functions`.)
#[test]
fn an_extension_function_takes_its_receiver_first() {
    let (db, file) = shapes();
    assert_eq!(
        facade_methods(&db, file, &["twice"]),
        vec![
            // `public static final java.lang.String twice(java.lang.String);`
            "public static final java.lang.String twice(java.lang.String)",
        ]
    );
    assert_eq!(
        methods(&db, file, "Outer", &["memberExt"]),
        vec![
            // `public final int memberExt(java.lang.String);` — a *member*
            // extension is an instance method of its class, the receiver
            // first, exactly as the top-level one is a static.
            "public final int memberExt(java.lang.String)",
        ]
    );
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

/// An `object`, an `interface` and an `enum class` expose no *public*
/// constructor. kotlinc 2.4.20, `javap -p`:
///
/// ```text
/// public final class m6.Single { private m6.Single(); }
/// public final class m6.Colour extends java.lang.Enum<m6.Colour> {
///   private m6.Colour();
/// }
/// public interface m6.Plain { }
/// public interface m6.Ann extends java.lang.annotation.Annotation { }
/// public final class m6.Normal { public m6.Normal(); }
/// public final class m6.Empty { public m6.Empty(); }
/// public final class m6.AllDefaults {
///   public m6.AllDefaults(int);
///   public m6.AllDefaults();
/// }
/// ```
///
/// An `object`'s and an `enum class`'s constructor is `private` (the object is
/// reached through its `INSTANCE` field: "show the object's constructor as
/// private" and "enum class constructors are private",
/// <https://kotlinlang.org/docs/object-declarations.html#object-declarations>,
/// <https://kotlinlang.org/docs/enum-classes.html>), and an `interface` and an
/// `annotation class` — one in the classfile — have none at all. A `class`
/// keeps the implicit public `<init>()`, and a primary constructor whose
/// parameters all declare defaults gains a *public* parameterless one
/// (<https://kotlinlang.org/docs/classes.html#constructors>).
#[test]
fn an_object_interface_and_enum_have_no_public_constructor() {
    let (db, file) = shapes();
    assert_eq!(
        methods(&db, file, "Single", &["Single"]),
        vec!["private void <init>()"]
    );
    assert_eq!(
        methods(&db, file, "Colour", &["Colour"]),
        vec!["private void <init>()"]
    );
    // No constructor at all: neither name answers for an `interface` or an
    // `annotation class`.
    assert!(methods(&db, file, "Plain", &["Plain"]).is_empty());
    assert!(methods(&db, file, "Ann", &["Ann"]).is_empty());
    // A `class` keeps the implicit public one — as `javap -p`'s
    // `public m6.Normal();` and `public m6.Empty();` say.
    assert_eq!(
        methods(&db, file, "Normal", &["Normal"]),
        vec!["public void <init>()"]
    );
    assert_eq!(
        methods(&db, file, "Empty", &["Empty"]),
        vec!["public void <init>()"]
    );
    // `public m6.AllDefaults(int);` beside `public m6.AllDefaults();`.
    assert_eq!(
        methods(&db, file, "AllDefaults", &["AllDefaults"]),
        vec!["public void <init>(int)", "public void <init>()"]
    );
}

/// A Kotlin property's accessors, and a Java member seen as a Kotlin property.
/// kotlinc 2.4.20, `javap -p m6.Props`:
///
/// ```text
/// private int v;
/// private final int r;
/// private boolean isOn;
/// public final int getV();
/// public final void setV(int);
/// public final int getR();
/// public final boolean isOn();
/// public final void setOn(boolean);
/// ```
///
/// A `var v` is the `getV()`/`setV(int)` pair; a `val r` only `getR()`; and the
/// `is`-prefixed `var isOn` keeps the name in the *reader* and drops it in the
/// *writer* (`isOn()`/`setOn(boolean)`) — there is no `getIsOn` and no
/// `setIsOn`
/// (<https://kotlinlang.org/docs/java-interop.html#getters-and-setters>).
///
/// The other direction is the JavaBeans *synthetic property*: a Java
/// `getDragEnabled()`/`setDragEnabled(boolean)` pair is the Kotlin property
/// `dragEnabled`, whose read resolves to a getter member and whose write to a
/// setter member (`java.awt.Container.getLayout`'s precedent;
/// `javax.swing.JList` is the fixture's classfile).
#[test]
fn a_propertys_accessor_is_the_member_each_direction_names() {
    let (db, file) = shapes();
    // A read is the getter — no parameters, the property's type — and a write
    // the setter — one parameter, `void`.
    assert_eq!(
        methods(&db, file, "Props", &["getV", "setV"]),
        vec!["public final int getV()", "public final void setV(int)",]
    );
    // A `val` has no writer.
    assert_eq!(
        methods(&db, file, "Props", &["getR"]),
        vec!["public final int getR()"]
    );
    assert!(methods(&db, file, "Props", &["setR"]).is_empty());
    // The `is`-prefixed reader keeps the name the property wrote; the writer
    // drops the `is`.
    assert_eq!(
        methods(&db, file, "Props", &["isOn", "setOn"]),
        vec![
            "public final boolean isOn()",
            "public final void setOn(boolean)"
        ]
    );
    assert!(methods(&db, file, "Props", &["getIsOn", "setIsOn"]).is_empty());
    // The Kotlin side of the same inversion: a `val`'s candidate member is its
    // getter, a `var`'s the setter a write names.
    let scope = java_scope(&db, file);
    let props = Ty::reference(&db, "m6.Props", Vec::new());
    let site = CallSite {
        file,
        item: Some(class_item(&db, file, "Props")),
    };
    let kinds = |name: &str| {
        kotlin_declared_members(
            &db,
            &scope,
            &props,
            &hir_expand::name::Name::new(name),
            site,
        )
        .into_iter()
        .map(|member| (member.kind, member.params.len()))
        .collect::<Vec<_>>()
    };
    assert_eq!(kinds("r"), vec![(MemberKind::Getter, 0)]);
    assert_eq!(kinds("v"), vec![(MemberKind::Setter, 1)]);
    // A Java getter/setter pair is the property `dragEnabled`, in both
    // directions.
    let jlist = Ty::reference(&db, "javax.swing.JList", Vec::new());
    let java_kinds = kotlin_declared_members(
        &db,
        &scope,
        &jlist,
        &hir_expand::name::Name::new("dragEnabled"),
        site,
    )
    .into_iter()
    .map(|member| (member.kind, member.params.len()))
    .collect::<Vec<_>>();
    assert!(
        java_kinds.contains(&(MemberKind::Getter, 0)),
        "the Java getter is the property's read: {java_kinds:?}"
    );
    assert!(
        java_kinds.contains(&(MemberKind::Setter, 1)),
        "the Java setter is the property's write: {java_kinds:?}"
    );
}

/// What `@JvmName` renames, and what it does not. kotlinc 2.4.20, `javap -p
/// m6.ShapesKt`:
///
/// ```text
/// public static final java.lang.String NAME;
/// private static final int topVal;
/// private static int topVar;
/// public static final int CONST;
/// public static final java.lang.String twice(java.lang.String);
/// public static final int renamed();
/// public static final int getRenamed();
/// public static final int getTopVar();
/// public static final void writeTopVar(int);
/// ```
///
/// `const val NAME = "renamed"` with `@JvmName(NAME)` names the *member*
/// `renamed()` — the constant's value is what the compiler folds — while the
/// `const val`'s own field keeps the property's name (`NAME`). A `const val` is
/// a property with a backing field, so `@JvmName` on one is a compiler error
/// ("this annotation is not applicable to target 'top level property with
/// backing field'. Applicable targets: function, getter, setter, file") and
/// renames nothing; the accessor targets do the renaming — `@get:JvmName`
/// names the reader, `@set:JvmName` the writer — and the *backing field* keeps
/// the property's name whatever they say (`topVal`, `topVar`).
/// (<https://kotlinlang.org/docs/java-interop.html#getters-and-setters>.)
#[test]
fn a_jvm_name_written_as_a_constant_renames_the_member() {
    let (db, file) = shapes();
    assert_eq!(
        facade_methods(
            &db,
            file,
            &[
                "renamed",
                "topFn",
                "getRenamed",
                "getTopVal",
                "getTopVar",
                "writeTopVar"
            ]
        ),
        vec![
            // `public static final int renamed();` — `@JvmName(NAME)` with
            // `const val NAME = "renamed"`.
            "public static final int renamed()",
            // `public static final int getRenamed();`
            "public static final int getRenamed()",
            // `public static final int getTopVar();`
            "public static final int getTopVar()",
            // `public static final void writeTopVar(int);` — the *setter* the
            // `@set:JvmName` names; the reader keeps the property's name.
            "public static final void writeTopVar(int)",
        ]
    );
    // A `const val`'s field is named by the property, never by `@JvmName`, and
    // its type is the one its initializer gives it.
    assert_eq!(
        facade_fields(&db, file, &["NAME", "CONST", "renamed"]),
        vec![
            // `public static final java.lang.String NAME;`
            "public static final java.lang.String NAME",
            // `public static final int CONST;`
            "public static final int CONST",
        ]
    );
}

/// A `data class` generates one `componentN` per component property, in
/// declaration order, and a `copy` taking them all and returning the class's
/// own type. `kotlinc 2.4.20` + `javap -p` on
///
/// ```kotlin
/// data class Point(val x: Int, var y: String) {
///     val label: String = "p"
/// }
/// ```
///
/// ```text
/// public final int component1();
/// public final java.lang.String component2();
/// public final Point copy(int, java.lang.String);
/// public static Point copy$default(Point, int, java.lang.String, int, java.lang.Object);
/// public java.lang.String toString();
/// public int hashCode();
/// public boolean equals(java.lang.Object);
/// ```
///
/// — `component2` is the `var`'s type, the *non*-component `label` generates
/// nothing, and `copy$default` is the compiler's `ACC_SYNTHETIC` bridge, which
/// a Java caller never writes.
#[test]
fn a_data_class_carries_its_generated_components_and_copy() {
    let source = r#"
data class Point(val x: Int, var y: String) {
    val label: String = "p"
}
"#;
    let (db, file) = fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    assert_eq!(
        methods(&db, file, "Point", &["component1", "component2", "copy"]),
        vec![
            "public final int component1()",
            "public final java.lang.String component2()",
            "public final Point copy(int, java.lang.String)",
        ]
    );
    assert!(
        methods(&db, file, "Point", &["copy$default"]).is_empty(),
        "the synthetic bridge is not a member a Java caller writes"
    );
    // `equals`/`hashCode`/`toString` are the class's overrides of `Object`'s,
    // which the supertype walk reaches: nothing is generated here for them.
    assert!(
        methods(&db, file, "Point", &["equals", "hashCode", "toString"]).is_empty(),
        "the `Object` overrides come from `java.lang.Object`"
    );
}

/// An `enum class` generates `values()` and `valueOf(String)` — the classfile's
/// *statics*, which a Java caller writes as it writes any enum's.
/// `kotlinc 2.4.20` + `javap -p` on
///
/// ```kotlin
/// enum class Color { RED, GREEN }
/// ```
///
/// ```text
/// public final class Color extends java.lang.Enum<Color> {
///   public static final Color RED;
///   public static final Color GREEN;
///   private static final Color[] $VALUES;
///   private static final kotlin.enums.EnumEntries $ENTRIES;
///   private Color();
///   public static Color[] values();
///   public static Color valueOf(java.lang.String);
///   public static kotlin.enums.EnumEntries<Color> getEntries();
///   private static final Color[] $values();
///   static {};
/// }
/// ```
///
/// — `values` is the array of the enum's own type and `valueOf` the type, both
/// `static` at the class's own access and neither `final`. `getEntries` is the
/// `entries` property's accessor, which no source declaration carries (it is a
/// standard-library member of `Enum`), so nothing pushes it.
#[test]
fn an_enum_carries_the_values_and_value_of_the_compiler_generates() {
    let source = r#"
enum class Color { RED, GREEN }
"#;
    let (db, file) = fixture(&[("/src/main/kotlin/Sample.kt", source)]);
    assert_eq!(
        methods(&db, file, "Color", &["values", "valueOf"]),
        vec![
            // `public static Color[] values();`
            "public static Color[] values()",
            // `public static Color valueOf(java.lang.String);`
            "public static Color valueOf(java.lang.String)",
        ]
    );
}
