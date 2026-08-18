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
//! * [`method`] — the member set, access control and the applicability phases
//!   of method resolution ([§15.12](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12)).
//! * [`inference`] — method invocation type inference ([§18.5.2]).
//! * [`db`] — [`TyDatabase`], the salsa database trait.
//!
//! # Known limitations
//!
//! * Method bodies are dropped during lowering, so there is no expression IR:
//!   expression-level type inference is unavailable, and the compatibility of
//!   the invocation with a *target type* ([JLS §18.5.2.4]) is not modelled —
//!   a bare invocation has none. The least upper bound of
//!   [§4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)
//!   is approximated by the most specific bound under subtyping, the capture
//!   of `? super T` ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10))
//!   is not modelled, and throws inference
//!   ([§18.5.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.3))
//!   is out of scope. Supporting expression-level inference is future work and
//!   requires `hir-def` to keep a body IR.
//! * Access control
//!   ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
//!   is enforced from the [`method::InvocationContext`]: when the caller omits
//!   the enclosing class or package, the corresponding restriction is treated
//!   permissively.
//!
//! All JLS references use the Java SE 26 edition
//! (<https://docs.oracle.com/javase/specs/jls/se26/html/index.html>).

pub mod db;
pub mod inference;
pub mod method;
pub mod resolve;
pub mod subtyping;
pub mod ty;

pub use db::TyDatabase;
pub use method::{
    Access, InvocationContext, InvocationMode, MethodData, MethodDisplay, MethodTypeParam,
    member_set, pick_method,
};
pub use resolve::{
    Resolver, item_ty, method_params, resolve_type_ref, scope_for_file, ty_from_library,
};
pub use subtyping::{is_assignable, is_subtype, supertypes};
pub use ty::{BoundKind, Ty, TyData, TyDisplay, TyKind, WildcardBound, ty_from_source};
