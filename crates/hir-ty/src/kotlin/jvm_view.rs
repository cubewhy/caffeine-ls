//! The JVM-visible shape of a Kotlin declaration, as the *JVM* member set the
//! type layer enumerates.
//!
//! A caller of another language reads a Kotlin declaration through the
//! classfile the compiler emits for it, so this module answers with the same
//! [`MethodData`]/[`FieldData`] shapes the JVM layer's member set carries for a
//! classfile — one whose `owner_file`/`decl_item` point at the *Kotlin*
//! declaration. The shapes are the compiler's, observed with kotlinc 2.4.20 and
//! `javap -p`, and described in the item tree's own class-kind table
//! ([`hir_def::kotlin::item_tree::KotlinClassKind`]):
//!
//! | Kotlin | classfile |
//! |---|---|
//! | `val x: T` | `getX(): T` |
//! | `var x: T` | `getX(): T` + `setX(T)` |
//! | `val isX: Boolean` | `isX(): boolean` |
//! | an `annotation class`'s `val x` | an element `x()`, named by the property |
//! | a member of an `interface` with no body | `abstract`; one with a body is the `default` method |
//! | an `override` that names no modality | *open* — neither `abstract` nor `final` |
//! | `const val x` / `@JvmField val x` | a field `x` — an instance field of a class, a static of an `object`, a static of the enclosing class for a companion's, a static of the facade for a file's |
//! | `object Util` | a static field `Util.INSTANCE`; its own members stay *instance* members, and its constructor is `private` |
//! | `companion object` | a static field `Companion` on the enclosing class; its own constructor is `private` |
//! | `@JvmStatic` on a companion member | *also* a static of the enclosing class |
//! | a top-level `fun f()` / `val x` | a static member of the file's facade `FooKt` |
//! | `fun T.f()` (an extension) | a member whose **first** parameter is the receiver — a static of the facade when it is top-level |
//! | `@JvmName("y")` on a function / `@get:JvmName("y")` on an accessor | the member is named `y` |
//! | `@file:JvmName("Y")` | the facade is named `Y` |
//! | `@JvmOverloads fun f(a: Int, b: Int = 0)` | one further method per parameter that declares a default |
//! | `@Throws(IOException::class) fun f()` | the method declares `throws IOException` |
//! | an `enum class` entry | a static field of the enum type, and its constructor is `private` |
//! | a constructor | `<init>`, and a parameterless `<init>()` for an all-defaults primary of a `class` |
//! | an `interface` / `annotation class` | no constructor at all |
//!
//! Every type is *erased*: the classfile carries the erasure of a Kotlin type
//! (`T` is its bound, `List<Int>` is `java.util.List`), so the type arguments a
//! Kotlin receiver wrote never reach a Java caller — which is why nothing here
//! takes the receiver's arguments.
//!
//! The annotations of the table are recognized by the canonical name their
//! application *resolves* to in the declaration's scope ([`JvmAnnotation`]),
//! never by the last segment the source wrote: `@JvmName`, the qualified
//! `@kotlin.jvm.JvmName` and an aliased import of it are one annotation, while
//! a `JvmName` the file's own package or an enclosing classifier declares is
//! another. They are library annotations of kotlin-stdlib, so a classpath
//! without the library resolves none of them — the compiler's own answer for a
//! file that cannot see the library.
//!
//! `kotlin.Int` and its siblings are the JVM primitives, every other classifier
//! is the JVM class [`MAPPED_TYPES`] names, `T?` is `T`, and a flexible type is
//! its lower half ([`ty_from_kotlin`]).
//!
//! KLS *Kotlin/Core* has no Java-interop section, so the rules here cite
//! <https://kotlinlang.org/docs/java-interop.html>, as the item tree's
//! class-kind table already does.

use hir::hir_def::kotlin::item_tree::{
    ClassData, ConstructorData, FunctionData, KotlinAnnotationRef, KotlinClassKind, KotlinItemData,
    KotlinItemTree, PropertyData,
};
use hir::hir_def::kotlin::modifiers::{
    KotlinModality, KotlinModifierFlags, KotlinModifiers, KotlinVisibility,
};
use hir_def::jvm::decl::ItemAnnotationValue;
use hir_def::kotlin::annotations::JvmAnnotation;
use hir_expand::body::Literal;
use hir_expand::ids::ItemId;
use hir_expand::name::Name;
use vfs::FileId;

use super::resolve::KotlinResolver;
use super::ty::{ty_from_kotlin, ty_from_type_ref};
use crate::jvm::db::TyDatabase;
use crate::jvm::member::{Access, ClassKey, FieldData, MethodData, MethodTypeParam};
use crate::jvm::member_set::source_top_level;
use crate::ty::{Ty, TypeVarScope};

/// The JVM methods a Kotlin classifier declares under the JVM name `name` —
/// the Java source enumeration's twin, for a Kotlin file.
pub fn java_view_members(
    db: &dyn TyDatabase,
    source: hir::SourceClass,
    name: &str,
) -> Vec<MethodData> {
    let tree = hir::file_item_tree(db, source.file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return Vec::new();
    };
    let KotlinItemData::Class(class) = tree.data(source.item) else {
        return Vec::new();
    };
    let shapes = Shapes::of(db, tree, source.file, class, class_key(db, source));
    let mut out = Vec::new();
    // The Java `new` path names a *source* class's constructor by the class
    // itself and a classfile one `<init>`
    // ([`crate::java::infer::new_expr`]), so both names answer.
    if name == "<init>" || name == class.name.as_str() {
        shapes.constructors(source.item, class, &mut out);
        return out;
    }
    for member in class.body.iter().copied() {
        let resolver = resolver_of(db, tree, source.file, member);
        shapes.push_matching(&resolver, member, name, false, false, &mut out);
        // A `companion object`'s `@JvmStatic` members are *also* statics of the
        // enclosing class
        // (<https://kotlinlang.org/docs/java-interop.html#static-methods>).
        if let KotlinItemData::Class(companion) = tree.data(member)
            && companion.kind == KotlinClassKind::CompanionObject
        {
            for inner in companion.body.iter().copied() {
                let resolver = resolver_of(db, tree, source.file, inner);
                shapes.push_matching(&resolver, inner, name, true, false, &mut out);
            }
        }
    }
    // `object Util` reaches its members through `Util.INSTANCE`, and a
    // companion object through the `Companion` field; the *field* is what
    // [`java_view_fields`] answers, the members stay instance members.
    out
}

