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
    /// it is not a subtype of the overridden method's return type. The return
    /// types are stored unresolved (the canonical FQN) and the owner's FQN
    /// kept, rendered simple only in [`DeclDiagnostic::message`], so future
    /// quickfixes keep the full types.
    IncompatibleOverride {
        method: Name,
        found: Ty,
        expected_owner: Name,
        expected_ret: Ty,
    },
    /// §9.4.1.3: two unrelated superinterfaces declare matching default
    /// methods and the class inherits both without overriding.
    ConflictingDefaults { method: Name },
    /// §9.6.4.4: a method annotated `@Override` overrides or implements no
    /// supertype method — either nothing matches, or the annotated method is
    /// `static` (static methods hide, they never override).
    MethodDoesNotOverride { method: Name },
    /// §6.5.5.1: a reference type name in a *declaration* — a field type, a
    /// method's parameter/return/`throws` type, a type-parameter bound, a
    /// superclass or implemented interface, a record component type or a
    /// module directive — resolves to nothing on the classpath.
    CannotResolveType {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §6.5.5.1/[§7.5.2]: a simple type name is available through two or more
    /// on-demand imports that denote different types.
    AmbiguousName {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.5.1: a single-type import names a class or interface that cannot be
    /// found (or is not accessible).
    UnresolvedImport {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.5.2: an on-demand import (`import pkg.*;`) names a package that is
    /// not observable on the classpath — javac reports `package pkg does not
    /// exist`. (The package may still be *empty* of the wanted simple name;
    /// that is a name-resolution error at the use site.)
    UnresolvedImportPackage {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.5.4: a static on-demand import (`import static pkg.Type.*;`) names
    /// a class or interface that cannot be found.
    UnresolvedStaticImport {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.5.1: two single-type imports name different classes with the same
    /// simple name, or an import collides with a same-name top-level
    /// declaration of the compilation unit.
    ConflictingImport {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.4.3/[§7.7.2]: a class exists on the classpath, but its package is
    /// not visible from the resolving source set's module.
    ModuleNotAccessible {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.2.1/compilation-unit packaging (javadoc-classpath convention; no
    /// javac `compiler.*` twin): the file's package directory under its
    /// source root does not equal its declared package.
    ///
    /// [JLS §7.2.1]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.2.1
    UnexpectedPackagePath {
        /// The declared package as written.
        expected: Name,
        /// The file's directory chain under its source root, `/`-joined
        /// (the package it resolves to on a conventional classpath).
        dir: String,
        /// The source range of the package declaration's name.
        name_range: Option<rowan::TextRange>,
    },
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
            DeclDiagnostic::CannotResolveType { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::CannotResolveType)
            }
            DeclDiagnostic::AmbiguousName { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::AmbiguousName)
            }
            DeclDiagnostic::UnresolvedImport { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnresolvedImport)
            }
            DeclDiagnostic::UnresolvedImportPackage { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnresolvedImportPackage)
            }
            DeclDiagnostic::UnresolvedStaticImport { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnresolvedStaticImport)
            }
            DeclDiagnostic::ConflictingImport { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::ConflictingImport)
            }
            DeclDiagnostic::ModuleNotAccessible { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::ModuleNotAccessible)
            }
            DeclDiagnostic::UnexpectedPackagePath { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnexpectedPackagePath)
            }
        }
    }

    /// The human-readable message, using javac's *simple* class-name
    /// rendering ([`Ty::display_simple`]). The structured fields keep the
    /// canonical FQN; the simple rendering happens only here, at display time.
    pub fn message(&self, db: &dyn TyDatabase) -> String {
        match self {
            DeclDiagnostic::IncompatibleOverride {
                found,
                expected_owner,
                expected_ret,
                ..
            } => {
                format!(
                    "incompatible override: {} cannot override {}.{}",
                    found.display_simple(db),
                    expected_owner.simple_name(),
                    expected_ret.display_simple(db)
                )
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
            DeclDiagnostic::CannotResolveType { name, .. } => {
                format!("cannot resolve symbol '{}'", name.as_str())
            }
            DeclDiagnostic::AmbiguousName { name, .. } => {
                format!(
                    "reference to '{}' is ambiguous; both are imported on demand",
                    name.as_str()
                )
            }
            DeclDiagnostic::UnresolvedImport { name, .. } => {
                format!(
                    "cannot find symbol '{}' in the single-type import",
                    name.as_str()
                )
            }
            DeclDiagnostic::UnresolvedImportPackage { name, .. } => {
                format!("package {} does not exist", name.as_str())
            }
            DeclDiagnostic::UnresolvedStaticImport { name, .. } => {
                // javac's message for a static on-demand import of a missing
                // type: `cannot find symbol: class Type`.
                format!(
                    "cannot find symbol\n  symbol:   class {}",
                    name.simple_name()
                )
            }
            DeclDiagnostic::ConflictingImport { name, .. } => {
                format!(
                    "import conflicts with another declaration of '{}'",
                    name.as_str()
                )
            }
            DeclDiagnostic::ModuleNotAccessible { name, .. } => {
                format!(
                    "package in which '{}' is declared is not visible from the current module",
                    name.as_str()
                )
            }
            DeclDiagnostic::UnexpectedPackagePath { expected, dir, .. } => format!(
                "Package name '{}' does not correspond to the file path '{}'",
                expected.as_str(),
                dir
            ),
        }
    }

    /// The name of the offending method, for rendering.
    pub fn method_name(&self) -> &str {
        match self {
            DeclDiagnostic::IncompatibleOverride { method, .. }
            | DeclDiagnostic::ConflictingDefaults { method }
            | DeclDiagnostic::MethodDoesNotOverride { method } => method.as_str(),
            DeclDiagnostic::CannotResolveType { .. }
            | DeclDiagnostic::AmbiguousName { .. }
            | DeclDiagnostic::UnresolvedImport { .. }
            | DeclDiagnostic::UnresolvedImportPackage { .. }
            | DeclDiagnostic::UnresolvedStaticImport { .. }
            | DeclDiagnostic::ConflictingImport { .. }
            | DeclDiagnostic::ModuleNotAccessible { .. }
            | DeclDiagnostic::UnexpectedPackagePath { .. } => "",
        }
    }

    /// The source range of a reference-position diagnostic (unknown type,
    /// ambiguous name, import), when it has one.
    pub fn range(&self) -> Option<rowan::TextRange> {
        match self {
            DeclDiagnostic::CannotResolveType { range, .. }
            | DeclDiagnostic::AmbiguousName { range, .. }
            | DeclDiagnostic::UnresolvedImport { range, .. }
            | DeclDiagnostic::UnresolvedImportPackage { range, .. }
            | DeclDiagnostic::UnresolvedStaticImport { range, .. }
            | DeclDiagnostic::ConflictingImport { range, .. }
            | DeclDiagnostic::ModuleNotAccessible { range, .. } => *range,
            DeclDiagnostic::UnexpectedPackagePath { name_range, .. } => *name_range,
            _ => None,
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

    // §6.5.5.1/[§7.5.1]: the unknown-reference and import diagnostics of the
    // file's declarations (see [`crate::name_check`]).
    out.extend(crate::name_check::declaration_type_diagnostics(
        db, file, &tree,
    ));

    // §7.2.1: the file's package directory must match its declared package
    // (see [`crate::name_check::package_path_diagnostics`]).
    out.extend(crate::name_check::package_path_diagnostics(db, file, &tree));

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
            out.extend(check_class(db, file, scope, tree, fqn.as_str(), id));
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
    // Every member visible from the class, most-derived first ([§8.4.8.1]),
    // *without* the most-derived dedup: an override must still see the super
    // declaration it hides — both for the return-type-substitutability check
    // and for `@Override` ([§9.6.4.4]). Split into the class's own
    // declarations and the inherited set.
    let self_ty = Ty::reference(db, fqn, Vec::new());
    let all = method::all_methods_raw(db, scope, &self_ty, &ctx);
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
                // §8.4.8.3: the overriding return must be *substitutable*
                // for the overridden one — `R1 <: R2`, or `R1 <: |R2|` against
                // its ERASURE when the overridden return is a type variable
                // ([§8.4.4] adaptation, [§4.6]).
                let super_ret_erasure = super_method.ret.erasure(db);
                if !super_method.ret.is_error(db)
                    && !subtyping::is_subtype(db, scope, &method.ret.clone(), &super_ret_erasure)
                {
                    out.push(DeclDiagnostic::IncompatibleOverride {
                        method: Name::new(&method.name),
                        found: method.ret,
                        expected_owner: Name::new(&super_method.owner),
                        expected_ret: super_method.ret,
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
    // *hides*), so its annotation always fails. An explicitly declared
    // record accessor is the accessor mandated by its component ([§8.10.3]),
    // so `@Override` is accepted on it ([§9.6.4.4]).
    let record_components: &[hir_expand::item_tree::RecordComponent] = match tree.data(item) {
        ItemData::Record(record) => &record.components,
        _ => &[],
    };
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
                .any(|annotation| is_override_annotation(db, scope, &resolver, &annotation.name))
        {
            let Some(method) = declared
                .iter()
                .find(|d| d.name == m.name.as_str() && d.params.len() == m.sig.params.len())
            else {
                continue;
            };
            let is_record_accessor = record_components
                .iter()
                .any(|component| component.name.as_str() == method.name);
            let overrides = is_record_accessor
                || inherited
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
        && a.params.iter().zip(&b.params).all(|(x, y)| {
            x.is_error(db)
                || y.is_error(db)
                || x == y
                // [§8.4.8.1] with [§4.6]: a member inherited through a raw
                // supertype may arrive with its type variables unerased when
                // the stub record lacks the class `Signature`; the override
                // is still exact after erasure. Captured types (`CAP#n`)
                // never erase-match: they stand for unknown arguments.
                || (x.erasure(db) == y.erasure(db)
                    && !x.contains_type_var_named_capture(db)
                    && !y.contains_type_var_named_capture(db))
        })
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
