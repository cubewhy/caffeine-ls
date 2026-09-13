//! The annotation checks over a compilation unit ([JLS §9.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6)):
//!
//! - The `@Target` applicability check
//!   ([JLS §9.6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.1),
//!   [§9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4)):
//!   an annotation type declares the element types it may be applied to via
//!   `@Target`, so an annotation used on a declaration whose element type is not
//!   in that set is a compile-time error.
//! - The element-value argument check
//!   ([JLS §9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1)):
//!   every `name = value` pair of an annotation's argument list must name an
//!   element of the annotation type ([§9.6.1]) exactly once, and the value must
//!   be assignable to the element's declared type.
//!
//! The rules applied here:
//!
//! - A declaration `D` with element type `E` may carry an annotation `T` iff
//!   `T`'s target set contains `E` (or is empty, which makes `T` applicable to
//!   every declaration except type parameters and package declarations).
//! - An annotation written among the modifiers of a *variable* declaration
//!   (`@Ann int x;`, `void m(@Ann int p)`) — where §9.7.4 makes the same
//!   annotation plausibly a declaration annotation or a type annotation — is
//!   applicable iff `T`'s target contains the declaration's element type, or
//!   contains `TYPE_USE` and the declaration writes a type for the annotation
//!   to apply to. A `var` declaration ([§14.4], [§15.27.1]) writes none, so a
//!   `TYPE_USE`-only annotation is an error there.
//! - A *type annotation* `T` on a type — a type argument, an array dimension,
//!   a cast, a class literal, ... — is applicable iff `T`'s target contains
//!   `TYPE_USE` ([§9.6.4.1]: a type context admits nothing else, however the
//!   type relates to a declaration).
//! - An element value `V` is assignable to an element of declared type `T` by
//!   assignment conversion ([§5.2]), with the §9.7.1 array shorthand (a single
//!   non-initializer value against `T[]` is checked against `T`) and the
//!   constant-narrowing of an `int` literal to `byte`/`short`/`char`.
//!
//! The `@Target` argument list and the element-value pairs are read from the
//! annotation type's own declaration: from *source* via the lowered
//! [`AnnotationRef`]s (whose element values are the enum constants of
//! `java.lang.annotation.ElementType`), and from *library* classes via the
//! classfile `RuntimeVisibleAnnotations` and method-signature stubs
//! ([`hir::ClassRecord`]), so a `@Target(ElementType.X)` from a dependency jar
//! is honored the same way. The elements of a library annotation type are the
//! methods its classfile declares ([§9.6.1]); an annotation interface that
//! declares no methods declares no elements, so every pair of a normal
//! annotation of it is an error ([§9.7.1]) and none can be missing.

use hir_def::java::item_tree::{
    ItemAnnotationRef, ItemAnnotationValue, ItemData, ItemId, ItemTree, ItemTypeRef,
};
use hir_expand::{
    body::{BodyTree, ExprId, LocalId, PatternId, StmtId},
    name::Name,
    span::{AnnotationValue, SpannedTypeRef},
};
use rust_asm::constants::ACC_ENUM;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::stub::PrimitiveType;
use vfs::FileId;

use crate::java::annotation_value::{self, ValueCtx};
use crate::java::db::TyDatabase;
use crate::java::decl_check::{DeclDiagnostic, SafeVarargsRejection};
use crate::java::range_ctx::range_ctx;
use crate::java::resolve::{Resolver, candidate_fqns, resolve_type_ref, ty_from_library};
use crate::java::subtyping::is_assignable;
use crate::java::ty::{Ty, TyKind};
use hir_def::java::ranges;

/// The element types an annotation may be applied to on a *declaration*
/// ([JLS §9.6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.1)
/// Table 9.7-1).
fn element_type_of(data: &ItemData) -> Option<&'static str> {
    match data {
        ItemData::Class(_) | ItemData::Interface(_) | ItemData::Enum(_) | ItemData::Record(_) => {
            Some("TYPE")
        }
        // §9.6.4.1: an annotation type declaration has element type
        // `ANNOTATION_TYPE`; for historical reasons an annotation targeted to
        // `TYPE` is also applicable to it (handled by the caller).
        ItemData::Annotation(_) => Some("ANNOTATION_TYPE"),
        ItemData::Method(method) => Some(if method.is_constructor() {
            "CONSTRUCTOR"
        } else {
            "METHOD"
        }),
        ItemData::Field(_) | ItemData::EnumConstant(_) => Some("FIELD"),
        ItemData::Module(_) => Some("MODULE"),
        ItemData::StaticInit(_) | ItemData::InstanceInit(_) => None,
    }
}

/// The annotation diagnostics of every annotation in `file`, declaration and
/// type-use alike, in source order: the `@Target` applicability checks
/// ([JLS §9.6.4.1], [§9.7.4]) — over the declarations, the variable
/// declarations a body introduces and every written type — and the
/// element-value argument checks ([§9.7.1]).
pub(crate) fn annotation_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &ItemTree,
) -> Vec<DeclDiagnostic> {
    let scope = crate::java::resolve::scope_for_file(db, file);
    let Some((map, source)) = range_ctx(db, file, tree.language) else {
        return Vec::new();
    };
    let type_params = crate::java::db::type_params_map_query(db, db.file_text(file));
    let bodies = hir::file_body_tree(db, file);
    let mut out = Vec::new();

    fn walk(
        db: &dyn TyDatabase,
        file: FileId,
        tree: &ItemTree,
        bodies: &BodyTree,
        scope: &hir::ResolutionScope,
        map: &hir_expand::ast_id_map::AstIdMap,
        source: &syntax::SourceFile,
        type_params: &FxHashMap<ItemId, Vec<crate::java::resolve::ScopedTypeParam>>,
        id: ItemId,
        out: &mut Vec<DeclDiagnostic>,
    ) {
        let data = tree.data(id);
        // The annotation names resolve like any type name in the item's scope
        // ([JLS §6.5.5.1]); the resolver is built once per item and shared by
        // the declaration and type-use checks of its annotations. The element
        // values of those annotations were lowered with this item as their
        // owner, so the same context serves their §15.29 checks.
        let resolver = Resolver::new(tree, type_params, id);
        let cx = ValueCtx {
            db,
            file,
            item: id,
            scope,
            resolver: &resolver,
            bodies,
        };
        // §9.6.4.1: the declaration annotations of the item itself.
        for annotation in declaration_annotations(data) {
            check_declaration_annotation(&cx, data, annotation, map, source, out);
        }
        // §9.6.4.1/[§9.7.4]: the annotations of a record's *components* have
        // element type `RECORD_COMPONENT` (Table 9.7-1); like a field, a
        // component's type gives the type-use fallback a target.
        if let ItemData::Record(record) = data {
            for component in &record.components {
                for annotation in &component.annotations {
                    check_target(
                        &cx,
                        &["RECORD_COMPONENT"],
                        true,
                        annotation,
                        map,
                        source,
                        out,
                    );
                }
            }
        }
        // §9.6.4.1/[§9.7.4]: the declaration annotations of a method's or
        // constructor's formal parameters (`void m(@A int p)`). Their element
        // type is `PARAMETER` (Table 9.7-1); the written type gives a
        // `TYPE_USE`-only annotation the parameter's type to apply to, and a
        // formal parameter always writes one (`var` is not a legal parameter
        // type, [§8.4.1]).
        if let ItemData::Method(method) = data {
            for param in &method.sig.params {
                for annotation in &param.annotations {
                    check_variable_annotation(&cx, annotation, "PARAMETER", true, map, source, out);
                }
            }
        }
        // §9.6.4.1/[§9.7.4]: the type-use annotations on the item's type
        // references (field types, method signatures, record component types,
        // superclass/interfaces, type-parameter bounds). Every one of these
        // is a *type* position — a type argument, an array dimension, a
        // qualified-segment annotation — so the annotation must be applicable
        // in type contexts ([§9.6.4.1]).
        for tyref in declaration_type_refs(data) {
            check_type_use_item(&cx, tyref, map, source, out);
        }
        // The body-side annotations: the declaration annotations of every
        // variable the body declares (locals, enhanced-for variables,
        // resources, exception parameters, pattern bindings and lambda
        // parameters) and the type annotations of every type it writes
        // (casts, `new`, `instanceof`, class literals, method-reference type
        // names, lambda parameter types and local variable types).
        if let Some(body) = body_of(tree, id) {
            check_body_annotations(&cx, bodies.bodies.get(body.0), out);
        }
        for &child in data.body() {
            walk(
                db,
                file,
                tree,
                bodies,
                scope,
                map,
                source,
                type_params,
                child,
                out,
            );
        }
    }

    for &top in &tree.top {
        walk(
            db,
            file,
            tree,
            &bodies,
            &scope,
            map,
            &source,
            type_params,
            top,
            &mut out,
        );
    }
    out
}

