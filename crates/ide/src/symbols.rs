//! Source symbol lookups over the HIR: per-file document symbols and
//! workspace-wide symbol search, plus direct access to the HIR type layer.
//!
//! These are the IDE-facing wrappers over the classpath-scoped source symbol
//! index of `hir` (see [`hir::source_set_symbols`]). All queries are salsa
//! memoized; the `ide::Analysis` methods below funnel them through the
//! cancellation boundary.

use rowan::TextRange;
use triomphe::Arc;
use vfs::FileId;

use crate::RootDatabase;

/// A source symbol as seen by the IDE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSymbol {
    /// The canonical qualified name ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)).
    pub name: String,
    pub kind: hir::SourceSymbolKind,
    /// The source range of the declaration.
    pub range: TextRange,
    /// The lowered item, for later HIR queries (e.g. [`crate::Analysis::item_ty`]).
    pub item: hir_expand::item_tree::ItemId,
}

/// A document symbol plus the file it lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSymbol {
    pub file: FileId,
    pub symbol: DocumentSymbol,
}

/// The declared symbols of a file, in declaration order.
pub fn document_symbols(db: &RootDatabase, file_id: FileId) -> Vec<DocumentSymbol> {
    hir::file_symbols(db, file_id)
        .iter()
        .map(|symbol| DocumentSymbol {
            name: symbol.name.as_str().to_owned(),
            kind: symbol.kind,
            range: symbol.range,
            item: symbol.item,
        })
        .collect()
}

/// Symbols whose simple name matches `query` (case-insensitive substring,
/// prefix-preferred) across every registered source set, sorted by
/// (name, file, item) for determinism. An empty query returns everything.
pub fn workspace_symbols(db: &RootDatabase, query: &str) -> Vec<WorkspaceSymbol> {
    let mut out = Vec::new();
    for source_set in registered_source_sets(db) {
        let index = hir::source_set_symbols(db, source_set);
        if query.trim().is_empty() {
            out.extend(index.iter().cloned());
        } else {
            out.extend(index.lookup_simple(query));
            out.extend(index.lookup_substring(query));
        }
    }
    out.sort_by_key(|reference| {
        (
            reference.symbol.name.as_str().to_owned(),
            reference.file,
            reference.symbol.item,
        )
    });
    // Prefix and substring lookups overlap on prefix matches.
    out.dedup();
    out.into_iter()
        .map(|reference| WorkspaceSymbol {
            file: reference.file,
            symbol: DocumentSymbol {
                name: reference.symbol.name.as_str().to_owned(),
                kind: reference.symbol.kind,
                range: reference.symbol.range,
                item: reference.symbol.item,
            },
        })
        .collect()
}

/// The registered source sets of the project graph, in unspecified order.
fn registered_source_sets(db: &RootDatabase) -> Vec<hir::SourceSetId> {
    hir::project_graph(db)
        .map(|graph| graph.source_sets(db).keys().cloned().collect())
        .unwrap_or_default()
}

/// The declared type of an item — a field's type, a method's return type, or
/// the type of a class-like declaration — rendered from the HIR type layer.
pub fn item_ty(db: &RootDatabase, file_id: FileId, item: hir_expand::item_tree::ItemId) -> String {
    hir_ty::item_ty(db, file_id, item).display(db).to_string()
}

/// The parameter types of a method or constructor, in declaration order,
/// rendered from the HIR type layer.
pub fn method_params(
    db: &RootDatabase,
    file_id: FileId,
    item: hir_expand::item_tree::ItemId,
) -> Arc<Vec<String>> {
    Arc::new(
        hir_ty::method_params(db, file_id, item)
            .into_iter()
            .map(|ty| ty.display(db).to_string())
            .collect(),
    )
}
