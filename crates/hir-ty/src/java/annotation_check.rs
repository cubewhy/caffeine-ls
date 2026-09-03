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
//! - A *type-use* annotation `T` on a type of `D` is applicable iff `T`'s
//!   target contains `TYPE_USE`, or contains `E` itself ([§9.6.4.1] — the
//!   annotated type belongs to the declaration, so its element type counts).
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
//! is honored the same way.

use hir_def::java::item_tree::{ItemData, ItemId, ItemTree, TypeParam};
use hir_expand::{
    body::{BodyTree, ExprId, Literal, PatternId, StmtId},
    name::Name,
    span::{AnnotationArg, AnnotationRef, AnnotationValue, SpannedTypeRef},
};
use rust_asm::constants::ACC_ENUM;
use rustc_hash::FxHashMap;
use syntax::stub::PrimitiveType;
use vfs::FileId;

use crate::java::db::TyDatabase;
use crate::java::decl_check::DeclDiagnostic;
use crate::java::resolve::{Resolver, candidate_fqns, resolve_type_ref, ty_from_library};
use crate::java::subtyping::is_assignable;
use crate::java::ty::{Ty, TyKind};

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
/// ([JLS §9.6.4.1], [§9.7.4]) and the element-value argument checks
/// ([§9.7.1]).
pub(crate) fn annotation_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &ItemTree,
) -> Vec<DeclDiagnostic> {
    let scope = crate::java::resolve::scope_for_file(db, file);
    let type_params = crate::java::db::type_params_map_query(db, db.file_text(file));
    let bodies = hir::file_body_tree(db, file);
    let mut out = Vec::new();

    fn walk(
        db: &dyn TyDatabase,
        tree: &ItemTree,
        bodies: &BodyTree,
        scope: &hir::ResolutionScope,
        type_params: &FxHashMap<ItemId, Vec<TypeParam>>,
        id: ItemId,
        out: &mut Vec<DeclDiagnostic>,
    ) {
        let data = tree.data(id);
        // The annotation names resolve like any type name in the item's scope
        // ([JLS §6.5.5.1]); the resolver is built once per item and shared by
        // the declaration and type-use checks of its annotations.
        let resolver = Resolver::new(tree, type_params, id);
        // §9.6.4.1: the declaration annotations of the item itself.
        for annotation in declaration_annotations(data) {
            check_declaration_annotation(db, &resolver, scope, data, annotation, out);
        }
        // §9.6.4.1/[§9.7.4]: the annotations of a record's *components* have
        // element type `RECORD_COMPONENT` (Table 9.7-1); like a field, a
        // component's type gives the type-use fallback a target.
        if let ItemData::Record(record) = data {
            for component in &record.components {
                for annotation in &component.annotations {
                    check_target(
                        db,
                        &resolver,
                        scope,
                        &["RECORD_COMPONENT"],
                        true,
                        annotation,
                        out,
                    );
                }
            }
        }
        // §9.6.4.1/[§9.7.4]: the type-use annotations on the item's type
        // references (field types, method signatures, record component types,
        // superclass/interfaces, type-parameter bounds).
        for spanned in declaration_type_refs(data) {
            check_type_use(db, &resolver, scope, element_type_of(data), spanned, out);
        }
        // The body-position type-use annotations: casts, `new`, `instanceof`,
        // class literals, method-reference type names, lambda parameter types
        // and local variable types. The "enclosing declaration" of a type in
        // a body is the body's owner.
        if let Some(body) = body_of(tree, id) {
            let body_data = bodies.bodies.get(body.0);
            check_body_type_use(
                db,
                &resolver,
                bodies,
                scope,
                element_type_of(data),
                body_data,
                out,
            );
        }
        for &child in data.body() {
            walk(db, tree, bodies, scope, type_params, child, out);
        }
    }

    for &top in &tree.top {
        walk(db, tree, &bodies, &scope, &type_params, top, &mut out);
    }
    out
}