/// JLS §9.6.4.7: `@SafeVarargs` may annotate only a method or constructor that
/// is *variable arity* and is `static`, `final` or `private` — the cases in
/// which the declaration cannot be overridden with a different arity and
/// therefore cannot produce the heap pollution the annotation promises to
/// suppress ([§8.4.1]). javac: `Invalid SafeVarargs annotation. …`.
fn check_safe_varargs(
    data: &ItemData,
    annotation_range: Option<rowan::TextRange>,
    out: &mut Vec<DeclDiagnostic>,
) {
    let ItemData::Method(method) = data else {
        out.push(DeclDiagnostic::InvalidSafeVarargs {
            reason: SafeVarargsRejection::NotAMethod,
            range: annotation_range,
        });
        return;
    };
    // JLS §9.6.4.7: the requirement is variable arity first — a *varargs*
    // constructor is a legal target (javac accepts `@SafeVarargs P(T... a)`),
    // while a non-varargs one is rejected as "not a varargs method".
    if !method.sig.params.last().is_some_and(|param| param.varargs) {
        out.push(DeclDiagnostic::InvalidSafeVarargs {
            reason: SafeVarargsRejection::NotVarargs,
            range: annotation_range,
        });
        return;
    }
    if method.is_constructor() {
        return;
    }
    let modifiers = &method.modifiers;
    if !(modifiers.is_static() || modifiers.is_final() || modifiers.is_private()) {
        out.push(DeclDiagnostic::InvalidSafeVarargs {
            reason: SafeVarargsRejection::Instance,
            range: annotation_range,
        });
    }
}

/// JLS §9.6.4.9: `@FunctionalInterface` may annotate only an interface, and
/// that interface must declare exactly one abstract method — excluding
/// `Object`'s public methods ([§9.8]) and `default`/`static` members, which are
/// not abstract. javac: `Unexpected @FunctionalInterface annotation`.
fn check_functional_interface(
    cx: &ValueCtx<'_>,
    data: &ItemData,
    annotation_range: Option<rowan::TextRange>,
    out: &mut Vec<DeclDiagnostic>,
) {
    let (db, file, item, scope) = (cx.db, cx.file, cx.item, cx.scope);
    let ItemData::Interface(_) = data else {
        out.push(DeclDiagnostic::NotAFunctionalInterfaceAnnotation {
            range: annotation_range,
        });
        return;
    };
    // §9.8: the interface's abstract methods are *the members of `F`* — its
    // own declaration's plus every superinterface's, deduped by signature
    // ([§9.4.1]), since a functional interface may inherit its single abstract
    // method rather than declare it. The member set is the one the override
    // checks use: a declaration-level enumeration of the interface's own
    // hierarchies.
    let Some(fqn) = hir::source_class_fqn(db, file, item) else {
        return;
    };
    let class_ty = Ty::reference(db, Name::new(fqn.as_str()), Vec::new());
    let ctx = crate::java::method::access_context(db, file, item);
    let abstract_count = crate::java::method::all_methods(db, scope, &class_ty, &ctx)
        .into_iter()
        .filter(|method| method.abstract_)
        .filter(|method| !is_object_method_override(&Name::new(&method.name)))
        .count();
    if abstract_count != 1 {
        out.push(DeclDiagnostic::NotAFunctionalInterfaceAnnotation {
            range: annotation_range,
        });
    }
}

/// Whether `name` is one of `java.lang.Object`'s public methods, which
/// [JLS §9.8] excludes from the abstract-method count of a functional
/// interface.
fn is_object_method_override(name: &Name) -> bool {
    matches!(
        name.as_str(),
        "equals" | "hashCode" | "toString" | "clone" | "finalize"
    )
}

/// The declaration annotations of an item, in source order.
pub(crate) fn declaration_annotations(data: &ItemData) -> Vec<&ItemAnnotationRef> {
    match data {
        ItemData::Class(d) | ItemData::Interface(d) => d.annotations.iter().collect(),
        ItemData::Enum(d) => d.annotations.iter().collect(),
        ItemData::Record(d) => d.annotations.iter().collect(),
        ItemData::Annotation(d) => d.annotations.iter().collect(),
        ItemData::Method(d) => d.annotations.iter().collect(),
        ItemData::Field(d) => d.annotations.iter().collect(),
        ItemData::Module(d) => d.annotations.iter().collect(),
        _ => Vec::new(),
    }
}

/// The type references of an item's *declaration* ([JLS §9.7.4]): the types
/// that carry type-use annotations.
pub(crate) fn declaration_type_refs(data: &ItemData) -> Vec<&ItemTypeRef> {
    let mut out = Vec::new();
    match data {
        ItemData::Class(d) | ItemData::Interface(d) => {
            if let Some(super_class) = &d.super_class {
                out.push(super_class);
            }
            out.extend(d.interfaces.iter());
            for param in &d.type_params {
                out.extend(param.bounds.iter());
            }
        }
        ItemData::Enum(d) => out.extend(d.interfaces.iter()),
        ItemData::Record(d) => {
            out.extend(d.interfaces.iter());
            for param in &d.type_params {
                out.extend(param.bounds.iter());
            }
            for component in &d.components {
                out.push(&component.ty);
            }
        }
        ItemData::Method(d) => {
            for param in &d.sig.type_params {
                out.extend(param.bounds.iter());
            }
            for param in &d.sig.params {
                out.push(&param.ty);
            }
            if let Some(ret) = &d.sig.ret {
                out.push(ret);
            }
            out.extend(d.sig.throws.iter());
        }
        ItemData::Field(d) => out.push(&d.ty),
        _ => {}
    }
    out
}

/// The body id of an item, when it declares one — a method or constructor
/// body. A field's initializer, an enum constant's arguments and an
/// annotation element's default are *expression forests* rather than bodies,
/// and the type annotations of the types they write (`(@A X) e`, `new @A X()`)
/// are not walked yet.
fn body_of(tree: &ItemTree, id: ItemId) -> Option<hir_expand::body::BodyId> {
    match tree.data(id) {
        ItemData::Method(method) => method.body(),
        ItemData::Field(_) | ItemData::EnumConstant(_) => None,
        _ => None,
    }
}

/// Checks the declaration annotations of one item against its element type
/// ([JLS §9.6.4.1]).
fn check_declaration_annotation(
    cx: &ValueCtx<'_>,
    data: &ItemData,
    annotation: &ItemAnnotationRef,
    map: &hir_expand::ast_id_map::AstIdMap,
    source: &syntax::SourceFile,
    out: &mut Vec<DeclDiagnostic>,
) {
    // §9.6.4.7/§9.6.4.9: two annotations carry a *well-formedness* requirement
    // on the declaration they annotate, beyond the `@Target` applicability of
    // §9.6.4.1 — `@SafeVarargs` on a method that cannot suppress heap
    // pollution, and `@FunctionalInterface` on something that is not one.
    let annotation_range = hir_def::java::ranges::annotation_name_range(map, source, annotation);
    if annotation.name.as_str() == "SafeVarargs" {
        check_safe_varargs(data, annotation_range, out);
    }
    if annotation.name.as_str() == "FunctionalInterface" {
        check_functional_interface(cx, data, annotation_range, out);
    }
    let Some(element_type) = element_type_of(data) else {
        return;
    };
    // §9.6.4.1: an annotation type declaration accepts both `ANNOTATION_TYPE`
    // and, for historical reasons, `TYPE`.
    let element_types: &[&str] = if element_type == "ANNOTATION_TYPE" {
        &["ANNOTATION_TYPE", "TYPE"]
    } else {
        std::slice::from_ref(&element_type)
    };
    let has_annotatable_type = has_annotatable_type(data);
    check_target(
        cx,
        element_types,
        has_annotatable_type,
        annotation,
        map,
        source,
        out,
    );
}

