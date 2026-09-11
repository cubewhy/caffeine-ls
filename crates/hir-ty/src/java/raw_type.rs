//! Raw-type detection ([JLS §4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)).
//!
//! A raw type is the *written* form of a generic class without its type
//! arguments. Detection lives here, in the type layer; the warning it feeds
//! (its lint key, its suppression and its message) is owned by
//! `ide-diagnostics`.

use crate::java::db::TyDatabase;
use crate::java::ty::{Ty, TyKind};

/// Whether `ty` is a *raw* use of a generic class
/// ([JLS §4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8),
/// [§4.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.2)):
/// a reference type written without type arguments whose class declares type
/// parameters. A non-generic class (`String`) and a parameterized use
/// (`List<String>`) are not raw.
///
/// The check is on the *written* form: `List<String>`'s erasure is also named
/// `List` with no arguments, so this must be asked of the reference as it
/// appears in source (or in a classfile `Signature`), never of an erased type.
pub fn is_raw_reference(db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: &Ty) -> bool {
    let TyKind::Reference { name, args } = ty.kind(db) else {
        return false;
    };
    args.is_empty() && !ty.is_error(db) && crate::java::resolve::class_is_generic(db, scope, name)
}
