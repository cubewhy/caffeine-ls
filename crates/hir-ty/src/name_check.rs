//! Unknown-reference diagnostics ([JLS §6.5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5),
//! [§7.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5)).
//!
//! Type resolution (`[`crate::resolve`]`) degrades an unresolvable name to
//! its most-qualified candidate so the [`Ty` stays displayable and broken
//! classpaths stay conservative. This module turns *failure to resolve* into
//! structured diagnostics, walking the type references of a file's
//! declaration item tree (§Phase-2) and of the body IR (locals' declared
//! types, patterns and expression type references), and validating the
//! single-type imports of a compilation unit ([§7.5.1]).
//!
//! Nothing is reported until the workspace is loaded: before a project graph
//! exists (or when a file is not mapped to a source set and no JDK is
//! registered) every name would fail and the reports would be noise.

use hir_expand::{
    body::{BodyId, BodyTree, ExprData, ExprId, LocalId, PatternId, StmtData, StmtId},
    item_tree::{ItemData, ItemId, ItemTree},
    name::Name,
    span::{NameRef, SpannedTypeRef},
};
use rowan::TextRange;
use rustc_hash::FxHashMap;
use vfs::FileId;

use crate::{
    db::TyDatabase,
    decl_check::DeclDiagnostic,
    diagnostics::DiagLocation,
    resolve::{NameResolution, Resolver, resolve_name_checked},
};

/// Whether name resolution has a real classpath to answer against. Before the
/// workspace loads (`project_graph` is `None`) or when a file outside any
/// source set has no JDK registered, names degrade silently ([`crate::resolve`])
/// and no unknown-symbol report is emitted — it would be pure noise.
fn can_resolve(db: &dyn TyDatabase, scope: &hir::ResolutionScope) -> bool {
    hir::project_graph(db).is_some()
        && match scope {
            hir::ResolutionScope::SourceSet(_) => true,
            hir::ResolutionScope::Classpath(libraries) => !libraries.is_empty(),
            hir::ResolutionScope::JdkBuiltins => !hir::jdk_builtin_libraries(db).is_empty(),
        }
}

/// The unresolved-reference issue of one reference name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypeRefDiag {
    /// §6.5.5.1: the name resolves to nothing on the classpath.
    CannotResolve {
        name: Name,
        range: Option<TextRange>,
    },
    /// §6.5.5.1/[§7.5.2]: the name is ambiguous between on-demand imports.
    Ambiguous {
        name: Name,
        range: Option<TextRange>,
    },
    /// §7.4.3/[§7.7.2]: a class exists on the classpath, but its package is
    /// not visible from the resolving source set's module.
    ModuleNotAccessible {
        name: Name,
        range: Option<TextRange>,
    },
}

/// Checks the reference names of a source type reference against `scope`'s
/// classpath, pushing the unresolved ones into `into`. Skips the whole
/// reference when the workspace cannot answer yet ([`can_resolve`]).
pub(crate) fn check_spanned(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    spanned: &SpannedTypeRef,
    into: &mut Vec<TypeRefDiag>,
) {
    if !can_resolve(db, scope) {
        return;
    }
    for reference in &spanned.refs {
        check_reference(db, scope, resolver, reference, into);
    }
}

/// The checked resolution outcome of one reference name, pushed into `into`.
fn check_reference(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    reference: &NameRef,
    into: &mut Vec<TypeRefDiag>,
) {
    match resolve_name_checked(db, scope, resolver, &reference.name) {
        NameResolution::TypeVar | NameResolution::Resolved(_) => {}
        NameResolution::Ambiguous(_) => into.push(TypeRefDiag::Ambiguous {
            name: reference.name.clone(),
            range: reference.range,
        }),
        NameResolution::NotAccessible(_) => into.push(TypeRefDiag::ModuleNotAccessible {
            name: reference.name.clone(),
            range: reference.range,
        }),
        NameResolution::Unresolved => into.push(TypeRefDiag::CannotResolve {
            name: reference.name.clone(),
            range: reference.range,
        }),
    }
}

