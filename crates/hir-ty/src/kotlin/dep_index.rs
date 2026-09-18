//! Cross-file dependency index of a Kotlin source file — the Kotlin twin of
//! [`crate::java::dep_index`].
//!
//! The LSP layer needs to know, when a file `A` changes, exactly which *other*
//! files' diagnostics may be invalidated by the edit. This module answers with
//! the same two complementary, salsa-pure queries per file `B`:
//!
//! * [`file_resolved_deps_impl`]: every workspace source file `B`'s type
//!   outputs *actually* resolve to — the imports it resolves, the type
//!   references of its declaration model ([`ItemTypeRef`]) and of the body IR
//!   the lowering stored (the inferred expression and local types of every
//!   declaration, which name the types those references resolved to), and the
//!   declarations its bodies resolved their names to
//!   ([`KotlinBodyTypes::resolved`]) — plus the transitive source-supertype
//!   closure of every referenced source class, so a member inherited from a
//!   *different* file is attributed to the file that declares it.
//! * [`file_dependency_refs_impl`]: every *name* `B` resolves against that does
//!   not necessarily leave a [`crate::Ty`] footprint — the names of its
//!   imports, of its type references and of the names its expressions write.
//!   This is the sound fallback of the reverse-dependency index: a call to
//!   another file's `Unit`-returning function leaves no reference type behind,
//!   and a name that resolves to nothing leaves nothing at all, yet an edit to
//!   the file it names can change what `B` resolves.
//!
//! Both queries are keyed on the interned [`base_db::FileText`], so a text edit
//! invalidates exactly the edited file's result and salsa re-derives only what
//! changed. The LSP layer combines the two into a candidate set, then verifies
//! each candidate's diagnostics against its memoized digest to obtain the exact
//! affected set.

use base_db::{FileText, salsa};
use rustc_hash::FxHashSet;
use triomphe::Arc;
use vfs::FileId;

use hir_def::jvm::decl::ItemTypeRef;
use hir_def::kotlin::item_tree::{KotlinItemData, KotlinTypeParam};
use hir_expand::body::{ExprData, PatternData, WhenCondition};
use hir_expand::ids::ItemId;
use hir_expand::name::Name;
use hir_expand::span::SpannedTypeRef;

use crate::jvm::db::TyDatabase;
use crate::kotlin::db;
use crate::kotlin::infer::KotlinResolvedMember;
use crate::kotlin::resolve::KotlinResolver;
use crate::kotlin::ty::ty_from_type_ref;
use crate::ty::Ty;

/// The workspace source files whose declarations `file`'s type outputs resolve
/// against — the exact cross-file dependency set of `file`. Tracked on the
/// interned [`FileText`], so a text edit to `file` invalidates exactly its
/// dependency set and leaves every other file's memo untouched. See
/// [`file_resolved_deps_impl`].
#[salsa::tracked(returns(ref))]
pub(crate) fn file_resolved_deps_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Arc<FxHashSet<FileId>> {
    let file_id = *file.file_id(db);
    Arc::new(file_resolved_deps_impl(db, file_id))
}

/// The workspace source files whose declarations `file`'s type outputs resolve
/// against.
pub fn file_resolved_deps(db: &dyn TyDatabase, file_id: FileId) -> Arc<FxHashSet<FileId>> {
    file_resolved_deps_query(db, db.file_text(file_id)).clone()
}

/// The resolution-relevant *names* of `file` — the sound name-level fallback of
/// the cross-file dependency index. See [`file_dependency_refs_impl`].
#[salsa::tracked(returns(ref))]
pub(crate) fn file_dependency_refs_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Arc<FxHashSet<Name>> {
    let file_id = *file.file_id(db);
    Arc::new(file_dependency_refs_impl(db, file_id))
}

/// The resolution-relevant names of `file`.
pub fn file_dependency_refs(db: &dyn TyDatabase, file_id: FileId) -> Arc<FxHashSet<Name>> {
    file_dependency_refs_query(db, db.file_text(file_id)).clone()
}

