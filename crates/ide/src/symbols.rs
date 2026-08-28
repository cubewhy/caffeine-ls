//! Source symbol lookups over the HIR: per-file document symbols and
//! workspace-wide symbol search, plus direct access to the HIR type layer.
//!
//! These are the IDE-facing wrappers over the classpath-scoped source symbol
//! index of `hir` (see [`hir::source_set_symbols`]). All queries are salsa
//! memoized; the `ide::Analysis` methods below funnel them through the
//! cancellation boundary.

use hir_expand::item_tree::ItemData;
use rowan::TextRange;
use rustc_hash::FxHashSet;
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
    /// The source range of just the declared name (the identifier).
    pub name_range: TextRange,
    /// The signature shown alongside the name: the fully qualified name for a
    /// top-level type, `name(params): ret` for a method, `name: type` for a
    /// field.
    pub detail: Option<String>,
    /// The lowered item, for later HIR queries (e.g. [`crate::Analysis::item_ty`]).
    /// `None` for synthesized symbols (the file's package).
    pub item: Option<hir_expand::item_tree::ItemId>,
}

/// A document symbol plus the file it lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSymbol {
    pub file: FileId,
    pub symbol: DocumentSymbol,
}

/// The declared symbols of a file, in declaration order, prefixed by a
/// synthesized symbol for the file's package.
pub fn document_symbols(db: &RootDatabase, file_id: FileId) -> Vec<DocumentSymbol> {
    let symbols = hir::file_symbols(db, file_id);
    let names: FxHashSet<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
    let mut out = Vec::with_capacity(symbols.len() + 1);
    if !symbols.is_empty() {
        // The package is an independent item above the file's top-level
        // types; it is not a container of them.
        out.push(package_symbol(db, file_id));
    }
    out.extend(symbols.iter().map(|symbol| {
        let top_level = symbol
            .name
            .as_str()
            .rsplit_once('.')
            .is_none_or(|(parent, _)| !names.contains(parent));
        DocumentSymbol {
            name: symbol.name.as_str().to_owned(),
            kind: symbol.kind,
            range: symbol.range,
            name_range: symbol.name_range,
            detail: symbol_detail(db, file_id, symbol, top_level),
            item: Some(symbol.item),
        }
    }));
    out
}

/// The synthesized symbol for the file's package declaration, shown above its
/// top-level types. The unnamed package ([JLS §7.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.2))
/// is rendered explicitly as `<default package>`.
fn package_symbol(db: &RootDatabase, file_id: FileId) -> DocumentSymbol {
    let tree = hir::file_item_tree(db, file_id);
    let name = tree
        .package
        .as_ref()
        .map(|name| name.as_str().to_owned())
        .unwrap_or_else(|| "<default package>".to_owned());
    let range = tree.package_range.unwrap_or_default();
    DocumentSymbol {
        name,
        kind: hir::SourceSymbolKind::Package,
        range,
        name_range: range,
        detail: None,
        item: None,
    }
}

/// The signature/detail of a symbol for the LSP `detail` field. Top-level
/// types show their fully qualified name; members show their signature.
fn symbol_detail(
    db: &RootDatabase,
    file_id: FileId,
    symbol: &hir::SourceSymbol,
    top_level: bool,
) -> Option<String> {
    let simple = symbol.name.simple_name();
    if top_level {
        return Some(symbol.name.as_str().to_owned());
    }
    match symbol.kind {
        hir::SourceSymbolKind::Method => {
            let ret = item_ty(db, file_id, symbol.item);
            let params = method_params(db, file_id, symbol.item);
            // Constructors have no return type; the type layer would render
            // it as `<error>`, so show only the parameter list.
            let is_constructor = matches!(
                hir::file_item_tree(db, file_id).data(symbol.item),
                ItemData::Method(method) if method.is_constructor
            );
            if is_constructor {
                Some(format!("{simple}({})", params.join(", ")))
            } else {
                Some(format!("{simple}({}): {ret}", params.join(", ")))
            }
        }
        hir::SourceSymbolKind::Field => {
            let ty = item_ty(db, file_id, symbol.item);
            Some(format!("{simple}: {ty}"))
        }
        hir::SourceSymbolKind::EnumConstant => Some(simple.to_owned()),
        kind => Some(format!("{} {simple}", kind.label())),
    }
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
                name_range: reference.symbol.name_range,
                detail: None,
                item: Some(reference.symbol.item),
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