/// The declaration annotations of an item, in source order.
fn declaration_annotations(data: &ItemData) -> Vec<&AnnotationRef> {
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
fn declaration_type_refs(data: &ItemData) -> Vec<&SpannedTypeRef> {
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
/// body. (The type-use annotations of a field initializer's expression types
/// are covered by the enclosing file walk.)
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
    db: &dyn TyDatabase,
    resolver: &Resolver,
    scope: &hir::ResolutionScope,
    data: &ItemData,
    annotation: &AnnotationRef,
    out: &mut Vec<DeclDiagnostic>,
) {
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
        db,
        resolver,
        scope,
        element_types,
        has_annotatable_type,
        annotation,
        out,
    );
}

/// The shared applicability check of an annotation written in a declaration
/// position ([JLS §9.6.4.1], [§9.7.4]): it is applicable when its `@Target`
/// contains one of the declaration's element types (`element_types`), or — for
/// a declaration with an annotatable type — contains `TYPE_USE`, in which case
/// the annotation is a *type annotation* on that type.
fn check_target(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    scope: &hir::ResolutionScope,
    element_types: &[&'static str],
    has_annotatable_type: bool,
    annotation: &AnnotationRef,
    out: &mut Vec<DeclDiagnostic>,
) {
    // §9.7.1: the element-value arguments are checked against the annotation
    // type's elements regardless of whether the target check passes.
    check_annotation_elements(db, resolver, scope, annotation, out);
    let Some(targets) = resolve_annotation_type(db, resolver, scope, &annotation.name.name) else {
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
        name: annotation.name.name.clone(),
        element_type: element_types[0],
        range: annotation.name.range,
    });
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

/// Checks the type-use annotations of one spanned type
/// ([JLS §9.7.4], [§9.6.4.1]).
fn check_type_use(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    scope: &hir::ResolutionScope,
    element_type: Option<&'static str>,
    spanned: &SpannedTypeRef,
    out: &mut Vec<DeclDiagnostic>,
) {
    for annotation in &spanned.type_use_annotations {
        check_type_use_annotation(db, resolver, scope, element_type, annotation, out);
    }
}

/// The shared type-use applicability check: the annotation's target must
/// contain `TYPE_USE` or the element type of the enclosing declaration
/// ([§9.6.4.1]).
fn check_type_use_annotation(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    scope: &hir::ResolutionScope,
    element_type: Option<&'static str>,
    annotation: &AnnotationRef,
    out: &mut Vec<DeclDiagnostic>,
) {
    // §9.7.1: the element-value arguments are checked regardless of whether
    // the target check passes.
    check_annotation_elements(db, resolver, scope, annotation, out);
    let Some(targets) = resolve_annotation_type(db, resolver, scope, &annotation.name.name) else {
        return;
    };
    let applicable = targets_contain(&targets, "TYPE_USE")
        || element_type.is_some_and(|et| targets_contain(&targets, et));
    if !applicable {
        out.push(DeclDiagnostic::AnnotationNotApplicable {
            name: annotation.name.name.clone(),
            element_type: "TYPE_USE",
            range: annotation.name.range,
        });
    }
}

/// Walks one body for type-use annotations in expression and local-variable
/// type contexts ([JLS §9.7.4]): casts, `new`, `instanceof`, class literals,
/// method-reference type names, lambda parameter types, explicit invocation
/// type arguments and local variable types.
fn check_body_type_use(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    bodies: &BodyTree,
    scope: &hir::ResolutionScope,
    element_type: Option<&'static str>,
    body: &hir_expand::body::Body,
    out: &mut Vec<DeclDiagnostic>,
) {
    for &param in &body.params {
        let local = bodies.local(param);
        if let Some(ty) = &local.ty {
            check_type_use(db, resolver, scope, element_type, ty, out);
        }
    }
    for &stmt in &body.stmts {
        check_stmt_type_use(db, resolver, bodies, scope, element_type, stmt, out);
    }
}

/// Recurses over a statement's expressions, checking each type reference.
fn check_stmt_type_use(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    bodies: &BodyTree,
    scope: &hir::ResolutionScope,
    element_type: Option<&'static str>,
    stmt: StmtId,
    out: &mut Vec<DeclDiagnostic>,
) {
    use hir_expand::body::{StmtData as S, SwitchLabel as L};
    match bodies.stmt(stmt).clone() {
        S::Decl { local, initializer } => {
            let local = bodies.local(local);
            if let Some(ty) = &local.ty {
                check_type_use(db, resolver, scope, element_type, ty, out);
            }
            if let Some(expr) = initializer {
                check_expr_type_use(db, resolver, bodies, scope, element_type, expr, out);
            }
        }
        S::DeclGroup(stmts) => {
            for stmt in stmts {
                check_stmt_type_use(db, resolver, bodies, scope, element_type, stmt, out);
            }
        }
        S::Block(stmts) => {
            for stmt in stmts {
                check_stmt_type_use(db, resolver, bodies, scope, element_type, stmt, out);
            }
        }
        S::Expr(expr) => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, expr, out);
        }
        S::Labeled { stmt, .. } => {
            check_stmt_type_use(db, resolver, bodies, scope, element_type, stmt, out);
        }
        S::If { cond, then, els } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, cond, out);
            check_stmt_type_use(db, resolver, bodies, scope, element_type, then, out);
            if let Some(els) = els {
                check_stmt_type_use(db, resolver, bodies, scope, element_type, els, out);
            }
        }
        S::While { cond, body, .. } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, cond, out);
            check_stmt_type_use(db, resolver, bodies, scope, element_type, body, out);
        }
        S::DoWhile { body, cond } => {
            check_stmt_type_use(db, resolver, bodies, scope, element_type, body, out);
            check_expr_type_use(db, resolver, bodies, scope, element_type, cond, out);
        }
        S::For {
            init,
            cond,
            step,
            body,
        } => {
            for stmt in init {
                check_stmt_type_use(db, resolver, bodies, scope, element_type, stmt, out);
            }
            if let Some(cond) = cond {
                check_expr_type_use(db, resolver, bodies, scope, element_type, cond, out);
            }
            for expr in step {
                check_expr_type_use(db, resolver, bodies, scope, element_type, expr, out);
            }
            check_stmt_type_use(db, resolver, bodies, scope, element_type, body, out);
        }
        S::ForEach {
            var,
            iterable,
            body,
        } => {
            let local = bodies.local(var);
            if let Some(ty) = &local.ty {
                check_type_use(db, resolver, scope, element_type, ty, out);
            }
            check_expr_type_use(db, resolver, bodies, scope, element_type, iterable, out);
            check_stmt_type_use(db, resolver, bodies, scope, element_type, body, out);
        }
        S::Switch { scrutinee, arms } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, scrutinee, out);
            for arm in arms {
                for label in &arm.labels {
                    match label {
                        L::Expr(expr) | L::Guard(expr) => check_expr_type_use(
                            db,
                            resolver,
                            bodies,
                            scope,
                            element_type,
                            *expr,
                            out,
                        ),
                        L::Pattern(_) => {}
                    }
                }
                for stmt in arm.body {
                    check_stmt_type_use(db, resolver, bodies, scope, element_type, stmt, out);
                }
            }
        }
        S::Return(expr) => {
            if let Some(expr) = expr {
                check_expr_type_use(db, resolver, bodies, scope, element_type, expr, out);
            }
        }
        S::Yield(expr) => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, expr, out);
        }
        S::Throw(expr) | S::Synchronized { expr, .. } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, expr, out);
        }
        S::Try {
            resources,
            body,
            catches,
            finally,
        } => {
            for resource in resources {
                let local = bodies.local(resource.local);
                if let Some(ty) = &local.ty {
                    check_type_use(db, resolver, scope, element_type, ty, out);
                }
                if let Some(initializer) = resource.initializer {
                    check_expr_type_use(
                        db,
                        resolver,
                        bodies,
                        scope,
                        element_type,
                        initializer,
                        out,
                    );
                }
            }
            check_stmt_type_use(db, resolver, bodies, scope, element_type, body, out);
            for catch in catches {
                for ty in &catch.param_types {
                    check_type_use(db, resolver, scope, element_type, ty, out);
                }
                check_stmt_type_use(db, resolver, bodies, scope, element_type, catch.body, out);
            }
            if let Some(finally) = finally {
                check_stmt_type_use(db, resolver, bodies, scope, element_type, finally, out);
            }
        }
        S::Assert { cond, msg } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, cond, out);
            if let Some(msg) = msg {
                check_expr_type_use(db, resolver, bodies, scope, element_type, msg, out);
            }
        }
        S::Empty | S::Break(_) | S::Continue(_) | S::LocalClass { .. } | S::Missing => {}
    }
}

