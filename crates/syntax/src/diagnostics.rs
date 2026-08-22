//! Language-tagged diagnostic codes shared across the parser layer
//! ([crate::java], [`crate::kotlin`]) and the type layer (`hir-ty`), so the
//! IDE/LSP layer can attach a stable `code` to every diagnostic it surfaces
//! and key code actions off it (the LSP `Diagnostic.code` field and
//! `CodeAction.data`).
//!
//! Codes are kebab-case strings scoped per language (each document is a
//! single language, so no language prefix is needed), distinct from the
//! human-readable message.

/// The code of a diagnostic, tagged by language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticCode {
    /// A Java diagnostic.
    Java(JavaDiagnosticCode),
    /// A Kotlin diagnostic. The enum is currently empty; Kotlin diagnostics
    /// carry no code today.
    Kotlin(KotlinDiagnosticCode),
}

/// Java diagnostic codes, spanning the lexer, the parser and the type layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JavaDiagnosticCode {
    // Type layer (`hir-ty`).
    /// §14.4.1: a `var` declaration must have an initializer.
    VarWithoutInitializer,
    /// §6.5: a simple name resolves to nothing.
    CannotResolveName,
    /// §15.11: no field of the name on the receiver.
    NoSuchField,
    /// §15.12.1: no method of the name on the receiver.
    NoSuchMethod,
    /// §15.12.2: no member is applicable to the actual arguments.
    WrongArity,
    /// §14.18: a `throw` operand is not assignable to `Throwable`; also used
    /// for a declaration initializer, an assignment or a returned expression
    /// not assignable to its target ([§5.2], [§14.17]).
    IncompatibleTypes,
    /// §14.9/§14.11/§14.12.1/§14.16/§15.25: a condition position requires a
    /// `boolean`.
    NonBooleanCondition,
    /// §15.15/§15.17/§15.18/§15.22: a numeric operand of a unary, binary or
    /// shift operator is not numeric.
    IncompatibleOperand,
    /// §15.21/§15.20: the operands of an equality or relational operator are
    /// not comparable.
    IncomparableTypes,
    /// §14.14.2: the expression of a for-each loop is not an array or
    /// `Iterable`.
    NonIterableForEach,
    /// §5.5/§15.16: a cast is not a casting conversion.
    BadCast,
    /// §15.10: generic array creation (`new List<String>[3]`).
    GenericArrayCreation,
    /// §15.9: instantiating a type variable, interface, abstract class or enum
    /// with `new`.
    CannotInstantiateTypeVar,
    /// §14.4: a `var` initializer cannot be an array initializer.
    VarArrayInitializer,
    // Lexical ([JLS §3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html)).
    UnexpectedChar,
    UnterminatedString,
    UnterminatedComment,
    InvalidChar,
    IllegalTextBlockOpen,
    UnterminatedTextBlock,
    InvalidNumber,
    InvalidUnicodeEscape,
    UnterminatedChar,
    InvalidEscapeSequence,
    UnterminatedTemplate,
    // Parse.
    ExpectedToken,
    ExpectedKeyword,
    ExpectedConstruct,
    NotAStatement,
}

/// Kotlin diagnostic codes — none defined yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KotlinDiagnosticCode {}

impl DiagnosticCode {
    /// The stable machine-readable code string, without a language prefix.
    pub fn as_str(&self) -> &'static str {
        match self {
            DiagnosticCode::Java(code) => code.as_str(),
            DiagnosticCode::Kotlin(code) => match *code {},
        }
    }
}