/// The shared applicability check of an annotation written in a declaration
/// position ([JLS §9.6.4.1], [§9.7.4]): it is applicable when its `@Target`
/// contains one of the declaration's element types (`element_types`), or — for
/// a declaration with an annotatable type — contains `TYPE_USE`, in which case
/// the annotation is a *type annotation* on that type.
fn check_target(
    cx: &ValueCtx<'_>,
    element_types: &[&'static str],
    has_annotatable_type: bool,
    annotation: &ItemAnnotationRef,
    map: &hir_expand::ast_id_map::AstIdMap,
    source: &syntax::SourceFile,
    out: &mut Vec<DeclDiagnostic>,
) {
    let (db, resolver, scope) = (cx.db, cx.resolver, cx.scope);
    // §9.7.1: the element-value arguments are checked against the annotation
    // type's elements regardless of whether the target check passes.
    check_annotation_elements(cx, annotation, map, source, out);
    let Some(targets) = resolve_annotation_type(db, resolver, scope, &annotation.name) else {
        // An unresolvable annotation type (or one without `@Target`) has no
        // target to enforce: empty `@Target` is applicable to every
        // declaration ([§9.6.4.1]).
        return;
    };
    if element_types.iter().any(|et| targets_contain(&targets, et)) {
        return;
    }
    // §9.7.4: an annotation written before the *type* of a declaration (a
    // field's type, a method's return type, a record component's type) that is
    // not applicable to the declaration itself is a **type annotation** on
    // that type — legal iff the annotation's target contains `TYPE_USE`. (A
    // class/enum/interface/annotation/module/constructor has no type for the
    // annotation to attach to, so no fallback applies there.)
    if targets_contain(&targets, "TYPE_USE") && has_annotatable_type {
        return;
    }
    out.push(DeclDiagnostic::AnnotationNotApplicable {
        name: annotation.name.clone(),
        element_type: element_types[0],
        range: ranges::annotation_name_range(map, source, annotation),
    });
}

/// The applicability core of a *variable declaration*'s annotation
/// ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4),
/// [§9.6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.1)):
/// an annotation written among the modifiers of a variable declaration
/// (`@Ann int x;`, `void m(@Ann int p)`, `(x instanceof @Ann String s)`) is
/// applicable when
///
/// - its `@Target` contains the declaration's element type (`element_type` —
///   `LOCAL_VARIABLE` for a local, enhanced-for, resource or pattern
///   variable; `PARAMETER` for a formal, exception or lambda parameter
///   [§9.6.4.1] Table 9.7-1), in which case it is a *declaration*
///   annotation; or
/// - its `@Target` contains `TYPE_USE` **and** the declaration writes a type,
///   in which case it is a *type annotation* on that type (§9.7.4: the type
///   closest to the annotation is the written type, the element type of an
///   array type).
///
/// A `var` declaration ([§14.4], [§15.27.1]) writes no type, so the second
/// case has no closest type to attach to and is a compile-time error
/// ([§9.7.4]).
fn check_variable_target(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    scope: &hir::ResolutionScope,
    name: &Name,
    range: Option<rowan::TextRange>,
    element_type: &'static str,
    has_written_type: bool,
    out: &mut Vec<DeclDiagnostic>,
) {
    let Some(targets) = resolve_annotation_type(db, resolver, scope, name) else {
        // An unresolvable annotation type (or one without `@Target`) has no
        // target to enforce: empty `@Target` is applicable to every
        // declaration ([§9.6.4.1]).
        return;
    };
    if targets_contain(&targets, element_type) {
        return;
    }
    if targets_contain(&targets, "TYPE_USE") {
        if has_written_type {
            return;
        }
        out.push(DeclDiagnostic::AnnotatedVar {
            name: name.clone(),
            range,
        });
        return;
    }
    out.push(DeclDiagnostic::AnnotationNotApplicable {
        name: name.clone(),
        element_type,
        range,
    });
}

/// Whether an annotation is applicable in a *type context* ([§9.6.4.1],
/// [§9.7.4]): every type context — a type argument, an array dimension, a
/// cast, a class literal, ... — requires an annotation whose `@Target`
/// contains `TYPE_USE`. Unlike a declaration position, no other element type
/// makes the annotation applicable: the annotated type may belong to a
/// declaration, but the annotation applies to the *type*, not to the
/// declaration ([§9.6.4.1]).
fn type_context_applicable(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    scope: &hir::ResolutionScope,
    name: &Name,
) -> bool {
    let Some(targets) = resolve_annotation_type(db, resolver, scope, name) else {
        // An unresolvable annotation type (or one without `@Target`) has no
        // target to enforce.
        return true;
    };
    targets_contain(&targets, "TYPE_USE")
}

/// The variable-declaration applicability check over an *item* annotation —
/// a method's or constructor's formal parameter
/// ([`hir_def::java::item_tree::Param::annotations`]).
fn check_variable_annotation(
    cx: &ValueCtx<'_>,
    annotation: &ItemAnnotationRef,
    element_type: &'static str,
    has_written_type: bool,
    map: &hir_expand::ast_id_map::AstIdMap,
    source: &syntax::SourceFile,
    out: &mut Vec<DeclDiagnostic>,
) {
    // §9.7.1: the element-value arguments are checked regardless of whether
    // the target check passes.
    check_annotation_elements(cx, annotation, map, source, out);
    check_variable_target(
        cx.db,
        cx.resolver,
        cx.scope,
        &annotation.name,
        ranges::annotation_name_range(map, source, annotation),
        element_type,
        has_written_type,
        out,
    );
}

/// The variable-declaration applicability check over a *span* annotation —
/// every body-side variable (a local, enhanced-for variable, resource,
/// exception parameter, pattern binding or lambda parameter), whose ranges
/// are still carried.
fn check_variable_annotation_ranged(
    cx: &ValueCtx<'_>,
    annotation: &hir_expand::span::AnnotationRef,
    element_type: &'static str,
    has_written_type: bool,
    out: &mut Vec<DeclDiagnostic>,
) {
    // §9.7.1: the element-value arguments are checked regardless of whether
    // the target check passes.
    check_annotation_elements_ranged(cx, annotation, out);
    check_variable_target(
        cx.db,
        cx.resolver,
        cx.scope,
        &annotation.name.name,
        annotation.name.range,
        element_type,
        has_written_type,
        out,
    );
}

/// Whether a declaration carries a type that an annotation written before it
/// may attach to as a type annotation ([§9.7.4]): the field's type, the
/// method's return type. A *type* declaration — a class, interface, enum,
/// record or annotation type — also names a type: a `TYPE_USE`-only
/// annotation written before it (`@Unmodifiable class C`) annotates the
/// declared type itself and is legal (§9.7.4: the declaration of a class or
/// interface is a type context). (A record *component* has its own type,
/// checked separately; a module or constructor has no type.)
fn has_annotatable_type(data: &ItemData) -> bool {
    matches!(
        data,
        ItemData::Field(_)
            | ItemData::Method(_)
            | ItemData::Class(_)
            | ItemData::Interface(_)
            | ItemData::Enum(_)
            | ItemData::Record(_)
            | ItemData::Annotation(_)
    )
}

/// Checks the type-use annotations of one *item* type reference
/// ([JLS §9.7.4], [§9.6.4.1]) — the declaration-side, range-free form.
fn check_type_use_item(
    cx: &ValueCtx<'_>,
    tyref: &ItemTypeRef,
    map: &hir_expand::ast_id_map::AstIdMap,
    source: &syntax::SourceFile,
    out: &mut Vec<DeclDiagnostic>,
) {
    for annotation in &tyref.type_use_annotations {
        check_type_use_annotation(cx, annotation, map, source, out);
    }
}

/// The shared type-use applicability check over an *item* annotation: every
/// type context requires an annotation applicable in type contexts, i.e. one
/// whose `@Target` contains `TYPE_USE` ([§9.6.4.1], [§9.7.4]).
fn check_type_use_annotation(
    cx: &ValueCtx<'_>,
    annotation: &ItemAnnotationRef,
    map: &hir_expand::ast_id_map::AstIdMap,
    source: &syntax::SourceFile,
    out: &mut Vec<DeclDiagnostic>,
) {
    // §9.7.1: the element-value arguments are checked regardless of whether
    // the target check passes.
    check_annotation_elements(cx, annotation, map, source, out);
    if !type_context_applicable(cx.db, cx.resolver, cx.scope, &annotation.name) {
        out.push(DeclDiagnostic::AnnotationNotApplicableToType {
            name: annotation.name.clone(),
            range: ranges::annotation_name_range(map, source, annotation),
        });
    }
}

/// The shared type-use applicability check over a *span* (`&AnnotationRef`,
/// the body path — its ranges are still carried).
fn check_type_use_annotation_ranged(
    cx: &ValueCtx<'_>,
    annotation: &hir_expand::span::AnnotationRef,
    out: &mut Vec<DeclDiagnostic>,
) {
    // §9.7.1: the element-value arguments are checked regardless of whether
    // the target check passes.
    check_annotation_elements_ranged(cx, annotation, out);
    if !type_context_applicable(cx.db, cx.resolver, cx.scope, &annotation.name.name) {
        out.push(DeclDiagnostic::AnnotationNotApplicableToType {
            name: annotation.name.name.clone(),
            range: annotation.name.range,
        });
    }
}

/// The §9.7.1 element-value argument check over a *span* (the body path). The
/// item path runs [`check_annotation_elements`].
fn check_annotation_elements_ranged(
    cx: &ValueCtx<'_>,
    annotation: &hir_expand::span::AnnotationRef,
    out: &mut Vec<DeclDiagnostic>,
) {
    let (db, scope, resolver) = (cx.db, cx.scope, cx.resolver);
    // §9.7.1: the same missing-element rule as [`check_annotation_elements`],
    // over the body path's spanned annotation — its name already carries the
    // range the report is anchored at.
    let Some(annotation_type) = annotation_type(db, scope, resolver, &annotation.name.name) else {
        return;
    };
    let elements = &annotation_type.elements;
    let missing: Vec<Name> = elements
        .iter()
        .filter(|element| !element.has_default)
        .filter(|element| !annotation.args.iter().any(|arg| arg.name == element.name))
        .map(|element| element.name.clone())
        .collect();
    if !missing.is_empty() {
        out.push(DeclDiagnostic::MissingAnnotationElement {
            names: missing,
            range: annotation.name.range,
        });
    }
    if annotation.args.is_empty() {
        return;
    }
    for (idx, arg) in annotation.args.iter().enumerate() {
        if annotation.args[..idx]
            .iter()
            .any(|prev| prev.name == arg.name)
        {
            out.push(DeclDiagnostic::DuplicateAnnotationMemberValue {
                name: arg.name.clone(),
                range: Some(arg.range),
            });
            continue;
        }
        let Some(element) = elements.iter().find(|element| element.name == arg.name) else {
            out.push(no_such_element(
                cx,
                &annotation_type.ty,
                &arg.name,
                Some(arg.range),
            ));
            continue;
        };
        check_value_commensurate_ranged(cx, &arg.value, &element.ty, arg.range, out);
    }
}

/// Walks one body for the annotations it contains ([JLS §9.7.4]): the
/// declaration annotations of every variable declaration in it — locals,
/// enhanced-for variables, resources, exception parameters, pattern bindings
/// and lambda parameters — and the type annotations of every type it writes —
/// casts, `new`, `instanceof`, class literals, method-reference type names,
/// explicit invocation type arguments, lambda parameter types and local
/// variable types.
///
/// The *method's* own parameters are absent here: their declaration
/// annotations are lowered with the signature
/// ([`hir_def::java::item_tree::Param::annotations`]) and their types are
/// declaration type references, so the item walk covers both (checking them
/// again here would report every one of them twice).
/// The annotation walk of one body: the state a single pass needs, plus the
/// *annotation occurrences* it has already checked.
///
/// A declaration's modifier list is shared by every variable it declares
/// (`@Ann int a = 1, b = 2;`) and a written type is copied to each of them,
/// so the same source occurrence reaches the walk more than once. javac
/// reports one diagnostic per *written* annotation, so an occurrence — the
/// annotation name's source range — is the unit checked, and a body is the
/// unit it is checked in.
struct BodyAnnotations<'a> {
    cx: &'a ValueCtx<'a>,
    /// The source ranges of the annotation occurrences already checked.
    checked: FxHashSet<rowan::TextRange>,
}

