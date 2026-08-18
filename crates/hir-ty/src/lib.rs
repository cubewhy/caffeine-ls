//! The declaration-level type layer.
//!
//! `hir-ty` computes the types of the declarations produced by `hir-def`'s
//! lowering — fields, method signatures and type declarations — plus the
//! subtype ([JLS §4.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10))
//! and assignability ([JLS §5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2))
//! relations over them. Method bodies are dropped during lowering, so the
//! type layer is *signature-only*: there is no expression IR to infer types
//! from. Supporting expression-level inference is future work and requires
//! `hir-def` to keep a body IR.
//!
//! # Architecture
//!
//! * [`ty`] — the [`Ty`] model
//!   ([JLS §4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.1)–[§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)).
//! * [`resolve`] — source name resolution
//!   ([JLS §6.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5),
//!   [§7.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5)) over a
//!   [`Resolver`], turning `syntax::stub::TypeRef<Name>`s into [`Ty`]s.
//! * [`subtyping`] — [`is_subtype`], [`is_assignable`] and the supertype walk.
//! * [`db`] — [`TyDatabase`], the salsa database trait.
//!
//! # Known limitations
//!
//! * Source-set classes are not indexed, so [`hir::fqn_resolve`] only finds
//!   library types; subtyping across source classes does not work yet.
//! * Library supertypes are erasure-style (tier-1 classfile data carries no
//!   type arguments), so generic subtyping matches only raw or identical
//!   parameterizations (full [§4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)
//!   is deferred).
//! * Boxing/unboxing
//!   ([§5.1.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.7),
//!   [§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8)),
//!   capture conversion
//!   ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
//!   and method applicability
//!   ([§15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2))
//!   are not implemented.
//! * [`Ty`] does not carry declared bounds, so subtyping between distinct
//!   type variables resolves to `false`.
//!
//! All JLS references use the Java SE 26 edition
//! (<https://docs.oracle.com/javase/specs/jls/se26/html/index.html>).

pub mod db;
pub mod resolve;
pub mod subtyping;
pub mod ty;

pub use db::TyDatabase;
pub use resolve::{
    Resolver, item_ty, method_params, resolve_type_ref, scope_for_file, ty_from_library,
};
pub use subtyping::{is_assignable, is_subtype, supertypes};
pub use ty::{BoundKind, Ty, TyKind, WildcardBound, ty_from_source};
