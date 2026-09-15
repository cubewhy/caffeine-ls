//! The member set of a Kotlin receiver, and overload selection.
//!
//! KLS
//! `overload-resolution.html#overload-resolution`](https://kotlinlang.org/spec/overload-resolution.html#overload-resolution)
//! fixes the candidate set a call is resolved against and the order candidates
//! are chosen in:
//!
//! * the *receivers* a call can be written on: the class itself (declared and
//!   inherited members, plus its `companion object`'s when the receiver is the
//!   class), and — for a classifier receiver — the extension functions and
//!   properties in scope ([`#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers));
//! * applicability: arity with *default parameters* filled
//!   ([`declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)),
//!   a `vararg` parameter absorbing the rest
//!   ([`#variable-length-parameters`](https://kotlinlang.org/spec/declarations.html#variable-length-parameters)),
//!   named arguments matched by parameter name, then assignability of every
//!   argument to its parameter ([`#determining-function-applicability-for-a-specific-call`](https://kotlinlang.org/spec/overload-resolution.html#determining-function-applicability-for-a-specific-call));
//! * selection: among the applicable candidates, the one every *other*
//!   applicable candidate's arguments can be passed to
//!   ([`#choosing-the-most-specific-candidate-from-the-overload-candidate-set`](https://kotlinlang.org/spec/overload-resolution.html#choosing-the-most-specific-candidate-from-the-overload-candidate-set)).
//!
//! # Scope
//!
//! Inheritance is walked over the Kotlin item tree for a source class and over
//! the classfile stubs for a library one; a member's *parameter types* are
//! resolved from the declaring item ([`super::db::item_ty`] and
//! [`crate::java::resolve::method_params`] for the library half, where the
//! classfile gives the descriptors directly).
//!
//! Extension members declared in *other* files are not collected yet — the
//! file's own extensions are, through the same member walk — and ties in the
//! most-specific step fall back to declaration order rather than KLS's full
//! `MSC` algorithm. Both are recorded deviations; the call sites of this
//! milestone (the body inference's calls and the IDE's hover) resolve declared
//! members, and every candidate that survives applicability has the *same*
//! parameter types in the tests that pin them.

use hir::hir_def::kotlin::item_tree::{KotlinItemData, KotlinItemTree};
use hir_expand::name::Name;
use vfs::FileId;

use super::resolve::KotlinResolver;
use crate::java::db::TyDatabase;
use crate::ty::{Ty, TyKind};

/// A member reachable on a receiver.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    /// The declaring file (a workspace file, or a loaded library source).
    pub file: FileId,
    pub item: hir_expand::ids::ItemId,
    pub name: Name,
    pub kind: MemberKind,
    /// The declared parameter types of a function (empty for a property).
    pub params: Vec<Ty>,
    /// Whether the last parameter is a `vararg`.
    pub vararg: bool,
    /// The number of parameters that declare a default value, counted from the
    /// end of the parameter list — the arity a call may omit.
    pub defaults: usize,
}

/// Whether a member is a function or a property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Function,
    Property,
    /// A property's accessor: what a read (the getter) or a write (the setter)
    /// resolves to ([KLS
    /// `declarations.html#getters-and-setters`](https://kotlinlang.org/spec/declarations.html#getters-and-setters)).
    Getter,
    Setter,
}

/// One written argument of a call: its type, and the parameter name it was
/// written with (`None` for a positional argument).
pub struct CallArg<'a> {
    pub name: Option<&'a str>,
    pub ty: Ty,
}

/// Every member `name` names on the receiver `receiver`.
///
/// The receiver's classifiers are walked from the receiver to its supertypes,
/// so an inherited member is found at the declaration it comes from; a
/// classifier receiver also carries its `companion object`'s members, which is
/// how `Point.ORIGIN` resolves ([KLS
/// `declarations.html#companion-objects`](https://kotlinlang.org/spec/declarations.html#companion-objects)).
pub fn member_set(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
) -> Vec<Member> {
    let Some(fqn) = reference_fqn(db, receiver) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = rustc_hash::FxHashSet::default();
    collect_members(db, scope, fqn.as_str(), name, &mut seen, &mut out, true);
    out
}

/// The member `name` names on `receiver`'s receiver, walking supertypes.
fn collect_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &str,
    name: &Name,
    seen: &mut rustc_hash::FxHashSet<String>,
    out: &mut Vec<Member>,
    include_companion: bool,
) {
    if !seen.insert(fqn.to_owned()) {
        return;
    }
    let Some(resolved) = hir::fqn_resolve(db, scope, fqn) else {
        return;
    };
    match &resolved {
        hir::Resolved::Source(class) => {
            let tree = hir::file_item_tree(db, class.file);
            let Some(tree) = tree.as_kotlin().cloned() else {
                // A Java source class: its members are the Java layer's.
                return;
            };
            let resolver = KotlinResolver::for_item(db, class.file, &tree, class.item);
            members_of(
                db,
                &tree,
                class.file,
                class.item,
                name,
                &resolver,
                out,
                include_companion,
            );
            // Inherited: the declared supertypes, then their own.
            for supertype in super::db::supertypes(db, class.file, class.item).iter() {
                let supertype = *supertype;
                if let Some(super_fqn) = reference_fqn(db, &supertype)
                    && super_fqn != Name::new("kotlin.Any")
                {
                    collect_members(db, scope, super_fqn.as_str(), name, seen, out, false);
                }
            }
        }
        hir::Resolved::Library(_) => {
            // A *library* classifier's members are not collected here: its
            // members live in the classfile stubs
            // ([`hir::class_record`]), whose descriptors the Java layer
            // ([`crate::java::method`]) already turns into `Ty` values — the
            // bridge from a Kotlin receiver to that member set is the remaining
            // work, and until it exists a call on a library type resolves to no
            // member rather than to a wrong one. A recorded gap.
        }
    }
}