impl JavaDiagnosticCode {
    /// The stable machine-readable code string; kept `'static` so it can be
    /// cheaply embedded in an LSP `code`/code-action `data` payload.
    pub fn as_str(&self) -> &'static str {
        match self {
            JavaDiagnosticCode::VarWithoutInitializer => "var-without-initializer",
            JavaDiagnosticCode::CannotResolveName => "cannot-resolve-symbol",
            JavaDiagnosticCode::NoSuchField => "no-such-field",
            JavaDiagnosticCode::NoSuchMethod => "no-such-method",
            JavaDiagnosticCode::WrongArity => "wrong-argument-count",
            JavaDiagnosticCode::IncompatibleTypes => "incompatible-types",
            JavaDiagnosticCode::NonBooleanCondition => "non-boolean-condition",
            JavaDiagnosticCode::IncompatibleOperand => "incompatible-operand",
            JavaDiagnosticCode::IncomparableTypes => "incomparable-types",
            JavaDiagnosticCode::NonIterableForEach => "non-iterable-for-each",
            JavaDiagnosticCode::BadCast => "bad-cast",
            JavaDiagnosticCode::GenericArrayCreation => "generic-array-creation",
            JavaDiagnosticCode::CannotInstantiateTypeVar => "cannot-instantiate-type-var",
            JavaDiagnosticCode::VarArrayInitializer => "var-array-initializer",
            JavaDiagnosticCode::UnexpectedChar => "unexpected-char",
            JavaDiagnosticCode::UnterminatedString => "unterminated-string",
            JavaDiagnosticCode::UnterminatedComment => "unterminated-comment",
            JavaDiagnosticCode::InvalidChar => "invalid-char",
            JavaDiagnosticCode::IllegalTextBlockOpen => "illegal-text-block-open",
            JavaDiagnosticCode::UnterminatedTextBlock => "unterminated-text-block",
            JavaDiagnosticCode::InvalidNumber => "invalid-number",
            JavaDiagnosticCode::InvalidUnicodeEscape => "invalid-unicode-escape",
            JavaDiagnosticCode::UnterminatedChar => "unterminated-char",
            JavaDiagnosticCode::InvalidEscapeSequence => "invalid-escape-sequence",
            JavaDiagnosticCode::UnterminatedTemplate => "unterminated-template",
            JavaDiagnosticCode::ExpectedToken => "expected-token",
            JavaDiagnosticCode::ExpectedKeyword => "expected-keyword",
            JavaDiagnosticCode::ExpectedConstruct => "expected-construct",
            JavaDiagnosticCode::NotAStatement => "not-a-statement",
        }
    }
}

impl std::fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for JavaDiagnosticCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl DiagnosticCode {
    /// The Java code for a lexical error kind, when the kind is structured
    /// enough to carry one (free-text `Message` kinds have none).
    pub(crate) fn from_java_syntax(kind: &crate::java::SyntaxErrorKind) -> Option<DiagnosticCode> {
        use crate::java::{LexicalErrorKind, ParseErrorKind};
        let code = match kind {
            crate::java::SyntaxErrorKind::Lexer(kind) => match kind {
                LexicalErrorKind::UnexpectedChar(_) => JavaDiagnosticCode::UnexpectedChar,
                LexicalErrorKind::UnterminatedString => JavaDiagnosticCode::UnterminatedString,
                LexicalErrorKind::UnterminatedComment => JavaDiagnosticCode::UnterminatedComment,
                LexicalErrorKind::InvalidChar => JavaDiagnosticCode::InvalidChar,
                LexicalErrorKind::IllegalTextBlockOpen => JavaDiagnosticCode::IllegalTextBlockOpen,
                LexicalErrorKind::UnterminatedTextBlock => {
                    JavaDiagnosticCode::UnterminatedTextBlock
                }
                LexicalErrorKind::InvalidNumber => JavaDiagnosticCode::InvalidNumber,
                LexicalErrorKind::InvalidUnicodeEscape => JavaDiagnosticCode::InvalidUnicodeEscape,
                LexicalErrorKind::UnterminatedChar => JavaDiagnosticCode::UnterminatedChar,
                LexicalErrorKind::InvalidEscapeSequence => {
                    JavaDiagnosticCode::InvalidEscapeSequence
                }
                LexicalErrorKind::UnterminatedTemplate => JavaDiagnosticCode::UnterminatedTemplate,
            },
            crate::java::SyntaxErrorKind::Parser(kind) => match kind {
                ParseErrorKind::ExpectedToken { .. } => JavaDiagnosticCode::ExpectedToken,
                ParseErrorKind::ExpectedContextualKeyword { .. } => {
                    JavaDiagnosticCode::ExpectedKeyword
                }
                ParseErrorKind::ExpectedConstruct(_) => JavaDiagnosticCode::ExpectedConstruct,
                ParseErrorKind::NotAStatement => JavaDiagnosticCode::NotAStatement,
                ParseErrorKind::Message(_) => return None,
            },
        };
        Some(DiagnosticCode::Java(code))
    }

    pub(crate) fn from_kotlin_syntax(
        _kind: &crate::kotlin::SyntaxErrorKind,
    ) -> Option<DiagnosticCode> {
        // TODO: add kotlin syntax code
        None
    }
}
