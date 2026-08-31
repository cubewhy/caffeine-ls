//! Cross-file dependency index of a source file.
//!
//! The LSP layer needs to know, when a file `A` changes, exactly which *other*
//! files' diagnostics may be invalidated by the edit — the IDEA-style
//! cross-file diagnostic experience. This module answers with two
//! complementary, salsa-pure queries per file `B`:
//!
//! * [`file_resolved_deps`]: every workspace source file `B`'s type outputs
//!   *actually* resolve to. Collected from the memoized HIR of `B` — the
//!   declared types (`item_ty_query`), method parameters
//!   (`method_params_query`) and inferred body types (`body_types_query`) —
//!   plus the transitive source-supertype closure of every referenced source
//!   class and of `B`'s own declared classes, so a member inherited from a
//!   *different* file is attributed to the file that declares it.
//! * [`file_dependency_refs`]: every *name* `B` resolves against that does not
//!   necessarily leave a [`crate::Ty`] footprint — the names of its type
//!   references, its imports and the simple names of the methods and fields it
//!   accesses. This is the sound fallback of the reverse-dependency index: a
//!   statically imported member (`import static p.A.*` used as `foo()` with no
//!   arguments and a void return type) leaves no reference type behind, but
//!   *does* consult the per-source-set symbol index by simple name, so the
//!   name must be recorded here.
//!
//! Both queries are keyed on the interned [`base_db::FileText`], so a text
//! edit invalidates exactly the edited file's result and salsa re-derives only
//! what changed. The LSP layer combines the two into a candidate set, then
//! verifies each candidate's diagnostics against its memoized digest to obtain
//! the exact affected set.

use rustc_hash::FxHashSet;
use vfs::FileId;

use hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use hir_expand::{
    body::{BodyId, BodyTree, ExprData, ExprId, StmtData, StmtId},
    name::Name,
    span::SpannedTypeRef,
};

use crate::{
    java::db::{ItemKey, TyDatabase, body_types_query, item_ty_query, method_params_query},
    java::name_check::{body_type_refs, expr_forest_type_refs, item_type_refs},
    java::subtyping::source_supertypes,
};

/// The workspace source files whose declarations `file`'s type outputs resolve
/// against: the files declaring every reference type that appears in
/// `file`'s memoized types, transitively closed over source-side supertypes
/// (both of the referenced classes and of `file`'s own declared classes), so
/// members inherited from a different file are attributed to their declaring
/// file.
pub(crate) fn file_resolved_deps_impl(db: &dyn TyDatabase, file: FileId) -> FxHashSet<FileId> {
    let scope = crate::java::resolve::scope_for_file(db, file);
    let tree = hir::file_item_tree(db, file);

    let mut out: FxHashSet<FileId> = FxHashSet::default();
    // Classes still to expand their supertype chains; `visited` keys on the
    // (file, item) pair so a chain reaching the same class twice (a diamond
    // like `interface I extends A, B` where `A` and `B` both extend `C`)
    // expands it once. The pair also bounds broken self-cyclic source
    // (`class A extends A`), which would otherwise loop forever.
    let mut queue: Vec<hir::SourceClass> = Vec::new();
    let mut visited: FxHashSet<(FileId, ItemId)> = FxHashSet::default();

    // 1. The reference types of the file's memoized HIR: declared types,
    //    parameter types and inferred body types (expression and local types,
    //    which also cover field-initializer and enum-constant-argument
    //    forests, whose inferred types land in `BodyTypes::exprs`).
    {
        // Recording a reference name resolved to a source class: the declaring
        // file becomes a dependency and the class joins the supertype-closure
        // queue. Resolution happens against `file`'s own scope, so a supertype
        // FQN recovered from a *referenced* class (whose superclass references
        // were resolved in that class's own file scope) still resolves when
        // the class and its supertype are reachable on `file`'s classpath —
        // which they necessarily are, since `file` resolves the class itself.
        // The closure borrows `out`/`queue`/`visited` only for this pass; the
        // work-queue loop below needs its own mutable borrow of `queue`.
        let mut record = |name: &Name| {
            record_source(db, file, &scope, name, &mut out, &mut queue, &mut visited);
        };
        for item in all_items(&tree) {
            let key = ItemKey::new(db, file, item);
            item_ty_query(db, key).for_each_reference(db, &mut record);
            if tree.data(item).is_method() {
                for param in method_params_query(db, key) {
                    param.for_each_reference(db, &mut record);
                }
            }
            if let Some(types) = body_types_query(db, key) {
                for ty in types.exprs.values() {
                    ty.for_each_reference(db, &mut record);
                }
                for ty in types.locals.values() {
                    ty.for_each_reference(db, &mut record);
                }
            }
        }
    }

    // 2. The inheritance edges of the file's own class-like declarations: a
    //    superclass or interface declared in another file carries the members
    //    `file` inherits, so a change there invalidates `file`'s member
    //    resolution even when nothing in `file` names the supertype directly.
    for (id, data) in all_items_data(&tree) {
        if is_class_like(data) {
            let source = hir::SourceClass { file, item: id };
            for super_ty in source_supertypes(db, source, &[]) {
                super_ty.for_each_reference(db, &mut |name| {
                    record_source(db, file, &scope, name, &mut out, &mut queue, &mut visited);
                });
            }
        }
    }

    // 3. Transitive source-supertype closure of every class recorded above,
    //    mirroring the member-set walk ([§15.12.1]): a method `a.foo()` whose
    //    receiver is `A` but whose declaration lives on `A`'s superclass `C`
    //    (in a third file) only depends on `C`'s file through this walk.
    while let Some(source) = queue.pop() {
        for super_ty in source_supertypes(db, source, &[]) {
            super_ty.for_each_reference(db, &mut |name| {
                record_source(db, file, &scope, name, &mut out, &mut queue, &mut visited);
            });
        }
    }

    out
}

