//! Source-side type name resolution.
//!
//! Resolves the names inside a lowered item tree to canonical fully qualified
//! names, following the declaration rules for simple type names
//! ([JLS §6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1))
//! and qualified type names
//! ([JLS §6.5.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.2)),
//! with the import machinery of
//! [JLS §7.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.1)
//! (single-type imports),
//! [JLS §7.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.2)
//! (on-demand imports) and the implicit `java.lang` fallback. Type parameters
//! in scope ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3))
//! become [`TyKind::TypeVar`].
//!
//! A [`Resolver`] captures the per-file name context of one item: the
//! compilation unit's package and imports, plus the type parameters in scope
//! at that item. Resolution itself runs against a [`hir::ResolutionScope`]:
//! the candidates are probed with [`hir::fqn_resolve`] against that scope's
//! classpath, and the first one that exists wins.

use rustc_hash::FxHashMap;
use vfs::FileId;

use hir_expand::{
    item_tree::{ImportItem, ItemData, ItemId, ItemTree, TypeParam},
    name::Name,
};
use syntax::stub::{TypeBound, TypeRef};

use crate::{
    db::TyDatabase,
    ty::{BoundKind, Ty, WildcardBound, ty_from_type_ref},
};

/// The per-file name context of a single item: its package, the compilation
/// unit's imports and the type parameters in scope at the item.
#[derive(Debug, Clone)]
pub struct Resolver {
    package: Option<Name>,
    imports: Vec<ImportItem>,
    type_params: Vec<TypeParam>,
}

impl Resolver {
    /// Builds the resolver for `item_id` within `tree`, looking the type
    /// parameters in scope up in the per-file map computed by
    /// [`type_params_map`].
    pub fn new(
        tree: &ItemTree,
        type_params: &FxHashMap<ItemId, Vec<TypeParam>>,
        item_id: ItemId,
    ) -> Self {
        Self {
            package: tree.package.clone(),
            imports: tree.imports.clone(),
            type_params: type_params.get(&item_id).cloned().unwrap_or_default(),
        }
    }

    pub fn package(&self) -> Option<&Name> {
        self.package.as_ref()
    }

    pub fn imports(&self) -> &[ImportItem] {
        &self.imports
    }

    /// For a static import ([JLS §7.5.4]) that names `simple` as a member —
    /// `import static pkg.Type.MEMBER` or `import static pkg.Type.*` — the
    /// declaring type's FQN and the member's simple name, in declaration
    /// order (the first matching import wins, [JLS §7.5.4]).
    pub fn static_import_owner(&self, simple: &str) -> Option<(Name, String)> {
        for import in &self.imports {
            if !import.is_static {
                continue;
            }
            let text = import.name.as_str();
            if import.is_asterisk {
                return Some((import.name.clone(), simple.to_owned()));
            }
            let (owner, member) = text.rsplit_once('.')?;
            if member == simple {
                return Some((Name::new(owner), member.to_owned()));
            }
        }
        None
    }

    pub fn type_params(&self) -> &[TypeParam] {
        &self.type_params
    }
}

/// The type parameters in scope at every item of `tree` ([JLS §6.3]):
/// those of every enclosing type declaration plus, for methods, the method's
/// own parameters, with their declared bounds
/// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)).
/// Computed in a single tree walk so each item's scope is a map lookup.
pub(crate) fn type_params_map(tree: &ItemTree) -> FxHashMap<ItemId, Vec<TypeParam>> {
    fn collect(
        tree: &ItemTree,
        id: ItemId,
        outer: &[TypeParam],
        map: &mut FxHashMap<ItemId, Vec<TypeParam>>,
    ) {
        let data = tree.data(id);
        let mut own = outer.to_vec();
        match data {
            ItemData::Class(d) | ItemData::Interface(d) => {
                own.extend(d.type_params.iter().cloned());
            }
            ItemData::Record(d) => {
                own.extend(d.type_params.iter().cloned());
            }
            ItemData::Method(m) => {
                own.extend(m.sig.type_params.iter().cloned());
            }
            _ => {}
        }
        map.insert(id, own.clone());
        for &child in data.body() {
            collect(tree, child, &own, map);
        }
    }

    let mut map = FxHashMap::default();
    for &top in &tree.top {
        collect(tree, top, &[], &mut map);
    }
    map
}