/// The named type references of a *declaration* item ([JLS §8], [§9], [§7.7]):
/// the class's superclass/interfaces, every field type, every method's
/// signature types and type-parameter bounds, record components and module
/// directives.
pub(crate) fn item_type_refs(data: &ItemData) -> Vec<&SpannedTypeRef> {
    fn collect_params<'a>(
        params: &'a [hir_expand::item_tree::TypeParam],
        out: &mut Vec<&'a SpannedTypeRef>,
    ) {
        for param in params {
            out.extend(param.bounds.iter());
        }
    }
    let mut out = Vec::new();
    match data {
        ItemData::Class(data) | ItemData::Interface(data) => {
            if let Some(super_class) = &data.super_class {
                out.push(super_class);
            }
            out.extend(data.interfaces.iter());
            collect_params(&data.type_params, &mut out);
        }
        ItemData::Enum(data) => out.extend(data.interfaces.iter()),
        ItemData::Record(data) => {
            out.extend(data.interfaces.iter());
            for component in &data.components {
                out.push(&component.ty);
            }
            collect_params(&data.type_params, &mut out);
        }
        ItemData::Annotation(_) => {}
        ItemData::Module(data) => {
            out.extend(data.uses.iter());
            for provide in &data.provides {
                out.push(&provide.service);
                out.extend(provide.implementations.iter());
            }
        }
        ItemData::Method(data) => {
            collect_params(&data.sig.type_params, &mut out);
            for param in &data.sig.params {
                out.push(&param.ty);
            }
            if let Some(ret) = &data.sig.ret {
                out.push(ret);
            }
            out.extend(data.sig.throws.iter());
        }
        ItemData::Field(data) => out.push(&data.ty),
        ItemData::EnumConstant(_) | ItemData::StaticInit(_) | ItemData::InstanceInit(_) => {}
    }
    out
}

/// The duplicate-package-declaration check of a compilation unit
/// ([JLS §7.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.1)):
/// a compilation unit declares at most one `package` declaration, so every
/// declaration after the first is an error. Each is reported at its own name
/// range. (javac treats a second `package` as a parse error — "class, interface,
/// enum, or record expected" — so this carries a custom code, not a
/// `compiler.*` twin.)
pub(crate) fn duplicate_package_diagnostics(tree: &ItemTree) -> Vec<DeclDiagnostic> {
    tree.package_decl_ranges
        .iter()
        .skip(1)
        .map(|name_range| DeclDiagnostic::DuplicatePackage {
            package: tree.package.clone().unwrap_or_else(|| Name::new("")),
            name_range: Some(*name_range),
        })
        .collect()
}

/// The package-declaration vs filesystem-path consistency check of a
/// the file's *directory chain* must end with the declared package chain —
/// the shape a conventional classpath looks the class up under. `module-info.java`
/// (no package declaration, [JLS §7.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7))
/// and files with the default package are exempt; files without a resolvable
/// path are skipped (see [`hir::file_path_segments`]).
///
/// The check is a *suffix* match rather than an exact one because the source
/// root base directory is not recoverable from the file-set (a single top
/// package tree has no shorter shared prefix); requiring the tail to equal
/// the package is exactly the requirement that a classpath lookup finds the
/// file. An IDE-style check: javac compiles such files fine, so it carries a
/// custom code, not a `compiler.*` twin.
pub(crate) fn package_path_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &ItemTree,
) -> Vec<DeclDiagnostic> {
    let Some(package) = tree.package.clone() else {
        return Vec::new();
    };
    let Some(dir) = hir::file_path_segments(db, file) else {
        return Vec::new();
    };
    let expected: Vec<&str> = package.as_str().split('.').collect();
    let ok = dir.len() >= expected.len()
        && dir[dir.len() - expected.len()..]
            .iter()
            .zip(&expected)
            .all(|(part, want)| part == want);
    if !ok {
        return vec![DeclDiagnostic::UnexpectedPackagePath {
            expected: package,
            // IntelliJ-style root-relative package directory (`org.example`),
            // with the full slash path as a fallback
            // ([`hir::file_package_dir`]).
            dir: hir::file_package_dir(db, file).unwrap_or_else(|| dir.join("/")),
            name_range: tree.package_range,
        }];
    }
    Vec::new()
}