/// Checks the type references of one expression node, then recurses into its
/// children ([JLS §9.7.4] type-use contexts).
fn check_expr_type_use(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    bodies: &BodyTree,
    scope: &hir::ResolutionScope,
    element_type: Option<&'static str>,
    expr: ExprId,
    out: &mut Vec<DeclDiagnostic>,
) {
    use hir_expand::body::{ExprData as E, LambdaBody as L, SwitchLabel as SL};
    match bodies.expr(expr).clone() {
        E::Cast { ty, expr: inner } => {
            check_type_use(db, resolver, scope, element_type, &ty, out);
            check_expr_type_use(db, resolver, bodies, scope, element_type, inner, out);
        }
        E::New {
            ty, args, receiver, ..
        } => {
            check_type_use(db, resolver, scope, element_type, &ty, out);
            for arg in args {
                check_expr_type_use(db, resolver, bodies, scope, element_type, arg, out);
            }
            if let Some(receiver) = receiver {
                check_expr_type_use(db, resolver, bodies, scope, element_type, receiver, out);
            }
        }
        E::InstanceOf {
            expr: inner,
            ty: Some(ty),
            pattern,
        } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, inner, out);
            check_type_use(db, resolver, scope, element_type, &ty, out);
            if let Some(pattern) = pattern {
                check_pattern_type_use(db, resolver, bodies, scope, element_type, pattern, out);
            }
        }
        E::InstanceOf {
            expr: inner,
            ty: None,
            pattern,
        } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, inner, out);
            if let Some(pattern) = pattern {
                check_pattern_type_use(db, resolver, bodies, scope, element_type, pattern, out);
            }
        }
        E::ClassLit(ty) => {
            check_type_use(db, resolver, scope, element_type, &ty, out);
        }
        E::MethodRef {
            qualifier,
            type_name,
            ..
        } => {
            if let Some(qualifier) = qualifier {
                check_expr_type_use(db, resolver, bodies, scope, element_type, qualifier, out);
            }
            if let Some(ty) = type_name {
                check_type_use(db, resolver, scope, element_type, &ty, out);
            }
        }
        E::MethodCall {
            receiver,
            type_args,
            args,
            ..
        } => {
            if let Some(receiver) = receiver {
                check_expr_type_use(db, resolver, bodies, scope, element_type, receiver, out);
            }
            for ty in type_args {
                check_type_use(db, resolver, scope, element_type, &ty, out);
            }
            for arg in args {
                check_expr_type_use(db, resolver, bodies, scope, element_type, arg, out);
            }
        }
        E::Lambda { params, body } => {
            for (_, declared, _) in params {
                if let Some(ty) = declared {
                    check_type_use(db, resolver, scope, element_type, &ty, out);
                }
            }
            match body {
                L::Expr(expr) => {
                    check_expr_type_use(db, resolver, bodies, scope, element_type, expr, out);
                }
                L::Block(stmt) => {
                    check_stmt_type_use(db, resolver, bodies, scope, element_type, stmt, out);
                }
            }
        }
        E::NewArray {
            ty,
            dims,
            initializer,
        } => {
            check_type_use(db, resolver, scope, element_type, &ty, out);
            for dim in dims {
                check_expr_type_use(db, resolver, bodies, scope, element_type, dim, out);
            }
            if let Some(initializer) = initializer {
                for elem in initializer {
                    check_expr_type_use(db, resolver, bodies, scope, element_type, elem, out);
                }
            }
        }
        E::ArrayInit(elems) => {
            for elem in elems {
                check_expr_type_use(db, resolver, bodies, scope, element_type, elem, out);
            }
        }
        E::Unary { expr: inner, .. } | E::Postfix { expr: inner, .. } | E::Paren(inner) => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, inner, out);
        }
        E::Binary { lhs, rhs, .. } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, lhs, out);
            check_expr_type_use(db, resolver, bodies, scope, element_type, rhs, out);
        }
        E::Assign { lhs, rhs, .. } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, lhs, out);
            check_expr_type_use(db, resolver, bodies, scope, element_type, rhs, out);
        }
        E::Conditional {
            cond, then, els, ..
        } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, cond, out);
            check_expr_type_use(db, resolver, bodies, scope, element_type, then, out);
            check_expr_type_use(db, resolver, bodies, scope, element_type, els, out);
        }
        E::Switch { scrutinee, arms } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, scrutinee, out);
            for arm in arms {
                for label in &arm.labels {
                    if let SL::Expr(expr) = label {
                        check_expr_type_use(db, resolver, bodies, scope, element_type, *expr, out);
                    }
                }
                for stmt in arm.body {
                    check_stmt_type_use(db, resolver, bodies, scope, element_type, stmt, out);
                }
            }
        }
        E::CtorCall { args, .. } | E::Template { args } => {
            for arg in args {
                check_expr_type_use(db, resolver, bodies, scope, element_type, arg, out);
            }
        }
        E::ArrayAccess { array, index } => {
            check_expr_type_use(db, resolver, bodies, scope, element_type, array, out);
            check_expr_type_use(db, resolver, bodies, scope, element_type, index, out);
        }
        E::FieldAccess { target, .. } => {
            if let Some(target) = target {
                check_expr_type_use(db, resolver, bodies, scope, element_type, target, out);
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

/// The type references of a pattern ([JLS §14.30] type patterns and record
/// patterns).
fn check_pattern_type_use(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    bodies: &BodyTree,
    scope: &hir::ResolutionScope,
    element_type: Option<&'static str>,
    pattern: PatternId,
    out: &mut Vec<DeclDiagnostic>,
) {
    use hir_expand::body::{PatternData as P, TypePattern as TP};
    match bodies.pattern(pattern).clone() {
        P::Type(TP { ty, .. }) => {
            check_type_use(db, resolver, scope, element_type, &ty, out);
        }
        P::Record(pattern) => {
            check_type_use(db, resolver, scope, element_type, &pattern.ty, out);
            for component in pattern.components {
                check_pattern_type_use(db, resolver, bodies, scope, element_type, component, out);
            }
        }
        P::MatchAll => {}
    }
}

/// Checks the element-value arguments of one annotation against the elements
/// of its type ([JLS §9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1)):
/// each `name = value` pair must name a declared element exactly once
/// ([§9.6.1]), and the value must be assignable to the element's declared
/// type ([§5.2]). An annotation type that cannot be resolved (or is not an
/// annotation) has nothing to check — an unknown annotation is reported by
/// the name-resolution check.
fn check_annotation_elements(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    scope: &hir::ResolutionScope,
    annotation: &AnnotationRef,
    out: &mut Vec<DeclDiagnostic>,
) {
    if annotation.args.is_empty() {
        return;
    }
    let Some(elements) = annotation_type_elements(db, scope, resolver, &annotation.name.name)
    else {
        return;
    };
    for (idx, arg) in annotation.args.iter().enumerate() {
        // §9.7.1: no element may be given a value twice — the later pair is
        // the error.
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
            // §9.7.1: the pair names an element the annotation type does not
            // declare.
            out.push(DeclDiagnostic::UnknownAnnotationMember {
                name: arg.name.clone(),
                range: Some(arg.range),
            });
            continue;
        };
        check_value_assignable(db, resolver, scope, &arg.value, &element.ty, arg.range, out);
    }
}

/// Checks one annotation element value against the element's declared type
/// ([JLS §9.7.1], [§5.2]). `range` is the source range of the value, where
/// the mismatch is reported.
fn check_value_assignable(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    scope: &hir::ResolutionScope,
    value: &AnnotationValue,
    element_ty: &Ty,
    range: rowan::TextRange,
    out: &mut Vec<DeclDiagnostic>,
) {
    match value {
        // An array initializer ([§10.6]) is checked element-wise against the
        // component type; an initializer where the element is not an array is
        // a compile-time error ([§9.7.1]).
        AnnotationValue::Array(values) => match element_ty.kind(db) {
            TyKind::Array(component) => {
                for v in values {
                    check_value_assignable(db, resolver, scope, v, component, range, out);
                }
            }
            _ => out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                found: Ty::array(db, element_ty.clone()),
                expected: element_ty.clone(),
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
            check_single_value_assignable(db, resolver, scope, value, target, range, out);
        }
    }
}

/// The §9.7.1 single-value checks of [`check_value_assignable`], against the
/// effective target type `target` (the component type of an array-typed
/// element, or the element type itself).
fn check_single_value_assignable(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    scope: &hir::ResolutionScope,
    value: &AnnotationValue,
    target: &Ty,
    range: rowan::TextRange,
    out: &mut Vec<DeclDiagnostic>,
) {
    use hir_expand::span::AnnotationValue as V;
    match value {
        // A literal carries its primitive or `String` type ([§15.28]).
        V::Literal(lit) => {
            let value_ty = literal_ty(db, lit);
            // §5.2: an `int` constant narrows to `byte`/`short`/`char` when
            // its value fits; a value that does not fit is already rejected
            // by the assignment check below.
            if let (Literal::Int(v), TyKind::Primitive(p)) = (lit, target.kind(db))
                && narrows_to(*v, *p).is_some_and(|fits| fits)
            {
                return;
            }
            if !is_assignable(db, scope, &value_ty, target) {
                out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                    found: value_ty,
                    expected: target.clone(),
                    range: Some(range),
                });
            }
        }
        // An enum constant values its own enum type ([§8.9.1], [§15.8.1]).
        // A bare `CONST` infers its declaring type from the element's type
        // ([§9.7.1]); a qualified `E.CONST` names `E` explicitly.
        V::EnumConstant { qualifier, member } => {
            if let Some(qualifier) = qualifier {
                let Some(enum_ty) = resolve_name_ty(db, scope, resolver, qualifier) else {
                    return;
                };
                if let Some(constants) = enum_constants(db, scope, &enum_ty)
                    && !constants.iter().any(|c| c == member.as_str())
                {
                    out.push(DeclDiagnostic::UnknownAnnotationElementConstant {
                        member: member.clone(),
                        range: Some(range),
                    });
                }
                if !is_assignable(db, scope, &enum_ty, target) {
                    out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                        found: enum_ty,
                        expected: target.clone(),
                        range: Some(range),
                    });
                }
            } else if let Some(constants) = enum_constants(db, scope, target) {
                // §9.7.1: the bare constant's declaring type is the element's
                // type, which must be an enum declaring the constant.
                if !constants.iter().any(|c| c == member.as_str()) {
                    out.push(DeclDiagnostic::UnknownAnnotationElementConstant {
                        member: member.clone(),
                        range: Some(range),
                    });
                }
            } else {
                // The element's type is not an enum, so the bare constant has
                // no declaring type to resolve against ([§9.7.1]).
                out.push(DeclDiagnostic::UnknownAnnotationElementConstant {
                    member: member.clone(),
                    range: Some(range),
                });
            }
        }
        // A class literal `Foo.class` values `Class` ([§15.8.2]).
        V::ClassLit(_) => {
            let class = Ty::reference(db, "java.lang.Class", Vec::new());
            if !is_assignable(db, scope, &class, target) {
                out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                    found: class,
                    expected: target.clone(),
                    range: Some(range),
                });
            }
        }
        // A nested annotation values the annotation type it names
        // ([§9.7.1]); its own argument list is checked recursively.
        V::Annotation(inner) => {
            if let Some(inner_ty) = resolve_name_ty(db, scope, resolver, &inner.name.name) {
                if !is_assignable(db, scope, &inner_ty, target) {
                    out.push(DeclDiagnostic::AnnotationElementTypeMismatch {
                        found: inner_ty,
                        expected: target.clone(),
                        range: Some(range),
                    });
                }
            }
            check_annotation_elements(db, resolver, scope, inner, out);
        }
        // A non-constant element value (a unary/binary/conditional
        // expression) carries no standalone type; javac rejects it (an
        // annotation element value must be a §15.28 constant) but the raw
        // text gives nothing to compare — the syntax layer already holds the
        // failing parse.
        V::Unresolved { .. } => {}
        // Unreachable: [`check_value_assignable`] routes array values before
        // delegating a single value here.
        V::Array(_) => {}
    }
}

