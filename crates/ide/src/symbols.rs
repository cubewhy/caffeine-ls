//! Source symbol lookups over the HIR: per-file document symbols and
//! workspace-wide symbol search, plus direct access to the HIR type layer.
//!
//! These are the IDE-facing wrappers over the classpath-scoped source symbol
//! index of `hir` (see [`hir::source_set_symbols`]). All queries are salsa
//! memoized; the `ide::Analysis` methods below funnel them through the
//! cancellation boundary.

use hir::hir_def::java::item_tree::ItemData;

/// The `(map, source)` pair the on-demand range helpers resolve against, for
/// a file whose language is known.
fn range_ctx(
    db: &RootDatabase,
    file: FileId,
    language: ide_db::base_db::LanguageKind,
) -> Option<(hir_expand::ast_id_map::AstIdMap, syntax::SourceFile)> {
    if language == ide_db::base_db::LanguageKind::Unknown {
        return None;
    }
    let parse = ide_db::base_db::parse(db, file, language);
    let source = parse.syntax_node(language);
    let map = hir::hir_def::db::ast_id_map(db, file, language).clone();
    Some((map, source))
}
use rowan::TextRange;
use rustc_hash::FxHashSet;
use triomphe::Arc;
use vfs::FileId;

use crate::RootDatabase;

use hir_expand::arena::ArenaId;
type ItemId = hir::hir_def::java::item_tree::ItemId;

/// A source symbol as seen by the IDE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSymbol {
    /// The canonical qualified name ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)).
    /// Kept fully qualified so the LSP layer can nest members under their
    /// enclosing type; the client-facing text is [`DocumentSymbol::display_name`].
    pub name: String,
    pub kind: hir::SourceSymbolKind,
    /// The source range of the declaration.
    pub range: TextRange,
    /// The source range of just the declared name (the identifier).
    pub name_range: TextRange,
    /// The name shown to the client: the *simple* name (the last `.`-segment,
    /// `$` kept as an identifier character per [JLS
    /// §3.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.8)),
    /// plus the signature where javac/IntelliJ render it inline — `name(params): ret`
    /// for a method (with `...` on a variable-arity parameter), `name: type`
    /// for a field. The package symbol keeps its full package name.
    pub display_name: String,
    /// The signature shown alongside the name: the fully qualified name for a
    /// top-level type, `kind simple` for a nested type, `None` for methods,
    /// fields, enum constants and the package.
    pub detail: Option<String>,
    /// The lowered item, for later HIR queries (e.g. [`crate::Analysis::item_ty`]).
    /// `None` for synthesized symbols (the file's package).
    pub item: Option<hir::hir_def::java::item_tree::ItemId>,
}

/// A workspace symbol picker row: the three fields a client renders before
/// navigation plus the `(file, item)` identity carried in the row's `data`.
/// Deliberately range- and signature-free: `workspace/symbol` stays O(index);
/// the LSP layer resolves the location for the one row the client navigates
/// to via `workspaceSymbol/resolve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSymbolSummary {
    pub file: FileId,
    /// Client-facing name: the simple name for type rows (`Foo`), and
    /// `{EnclosingType}.{simple}` for member rows (`Foo.bar`) — the dotted
    /// member form keeps `Class.member` queries matching the row's label in
    /// clients that filter server results client-side (VS Code).
    pub name: String,
    pub kind: hir::SourceSymbolKind,
    /// Client-facing container: the package for top-level types and members
    /// (`com.example`); the enclosing type's FQN for nested types. `None` in
    /// the unnamed package.
    pub container_name: Option<String>,
    pub item: ItemId,
}