/// The named annotation references of a *declaration* item
/// ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7)):
/// the annotations of every modifier list, of record components and of the
/// type parameters of classes/interfaces/records and methods. Each resolves
/// like a type name ([JLS §6.5.5.1]) — an annotation type *is* a reference
/// type — so an unknown one is reported the same way.
fn item_annotation_refs(data: &ItemData) -> Vec<&NameRef> {
    fn mods<'a>(m: &'a hir_expand::modifiers::Modifiers, out: &mut Vec<&'a NameRef>) {
        out.extend(m.annotations.iter());
    }
    fn type_params<'a>(params: &'a [hir_expand::item_tree::TypeParam], out: &mut Vec<&'a NameRef>) {
        for param in params {
            out.extend(param.annotations.iter());
        }
    }
    let mut out = Vec::new();
    match data {
        ItemData::Class(d) | ItemData::Interface(d) => {
            mods(&d.modifiers, &mut out);
            type_params(&d.type_params, &mut out);
        }
        ItemData::Enum(d) => mods(&d.modifiers, &mut out),
        ItemData::Record(d) => {
            mods(&d.modifiers, &mut out);
            type_params(&d.type_params, &mut out);
            for component in &d.components {
                out.extend(component.annotations.iter());
            }
        }
        ItemData::Annotation(d) => mods(&d.modifiers, &mut out),
        ItemData::Module(d) => mods(&d.modifiers, &mut out),
        ItemData::Method(d) => {
            mods(&d.modifiers, &mut out);
            type_params(&d.sig.type_params, &mut out);
        }
        ItemData::Field(d) => mods(&d.modifiers, &mut out),
        ItemData::EnumConstant(_) | ItemData::StaticInit(_) | ItemData::InstanceInit(_) => {}
    }
    out
}

/// The unknown-reference diagnostics of the *declaration* type references
/// ([JLS §6.5.5.1], [§7.5.1]) — including the *annotation* references of the
/// declarations ([JLS §9.7]) — and of the imports of a file.
pub(crate) fn declaration_type_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &ItemTree,
) -> Vec<DeclDiagnostic> {
    let scope = crate::resolve::scope_for_file(db, file);
    if !can_resolve(db, &scope) {
        return Vec::new();
    }
    let type_params = crate::db::type_params_map_query(db, db.file_text(file));
    let mut out = Vec::new();

    fn walk(
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
        tree: &ItemTree,
        type_params: &FxHashMap<ItemId, Vec<hir_expand::item_tree::TypeParam>>,
        id: ItemId,
        out: &mut Vec<DeclDiagnostic>,
    ) {
        let resolver = Resolver::new(tree, type_params, id);
        let mut issues = Vec::new();
        for spanned in item_type_refs(tree.data(id)) {
            check_spanned(db, scope, &resolver, spanned, &mut issues);
        }
        // §9.7/§6.5.5.1: the declaration's annotation names resolve like any
        // type reference — an unknown `@Name` is reported the same way (*not*
        // skipped by the caller's `check_spanned`, which the annotations
        // bypass because they are not `SpannedTypeRef`s).
        for reference in item_annotation_refs(tree.data(id)) {
            check_reference(db, scope, &resolver, reference, &mut issues);
        }
        for issue in issues {
            match issue {
                TypeRefDiag::CannotResolve { name, range } => {
                    out.push(DeclDiagnostic::CannotResolveType { name, range });
                }
                TypeRefDiag::Ambiguous { name, range } => {
                    out.push(DeclDiagnostic::AmbiguousName { name, range });
                }
                TypeRefDiag::ModuleNotAccessible { name, range } => {
                    out.push(DeclDiagnostic::ModuleNotAccessible { name, range });
                }
            }
        }
        for &child in tree.data(id).body() {
            walk(db, scope, tree, type_params, child, out);
        }
    }

    for &top in &tree.top {
        walk(db, &scope, tree, type_params.as_ref(), top, &mut out);
    }
    out.extend(import_diagnostics(db, &scope, tree));
    out
}

