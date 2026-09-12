//! Name-level navigation over the HIR: goto-definition and hover at an
//! offset of a source file.
//!
//! Navigation is source-side and name-based: a reference — a local variable
//! use, a field access, a method invocation, or a type reference in a `new`,
//! `instanceof` or class literal — resolves to the declaration(s) carrying
//! the same simple name within the file ([JLS §6.5]). Locals resolve to
//! their declaration exactly; members and types resolve to every same-named
//! declaration in the file (overloads and shadows included). This foundation
//! serves the LSP `textDocument/definition` and `textDocument/hover`
//! requests; it deliberately stays name-based rather than running the full
//! type-directed resolution of [§15.12].
//!
//! When the file-local walk finds nothing, the reference is resolved through
//! the classpath instead: a type or member reference that denotes a library
//! declaration resolves to the *library's source file*, which the LSP layer
//! materializes on demand. A library declaration whose source is not loaded
//! yet is reported as pending rather than being answered.

use rowan::{TextRange, TextSize};
use triomphe::Arc;
use vfs::{AbsPathBuf, FileId};

use hir::hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use hir_expand::{
    arena::ArenaId,
    body::{BodyTree, ExprData, ExprId, LocalId},
    name::Name,
};

use crate::RootDatabase;
use ide_db::base_db::{self, LanguageKind};

/// The source range of a declaration item, resolved on demand from the file's
/// parse (the item tree carries no offsets).
fn item_range(db: &RootDatabase, file: FileId, tree: &ItemTree, item: ItemId) -> Option<TextRange> {
    let language = tree.language;
    if language == LanguageKind::Unknown {
        return None;
    }
    let source = base_db::parse(db, file, language).syntax_node(language);
    let map = hir::hir_def::db::ast_id_map(db, file, language);
    hir::hir_def::java::ranges::item_range(map, &source, tree, item)
}

/// The declaration a reference resolves to: a file and the source range of
/// the declaring construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationTarget {
    pub file: FileId,
    pub range: TextRange,
    pub name: String,
}

/// A hover result: a rendered signature or type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInfo {
    pub value: String,
}

/// The declarations the reference at `offset` resolves to ([JLS §6.5]) — a
/// local variable use to its declaration, a field/method/type reference to
/// every same-named source declaration of the file, or — when the file-local
/// walk finds nothing — the classpath declaration the reference actually
/// denotes.
pub fn definition(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<NavigationTarget> {
    let bodies = hir::file_body_tree(db, file);
    for expr_id in exprs_at(&bodies, offset) {
        let targets = match bodies.expr(expr_id).clone() {
            ExprData::Var(name) => {
                let name_str = name.as_str();
                // §6.3: a local of this body — the innermost declaration in
                // scope, so a shadowing inner declarator beats an outer one.
                if let Some(local) = resolve_local(&bodies, name_str, offset) {
                    return vec![NavigationTarget {
                        file,
                        range: bodies.local_range(local).unwrap_or_default(),
                        name: name_str.to_owned(),
                    }];
                }
                // Otherwise an implicit-receiver field or a statically
                // imported constant — resolve like a field access.
                member_targets(db, file, name_str, Use::Field, None)
            }
            ExprData::FieldAccess { name, .. } => {
                member_targets(db, file, name.as_str(), Use::Field, None)
            }
            ExprData::MethodCall { name, args, .. } => {
                let mut found =
                    member_targets(db, file, name.as_str(), Use::Method, Some(args.len()));
                if found.is_empty() {
                    found = member_targets(db, file, name.as_str(), Use::Method, None);
                }
                found
            }
            // Type references: `new`, `instanceof`, class literals and
            // qualified type names resolve to the class-like declarations of
            // the name.
            ExprData::New { ty, .. } | ExprData::ClassLit(ty) => {
                let Some(name) = type_ref_name(&ty) else {
                    return Vec::new();
                };
                type_targets(db, file, name)
            }
            ExprData::InstanceOf { ty, .. } => {
                let Some(name) = ty.as_ref().and_then(|t| type_ref_name(t)) else {
                    return Vec::new();
                };
                type_targets(db, file, name)
            }
            ExprData::NamePath(name) => type_targets(db, file, name.simple_name().to_owned()),
            _ => Vec::new(),
        };
        if !targets.is_empty() {
            return targets;
        }
    }

    // No same-file declaration: the reference may denote a classpath
    // declaration. A library one that is not materialized yet cannot be
    // answered here — the LSP layer reads it into the database and re-runs the
    // request (see [`pending_library_sources`]).
    resolve_at(db, file, offset)
        .into_iter()
        .filter_map(|resolution| match resolution {
            Resolution::Decl { file, item, name } => decl_target(db, file, item, &name),
            Resolution::LibraryMember {
                decl: hir::LibrarySourceDecl::Loaded { file, item },
                name,
                ..
            } => decl_target(db, file, item, &name),
            Resolution::LibraryMember { .. } | Resolution::Pending(_) => None,
        })
        .collect()
}

/// The library source files a reference resolves into but which are not loaded
/// into the database yet, in resolution order. The LSP layer reads each
/// `entry` out of `archive` into `path` and re-runs the request.
pub fn pending_library_sources(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
) -> Vec<LibrarySourceRef> {
    resolve_at(db, file, offset)
        .into_iter()
        .filter_map(|resolution| match resolution {
            Resolution::Pending(source) => Some(source),
            _ => None,
        })
        .collect()
}

/// A library source file a reference resolves into but which is not loaded
/// into the database yet: the LSP layer reads `entry` out of `archive` into
/// `path` and re-runs the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySourceRef {
    pub library: hir::LibraryId,
    pub archive: AbsPathBuf,
    pub entry: Arc<str>,
    pub path: AbsPathBuf,
}