/// The workspace source files whose declarations `file`'s type outputs resolve
/// against, as [`file_resolved_deps`] serves them.
///
/// The set is built from three sources:
///
/// 1. the file's imports, whose paths name the classes and file facades its
///    resolution consults (KLS
///    `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing));
/// 2. the type references of every declaration of the file's item tree
///    ([`item_type_refs`]), resolved through the item's own scopes
///    ([`KotlinResolver::for_item`]) and walked for every reference type they
///    contain ([`Ty::for_each_reference`]);
/// 3. the resolved targets of the file's bodies — the declarations and members
///    every name of every body resolved to, which a call to another file's
///    `Unit`-returning function leaves as its only trace, and the inferred
///    expression and local types of those bodies, which carry the types a
///    body-written reference resolved to.
///
/// Every source class recorded by 2 or 3 is then transitively closed over its
/// source supertypes ([`source_supertypes`]): a member inherited from a class
/// of a *third* file makes that file a dependency of `file` too, exactly as the
/// member set that resolves it walks the same chain.
pub(crate) fn file_resolved_deps_impl(db: &dyn TyDatabase, file: FileId) -> FxHashSet<FileId> {
    let Some(tree) = hir_def::kotlin::plugin::tree(db, file) else {
        return FxHashSet::default();
    };
    let scope = scope_for_file(db, file);

    let mut out: FxHashSet<FileId> = FxHashSet::default();
    // Classes still to expand their supertype chains; `visited` keys on the
    // (file, item) pair so a chain reaching the same class twice (a diamond, or
    // a broken self-cyclic source) expands it once.
    let mut queue: Vec<hir::SourceClass> = Vec::new();
    let mut visited: FxHashSet<(FileId, ItemId)> = FxHashSet::default();

    // 1. The imported declarations: an import names a class or a file facade
    //    the file's resolution consults even before any use of the name.
    for import in &tree.imports {
        if let Some(resolved) = hir::fqn_resolve(db, &scope, import.path.as_str()) {
            record_class(&resolved, file, &mut out, &mut queue, &mut visited);
        }
    }

    for (id, data) in tree.items.iter() {
        let item = ItemId(id);
        // The declaration- and body-side references, walked as resolved types.
        {
            let mut record = |name: &Name, local: Option<hir::SourceClass>| {
                record_source(
                    db,
                    file,
                    &scope,
                    name,
                    local,
                    &mut out,
                    &mut queue,
                    &mut visited,
                );
            };
            let resolver = KotlinResolver::for_item(db, file, &tree, item);
            for tyref in item_type_refs(data) {
                ty_from_type_ref(db, &resolver, &tyref.ty).for_each_reference(db, &mut record);
            }
            let types = db::declaration_types(db, file, item);
            for ty in types.exprs.values() {
                ty.for_each_reference(db, &mut record);
            }
            for ty in types.locals.values() {
                ty.for_each_reference(db, &mut record);
            }
        }
        // The declarations the body resolved its *names* to, which carry no
        // type of their own (a `Unit`-returning function of another file, a
        // property read whose value is a built-in).
        for member in db::declaration_types(db, file, item).resolved.values() {
            record_member(db, member, file, &mut out, &mut queue, &mut visited);
        }
    }

    // 2. The transitive source-supertype closure of everything recorded above.
    while let Some(source) = queue.pop() {
        for super_ty in source_supertypes(db, source) {
            super_ty.for_each_reference(db, &mut |name, local| {
                record_source(
                    db,
                    file,
                    &scope,
                    name,
                    local,
                    &mut out,
                    &mut queue,
                    &mut visited,
                );
            });
        }
    }

    out
}

/// The resolution scope `file` resolves a name in: its source set when a source
/// root owns the file, the JDK built-ins alone otherwise.
fn scope_for_file(db: &dyn TyDatabase, file: FileId) -> hir::ResolutionScope {
    match hir::source_set_for_file(db, file) {
        Some(source_set) => hir::ResolutionScope::SourceSet(source_set),
        None => hir::ResolutionScope::JdkBuiltins,
    }
}