/// The single-type-import validation of a compilation unit ([JLS §7.5.1])
/// plus the on-demand-import validation of [JLS §7.5.2]:
///
/// - a single-type import must name an existing (accessible) class;
///   two single-type imports of the same simple name for different classes
///   conflict; a single-type import colliding with a same-name top-level
///   declaration of the compilation unit is an error ([§7.5.1]);
/// - a type-import-on-demand (`import pkg.*;`) must name an observable
///   package ([§7.5.2]);
/// - a static on-demand import (`import static pkg.Type.*;`) must name an
///   existing class or interface ([§7.5.4]).
pub(crate) fn import_diagnostics(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    tree: &ItemTree,
) -> Vec<DeclDiagnostic> {
    let single_imports: Vec<&hir_expand::item_tree::ImportItem> = tree
        .imports
        .iter()
        .filter(|import| !import.is_static && !import.is_asterisk)
        .collect();

    // §7.5.1: the named class or interface must exist (and be accessible);
    // an unresolvable import is a compile-time error.
    let mut out = Vec::new();
    for import in &single_imports {
        if hir::fqn_resolve(db, scope, import.name.as_str()).is_none() {
            out.push(DeclDiagnostic::UnresolvedImport {
                name: import.name.clone(),
                range: Some(import.range),
            });
        }
    }

    // §7.5.2: the package of an on-demand import must exist
    // (`import java.*;` is rejected by javac). The stored name already has
    // the trailing `.*` stripped at lowering.
    for import in tree
        .imports
        .iter()
        .filter(|import| !import.is_static && import.is_asterisk)
    {
        if !hir::package_exists(db, scope, import.name.as_str()) {
            out.push(DeclDiagnostic::UnresolvedImportPackage {
                name: import.name.clone(),
                range: Some(import.range),
            });
        }
    }

    // §7.5.4: a static on-demand import names the *class or interface* whose
    // members are imported on demand (`import static pkg.Type.*;`). Its
    // package must exist ([§7.5.2] — javac: `package pkg does not exist`),
    // and the declaring type must exist within it (javac: `cannot find
    // symbol: class Type`). The stored name already has the trailing `.*`
    // stripped, so it is the declaring type's FQN.
    for import in tree
        .imports
        .iter()
        .filter(|import| import.is_static && import.is_asterisk)
    {
        let text = import.name.as_str();
        let Some((package, _)) = text.rsplit_once('.') else {
            // `import static Type.*;` — a type of the unnamed package. There
            // is nothing observable to check for the package half, and the
            // type half resolves through the normal name-resolution of a
            // same-unit reference (never reported here).
            continue;
        };
        if !hir::package_exists(db, scope, package) {
            out.push(DeclDiagnostic::UnresolvedImportPackage {
                name: Name::new(package),
                range: Some(import.range),
            });
        } else if hir::fqn_resolve(db, scope, text).is_none() {
            out.push(DeclDiagnostic::UnresolvedStaticImport {
                name: import.name.clone(),
                range: Some(import.range),
            });
        }
    }

    // §7.5.1: two single-type imports of the same simple name that name
    // different classes conflict (a duplicate of the same class is ignored).
    for (i, a) in single_imports.iter().enumerate() {
        let simple_a = a.name.simple_name();
        for b in &single_imports[i + 1..] {
            if b.name.simple_name() == simple_a && b.name != a.name {
                out.push(DeclDiagnostic::ConflictingImport {
                    name: a.name.clone(),
                    range: Some(a.range),
                });
                out.push(DeclDiagnostic::ConflictingImport {
                    name: b.name.clone(),
                    range: Some(b.range),
                });
            }
        }
    }

    // §7.5.1: a single-type import whose simple name is also declared by a
    // top-level type of this compilation unit conflicts — unless it is the
    // same class.
    for &import in &single_imports {
        let simple = import.name.simple_name();
        for &top in &tree.top {
            let data = tree.data(top);
            let declared = match data {
                ItemData::Class(d) | ItemData::Interface(d) => &d.name,
                ItemData::Enum(d) => &d.name,
                ItemData::Record(d) => &d.name,
                ItemData::Annotation(d) => &d.name,
                _ => continue,
            };
            if declared.as_str() == simple {
                let own_fqn = match &tree.package {
                    Some(package) => format!("{}.{}", package.as_str(), simple),
                    None => simple.to_owned(),
                };
                if import.name.as_str() != own_fqn {
                    out.push(DeclDiagnostic::ConflictingImport {
                        name: import.name.clone(),
                        range: Some(import.range),
                    });
                }
            }
        }
    }
    out
}

/// The type references *owned by a body* ([JLS §14], [§15]): the declared
/// types of the locals it declares (parameters are reported through the
/// declaration pass, which covers the method's signature), the pattern types
/// of its `instanceof` tests and `case` labels, and the type references of its
/// expressions (`new`, casts, array creations, class literals, method
/// references, lambda parameter types, qualified `this`/`super`). Each comes
/// with the body-IR location its diagnostics attach to.
pub(crate) fn body_type_refs(
    bodies: &BodyTree,
    body: BodyId,
) -> Vec<(DiagLocation, SpannedTypeRef)> {
    let mut out = Vec::new();
    for &stmt in &bodies.body(body).stmts {
        walk_stmt(bodies, stmt, &mut out);
    }
    out
}