/// What the reference at an offset resolves to through the classpath.
enum Resolution {
    /// A declaration in the database: a workspace or already-loaded library
    /// declaration.
    Decl {
        file: FileId,
        item: ItemId,
        name: String,
    },
    /// A resolved library member: everything the signature renderer needs plus
    /// where its declaring source is, if it is loaded.
    LibraryMember {
        library: hir::LibraryId,
        owner_fqn: Name,
        name: String,
        use_kind: Use,
        arity: Option<usize>,
        decl: hir::LibrarySourceDecl,
    },
    /// A library source entry that has to be materialized before the reference
    /// can be answered.
    Pending(LibrarySourceRef),
}

/// The classpath resolutions of the innermost navigable expression at
/// `offset`, innermost first: the first expression with a resolution wins.
fn resolve_at(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    let bodies = hir::file_body_tree(db, file);
    let tree = hir::file_item_tree(db, file);
    let symbols = hir::file_symbols(db, file);
    let item = body_items_at(db, file, &tree, &symbols, offset)
        .first()
        .copied();

    for expr_id in exprs_at(&bodies, offset) {
        let resolution = match bodies.expr(expr_id).clone() {
            ExprData::New { ty, .. } | ExprData::ClassLit(ty) => type_ref_name(&ty)
                .map_or_else(Vec::new, |name| {
                    type_resolution(db, file, item, &Name::new(&name))
                }),
            ExprData::InstanceOf { ty, .. } => ty
                .as_ref()
                .and_then(|t| type_ref_name(t))
                .map_or_else(Vec::new, |name| {
                    type_resolution(db, file, item, &Name::new(&name))
                }),
            ExprData::NamePath(name) => type_resolution(db, file, item, &name),
            _ => Vec::new(),
        };
        if !resolution.is_empty() {
            return resolution;
        }
    }
    Vec::new()
}

/// The classpath resolution of the type name written at `item` in `file`.
fn type_resolution(
    db: &RootDatabase,
    file: FileId,
    item: Option<ItemId>,
    name: &Name,
) -> Vec<Resolution> {
    // §7.4.3: a name that exists on the classpath but is not visible from the
    // file's module still denotes that class — navigation is not a compile
    // check.
    let fqn = match hir_ty::resolve_type_name_at(db, file, item, name) {
        hir_ty::NameResolution::Resolved(fqn) | hir_ty::NameResolution::NotAccessible(fqn) => fqn,
        hir_ty::NameResolution::TypeVar
        | hir_ty::NameResolution::Ambiguous(_)
        | hir_ty::NameResolution::Unresolved => return Vec::new(),
    };
    class_resolution(db, file, &fqn)
}

/// The resolution of the canonical class name `fqn` in `file`'s scope.
fn class_resolution(db: &RootDatabase, file: FileId, fqn: &Name) -> Vec<Resolution> {
    let scope = hir_ty::scope_for_file(db, file);
    let Some(resolved) = hir::fqn_resolve(db, &scope, fqn.as_str()) else {
        return Vec::new();
    };
    match &resolved {
        hir::Resolved::Source(class) => vec![Resolution::Decl {
            file: class.file,
            item: class.item,
            name: fqn.simple_name().to_owned(),
        }],
        hir::Resolved::Library(class) => {
            let library_fqn = resolved.fqn(db);
            library_class_resolution(db, class.library, library_fqn.as_name())
        }
    }
}

