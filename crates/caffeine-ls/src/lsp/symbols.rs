//! Conversion of HIR source symbols into the LSP wire format.
//!
//! The HIR index is a flat, name-keyed list of declarations (types and
//! members); the LSP `textDocument/documentSymbol` response wants a hierarchy.
//! Members carry the canonical qualified name
//! ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7))
//! `EnclosingFqn.simple`, so the parent of a symbol is simply the longest
//! already-indexed name prefix — nesting is a pure name join, no extra
//! traversal.

use ide::{
    DocumentSymbol as IdeDocumentSymbol, WorkspaceSymbolSummary as IdeWorkspaceSymbolSummary,
};
use lsp_types::{
    DocumentSymbol as LspDocumentSymbol, Location, LocationUriOnly, SymbolKind, WorkspaceSymbol,
    WorkspaceSymbolLocation,
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::line_index::LineIndex;

use super::to_proto;

/// Maps an HIR symbol kind to the closest LSP [`SymbolKind`]. Records and
/// annotation types have no direct LSP kind; they map to `Struct` and
/// `Interface` respectively — and Kotlin's `object`, which declares a class
/// with a single instance, to `Class`.
pub(crate) fn symbol_kind(kind: ide::SourceSymbolKind) -> SymbolKind {
    use ide::SourceSymbolKind as Kind;
    match kind {
        Kind::Class => SymbolKind::Class,
        Kind::Interface => SymbolKind::Interface,
        Kind::Enum => SymbolKind::Enum,
        Kind::Record => SymbolKind::Struct,
        Kind::Annotation => SymbolKind::Interface,
        Kind::Module => SymbolKind::Module,
        Kind::Method => SymbolKind::Method,
        Kind::Field => SymbolKind::Field,
        Kind::EnumConstant => SymbolKind::EnumMember,
        Kind::Package => SymbolKind::Package,
        Kind::Object => SymbolKind::Class,
        Kind::Function => SymbolKind::Function,
        Kind::Property => SymbolKind::Property,
        Kind::Constructor => SymbolKind::Constructor,
        Kind::TypeAlias => SymbolKind::TypeParameter,
    }
}

/// Converts one flat HIR document symbol into the LSP wire shape. The range
/// is used as both the enclosing and the selection range (the whole
/// declaration). The name is the IDE-side [`ide::DocumentSymbol::display_name`]
/// — the simple name (last `.`-segment, `$` kept), with the method signature
/// and field type rendered inline.
#[allow(deprecated)]
pub(crate) fn document_symbol(
    line_index: &LineIndex,
    symbol: &IdeDocumentSymbol,
) -> LspDocumentSymbol {
    let range = to_proto::range(line_index, symbol.range);
    let selection_range = to_proto::range(line_index, symbol.name_range);
    LspDocumentSymbol {
        name: symbol.display_name.clone(),
        detail: symbol.detail.clone(),
        kind: symbol_kind(symbol.kind),
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children: None,
    }
}

/// Nests the flat per-file symbol list into a type hierarchy: a symbol whose
/// qualified name is a strict prefix of another's becomes its parent. Symbols
/// without an indexed parent (top-level types, modules, unnamed-package
/// declarations) are returned at the top level, in declaration order.
pub(crate) fn nest_document_symbols(
    line_index: &LineIndex,
    symbols: &[IdeDocumentSymbol],
) -> Vec<LspDocumentSymbol> {
    // name → index into `symbols`, for parent lookup. The package symbol is
    // deliberately excluded: its name is a strict prefix of the top-level
    // types', but it is an independent item above them, not their parent.
    let index: FxHashMap<&str, usize> = symbols
        .iter()
        .enumerate()
        .filter(|(_, symbol)| symbol.kind != ide::SourceSymbolKind::Package)
        .map(|(idx, symbol)| (symbol.name.as_str(), idx))
        .collect();

    // The index is a name tree: every symbol's parent is the longest already
    // indexed name prefix, so children indices are a pure name join.
    let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); symbols.len()];
    for (idx, symbol) in symbols.iter().enumerate() {
        // `EnclosingFqn.simple`: the parent is the name minus the last `.`
        // segment. The unnamed package ([JLS §7.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.2))
        // yields no parent.
        let Some((parent, _)) = symbol.name.rsplit_once('.') else {
            continue;
        };
        if let Some(&parent_idx) = index.get(parent) {
            children_of[parent_idx].push(idx);
        }
    }

    // Converts a symbol and, recursively, its nested members. Because the
    // flat list is in declaration order, every child is converted after its
    // parent has been placed.
    fn build(
        line_index: &LineIndex,
        symbols: &[IdeDocumentSymbol],
        children_of: &[Vec<usize>],
        idx: usize,
    ) -> LspDocumentSymbol {
        let mut symbol = document_symbol(line_index, &symbols[idx]);
        if !children_of[idx].is_empty() {
            symbol.children = Some(
                children_of[idx]
                    .iter()
                    .map(|&child| build(line_index, symbols, children_of, child))
                    .collect(),
            );
        }
        symbol
    }

    symbols
        .iter()
        .enumerate()
        .filter_map(|(idx, symbol)| {
            // Top level: no already-indexed enclosing name.
            symbol
                .name
                .rsplit_once('.')
                .is_none_or(|(parent, _)| !index.contains_key(parent))
                .then_some(build(line_index, symbols, &children_of, idx))
        })
        .collect()
}

/// The server→client handle of a workspace symbol row: `(file, item)`,
/// encoded in the row's `data` field so `workspaceSymbol/resolve` can find
/// the declaration without any client-side state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceSymbolData {
    pub(crate) file_id: u32,
    pub(crate) item: u32,
}

/// A workspace symbol picker row: name, kind and container only, with the
/// `(file, item)` handle in `data` and a uri-only location (no range — that
/// costs a parse+line-index per file and is deferred to
/// `workspaceSymbol/resolve`).
pub(crate) fn workspace_symbol(
    uri: lsp_types::Uri,
    symbol: &IdeWorkspaceSymbolSummary,
) -> WorkspaceSymbol {
    WorkspaceSymbol {
        location: WorkspaceSymbolLocation::LocationUriOnly(LocationUriOnly { uri }),
        data: Some(
            serde_json::to_value(WorkspaceSymbolData {
                file_id: symbol.file.index(),
                item: symbol.item.0.0,
            })
            .unwrap(), // a struct of two u32s cannot fail to serialize
        ),
        base_symbol_information: lsp_types::BaseSymbolInformation {
            name: symbol.name.clone(),
            kind: symbol_kind(symbol.kind),
            tags: None,
            container_name: symbol.container_name.clone(),
        },
    }
}

/// `workspaceSymbol/resolve`: fills the real `Location` into the picker row
/// the client echoed back; everything else (name/kind/container/data) stays
/// as the client sent it.
pub(crate) fn resolve_workspace_symbol(
    mut symbol: WorkspaceSymbol,
    location: Location,
) -> WorkspaceSymbol {
    symbol.location = WorkspaceSymbolLocation::Location(location);
    symbol
}