/// The shared step of [`file_resolved_deps_impl`]: a reference name resolved
/// against `scope` is a cross-file dependency when it names a source class — or
/// the file facade the compiler synthesizes for a workspace file's top-level
/// declarations
/// (<https://kotlinlang.org/docs/java-interop.html#package-level-functions>) —
/// in a *different* file. A source class joins the supertype-closure queue.
///
/// A reference to a *local* type carries its declaration directly
/// ([`crate::ty::TyKind::Reference`]'s `local`): the declaration is in `file`
/// itself, so it is never a cross-file dependency and is skipped without a name
/// lookup — a local classifier has no canonical name ([KLS
/// `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)).
#[allow(clippy::too_many_arguments)]
fn record_source(
    db: &dyn TyDatabase,
    file: FileId,
    scope: &hir::ResolutionScope,
    name: &Name,
    local: Option<hir::SourceClass>,
    out: &mut FxHashSet<FileId>,
    queue: &mut Vec<hir::SourceClass>,
    visited: &mut FxHashSet<(FileId, ItemId)>,
) {
    if local.is_some() {
        return;
    }
    if let Some(resolved) = hir::fqn_resolve(db, scope, name.as_str()) {
        record_class(&resolved, file, out, queue, visited);
    }
}

/// Records the source file `resolved` denotes as a dependency of `file`, and
/// queues a source *class* for the supertype-closure walk. `file`'s own
/// declarations are not a *cross*-file dependency.
fn record_class(
    resolved: &hir::Resolved,
    file: FileId,
    out: &mut FxHashSet<FileId>,
    queue: &mut Vec<hir::SourceClass>,
    visited: &mut FxHashSet<(FileId, ItemId)>,
) {
    match resolved {
        hir::Resolved::Source(source) => {
            if source.file == file {
                return;
            }
            out.insert(source.file);
            if visited.insert((source.file, source.item)) {
                queue.push(*source);
            }
        }
        // A facade is the file its top-level declarations live in; it declares
        // no class of its own, so it has no supertype chain to close over.
        hir::Resolved::Facade { file: facade, .. } => {
            if *facade != file {
                out.insert(*facade);
            }
        }
        hir::Resolved::Library(_) => {}
    }
}

/// Records the declaration a body's name resolved to
/// ([`KotlinBodyTypes::resolved`]) as a dependency of `file` — the resolved
/// call targets (KLS
/// `overload-resolution.html`](https://kotlinlang.org/spec/overload-resolution.html)):
/// a member of a *source* class carries its declaring file, so the file that
/// declares the member is a dependency even when the receiver type left no
/// reference behind.
fn record_member(
    db: &dyn TyDatabase,
    member: &KotlinResolvedMember,
    file: FileId,
    out: &mut FxHashSet<FileId>,
    queue: &mut Vec<hir::SourceClass>,
    visited: &mut FxHashSet<(FileId, ItemId)>,
) {
    match member {
        KotlinResolvedMember::Kotlin { file: target, item } => {
            if *target == file {
                return;
            }
            out.insert(*target);
            // Only a classifier has a supertype chain of its own to close over;
            // a function or property target contributes its file alone (the
            // receiver it was resolved on is walked through its own type).
            if is_source_class(db, *target, *item) && visited.insert((*target, *item)) {
                queue.push(hir::SourceClass {
                    file: *target,
                    item: *item,
                });
            }
        }
        // A Java source or classfile member: `owner_file` carries the workspace
        // file that declares it, `None` for a classfile.
        KotlinResolvedMember::Java(method) => {
            if let Some(owner) = method.owner_file
                && owner != file
            {
                out.insert(owner);
            }
        }
        KotlinResolvedMember::JavaField(field) => {
            if let Some(owner) = field.owner_file
                && owner != file
            {
                out.insert(owner);
            }
        }
        KotlinResolvedMember::Local(_) => {}
    }
}