/// The resolution of a class known to live in `library`.
fn library_class_resolution(
    db: &RootDatabase,
    library: hir::LibraryId,
    fqn: &Name,
) -> Vec<Resolution> {
    match hir::library_source_decl(db, library, fqn.as_str()) {
        Some(hir::LibrarySourceDecl::Loaded { file, item }) => vec![Resolution::Decl {
            file,
            item,
            name: fqn.simple_name().to_owned(),
        }],
        Some(hir::LibrarySourceDecl::Pending { entry, path }) => {
            let Some(archive) = hir::library_sources(db, library).map(|sources| sources.archive)
            else {
                return Vec::new();
            };
            vec![Resolution::Pending(LibrarySourceRef {
                library,
                archive,
                entry,
                path,
            })]
        }
        None => Vec::new(),
    }
}

/// A navigation target for one resolved declaration: its name range inside
/// `decl_file`.
fn decl_target(
    db: &RootDatabase,
    decl_file: FileId,
    item: ItemId,
    name: &str,
) -> Option<NavigationTarget> {
    let tree = hir::file_item_tree(db, decl_file);
    let range = item_range(db, decl_file, &tree, item)?;
    Some(NavigationTarget {
        file: decl_file,
        range,
        name: Name::new(name).simple_name().to_owned(),
    })
}

/// The local of `name` in scope at `offset` ([JLS §6.3], [§6.4]): a
/// same-named declarator *enclosing* the reference is a shadowing inner
/// declaration and wins over every outer one; otherwise the nearest
/// declaration *before* the use wins, since a local's scope begins at its own
/// declarator and cannot reach forward. The body IR records declarator
/// ranges but not block extents ([§6.3] scopes), so a use lexically *after*
/// an inner block closed still resolves to the inner declaration — the
/// innermost-first order matches javac everywhere else. `None` when no
/// declaration is in scope: the name may denote a field or import.
fn resolve_local(bodies: &BodyTree, name: &str, offset: TextSize) -> Option<LocalId> {
    let mut enclosing: Option<(TextRange, LocalId)> = None;
    let mut preceding: Option<(TextRange, LocalId)> = None;
    for (id, local) in bodies.locals.iter() {
        if local.name.as_str() != name {
            continue;
        }
        let id = LocalId(id);
        let Some(range) = bodies.local_range(id) else {
            continue;
        };
        if range.contains(offset) {
            if enclosing.is_none_or(|(best, _)| range.len() < best.len()) {
                enclosing = Some((range, id));
            }
        } else if range.start() <= offset
            && preceding.is_none_or(|(best, _)| range.start() > best.start())
        {
            preceding = Some((range, id));
        }
    }
    enclosing.or(preceding).map(|(_, id)| id)
}

/// The plain (possibly qualified) type name of a `TypeRef`, descending
/// through array dimensions.
fn type_ref_name(tyref: &syntax::stub::TypeRef<hir_expand::name::Name>) -> Option<String> {
    match tyref {
        syntax::stub::TypeRef::Reference { name, .. } => Some(name.as_str().to_owned()),
        syntax::stub::TypeRef::Array(inner) => type_ref_name(inner),
        _ => None,
    }
}

/// The hover at `offset`: the type of the expression or local the offset
/// falls on, or the signature of the declaration it falls inside.
pub fn hover(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<HoverInfo> {
    let tree = hir::file_item_tree(db, file);
    let bodies = hir::file_body_tree(db, file);
    let symbols = hir::file_symbols(db, file);

    // An expression's inferred type, from the enclosing body — walk the
    // innermost enclosing expressions first.
    for expr_id in exprs_at(&bodies, offset) {
        for item in body_items_at(db, file, &tree, &symbols, offset) {
            if let Some(body) = hir_ty::body_types(db, file, item)
                && let Some(ty) = body.exprs.get(&expr_id)
            {
                return Some(HoverInfo {
                    value: ty.display(db).to_string(),
                });
            }
        }
    }

    // A local variable — the declaration's declarator contains the offset.
    for (id, local) in bodies.locals.iter() {
        if bodies
            .local_range(LocalId(id))
            .is_some_and(|range| range.contains_inclusive(offset))
        {
            for item in body_items_at(db, file, &tree, &symbols, offset) {
                if let Some(body) = hir_ty::body_types(db, file, item)
                    && let Some(ty) = body.locals.get(&LocalId(id))
                {
                    return Some(HoverInfo {
                        value: format!("{}: {}", local.name.as_str(), ty.display(db)),
                    });
                }
            }
        }
    }

    // A declaration's signature.
    render_symbol_decl(db, file, &tree, &symbols, offset)
}

/// The body-carrying item ids whose range contains `offset`, most-derived
/// first — the owners whose `BodyTypes` may type the construct at the offset.
fn body_items_at(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    symbols: &[hir::SourceSymbol],
    offset: TextSize,
) -> Vec<ItemId> {
    let range_of = |item| item_range(db, file, &tree, item);
    let mut candidates: Vec<(TextRange, ItemId)> = symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                hir::SourceSymbolKind::Method | hir::SourceSymbolKind::Field
            ) && range_of(s.item).is_some_and(|range| range.contains(offset))
        })
        .filter_map(|s| range_of(s.item).map(|range| (range, s.item)))
        .collect();
    candidates.sort_by_key(|(range, _)| range.end() - range.start());
    candidates.into_iter().map(|(_, item)| item).collect()
}

