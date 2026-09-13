//! The Kotlin type errors, with the compiler's wordings.
//!
//! Each variant carries the message kotlinc 2.4.20 reports for the same source
//! (captured with the probe oracle and the fixtures of the test suite), because
//! the wording is what a user compares against a compiler run: an *unresolved*
//! name is `unresolved reference 'x'.`, a type mismatch is
//! `initializer type mismatch: expected 'String', actual 'List<Int>'.`, a
//! non-null declaration initialized with `null` is `null cannot be a value of a
//! non-null type 'String'.`, and assigning to a `val` is `'val' cannot be
//! reassigned.`.
//!
//! A diagnostic the HIR reports but kotlinc *accepts* is a bug in the HIR; a
//! kotlinc error the HIR misses is a missing case. Both are recorded in the
//! test suite, which pins the wordings against the compiler.

use hir_expand::body::{ExprId, LocalId};
use hir_expand::name::Name;
use rowan::TextRange;

use crate::ty::Ty;

/// A type error of a Kotlin body.
#[derive(Debug, Clone, PartialEq)]
pub enum KotlinTypeError {
    /// KLS
    /// `scopes-and-identifiers.html#scopes-and-identifiers`](https://kotlinlang.org/spec/scopes-and-identifiers.html#scopes-and-identifiers):
    /// a simple name resolves to nothing — no local, no parameter, no member
    /// of the implicit receiver, no classifier. kotlinc: `unresolved reference
    /// '<name>'.`
    UnresolvedReference {
        expr: ExprId,
        name: Name,
        range: Option<TextRange>,
    },
    /// A value of one type initialized or assigned to a declaration of an
    /// incompatible one. kotlinc: `initializer type mismatch: expected '<T>',
    /// actual '<S>'.` for a declaration, `type mismatch: inferred type is
    /// '<S>' but '<T>' was expected.` for an assignment.
    TypeMismatch {
        /// The declaration the value is bound to (a local, a property or a
        /// parameter), for the range and the diagnostic's identity.
        target: MismatchTarget,
        expected: Ty,
        actual: Ty,
        range: Option<TextRange>,
    },
    /// `null` — or a nullable value — where a non-null type is required
    /// ([KLS
    /// `type-system.html#nullable-types`](https://kotlinlang.org/spec/type-system.html#nullable-types)).
    /// kotlinc: `null cannot be a value of a non-null type '<T>'.` for the
    /// literal, `type mismatch: inferred type is '<S>?' but '<T>' was
    /// expected.` for a value.
    NullabilityMismatch {
        target: MismatchTarget,
        expected: Ty,
        actual: Ty,
        range: Option<TextRange>,
    },
    /// An assignment to a `val` — a read-only property or local ([KLS
    /// `declarations.html#read-only-property-declaration`](https://kotlinlang.org/spec/declarations.html#read-only-property-declaration)).
    /// kotlinc: `'val' cannot be reassigned.`
    ValReassignment {
        expr: ExprId,
        name: Name,
        range: Option<TextRange>,
    },
}

/// What a mismatched value is bound to, for the diagnostic's range and text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchTarget {
    /// A local declaration's initializer.
    Local(LocalId),
    /// A property declaration's initializer.
    Property(hir_expand::ids::ItemId),
    /// An assignment's right-hand side.
    Assignment,
}

impl KotlinTypeError {
    /// The message kotlinc 2.4.20 reports for this error, as the type layer can
    /// spell it (`<type>` is the type's own display).
    pub fn message(&self, db: &dyn crate::java::db::TyDatabase) -> String {
        use crate::kotlin::ty::display_kotlin;
        let display = |ty: &Ty| display_kotlin(db, *ty).to_string();
        match self {
            KotlinTypeError::UnresolvedReference { name, .. } => {
                format!("unresolved reference '{}'.", name.as_str())
            }
            KotlinTypeError::TypeMismatch {
                expected, actual, ..
            } => format!(
                "initializer type mismatch: expected '{}', actual '{}'.",
                display(expected),
                display(actual)
            ),
            KotlinTypeError::NullabilityMismatch {
                expected, actual, ..
            } => {
                if matches!(actual.kind(db), crate::ty::TyKind::Null) {
                    format!(
                        "null cannot be a value of a non-null type '{}'.",
                        display(expected)
                    )
                } else {
                    format!(
                        "type mismatch: inferred type is '{}' but '{}' was expected.",
                        display(actual),
                        display(expected)
                    )
                }
            }
            KotlinTypeError::ValReassignment { .. } => "'val' cannot be reassigned.".to_owned(),
        }
    }
}

impl KotlinTypeError {
    /// The stable diagnostic code of the error, so a client can key on it
    /// independently of the message's wording ([`KotlinDiagnosticCode`]).
    ///
    /// [`KotlinDiagnosticCode`]: syntax::KotlinDiagnosticCode
    pub fn code(&self) -> syntax::KotlinDiagnosticCode {
        match self {
            KotlinTypeError::UnresolvedReference { .. } => {
                syntax::KotlinDiagnosticCode::UnresolvedReference
            }
            KotlinTypeError::TypeMismatch { .. } => syntax::KotlinDiagnosticCode::TypeMismatch,
            KotlinTypeError::NullabilityMismatch { .. } => {
                syntax::KotlinDiagnosticCode::NullabilityMismatch
            }
            KotlinTypeError::ValReassignment { .. } => {
                syntax::KotlinDiagnosticCode::ValReassignment
            }
        }
    }

    /// The source range the error underlines: the value expression for a
    /// mismatch, the name for an unresolved reference or a `val` assignment.
    pub fn range(&self) -> Option<TextRange> {
        match self {
            KotlinTypeError::UnresolvedReference { range, .. }
            | KotlinTypeError::TypeMismatch { range, .. }
            | KotlinTypeError::NullabilityMismatch { range, .. }
            | KotlinTypeError::ValReassignment { range, .. } => *range,
        }
    }
}