/// The type references of an expression forest that is *not* Body-owned: a
/// field initializer, enum constant arguments or an annotation element
/// default.
pub(crate) fn expr_forest_type_refs(
    bodies: &BodyTree,
    exprs: &[ExprId],
) -> Vec<(DiagLocation, SpannedTypeRef)> {
    let mut out = Vec::new();
    for &expr in exprs {
        walk_expr(bodies, expr, &mut out);
    }
    out
}

fn record_local(bodies: &BodyTree, local: LocalId, out: &mut Vec<(DiagLocation, SpannedTypeRef)>) {
    if let Some(ty) = &bodies.local(local).ty {
        out.push((DiagLocation::Local(local), ty.clone()));
    }
}

fn record_pattern(bodies: &BodyTree, id: PatternId, out: &mut Vec<(DiagLocation, SpannedTypeRef)>) {
    match bodies.pattern(id) {
        hir_expand::body::PatternData::Type(data) => {
            out.push((DiagLocation::Pattern(id), data.ty.clone()));
        }
        hir_expand::body::PatternData::Record(data) => {
            out.push((DiagLocation::Pattern(id), data.ty.clone()));
            for &component in &data.components {
                record_pattern(bodies, component, out);
            }
        }
        hir_expand::body::PatternData::MatchAll => {}
    }
}

fn walk_stmt(bodies: &BodyTree, id: StmtId, out: &mut Vec<(DiagLocation, SpannedTypeRef)>) {
    use StmtData::*;
    match bodies.stmt(id) {
        Empty | Missing | LocalClass { .. } => {}
        Block(stmts) => {
            for &stmt in stmts {
                walk_stmt(bodies, stmt, out);
            }
        }
        Decl { local, initializer } => {
            record_local(bodies, *local, out);
            if let Some(initializer) = initializer {
                walk_expr(bodies, *initializer, out);
            }
        }
        DeclGroup(stmts) => {
            for &stmt in stmts {
                walk_stmt(bodies, stmt, out);
            }
        }
        Expr(expr) => walk_expr(bodies, *expr, out),
        Labeled { stmt, .. } => walk_stmt(bodies, *stmt, out),
        If { cond, then, els } => {
            walk_expr(bodies, *cond, out);
            walk_stmt(bodies, *then, out);
            if let Some(els) = els {
                walk_stmt(bodies, *els, out);
            }
        }
        While { cond, body } => {
            walk_expr(bodies, *cond, out);
            walk_stmt(bodies, *body, out);
        }
        DoWhile { body, cond } => {
            walk_stmt(bodies, *body, out);
            walk_expr(bodies, *cond, out);
        }
        For {
            init,
            cond,
            step,
            body,
        } => {
            for &stmt in init {
                walk_stmt(bodies, stmt, out);
            }
            if let Some(cond) = cond {
                walk_expr(bodies, *cond, out);
            }
            for &step in step {
                walk_expr(bodies, step, out);
            }
            walk_stmt(bodies, *body, out);
        }
        ForEach {
            var,
            iterable,
            body,
        } => {
            record_local(bodies, *var, out);
            walk_expr(bodies, *iterable, out);
            walk_stmt(bodies, *body, out);
        }
        Switch { scrutinee, arms } => walk_switch(bodies, *scrutinee, arms, out),
        Return(Some(expr)) | Throw(expr) | Yield(expr) => walk_expr(bodies, *expr, out),
        Return(None) | Break(_) | Continue(_) => {}
        Synchronized { expr, body } => {
            walk_expr(bodies, *expr, out);
            walk_stmt(bodies, *body, out);
        }
        Try {
            resources,
            body,
            catches,
            finally,
        } => {
            for resource in resources {
                record_local(bodies, resource.local, out);
                if let Some(init) = resource.initializer {
                    walk_expr(bodies, init, out);
                }
            }
            walk_stmt(bodies, *body, out);
            for catch in catches {
                record_local(bodies, catch.param, out);
                walk_stmt(bodies, catch.body, out);
            }
            if let Some(finally) = finally {
                walk_stmt(bodies, *finally, out);
            }
        }
        Assert { cond, msg } => {
            walk_expr(bodies, *cond, out);
            if let Some(msg) = msg {
                walk_expr(bodies, *msg, out);
            }
        }
    }
}