/// Resolves a source [`TypeRef<Name>`] to a [`Ty`]. Reference names are
/// resolved per
/// [JLS §6.5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5)
/// against `scope`'s classpath; names that resolve to nothing degrade to the
/// most qualified candidate so the [`Ty`] stays displayable.
pub fn resolve_type_ref(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    tyref: &TypeRef<Name>,
) -> Ty {
    resolve_type_ref_impl(db, scope, resolver, tyref, &mut Vec::new())
}

/// The recursion-guarded form of [`resolve_type_ref`]. `resolving` is the
/// stack of type parameters currently having their bounds resolved; a
/// re-entrant reference to one of them ([JLS §4.4] recursion such as
/// `T extends Comparable<T>`) yields the type variable without bounds so
/// interning terminates.
fn resolve_type_ref_impl(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    tyref: &TypeRef<Name>,
    resolving: &mut Vec<Name>,
) -> Ty {
    match tyref {
        TypeRef::Primitive(p) => Ty::primitive(db, *p),
        TypeRef::Reference { name, generic_args } => {
            let args = generic_args
                .iter()
                .map(|arg| resolve_type_ref_impl(db, scope, resolver, arg, resolving))
                .collect();
            if let Some(tp) = resolver.type_params.iter().find(|tp| tp.name == *name) {
                // A type parameter in scope wins over any type named the same.
                let bounds = if resolving.iter().any(|n| n == name) {
                    Vec::new()
                } else {
                    resolving.push(name.clone());
                    let bounds = tp
                        .bounds
                        .iter()
                        .map(|bound| resolve_type_ref_impl(db, scope, resolver, bound, resolving))
                        .collect();
                    resolving.pop();
                    bounds
                };
                Ty::type_var(db, name.clone(), bounds)
            } else {
                Ty::reference(db, resolve_reference_name(db, scope, resolver, name), args)
            }
        }
        TypeRef::Wildcard { bound } => Ty::wildcard(
            db,
            bound.as_deref().map(|b| match b {
                TypeBound::Upper(t) => Box::new(WildcardBound {
                    kind: BoundKind::Upper,
                    ty: resolve_type_ref_impl(db, scope, resolver, t, resolving),
                }),
                TypeBound::Lower(t) => Box::new(WildcardBound {
                    kind: BoundKind::Lower,
                    ty: resolve_type_ref_impl(db, scope, resolver, t, resolving),
                }),
            }),
        ),
        TypeRef::TypeVariable(v) => Ty::type_var(db, v.clone(), Vec::new()),
        TypeRef::Array(inner) => Ty::array(
            db,
            resolve_type_ref_impl(db, scope, resolver, inner, resolving),
        ),
        TypeRef::Error => Ty::error(db),
    }
}

/// Resolves `name` to a canonical fully qualified name
/// ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)).
///
/// The candidate order follows
/// [JLS §6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1)
/// and [JLS §7.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5):
/// single-type imports, the current package, `java.lang`, on-demand imports,
/// then the unnamed package. The first candidate that exists on the classpath
/// wins; if none does, the most qualified candidate is kept so the name
/// remains usable for display and later resolution.
fn resolve_reference_name(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    name: &Name,
) -> Name {
    let candidates = candidate_fqns(resolver, name);
    for candidate in &candidates {
        if hir::fqn_resolve(db, scope, candidate.as_str()).is_some() {
            return candidate.clone();
        }
    }
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| name.clone())
}