/// The JVM fields a Kotlin classifier declares under the name `name`: a
/// `const val`, an `@JvmField` property, an `object`'s `INSTANCE`, a
/// `companion object`'s `Companion` field, a companion's `const val`/
/// `@JvmField` (which the *enclosing* class carries) and an `enum class`'s
/// entries.
pub fn java_view_fields(
    db: &dyn TyDatabase,
    source: hir::SourceClass,
    name: &str,
) -> Vec<FieldData> {
    let tree = hir::file_item_tree(db, source.file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return Vec::new();
    };
    let KotlinItemData::Class(class) = tree.data(source.item) else {
        return Vec::new();
    };
    let shapes = Shapes::of(db, tree, source.file, class, class_key(db, source));
    let mut out = Vec::new();
    // An `object` *is* the singleton: the compiler gives it the static final
    // field `INSTANCE` of its own type, which is what a Java caller reads
    // (<https://kotlinlang.org/docs/java-interop.html#static-methods>). It is
    // the *object's* field, so it is answered here and not by the walk of an
    // enclosing class's body.
    if class.kind == KotlinClassKind::Object && name == "INSTANCE" {
        out.push(FieldData {
            name: name.to_owned(),
            owner: shapes.owner.clone(),
            owner_file: Some(source.file),
            decl_item: Some(source.item),
            // The object itself, by its canonical name.
            ty: shapes.owner.as_ty(db, Vec::new()),
            descriptor: None,
            is_static: true,
            access: Access::Public,
            is_final: true,
            declaring_package: tree.package.as_ref().map(|package| package.to_string()),
            declaring_top_level: Some(class.name.to_string()),
        });
    }
    for member in class.body.iter().copied() {
        let resolver = resolver_of(db, tree, source.file, member);
        shapes.push_field(&resolver, member, name, false, &mut out);
        // A `companion object`'s `const val`s and `@JvmField` properties are
        // static fields of the *enclosing* class, exactly as its `@JvmStatic`
        // members are statics of it
        // (<https://kotlinlang.org/docs/java-interop.html#static-fields>; the
        // companion's own class carries neither).
        if let KotlinItemData::Class(companion) = tree.data(member)
            && companion.kind == KotlinClassKind::CompanionObject
        {
            for inner in companion.body.iter().copied() {
                let resolver = resolver_of(db, tree, source.file, inner);
                shapes.push_field(&resolver, inner, name, true, &mut out);
            }
        }
    }
    out
}

/// The static members of a Kotlin file's facade class: every top-level
/// function and property of the file, under the JVM names the compiler gives
/// them.
pub fn file_facade_members(db: &dyn TyDatabase, file: FileId, name: &str) -> Vec<MethodData> {
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return Vec::new();
    };
    let Some(facade) = facade_fqn(tree, db, file) else {
        return Vec::new();
    };
    let shapes = Shapes::of_file(db, tree, file, facade);
    let mut out = Vec::new();
    for &top in &tree.top {
        let resolver = resolver_of(db, tree, file, top);
        shapes.push_matching(&resolver, top, name, false, true, &mut out);
    }
    out
}

/// The static fields of a Kotlin file's facade: its `const val`s and
/// `@JvmField` properties.
pub fn file_facade_fields(db: &dyn TyDatabase, file: FileId, name: &str) -> Vec<FieldData> {
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return Vec::new();
    };
    let Some(facade) = facade_fqn(tree, db, file) else {
        return Vec::new();
    };
    let shapes = Shapes::of_file(db, tree, file, facade);
    let mut out = Vec::new();
    for &top in &tree.top {
        let resolver = resolver_of(db, tree, file, top);
        // A facade's fields are its statics ([`Shapes::class`] is `None` for a
        // file, which is what makes them static).
        shapes.push_field(&resolver, top, name, false, &mut out);
    }
    out
}

/// The resolver of a declaration in `file` — its own type parameters and the
/// enclosing classifiers'.
fn resolver_of<'a>(
    db: &'a dyn TyDatabase,
    tree: &'a KotlinItemTree,
    file: FileId,
    item: ItemId,
) -> KotlinResolver<'a> {
    KotlinResolver::for_item(db, file, tree, item)
}

/// The fully qualified name of a Kotlin file's facade class — the *index*
/// layer's own answer ([`hir::file_facade_class`]), which the type layer reads
/// rather than deriving a second one of its own: the file's stem with `Kt`
/// appended, or the `@file:JvmName` the file writes
/// (<https://kotlinlang.org/docs/java-interop.html#package-level-functions>).
fn facade_fqn(tree: &KotlinItemTree, db: &dyn TyDatabase, file: FileId) -> Option<Name> {
    let facade = hir::file_facade_class(db, file)?;
    Some(match &tree.package {
        Some(package) => Name::new(&format!("{package}.{facade}")),
        None => facade,
    })
}

/// The class key of a Kotlin source class.
fn class_key(db: &dyn TyDatabase, source: hir::SourceClass) -> ClassKey {
    match hir::source_class_fqn(db, source.file, source.item) {
        Some(fqn) => ClassKey::Named(fqn),
        None => ClassKey::Local(source),
    }
}