fn walk_switch(
    bodies: &BodyTree,
    scrutinee: ExprId,
    arms: &[hir_expand::body::SwitchArm],
    out: &mut Vec<(DiagLocation, SpannedTypeRef)>,
) {
    walk_expr(bodies, scrutinee, out);
    for arm in arms {
        for label in &arm.labels {
            match label {
                hir_expand::body::SwitchLabel::Expr(expr)
                | hir_expand::body::SwitchLabel::Guard(expr) => {
                    walk_expr(bodies, *expr, out);
                }
                hir_expand::body::SwitchLabel::Pattern(pattern) => {
                    record_pattern(bodies, *pattern, out);
                }
            }
        }
        for &stmt in &arm.body {
            walk_stmt(bodies, stmt, out);
        }
    }
}

fn walk_expr(bodies: &BodyTree, id: ExprId, out: &mut Vec<(DiagLocation, SpannedTypeRef)>) {
    use ExprData::*;
    match bodies.expr(id) {
        New { ty, args, .. } => {
            out.push((DiagLocation::Expr(id), ty.clone()));
            for &arg in args {
                walk_expr(bodies, arg, out);
            }
        }
        NewArray {
            ty,
            dims,
            initializer,
        } => {
            out.push((DiagLocation::Expr(id), ty.clone()));
            for &dim in dims {
                walk_expr(bodies, dim, out);
            }
            if let Some(elems) = initializer {
                for &elem in elems {
                    walk_expr(bodies, elem, out);
                }
            }
        }
        Cast { ty, expr } => {
            out.push((DiagLocation::Expr(id), ty.clone()));
            walk_expr(bodies, *expr, out);
        }
        InstanceOf { expr, ty, pattern } => {
            if let Some(ty) = ty {
                out.push((DiagLocation::Expr(id), ty.clone()));
            }
            walk_expr(bodies, *expr, out);
            if let Some(pattern) = pattern {
                record_pattern(bodies, *pattern, out);
            }
        }
        ClassLit(ty) => out.push((DiagLocation::Expr(id), ty.clone())),
        MethodCall {
            receiver,
            type_args,
            args,
            ..
        } => {
            for ty in type_args {
                out.push((DiagLocation::Expr(id), ty.clone()));
            }
            if let Some(receiver) = receiver {
                walk_expr(bodies, *receiver, out);
            }
            for &arg in args {
                walk_expr(bodies, arg, out);
            }
        }
        MethodRef {
            qualifier,
            type_name,
            ..
        } => {
            if let Some(ty) = type_name {
                out.push((DiagLocation::Expr(id), ty.clone()));
            }
            if let Some(qualifier) = qualifier {
                walk_expr(bodies, *qualifier, out);
            }
        }
        Lambda { params, body } => {
            for (_, ty) in params {
                if let Some(ty) = ty {
                    out.push((DiagLocation::Expr(id), ty.clone()));
                }
            }
            match body {
                hir_expand::body::LambdaBody::Expr(expr) => walk_expr(bodies, *expr, out),
                hir_expand::body::LambdaBody::Block(stmt) => walk_stmt(bodies, *stmt, out),
            }
        }
        This { qualifier } | Super { qualifier } => {
            if let Some(ty) = qualifier {
                out.push((DiagLocation::Expr(id), ty.clone()));
            }
        }
        FieldAccess { target, .. } => {
            if let Some(target) = target {
                walk_expr(bodies, *target, out);
            }
        }
        ArrayAccess { array, index } => {
            walk_expr(bodies, *array, out);
            walk_expr(bodies, *index, out);
        }
        Unary { expr, .. } | Postfix { expr, .. } => walk_expr(bodies, *expr, out),
        Binary { lhs, rhs, .. } => {
            walk_expr(bodies, *lhs, out);
            walk_expr(bodies, *rhs, out);
        }
        Assign { lhs, rhs, .. } => {
            walk_expr(bodies, *lhs, out);
            walk_expr(bodies, *rhs, out);
        }
        Conditional { cond, then, els } => {
            walk_expr(bodies, *cond, out);
            walk_expr(bodies, *then, out);
            walk_expr(bodies, *els, out);
        }
        Paren(expr) => walk_expr(bodies, *expr, out),
        Switch { scrutinee, arms } => walk_switch(bodies, *scrutinee, arms, out),
        CtorCall { args, .. } => {
            for &arg in args {
                walk_expr(bodies, arg, out);
            }
        }
        ArrayInit(elems) => {
            for &elem in elems {
                walk_expr(bodies, elem, out);
            }
        }
        Template { args } => {
            for &arg in args {
                walk_expr(bodies, arg, out);
            }
        }
        Literal(_) | Null | Var(_) | NamePath(_) | Missing => {}
    }
}