/// The candidate FQNs for a (possibly qualified) type name, most specific
/// first. A qualified name is tried as-is first (it may already be fully
/// qualified), then with each simple-name resolution of its prefix
/// ([JLS §6.5.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.2)).
pub(crate) fn candidate_fqns(resolver: &Resolver, name: &Name) -> Vec<Name> {
    let text = name.as_str();
    if let Some((prefix, rest)) = text.split_once('.') {
        let mut out = vec![name.clone()];
        for candidate in simple_candidates(resolver, prefix) {
            out.push(join(&candidate, rest));
        }
        out
    } else {
        simple_candidates(resolver, text)
    }
}

/// The candidate FQNs for a simple type name, in
/// [JLS §7.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5)
/// precedence order.
fn simple_candidates(resolver: &Resolver, simple: &str) -> Vec<Name> {
    simple_candidates_with_kind(resolver, simple)
        .into_iter()
        .map(|(_, name)| name)
        .collect()
}

/// The step ([JLS §6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1),
/// [§7.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5))
/// a simple-name candidate belongs to. The step drives the *checked*
/// resolution ([`resolve_name_checked`]): whether a name may fall through to
/// a later step, and whether two on-demand imports make it ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateStep {
    /// A single-type import whose simple name matches ([§7.5.1]).
    SingleImport,
    /// A type in the current package ([§7.4.2]).
    CurrentPackage,
    /// A type in `java.lang` (implicitly imported, [§7.3]).
    JavaLang,
    /// A type reachable through an on-demand import ([§7.5.2]).
    OnDemand,
    /// A type in the unnamed package ([§7.4.2]).
    UnnamedPackage,
}

fn simple_candidates_with_kind(resolver: &Resolver, simple: &str) -> Vec<(CandidateStep, Name)> {
    let mut out = Vec::new();

    // 1. a single-type import whose simple name matches (§7.5.1)
    if let Some(import) = resolver.imports.iter().find(|import| {
        !import.is_static
            && !import.is_asterisk
            && import.name.as_str().rsplit('.').next() == Some(simple)
    }) {
        out.push((CandidateStep::SingleImport, import.name.clone()));
    }

    // 2. a type in the current package (§7.4.2)
    if let Some(package) = &resolver.package {
        out.push((CandidateStep::CurrentPackage, join(package, simple)));
    }

    // 3. a type in `java.lang`
    out.push((
        CandidateStep::JavaLang,
        Name::new(&format!("java.lang.{simple}")),
    ));

    // 4. a type reachable through an on-demand import (§7.5.2)
    for import in resolver
        .imports
        .iter()
        .filter(|import| !import.is_static && import.is_asterisk)
    {
        out.push((CandidateStep::OnDemand, join(&import.name, simple)));
    }

    // 5. a type in the unnamed package
    out.push((CandidateStep::UnnamedPackage, Name::new(simple)));
    out
}

/// The outcome of a *checked* name resolution ([JLS §6.5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5))
/// — the primitive the unknown-type diagnostics are built from. Unlike
/// [`resolve_reference_name`], which degrades an unresolvable name to its
/// most-qualified candidate so the [`Ty`] stays displayable, this reports
/// *whether* the name resolved, and the exact divergences from the JLS rules
/// ([§6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1),
/// [§7.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.2)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameResolution {
    /// The name denotes a type variable of the enclosing scope
    /// ([§6.5.5.1] step 1).
    TypeVar,
    /// Resolved to this canonical fully qualified name ([§6.7]).
    Resolved(Name),
    /// The simple name is accessible through two or more on-demand imports
    /// that denote different types — a compile-time error ([§6.5.5.1],
    /// [§7.5.2]).
    Ambiguous(Vec<Name>),
    /// A candidate class exists on the classpath, but its package is not
    /// visible from the resolving source set's module
    /// ([§7.4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.3),
    /// [§7.7.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7.2)) —
    /// the package is *observable* but not *visible*.
    NotAccessible(Name),
    /// No candidate resolves — either the name is shadowed by a broken
    /// single-type import whose imported type does not exist ([§7.5.1],
    /// in which case the name cannot fall through), or nothing on the
    /// classpath provides it.
    Unresolved,
}

