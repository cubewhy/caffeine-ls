//! The JVM type substrate of `hir-ty`: language-agnostic helpers shared by
//! every language's type layer.
//!
//! This namespace must stay free of Java (or Kotlin) syntax concepts: it
//! hosts the primitive-type naming, boxing and numeric-promotion tables
//! ([JLS §5.1.7], [§5.1.8], [§5.6.2]) over the shared
//! [`syntax::stub::PrimitiveType`] ([`ty`]), the salsa database trait and its
//! interned keys ([`db`]), the member vocabulary every language's type layer
//! speaks ([`member`]) and the classfile-shaped member set built on it
//! ([`member_set`]). Language-specific resolution and inference live in
//! [`crate::java`]; [`crate::kotlin`] builds its own type layer on top of this
//! substrate.

pub mod db;
pub mod member;
pub mod member_set;
pub mod ty;