/// The canonical name an annotation application's type resolves to in the
/// scope of the declaration it is written on.
///
/// An application names a *type* ([KLS
/// `annotations.html#annotation-declarations`](https://kotlinlang.org/spec/annotations.html#annotation-declarations)),
/// so the name the source wrote is not the annotation's identity:
/// `@kotlin.jvm.JvmName("x")` and `@JN("x")` under
/// `import kotlin.jvm.JvmName as JN` are both the library's annotation, while a
/// `JvmName` that the file's own package, an import or an enclosing classifier
/// declares is a different annotation that merely shares the last segment.
fn annotation_fqn(
    resolver: &KotlinResolver<'_>,
    application: &KotlinAnnotationRef,
) -> Option<Name> {
    resolver.class_fqn(application.annotation.name.as_str())
}

/// Whether the application is the `kotlin.jvm` annotation `wanted`
/// ([`JvmAnnotation`]), which the compiler reads for the declaration's JVM
/// shape.
///
/// The *use-site target* does not decide membership: the annotations are
/// recognized wherever they are written — `@field:JvmField` is the same
/// application as `@JvmField`, since `kotlin.jvm.JvmField`'s only target is the
/// field — and an application on a target the annotation does not allow is a
/// compiler error (`this annotation is not applicable to target …`, kotlinc
/// 2.4.20) that the JVM view has no reason to reproduce.
fn is_jvm_annotation(
    resolver: &KotlinResolver<'_>,
    application: &KotlinAnnotationRef,
    wanted: JvmAnnotation,
) -> bool {
    annotation_fqn(resolver, application).is_some_and(|fqn| wanted.is(fqn.as_str()))
}

/// The `@JvmName("x")` of an annotation list: the JVM name the compiler gives
/// the member instead of the Kotlin one. The element values are the ones M2
/// lowered, so a name written as a *constant* (`@JvmName(SOME_NAME)`) is not
/// read yet — a recorded gap, not a different rule.
fn annotation_name(
    resolver: &KotlinResolver<'_>,
    annotations: &[KotlinAnnotationRef],
) -> Option<Name> {
    for application in annotations {
        if !is_jvm_annotation(resolver, application, JvmAnnotation::Name) {
            continue;
        }
        for arg in &application.annotation.args {
            if let ItemAnnotationValue::Literal(Literal::Str(value)) = &arg.value {
                return Some(Name::new(value));
            }
        }
    }
    None
}

/// The `@useSite:JvmName("x")` of an annotation list, if it writes one: the
/// `get` of `@get:JvmName("x")`, the `set` of `@set:JvmName("x")`
/// (<https://kotlinlang.org/docs/java-interop.html#getters-and-setters>).
fn targeted_name(
    resolver: &KotlinResolver<'_>,
    annotations: &[KotlinAnnotationRef],
    target: &str,
) -> Option<String> {
    for application in annotations {
        let applies = application
            .target
            .as_ref()
            .is_some_and(|written| written.as_str() == target);
        if applies && is_jvm_annotation(resolver, application, JvmAnnotation::Name) {
            for arg in &application.annotation.args {
                if let ItemAnnotationValue::Literal(Literal::Str(value)) = &arg.value {
                    return Some(value.clone());
                }
            }
        }
    }
    None
}

/// Whether the annotation list applies the `kotlin.jvm` annotation `wanted`.
fn has_annotation(
    resolver: &KotlinResolver<'_>,
    annotations: &[KotlinAnnotationRef],
    wanted: JvmAnnotation,
) -> bool {
    annotations
        .iter()
        .any(|application| is_jvm_annotation(resolver, application, wanted))
}

/// What the JVM view of one Kotlin declaration needs: the owner, the package
/// and the two properties of the enclosing classifier the classfile shapes
/// depend on.
struct Shapes<'a> {
    db: &'a dyn TyDatabase,
    tree: &'a KotlinItemTree,
    file: FileId,
    /// The classifier's own package, for a member's declaring package.
    package: Option<String>,
    /// The canonical name of the *top-level* class the declaration belongs to
    /// ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)).
    top_level: String,
    /// The classifier the members belong to; `None` for a file's facade.
    class: Option<&'a ClassData>,
    /// The `ClassKey` of the owner — the classifier, or the facade.
    owner: ClassKey,
}

