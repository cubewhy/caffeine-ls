//! The JVM-visible shape of a Kotlin declaration, as the *Java* model the Java
//! layer consumes.
//!
//! A Java file reads a Kotlin declaration through the classfile the compiler
//! emits for it, so this module answers with the same
//! [`MethodData`]/[`FieldData`] shapes [`crate::java::method`] produces for a
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
//! | `const val x` / `@JvmField val x` | a field `x` |
//! | `object Util` | a static field `Util.INSTANCE` |
//! | `companion object` | a static field `Companion` on the enclosing class |
//! | `@JvmStatic` on a companion member | *also* a static of the enclosing class |
//! | a top-level `fun f()` / `val x` | a static member of the file's facade `FooKt` |
//! | `@JvmName("y")` / `@get:JvmName("y")` | the member is named `y` |
//! | `@file:JvmName("Y")` | the facade is named `Y` |
//! | `@JvmOverloads fun f(a: Int, b: Int = 0)` | one overload per trailing default |
//! | an `enum class` entry | a static field of the enum type |
//! | a constructor | `<init>` |
//!
//! Every type is *erased*: the classfile carries the erasure of a Kotlin type
//! (`T` is its bound, `List<Int>` is `java.util.List`), so the type arguments a
//! Kotlin receiver wrote never reach a Java caller — which is why nothing here
//! takes the receiver's arguments.
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
use hir_expand::body::Literal;
use hir_expand::ids::ItemId;
use hir_expand::name::Name;
use vfs::FileId;

use super::resolve::KotlinResolver;
use super::ty::{ty_from_kotlin, ty_from_type_ref};
use crate::java::db::TyDatabase;
use crate::java::method::{
    Access, ClassKey, FieldData, MethodData, MethodTypeParam, source_top_level,
};
use crate::ty::{Ty, TypeVarScope};

/// The JVM methods a Kotlin classifier declares under the JVM name `name` —
/// [`crate::java::method::source_class_methods`]'s twin for a Kotlin file.
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
/// `companion object`'s `Companion` field and an `enum class`'s entries.
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
    // (<https://kotlinlang.org/docs/java-interop.html#static-methods>).
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
        shapes.push_field(&resolver, member, name, &mut out);
    }
    out
}