/// Whether `item` of `file` is a classifier declaration of a *Kotlin* file —
/// the only kind of source declaration with a declared supertype list.
fn is_source_class(db: &dyn TyDatabase, file: FileId, item: ItemId) -> bool {
    let items = hir::file_item_tree(db, file);
    hir_def::kotlin::plugin::model(&items)
        .is_some_and(|tree| matches!(tree.data(item), KotlinItemData::Class(_)))
}

/// The declared supertypes of a *source* classifier, answered by the language
/// that declares it: Kotlin's own supertype walk for a Kotlin file (KLS
/// `declarations.html#supertype-specifiers`](https://kotlinlang.org/spec/declarations.html#supertype-specifiers)),
/// the Java layer's for a Java source class a Kotlin file can inherit from —
/// whose own supertypes stay in that layer.
fn source_supertypes(db: &dyn TyDatabase, source: hir::SourceClass) -> Vec<Ty> {
    let items = hir::file_item_tree(db, source.file);
    if hir_def::kotlin::plugin::model(&items).is_some() {
        return db::supertypes(db, source.file, source.item).to_vec();
    }
    crate::java::subtyping::source_supertypes(db, source, &[])
}

/// The named type references of a *declaration* item ([KLS
/// `type-system.html#classifier-types`](https://kotlinlang.org/spec/type-system.html#classifier-types)):
/// a classifier's supertypes and its type parameters' bounds, a callable's
/// receiver, parameter and return types, a property's receiver and declared
/// type, and a type alias's target.
fn item_type_refs(data: &KotlinItemData) -> Vec<&ItemTypeRef> {
    fn collect_bounds<'a>(params: &'a [KotlinTypeParam], out: &mut Vec<&'a ItemTypeRef>) {
        for param in params {
            out.extend(param.bounds.iter());
        }
    }
    let mut out = Vec::new();
    match data {
        KotlinItemData::Class(data) => {
            for super_type in &data.super_types {
                out.push(&super_type.ty);
            }
            collect_bounds(&data.type_params, &mut out);
        }
        KotlinItemData::Constructor(data) => {
            for param in &data.params {
                out.push(&param.param.ty);
            }
        }
        KotlinItemData::Function(data) => {
            collect_bounds(&data.type_params, &mut out);
            if let Some(receiver) = &data.receiver {
                out.push(receiver);
            }
            for param in &data.params {
                out.push(&param.param.ty);
            }
            if let Some(ret) = &data.ret {
                out.push(ret);
            }
        }
        KotlinItemData::Accessor(data) => {
            for param in &data.params {
                out.push(&param.param.ty);
            }
        }
        KotlinItemData::Property(data) => {
            collect_bounds(&data.type_params, &mut out);
            if let Some(receiver) = &data.receiver {
                out.push(receiver);
            }
            if let Some(ty) = &data.ty {
                out.push(ty);
            }
        }
        KotlinItemData::TypeAlias(data) => {
            collect_bounds(&data.type_params, &mut out);
            out.push(&data.target);
        }
        KotlinItemData::EnumEntry(_) | KotlinItemData::AnonymousInitializer(_) => {}
    }
    out
}

