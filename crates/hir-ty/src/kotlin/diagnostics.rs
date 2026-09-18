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
    /// A written argument that is not assignable to the parameter it lands on
    /// ([KLS
    /// `overload-resolution.html#determining-function-applicability-for-a-specific-call`](https://kotlinlang.org/spec/overload-resolution.html#determining-function-applicability-for-a-specific-call)).
    /// kotlinc: `argument type mismatch: actual type is '<S>', but '<T>' was
    /// expected.`
    ArgumentMismatch {
        parameter: Ty,
        actual: Ty,
        range: Option<TextRange>,
    },
    /// A call that passes no value for a parameter without a default ([KLS
    /// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
    /// kotlinc: `no value passed for parameter '<name>'.` — the parameter's own
    /// name where the declaration carries one, and `p0`, `p1`, … otherwise
    /// (which is what a classfile without a `MethodParameters` attribute gives).
    MissingArgument {
        parameter: Name,
        range: Option<TextRange>,
    },
    /// A `when` whose value is used and whose subject needs an `else` ([KLS
    /// `expressions.html#when-expressions`](https://kotlinlang.org/spec/expressions.html#when-expressions)).
    /// kotlinc: `'when' expression must be exhaustive. Add an 'else' branch.`
    NonExhaustiveWhen { range: Option<TextRange> },
    /// A `when` arm's condition that is not a `Boolean` — the subject-less form
    /// tests each condition itself ([KLS
    /// `expressions.html#when-expressions`](https://kotlinlang.org/spec/expressions.html#when-expressions)).
    /// kotlinc 2.4.20 words the two shapes differently: a condition that *has* a
    /// type is `condition type mismatch: inferred type is 'Int' but 'Boolean'
    /// was expected.`, while a *type test* with no subject — which tests
    /// nothing, and so has no type — is `condition of type 'Boolean'
    /// expected.`
    NonBooleanWhenCondition {
        range: Option<TextRange>,
        /// The condition's own type, for the first wording; `None` for a type
        /// test with no subject.
        actual: Option<Ty>,
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
    /// A `return`'s value ([KLS
    /// `expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions)),
    /// which answers the return type the enclosing declaration writes. kotlinc
    /// words it as an assignment does: `type mismatch: inferred type is '<S>'
    /// but '<T>' was expected.`
    Return,
}

impl KotlinTypeError {
    /// The message kotlinc 2.4.20 reports for this error, as the type layer can
    /// spell it (`<type>` is the type's own display).
    pub fn message(&self, db: &dyn crate::jvm::db::TyDatabase) -> String {
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
            KotlinTypeError::ArgumentMismatch {
                parameter, actual, ..
            } => format!(
                "argument type mismatch: actual type is '{}', but '{}' was expected.",
                display(actual),
                display(parameter)
            ),
            KotlinTypeError::MissingArgument { parameter, .. } => {
                format!("no value passed for parameter '{}'.", parameter.as_str())
            }
            KotlinTypeError::NonExhaustiveWhen { .. } => {
                "'when' expression must be exhaustive. Add an 'else' branch.".to_owned()
            }
            KotlinTypeError::NonBooleanWhenCondition { actual, .. } => match actual {
                Some(actual) => format!(
                    "condition type mismatch: inferred type is '{}' but 'Boolean' was expected.",
                    display(actual)
                ),
                None => "condition of type 'Boolean' expected.".to_owned(),
            },
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
            KotlinTypeError::ArgumentMismatch { .. } => {
                syntax::KotlinDiagnosticCode::ArgumentMismatch
            }
            KotlinTypeError::MissingArgument { .. } => {
                syntax::KotlinDiagnosticCode::MissingArgument
            }
            KotlinTypeError::NonExhaustiveWhen { .. } => {
                syntax::KotlinDiagnosticCode::NonExhaustiveWhen
            }
            KotlinTypeError::NonBooleanWhenCondition { .. } => {
                syntax::KotlinDiagnosticCode::NonBooleanWhenCondition
            }
        }
    }

    /// The source range the error underlines: the value expression for a
    /// mismatch, the name for an unresolved reference or a `val` assignment.
    pub fn range(&self) -> Option<TextRange> {
        match self {
            KotlinTypeError::UnresolvedReference { range, .. }
            | KotlinTypeError::ArgumentMismatch { range, .. }
            | KotlinTypeError::MissingArgument { range, .. }
            | KotlinTypeError::NonExhaustiveWhen { range }
            | KotlinTypeError::NonBooleanWhenCondition { range, .. }
            | KotlinTypeError::TypeMismatch { range, .. }
            | KotlinTypeError::NullabilityMismatch { range, .. }
            | KotlinTypeError::ValReassignment { range, .. } => *range,
        }
    }
}