/// The declared symbols of a file, in declaration order, prefixed by a
/// synthesized symbol for the file's package.
///
/// A record ([JLS §8.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10))
/// is expanded with its *implicit* members — a public accessor method per
/// component ([§8.10.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.3))
/// and its canonical constructor ([§8.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.4))
/// — so the outline shows what the language synthesizes. These are synthetic
/// (`item: None`) and carry no source ranges of their own: the accessor
/// ranges target its record component, and the canonical constructor's
/// ranges target the record declaration.
pub fn document_symbols(db: &RootDatabase, file_id: FileId) -> Vec<DocumentSymbol> {
    let symbols = hir::file_symbols(db, file_id);
    let names: FxHashSet<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
    let tree = hir::file_item_tree(db, file_id);
    let ctx = range_ctx(db, file_id, tree.language);
    let mut out = Vec::with_capacity(symbols.len() + 1);
    if !symbols.is_empty() {
        // The package is an independent item above the file's top-level
        // types; it is not a container of them.
        out.push(package_symbol(db, file_id));
    }
    out.extend(symbols.iter().flat_map(|source| {
        let top_level = source
            .name
            .as_str()
            .rsplit_once('.')
            .is_none_or(|(parent, _)| !names.contains(parent));
        let data = tree.data(source.item);
        let item = source.item;
        // The item tree carries no offsets: the ranges are resolved from the
        // file's parse. A record's outline range points at its declaration
        // *header* (the definition, including the component list) rather than
        // the whole body; its selection points at the component list itself.
        let (range, name_range) = match (ctx.as_ref(), data) {
            (Some((map, source)), ItemData::Record(_)) => (
                hir::hir_def::java::ranges::record_header_range(map, source, &tree, item),
                hir::hir_def::java::ranges::record_components_range(map, source, &tree, item),
            ),
            (Some((map, source)), _) => (
                hir::hir_def::java::ranges::item_range(map, source, &tree, item),
                hir::hir_def::java::ranges::item_name_range(map, source, &tree, item),
            ),
            // An unknown-language file has no parse; no ranges to point at.
            (None, _) => (None, None),
        };
        let (range, name_range) = (range.unwrap_or_default(), name_range.unwrap_or_default());
        let symbol = DocumentSymbol {
            name: source.name.as_str().to_owned(),
            kind: source.kind,
            range,
            name_range,
            display_name: symbol_display_name(db, file_id, source, top_level),
            detail: symbol_detail(source, top_level),
            item: Some(source.item),
        };
        // Records synthesize their implicit members right after themselves,
        // so the name-prefix nesting (parent = name minus the last `.`-segment)
        // groups each under the record.
        std::iter::once(symbol.clone()).chain(record_members(db, file_id, &symbol, data))
    }));
    out
}