/// Whether `fqn`'s package is visible from `module_ctx` ([§7.4.3], [§7.7.2]).
/// Types in the unnamed package are never module-hidden.
fn fqn_visible(db: &dyn TyDatabase, module_ctx: &hir::ModuleCtx, fqn: &str) -> bool {
    match fqn.rsplit_once('.') {
        Some((package, _)) => module_ctx.package_visible(&db.hir_state().interner, package),
        None => true,
    }
}

/// The checked resolution of `name` against `scope`'s classpath
/// ([JLS §6.5.5.1], [§6.5.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.2)).
///
/// A name in expression position that is *not* a type — a local or field of
/// the implicit receiver — must not be reported here; callers only pass names
/// from type-reference positions. A name shadowed by a single-type import
/// that itself names a non-existent class (§7.5.1 makes the import a
/// compile-time error) resolves to nothing rather than falling through to a
/// same-package class.
pub fn resolve_name_checked(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    name: &Name,
) -> NameResolution {
    let text = name.as_str();
    // 1. a type parameter in scope wins over any type named the same (§6.5.5.1).
    if resolver.type_params.iter().any(|tp| tp.name == *name) {
        return NameResolution::TypeVar;
    }

    // The resolving source set's module context gates each candidate's package
    // visibility ([§7.4.3], [§7.7.2]); a candidate whose package is not
    // visible is not a resolution, but a class that *exists* invisibly is
    // worth distinguishing from "nothing on the classpath" for diagnostics.
    let module_ctx = hir::module_ctx_for_scope(db, scope);

    // §6.5.5.2: a qualified name — tried as-is first, then with each
    // simple-name resolution of its prefix. (On-demand ambiguity only applies
    // to the *simple-name* step and is reported at the prefix's own use.)
    if let Some((prefix, rest)) = text.split_once('.') {
        let mut candidates = vec![name.clone()];
        for candidate in simple_candidates(resolver, prefix) {
            candidates.push(join(&candidate, rest));
        }
        let mut hidden: Option<Name> = None;
        for candidate in &candidates {
            if hir::fqn_resolve(db, scope, candidate.as_str()).is_none() {
                continue;
            }
            if fqn_visible(db, &module_ctx, candidate.as_str()) {
                return NameResolution::Resolved(candidate.clone());
            }
            hidden.get_or_insert_with(|| candidate.clone());
        }
        return match hidden {
            Some(fqn) => NameResolution::NotAccessible(fqn),
            None => NameResolution::Unresolved,
        };
    }

    let candidates = simple_candidates_with_kind(resolver, text);
    let mut hidden: Option<Name> = None;
    for (idx, (step, candidate)) in candidates.iter().enumerate() {
        // §7.5.1: a single-type import *shadows* the simple name; if it names
        // a class that cannot be found the import is an error and the name
        // does not fall through to a later step.
        if *step == CandidateStep::SingleImport {
            if hir::fqn_resolve(db, scope, candidate.as_str()).is_none() {
                return NameResolution::Unresolved;
            }
            return if fqn_visible(db, &module_ctx, candidate.as_str()) {
                NameResolution::Resolved(candidate.clone())
            } else {
                NameResolution::NotAccessible(candidate.clone())
            };
        }
        if hir::fqn_resolve(db, scope, candidate.as_str()).is_some() {
            if !fqn_visible(db, &module_ctx, candidate.as_str()) {
                hidden.get_or_insert_with(|| candidate.clone());
                continue;
            }
            if *step == CandidateStep::OnDemand {
                // §6.5.5.1/[§7.5.2]: two or more on-demand imports that supply
                // the simple name from different types make the name
                // ambiguous — a compile-time error.
                let mut conflicting = Vec::new();
                for (later_step, later) in &candidates[idx + 1..] {
                    if *later_step == CandidateStep::OnDemand
                        && hir::fqn_resolve(db, scope, later.as_str()).is_some()
                        && fqn_visible(db, &module_ctx, later.as_str())
                        && later != candidate
                    {
                        conflicting.push(later.clone());
                    }
                }
                if !conflicting.is_empty() {
                    conflicting.insert(0, candidate.clone());
                    return NameResolution::Ambiguous(conflicting);
                }
            }
            return NameResolution::Resolved(candidate.clone());
        }
    }
    match hidden {
        Some(fqn) => NameResolution::NotAccessible(fqn),
        None => NameResolution::Unresolved,
    }
}

