pub(crate) mod ast;
pub(crate) mod class_parser;
pub(crate) mod java_parser;
pub(crate) mod kotlin_parser;

use std::fmt;

pub use ast::*;

use lasso::ThreadedRodeo;
use rowan::{GreenNode, TextRange};

use crate::{java_parser::parse_java_file, kotlin_parser::parse_kotlin_file};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageId {
    Java,
    Kotlin,
}

impl LanguageId {
    pub fn from_ext(ext: &str) -> Option<Self> {
        match ext {
            "java" => Some(Self::Java),
            "kt" | "kts" => Some(Self::Kotlin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntaxError {
    /// The exact byte range in the source text where the error occurred.
    pub range: TextRange,
    /// The human-readable error message (e.g., "Expected ';'", "Unclosed string literal").
    pub message: String,
}

impl SyntaxError {
    pub fn new(message: impl Into<String>, range: TextRange) -> Self {
        Self {
            message: message.into(),
            range,
        }
    }

    /// Helper to create a new error at a specific byte offset (zero-width range).
    /// Useful for "Expected X here" errors.
    pub fn new_at_offset(message: impl Into<String>, offset: rowan::TextSize) -> Self {
        Self {
            message: message.into(),
            range: TextRange::empty(offset),
        }
    }
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error at {:?}: {}", self.range, self.message)
    }
}

pub struct ParseResult {
    pub tree: GreenNode,
    pub errors: Vec<SyntaxError>,

    pub stubs: Vec<ClassStub>,
}

pub fn parse_file(language: LanguageId, text: &str, interner: &ThreadedRodeo) -> ParseResult {
    match language {
        LanguageId::Java => parse_java_file(text, interner),
        LanguageId::Kotlin => parse_kotlin_file(text, interner),
    }
}
