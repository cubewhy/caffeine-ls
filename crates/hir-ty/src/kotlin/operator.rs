//! Kotlin's operator conventions and the built-in arithmetic that precedes them.
//!
//! Every Kotlin operator is a call of a named function
//! ([KLS
//! `operator-overloading.html`](https://kotlinlang.org/spec/operator-overloading.html)
//! gives the convention each operator resolves): `a + b` is `a.plus(b)`, `a < b`
//! is `a.compareTo(b) < 0` and `a[i]` is `a.get(i)`. The conventions are
//! resolved like any other call — through [`crate::kotlin::method`]'s member set
//! and its selection — so an operator a class declares is a member of it, and an
//! operator the *library* declares is a member of the classfile receiver.
//!
//! The built-in types have their own rules, which win over a declared operator
//! ([KLS
//! `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)):
//! two numeric operands give the *wider* of the two types, `String` `plus`
//! anything is a `String`, and the comparisons and the logical operators are
//! always `Boolean`. This module owns the table and the result types; the walk
//! that uses them is [`crate::kotlin::infer`].

use hir_expand::body::{BinaryOp, UnaryOp};

use crate::jvm::db::TyDatabase;
use crate::ty::{Ty, TyKind};

/// The convention function a binary operator is spelled as
/// ([KLS
/// `operator-overloading.html#binary-operations`](https://kotlinlang.org/spec/operator-overloading.html#binary-operations)),
/// or `None` for an operator whose result is always a `Boolean` and which
/// therefore needs no call (`==`, `!=`, `&&`, `||`).
pub fn binary_convention(op: BinaryOp) -> Option<&'static str> {
    match op {
        BinaryOp::Add => Some("plus"),
        BinaryOp::Sub => Some("minus"),
        BinaryOp::Mul => Some("times"),
        BinaryOp::Div => Some("div"),
        BinaryOp::Rem => Some("rem"),
        // `a < b` is `a.compareTo(b) < 0`: the convention's result is the
        // comparison's sign, and the operator's own type is `Boolean`.
        BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => Some("compareTo"),
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::And | BinaryOp::Or => None,
        // The bit operators are Java's; Kotlin spells them as named infix
        // functions (`shl`, `and`) and reaches them through the convention
        // table's *names* like any other function.
        BinaryOp::Shl
        | BinaryOp::Shr
        | BinaryOp::UShr
        | BinaryOp::BitAnd
        | BinaryOp::BitXor
        | BinaryOp::BitOr => None,
    }
}

/// The convention function a unary operator is spelled as
/// ([KLS
/// `operator-overloading.html#unary-operations`](https://kotlinlang.org/spec/operator-overloading.html#unary-operations)).
/// `!` is not one: it is `Boolean` negation, which no declaration takes part in.
pub fn unary_convention(op: UnaryOp) -> Option<&'static str> {
    match op {
        UnaryOp::Plus => Some("unaryPlus"),
        UnaryOp::Minus => Some("unaryMinus"),
        UnaryOp::Not => Some("not"),
        UnaryOp::Inc => Some("inc"),
        UnaryOp::Dec => Some("dec"),
        UnaryOp::BitNot => None,
    }
}

/// The convention function an *augmented* assignment is spelled as
/// ([KLS
/// `operator-overloading.html#augmented-assignments`](https://kotlinlang.org/spec/operator-overloading.html#augmented-assignments)):
/// `a += b` is `a.plusAssign(b)` where the left operand declares one, and a
/// plain reassignment of `a.plus(b)` where it does not — which is the caller's
/// question, so only the *name* is answered here.
pub fn assign_convention(op: hir_expand::body::AssignOp) -> Option<&'static str> {
    match op {
        hir_expand::body::AssignOp::Assign => None,
        hir_expand::body::AssignOp::Add => Some("plusAssign"),
        hir_expand::body::AssignOp::Sub => Some("minusAssign"),
        hir_expand::body::AssignOp::Mul => Some("timesAssign"),
        hir_expand::body::AssignOp::Div => Some("divAssign"),
        hir_expand::body::AssignOp::Rem => Some("remAssign"),
        // Java's compound bit operators have no Kotlin spelling.
        hir_expand::body::AssignOp::Shl
        | hir_expand::body::AssignOp::Shr
        | hir_expand::body::AssignOp::UShr
        | hir_expand::body::AssignOp::BitAnd
        | hir_expand::body::AssignOp::BitXor
        | hir_expand::body::AssignOp::BitOr => None,
    }
}