/// The shared step of [`file_resolved_deps_impl`]: a reference name resolved
/// against `scope` is a cross-file dependency when it names a source class in
/// a different file, which then joins the supertype-closure queue.
fn record_source(
    db: &dyn TyDatabase,
    file: FileId,
    scope: &hir::ResolutionScope,
    name: &Name,
    out: &mut FxHashSet<FileId>,
    queue: &mut Vec<hir::SourceClass>,
    visited: &mut FxHashSet<(FileId, ItemId)>,
) {
    if let Some(hir::Resolved::Source(source)) = hir::fqn_resolve(db, scope, name.as_str()) {
        // `file`'s own declarations are not a *cross*-file dependency.
        if source.file == file {
            return;
        }
        out.insert(source.file);
        if visited.insert((source.file, source.item)) {
            queue.push(source);
        }
    }
}

/// Every *name* `file` resolves against that may not be recoverable from its
/// types: the canonical and simple names of its type references (declaration
/// and body positions), its imports, and the simple names of every method and
/// field it accesses. Used as the sound fallback of the reverse-dependency
/// index — a change in a file cannot alter `file`'s resolution without either
/// appearing in this set or being reachable through [`file_resolved_deps`].
pub(crate) fn file_dependency_refs_impl(db: &dyn TyDatabase, file: FileId) -> FxHashSet<Name> {
    let tree = hir::file_item_tree(db, file);
    let bodies = hir::file_body_tree(db, file);
    let mut out: FxHashSet<Name> = FxHashSet::default();

    // Imports: the full name and its leaf simple name, so a symbol added or
    // renamed in its target file re-matches the import.
    for import in &tree.imports {
        out.insert(import.name.clone());
        out.insert(Name::new(import.name.simple_name()));
    }

    for (_id, data) in all_items_data(&tree) {
        // Declaration-level type references (superclass/interfaces, field
        // types, signatures, type-parameter bounds, record components).
        for spanned in item_type_refs(data) {
            collect_type_ref_names(spanned, &mut out);
        }
        match data {
            ItemData::Method(method) => {
                if let Some(body) = method.body() {
                    collect_body_names(&bodies, body, &mut out);
                }
                if let Some(default) = method.default_expr() {
                    collect_expr_forest_names(&bodies, &[default], &mut out);
                }
            }
            ItemData::StaticInit(init) => {
                if let Some(body) = init.body {
                    collect_body_names(&bodies, body, &mut out);
                }
            }
            ItemData::InstanceInit(init) => {
                if let Some(body) = init.body {
                    collect_body_names(&bodies, body, &mut out);
                }
            }
            ItemData::Field(field) => {
                if let Some(init) = field.initializer_expr {
                    collect_expr_forest_names(&bodies, &[init], &mut out);
                }
            }
            ItemData::EnumConstant(constant) => {
                collect_expr_forest_names(&bodies, &constant.argument_exprs, &mut out);
            }
            _ => {}
        }
    }

    out
}

/// The reference names of a lowered type reference, depth-first.
fn collect_type_ref_names(spanned: &SpannedTypeRef, out: &mut FxHashSet<Name>) {
    for reference in &spanned.refs {
        out.insert(reference.name.clone());
    }
}

/// The names of a body: the reference names of its type references plus the
/// simple names of the methods and fields it accesses.
fn collect_body_names(bodies: &BodyTree, body: BodyId, out: &mut FxHashSet<Name>) {
    for (_, spanned) in body_type_refs(bodies, body) {
        collect_type_ref_names(&spanned, out);
    }
    collect_member_names(bodies, &bodies.body(body).stmts, out);
}

