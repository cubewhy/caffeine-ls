//! Unknown-reference diagnostics ([JLS §6.5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5),
//! [§7.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5)).
//!
//! Type resolution (`[`crate::java::resolve`]`) degrades an unresolvable name to
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

use hir_def::java::item_tree::{ItemAnnotationRef, ItemData, ItemId, ItemTree, ItemTypeRef};
use hir_expand::{
    ast_id_map::AstIdMap,
    body::{BodyId, BodyTree, ExprData, ExprId, LocalId, PatternId, StmtData, StmtId},
    name::Name,
    span::SpannedTypeRef,
};
use rowan::TextRange;
use rustc_hash::FxHashMap;
use syntax::SourceFile;
use syntax::stub::TypeRef;
use vfs::FileId;

use crate::{
    java::db::{TyDatabase, deprecated_enclosing_query},
    java::decl_check::DeclDiagnostic,
    java::deprecation::{self, DeprecatedReference},
    java::diagnostics::DiagLocation,
    java::range_ctx::range_ctx,
    java::release_api,
    java::resolve::{NameResolution, Resolver, resolve_name_checked, resolve_type_ref},
};
use hir_def::java::ranges;

/// Whether name resolution has a real classpath to answer against. Before the
/// workspace loads (`project_graph` is `None`) or when a file outside any
/// source set has no JDK registered, names degrade silently ([`crate::java::resolve`])
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
    /// JEP 247: the name resolves against the runtime JDK, but the platform
    /// API of the source set's release does not provide it.
    NotSupportedInRelease {
        name: Name,
        range: Option<TextRange>,
        /// The release the source set compiles against.
        found: u8,
        /// The earliest release whose platform view provides the name.
        added: u8,
    },
}

/// Checks the reference names of a source type reference (`&SpannedTypeRef`,
/// the *body* path — locals' declared types, patterns and expression type
/// references) against `scope`'s classpath, pushing the unresolved ones into
/// `into`. Skips the whole reference when the workspace cannot answer yet
/// ([`can_resolve`]).
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
        check_reference(db, scope, resolver, &reference.name, reference.range, into);
    }
}

/// The *deprecated* classes a source type reference names ([JLS §9.6.4.6]),
/// one entry per reference name — the class itself and each of its enclosing
/// classes, at the reference's own name span.
///
/// The range-bearing reference list is the source of truth for both the
/// resolution and the report, exactly as for
/// [`check_spanned`](crate::java::name_check::check_spanned).
pub(crate) fn deprecation_hits(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    spanned: &SpannedTypeRef,
) -> Vec<DeprecatedReference> {
    if !can_resolve(db, scope) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for reference in &spanned.refs {
        let NameResolution::Resolved(name) =
            resolve_name_checked(db, scope, resolver, &reference.name)
        else {
            continue;
        };
        for (deprecation, api) in deprecation::class_hits(db, scope, &name) {
            out.push(DeprecatedReference {
                api,
                deprecation,
                range: reference.range,
            });
        }
    }
    out
}

/// The checked resolution outcome of one reference name, pushed into `into`.
fn check_reference(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    name: &Name,
    range: Option<TextRange>,
    into: &mut Vec<TypeRefDiag>,
) {
    match resolve_name_checked(db, scope, resolver, name) {
        NameResolution::TypeVar => {}
        NameResolution::Resolved(name) => {
            // JEP 247: a name that resolves against the runtime JDK may still
            // be outside the platform API of the source set's `--release`.
            if let Some((found, added)) = release_api::class_of_reference(db, scope, &name) {
                into.push(TypeRefDiag::NotSupportedInRelease {
                    name,
                    range,
                    found,
                    added,
                });
            }
        }
        NameResolution::Ambiguous(_) => into.push(TypeRefDiag::Ambiguous {
            name: name.clone(),
            range,
        }),
        NameResolution::NotAccessible(_) => into.push(TypeRefDiag::ModuleNotAccessible {
            name: name.clone(),
            range,
        }),
        NameResolution::Unresolved => into.push(TypeRefDiag::CannotResolve {
            name: name.clone(),
            range,
        }),
    }
}