/// Walks one body for the annotations it contains ([JLS §9.7.4]): the
/// declaration annotations of every variable declaration in it — locals,
/// enhanced-for variables, resources, exception parameters, pattern bindings
/// and lambda parameters — and the type annotations of every type it writes —
/// casts, `new`, `instanceof`, class literals, method-reference type names,
/// explicit invocation type arguments, lambda parameter types and local
/// variable types.
///
/// The *method's* own parameters are absent here: their declaration
/// annotations are lowered with the signature
/// ([`hir_def::java::item_tree::Param::annotations`]) and their types are
/// declaration type references, so the item walk covers both (checking them
/// again here would report every one of them twice).
///
/// The context is the *body owner's*: the body's variable declarations were
/// lowered with the owning item, so their element values carry the same owner
/// as the item's own annotations.
fn check_body_annotations(
    cx: &ValueCtx<'_>,
    body: &hir_expand::body::Body,
    out: &mut Vec<DeclDiagnostic>,
) {
    let mut walk = BodyAnnotations {
        cx,
        checked: FxHashSet::default(),
    };
    for &stmt in &body.stmts {
        walk.stmt(stmt, out);
    }
}

impl BodyAnnotations<'_> {
    /// Checks the declaration annotations of one body-side variable binding
    /// ([JLS §9.7.4]): each is applicable iff its `@Target` contains the
    /// declaration's element type (`element_type`), or — when the
    /// declaration writes a type — `TYPE_USE`, in which case the annotation
    /// applies to that type. A `var` declaration writes no type ([§14.4],
    /// [§15.27.1]), so a `TYPE_USE`-only annotation has no closest type to
    /// apply to there.
    fn binding(
        &mut self,
        local: LocalId,
        element_type: &'static str,
        out: &mut Vec<DeclDiagnostic>,
    ) {
        let binding = self.cx.bodies.local(local);
        let has_written_type = binding.ty.is_some();
        for annotation in &binding.annotations {
            self.annotation(annotation, element_type, has_written_type, out);
        }
    }

    /// Checks one annotation occurrence, once (see [`Self::mark_checked`]).
    fn annotation(
        &mut self,
        annotation: &hir_expand::span::AnnotationRef,
        element_type: &'static str,
        has_written_type: bool,
        out: &mut Vec<DeclDiagnostic>,
    ) {
        if !self.mark_checked(annotation.name.range) {
            return;
        }
        check_variable_annotation_ranged(self.cx, annotation, element_type, has_written_type, out);
    }

    /// Checks the type annotations of one written type, each occurrence once
    /// (see [`Self::mark_checked`]).
    fn type_use(&mut self, spanned: &SpannedTypeRef, out: &mut Vec<DeclDiagnostic>) {
        for annotation in &spanned.type_use_annotations {
            if !self.mark_checked(annotation.name.range) {
                continue;
            }
            check_type_use_annotation_ranged(self.cx, annotation, out);
        }
    }

    /// Marks one annotation occurrence as checked, returning whether it is
    /// new. An occurrence with no source range cannot be identified, so it is
    /// always checked.
    fn mark_checked(&mut self, range: Option<rowan::TextRange>) -> bool {
        match range {
            Some(range) => self.checked.insert(range),
            None => true,
        }
    }

    /// Recurses over a statement's expressions and variable declarations,
    /// checking each type reference and each declaration annotation.
    fn stmt(&mut self, stmt: StmtId, out: &mut Vec<DeclDiagnostic>) {
        use hir_expand::body::{StmtData as S, SwitchLabel as L};
        match self.cx.bodies.stmt(stmt).clone() {
            S::Decl { local, initializer } => {
                // §9.7.4/[§9.6.4.1]: a local variable declaration's annotation
                // modifiers (`@Ann int x = ...`), element type `LOCAL_VARIABLE`.
                self.binding(local, "LOCAL_VARIABLE", out);
                let binding = self.cx.bodies.local(local);
                if let Some(ty) = &binding.ty {
                    self.type_use(ty, out);
                }
                if let Some(expr) = initializer {
                    self.expr(expr, out);
                }
            }
            S::DeclGroup(stmts) => {
                for stmt in stmts {
                    self.stmt(stmt, out);
                }
            }
            S::Block(stmts) => {
                for stmt in stmts {
                    self.stmt(stmt, out);
                }
            }
            S::Expr(expr) => {
                self.expr(expr, out);
            }
            S::Labeled { stmt, .. } => {
                self.stmt(stmt, out);
            }
            S::If { cond, then, els } => {
                self.expr(cond, out);
                self.stmt(then, out);
                if let Some(els) = els {
                    self.stmt(els, out);
                }
            }
            S::While { cond, body, .. } => {
                self.expr(cond, out);
                self.stmt(body, out);
            }
            S::DoWhile { body, cond } => {
                self.stmt(body, out);
                self.expr(cond, out);
            }
            S::For {
                init,
                cond,
                step,
                body,
            } => {
                for stmt in init {
                    self.stmt(stmt, out);
                }
                if let Some(cond) = cond {
                    self.expr(cond, out);
                }
                for expr in step {
                    self.expr(expr, out);
                }
                self.stmt(body, out);
            }
            S::ForEach {
                var,
                iterable,
                body,
            } => {
                // §14.14.2: the loop variable is a local variable declaration
                // ([§9.6.4.1]: element type `LOCAL_VARIABLE`).
                self.binding(var, "LOCAL_VARIABLE", out);
                if let Some(ty) = &self.cx.bodies.local(var).ty {
                    self.type_use(ty, out);
                }
                self.expr(iterable, out);
                self.stmt(body, out);
            }
            S::Switch { scrutinee, arms } => {
                self.expr(scrutinee, out);
                for arm in arms {
                    for label in &arm.labels {
                        match label {
                            L::Expr(expr) | L::Guard(expr) => {
                                self.expr(*expr, out);
                            }
                            // §14.30.2/§14.30.3: a `case` label's pattern — its
                            // type reference carries type annotations and its
                            // binding carries declaration annotations, exactly
                            // like an `instanceof` pattern.
                            L::Pattern(pattern) => {
                                self.pattern(*pattern, out);
                            }
                        }
                    }
                    for stmt in arm.body {
                        self.stmt(stmt, out);
                    }
                }
            }
            S::Return(expr) => {
                if let Some(expr) = expr {
                    self.expr(expr, out);
                }
            }
            S::Yield(expr) => {
                self.expr(expr, out);
            }
            S::Throw(expr) | S::Synchronized { expr, .. } => {
                self.expr(expr, out);
            }
            S::Try {
                resources,
                body,
                catches,
                finally,
            } => {
                for resource in resources {
                    // §9.6.4.1: a resource variable is a local variable
                    // declaration ([§14.20.3]).
                    self.binding(resource.local, "LOCAL_VARIABLE", out);
                    if let Some(ty) = &self.cx.bodies.local(resource.local).ty {
                        self.type_use(ty, out);
                    }
                    if let Some(initializer) = resource.initializer {
                        self.expr(initializer, out);
                    }
                }
                self.stmt(body, out);
                for catch in catches {
                    // §9.6.4.1: "Formal and exception parameter declarations"
                    // ([§8.4.1], [§9.4], [§14.20]) — element type `PARAMETER`.
                    self.binding(catch.param, "PARAMETER", out);
                    for ty in &catch.param_types {
                        self.type_use(ty, out);
                    }
                    self.stmt(catch.body, out);
                }
                if let Some(finally) = finally {
                    self.stmt(finally, out);
                }
            }
            S::Assert { cond, msg } => {
                self.expr(cond, out);
                if let Some(msg) = msg {
                    self.expr(msg, out);
                }
            }
            // [JLS §14.3]: a local declaration's own annotations are checked with
            // its declaration item, and its members' bodies are bodies of their own.
            S::Empty | S::Break(_) | S::Continue(_) | S::LocalClass { .. } | S::Missing => {}
        }
    }

    /// Checks the type references of one expression node, then recurses into
    /// its children ([JLS §9.7.4] type-use contexts).
    fn expr(&mut self, expr: ExprId, out: &mut Vec<DeclDiagnostic>) {
        use hir_expand::body::{ExprData as E, LambdaBody as L, SwitchLabel as SL};
        match self.cx.bodies.expr(expr).clone() {
            E::Cast { ty, expr: inner } => {
                self.type_use(&ty, out);
                self.expr(inner, out);
            }
            E::New {
                ty, args, receiver, ..
            } => {
                self.type_use(&ty, out);
                for arg in args {
                    self.expr(arg, out);
                }
                if let Some(receiver) = receiver {
                    self.expr(receiver, out);
                }
            }
            E::InstanceOf {
                expr: inner,
                ty: Some(ty),
                pattern,
            } => {
                self.expr(inner, out);
                self.type_use(&ty, out);
                if let Some(pattern) = pattern {
                    self.pattern(pattern, out);
                }
            }
            E::InstanceOf {
                expr: inner,
                ty: None,
                pattern,
            } => {
                self.expr(inner, out);
                if let Some(pattern) = pattern {
                    self.pattern(pattern, out);
                }
            }
            E::ClassLit(ty) => {
                self.type_use(&ty, out);
            }
            E::MethodRef {
                qualifier,
                type_name,
                ..
            } => {
                if let Some(qualifier) = qualifier {
                    self.expr(qualifier, out);
                }
                if let Some(ty) = type_name {
                    self.type_use(&ty, out);
                }
            }
            E::MethodCall {
                receiver,
                type_args,
                args,
                ..
            } => {
                if let Some(receiver) = receiver {
                    self.expr(receiver, out);
                }
                for ty in type_args {
                    self.type_use(&ty, out);
                }
                for arg in args {
                    self.expr(arg, out);
                }
            }
            E::Lambda { params, body } => {
                for param in params {
                    // §9.7.4: a lambda parameter's declaration annotations
                    // (`(@Ann int v) -> ...`), element type `PARAMETER` — a
                    // lambda parameter is a formal parameter declaration
                    // ([§15.27.1]). A *concise* parameter (`(v) -> ...`) writes
                    // neither a type nor a modifier, so it carries none.
                    for annotation in &param.annotations {
                        self.annotation(annotation, "PARAMETER", param.ty.is_some(), out);
                    }
                    if let Some(ty) = &param.ty {
                        self.type_use(ty, out);
                    }
                }
                match body {
                    L::Expr(expr) => {
                        self.expr(expr, out);
                    }
                    L::Block(stmt) => {
                        self.stmt(stmt, out);
                    }
                }
            }
            E::NewArray {
                ty,
                dims,
                initializer,
            } => {
                self.type_use(&ty, out);
                for dim in dims {
                    self.expr(dim, out);
                }
                if let Some(initializer) = initializer {
                    for elem in initializer {
                        self.expr(elem, out);
                    }
                }
            }
            E::ArrayInit(elems) => {
                for elem in elems {
                    self.expr(elem, out);
                }
            }
            E::Unary { expr: inner, .. } | E::Postfix { expr: inner, .. } | E::Paren(inner) => {
                self.expr(inner, out);
            }
            E::Binary { lhs, rhs, .. } => {
                self.expr(lhs, out);
                self.expr(rhs, out);
            }
            E::Assign { lhs, rhs, .. } => {
                self.expr(lhs, out);
                self.expr(rhs, out);
            }
            E::Conditional {
                cond, then, els, ..
            } => {
                self.expr(cond, out);
                self.expr(then, out);
                self.expr(els, out);
            }
            E::Switch { scrutinee, arms } => {
                self.expr(scrutinee, out);
                for arm in arms {
                    for label in &arm.labels {
                        match label {
                            SL::Expr(expr) | SL::Guard(expr) => {
                                self.expr(*expr, out);
                            }
                            // §14.30.2/§14.30.3: a `case` label's pattern — a
                            // type pattern's type carries type annotations and
                            // its binding carries declaration annotations, like
                            // an `instanceof` pattern.
                            SL::Pattern(pattern) => {
                                self.pattern(*pattern, out);
                            }
                        }
                    }
                    for stmt in arm.body {
                        self.stmt(stmt, out);
                    }
                }
            }
            E::CtorCall { args, .. } | E::Template { args } => {
                for arg in args {
                    self.expr(arg, out);
                }
            }
            E::ArrayAccess { array, index } => {
                self.expr(array, out);
                self.expr(index, out);
            }
            E::FieldAccess { target, .. } => {
                if let Some(target) = target {
                    self.expr(target, out);
                }
            }
            E::This { .. }
            | E::Super { .. }
            | E::Var(_)
            | E::NamePath(_)
            | E::Literal(_)
            | E::Null
            | E::Missing => {}
        }
    }

    /// The type references of a pattern ([JLS §14.30] type patterns and
    /// record patterns), plus the declaration annotations of the variables
    /// its components bind.
    fn pattern(&mut self, pattern: PatternId, out: &mut Vec<DeclDiagnostic>) {
        use hir_expand::body::{PatternData as P, TypePattern as TP};
        match self.cx.bodies.pattern(pattern).clone() {
            P::Type(TP { ty, binding }) => {
                self.type_use(&ty, out);
                // §14.30.1/[§9.6.4.1]: a type pattern is a local variable
                // declaration, so the pattern variable's element type is
                // `LOCAL_VARIABLE`. The binding's own `ty` is the pattern's type,
                // so only its *declaration* annotations are checked here (the
                // type annotations were checked just above).
                if let Some(binding) = binding {
                    self.binding(binding, "LOCAL_VARIABLE", out);
                }
            }
            P::Record(pattern) => {
                self.type_use(&pattern.ty, out);
                for component in pattern.components {
                    self.pattern(component, out);
                }
            }
            P::MatchAll => {}
        }
    }
}