/// The names of an expression forest (a field initializer, enum constant
/// arguments or an annotation element default): same rule as
/// [`collect_body_names`].
fn collect_expr_forest_names(bodies: &BodyTree, exprs: &[ExprId], out: &mut FxHashSet<Name>) {
    for (_, spanned) in expr_forest_type_refs(bodies, exprs) {
        collect_type_ref_names(&spanned, out);
    }
    for &expr in exprs {
        walk_expr_members(bodies, expr, out);
    }
}

/// Walks a body's statements recording the simple name of every method call
/// and field access, which — unlike the local variables, literals and
/// control-flow names — are the names that can resolve to statically imported
/// members ([JLS §7.5.4]) and thus consult the symbol index by simple name.
fn collect_member_names(bodies: &BodyTree, stmts: &[StmtId], out: &mut FxHashSet<Name>) {
    for &stmt in stmts {
        walk_stmt_members(bodies, stmt, out);
    }
}

fn walk_stmt_members(bodies: &BodyTree, id: StmtId, out: &mut FxHashSet<Name>) {
    use StmtData::*;
    match bodies.stmt(id) {
        Empty | Missing | LocalClass { .. } => {}
        Block(stmts) => {
            for &stmt in stmts {
                walk_stmt_members(bodies, stmt, out);
            }
        }
        Decl { initializer, .. } => {
            if let Some(init) = initializer {
                walk_expr_members(bodies, *init, out);
            }
        }
        Return(Some(expr)) | Throw(expr) | Yield(expr) => walk_expr_members(bodies, *expr, out),
        DeclGroup(stmts) => {
            for &stmt in stmts {
                walk_stmt_members(bodies, stmt, out);
            }
        }
        Expr(expr) => walk_expr_members(bodies, *expr, out),
        Labeled { stmt, .. } => walk_stmt_members(bodies, *stmt, out),
        If { cond, then, els } => {
            walk_expr_members(bodies, *cond, out);
            walk_stmt_members(bodies, *then, out);
            if let Some(els) = els {
                walk_stmt_members(bodies, *els, out);
            }
        }
        While { cond, body } => {
            walk_expr_members(bodies, *cond, out);
            walk_stmt_members(bodies, *body, out);
        }
        DoWhile { body, cond } => {
            walk_stmt_members(bodies, *body, out);
            walk_expr_members(bodies, *cond, out);
        }
        For {
            init,
            cond,
            step,
            body,
        } => {
            for &stmt in init {
                walk_stmt_members(bodies, stmt, out);
            }
            if let Some(cond) = cond {
                walk_expr_members(bodies, *cond, out);
            }
            for &step in step {
                walk_expr_members(bodies, step, out);
            }
            walk_stmt_members(bodies, *body, out);
        }
        ForEach { iterable, body, .. } => {
            walk_expr_members(bodies, *iterable, out);
            walk_stmt_members(bodies, *body, out);
        }
        Switch { scrutinee, arms } => {
            walk_expr_members(bodies, *scrutinee, out);
            for arm in arms {
                for label in &arm.labels {
                    match label {
                        hir_expand::body::SwitchLabel::Expr(expr)
                        | hir_expand::body::SwitchLabel::Guard(expr) => {
                            walk_expr_members(bodies, *expr, out);
                        }
                        hir_expand::body::SwitchLabel::Pattern(_) => {}
                    }
                }
                for &stmt in &arm.body {
                    walk_stmt_members(bodies, stmt, out);
                }
            }
        }
        Return(None) | Break(_) | Continue(_) => {}
        Synchronized { expr, body } => {
            walk_expr_members(bodies, *expr, out);
            walk_stmt_members(bodies, *body, out);
        }
        Try {
            resources,
            body,
            catches,
            finally,
        } => {
            for resource in resources {
                if let Some(init) = resource.initializer {
                    walk_expr_members(bodies, init, out);
                }
            }
            walk_stmt_members(bodies, *body, out);
            for catch in catches {
                walk_stmt_members(bodies, catch.body, out);
            }
            if let Some(finally) = finally {
                walk_stmt_members(bodies, *finally, out);
            }
        }
        Assert { cond, msg } => {
            walk_expr_members(bodies, *cond, out);
            if let Some(msg) = msg {
                walk_expr_members(bodies, *msg, out);
            }
        }
    }
}

