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
//! # Method invocation inference and access control
//!
//! * Method invocation inference ([JLS §18.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2))
//!   is joint for a nested poly invocation in an argument position: its
//!   constraints are contributed to the enclosing call's inference table, so
//!   `take(emptyList())` types the nested `emptyList()` as `List<String>`
//!   against `take(List<String>)` even when the formal still mentions an
//!   uninstantiated type variable. The nested invocation's own candidate
//!   selection is independent of the enclosing one: each candidate is probed
//!   against its own fresh bound set ([JLS §18.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.1)),
//!   the most specific applicable one wins
//!   ([§15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5),
//!   [JLS §18.5.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.4)),
//!   and only its constraints are lifted into the enclosing table
//!   ([JLS §18.5.2.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.1),
//!   [JLS §18.5.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.2)).
//!   The poly arguments — lambdas, method references and nested invocations —
//!   are re-inferred against the resolved formal parameters
//!   ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4)).
//!   Lambdas and method references are poly expressions
//!   ([§15.27](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27),
//!   [§15.13](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13))
//!   whose type comes from the target functional interface, so they infer to
//!   `error` in isolation. Boxing in binary numeric promotion
//!   ([§5.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2))
//!   is modelled: a boxed reference operand is unboxed
//!   ([§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8))
//!   before the promoted type is computed. For-each loops over an `Iterable`
//!   ([§14.14.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14.2))
//!   bind the loop variable to the element type of the iterable, resolved
//!   through its `iterator()` method ([§14.14.2.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14.2.1));
//!   arrays resolve the element type directly. An array creation initializer
//!   `new T[] { ... }`
//!   ([§15.10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.10.1))
//!   is lowered into the body IR with its element expressions, and the created
//!   array has type `T[]`.
//! * Access control
//!   ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
//!   is enforced from the [`method::InvocationContext`]. For source call sites
//!   [`method::access_context`] derives the enclosing class and package from
//!   the call site ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)),
//!   and the invocation mode ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
//!   is derived per call site — a bare type name receiver is a static
//!   invocation, `super` a super invocation, an expression a virtual
//!   invocation. A `None` context field is never permissive: private members
//!   are limited to their top-level class (§6.6.1), package members to their
//!   package, and `protected` members (§6.6.2) to the declaring package or a
//!   subclass whose receiver is a subtype of the enclosing class. The member
//!   set of a name ([`method::member_set`]) and the access context of a call
//!   site ([`db::access_context_key_query`]) are memoized salsa queries.
//!
//! All JLS references use the Java SE 26 edition
//! (<https://docs.oracle.com/javase/specs/jls/se26/html/index.html>).

pub mod db;
pub mod diagnostics;
pub mod infer;
pub mod inference;
pub mod method;
pub mod resolve;
pub mod subtyping;
pub mod ty;

pub use db::TyDatabase;
// `DiagnosticCode` lives in the shared `syntax` crate; re-export it here so
// the hir-ty API can keep naming it directly.
pub use diagnostics::{DiagLocation, TypeError};
pub use infer::{BodyTypes, body_types};
pub use inference::least_upper_bound;
pub use method::{
    Access, FieldData, InvocationContext, InvocationMode, MethodData, MethodDisplay,
    MethodTypeParam, PolyArg, abstract_methods, access_context, member_set, pick_field,
    pick_method, single_abstract_method,
};
pub use resolve::{
    Resolver, item_ty, method_params, resolve_type_ref, scope_for_file, ty_from_library,
};
pub use subtyping::{is_assignable, is_subtype, supertypes};
pub use syntax::{DiagnosticCode, JavaDiagnosticCode};
pub use ty::{
    BoundKind, Ty, TyData, TyDisplay, TyKind, WildcardBound, capture_conversion, ty_from_source,
};