/// The implicit members of a record declaration, synthesized as `item: None`
/// `DocumentSymbol`s: an accessor per component ([JLS §8.10.3]) and the
/// canonical constructor ([JLS §8.10.4], skipped when the body declares an
/// explicit full-form constructor of matching arity).
fn record_members(
    db: &RootDatabase,
    file_id: FileId,
    symbol: &DocumentSymbol,
    data: &ItemData,
) -> Vec<DocumentSymbol> {
    let ItemData::Record(record) = data else {
        return Vec::new();
    };
    let mut members = Vec::with_capacity(record.components.len() + 1);
    let tree = hir::file_item_tree(db, file_id);
    let ctx = range_ctx(db, file_id, tree.language);
    let component_tys: Arc<Vec<String>> = Arc::new(
        hir_ty::record_component_types(db, file_id, symbol.item.unwrap())
            .into_iter()
            .map(|ty| ty.display_simple(db).to_string())
            .collect(),
    );

    // §8.10.3: each component has a public accessor `component(): T`. For a
    // varargs component the accessor returns the array type `T[]` (§8.4.1).
    // An accessor explicitly declared by the body (same name, zero parameters)
    // replaces the implicit one ([§8.10.3]).
    for (index, component) in record.components.iter().enumerate() {
        let simple = component.name.as_str().to_owned();
        let declares_accessor = record.body.iter().any(|item| {
            matches!(tree.data(*item), ItemData::Method(method)
                if method.name.as_str() == simple && method.sig.params.is_empty())
        });
        if declares_accessor {
            continue;
        }
        let ret = if component.varargs {
            format!("{}[]", component_tys[index])
        } else {
            component_tys[index].clone()
        };
        let component_range = ctx
            .as_ref()
            .and_then(|(map, source)| {
                hir::hir_def::java::ranges::component_range(map, source, component)
            })
            .unwrap_or_default();
        members.push(DocumentSymbol {
            name: format!("{}.{simple}", symbol.name),
            kind: hir::SourceSymbolKind::Method,
            // The accessor's range and selection range both target its whole
            // component declaration (`int x`, not just the name) — the
            // "definition" of the member.
            range: component_range,
            name_range: component_range,
            display_name: format!("{simple}(): {ret}"),
            detail: None,
            item: None,
        });
    }

    // §8.10.4: a record has a canonical constructor whose parameters mirror
    // the components in order. An explicit full-form constructor of matching
    // arity replaces it ([§8.10.4]).
    let simple = symbol
        .name
        .rsplit('.')
        .next()
        .unwrap_or(&symbol.name)
        .to_owned();
    let declares_canonical = record.body.iter().any(|item| {
        matches!(tree.data(*item), ItemData::Method(method) if method.is_constructor()
            && method.sig.params.len() == record.components.len())
    });
    if !declares_canonical {
        let params = record
            .components
            .iter()
            .enumerate()
            .map(|(index, component)| {
                if component.varargs {
                    format!("{}...", component_tys[index])
                } else {
                    component_tys[index].clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        members.push(DocumentSymbol {
            name: format!("{}.{simple}", symbol.name),
            kind: hir::SourceSymbolKind::Method,
            range: symbol.range,
            name_range: symbol.name_range,
            display_name: format!("{simple}({params})"),
            detail: None,
            item: None,
        });
    }
    members
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
    let range = range_ctx(db, file_id, tree.language)
        .and_then(|(map, source)| {
            tree.package_decls.last().and_then(|decl| {
                hir::hir_def::java::ranges::package_name_range(&map, &source, *decl)
            })
        })
        .unwrap_or_default();
    DocumentSymbol {
        name: name.clone(),
        display_name: name,
        kind: hir::SourceSymbolKind::Package,
        range,
        name_range: range,
        detail: None,
        item: None,
    }
}

/// The client-facing name of a document symbol: the simple name (the last
/// `.`-segment, `$` kept per [JLS §3.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.8)),
/// with the signature rendered inline — `name(params): ret` for a method,
/// `name: type` for a field. The package keeps its full package name.
fn symbol_display_name(
    db: &RootDatabase,
    file_id: FileId,
    symbol: &hir::SourceSymbol,
    top_level: bool,
) -> String {
    let simple = symbol.name.simple_name();
    if top_level {
        return simple.to_owned();
    }
    match symbol.kind {
        hir::SourceSymbolKind::Method => method_signature(db, file_id, symbol.item, simple, true),
        hir::SourceSymbolKind::Field => {
            format!("{simple}: {}", item_ty(db, file_id, symbol.item))
        }
        _ => simple.to_owned(),
    }
}

/// The `detail` of a symbol for the LSP `detail` field. Top-level types show
/// their fully qualified name; nested types show their kind and simple name;
/// methods, fields, enum constants and the package show none — their signature
/// is already part of [`DocumentSymbol::display_name`].
fn symbol_detail(symbol: &hir::SourceSymbol, top_level: bool) -> Option<String> {
    let simple = symbol.name.simple_name();
    if top_level {
        return Some(symbol.name.as_str().to_owned());
    }
    match symbol.kind {
        hir::SourceSymbolKind::Method
        | hir::SourceSymbolKind::Field
        | hir::SourceSymbolKind::EnumConstant => None,
        kind => Some(format!("{} {simple}", kind.label())),
    }
}

/// The rendered signature of a method or constructor: `simple(params)` with an
/// optional `: ret` (skipped for constructors and when `include_return` is
/// false). A variable-arity parameter renders its element type plus the
/// ellipsis ([JLS §8.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.1)),
/// so `String... names` is `String...`.
pub fn method_signature(
    db: &RootDatabase,
    file_id: FileId,
    item: hir::hir_def::java::item_tree::ItemId,
    simple: &str,
    include_return: bool,
) -> String {
    let is_constructor = matches!(
        hir::file_item_tree(db, file_id).data(item),
        ItemData::Method(method) if method.is_constructor()
    );
    let ret = if include_return && !is_constructor {
        format!(": {}", item_ty(db, file_id, item))
    } else {
        String::new()
    };
    format!("{simple}({}){ret}", render_params(db, file_id, item))
}

/// The parameter types of a method or constructor, in declaration order,
/// simple-rendered and joined with `, `, with `...` appended to a
/// variable-arity parameter.
fn render_params(
    db: &RootDatabase,
    file_id: FileId,
    item: hir::hir_def::java::item_tree::ItemId,
) -> String {
    let tree = hir::file_item_tree(db, file_id);
    let varargs = matches!(
        tree.data(item),
        ItemData::Method(method) if method.sig.params.last().is_some_and(|param| param.varargs)
    );
    let mut params = method_params(db, file_id, item).to_vec();
    if varargs && let Some(last) = params.last_mut() {
        *last = format!("{}...", last);
    }
    params.join(", ")
}

/// Symbols whose simple name starts with `query`, or whose canonical name
/// (`pkg.Enclosing.simple`, JLS §6.7) contains it — case-insensitive;
/// prefix-preferred (simple-name prefix matches are included alongside FQN
/// substring matches). Dotted queries work: `Class.member` and FQNs like
/// `com.example.Foo` match the canonical name. Restricted to `files` when
/// given, else every registered source set; sorted by (canonical name, file,
/// item) and deduplicated, with an empty query returning everything (within
/// scope).
pub fn workspace_symbol_summaries(
    db: &RootDatabase,
    query: &str,
    files: Option<&[FileId]>,
) -> Vec<WorkspaceSymbolSummary> {
    let mut refs: Vec<hir::SourceSymbolRef> = Vec::new();
    match files {
        None => {
            // Index path — exactly the current collection logic, no filtering.
            for source_set in registered_source_sets(db) {
                let index = hir::source_set_symbols(db, source_set);
                if query.trim().is_empty() {
                    refs.extend(index.iter().cloned());
                } else {
                    refs.extend(index.lookup_simple(query));
                    refs.extend(index.lookup_fqn_substring(query));
                }
            }
        }
        Some(files) => {
            // Opened-files path (empty query by contract): per-file item trees
            // only, no source-set index scan. `contains` on the canonical
            // name, mirroring the index lookups (simple-name prefix ∪ FQN
            // substring).
            let query = query.to_lowercase();
            for &file in files {
                for symbol in hir::file_symbols(db, file).iter() {
                    if query.trim().is_empty()
                        || symbol.name.as_str().to_lowercase().contains(&query)
                    {
                        refs.push(hir::SourceSymbolRef {
                            file,
                            symbol: symbol.clone(),
                        });
                    }
                }
            }
        }
    }
    refs.sort_by_key(|reference| {
        (
            reference.symbol.name.as_str().to_owned(),
            reference.file,
            reference.symbol.item,
        )
    });
    refs.dedup();
    refs.into_iter()
        .map(|reference| {
            let fqn = reference.symbol.name.as_str().to_owned();
            let (name, container_name) = match reference.symbol.kind {
                // Members render as `{EnclosingType}.{simple}` (`Foo.bar`)
                // with the container reduced to the package, so dotted and
                // fully-qualified queries survive the client's local filter,
                // which fuzzy-matches the row's `name` and drops rows it
                // does not match.
                hir::SourceSymbolKind::Method
                | hir::SourceSymbolKind::Field
                | hir::SourceSymbolKind::EnumConstant => {
                    let (parent, simple) = fqn.rsplit_once('.').unwrap_or(("", fqn.as_str()));
                    let (container, enclosing) = parent
                        .rsplit_once('.')
                        .map_or((None, parent), |(pkg, ty)| (Some(pkg), ty));
                    (
                        format!("{}.{}", enclosing, simple),
                        container.map(str::to_owned),
                    )
                }
                _ => (
                    fqn.rsplit_once('.')
                        .map(|(_, simple)| simple.to_owned())
                        .unwrap_or_else(|| fqn.clone()),
                    fqn.rsplit_once('.').map(|(parent, _)| parent.to_owned()),
                ),
            };
            WorkspaceSymbolSummary {
                file: reference.file,
                name,
                kind: reference.symbol.kind,
                container_name,
                item: reference.symbol.item,
            }
        })
        .collect()
}

/// The declaration range of one lowered item, on demand — the
/// `workspaceSymbol/resolve` half of the flow. Returns `None` for a stale
/// `(file, item)` (file rewritten or deleted since the row was served).
pub fn source_symbol_range(db: &RootDatabase, file_id: FileId, item: u32) -> Option<TextRange> {
    let item = hir::hir_def::java::item_tree::ItemId(ArenaId(item));
    // The id indexes into this revision's item tree; re-validate before
    // touching the arena — `Arena::get` panics out of bounds and a rewritten
    // file's tree can shrink.
    if !hir::file_symbols(db, file_id)
        .iter()
        .any(|symbol| symbol.item == item)
    {
        return None;
    }
    let tree = hir::file_item_tree(db, file_id);
    let (map, source) = range_ctx(db, file_id, tree.language)?;
    hir::hir_def::java::ranges::item_range(&map, &source, &tree, item)
}

/// The registered source sets of the project graph, in unspecified order.
fn registered_source_sets(db: &RootDatabase) -> Vec<hir::SourceSetId> {
    hir::project_graph(db)
        .map(|graph| graph.source_sets(db).keys().cloned().collect())
        .unwrap_or_default()
}

/// The declared type of an item — a field's type, a method's return type, or
/// the type of a class-like declaration — rendered from the HIR type layer
/// with the *simple* class name (the last `.`-segment).
pub fn item_ty(
    db: &RootDatabase,
    file_id: FileId,
    item: hir::hir_def::java::item_tree::ItemId,
) -> String {
    hir_ty::item_ty(db, file_id, item)
        .display_simple(db)
        .to_string()
}

/// The parameter types of a method or constructor, in declaration order,
/// simple-rendered from the HIR type layer.
pub fn method_params(
    db: &RootDatabase,
    file_id: FileId,
    item: hir::hir_def::java::item_tree::ItemId,
) -> Arc<[String]> {
    Arc::from(
        hir_ty::method_params(db, file_id, item)
            .into_iter()
            .map(|ty| ty.display_simple(db).to_string())
            .collect::<Vec<_>>(),
    )
}
