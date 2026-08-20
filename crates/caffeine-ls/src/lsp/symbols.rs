//! Conversion of HIR source symbols into the LSP wire format.
//!
//! The HIR index is a flat, name-keyed list of declarations (types and
//! members); the LSP `textDocument/documentSymbol` response wants a hierarchy.
//! Members carry the canonical qualified name
//! ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7))
//! `EnclosingFqn.simple`, so the parent of a symbol is simply the longest
//! already-indexed name prefix — nesting is a pure name join, no extra
//! traversal.

use ide::{DocumentSymbol as IdeDocumentSymbol, WorkspaceSymbol as IdeWorkspaceSymbol};
use lsp_types::{
    DocumentSymbol as LspDocumentSymbol, Location, SymbolKind, WorkspaceSymbol,
    WorkspaceSymbolLocation,
};
use rustc_hash::FxHashMap;

use crate::line_index::LineIndex;

use super::to_proto;

/// Maps an HIR symbol kind to the closest LSP [`SymbolKind`]. Records and
/// annotation types have no direct LSP kind; they map to `Struct` and
/// `Interface` respectively.
pub(crate) fn symbol_kind(kind: hir::SourceSymbolKind) -> SymbolKind {
    use hir::SourceSymbolKind as Kind;
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
    }
}

/// Converts one flat HIR document symbol into the LSP wire shape. The range
/// is used as both the enclosing and the selection range (the whole
/// declaration).
#[allow(deprecated)]
pub(crate) fn document_symbol(
    line_index: &LineIndex,
    symbol: &IdeDocumentSymbol,
) -> LspDocumentSymbol {
    let range = to_proto::range(line_index, symbol.range);
    LspDocumentSymbol {
        name: symbol.name.clone(),
        detail: None,
        kind: symbol_kind(symbol.kind),
        tags: None,
        deprecated: None,
        range,
        selection_range: range,
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
    // name → index into `symbols`, for parent lookup.
    let index: FxHashMap<&str, usize> = symbols
        .iter()
        .enumerate()
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

/// Converts one HIR workspace symbol into the LSP wire shape, with the
/// enclosing type as the container name.
pub(crate) fn workspace_symbol(location: Location, symbol: &IdeWorkspaceSymbol) -> WorkspaceSymbol {
    WorkspaceSymbol {
        location: WorkspaceSymbolLocation::Location(location),
        data: None,
        base_symbol_information: lsp_types::BaseSymbolInformation {
            name: symbol.symbol.name.clone(),
            kind: symbol_kind(symbol.symbol.kind),
            tags: None,
            // The enclosing type FQN (name minus the last segment), for the
            // client's UI qualifier.
            container_name: symbol
                .symbol
                .name
                .rsplit_once('.')
                .map(|(parent, _)| parent.to_owned()),
        },
    }
}

/// The full `Location` of a workspace symbol: its file's URI and the
/// declaration range.
pub(crate) fn location(
    line_index: &LineIndex,
    uri: lsp_types::Uri,
    symbol: &IdeWorkspaceSymbol,
) -> Location {
    Location {
        uri,
        range: to_proto::range(line_index, symbol.symbol.range),
    }
}