fn join(prefix: &Name, suffix: &str) -> Name {
    let mut text = String::with_capacity(prefix.as_str().len() + 1 + suffix.len());
    text.push_str(prefix.as_str());
    text.push('.');
    text.push_str(suffix);
    Name::new(&text)
}

/// The resolution scope of a source file: its source set, or the JDK
/// built-ins when the file is not mapped to a source root.
pub fn scope_for_file(db: &dyn TyDatabase, file_id: FileId) -> hir::ResolutionScope {
    match hir::source_set_for_file(db, file_id) {
        Some(source_set) => hir::ResolutionScope::SourceSet(source_set),
        None => hir::ResolutionScope::JdkBuiltins,
    }
}

/// Whether `fqn` names a *generic class* — one declaring type parameters
/// ([JLS §8.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.2)).
/// A reference to it without type arguments is a raw type ([§4.8], [§4.12.2]).
pub(crate) fn class_is_generic(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &Name,
) -> bool {
    let Some(resolved) = hir::fqn_resolve(db, scope, fqn.as_str()) else {
        return false;
    };
    match resolved {
        // A library class is generic when its classfile `Signature` attribute
        // ([JVMS §4.7.9.1]) declares type parameters.
        hir::Resolved::Library(_) => {
            hir::class_generic_info(db, &resolved).is_some_and(|info| !info.type_params.is_empty())
        }
        // A source class is generic when its declaration carries them.
        hir::Resolved::Source(source) => {
            let tree = hir::file_item_tree(db, source.file);
            let type_params = match tree.data(source.item) {
                ItemData::Class(d) | ItemData::Interface(d) => Some(&d.type_params),
                ItemData::Record(d) => Some(&d.type_params),
                _ => None,
            };
            type_params.is_some_and(|params| !params.is_empty())
        }
    }
}

/// The declared type of an item: the type of a field, the return type of a
/// method, or the type of a class/interface/enum/record/annotation
/// declaration. Memoized per (file, item) by the tracked query in [`crate::db`].
pub fn item_ty(db: &dyn TyDatabase, file_id: FileId, item_id: ItemId) -> Ty {
    crate::db::item_ty_query(db, crate::db::ItemKey::new(db, file_id, item_id))
}

/// The parameter types of a method or constructor, in declaration order.
/// Memoized per (file, item) by the tracked query in [`crate::db`].
pub fn method_params(db: &dyn TyDatabase, file_id: FileId, item_id: ItemId) -> Vec<Ty> {
    crate::db::method_params_query(db, crate::db::ItemKey::new(db, file_id, item_id))
}

/// Lowers a library [`TypeRef<Symbol>`] to a [`Ty`]. Library names are
/// already fully qualified, so only the interner lookup is needed.
pub fn ty_from_library(db: &dyn TyDatabase, tyref: &TypeRef<hir::Symbol>) -> Ty {
    let interner = &db.hir_state().interner;
    ty_from_type_ref(db, tyref, &mut |symbol| Name::new(interner.resolve(symbol)))
}

pub(crate) fn item_data(tree: &ItemTree, item_id: ItemId) -> Option<&ItemData> {
    // `Arena::get` panics on unknown ids, so bounds-check first.
    (item_id.0.0 < tree.items.len() as u32).then(|| tree.data(item_id))
}