/// Checks the element-value arguments of one annotation against the elements
/// of its type ([JLS §9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1)):
/// each `name = value` pair must name a declared element exactly once
/// ([§9.6.1]), a normal annotation must give every element without a default
/// value a pair ([§9.7.1]), and the value must be *commensurate* with the
/// element's declared type ([§9.7.1], [§5.2], [§15.29]). An annotation type
/// that cannot be resolved (or is not an annotation) has nothing to check — an
/// unknown annotation is reported by the name-resolution check.
fn check_annotation_elements(
    cx: &ValueCtx<'_>,
    annotation: &ItemAnnotationRef,
    map: &hir_expand::ast_id_map::AstIdMap,
    source: &syntax::SourceFile,
    out: &mut Vec<DeclDiagnostic>,
) {
    let (db, scope, resolver) = (cx.db, cx.scope, cx.resolver);
    // §9.7.1: a normal annotation must contain an element-value pair for every
    // element of its annotation interface except those with default values. A
    // marker annotation (`@Ann`) is the degenerate case of no pairs at all, so
    // the elements are resolved before the pairs are walked. The report is
    // anchored at the annotation's *name*, where IntelliJ anchors it.
    let Some(annotation_type) = annotation_type(db, scope, resolver, &annotation.name) else {
        return;
    };
    let elements = &annotation_type.elements;
    let missing: Vec<Name> = elements
        .iter()
        .filter(|element| !element.has_default)
        .filter(|element| !annotation.args.iter().any(|arg| arg.name == element.name))
        .map(|element| element.name.clone())
        .collect();
    if !missing.is_empty() {
        out.push(DeclDiagnostic::MissingAnnotationElement {
            names: missing,
            range: ranges::annotation_name_range(map, source, annotation),
        });
    }
    if annotation.args.is_empty() {
        return;
    }
    for (idx, arg) in annotation.args.iter().enumerate() {
        // The value's source range (where the mismatch is reported) is
        // re-derived from the annotation's syntax node; a value that cannot
        // be resolved falls back to the annotation's name range.
        let range = ranges::annotation_arg_value_range(map, source, annotation, idx)
            .or_else(|| ranges::annotation_name_range(map, source, annotation));
        // §9.7.1: no element may be given a value twice — the later pair is
        // the error.
        if annotation.args[..idx]
            .iter()
            .any(|prev| prev.name == arg.name)
        {
            out.push(DeclDiagnostic::DuplicateAnnotationMemberValue {
                name: arg.name.clone(),
                range,
            });
            continue;
        }
        let Some(element) = elements.iter().find(|element| element.name == arg.name) else {
            // §9.7.1: the pair names no element the annotation type declares.
            out.push(no_such_element(cx, &annotation_type.ty, &arg.name, range));
            continue;
        };
        check_value_commensurate(
            cx,
            &arg.value,
            &element.ty,
            range.unwrap_or_default(),
            map,
            source,
            out,
        );
    }
}

/// Checks one annotation element value against the element's declared type
/// ([JLS §9.7.1], [§5.2]). `range` is the source range of the value, where the
/// mismatch is reported.
fn check_value_commensurate(
    cx: &ValueCtx<'_>,
    value: &ItemAnnotationValue,
    element_ty: &Ty,
    range: rowan::TextRange,
    map: &hir_expand::ast_id_map::AstIdMap,
    source: &syntax::SourceFile,
    out: &mut Vec<DeclDiagnostic>,
) {
    let db = cx.db;
    match value {
        // An array initializer ([§10.6]) is checked element-wise against the
        // component type; an initializer where the element is not an array is
        // a compile-time error ([§9.7.1]).
        ItemAnnotationValue::Array(values) => match element_ty.kind(db) {
            TyKind::Array(component) => {
                for v in values {
                    check_value_commensurate(cx, v, component, range, map, source, out);
                }
            }
            _ => out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                found: Ty::array(db, *element_ty),
                expected: *element_ty,
                range: Some(range),
            }),
        },
        // §9.7.1: a single, non-initializer value against an array-typed
        // element is a one-element array shortcut — check it against the
        // component type instead.
        _ => {
            let target = match element_ty.kind(db) {
                TyKind::Array(component) => component,
                _ => element_ty,
            };
            check_single_value_commensurate(cx, value, target, range, map, source, out);
        }
    }
}

