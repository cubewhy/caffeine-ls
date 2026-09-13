//! The language-neutral snapshot surface of `hir-def`.
//!
//! `hir-def`'s tests render a lowered file as a stable, human-readable text so
//! a lowering change shows up as a reviewable diff. The rendering of a
//! *language's own* declaration model lives with that language
//! ([`crate::java::pretty`]); this module dispatches on what the file actually
//! lowered to, so a snapshot can be taken of any file.

use hir_expand::ast_id_map::AstIdMap;
use syntax::SourceFile;

use crate::item_tree::{FileItemTree, LoweredFile, language_name};

/// The stable, human-readable rendering of a lowered file: its declaration
/// model plus the source ranges resolved from the current syntax tree.
pub fn pretty_print(lowered: &LoweredFile, map: &AstIdMap, source: &SourceFile) -> String {
    match &lowered.items {
        FileItemTree::Java(tree) => crate::java::pretty::pretty_print(tree, map, source),
        FileItemTree::Kotlin(tree) => crate::kotlin::pretty::pretty_print(tree, map, source),
        FileItemTree::Empty(language) => format!("file ({})\n", language_name(*language)),
    }
}

/// The stable, human-readable rendering of a lowered file's bodies.
pub fn pretty_body(lowered: &LoweredFile) -> String {
    match &lowered.items {
        FileItemTree::Java(tree) => crate::java::pretty::pretty_body(tree, &lowered.bodies),
        FileItemTree::Kotlin(tree) => crate::kotlin::pretty::pretty_body(tree, &lowered.bodies),
        FileItemTree::Empty(_) => String::new(),
    }
}
