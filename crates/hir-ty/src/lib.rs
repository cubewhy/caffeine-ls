//! The type layer.
//!
//! `hir-ty` computes the types of the declarations produced by `hir-def`'s
//! lowering — fields, method signatures and type declarations — plus the
//! subtype ([JLS §4.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10))
//! and assignability ([JLS §5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2))
//! relations over them, and the types of the expressions and locals of method
//! bodies against the body IR kept by `hir-def`.
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
//! * [`method`] — the member set, access control, field resolution and the
//!   applicability phases of method resolution ([§15.12](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12)).
//! * [`infer`] — expression-level type inference over the lowered body IR.
//! * [`inference`] — method invocation type inference ([§18.5.2]).
//! * [`db`] — [`TyDatabase`], the salsa database trait.
//!
//! # Known limitations
//!
//! * Target typing ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4))
//!   refines a method invocation's inference variables with its expected type
//!   in the contexts that fix it — declaration initializers, assignment
//!   right-hand sides and returned expressions — but the general poly-
//!   expression rules of §18.5.2.4 (e.g. argument positions) are not fully
//!   modelled. Lambdas and method references are poly expressions
//!   ([§15.27](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27),
//!   [§15.13](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13))
//!   whose type comes from the target functional interface, so they infer to
//!   `error` in isolation. Boxing is not modelled in binary numeric promotion
//!   ([§5.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2));
//!   the element type of a for-each loop is modelled for arrays but not for
//!   `Iterable` ([§14.14.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14.2)).
//!   The initializer of `new T[] {...}` is not lowered into the body IR.
//! * Access control
//!   ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
//!   is enforced from the [`method::InvocationContext`]. For source call sites
//!   [`method::access_context`] derives the enclosing class and package from
//!   the call site ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)),
//!   so the corresponding restrictions are enforced rather than treated
//!   permissively; a `None` context field (library/test callers) remains
//!   permissive.
//!
//! All JLS references use the Java SE 26 edition
//! (<https://docs.oracle.com/javase/specs/jls/se26/html/index.html>).

pub mod db;
pub mod infer;
pub mod inference;
pub mod method;
pub mod resolve;
pub mod subtyping;
pub mod ty;

pub use db::TyDatabase;
pub use infer::{BodyTypes, body_types};
pub use inference::least_upper_bound;
pub use method::{
    Access, FieldData, InvocationContext, InvocationMode, MethodData, MethodDisplay,
    MethodTypeParam, access_context, member_set, pick_field, pick_method,
};
pub use resolve::{
    Resolver, item_ty, method_params, resolve_type_ref, scope_for_file, ty_from_library,
};
pub use subtyping::{is_assignable, is_subtype, supertypes};
pub use ty::{
    BoundKind, Ty, TyData, TyDisplay, TyKind, WildcardBound, capture_conversion, ty_from_source,
};