/// The §9.7.1 single-value checks of [`check_value_commensurate`], against the
/// effective target type `target` (the component type of an array-typed
/// element, or the element type itself).
///
/// Commensurability is checked in javac's order: the value's type must be
/// *assignable* to the element's ([§5.2], with the constant narrowing of an
/// integral constant), and only then must it be of the form the element's type
/// prescribes — a class literal ([§15.8.2]) for `Class`, an enum constant
/// ([§8.9.1]) for an enum, a constant expression ([§15.29]) for a primitive or
/// `String`, and never `null`. A value that trips several of the conjuncts
/// reports exactly one diagnostic: the first one below.
fn check_single_value_commensurate(
    cx: &ValueCtx<'_>,
    value: &ItemAnnotationValue,
    target: &Ty,
    range: rowan::TextRange,
    map: &hir_expand::ast_id_map::AstIdMap,
    source: &syntax::SourceFile,
    out: &mut Vec<DeclDiagnostic>,
) {
    use hir_def::java::item_tree::ItemAnnotationValue as V;
    let (db, scope) = (cx.db, cx.scope);
    // A nested annotation values the annotation type it names ([§9.7.1]), and
    // its own argument list is checked recursively.
    if let V::Annotation(inner) = value {
        if let Some(inner_ty) = resolve_name_ty(db, scope, cx.resolver, &inner.name)
            && !is_assignable(db, scope, &inner_ty, target)
        {
            out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                found: inner_ty,
                expected: *target,
                range: Some(range),
            });
        }
        check_annotation_elements(cx, inner, map, source, out);
        return;
    }
    // §6.5.6.1/§6.5.6.2/[§7.5.4]: a name is resolved as a *variable* — a bare
    // `CONSTANT` or a qualified `Type.CONSTANT` whose qualifier is not an enum
    // type — and [§4.12.4] decides whether it is a constant variable. A name
    // that denotes no accessible field keeps the `cannot resolve symbol`
    // report.
    let kind = match value {
        V::EnumConstant { qualifier, member } => {
            match annotation_value::name_kind(cx, qualifier.as_ref(), member) {
                annotation_value::NameKind::Field(kind) => kind,
                annotation_value::NameKind::NotQualified => {
                    annotation_value::ConstKind::NotConstant { ty: None }
                }
                annotation_value::NameKind::Unresolved => {
                    out.push(DeclDiagnostic::UnknownAnnotationElementConstant {
                        member: member.clone(),
                        range: Some(range),
                    });
                    return;
                }
            }
        }
        _ => annotation_value::value_kind(cx, value),
    };
    // §5.2/§15.29: the value's type must be assignable to the element's —
    // including the constant narrowing of an integral constant into a
    // `byte`/`short`/`char` target.
    if let Some(value_ty) = kind.ty().cloned()
        && !value_assignable(cx, &kind, &value_ty, target)
    {
        out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
            found: value_ty,
            expected: *target,
            range: Some(range),
        });
        return;
    }
    // §9.7.1: "`v` is not `null`". A `null` that reached here is assignable,
    // so the element's type is a reference type: an enum element rejects it as
    // javac's `enum.annotation.must.be.enum.constant` does, an
    // annotation-typed element as the ordinary type mismatch with the null
    // type as `found`, and a `Class`, array or `String` element falls through
    // to its own rule below.
    if is_null_value(cx.bodies, value) {
        if enum_constants(db, scope, target).is_some() {
            out.push(DeclDiagnostic::AnnotationElementNotEnumConstant { range: Some(range) });
            return;
        }
        if !annotation_value::is_class(target, db) && !annotation_value::is_string(target, db) {
            out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                found: Ty::null(db),
                expected: *target,
                range: Some(range),
            });
            return;
        }
    }
    // §9.7.1: a `Class`-typed element takes a class literal itself
    // ([§15.8.2]) — a parenthesized class literal is not one, nor is a
    // `Class`-valued constant variable.
    if annotation_value::is_class(target, db) {
        if !is_class_literal(value) {
            out.push(DeclDiagnostic::AnnotationElementNotClassLiteral { range: Some(range) });
        }
        return;
    }
    // §9.7.1/§8.9.1: an enum-typed element takes an enum constant.
    if enum_constants(db, scope, target).is_some() {
        if !is_enum_constant_form(cx, value, target) {
            out.push(DeclDiagnostic::AnnotationElementNotEnumConstant { range: Some(range) });
        }
        return;
    }
    // §9.7.1/§15.29: a primitive- or `String`-typed element takes a constant
    // expression.
    // §9.7.1/§15.29: a primitive- or `String`-typed element takes a constant
    // expression. Only a value this layer *decided* is not one is reported:
    // [`annotation_value::ConstKind::Unknown`] — a value of a written type, an
    // unreadable library constant — is possibly constant and stays silent.
    if annotation_value::is_primitive_or_string(target, db)
        && matches!(kind, annotation_value::ConstKind::NotConstant { .. })
    {
        out.push(DeclDiagnostic::NonConstantAnnotationElement { range: Some(range) });
    }
}

/// Whether the value's type `value_ty` is assignable to `target` ([§5.2]),
/// with the constant narrowing of an integral constant that fits the
/// narrower integral target.
fn value_assignable(
    cx: &ValueCtx<'_>,
    kind: &annotation_value::ConstKind,
    value_ty: &Ty,
    target: &Ty,
) -> bool {
    let db = cx.db;
    // §5.2: a constant of type `byte`, `short`, `char` or `int` narrows to
    // `byte`, `short` or `char` when its value fits; one that does not fit is
    // rejected by the assignment check below.
    if let (Some(value), TyKind::Primitive(primitive)) = (kind.narrowing_value(db), target.kind(db))
        && narrows_to(value, *primitive).is_some_and(|fits| fits)
    {
        return true;
    }
    is_assignable(db, cx.scope, value_ty, target)
}

/// Whether the value *is* a class literal ([§15.8.2]), parentheses and all: a
/// parenthesized class literal is an expression, not the class literal
/// [§9.7.1] requires.
fn is_class_literal(value: &ItemAnnotationValue) -> bool {
    matches!(value, ItemAnnotationValue::ClassLit(_))
}

/// Whether the value is the `null` literal, looking through parentheses
/// ([§3.10.8]).
fn is_null_value(bodies: &BodyTree, value: &ItemAnnotationValue) -> bool {
    let ItemAnnotationValue::Expr(expr) = value else {
        return false;
    };
    is_null_expr(bodies, *expr)
}

fn is_null_expr(bodies: &BodyTree, expr: ExprId) -> bool {
    match bodies.expr(expr) {
        hir_expand::body::ExprData::Null => true,
        hir_expand::body::ExprData::Paren(inner) => is_null_expr(bodies, *inner),
        _ => false,
    }
}

/// Whether the value is the *enum-constant form* of §9.7.1 — a bare constant
/// whose declaring type is the element's own enum type, or a qualified
/// `Type.CONSTANT` naming a constant of the enum — looking through
/// parentheses as IntelliJ's `skipParenthesizedExprDown` does.
fn is_enum_constant_form(cx: &ValueCtx<'_>, value: &ItemAnnotationValue, target: &Ty) -> bool {
    let db = cx.db;
    let Some((qualifier, member)) = value_name(cx.bodies, value) else {
        return false;
    };
    // A bare constant's declaring type is the element's own type ([§9.7.1]);
    // a qualified one names its enum explicitly ([§6.5.6.2]).
    let owner = match qualifier {
        Some(qualifier) => resolve_name_ty(db, cx.scope, cx.resolver, &qualifier),
        None => Some(*target),
    };
    owner.is_some_and(|owner| {
        enum_constants(db, cx.scope, &owner)
            .is_some_and(|constants| constants.iter().any(|c| c == member.as_str()))
    })
}

/// The name a value *is*, when it is one — a bare `CONSTANT` or a qualified
/// `Type.CONSTANT` — looking through parentheses. `None` for every other
/// value, and for a member access whose receiver is not a plain name.
fn value_name(bodies: &BodyTree, value: &ItemAnnotationValue) -> Option<(Option<Name>, Name)> {
    match value {
        ItemAnnotationValue::EnumConstant { qualifier, member } => {
            Some((qualifier.clone(), member.clone()))
        }
        ItemAnnotationValue::Expr(expr) => value_name_expr(bodies, *expr),
        _ => None,
    }
}

