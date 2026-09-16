//! Shared helpers for resolving the item tree's source ranges on demand.
//!
//! The item tree stores no source offsets — every declaration anchors to its
//! syntax node through a [`FileAstId`](hir_expand::ast_id_map::FileAstId) —
//! so the diagnostics and the IDE resolve the source ranges they need from
//! the file's [`AstIdMap`] and current syntax tree
//! ([`crate::java::ranges`]). This module owns the one bit of database glue
//! all consumers share: fetching the memoized parse and map of a file and
//! guarding files whose language is not known (an `Unknown` file has no
//! parse, and range resolution for it is meaningless — an empty item tree).

use base_db::LanguageKind;
use hir_expand::ast_id_map::AstIdMap;
use syntax::SourceFile;
use vfs::FileId;

use crate::jvm::db::TyDatabase;

/// The `(map, source)` pair the range helpers resolve against, when the
/// file's language is known; `None` for an `Unknown` file (no parse exists).
pub fn range_ctx(
    db: &dyn TyDatabase,
    file: FileId,
    language: LanguageKind,
) -> Option<(&AstIdMap, SourceFile)> {
    if language == LanguageKind::Unknown {
        return None;
    }
    // Both reads are salsa-memoized; the map is keyed exactly like the parse.
    let parse = base_db::parse(db, file, language);
    let source = parse.syntax_node(language);
    let map = hir_def::db::ast_id_map(db, file, language);
    Some((map, source))
}