/// The JVM facade class the compiler synthesizes for a Kotlin file's top-level
/// declarations: the file's stem with `Kt` appended — every character that is
/// not a Kotlin identifier spelled `_` — or the `@file:JvmName` the file writes
/// (<https://kotlinlang.org/docs/java-interop.html#package-level-functions>).
pub fn file_facade_class(db: &dyn TyDatabase, file: FileId) -> Option<Name> {
    let tree = hir::file_item_tree(db, file);
    let tree = hir_def::kotlin::plugin::model(&tree)?;
    if let Some(name) = tree.facade_class() {
        return Some(Name::new(&name));
    }
    let name = hir::file_name(db, file)?;
    let stem = name
        .strip_suffix(".kt")
        .or_else(|| name.strip_suffix(".kts"))?;
    let mut facade = String::with_capacity(stem.len() + 2);
    for ch in stem.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            facade.push(ch);
        } else {
            facade.push('_');
        }
    }
    facade.push_str("Kt");
    Some(Name::new(&facade))
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
        shapes.push_field(&resolver, top, name, &mut out);
    }
    // A facade field is static.
    for field in out.iter_mut() {
        field.is_static = true;
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

/// The fully qualified name of a Kotlin file's facade class.
fn facade_fqn(tree: &KotlinItemTree, db: &dyn TyDatabase, file: FileId) -> Option<Name> {
    let facade = file_facade_class(db, file)?;
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

/// The `@JvmName("x")` of an annotation list: the value the annotation's `name`
/// element carries ([`annotation_name`] reads the element values M2 lowered).
fn annotation_name(annotations: &[KotlinAnnotationRef]) -> Option<Name> {
    for application in annotations {
        if application.annotation.name.as_str() != "JvmName" {
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

/// The `@useSite:JvmName("x")` of an annotation list, if it writes one.
fn targeted_name(annotations: &[KotlinAnnotationRef], target: &str) -> Option<String> {
    for application in annotations {
        let applies = application
            .target
            .as_ref()
            .is_some_and(|written| written.as_str() == target);
        if applies && application.annotation.name.as_str() == "JvmName" {
            for arg in &application.annotation.args {
                if let ItemAnnotationValue::Literal(Literal::Str(value)) = &arg.value {
                    return Some(value.clone());
                }
            }
        }
    }
    None
}

/// Whether the annotation list writes `@name` (a declaration annotation, which
/// never carries a use-site target).
fn has_annotation(annotations: &[KotlinAnnotationRef], name: &str) -> bool {
    annotations
        .iter()
        .any(|application| application.annotation.name.as_str() == name)
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

    /// The JVM members of `item` whose JVM name is `name`. `only_static`
    /// restricts the walk to `@JvmStatic` members (the enclosing class's view
    /// of its companion's statics); `force_static` marks every member pushed as
    /// static (a file's facade).
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
                has_annotation(&function.annotations, "JvmStatic")
            }
            KotlinItemData::Property(property) => {
                has_annotation(&property.annotations, "JvmStatic")
            }
            _ => false,
        };
        if only_static && !jvm_static {
            return;
        }
        let is_static = force_static || jvm_static;
        match data {
            KotlinItemData::Function(function) => {
                let jvm = annotation_name(&function.annotations)
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| function.name.to_string());
                if jvm == name {
                    self.push_function(resolver, item, function, is_static, out);
                }
            }
            KotlinItemData::Property(property) => {
                // `@JvmField` suppresses the accessors: the property is a
                // field, which [`Shapes::push_field`] answers.
                if has_annotation(&property.annotations, "JvmField") {
                    return;
                }
                let is_static = is_static || self.kind() == Some(KotlinClassKind::Object);
                if let Some(getter) = property_getter_name(property)
                    && getter == name
                {
                    self.push_accessor(resolver, item, property, &getter, false, is_static, out);
                }
                if property.is_var
                    && let Some(setter) = property_setter_name(property)
                    && setter == name
                {
                    self.push_accessor(resolver, item, property, &setter, true, is_static, out);
                }
            }
            _ => {}
        }
    }

    /// The `<init>` methods of a Kotlin classifier: one per declared
    /// constructor, or the implicit `public <init>()` when it declares none.
    fn constructors(&self, item: ItemId, class: &ClassData, out: &mut Vec<MethodData>) {
        // The *primary* constructor lives in the class header, not in the body
        // ([KLS `declarations.html#primary-constructor`]).
        let mut declared = false;
        if let Some(primary) = class.primary_constructor
            && let KotlinItemData::Constructor(constructor) = self.tree.data(primary)
        {
            declared = true;
            let resolver = resolver_of(self.db, self.tree, self.file, primary);
            self.push_constructor(&resolver, primary, constructor, out);
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
            let empty = ConstructorData {
                params: Vec::new(),
                defaults: Vec::new(),
                modifiers: KotlinModifiers::none(),
                annotations: Vec::new(),
                delegation: None,
                body: None,
                ast: hir_expand::ast_id_map::FileAstId::placeholder(),
            };
            let resolver = resolver_of(self.db, self.tree, self.file, item);
            self.push_constructor(&resolver, item, &empty, out);
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
        let jvm_name = annotation_name(&function.annotations)
            .map(|name| name.to_string())
            .unwrap_or_else(|| function.name.to_string());
        let params: Vec<Ty> = function
            .params
            .iter()
            .map(|param| {
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
            })
            .collect();
        let ret = match &function.ret {
            Some(ret) => ty_from_kotlin(self.db, ty_from_type_ref(self.db, resolver, &ret.ty)),
            // An expression-bodied function without a written type, and a
            // `Unit`-returning one, compile to `void` — the compiler's
            // signature inference is what decides which (KLS
            // `type-inference.html#function-signature-type-inference`), and
            // until it lands the erased answer is `void`.
            None => Ty::reference(self.db, "void", Vec::new()),
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
        // `@JvmOverloads`: one further method per trailing default
        // (<https://kotlinlang.org/docs/java-interop.html#overloads-generation>).
        let overloads = if has_annotation(&function.annotations, "JvmOverloads") {
            function
                .defaults
                .iter()
                .rev()
                .take_while(|default| default.is_some())
                .count()
        } else {
            0
        };
        let shortest = params.len().saturating_sub(overloads);
        for arity in (shortest..=params.len()).rev() {
            out.push(MethodData {
                name: jvm_name.clone(),
                owner: self.owner.clone(),
                owner_file: Some(self.file),
                decl_item: Some(item),
                params: params[..arity].to_vec(),
                param_names: None,
                ret,
                throws: Vec::new(),
                varargs: varargs && arity == params.len(),
                is_static,
                abstract_: function.modifiers.modality == KotlinModality::Abstract,
                is_final: function.modifiers.modality == KotlinModality::Final,
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

    fn push_constructor(
        &self,
        resolver: &KotlinResolver<'_>,
        item: ItemId,
        constructor: &ConstructorData,
        out: &mut Vec<MethodData>,
    ) {
        out.push(MethodData {
            name: "<init>".to_owned(),
            owner: self.owner.clone(),
            owner_file: Some(self.file),
            decl_item: Some(item),
            params: constructor
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
                .collect(),
            param_names: None,
            ret: Ty::reference(self.db, "void", Vec::new()),
            throws: Vec::new(),
            varargs: constructor
                .params
                .last()
                .is_some_and(|param| param.param.varargs),
            is_static: false,
            abstract_: false,
            is_final: false,
            access: access(constructor.modifiers.visibility),
            declaring_package: self.package.clone(),
            declaring_top_level: Some(self.top_level.clone()),
            declaring_interface: false,
            type_params: Vec::new(),
            raw_erased: false,
            descriptor: None,
        });
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
        out.push(MethodData {
            name: name.to_owned(),
            owner: self.owner.clone(),
            owner_file: Some(self.file),
            decl_item: Some(item),
            params: if is_setter { vec![ty] } else { Vec::new() },
            param_names: None,
            // A setter returns `void`; a getter the property's type.
            ret: if is_setter {
                Ty::reference(self.db, "void", Vec::new())
            } else {
                ty
            },
            throws: Vec::new(),
            varargs: false,
            is_static,
            abstract_: property.modifiers.modality == KotlinModality::Abstract,
            is_final: property.modifiers.modality == KotlinModality::Final,
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
    fn push_field(
        &self,
        resolver: &KotlinResolver<'_>,
        item: ItemId,
        name: &str,
        out: &mut Vec<FieldData>,
    ) {
        match self.tree.data(item) {
            KotlinItemData::Property(property) => {
                let constant = property
                    .modifiers
                    .flags
                    .contains(KotlinModifierFlags::CONST);
                let jvm_field = has_annotation(&property.annotations, "JvmField");
                if !constant && !jvm_field {
                    return;
                }
                let jvm_name = annotation_name(&property.annotations)
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| property.name.to_string());
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
                    // A `const val` is a static field wherever it stands; an
                    // `@JvmField` of a file is a facade static too.
                    is_static: constant || self.class.is_none(),
                    access: access(property.modifiers.visibility),
                    is_final: constant || !property.is_var,
                    declaring_package: self.package.clone(),
                    declaring_top_level: Some(self.top_level.clone()),
                });
            }
            // `object Util`: the static field `INSTANCE` holding the object,
            // reached by a Java caller as `Util.INSTANCE`.
            KotlinItemData::Class(class) if class.kind == KotlinClassKind::Object => {
                if name != "INSTANCE" {
                    return;
                }
                out.push(FieldData {
                    name: name.to_owned(),
                    owner: self.owner.clone(),
                    owner_file: Some(self.file),
                    decl_item: Some(item),
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
fn property_getter_name(property: &PropertyData) -> Option<String> {
    if let Some(name) = targeted_name(&property.annotations, "get") {
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
fn property_setter_name(property: &PropertyData) -> Option<String> {
    if let Some(name) = targeted_name(&property.annotations, "set") {
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