fn value_name_expr(bodies: &BodyTree, expr: ExprId) -> Option<(Option<Name>, Name)> {
    use hir_expand::body::ExprData as E;
    match bodies.expr(expr) {
        E::Paren(inner) => value_name_expr(bodies, *inner),
        E::Var(name) => Some((None, name.clone())),
        E::NamePath(name) => Some(match annotation_value::qualified_parts(name) {
            Some((qualifier, member)) => (Some(qualifier), member),
            None => (None, name.clone()),
        }),
        E::FieldAccess {
            target: Some(receiver),
            name,
        } => match bodies.expr(*receiver) {
            E::Var(qualifier) | E::NamePath(qualifier) => {
                Some((Some(qualifier.clone()), name.clone()))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The `check_value_commensurate` twin over a *span* (`&AnnotationValue`, the
/// body path — a body-side annotation whose value carries its range).
fn check_value_commensurate_ranged(
    cx: &ValueCtx<'_>,
    value: &AnnotationValue,
    element_ty: &Ty,
    range: rowan::TextRange,
    out: &mut Vec<DeclDiagnostic>,
) {
    let db = cx.db;
    match value {
        AnnotationValue::Array(values) => match element_ty.kind(db) {
            TyKind::Array(component) => {
                for v in values {
                    check_value_commensurate_ranged(cx, v, component, range, out);
                }
            }
            _ => out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                found: Ty::array(db, *element_ty),
                expected: *element_ty,
                range: Some(range),
            }),
        },
        _ => {
            let target = match element_ty.kind(db) {
                TyKind::Array(component) => component,
                _ => element_ty,
            };
            check_single_value_commensurate_ranged(cx, value, target, range, out);
        }
    }
}

/// The `check_single_value_commensurate` twin over a *span* (see
/// [`check_value_commensurate_ranged`]).
fn check_single_value_commensurate_ranged(
    cx: &ValueCtx<'_>,
    value: &AnnotationValue,
    target: &Ty,
    range: rowan::TextRange,
    out: &mut Vec<DeclDiagnostic>,
) {
    use hir_expand::span::AnnotationValue as V;
    let (db, scope) = (cx.db, cx.scope);
    if let V::Annotation(inner) = value {
        if let Some(inner_ty) = resolve_name_ty(db, scope, cx.resolver, &inner.name.name)
            && !is_assignable(db, scope, &inner_ty, target)
        {
            out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                found: inner_ty,
                expected: *target,
                range: Some(range),
            });
        }
        check_annotation_elements_ranged(cx, inner, out);
        return;
    }
    let kind = match value {
        V::EnumConstant { qualifier, member } => {
            match annotation_value::name_kind(cx, qualifier.as_ref(), member) {
                annotation_value::NameKind::Field(kind) => kind,
                annotation_value::NameKind::NotQualified => {
                    annotation_value::ConstKind::NotConstant { ty: None }
                }
                annotation_value::NameKind::Unresolved => {
                    out.push(DeclDiagnostic::UnknownAnnotationElementConstant {
                        member: member.clone(),
                        range: Some(range),
                    });
                    return;
                }
            }
        }
        _ => annotation_value::ranged_value_kind(cx, value),
    };
    if let Some(value_ty) = kind.ty().cloned()
        && !value_assignable(cx, &kind, &value_ty, target)
    {
        out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
            found: value_ty,
            expected: *target,
            range: Some(range),
        });
        return;
    }
    if is_null_value_ranged(cx.bodies, value) {
        if enum_constants(db, scope, target).is_some() {
            out.push(DeclDiagnostic::AnnotationElementNotEnumConstant { range: Some(range) });
            return;
        }
        if !annotation_value::is_class(target, db) && !annotation_value::is_string(target, db) {
            out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                found: Ty::null(db),
                expected: *target,
                range: Some(range),
            });
            return;
        }
    }
    if annotation_value::is_class(target, db) {
        if !matches!(value, V::ClassLit(_)) {
            out.push(DeclDiagnostic::AnnotationElementNotClassLiteral { range: Some(range) });
        }
        return;
    }
    if enum_constants(db, scope, target).is_some() {
        if !is_enum_constant_form_ranged(cx, value, target) {
            out.push(DeclDiagnostic::AnnotationElementNotEnumConstant { range: Some(range) });
        }
        return;
    }
    // §9.7.1/§15.29: a primitive- or `String`-typed element takes a constant
    // expression. Only a value this layer *decided* is not one is reported:
    // [`annotation_value::ConstKind::Unknown`] — a value of a written type, an
    // unreadable library constant — is possibly constant and stays silent.
    if annotation_value::is_primitive_or_string(target, db)
        && matches!(kind, annotation_value::ConstKind::NotConstant { .. })
    {
        out.push(DeclDiagnostic::NonConstantAnnotationElement { range: Some(range) });
    }
}

/// The [`is_null_value`] twin over a *span* value.
fn is_null_value_ranged(bodies: &BodyTree, value: &AnnotationValue) -> bool {
    let AnnotationValue::Expr(expr) = value else {
        return false;
    };
    is_null_expr(bodies, *expr)
}

/// The [`is_enum_constant_form`] twin over a *span* value.
fn is_enum_constant_form_ranged(cx: &ValueCtx<'_>, value: &AnnotationValue, target: &Ty) -> bool {
    let db = cx.db;
    let Some((qualifier, member)) = value_name_ranged(cx.bodies, value) else {
        return false;
    };
    let owner = match qualifier {
        Some(qualifier) => resolve_name_ty(db, cx.scope, cx.resolver, &qualifier),
        None => Some(*target),
    };
    owner.is_some_and(|owner| {
        enum_constants(db, cx.scope, &owner)
            .is_some_and(|constants| constants.iter().any(|c| c == member.as_str()))
    })
}

/// The [`value_name`] twin over a *span* value.
fn value_name_ranged(bodies: &BodyTree, value: &AnnotationValue) -> Option<(Option<Name>, Name)> {
    match value {
        AnnotationValue::EnumConstant { qualifier, member } => {
            Some((qualifier.clone(), member.clone()))
        }
        AnnotationValue::Expr(expr) => value_name_expr(bodies, *expr),
        _ => None,
    }
}

/// Whether an integral constant's value [§5.2]'s narrowing conversion fits a
/// narrower integral target; `None` for targets that never narrow from an
/// integral constant.
///
/// [§5.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2
fn narrows_to(value: i64, target: PrimitiveType) -> Option<bool> {
    let (lo, hi) = match target {
        PrimitiveType::Byte => (-128, 127),
        PrimitiveType::Short => (-32_768, 32_767),
        PrimitiveType::Char => (0, 65_535),
        _ => return None,
    };
    Some((lo..=hi).contains(&value))
}

/// The [`Ty`] a reference name resolves to, for an annotation argument's
/// enum qualifier (`@Ann(E.CONST)`) or a nested annotation name. Resolved
/// like any type name ([JLS §6.5.5.1]); `None` when it does not resolve.
fn resolve_name_ty(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    name: &Name,
) -> Option<Ty> {
    let fqn = candidate_fqns(resolver, name)
        .into_iter()
        .find(|candidate| hir::fqn_resolve(db, scope, candidate.as_str()).is_some())?;
    Some(Ty::reference(db, fqn.as_str(), Vec::new()))
}