/// Every *name* `file` resolves against that may not be recoverable from its
/// types — the sound fallback of the reverse-dependency index. A change in
/// another file cannot alter `file`'s resolution without either appearing in
/// this set or being reachable through [`file_resolved_deps_impl`]:
///
/// * the path of every import with its leaf simple name, so a declaration
///   renamed in the import's target re-matches the import (KLS
///   `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing));
/// * the reference names of every declaration's type references
///   ([`item_type_refs`]'s `refs`), which the item tree keeps without a
///   resolution;
/// * the names the body IR records — the type references the lowering stored on
///   the file's locals, expressions, catch clauses and patterns, and the member,
///   call and single names its expressions write. An expression written as a
///   plain name (`helper()`) leaves no *resolved* trace when it resolves to
///   nothing, and it is exactly such a name that an edit in another file can
///   make resolve.
pub(crate) fn file_dependency_refs_impl(db: &dyn TyDatabase, file: FileId) -> FxHashSet<Name> {
    let Some(tree) = hir_def::kotlin::plugin::tree(db, file) else {
        return FxHashSet::default();
    };
    let mut out: FxHashSet<Name> = FxHashSet::default();

    for import in &tree.imports {
        out.insert(import.path.clone());
        out.insert(Name::new(import.path.simple_name()));
        // The binding an aliased import introduces (`import a.b.C as D`) is the
        // name the file resolves through the import.
        if let Some(alias) = &import.alias {
            out.insert(alias.clone());
        }
    }

    for (_, data) in tree.items.iter() {
        for tyref in item_type_refs(data) {
            out.extend(tyref.refs.iter().cloned());
        }
    }

    let bodies = hir::file_body_tree(db, file);
    for (_, local) in bodies.locals.iter() {
        if let Some(ty) = &local.ty {
            collect_type_names(ty, &mut out);
        }
        for annotation in &local.annotations {
            out.insert(annotation.name.name.clone());
        }
    }
    for (_, expr) in bodies.exprs.iter() {
        collect_expr_names(expr, &mut out);
        for ty in expr_type_refs(expr) {
            collect_type_names(ty, &mut out);
        }
    }
    for (_, stmt) in bodies.stmts.iter() {
        if let hir_expand::body::StmtData::Try { catches, .. } = stmt {
            for catch in catches {
                for ty in &catch.param_types {
                    collect_type_names(ty, &mut out);
                }
            }
        }
    }
    for (_, pattern) in bodies.patterns.iter() {
        if let PatternData::Type(data) = pattern {
            collect_type_names(&data.ty, &mut out);
        }
    }

    out
}

/// The reference names of a lowered type reference, in the order the lowering
/// kept them (KLS
/// `type-system.html#classifier-types`](https://kotlinlang.org/spec/type-system.html#classifier-types)).
fn collect_type_names(ty: &SpannedTypeRef, out: &mut FxHashSet<Name>) {
    for reference in &ty.refs {
        out.insert(reference.name.clone());
    }
}

/// The names a body expression writes: the single names, member names and call
/// names a resolution consults — the names another file can rename out from
/// under the file.
fn collect_expr_names(expr: &ExprData, out: &mut FxHashSet<Name>) {
    match expr {
        ExprData::Var(name)
        | ExprData::NamePath(name)
        | ExprData::FieldAccess { name, .. }
        | ExprData::MethodCall { name, .. }
        | ExprData::InfixCall { name, .. }
        | ExprData::CallableReference { name, .. }
        | ExprData::MethodRef { name, .. } => {
            out.insert(name.clone());
        }
        _ => {}
    }
}

/// The type references a body expression *writes*: a cast's, type test's or
/// constructor invocation's type, a class literal's type, a call's written type
/// arguments, a qualified `this`, and a lambda parameter's declared type.
fn expr_type_refs(expr: &ExprData) -> Vec<&SpannedTypeRef> {
    let mut out = Vec::new();
    match expr {
        ExprData::This { qualifier } | ExprData::Super { qualifier } => {
            out.extend(qualifier.iter())
        }
        ExprData::ClassLit(ty) => out.push(ty),
        ExprData::MethodCall { type_args, .. } => out.extend(type_args.iter()),
        ExprData::New { ty, .. } | ExprData::NewArray { ty, .. } | ExprData::Cast { ty, .. } => {
            out.push(ty);
        }
        ExprData::InstanceOf { ty, .. } => out.extend(ty.iter()),
        ExprData::MethodRef { type_name, .. } => out.extend(type_name.iter()),
        ExprData::Lambda { params, .. } => {
            for param in params {
                out.extend(param.ty.iter());
            }
        }
        ExprData::When { arms, .. } => {
            for arm in arms {
                for condition in &arm.conditions {
                    if let WhenCondition::TypeTest { ty, .. } = condition {
                        out.push(ty);
                    }
                }
            }
        }
        _ => {}
    }
    out
}
