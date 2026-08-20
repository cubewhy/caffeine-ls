//! Type errors reported during body inference.
//!
//! The inference layer degrades broken source to [`crate::Ty::error`] so
//! downstream resolution never panics; every such degradation that is a
//! *language* error (not a compiler invariant) is additionally reported here
//! as a structured [`TypeError`] with a typed [`DiagnosticCode`] and a
//! human-readable `message`, keyed to the body IR it occurred in. The
//! diagnostics layer can collect them per file by running
//! [`crate::body_types`] for each body-carrying item and flattening the
//! [`crate::BodyTypes::diagnostics`].

use hir_expand::body::{BodyTree, ExprId, LocalId};
use rowan::TextRange;
use syntax::{DiagnosticCode, JavaDiagnosticCode};

/// Where a reported type error occurred, in the currency of the body IR: the
/// stable-per-file arena ids the inference layer works with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagLocation {
    /// An expression of the enclosing body.
    Expr(ExprId),
    /// A local (parameter or declared local) of the enclosing body.
    Local(LocalId),
}

impl DiagLocation {
    /// The source range of the location within its file, when the construct
    /// was lowered from a syntax node (synthetic `Missing` constructs have
    /// none). Offsets are file-relative; convert to line/column with the
    /// diagnostics layer's `LineIndex`.
    pub fn range(&self, tree: &BodyTree) -> Option<TextRange> {
        match self {
            DiagLocation::Expr(id) => tree.expr_range(*id),
            DiagLocation::Local(id) => tree.local_range(*id),
        }
    }
}

/// A type error reported during body inference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    /// §14.4.1: a `var` declaration must have an initializer — the initializer
    /// is what the local's type is inferred from, so without one the local
    /// has no type.
    VarWithoutInitializer { local: LocalId },
}

impl TypeError {
    /// The typed code of this error ([`DiagnosticCode`]).
    pub fn code(&self) -> DiagnosticCode {
        match self {
            TypeError::VarWithoutInitializer { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::VarWithoutInitializer)
            }
        }
    }

    /// The location of the error within its body.
    pub fn location(&self) -> DiagLocation {
        match self {
            TypeError::VarWithoutInitializer { local } => DiagLocation::Local(*local),
        }
    }

    /// The source range of the error within its file, when the construct was
    /// lowered from a syntax node.
    pub fn range(&self, tree: &BodyTree) -> Option<TextRange> {
        self.location().range(tree)
    }

    /// The human-readable message, rendered against the body IR the error
    /// occurred in (for the local's name).
    pub fn message(&self, tree: &BodyTree) -> String {
        match self {
            TypeError::VarWithoutInitializer { local } => {
                let name = tree.local(*local).name.as_str();
                format!("variable '{name}' is declared with 'var' but has no initializer")
            }
        }
    }
}
