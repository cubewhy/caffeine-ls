//! Declaration-level checks over classes and interfaces
//! ([JLS §8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html),
//! [§9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html)) — the
//! checks that need a class's *whole* inheritance graph rather than one body:
//!
//! - the return-type-substitutability of overrides
//!   ([§8.4.8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.3)),
//! - conflicting default methods inherited from unrelated superinterfaces
//!   ([§9.4.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.3)).
//!
//! Unlike the body-level [`TypeError`]s, these diagnostics are keyed to the
//! declaring method rather than an expression: they are collected per file by
//! [`class_diagnostics`] and carry the offending method's name.

use hir_expand::item_tree::ItemData;
use hir_expand::name::Name;
use syntax::{DiagnosticCode, JavaDiagnosticCode};
use vfs::FileId;

use crate::db::TyDatabase;
use crate::method::{self, MethodData};
use crate::resolve::scope_for_file;
use crate::subtyping;
use crate::ty::Ty;

/// A declaration-level diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclDiagnostic {
    /// §8.4.8.3: an override's return type is not return-type-substitutable —
    /// it is not a subtype of the overridden method's return type.
    IncompatibleOverride {
        method: Name,
        found: String,
        expected: String,
    },
    /// §9.4.1.3: two unrelated superinterfaces declare matching default
    /// methods and the class inherits both without overriding.
    ConflictingDefaults { method: Name },
    /// §9.6.4.4: a method annotated `@Override` overrides or implements no
    /// supertype method — either nothing matches, or the annotated method is
    /// `static` (static methods hide, they never override).
    MethodDoesNotOverride { method: Name },
}

impl DeclDiagnostic {
    /// The typed code of this diagnostic ([`DiagnosticCode`]).
    pub fn code(&self) -> DiagnosticCode {
        match self {
            DeclDiagnostic::IncompatibleOverride { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::IncompatibleOverride)
            }
            DeclDiagnostic::ConflictingDefaults { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::ConflictingDefaults)
            }
            DeclDiagnostic::MethodDoesNotOverride { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::MethodDoesNotOverride)
            }
        }
    }

    /// The human-readable message.
    pub fn message(&self) -> String {
        match self {
            DeclDiagnostic::IncompatibleOverride {
                found, expected, ..
            } => {
                format!("incompatible override: {found} cannot override {expected}")
            }
            DeclDiagnostic::ConflictingDefaults { method } => {
                let name = method.as_str();
                format!("class inherits unrelated default methods for {name}(); must be overridden")
            }
            DeclDiagnostic::MethodDoesNotOverride { method } => {
                let name = method.as_str();
                format!(
                    "method {name}() annotated @Override does not override or implement a method from a supertype"
                )
            }
        }
    }

    /// The name of the offending method, for rendering.
    pub fn method_name(&self) -> &str {
        match self {
            DeclDiagnostic::IncompatibleOverride { method, .. }
            | DeclDiagnostic::ConflictingDefaults { method }
            | DeclDiagnostic::MethodDoesNotOverride { method } => method.as_str(),
        }
    }
}

/// The declaration-level diagnostics of every class-like declaration in
/// `file`, in source order.
pub fn class_diagnostics(db: &dyn TyDatabase, file: FileId) -> Vec<DeclDiagnostic> {
    crate::db::class_diagnostics_query(db, db.file_text(file))
}

/// Enumerates the class-like declarations of the file in source order and
/// checks each against its inheritance graph.
pub(crate) fn class_diagnostics_impl(db: &dyn TyDatabase, file: FileId) -> Vec<DeclDiagnostic> {
    let tree = hir::file_item_tree(db, file);
    let scope = scope_for_file(db, file);
    let mut out = Vec::new();

    fn walk(
        db: &dyn TyDatabase,
        file: FileId,
        scope: &hir::ResolutionScope,
        tree: &hir_expand::item_tree::ItemTree,
        id: hir_expand::item_tree::ItemId,
        out: &mut Vec<DeclDiagnostic>,
    ) {
        let data = tree.data(id);
        if matches!(
            data,
            ItemData::Class(_) | ItemData::Interface(_) | ItemData::Enum(_) | ItemData::Record(_)
        ) && let Some(fqn) = hir::source_class_fqn(db, file, id)
        {
            out.extend(check_class(db, file, scope, &tree, fqn.as_str(), id));
        }
        for &child in data.body() {
            walk(db, file, scope, tree, child, out);
        }
    }
    for top in &tree.top {
        walk(db, file, &scope, &tree, *top, &mut out);
    }
    out
}