impl<'a> Shapes<'a> {
    fn of(
        db: &'a dyn TyDatabase,
        tree: &'a KotlinItemTree,
        file: FileId,
        class: &'a ClassData,
        owner: ClassKey,
    ) -> Shapes<'a> {
        let package = tree.package.as_ref().map(|package| package.to_string());
        let top_level = match &owner {
            ClassKey::Named(fqn) => source_top_level(package.as_deref(), fqn.as_str()),
            ClassKey::Local(_) => class.name.to_string(),
        };
        Shapes {
            db,
            tree,
            file,
            package,
            top_level,
            class: Some(class),
            owner,
        }
    }

    fn of_file(
        db: &'a dyn TyDatabase,
        tree: &'a KotlinItemTree,
        file: FileId,
        facade: Name,
    ) -> Shapes<'a> {
        let package = tree.package.as_ref().map(|package| package.to_string());
        let top_level = source_top_level(package.as_deref(), facade.as_str());
        Shapes {
            db,
            tree,
            file,
            package,
            top_level,
            class: None,
            owner: ClassKey::Named(facade),
        }
    }

    fn kind(&self) -> Option<KotlinClassKind> {
        self.class.map(|class| class.kind)
    }

    fn is_interface(&self) -> bool {
        matches!(
            self.kind(),
            Some(KotlinClassKind::Interface | KotlinClassKind::Annotation)
        )
    }

    /// Whether the classfile member carries `ACC_ABSTRACT`, and whether it
    /// carries `ACC_FINAL`.
    ///
    /// The written modality is only what the declaration *departs* from, so the
    /// flag pair is read from the declaration's context:
    ///
    /// * a member of an `interface`/`annotation class` that declares a body is
    ///   the classfile's `default` method — neither `abstract` nor `final` —
    ///   and one that declares none is `abstract`
    ///   (<https://kotlinlang.org/docs/interfaces.html>: a member of an
    ///   interface is `open` by default; kotlinc 2.4.20 emits
    ///   `public default int withBody();` beside `public abstract int
    ///   noBody();` for `interface Iface { fun withBody(): Int = 1; fun
    ///   noBody(): Int }`);
    /// * an `override` that names no modality is `open`
    ///   ([KLS `inheritance.html#overriding`](https://kotlinlang.org/spec/inheritance.html#overriding):
    ///   an override is open by default), even in a class the compiler
    ///   finalizes: `class Derived : Base2() { override fun o(): Int }` emits
    ///   `public int o();`;
    /// * every other member takes the written modality, whose default is
    ///   `final` ([KLS `declarations.html#classifier-declaration`]): a
    ///   declaration that names none is `final` in a `class`, an `open class`
    ///   and an `abstract class` alike (`public final int f();` in all three,
    ///   kotlinc 2.4.20).
    ///
    /// A deviation, recorded rather than modelled: `final override fun f()` is
    /// an `override` that *does* name a modality and carries `ACC_FINAL`, but
    /// [`KotlinModifiers`] keeps one modality tag and no flag for the written
    /// keyword, so an explicitly written `final` is indistinguishable from the
    /// default and only the `override` default is applied. The classifier's
    /// modifiers have the same shape ([`KotlinModifiers::names`] documents it).
    fn member_flags(&self, modifiers: KotlinModifiers, has_body: bool) -> (bool, bool) {
        if self.is_interface() {
            // A member with a body is the classfile's `default` method; one
            // without is the interface's abstract obligation.
            return match has_body {
                true => (false, false),
                false => (true, false),
            };
        }
        match modifiers.modality {
            KotlinModality::Abstract => (true, false),
            KotlinModality::Open => (false, false),
            // An `override` names no modality and is open by default; a
            // `sealed` class's members are `final` by default, exactly as a
            // `class`'s are.
            KotlinModality::Final | KotlinModality::Sealed
                if modifiers.flags.contains(KotlinModifierFlags::OVERRIDE) =>
            {
                (false, false)
            }
            KotlinModality::Final | KotlinModality::Sealed => (false, true),
        }
    }

    /// The JVM name of the property's *reader* in this classifier.
    ///
    /// An `annotation class`'s property is the annotation's *element*, and the
    /// classfile names it by the property alone — no `get` prefix and no
    /// `is` rule: `annotation class Ann(val x: Int, val y: String)` compiles to
    /// `public abstract int x();` and `public abstract java.lang.String y();`
    /// (kotlinc 2.4.20), which is what a Java caller writes
    /// (<https://kotlinlang.org/docs/annotations.html#constructors>). Every
    /// other kind's reader is the JavaBeans getter
    /// ([`property_getter_name`]).
    fn property_reader_name(
        &self,
        resolver: &KotlinResolver<'_>,
        property: &PropertyData,
    ) -> Option<String> {
        if self.kind() == Some(KotlinClassKind::Annotation) {
            return Some(property.name.to_string());
        }
        property_getter_name(resolver, property)
    }

    /// Whether the property declares an accessor of `is_setter`'s direction
    /// with a body: the accessor the classfile carries as a method with code
    /// (`val p: Int get() = 2`), as opposed to the one it must leave abstract
    /// (`val p: Int` in an interface).
    fn declares_accessor_body(&self, property: &PropertyData, is_setter: bool) -> bool {
        property.accessors.iter().any(|&accessor| {
            matches!(
                self.tree.data(accessor),
                KotlinItemData::Accessor(data)
                    if data.is_setter == is_setter && data.body.is_some()
            )
        })
    }

    /// The JVM access of the constructors the compiler emits for this
    /// classifier.
    ///
    /// An `object`, a `companion object` and an `enum class` declare no
    /// *public* constructor whatever the declaration writes: kotlinc 2.4.20
    /// emits `private m6b.Obj();`, `private m6b.Holder$Companion();` and
    /// `private m6b.En(int);`
    /// (<https://kotlinlang.org/docs/classes.html#constructors>,
    /// <https://kotlinlang.org/docs/enum-classes.html>: "enum class
    /// constructors are private").
    fn constructor_access(&self, visibility: KotlinVisibility) -> Access {
        match self.kind() {
            Some(
                KotlinClassKind::Object | KotlinClassKind::CompanionObject | KotlinClassKind::Enum,
            ) => Access::Private,
            _ => access(visibility),
        }
    }

    /// The JVM members of `item` whose JVM name is `name`, the empty name
    /// being the wildcard of a *declaration-level* enumeration — the Java
    /// path's own convention ([`crate::jvm::member_set`]'s `all_methods`,
    /// whose Java arm answers every method of the class the same way).
    /// `only_static` restricts the walk to `@JvmStatic` members (the enclosing
    /// class's view of its companion's statics); `force_static` marks every
    /// member pushed as static (a file's facade).
    fn push_matching(
        &self,
        resolver: &KotlinResolver<'_>,
        item: ItemId,
        name: &str,
        only_static: bool,
        force_static: bool,
        out: &mut Vec<MethodData>,
    ) {
        let data = self.tree.data(item);
        // `@JvmStatic` is only legal on a *function* or a property, and only
        // where the declaration sits in an `object` or a companion
        // (<https://kotlinlang.org/docs/java-interop.html#static-methods>).
        let jvm_static = match data {
            KotlinItemData::Function(function) => {
                has_annotation(resolver, &function.annotations, JvmAnnotation::Static)
            }
            KotlinItemData::Property(property) => {
                has_annotation(resolver, &property.annotations, JvmAnnotation::Static)
            }
            _ => false,
        };
        if only_static && !jvm_static {
            return;
        }
        // `@JvmStatic` promotes a member to a static of the class that *holds*
        // the object: on an `object`'s own class (`object Util { @JvmStatic val
        // sx }` compiles to `public static final int getSx()` on `Util`), and
        // on the *enclosing* class of a companion. The companion's own class
        // keeps the member an instance member — `javap -p` shows
        // `Holder$Companion.csn()` beside the static `Holder.csn()` — which is
        // why the enclosing class's walk (`only_static`) is the one that marks
        // it static there.
        let is_static =
            force_static || (jvm_static && self.kind() != Some(KotlinClassKind::CompanionObject));
        match data {
            KotlinItemData::Function(function) => {
                let jvm = annotation_name(resolver, &function.annotations)
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| function.name.to_string());
                if name.is_empty() || jvm == name {
                    self.push_function(resolver, item, function, is_static, out);
                }
            }
            KotlinItemData::Property(property) => {
                // `@JvmField` suppresses the accessors: the property is a
                // field, which [`Shapes::push_field`] answers.
                if has_annotation(resolver, &property.annotations, JvmAnnotation::Field) {
                    return;
                }
                // An `object`'s accessors are *instance* members of the
                // object's class — `object Util { val x = 1 }` compiles to
                // `public final int getX()` on `Util`, reached by a Java caller
                // as `Util.INSTANCE.getX()`, and only a `@JvmStatic` property
                // adds the static accessor (`is_static` carries it).
                if let Some(getter) = self.property_reader_name(resolver, property)
                    && (name.is_empty() || getter == name)
                {
                    self.push_accessor(resolver, item, property, &getter, false, is_static, out);
                }
                if property.is_var
                    && let Some(setter) = property_setter_name(resolver, property)
                    && (name.is_empty() || setter == name)
                {
                    self.push_accessor(resolver, item, property, &setter, true, is_static, out);
                }
            }
            _ => {}
        }
    }

    /// The `<init>` methods of a Kotlin classifier: one per declared
    /// constructor, or the implicit `<init>()` when it declares none —
    /// at the access [`Shapes::constructor_access`] gives.
    fn constructors(&self, item: ItemId, class: &ClassData, out: &mut Vec<MethodData>) {
        // An `interface` and an `annotation class` have no constructor at all:
        // kotlinc 2.4.20 emits none for either (an annotation class is an
        // interface in the classfile,
        // <https://kotlinlang.org/docs/annotations.html>), so a Java caller
        // cannot instantiate one.
        if self.is_interface() {
            return;
        }
        // The *primary* constructor lives in the class header, not in the body
        // ([KLS `declarations.html#primary-constructor`]).
        let mut declared = false;
        // The *primary* constructor an all-defaults list needs the compiler's
        // parameterless `<init>` added to, if it turns out to need one.
        let mut all_defaults = None;
        if let Some(primary) = class.primary_constructor
            && let KotlinItemData::Constructor(constructor) = self.tree.data(primary)
        {
            declared = true;
            let resolver = resolver_of(self.db, self.tree, self.file, primary);
            self.push_constructor(&resolver, primary, constructor, out);
            // "On the JVM, if all primary constructor parameters have default
            // values, the compiler implicitly provides a parameterless
            // constructor that uses those default values"
            // (<https://kotlinlang.org/docs/classes.html#constructors>), at the
            // primary constructor's own access — kotlinc 2.4.20 emits
            // `public E()` for `class E(val a: Int = 0, val b: Int = 0)` and
            // `internal`/`protected` alike, but *nothing* for a `private`
            // constructor (the documented rule says nothing of the access,
            // which is the compiler's own refinement). A *secondary*
            // constructor of all-defaults parameters gains no such constructor.
            if !constructor.params.is_empty()
                && constructor.defaults.iter().all(Option::is_some)
                && self.constructor_access(constructor.modifiers.visibility) != Access::Private
            {
                all_defaults = Some((primary, &constructor.modifiers));
            }
        }
        for member in class.body.iter().copied() {
            let KotlinItemData::Constructor(constructor) = self.tree.data(member) else {
                continue;
            };
            declared = true;
            let resolver = resolver_of(self.db, self.tree, self.file, member);
            self.push_constructor(&resolver, member, constructor, out);
        }
        if !declared {
            // A class with no declared constructor has the compiler's implicit
            // `public <init>()`.
            let empty = parameterless_constructor(KotlinModifiers::none());
            let resolver = resolver_of(self.db, self.tree, self.file, item);
            self.push_constructor(&resolver, item, &empty, out);
        } else if let Some((primary, modifiers)) = all_defaults
            // The parameterless constructor the class already has — from
            // `@JvmOverloads` on the primary, or from a secondary
            // `constructor()` of its own, which kotlinc 2.4.20 accepts beside
            // this rule and emits once — is the same `<init>()`.
            && out.iter().all(|method| !method.params.is_empty())
        {
            let parameterless = parameterless_constructor(modifiers.clone());
            let resolver = resolver_of(self.db, self.tree, self.file, primary);
            self.push_constructor(&resolver, primary, &parameterless, out);
        }
    }

    fn push_function(
        &self,
        resolver: &KotlinResolver<'_>,
        item: ItemId,
        function: &FunctionData,
        is_static: bool,
        out: &mut Vec<MethodData>,
    ) {
        let jvm_name = annotation_name(resolver, &function.annotations)
            .map(|name| name.to_string())
            .unwrap_or_else(|| function.name.to_string());
        // A Kotlin *extension* compiles to a member whose **first** parameter
        // is the receiver: `fun String.twice(): String` is the facade's
        // `public static final java.lang.String twice(java.lang.String);`
        // (<https://kotlinlang.org/docs/java-to-kotlin-interop.html#extension-functions>),
        // and a *member* extension is the receiver's class's instance method
        // of the same shape (`class Outer { fun String.memberExt(): Int }`
        // emits `public final int memberExt(java.lang.String);`, kotlinc
        // 2.4.20).
        let mut params: Vec<Ty> = Vec::new();
        if let Some(receiver) = &function.receiver {
            params.push(ty_from_kotlin(
                self.db,
                ty_from_type_ref(self.db, resolver, &receiver.ty),
            ));
        }
        params.extend(function.params.iter().map(|param| {
            let ty = ty_from_kotlin(
                self.db,
                ty_from_type_ref(self.db, resolver, &param.param.ty.ty),
            );
            // A `vararg` parameter is the array the classfile carries.
            if param.param.varargs {
                Ty::array(self.db, ty)
            } else {
                ty
            }
        }));
        let ret = match &function.ret {
            Some(ret) => ty_from_kotlin(self.db, ty_from_type_ref(self.db, resolver, &ret.ty)),
            // An expression-bodied function without a written type, and a
            // `Unit`-returning one, compile to `void` — the compiler's
            // signature inference is what decides which (KLS
            // `type-inference.html#function-signature-type-inference`), and
            // until it lands the erased answer is `void`.
            None => Ty::void(self.db),
        };
        let varargs = function
            .params
            .last()
            .is_some_and(|param| param.param.varargs);
        let type_params: Vec<MethodTypeParam> = function
            .type_params
            .iter()
            .map(|param| MethodTypeParam {
                scope: TypeVarScope::Method {
                    file: self.file,
                    item,
                    name: param.name.clone(),
                },
                bounds: param
                    .bounds
                    .iter()
                    .map(|bound| {
                        ty_from_kotlin(self.db, ty_from_type_ref(self.db, resolver, &bound.ty))
                    })
                    .collect(),
            })
            .collect();
        // `@JvmOverloads`: one further method per parameter that declares a
        // default value
        // (<https://kotlinlang.org/docs/java-interop.html#overloads-generation>).
        let jvm_overloads =
            has_annotation(resolver, &function.annotations, JvmAnnotation::Overloads);
        let throws = throws_of(self.db, resolver, &function.annotations, None);
        let mut defaulted: Vec<bool> = function.defaults.iter().map(Option::is_some).collect();
        // The receiver stands before every declared parameter, and is never
        // one the `@JvmOverloads` overloads drop.
        if function.receiver.is_some() {
            defaulted.insert(0, false);
        }
        let (abstract_, is_final) = self.member_flags(function.modifiers, function.body.is_some());
        for list in overload_lists(&params, &defaulted, jvm_overloads) {
            out.push(MethodData {
                name: jvm_name.clone(),
                owner: self.owner.clone(),
                owner_file: Some(self.file),
                decl_item: Some(item),
                varargs: varargs && list.len() == params.len(),
                params: list,
                param_names: None,
                ret,
                throws: throws.clone(),
                is_static,
                abstract_,
                is_final,
                access: access(function.modifiers.visibility),
                declaring_package: self.package.clone(),
                declaring_top_level: Some(self.top_level.clone()),
                declaring_interface: self.is_interface(),
                type_params: type_params.clone(),
                raw_erased: false,
                descriptor: None,
            });
        }
    }

    /// One `<init>` per parameter list the constructor compiles to:
    /// `@JvmOverloads` adds one list per parameter that declares a default
    /// value ([`overload_lists`]).
    fn push_constructor(
        &self,
        resolver: &KotlinResolver<'_>,
        item: ItemId,
        constructor: &ConstructorData,
        out: &mut Vec<MethodData>,
    ) {
        let params: Vec<Ty> = constructor
            .params
            .iter()
            .map(|param| {
                let ty = ty_from_kotlin(
                    self.db,
                    ty_from_type_ref(self.db, resolver, &param.param.ty.ty),
                );
                if param.param.varargs {
                    Ty::array(self.db, ty)
                } else {
                    ty
                }
            })
            .collect();
        let varargs = constructor
            .params
            .last()
            .is_some_and(|param| param.param.varargs);
        let jvm_overloads =
            has_annotation(resolver, &constructor.annotations, JvmAnnotation::Overloads);
        let throws = throws_of(self.db, resolver, &constructor.annotations, None);
        let defaulted: Vec<bool> = constructor.defaults.iter().map(Option::is_some).collect();
        for list in overload_lists(&params, &defaulted, jvm_overloads) {
            out.push(MethodData {
                name: "<init>".to_owned(),
                owner: self.owner.clone(),
                owner_file: Some(self.file),
                decl_item: Some(item),
                varargs: varargs && list.len() == params.len(),
                params: list,
                param_names: None,
                ret: Ty::void(self.db),
                throws: throws.clone(),
                is_static: false,
                abstract_: false,
                is_final: false,
                access: self.constructor_access(constructor.modifiers.visibility),
                declaring_package: self.package.clone(),
                declaring_top_level: Some(self.top_level.clone()),
                declaring_interface: false,
                type_params: Vec::new(),
                raw_erased: false,
                descriptor: None,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_accessor(
        &self,
        resolver: &KotlinResolver<'_>,
        item: ItemId,
        property: &PropertyData,
        name: &str,
        is_setter: bool,
        is_static: bool,
        out: &mut Vec<MethodData>,
    ) {
        let ty = match &property.ty {
            Some(ty) => ty_from_kotlin(self.db, ty_from_type_ref(self.db, resolver, &ty.ty)),
            None => Ty::reference(self.db, "java.lang.Object", Vec::new()),
        };
        // The accessor's body decides its abstractness where the property's
        // modality does not: a `val`/`var` member of an `interface` is
        // `abstract` unless an accessor declares a body, while a `class`'s
        // property carries a body through the backing field of its initializer
        // ([`Shapes::member_flags`]).
        let mut has_body = self.declares_accessor_body(property, is_setter);
        if !is_setter {
            has_body =
                has_body || property.initializer_expr.is_some() || property.delegate_expr.is_some();
        }
        let (abstract_, is_final) = self.member_flags(property.modifiers, has_body);
        out.push(MethodData {
            name: name.to_owned(),
            owner: self.owner.clone(),
            owner_file: Some(self.file),
            decl_item: Some(item),
            params: if is_setter { vec![ty] } else { Vec::new() },
            param_names: None,
            // A setter returns `void`; a getter the property's type.
            ret: if is_setter { Ty::void(self.db) } else { ty },
            // `@get:Throws(…)`/`@set:Throws(…)` is the annotation's form on an
            // accessor — its targets are the function, the getter, the setter
            // and the constructor.
            throws: throws_of(
                self.db,
                resolver,
                &property.annotations,
                Some(match is_setter {
                    true => "set",
                    false => "get",
                }),
            ),
            varargs: false,
            is_static,
            abstract_,
            is_final,
            access: access(property.modifiers.visibility),
            declaring_package: self.package.clone(),
            declaring_top_level: Some(self.top_level.clone()),
            declaring_interface: self.is_interface(),
            type_params: Vec::new(),
            raw_erased: false,
            descriptor: None,
        });
    }

    /// The JVM field of a declaration, when the compiler emits one for it.
    /// `force_static` marks the field static whatever the kind of the
    /// classifier the walk belongs to — a companion's members, which the
    /// *enclosing* class carries as statics.
    ///
    /// Only the fields the *walked* classifier carries are answered: a nested
    /// `object`'s `INSTANCE` belongs to the object's own class, which
    /// [`java_view_fields`] answers it for, and a nested classifier of any
    /// other kind carries no field of the enclosing class.
    fn push_field(
        &self,
        resolver: &KotlinResolver<'_>,
        item: ItemId,
        name: &str,
        force_static: bool,
        out: &mut Vec<FieldData>,
    ) {
        match self.tree.data(item) {
            KotlinItemData::Property(property) => {
                // A companion object's class carries no field of its own: the
                // compiler places a companion's `const val`/`@JvmField` on the
                // *enclosing* class, whose walk answers them as its statics
                // (`javap -p` shows `Holder$Companion` holding accessors
                // alone).
                if self.kind() == Some(KotlinClassKind::CompanionObject) {
                    return;
                }
                let constant = property
                    .modifiers
                    .flags
                    .contains(KotlinModifierFlags::CONST);
                let jvm_field =
                    has_annotation(resolver, &property.annotations, JvmAnnotation::Field);
                if !constant && !jvm_field {
                    return;
                }
                // The field carries the *property's* own name: `@JvmName`
                // renames a function or a property *accessor*, never the
                // backing field — its targets are the function, the getter, the
                // setter and the file
                // (`kotlin.jvm.JvmName`'s declaration in kotlin-stdlib), so an
                // application on the property itself is a compiler error
                // (`this annotation is not applicable to target 'member property
                // with backing field'`, kotlinc 2.4.20) rather than a rename.
                let jvm_name = property.name.to_string();
                if jvm_name != name {
                    return;
                }
                let ty = match &property.ty {
                    Some(ty) => {
                        ty_from_kotlin(self.db, ty_from_type_ref(self.db, resolver, &ty.ty))
                    }
                    None => Ty::reference(self.db, "java.lang.Object", Vec::new()),
                };
                out.push(FieldData {
                    name: jvm_name,
                    owner: self.owner.clone(),
                    owner_file: Some(self.file),
                    decl_item: Some(item),
                    ty,
                    descriptor: None,
                    // A `const val`'s field is static wherever it stands, and
                    // so is an `@JvmField` of an `object` — the object's own
                    // class carries it (`object Obj { @JvmField val f = 1 }`
                    // compiles to `public static final int f` on `Obj`) — or of
                    // a file, whose facade carries it. A `@JvmField` of a
                    // *class* is an instance field, and a companion's is a
                    // static of the enclosing class, which its walk marks
                    // ([`Self::push_field`]'s `force_static`).
                    is_static: constant
                        || force_static
                        || self.class.is_none()
                        || self.kind() == Some(KotlinClassKind::Object),
                    access: access(property.modifiers.visibility),
                    is_final: constant || !property.is_var,
                    declaring_package: self.package.clone(),
                    declaring_top_level: Some(self.top_level.clone()),
                });
            }
            // A `companion object`: the field `Companion` on the enclosing
            // class holds it.
            KotlinItemData::Class(class) if class.kind == KotlinClassKind::CompanionObject => {
                if name != "Companion" {
                    return;
                }
                out.push(FieldData {
                    name: name.to_owned(),
                    owner: self.owner.clone(),
                    owner_file: Some(self.file),
                    decl_item: Some(item),
                    // The companion object's own class, by its canonical name.
                    ty: Ty::reference(
                        self.db,
                        hir::source_class_fqn(self.db, self.file, item)
                            .unwrap_or_else(|| class.name.clone()),
                        Vec::new(),
                    ),
                    descriptor: None,
                    is_static: true,
                    access: Access::Public,
                    is_final: true,
                    declaring_package: self.package.clone(),
                    declaring_top_level: Some(self.top_level.clone()),
                });
            }
            // An enum entry: a static field of the enum's own type.
            KotlinItemData::EnumEntry(entry) => {
                if entry.name.as_str() != name {
                    return;
                }
                out.push(FieldData {
                    name: entry.name.to_string(),
                    owner: self.owner.clone(),
                    owner_file: Some(self.file),
                    decl_item: Some(item),
                    ty: Ty::reference(self.db, self.enum_ty(), Vec::new()),
                    descriptor: None,
                    is_static: true,
                    access: Access::Public,
                    is_final: true,
                    declaring_package: self.package.clone(),
                    declaring_top_level: Some(self.top_level.clone()),
                });
            }
            _ => {}
        }
    }

    /// The type an enum entry has: the enum class itself.
    fn enum_ty(&self) -> Name {
        match &self.owner {
            ClassKey::Named(fqn) => fqn.clone(),
            ClassKey::Local(source) => Name::new(&format!("item{}", source.item.0.0)),
        }
    }
}

/// The compiler's parameterless constructor, as the declaration the item tree
/// carries none of: the `public <init>()` a class with no declared constructor
/// has implicitly, and the one a primary constructor of all-defaulted
/// parameters gains ([`Shapes::constructors`]).
///
/// It is anchored at the item the compiler's `<init>` belongs to — the class
/// for the implicit one, the primary constructor for the added one.
fn parameterless_constructor(modifiers: KotlinModifiers) -> ConstructorData {
    ConstructorData {
        params: Vec::new(),
        param_locals: Vec::new(),
        defaults: Vec::new(),
        modifiers,
        annotations: Vec::new(),
        delegation: None,
        body: None,
        ast: hir_expand::ast_id_map::FileAstId::placeholder(),
    }
}

/// The checked exceptions the `@Throws(…)` of an annotation list declares —
/// the exceptions the compiler writes into the classfile's `Exceptions`
/// attribute
/// (<https://kotlinlang.org/docs/java-interop.html#checked-exceptions>), which
/// is what a *Java* caller's §11.2 liability is computed from: Kotlin's own
/// exceptions are unchecked, so only the annotation imposes one.
///
/// `target` selects the application the declaration carries its exceptions on:
/// the `get`/`set` of an accessor (`@get:Throws(…)`, the annotation's targets
/// being the function, the getter, the setter and the constructor), and `None`
/// for a function's or a constructor's own application. Each exception is an
/// argument's class literal (`IOException::class`,
/// [`ItemAnnotationValue::ClassLit`]), resolved like any other written type.
fn throws_of(
    db: &dyn TyDatabase,
    resolver: &KotlinResolver<'_>,
    annotations: &[KotlinAnnotationRef],
    target: Option<&str>,
) -> Vec<Ty> {
    let mut out = Vec::new();
    for application in annotations {
        let applies = match target {
            Some(target) => application
                .target
                .as_ref()
                .is_some_and(|written| written.as_str() == target),
            None => application.target.is_none(),
        };
        if !applies || !is_jvm_annotation(resolver, application, JvmAnnotation::Throws) {
            continue;
        }
        for arg in &application.annotation.args {
            if let ItemAnnotationValue::ClassLit(ty) = &arg.value {
                out.push(ty_from_kotlin(db, ty_from_type_ref(db, resolver, &ty.ty)));
            }
        }
    }
    out
}

/// The parameter lists of the classfile methods a Kotlin declaration with
/// default values compiles to, the declared list first.
///
/// `@JvmOverloads` adds one method per parameter that declares a default value,
/// from the last such parameter to the first, each holding the parameters
/// *before* it plus the parameters after it that declare none
/// (<https://kotlinlang.org/docs/java-interop.html#overloads-generation>).
///
/// The rule is the compiler's, observed with kotlinc 2.4.20: `@JvmOverloads fun
/// f(a: String = "x", b: Int, c: Long = 1L)` compiles to `f(String, int, long)`,
/// `f(String, int)` and `f(int)` — the last taking `b` alone, which is *not* a
/// prefix of the declaration — and `@JvmOverloads fun g(a: Int, b: Int = 1)`
/// compiles to `g(int, int)` and `g(int)`. The annotation has the same effect on
/// a constructor, at every visibility (kotlinc warns that it has "no effect on
/// private declarations" and emits the overloads all the same).
///
/// `defaulted` holds one entry per entry of `params`, in parameter order.
fn overload_lists(params: &[Ty], defaulted: &[bool], jvm_overloads: bool) -> Vec<Vec<Ty>> {
    let mut out = vec![params.to_vec()];
    if !jvm_overloads {
        return out;
    }
    for index in (0..params.len()).rev().filter(|&index| defaulted[index]) {
        out.push(
            params
                .iter()
                .enumerate()
                .filter(|(position, _)| *position < index || !defaulted[*position])
                .map(|(_, ty)| *ty)
                .collect(),
        );
    }
    out
}

/// The JVM access of a Kotlin visibility: `internal` is `public` in the
/// classfile, which is why a Java caller can reach an internal declaration
/// (<https://kotlinlang.org/docs/java-interop.html#visibility>).
fn access(visibility: KotlinVisibility) -> Access {
    match visibility {
        KotlinVisibility::Public | KotlinVisibility::Internal => Access::Public,
        KotlinVisibility::Protected => Access::Protected,
        KotlinVisibility::Private => Access::Private,
    }
}

/// The JVM name of a property's getter: `get` + the capitalized name — except
/// for an `is`-prefixed property, whose getter keeps the name — unless
/// `@get:JvmName` renames it
/// (<https://kotlinlang.org/docs/java-interop.html#getters-and-setters>).
fn property_getter_name(resolver: &KotlinResolver<'_>, property: &PropertyData) -> Option<String> {
    if let Some(name) = targeted_name(resolver, &property.annotations, "get") {
        return Some(name);
    }
    let name = property.name.as_str();
    if is_prefixed(name) {
        return Some(name.to_owned());
    }
    Some(format!("get{}", capitalize(name)))
}

/// The JVM name of a property's setter: `set` + the capitalized name without a
/// leading `is`, unless `@set:JvmName` renames it.
fn property_setter_name(resolver: &KotlinResolver<'_>, property: &PropertyData) -> Option<String> {
    if let Some(name) = targeted_name(resolver, &property.annotations, "set") {
        return Some(name);
    }
    let name = property.name.as_str();
    let base = match is_prefixed(name) {
        true => &name[2..],
        false => name,
    };
    Some(format!("set{}", capitalize(base)))
}

/// Whether a property name is the `is`-prefixed form Kotlin keeps in the JVM
/// getter.
fn is_prefixed(name: &str) -> bool {
    name.strip_prefix("is")
        .is_some_and(|rest| rest.chars().next().is_some_and(char::is_uppercase))
}

fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
