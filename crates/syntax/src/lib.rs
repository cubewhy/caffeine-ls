pub use java_syntax as java;
pub use kotlin_syntax as kotlin;

pub mod class_parser;
mod diagnostics;
mod language;
mod parse;
pub mod stub;

pub use diagnostics::{DiagnosticCode, JavaDiagnosticCode, KotlinDiagnosticCode};
pub use language::LanguageKind;
pub use parse::{Parse, SourceFile, SyntaxError};