fn walk_expr_members(bodies: &BodyTree, id: ExprId, out: &mut FxHashSet<Name>) {
    use ExprData::*;
    match bodies.expr(id) {
        Literal(_) | Null | This { .. } | Super { .. } | ClassLit(_) | ArrayInit(_) => {}
        Var(_) => {}
        NamePath(name) => {
            out.insert(name.clone());
            out.insert(Name::new(name.simple_name()));
        }
        FieldAccess { target, name } => {
            out.insert(name.clone());
            if let Some(target) = target {
                walk_expr_members(bodies, *target, out);
            }
        }
        ArrayAccess { array, index } => {
            walk_expr_members(bodies, *array, out);
            walk_expr_members(bodies, *index, out);
        }
        MethodCall {
            receiver,
            name,
            args,
            ..
        } => {
            out.insert(name.clone());
            if let Some(receiver) = receiver {
                walk_expr_members(bodies, *receiver, out);
            }
            for &arg in args {
                walk_expr_members(bodies, arg, out);
            }
        }
        New { args, receiver, .. } => {
            for &arg in args {
                walk_expr_members(bodies, arg, out);
            }
            if let Some(receiver) = receiver {
                walk_expr_members(bodies, *receiver, out);
            }
        }
        CtorCall { args, .. } => {
            for &arg in args {
                walk_expr_members(bodies, arg, out);
            }
        }
        NewArray {
            dims, initializer, ..
        } => {
            for &dim in dims {
                walk_expr_members(bodies, dim, out);
            }
            if let Some(elems) = initializer {
                for &elem in elems {
                    walk_expr_members(bodies, elem, out);
                }
            }
        }
        Template { args } => {
            for &arg in args {
                walk_expr_members(bodies, arg, out);
            }
        }
        Unary { expr, .. } | Postfix { expr, .. } => walk_expr_members(bodies, *expr, out),
        Binary { lhs, rhs, .. } => {
            walk_expr_members(bodies, *lhs, out);
            walk_expr_members(bodies, *rhs, out);
        }
        Assign { lhs, rhs, .. } => {
            walk_expr_members(bodies, *lhs, out);
            walk_expr_members(bodies, *rhs, out);
        }
        Cast { expr, .. } => walk_expr_members(bodies, *expr, out),
        InstanceOf { expr, .. } => walk_expr_members(bodies, *expr, out),
        Conditional { cond, then, els } => {
            walk_expr_members(bodies, *cond, out);
            walk_expr_members(bodies, *then, out);
            walk_expr_members(bodies, *els, out);
        }
        Switch { scrutinee, arms } => {
            walk_expr_members(bodies, *scrutinee, out);
            for arm in arms {
                for label in &arm.labels {
                    match label {
                        hir_expand::body::SwitchLabel::Expr(expr)
                        | hir_expand::body::SwitchLabel::Guard(expr) => {
                            walk_expr_members(bodies, *expr, out);
                        }
                        hir_expand::body::SwitchLabel::Pattern(_) => {}
                    }
                }
                for &stmt in &arm.body {
                    walk_stmt_members(bodies, stmt, out);
                }
            }
        }
        Paren(expr) => walk_expr_members(bodies, *expr, out),
        Missing => {}
        Lambda { params, body } => {
            for (_, ty, _) in params {
                let _ = ty; // parameter types are collected by the type-ref walk
            }
            match body {
                hir_expand::body::LambdaBody::Expr(expr) => walk_expr_members(bodies, *expr, out),
                hir_expand::body::LambdaBody::Block(stmt) => walk_stmt_members(bodies, *stmt, out),
            }
        }
        MethodRef { qualifier, .. } => {
            if let Some(qualifier) = qualifier {
                walk_expr_members(bodies, *qualifier, out);
            }
        }
    }
}

/// Every `ItemId` of the tree, parents before children, in a stable order.
fn all_items(tree: &ItemTree) -> Vec<ItemId> {
    all_items_data(tree).into_iter().map(|(id, _)| id).collect()
}

fn all_items_data(tree: &ItemTree) -> Vec<(ItemId, &ItemData)> {
    fn walk<'a>(tree: &'a ItemTree, id: ItemId, out: &mut Vec<(ItemId, &'a ItemData)>) {
        let data = tree.data(id);
        out.push((id, data));
        for &child in data.body() {
            walk(tree, child, out);
        }
    }
    let mut out = Vec::new();
    for &top in &tree.top {
        walk(tree, top, &mut out);
    }
    out
}

/// Whether a declaration is class-like: its supertype chain is a source of
/// inherited members ([JLS §8.1], [§9.1]) and thus a dependency of the file.
fn is_class_like(data: &ItemData) -> bool {
    matches!(
        data,
        ItemData::Class(_)
            | ItemData::Interface(_)
            | ItemData::Enum(_)
            | ItemData::Record(_)
            | ItemData::Annotation(_)
    )
}