/// The [`Ty`] of an annotation element literal ([JLS §15.28]).
fn literal_ty(db: &dyn TyDatabase, lit: &Literal) -> Ty {
    match lit {
        Literal::Int(_) => Ty::primitive(db, PrimitiveType::Int),
        Literal::Long(_) => Ty::primitive(db, PrimitiveType::Long),
        Literal::Float => Ty::primitive(db, PrimitiveType::Float),
        Literal::Double => Ty::primitive(db, PrimitiveType::Double),
        Literal::Boolean(_) => Ty::primitive(db, PrimitiveType::Boolean),
        Literal::Char(_) => Ty::primitive(db, PrimitiveType::Char),
        Literal::Str(_) => Ty::reference(db, "java.lang.String", Vec::new()),
    }
}

/// Whether an `int` literal's value fits a narrower primitive target by the
/// §5.2 constant narrowing; `None` for targets that never narrow from `int`.
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
}

/// The elements of the annotation type `name` ([§9.6.1]), in declaration
/// order: each is an abstract method of the annotation declaration whose
/// return type ([§8.4.5]) is the element's declared type. `None` when `name`
/// does not resolve to an annotation type (nothing to check).
fn annotation_type_elements(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    name: &Name,
) -> Option<Vec<AnnotationElement>> {
    let fqn = candidate_fqns(resolver, name)
        .into_iter()
        .find(|candidate| hir::fqn_resolve(db, scope, candidate.as_str()).is_some())?;
    let fqn = fqn.as_str();
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
            let resolver = Resolver::new(&source_tree, &type_params, source.item);
            let mut out = Vec::new();
            for &child in source_tree.data(source.item).body() {
                if let ItemData::Method(method) = source_tree.data(child)
                    && let Some(ret) = &method.sig.ret
                {
                    out.push(AnnotationElement {
                        name: method.name.clone(),
                        ty: resolve_type_ref(db, &file_scope, &resolver, &ret.ty),
                    });
                }
            }
            Some(out)
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
            if class.methods.is_empty() {
                // A stub with no method records (the test fixture's minimal
                // annotations) or a partially-read classfile carries no
                // element information — an empty element list would report
                // every argument as an unknown member, so treat it as
                // uncheckable instead.
                return None;
            }
            Some(
                class
                    .methods
                    .iter()
                    .map(|method| AnnotationElement {
                        name: Name::new(db.hir_state().interner.resolve(&method.name)),
                        ty: ty_from_library(db, &method.return_type),
                    })
                    .collect(),
            )
        }
    }
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
                    let resolver = Resolver::new(&source_tree, &type_params, source.item);
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
    annotation: &AnnotationRef,
) -> bool {
    candidate_fqns(resolver, &annotation.name.name)
        .into_iter()
        .find(|candidate| hir::fqn_resolve(db, scope, candidate.as_str()).is_some())
        .is_some_and(|candidate| candidate.as_str() == "java.lang.annotation.Target")
}

/// The `ElementType` constant names of a `@Target` argument list: the enum
/// constants of the `value` element ([§9.7.1]), single or in an array.
fn target_value_names(args: &[AnnotationArg]) -> Vec<String> {
    let mut out = Vec::new();
    for arg in args {
        if arg.name.as_str() == "value" {
            collect_enum_names(&arg.value, &mut out);
        }
    }
    out
}

fn collect_enum_names(value: &AnnotationValue, out: &mut Vec<String>) {
    match value {
        AnnotationValue::Array(values) => {
            for value in values {
                collect_enum_names(value, out);
            }
        }
        AnnotationValue::EnumConstant { member, .. } => out.push(member.as_str().to_owned()),
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