/// The enum constants of the type `ty` ([JLS §8.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9)),
/// when it resolves to an enum on the classpath: the `EnumConstant` children
/// of a source enum ([§8.9.1]), the `ACC_ENUM` fields ([JVMS §4.1]) of a
/// library one. `None` when `ty` is not an enum — a bare constant then has no
/// declaring type to resolve against ([§9.7.1]). Used to validate the
/// enum-constant element values of an annotation ([§9.7.1]).
fn enum_constants(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Option<Vec<String>> {
    let TyKind::Reference { name, .. } = ty.kind(db) else {
        return None;
    };
    let resolved = hir::fqn_resolve(db, scope, name.as_str())?;
    match resolved {
        hir::Resolved::Source(source) => {
            let source_tree = hir::file_item_tree(db, source.file);
            if !matches!(source_tree.data(source.item), ItemData::Enum(_)) {
                return None;
            }
            let mut out = Vec::new();
            for &child in source_tree.data(source.item).body() {
                if let ItemData::EnumConstant(constant) = source_tree.data(child) {
                    out.push(constant.name.as_str().to_owned());
                }
            }
            Some(out)
        }
        hir::Resolved::Library(resolved) => {
            let record = hir::class_record(db, &resolved)?;
            let hir::ClassOrModuleRecord::Class(class) = record.as_ref() else {
                return None;
            };
            if hir::ClassKind::from_flags(class.flags, class.is_record) != hir::ClassKind::Enum {
                return None;
            }
            Some(
                class
                    .fields
                    .iter()
                    .filter(|field| field.flags & ACC_ENUM != 0)
                    .map(|field| db.hir_state().interner.resolve(&field.name).to_owned())
                    .collect(),
            )
        }
    }
}

/// One element of an annotation type ([JLS §9.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.1)):
/// its name and the declared type of the value it accepts.
#[derive(Debug, Clone)]
struct AnnotationElement {
    name: Name,
    ty: Ty,
    /// Whether the element declares a default value
    /// ([§9.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.1)):
    /// a normal annotation must give a pair for every element that does *not*
    /// ([§9.7.1]).
    has_default: bool,
}

/// The resolved annotation type of one annotation occurrence: the `Ty` of the
/// annotation interface — what the pair reports probe for a member method —
/// and its elements ([§9.6.1]).
struct AnnotationType {
    ty: Ty,
    elements: Vec<AnnotationElement>,
}

/// The annotation type `name` resolves to, with its elements ([§9.6.1]) in
/// declaration order: each is an abstract method of the annotation
/// declaration whose return type ([§8.4.5]) is the element's declared type.
/// `None` when `name` does not resolve to an annotation type (nothing to
/// check).
fn annotation_type(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    name: &Name,
) -> Option<AnnotationType> {
    let fqn = candidate_fqns(resolver, name)
        .into_iter()
        .find(|candidate| hir::fqn_resolve(db, scope, candidate.as_str()).is_some())?;
    let fqn = fqn.as_str();
    let ty = Ty::reference(db, fqn, Vec::new());
    match hir::fqn_resolve(db, scope, fqn)? {
        hir::Resolved::Source(source) => {
            let source_tree = hir::file_item_tree(db, source.file);
            if !matches!(source_tree.data(source.item), ItemData::Annotation(_)) {
                return None;
            }
            // The elements are the methods of the annotation declaration; their
            // return types resolve in the annotation's own file scope
            // ([§6.5.5.1]).
            let file_scope = crate::java::resolve::scope_for_file(db, source.file);
            let type_params = crate::java::db::type_params_map_query(db, db.file_text(source.file));
            let resolver = Resolver::new(&source_tree, type_params, source.item);
            // §9.6.1/§9.7.1: whether an element declares a default is read
            // from the *annotation type's own* source — not from the lowered
            // default expression, which a default written as a nested
            // annotation (or any non-expression element value) does not
            // produce.
            let default_ctx = range_ctx(db, source.file, source_tree.language);
            let mut out = Vec::new();
            for &child in source_tree.data(source.item).body() {
                if let ItemData::Method(method) = source_tree.data(child)
                    && let Some(ret) = &method.sig.ret
                {
                    let has_default = default_ctx
                        .as_ref()
                        .and_then(|(map, source)| {
                            ranges::method_default_value_range(map, source, method)
                        })
                        .is_some();
                    out.push(AnnotationElement {
                        name: method.name.clone(),
                        ty: resolve_type_ref(db, &file_scope, &resolver, &ret.ty),
                        has_default,
                    });
                }
            }
            Some(AnnotationType { ty, elements: out })
        }
        hir::Resolved::Library(resolved) => {
            let record = hir::class_record(db, &resolved)?;
            let hir::ClassOrModuleRecord::Class(class) = record.as_ref() else {
                return None;
            };
            // §9.6.1: an annotation element is an abstract method stub whose
            // return type ([§8.4.5]) is the element's declared type.
            if hir::ClassKind::from_flags(class.flags, class.is_record)
                != hir::ClassKind::Annotation
            {
                return None;
            }
            // §9.6.1: the elements of an annotation interface are exactly the
            // methods it *declares* — so a classfile record's empty member
            // list means the annotation interface declares no elements. The
            // list is complete or absent: a classfile that fails to parse
            // yields no record at all (and resolution fails instead), never a
            // partial one. An element-free annotation interface therefore has
            // nothing that could be missing, while every element-value pair of
            // a normal annotation of it is an error ([§9.7.1]).
            Some(AnnotationType {
                ty,
                elements: class
                    .methods
                    .iter()
                    .map(|method| AnnotationElement {
                        name: Name::new(db.hir_state().interner.resolve(&method.name)),
                        ty: ty_from_library(db, &method.return_type),
                        // §9.7.1: a classfile element's default is its
                        // `AnnotationDefault` attribute ([JVMS §4.7.22]).
                        has_default: method.default_value.is_some(),
                    })
                    .collect(),
            })
        }
    }
}

/// The report for a pair that names no element of the annotation interface
/// ([§9.7.1]): javac reports a name that resolved to a member *method* the
/// annotation interface does not own — `java.lang.Object.toString`, inherited
/// by every annotation type — as `no annotation member named`
/// ([`DeclDiagnostic::UnknownAnnotationMember`]), while a name that resolved
/// to nothing at all is the failed resolution of the name itself
/// ([`DeclDiagnostic::UnresolvedAnnotationMember`],
/// `compiler.err.cant.resolve.location.args`).
fn no_such_element(
    cx: &ValueCtx<'_>,
    annotation_ty: &Ty,
    name: &Name,
    range: Option<rowan::TextRange>,
) -> DeclDiagnostic {
    if names_member_method(cx, annotation_ty, name) {
        DeclDiagnostic::UnknownAnnotationMember {
            name: name.clone(),
            range,
        }
    } else {
        DeclDiagnostic::UnresolvedAnnotationMember {
            name: name.clone(),
            range,
        }
    }
}

/// Whether `name` is the name of some member *method* of the annotation
/// interface `annotation_ty` ([JLS §9.2]: its own and every inherited member
/// method). This is javac's dispatch between the two reports of a pair that
/// names no element — `sym.kind == MTH && sym.owner != thisAnnotationType.tsym`
/// in `Annotate.attributeAnnotationNameValuePair`, which holds exactly for the
/// members another type owns, `Object`'s public ones above all.
fn names_member_method(cx: &ValueCtx<'_>, annotation_ty: &Ty, name: &Name) -> bool {
    let ctx = crate::java::method::access_context(cx.db, cx.file, cx.item);
    crate::java::method::all_methods(cx.db, cx.scope, annotation_ty, &ctx)
        .iter()
        .any(|method| method.name.as_str() == name.as_str())
}

/// Resolves an annotation name to its `@Target` element-type constant names.
/// `None` when the annotation type cannot be resolved (nothing to enforce) or
/// carries no `@Target` (empty target → applicable to every declaration,
/// [§9.6.4.1]).
fn resolve_annotation_type(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    scope: &hir::ResolutionScope,
    name: &Name,
) -> Option<Vec<String>> {
    let fqn = candidate_fqns(resolver, name)
        .into_iter()
        .find(|candidate| hir::fqn_resolve(db, scope, candidate.as_str()).is_some())?;
    let fqn = fqn.as_str();
    match hir::fqn_resolve(db, scope, fqn)? {
        hir::Resolved::Source(source) => {
            let source_tree = hir::file_item_tree(db, source.file);
            match source_tree.data(source.item) {
                ItemData::Annotation(annotation) => {
                    // The `@Target` argument list was lowered with the
                    // annotation type's own declaration ([§9.7.1]); the
                    // annotation itself is resolved in its own file's scope
                    // ([§6.5.5.1]), so the simple `@Target` — implicitly
                    // imported from `java.lang` ([JLS §7.3]) — and the fully
                    // qualified form both count, while a same-package
                    // `@interface Target` that shadows the JDK annotation
                    // ([§6.5.5.1]) does not.
                    let file_scope = crate::java::resolve::scope_for_file(db, source.file);
                    let type_params =
                        crate::java::db::type_params_map_query(db, db.file_text(source.file));
                    let resolver = Resolver::new(&source_tree, type_params, source.item);
                    annotation
                        .annotations
                        .iter()
                        .find(|annotation| {
                            is_target_annotation(db, &file_scope, &resolver, annotation)
                        })
                        .map(|annotation| target_value_names(&annotation.args))
                }
                _ => None,
            }
        }
        hir::Resolved::Library(resolved) => library_target_args(db, resolved),
    }
}

/// Whether an annotation name resolves to `java.lang.annotation.Target`
/// (`@Target`, [JLS §9.6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.1)):
/// the name resolves like any type name in the declaration's scope
/// ([§6.5.5.1]) — the simple name (implicitly imported from `java.lang`,
/// [§7.3]) or a fully qualified name — and only a resolution to the JDK
/// annotation counts, so a same-package `@interface Target` or a shadowing
/// import ([§6.5.5.1]) is not mistaken for it.
fn is_target_annotation(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    annotation: &ItemAnnotationRef,
) -> bool {
    candidate_fqns(resolver, &annotation.name)
        .into_iter()
        .find(|candidate| hir::fqn_resolve(db, scope, candidate.as_str()).is_some())
        .is_some_and(|candidate| candidate.as_str() == "java.lang.annotation.Target")
}

/// The `ElementType` constant names of a `@Target` argument list: the enum
/// constants of the `value` element ([§9.7.1]), single or in an array.
fn target_value_names(args: &[hir_def::java::item_tree::ItemAnnotationArg]) -> Vec<String> {
    let mut out = Vec::new();
    for arg in args {
        if arg.name.as_str() == "value" {
            collect_enum_names(&arg.value, &mut out);
        }
    }
    out
}

fn collect_enum_names(value: &ItemAnnotationValue, out: &mut Vec<String>) {
    match value {
        ItemAnnotationValue::Array(values) => {
            for value in values {
                collect_enum_names(value, out);
            }
        }
        ItemAnnotationValue::EnumConstant { member, .. } => out.push(member.as_str().to_owned()),
        _ => {}
    }
}

/// The `ElementType` constant names of a *library* annotation type's
/// `@Target` — read from the classfile `RuntimeVisibleAnnotations` stub.
fn library_target_args(db: &dyn TyDatabase, resolved: hir::ResolvedClass) -> Option<Vec<String>> {
    let record = hir::class_record(db, &resolved)?;
    let hir::ClassOrModuleRecord::Class(class) = record.as_ref() else {
        return None;
    };
    for annotation in &class.annotations {
        let is_target = annotation
            .annotation_type
            .as_reference_name()
            .is_some_and(|name| {
                db.hir_state().interner.resolve(name) == "java.lang.annotation.Target"
            });
        if !is_target {
            continue;
        }
        let mut out = Vec::new();
        for (name, value) in &annotation.arguments {
            if db.hir_state().interner.resolve(name) == "value" {
                collect_library_enum_names(db, value, &mut out);
            }
        }
        return Some(out);
    }
    None
}

/// Whether a `@Target` element-type set contains `et`.
fn targets_contain(targets: &[String], et: &str) -> bool {
    targets.iter().any(|target| target == et)
}

fn collect_library_enum_names(
    db: &dyn TyDatabase,
    value: &hir::AnnotationValue<hir::Symbol>,
    out: &mut Vec<String>,
) {
    match value {
        hir::AnnotationValue::Array(values) => {
            for value in values {
                collect_library_enum_names(db, value, out);
            }
        }
        hir::AnnotationValue::Enum { entry_name, .. } => {
            out.push(db.hir_state().interner.resolve(entry_name).to_owned());
        }
        _ => {}
    }
}