/// Checks one class-like declaration against its inheritance graph.
fn check_class(
    db: &dyn TyDatabase,
    file: FileId,
    scope: &hir::ResolutionScope,
    tree: &hir_expand::item_tree::ItemTree,
    fqn: &str,
    item: hir_expand::item_tree::ItemId,
) -> Vec<DeclDiagnostic> {
    // The access-control context of the class itself ([§6.6.1]): the walk is
    // a member enumeration, not an invocation from outside.
    let ctx = crate::method::access_context(db, file, item);
    let mut out = Vec::new();
    // Every member visible from the class, most-derived first ([§8.4.8.1]);
    // split into the class's own declarations and the inherited set.
    let self_ty = Ty::reference(db, fqn, Vec::new());
    let all = method::all_methods(db, scope, &self_ty, &ctx);
    let declared: Vec<&MethodData> = all.iter().filter(|m| m.owner == fqn).collect();
    let inherited: Vec<&MethodData> = all.iter().filter(|m| m.owner != fqn).collect();
    for method in &declared {
        // §8.4.8.3: an instance method overriding an inherited method must be
        // return-type-substitutable — its return type is a subtype of the
        // overridden return type.
        if method.is_static || method.ret.is_void(db) {
            continue;
        }
        for super_method in &inherited {
            if same_signature(db, method, super_method) {
                if !super_method.ret.is_error(db)
                    && !subtyping::is_subtype(
                        db,
                        scope,
                        &method.ret.clone(),
                        &super_method.ret.clone(),
                    )
                {
                    out.push(DeclDiagnostic::IncompatibleOverride {
                        method: Name::new(&method.name),
                        found: method.ret.display(db).to_string(),
                        expected: format!(
                            "{}.{}",
                            super_method.owner,
                            super_method.ret.display(db)
                        ),
                    });
                }
                break;
            }
        }
    }

    // §9.4.1.3: two default methods with the same signature whose declaring
    // interfaces are unrelated (neither a subtype of the other) conflict; the
    // class inherits them only if it overrides the signature itself. The
    // defaults are collected *without* the most-derived dedup — unrelated
    // defaults do not override each other, they conflict.
    let defaults = method::inherited_defaults(db, scope, &self_ty);
    let defaults: Vec<&MethodData> = defaults.iter().filter(|m| m.owner != fqn).collect();
    for (i, a) in defaults.iter().enumerate() {
        for b in &defaults[i + 1..] {
            if !same_signature(db, a, b) || related(db, scope, &a.owner, &b.owner) {
                continue;
            }
            let already_overridden = declared
                .iter()
                .any(|m| !m.is_static && same_signature(db, m, a));
            if !already_overridden {
                out.push(DeclDiagnostic::ConflictingDefaults {
                    method: Name::new(&a.name),
                });
            }
        }
    }

    // §9.6.4.4: a method annotated `@Override` must override or implement an
    // instance method declared in a supertype — otherwise the annotation is a
    // compile-time error. A `static` method never overrides ([§8.4.8.2]: it
    // *hides*), so its annotation always fails.
    let resolver = crate::resolve::Resolver::new(
        tree,
        crate::db::type_params_map_query(db, db.file_text(file)),
        item,
    );
    for &child in tree.data(item).body() {
        if let ItemData::Method(m) = tree.data(child)
            && !m.is_constructor
            && m.modifiers
                .annotations
                .iter()
                .any(|name| is_override_annotation(db, scope, &resolver, name))
        {
            let Some(method) = declared
                .iter()
                .find(|d| d.name == m.name.as_str() && d.params.len() == m.sig.params.len())
            else {
                continue;
            };
            let overrides = inherited
                .iter()
                .any(|s| !s.is_static && same_signature(db, method, s));
            if method.is_static || !overrides {
                out.push(DeclDiagnostic::MethodDoesNotOverride {
                    method: Name::new(&method.name),
                });
            }
        }
    }
    out
}

/// Whether two methods have the same overriding signature
/// ([JLS §8.4.2]): identical name and *identical* parameter types. Widening
/// ([§5.1.2]) or boxing ([§5.1.7]) conversions apply to invocation, never to
/// overriding, so `f(int)` and `f(long)` are unrelated overloads. A parameter
/// that failed to resolve is treated as matching, so a broken classpath stays
/// conservative. The substitution of a supertype's type arguments into an
/// inherited method's parameters happens when the member set is built; the
/// substitution of a method's own type variables ([§8.4.4]) is not modelled.
fn same_signature(db: &dyn TyDatabase, a: &MethodData, b: &MethodData) -> bool {
    a.name == b.name
        && a.params.len() == b.params.len()
        && a.params
            .iter()
            .zip(&b.params)
            .all(|(x, y)| x.is_error(db) || y.is_error(db) || x == y)
}

/// §9.7.1/§6.5.5: whether an annotation name resolves to
/// `java.lang.Override`. The name is resolved in the file's scope like any
/// type reference, so a same-package `@interface Override` ([§6.5.5.1]) or a
/// single-type import shadows the JDK annotation and does not count. A name
/// that resolves nowhere falls back to its simple form, keeping broken or
/// partial classpaths conservative.
fn is_override_annotation(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &crate::resolve::Resolver,
    name: &Name,
) -> bool {
    let resolved = crate::resolve::candidate_fqns(resolver, name)
        .into_iter()
        .find(|candidate| hir::fqn_resolve(db, scope, candidate.as_str()).is_some());
    match resolved {
        Some(fqn) => fqn.as_str() == "java.lang.Override",
        None => name.as_str().rsplit('.').next() == Some("Override"),
    }
}

/// Whether two declaring types are subtype-related in either direction, which
/// makes their default methods an override chain rather than a conflict
/// ([§9.4.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.1),
/// [§9.4.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.2)).
fn related(db: &dyn TyDatabase, scope: &hir::ResolutionScope, a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let a_ty = Ty::reference(db, a, Vec::new());
    let b_ty = Ty::reference(db, b, Vec::new());
    subtyping::is_subtype(db, scope, &a_ty, &b_ty) || subtyping::is_subtype(db, scope, &b_ty, &a_ty)
}
