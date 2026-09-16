//! The language-neutral snapshot surface of `hir-def`.
//!
//! `hir-def`'s tests render a lowered file as a stable, human-readable text so
//! a lowering change shows up as a reviewable diff. The rendering of a
//! *language's own* declaration model lives with that language
//! ([`crate::java::pretty`], [`crate::kotlin::pretty`]); this module renders
//! whichever model the facade holds.

use hir_expand::ast_id_map::AstIdMap;
use syntax::SourceFile;

use crate::item_tree::LoweredFile;

/// The stable, human-readable rendering of a lowered file: its declaration
/// model plus the source ranges resolved from the current syntax tree.
pub fn pretty_print(lowered: &LoweredFile, map: &AstIdMap, source: &SourceFile) -> String {
    let items = &lowered.items;
    if let Some(tree) = crate::java::plugin::model(items) {
        return crate::java::pretty::pretty_print(&tree, map, source);
    }
    if let Some(tree) = crate::kotlin::plugin::model(items) {
        return crate::kotlin::pretty::pretty_print(&tree, map, source);
    }
    format!("file ({})\n", items.language().name())
}

/// The stable, human-readable rendering of a lowered file's bodies.
pub fn pretty_body(lowered: &LoweredFile) -> String {
    let items = &lowered.items;
    if let Some(tree) = crate::java::plugin::model(items) {
        return crate::java::pretty::pretty_body(&tree, &lowered.bodies);
    }
    if let Some(tree) = crate::kotlin::plugin::model(items) {
        return crate::kotlin::pretty::pretty_body(&tree, &lowered.bodies);
    }
    String::new()
}