/// The rendered signature of the declaration the offset falls inside: a
/// method's `name(params): ret`, a field's `name: ty`, a class-like
/// declaration's `kind name`.
fn render_symbol_decl(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    symbols: &[hir::SourceSymbol],
    offset: TextSize,
) -> Option<HoverInfo> {
    let symbol = symbols
        .iter()
        .filter(|s| item_range(db, file, &tree, s.item).is_some_and(|range| range.contains(offset)))
        .min_by_key(|s| {
            let range = item_range(db, file, &tree, s.item).unwrap_or_default();
            range.end() - range.start()
        })?;
    let simple = symbol.name.simple_name();
    let value = match symbol.kind {
        hir::SourceSymbolKind::Method => {
            crate::symbols::method_signature(db, file, symbol.item, simple, true)
        }
        hir::SourceSymbolKind::Field => {
            format!(
                "{simple}: {}",
                crate::symbols::item_ty(db, file, symbol.item)
            )
        }
        hir::SourceSymbolKind::EnumConstant => simple.to_string(),
        kind => format!("{} {}", kind.label(), simple),
    };
    Some(HoverInfo { value })
}

/// The kind of member or type a reference resolves to.
#[derive(Debug, Clone, Copy)]
enum Use {
    Field,
    Method,
}

/// The same-named source declarations of `file` for a member use: fields and
/// enum constants for a field access, methods (arity-preferred when given)
/// for a call.
fn member_targets(
    db: &RootDatabase,
    file: FileId,
    simple: &str,
    use_kind: Use,
    arity: Option<usize>,
) -> Vec<NavigationTarget> {
    let tree = hir::file_item_tree(db, file);
    hir::file_symbols(db, file)
        .iter()
        .filter(|s| {
            let kind_matches = match use_kind {
                Use::Field => matches!(
                    s.kind,
                    hir::SourceSymbolKind::Field | hir::SourceSymbolKind::EnumConstant
                ),
                Use::Method => s.kind == hir::SourceSymbolKind::Method,
            };
            kind_matches
                && s.name.simple_name() == simple
                && arity.is_none_or(|arity| {
                    parameter_count(&tree, s.item).is_some_and(|count| count == arity)
                })
        })
        .filter_map(|s| {
            item_range(db, file, &tree, s.item).map(|range| NavigationTarget {
                file,
                range,
                name: simple.to_owned(),
            })
        })
        .collect()
}

/// The same-named class-like declarations of `file`.
fn type_targets(db: &RootDatabase, file: FileId, simple: String) -> Vec<NavigationTarget> {
    let tree = hir::file_item_tree(db, file);
    hir::file_symbols(db, file)
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                hir::SourceSymbolKind::Class
                    | hir::SourceSymbolKind::Interface
                    | hir::SourceSymbolKind::Enum
                    | hir::SourceSymbolKind::Record
                    | hir::SourceSymbolKind::Annotation
            ) && s.name.simple_name() == simple
        })
        .filter_map(|s| {
            item_range(db, file, &tree, s.item).map(|range| NavigationTarget {
                file,
                range,
                name: simple.clone(),
            })
        })
        .collect()
}

/// The parameter count of the method declaration `item`, from the item tree.
fn parameter_count(tree: &hir::hir_def::java::item_tree::ItemTree, item: ItemId) -> Option<usize> {
    match tree.data(item) {
        ItemData::Method(method) => Some(method.sig.params.len()),
        _ => None,
    }
}

/// The expressions whose source range contains `offset`, innermost (smallest
/// range) first. An offset on an argument or operand may fall inside several
/// nested expressions; navigation walks the innermost first.
fn exprs_at(bodies: &BodyTree, offset: TextSize) -> Vec<ExprId> {
    let mut enclosing: Vec<(u32, ExprId)> = bodies
        .expr_ranges
        .iter()
        .enumerate()
        .filter(|(_, range)| range.contains(offset))
        .map(|(idx, range)| (u32::from(range.len()), ExprId(ArenaId(idx as u32))))
        .collect();
    enclosing.sort_by_key(|(len, _)| *len);
    enclosing.into_iter().map(|(_, id)| id).collect()
}