/// The result type Kotlin's *built-in* rules give `lhs op rhs`, or `None` when
/// they do not apply and the operator's own convention decides
/// ([KLS
/// `built-in-types-and-their-semantics.html#built-in-integer-arithmetic-operators`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html#built-in-integer-arithmetic-operators),
/// [`#built-in-floating-point-arithmetic-operators`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html#built-in-floating-point-arithmetic-operators)
/// and the `Char`/`String` rules of the same section).
///
/// Every rule here is one kotlinc 2.4.20 applies without consulting a
/// declaration: `1 + 2` is an `Int` whatever `kotlin.Int` declares, `1L + 2` is
/// a `Long`, `"a" + 1` is a `String`, and `a < b` is a `Boolean`.
pub fn builtin_binary_ty(db: &dyn TyDatabase, op: BinaryOp, lhs: &Ty, rhs: &Ty) -> Option<Ty> {
    // The comparisons, the equality operators and the logical ones are
    // `Boolean` for every pair of operands ([KLS
    // `built-in-types-and-their-semantics.html#built-in-comparison-operators`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html#built-in-comparison-operators),
    // [`#built-in-equality-operator`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html#built-in-equality-operator),
    // [`#built-in-boolean-operators`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html#built-in-boolean-operators)).
    if matches!(
        op,
        BinaryOp::Lt
            | BinaryOp::Gt
            | BinaryOp::Le
            | BinaryOp::Ge
            | BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::And
            | BinaryOp::Or
    ) {
        return Some(Ty::reference(db, "kotlin.Boolean", Vec::new()));
    }
    // An operand this model could not resolve determines nothing: the other
    // operand's type is what the expression is, exactly as the error type
    // absorbs in [`crate::kotlin::subtyping`].
    let error = |ty: &Ty| matches!(ty.kind(db), TyKind::Error);
    if error(lhs) {
        return Some(*rhs);
    }
    if error(rhs) {
        return Some(*lhs);
    }
    // A *platform* type is its lower half's classifier with a nullability
    // attribute, and `?` changes nothing here: `"a" + b` is a `String` whether
    // the left operand came from Kotlin source or from a classfile signature.
    let name = |ty: &Ty| {
        let mut ty = *ty;
        loop {
            match ty.kind(db) {
                TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => ty = *inner,
                TyKind::Flexible { lower, .. } => ty = *lower,
                _ => break,
            }
        }
        match ty.kind(db) {
            TyKind::Reference { name, .. } => Some(super::ty::mapped_type_name(name)),
            _ => None,
        }
    };
    let lhs_name = name(lhs)?;
    let rhs_name = name(rhs)?;
    let lhs_name = lhs_name.as_str();
    let rhs_name = rhs_name.as_str();
    // `String.plus(other: Any?): String` — the left operand alone decides, and
    // it takes any right operand.
    if lhs_name == "kotlin.String" && op == BinaryOp::Add {
        return Some(Ty::reference(db, "kotlin.String", Vec::new()));
    }
    // `Char.plus(Int): Char`, `Char.minus(Int): Char`, `Int.plus(Char): Char`,
    // `Int.minus(Char): Char` and `Char.minus(Char): Int`.
    let integer = |name: &str| {
        matches!(
            name,
            "kotlin.Byte" | "kotlin.Short" | "kotlin.Int" | "kotlin.Long"
        )
    };
    if op == BinaryOp::Add || op == BinaryOp::Sub {
        if lhs_name == "kotlin.Char" && rhs_name == "kotlin.Char" {
            return Some(Ty::reference(db, "kotlin.Int", Vec::new()));
        }
        if (lhs_name == "kotlin.Char" && integer(rhs_name))
            || (integer(lhs_name) && rhs_name == "kotlin.Char")
        {
            return Some(Ty::reference(db, "kotlin.Char", Vec::new()));
        }
    }
    // Two numeric operands give the wider type ([KLS
    // `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html):
    // `Byte`/`Short`/`Int` with each other are an `Int`, and `Long`, `Float` and
    // `Double` each widen what they are combined with).
    if !is_numeric(lhs_name) || !is_numeric(rhs_name) {
        return None;
    }
    let widened = match rank(lhs_name).max(rank(rhs_name)) {
        0 => "kotlin.Int",
        1 => "kotlin.Long",
        2 => "kotlin.Float",
        _ => "kotlin.Double",
    };
    Some(Ty::reference(db, widened, Vec::new()))
}

/// Whether a built-in classifier name is one of Kotlin's numeric types.
fn is_numeric(name: &str) -> bool {
    matches!(
        name,
        "kotlin.Byte"
            | "kotlin.Short"
            | "kotlin.Int"
            | "kotlin.Long"
            | "kotlin.Float"
            | "kotlin.Double"
    )
}

/// A numeric type's width, for the widening rule: `Byte`, `Short` and `Int`
/// combine to an `Int`, and `Long`, `Float` and `Double` each widen what they are
/// combined with ([KLS
/// `built-in-types-and-their-semantics.html#built-in-integer-arithmetic-operators`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html#built-in-integer-arithmetic-operators)).
fn rank(name: &str) -> u8 {
    match name {
        "kotlin.Long" => 1,
        "kotlin.Float" => 2,
        "kotlin.Double" => 3,
        _ => 0,
    }
}