/// The named type references of a *declaration* item ([JLS §8], [§9], [§7.7]):
/// the class's superclass/interfaces, every field type, every method's
/// signature types and type-parameter bounds, record components and module
/// directives.
pub(crate) fn item_type_refs(data: &ItemData) -> Vec<&ItemTypeRef> {
    fn collect_params<'a>(
        params: &'a [hir_def::java::item_tree::TypeParam],
        out: &mut Vec<&'a ItemTypeRef>,
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
pub(crate) fn duplicate_package_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &ItemTree,
) -> Vec<DeclDiagnostic> {
    let Some((map, source)) = range_ctx(db, file, tree.language) else {
        return Vec::new();
    };
    tree.package_decls
        .iter()
        .skip(1)
        .map(|decl| DeclDiagnostic::DuplicatePackage {
            package: tree.package.clone().unwrap_or_else(|| Name::new("")),
            name_range: ranges::package_name_range(map, &source, *decl),
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
        let name_range = range_ctx(db, file, tree.language).and_then(|(map, source)| {
            tree.package_decls
                .last()
                .and_then(|decl| ranges::package_name_range(map, &source, *decl))
        });
        return vec![DeclDiagnostic::UnexpectedPackagePath {
            expected: package,
            // IntelliJ-style root-relative package directory (`org.example`),
            // with the full slash path as a fallback
            // ([`hir::file_package_dir`]).
            dir: hir::file_package_dir(db, file).unwrap_or_else(|| dir.join("/")),
            name_range,
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
fn item_annotation_refs(data: &ItemData) -> Vec<&ItemAnnotationRef> {
    fn annotations<'a>(items: &'a [ItemAnnotationRef], out: &mut Vec<&'a ItemAnnotationRef>) {
        out.extend(items.iter());
    }
    fn type_params<'a>(
        params: &'a [hir_def::java::item_tree::TypeParam],
        out: &mut Vec<&'a ItemAnnotationRef>,
    ) {
        for param in params {
            out.extend(param.annotations.iter());
        }
    }
    let mut out = Vec::new();
    match data {
        ItemData::Class(d) | ItemData::Interface(d) => {
            annotations(&d.annotations, &mut out);
            type_params(&d.type_params, &mut out);
        }
        ItemData::Enum(d) => annotations(&d.annotations, &mut out),
        ItemData::Record(d) => {
            annotations(&d.annotations, &mut out);
            type_params(&d.type_params, &mut out);
            for component in &d.components {
                out.extend(component.annotations.iter());
            }
        }
        ItemData::Annotation(d) => annotations(&d.annotations, &mut out),
        ItemData::Module(d) => annotations(&d.annotations, &mut out),
        ItemData::Method(d) => {
            annotations(&d.annotations, &mut out);
            type_params(&d.sig.type_params, &mut out);
            // §9.7.4: a formal parameter's declaration annotations are the
            // annotations of its own modifier list, lowered with the
            // signature ([`hir_def::java::item_tree::Param::annotations`]).
            for param in &d.sig.params {
                annotations(&param.annotations, &mut out);
            }
        }
        ItemData::Field(d) => annotations(&d.annotations, &mut out),
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
    let scope = crate::java::resolve::scope_for_file(db, file);
    if !can_resolve(db, &scope) {
        return Vec::new();
    }
    let Some((map, source)) = range_ctx(db, file, tree.language) else {
        return Vec::new();
    };
    let type_params = crate::java::db::type_params_map_query(db, db.file_text(file));
    let mut out = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn walk(
        db: &dyn TyDatabase,
        file_id: FileId,
        scope: &hir::ResolutionScope,
        tree: &ItemTree,
        map: &AstIdMap,
        source: &SourceFile,
        type_params: &FxHashMap<ItemId, Vec<crate::java::resolve::ScopedTypeParam>>,
        outermost: &Name,
        id: ItemId,
        out: &mut Vec<DeclDiagnostic>,
    ) {
        let resolver = Resolver::new(tree, type_params, id);
        let mut issues = Vec::new();
        // §9.6.4.6: the deprecation in force for this declaration — its own
        // `@Deprecated`, or the innermost enclosing one that carries it.
        let enclosing = deprecated_enclosing_query(db, db.file_text(file_id))
            .get(&id)
            .and_then(|info| info.enclosing);
        // The reference names of every declaration type reference, with the
        // source range of each (resolved on demand from the syntax tree);
        // name_check's `check_spanned` covers the *body* type references.
        for tyref in item_type_refs(tree.data(id)) {
            let occurrences = ranges::type_ref_occurrences(map, source, tyref);
            for (name, range) in occurrences.iter().cloned() {
                check_reference(db, scope, &resolver, &name, range, &mut issues);
                check_reference_deprecation(
                    db, scope, &resolver, &name, range, enclosing, outermost, out,
                );
            }
            // JLS §4.8/§4.12.2: a declared type naming a generic class without
            // its type arguments is a raw type — legal, reported as a warning.
            // The range is the whole type reference, as javac's caret is.
            let ty = resolve_type_ref(db, scope, &resolver, tyref);
            if crate::java::raw_type::is_raw_reference(db, scope, &ty)
                && let Some(range) = tyref_range(&occurrences)
            {
                out.push(DeclDiagnostic::RawTypeUse {
                    ty,
                    range: Some(range),
                });
            }
            // JLS §4.5: every written reference in the declaration type must
            // carry exactly the type arguments its class declares, nested
            // arguments and wildcard bounds included. javac: `wrong number of
            // type arguments; required {n}`.
            if let Some((bad, expected)) =
                crate::java::resolve::type_argument_arity_mismatch(db, scope, &resolver, &tyref.ty)
            {
                out.push(DeclDiagnostic::WrongTypeArgumentCount {
                    ty: bad,
                    expected,
                    range: tyref_range(&occurrences),
                });
            }
        }
        // §9.7/§6.5.5.1: the declaration's annotation names resolve like any
        // type reference — an unknown `@Name` is reported the same way (*not*
        // skipped by the caller's `check_spanned`, which the annotations
        // bypass because they are not `ItemTypeRef`s).
        for annotation in item_annotation_refs(tree.data(id)) {
            let range = ranges::annotation_name_range(map, source, annotation);
            check_reference(db, scope, &resolver, &annotation.name, range, &mut issues);
            // A deprecated *annotation type* is a deprecated class reference
            // like any other.
            check_reference_deprecation(
                db,
                scope,
                &resolver,
                &annotation.name,
                range,
                enclosing,
                outermost,
                out,
            );
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
                TypeRefDiag::NotSupportedInRelease {
                    name,
                    range,
                    found,
                    added,
                } => {
                    out.push(DeclDiagnostic::NotSupportedInRelease {
                        api: crate::java::release_api::ReleaseApi::Class { name },
                        found,
                        added,
                        range,
                    });
                }
            }
        }
        for &child in tree.data(id).body() {
            walk(
                db,
                file_id,
                scope,
                tree,
                map,
                source,
                type_params,
                outermost,
                child,
                out,
            );
        }
    }

    // The outermost class of the file's first top-level declaration is the
    // unit §9.6.4.6's same-outermost-class exemption compares; each top-level
    // declaration of the unit is its own outermost class.
    for &top in &tree.top {
        let outermost = hir::source_class_fqn(db, file, top).unwrap_or_else(|| Name::new(""));
        walk(
            db,
            file,
            &scope,
            tree,
            map,
            &source,
            type_params.as_ref(),
            &outermost,
            top,
            &mut out,
        );
    }
    out.extend(import_diagnostics(db, &scope, tree, map, &source));
    out
}

/// The deprecated classes a *declaration*-position reference name denotes,
/// pushed into `out` ([JLS §9.6.4.6]).
#[allow(clippy::too_many_arguments)]
fn check_reference_deprecation(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    name: &Name,
    range: Option<TextRange>,
    enclosing: Option<deprecation::Deprecation>,
    outermost: &Name,
    out: &mut Vec<DeclDiagnostic>,
) {
    let NameResolution::Resolved(fqn) = resolve_name_checked(db, scope, resolver, name) else {
        return;
    };
    for (deprecation, api) in deprecation::class_hits(db, scope, &fqn) {
        if deprecation::is_exempt(db, scope, enclosing, Some(outermost), &api, deprecation) {
            continue;
        }
        out.push(DeclDiagnostic::DeprecatedUse {
            api,
            deprecation,
            range,
        });
    }
}

/// The source range spanning a declaration type reference: from the first
/// reference name's start to the last one's end ([`ranges::type_ref_occurrences`]),
/// so a qualified or parameterized type is covered whole. `None` when the
/// reference was synthesized (a placeholder with no syntax node).
fn tyref_range(occurrences: &[(Name, Option<TextRange>)]) -> Option<TextRange> {
    let start = occurrences.first()?.1?.start();
    let end = occurrences.last()?.1?.end();
    Some(TextRange::new(start, end))
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
    map: &AstIdMap,
    source: &SourceFile,
) -> Vec<DeclDiagnostic> {
    let import_range = |import: &hir_def::java::item_tree::ImportItem| {
        ranges::import_name_range(map, source, import)
    };
    let single_imports: Vec<&hir_def::java::item_tree::ImportItem> = tree
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
                range: import_range(import),
            });
        }
    }

    // JEP 247: the imported class exists on the runtime classpath but not in
    // the platform API of the release — javac rejects the import as well as
    // every use of it.
    for import in &single_imports {
        if let Some((found, added)) = release_api::class_of_reference(db, scope, &import.name) {
            out.push(DeclDiagnostic::NotSupportedInRelease {
                api: crate::java::release_api::ReleaseApi::Class {
                    name: import.name.clone(),
                },
                found,
                added,
                range: import_range(import),
            });
        }
    }

    // §7.5.4: a static single import names one member of the type its prefix
    // names (`import static pkg.Type.member;`). JEP 247: both the type and the
    // member resolve against the runtime JDK here, so a member the release's
    // platform view does not declare is reported — by name alone, which is as
    // precise as the source form gets.
    for import in tree
        .imports
        .iter()
        .filter(|import| import.is_static && !import.is_asterisk)
    {
        let text = import.name.as_str();
        let Some((owner, member)) = text.rsplit_once('.') else {
            // `import static member;` — a type of the unnamed package; nothing
            // observable to check.
            continue;
        };
        if let Some((found, added)) = release_api::member_of_owner(db, scope, owner, member, None) {
            out.push(DeclDiagnostic::NotSupportedInRelease {
                api: crate::java::release_api::ReleaseApi::Member {
                    owner: owner.to_owned(),
                    name: member.to_owned(),
                },
                found,
                added,
                range: import_range(import),
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
                range: import_range(import),
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
                range: import_range(import),
            });
        } else if hir::fqn_resolve(db, scope, text).is_none() {
            out.push(DeclDiagnostic::UnresolvedStaticImport {
                name: import.name.clone(),
                range: import_range(import),
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
                    range: import_range(a),
                });
                out.push(DeclDiagnostic::ConflictingImport {
                    name: b.name.clone(),
                    range: import_range(b),
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
                        range: import_range(import),
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
    let binding = bodies.local(local);
    if let Some(ty) = &binding.ty {
        out.push((DiagLocation::Local(local), ty.clone()));
    }
    for annotation in &binding.annotations {
        out.push((DiagLocation::Local(local), annotation_reference(annotation)));
    }
}

/// A one-name reference to an annotation's type name ([JLS
/// §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7)):
/// an annotation is written as a type name and resolves like one
/// ([§6.5.5.1] — an annotation type *is* a reference type). It is how the
/// *declaration* annotations of a variable a body declares enter the same
/// resolution the type references of that body get: `refs` carries exactly
/// the annotation's name and source range, so the resolution and the report
/// are the ones every other reference receives; the type the wrapper names is
/// never read.
fn annotation_reference(annotation: &hir_expand::span::AnnotationRef) -> SpannedTypeRef {
    SpannedTypeRef {
        ty: TypeRef::Reference {
            name: annotation.name.name.clone(),
            generic_args: Vec::new(),
        },
        refs: vec![annotation.name.clone()],
        type_use_annotations: Vec::new(),
    }
}

fn record_pattern(bodies: &BodyTree, id: PatternId, out: &mut Vec<(DiagLocation, SpannedTypeRef)>) {
    match bodies.pattern(id) {
        hir_expand::body::PatternData::Type(data) => {
            out.push((DiagLocation::Pattern(id), data.ty.clone()));
            // §14.30.1: the pattern's binding is a variable declaration, so
            // an annotation written before its type is one of its own
            // ([§9.7.4]).
            if let Some(binding) = data.binding {
                for annotation in &bodies.local(binding).annotations {
                    out.push((DiagLocation::Pattern(id), annotation_reference(annotation)));
                }
            }
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
        New {
            ty, args, receiver, ..
        } => {
            // §15.9: a *qualified* class instance creation `primary.new
            // Inner(args)` names the member class of the receiver expression's
            // compile-time type, not a type in the lexical scope — the bare
            // `Inner` is unresolvable on its own (and may even be lexically
            // shadowed by an unrelated name), so its type reference is *not*
            // checked against the file's imports/scope here. Resolution runs
            // at inference time against the receiver's inferred type.
            if receiver.is_none() {
                out.push((DiagLocation::Expr(id), ty.clone()));
            }
            for &arg in args {
                walk_expr(bodies, arg, out);
            }
            if let Some(receiver) = receiver {
                walk_expr(bodies, *receiver, out);
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
            for param in params {
                if let Some(ty) = &param.ty {
                    out.push((DiagLocation::Expr(id), ty.clone()));
                }
                // §15.27.1/[§9.7.4]: a lambda parameter's declaration
                // annotations, like a formal parameter's.
                for annotation in &param.annotations {
                    out.push((DiagLocation::Expr(id), annotation_reference(annotation)));
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