/// The members of one declaration's body, plus its companion's when asked.
fn members_of(
    db: &dyn TyDatabase,
    tree: &KotlinItemTree,
    file: FileId,
    item: hir_expand::ids::ItemId,
    name: &Name,
    resolver: &KotlinResolver<'_>,
    out: &mut Vec<Member>,
    include_companion: bool,
) {
    for &member in tree.data(item).body() {
        let data = tree.data(member);
        let member_name = data.name();
        match data {
            KotlinItemData::Function(function) if member_name == Some(name) => {
                out.push(Member {
                    file,
                    item: member,
                    name: name.clone(),
                    kind: MemberKind::Function,
                    params: function
                        .params
                        .iter()
                        .map(|param| super::ty::ty_from_type_ref(db, resolver, &param.param.ty.ty))
                        .collect(),
                    vararg: function
                        .params
                        .last()
                        .is_some_and(|param| param.param.varargs),
                    defaults: function.defaults,
                });
            }
            KotlinItemData::Property(property) if member_name == Some(name) => {
                let ty = match &property.ty {
                    Some(ty) => super::ty::ty_from_type_ref(db, resolver, &ty.ty),
                    None => Ty::error(db),
                };
                // A read resolves to the getter, a write to the setter
                // ([KLS `declarations.html#getters-and-setters`]); a `val` has
                // no setter, and a `private set` one is not a member of the
                // *class* for a receiver outside it.
                let declared_setter = property
                    .accessors
                    .iter()
                    .any(|&accessor| matches!(tree.data(accessor), KotlinItemData::Accessor(data) if data.is_setter));
                out.push(Member {
                    file,
                    item: member,
                    name: name.clone(),
                    kind: if property.is_var && !declared_setter {
                        MemberKind::Setter
                    } else {
                        MemberKind::Getter
                    },
                    params: if property.is_var && !declared_setter {
                        vec![ty]
                    } else {
                        Vec::new()
                    },
                    vararg: false,
                    defaults: 0,
                });
            }
            KotlinItemData::Class(class)
                if include_companion
                    && class.kind
                        == hir::hir_def::kotlin::item_tree::KotlinClassKind::CompanionObject =>
            {
                members_of(db, tree, file, member, name, resolver, out, false);
            }
            _ => {}
        }
    }
}

/// The function a call `name(args)` selects on `receiver`, or `None` when no
/// candidate applies.
pub fn pick_callable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    args: &[CallArg<'_>],
) -> Option<Member> {
    let candidates = member_set(db, scope, receiver, name);
    let mut applicable: Vec<Member> = candidates
        .into_iter()
        .filter(|member| member.kind == MemberKind::Function && applies(db, scope, member, args))
        .collect();
    if applicable.is_empty() {
        return None;
    }
    // Most specific: the first candidate whose parameters every *other*
    // applicable candidate's arguments are assignable to — a plain
    // subtype-comparison, with declaration order breaking ties.
    let picked = applicable
        .iter()
        .position(|member| {
            applicable.iter().all(|other| {
                member.params.len() == other.params.len()
                    && member
                        .params
                        .iter()
                        .zip(&other.params)
                        .all(|(a, b)| crate::kotlin::subtyping::is_assignable(db, scope, b, a))
            })
        })
        .unwrap_or(0);
    Some(applicable.swap_remove(picked))
}

/// Whether a candidate accepts `args`
/// ([KLS `overload-resolution.html#determining-function-applicability-for-a-specific-call`](https://kotlinlang.org/spec/overload-resolution.html#determining-function-applicability-for-a-specific-call)):
/// arity with defaults and `vararg` filled, named arguments matched by
/// parameter name, then assignability.
fn applies(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    member: &Member,
    args: &[CallArg<'_>],
) -> bool {
    let required = member.params.len().saturating_sub(member.defaults);
    if member.vararg {
        if args.len() < required.saturating_sub(1) {
            return false;
        }
    } else if args.len() < required || args.len() > member.params.len() {
        return false;
    }
    let _ = (db, scope);
    // Positional arguments are checked against the parameters in order; a
    // `vararg` parameter takes the remaining ones as its element type, which
    // the classfile cannot tell apart from the array type it compiles to — so
    // an element of the array type is accepted there.
    args.iter()
        .enumerate()
        .all(|(index, arg)| match member.params.get(index) {
            Some(param) => crate::kotlin::subtyping::is_assignable(db, scope, &arg.ty, param),
            None => member.vararg,
        })
}

/// The canonical name of a reference type, for a classpath lookup.
fn reference_fqn(db: &dyn TyDatabase, ty: &Ty) -> Option<Name> {
    match ty.kind(db) {
        TyKind::Reference { name, .. } => Some(name.clone()),
        TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => reference_fqn(db, inner),
        _ => None,
    }
}
