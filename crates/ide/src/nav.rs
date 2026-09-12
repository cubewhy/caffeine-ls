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

use std::collections::VecDeque;

use rowan::{TextRange, TextSize};
use rustc_hash::FxHashSet;
use triomphe::Arc;
use vfs::{AbsPathBuf, FileId};

use hir::JvmDatabase;
use hir::hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use hir_expand::{
    arena::ArenaId,
    body::{BodyTree, ExprData, ExprId, LocalId},
    name::Name,
};
use hir_ty::Ty;

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
    let items = body_items_at(db, file, &tree, &symbols, offset);
    let item = items.first().copied();

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
            // A method invocation, with an implicit `this` receiver when
            // `receiver` is empty ([JLS §15.12.1]).
            ExprData::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                let arity = Some(args.len());
                match receiver {
                    Some(receiver) => receiver_ty(db, file, &items, receiver)
                        .map_or_else(Vec::new, |ty| {
                            member_in_hierarchy(db, file, ty, name.as_str(), Use::Method, arity)
                        }),
                    None => enclosing_class_receiver(db, file, &tree, &symbols, offset)
                        .map_or_else(Vec::new, |ty| {
                            member_in_hierarchy(db, file, ty, name.as_str(), Use::Method, arity)
                        }),
                }
            }
            // A field access, with an implicit receiver when `target` is empty.
            ExprData::FieldAccess { target, name } => match target {
                Some(target) => receiver_ty(db, file, &items, target).map_or_else(Vec::new, |ty| {
                    member_in_hierarchy(db, file, ty, name.as_str(), Use::Field, None)
                }),
                None => enclosing_class_receiver(db, file, &tree, &symbols, offset)
                    .map_or_else(Vec::new, |ty| {
                        member_in_hierarchy(db, file, ty, name.as_str(), Use::Field, None)
                    }),
            },
            // A statically imported member ([JLS §7.5.4]): `import static
            // pkg.Type.MEMBER` (or `.*`) puts the member itself in scope.
            ExprData::Var(name) => {
                let resolver = hir_ty::Resolver::for_file(&tree);
                let mut found = Vec::new();
                let mut pending = Vec::new();
                for (owner, member) in resolver.static_import_owners(name.as_str()) {
                    match member_of_named_owner(db, file, item, &owner, &member, Use::Field, None) {
                        MemberLookup::Found(resolution) => {
                            found = vec![resolution];
                            break;
                        }
                        // A pending owner is only reported after every owner
                        // was probed.
                        MemberLookup::PendingSource(source) => pending.push(source),
                        MemberLookup::Absent => {}
                    }
                }
                if !found.is_empty() {
                    found
                } else {
                    pending.into_iter().map(Resolution::Pending).collect()
                }
            }
            // A qualified name in expression position: `Outer.Inner`,
            // `Type.field`. [JLS §6.5.2] reclassifies an ambiguous name as a
            // type first and only then as an expression name, so the whole
            // text is tried as a type reference before its last segment is
            // read as a member of the class its prefix denotes.
            ExprData::NamePath(name) => {
                let as_type = type_resolution(db, file, item, &name);
                if !as_type.is_empty() {
                    as_type
                } else {
                    match name.as_str().rsplit_once('.') {
                        Some((prefix, member)) => {
                            match member_of_named_owner(
                                db,
                                file,
                                item,
                                &Name::new(prefix),
                                member,
                                Use::Field,
                                None,
                            ) {
                                MemberLookup::Found(resolution) => vec![resolution],
                                MemberLookup::PendingSource(source) => {
                                    vec![Resolution::Pending(source)]
                                }
                                MemberLookup::Absent => Vec::new(),
                            }
                        }
                        None => Vec::new(),
                    }
                }
            }
            _ => Vec::new(),
        };
        if !resolution.is_empty() {
            return resolution;
        }
    }
    Vec::new()
}

/// The type of the expression `receiver`, read from the body that owns it.
fn receiver_ty(db: &RootDatabase, file: FileId, items: &[ItemId], receiver: ExprId) -> Option<Ty> {
    items.iter().find_map(|&item| {
        hir_ty::body_types(db, file, item).and_then(|body| body.exprs.get(&receiver).cloned())
    })
}

/// The receiver type of an implicit-`this` member access: the enclosing
/// class-like declaration of the offset.
fn enclosing_class_receiver(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    symbols: &[hir::SourceSymbol],
    offset: TextSize,
) -> Option<Ty> {
    let fqn = symbols
        .iter()
        .filter(|symbol| {
            matches!(
                symbol.kind,
                hir::SourceSymbolKind::Class
                    | hir::SourceSymbolKind::Interface
                    | hir::SourceSymbolKind::Enum
                    | hir::SourceSymbolKind::Record
                    | hir::SourceSymbolKind::Annotation
            )
        })
        .filter(|symbol| {
            item_range(db, file, tree, symbol.item).is_some_and(|range| range.contains(offset))
        })
        .min_by_key(|symbol| {
            let range = item_range(db, file, tree, symbol.item).unwrap_or_default();
            range.end() - range.start()
        })
        .and_then(|symbol| hir::source_class_fqn(db, file, symbol.item))?;
    Some(Ty::reference(db, fqn, Vec::new()))
}

/// The resolution of a member of `receiver`, walking the receiver's own class
/// and its supertypes breadth-first, so the most-derived declaration wins
/// ([JLS §8.4.8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.1),
/// [§9.4.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.1)).
///
/// The walk collects **every** unloaded owner along the hierarchy in one pass
/// (typically the class plus its supertypes, 3-6 files), so a request needs
/// exactly one materialization round instead of one per supertype level. Those
/// pending refs are only reported once the hierarchy was walked with nothing
/// found — that is what keeps it to one round.
fn member_in_hierarchy(
    db: &RootDatabase,
    file: FileId,
    receiver: Ty,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
) -> Vec<Resolution> {
    let scope = hir_ty::scope_for_file(db, file);
    let mut queue = VecDeque::from([receiver]);
    let mut seen: FxHashSet<Name> = FxHashSet::default();
    let mut pending: Vec<LibrarySourceRef> = Vec::new();
    while let Some(ty) = queue.pop_front() {
        // A primitive or array receiver has no reference type to search.
        let Some((fqn, _)) = ty.as_reference(db) else {
            continue;
        };
        if !seen.insert(fqn.clone()) {
            continue;
        }
        match members_of_owner(db, file, fqn.as_str(), name, use_kind, arity) {
            MemberLookup::Found(resolution) => return vec![resolution],
            MemberLookup::PendingSource(source) => pending.push(source),
            MemberLookup::Absent => {}
        }
        queue.extend(hir_ty::supertypes(db, &scope, &ty));
    }
    pending.into_iter().map(Resolution::Pending).collect()
}

/// An owner class of a member access, resolved through the classpath.
enum OwnerLookup {
    /// A workspace source class.
    Source(FileId),
    /// A library class and the location of its source declaration.
    Library {
        library: hir::LibraryId,
        fqn: Name,
        decl: hir::LibrarySourceDecl,
    },
    /// Nothing on the classpath carries the name.
    Unresolved,
}

/// The declared member named `name` of one owner class.
enum MemberLookup {
    Found(Resolution),
    PendingSource(LibrarySourceRef),
    Absent,
}

/// The declared member named `name` of the owner class `owner_fqn`, which must
/// already be canonical ([JLS §6.7]).
fn members_of_owner(
    db: &RootDatabase,
    file: FileId,
    owner_fqn: &str,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
) -> MemberLookup {
    match owner_lookup(db, file, owner_fqn) {
        OwnerLookup::Source(owner_file) => {
            let tree = hir::file_item_tree(db, owner_file);
            match member_item(db, owner_file, &tree, name, use_kind, arity) {
                Some(item) => MemberLookup::Found(Resolution::Decl {
                    file: owner_file,
                    item,
                    name: name.to_owned(),
                }),
                // A *loaded* owner's member set is conclusive.
                None => MemberLookup::Absent,
            }
        }
        OwnerLookup::Library {
            library,
            fqn,
            decl: hir::LibrarySourceDecl::Loaded {
                file: decl_file, ..
            },
        } => {
            let tree = hir::file_item_tree(db, decl_file);
            match member_item(db, decl_file, &tree, name, use_kind, arity) {
                Some(item) => MemberLookup::Found(Resolution::LibraryMember {
                    library,
                    owner_fqn: fqn,
                    name: name.to_owned(),
                    use_kind,
                    arity,
                    decl: hir::LibrarySourceDecl::Loaded {
                        file: decl_file,
                        item,
                    },
                }),
                None => MemberLookup::Absent,
            }
        }
        // The owning source is not loaded, so nothing about its members is
        // known: the file has to be read before the member set can be.
        OwnerLookup::Library {
            library,
            decl: hir::LibrarySourceDecl::Pending { entry, path },
            ..
        } => match hir::library_sources(db, library) {
            Some(sources) => MemberLookup::PendingSource(LibrarySourceRef {
                library,
                archive: sources.archive,
                entry,
                path,
            }),
            None => MemberLookup::Absent,
        },
        OwnerLookup::Unresolved => MemberLookup::Absent,
    }
}

/// The declared member named `name` of the class the written type name
/// `owner_name` denotes ([JLS §6.5.5.1]).
fn member_of_named_owner(
    db: &RootDatabase,
    file: FileId,
    item: Option<ItemId>,
    owner_name: &Name,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
) -> MemberLookup {
    let fqn = match hir_ty::resolve_type_name_at(db, file, item, owner_name) {
        hir_ty::NameResolution::Resolved(fqn) | hir_ty::NameResolution::NotAccessible(fqn) => fqn,
        hir_ty::NameResolution::TypeVar
        | hir_ty::NameResolution::Ambiguous(_)
        | hir_ty::NameResolution::Unresolved => return MemberLookup::Absent,
    };
    members_of_owner(db, file, fqn.as_str(), name, use_kind, arity)
}

/// The class `owner_fqn` denotes in `file`'s scope, and where its source is.
fn owner_lookup(db: &RootDatabase, file: FileId, owner_fqn: &str) -> OwnerLookup {
    let scope = hir_ty::scope_for_file(db, file);
    let Some(resolved) = hir::fqn_resolve(db, &scope, owner_fqn) else {
        return OwnerLookup::Unresolved;
    };
    match &resolved {
        hir::Resolved::Source(class) => OwnerLookup::Source(class.file),
        hir::Resolved::Library(class) => {
            let library = class.library;
            let fqn = resolved.fqn(db);
            match hir::library_source_decl(db, library, fqn.as_str()) {
                Some(decl) => OwnerLookup::Library {
                    library,
                    fqn: fqn.as_name().clone(),
                    decl,
                },
                // A library class with no source layout has no members to
                // navigate into.
                None => OwnerLookup::Unresolved,
            }
        }
    }
}

/// The symbol of the member `name` declared in `file`, preferring the one that
/// takes `arity` parameters and falling back to a name-only match — mirroring
/// the file-local `member_targets` shape.
fn member_item(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
) -> Option<ItemId> {
    let symbols = hir::file_symbols(db, file);
    let candidates: Vec<&hir::SourceSymbol> = symbols
        .iter()
        .filter(|symbol| {
            let kind_matches = match use_kind {
                Use::Field => matches!(
                    symbol.kind,
                    hir::SourceSymbolKind::Field | hir::SourceSymbolKind::EnumConstant
                ),
                Use::Method => symbol.kind == hir::SourceSymbolKind::Method,
            };
            kind_matches && symbol.name.simple_name() == name
        })
        .collect();
    let by_arity = arity.and_then(|arity| {
        candidates
            .iter()
            .find(|symbol| parameter_count(tree, symbol.item) == Some(arity))
    });
    by_arity
        .or_else(|| candidates.first())
        .map(|symbol| symbol.item)
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

/// The rendered signature of a resolved library member: types, flags and the
/// return type come from the classfile stub (the authority); parameter names
/// are the classfile's `MethodParameters` names when it has them
/// ([JVMS §4.7.24](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.24))
/// and the source declaration's names at the same index otherwise, and
/// `arg{i}` when neither is available.
///
/// Renders as `ret name(T p, T p2)` for a method and `type name` for a field —
/// the shape a Java declaration reads as, rather than a classfile descriptor.
fn library_member_signature(
    db: &RootDatabase,
    library: hir::LibraryId,
    owner_fqn: &Name,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
    source_file: Option<FileId>,
) -> Option<HoverInfo> {
    // The class index is keyed by binary names
    // ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2)),
    // which is the spelling `Resolved::fqn` hands out for a library class.
    let interner = &db.hir_state().interner;
    let symbol = interner.get_or_intern(owner_fqn.as_str());
    let index = hir::library_name_index(db, library);
    let (entry_idx, entry) = index.lookup(symbol)?;
    let resolved = hir::ResolvedClass {
        library,
        entry_idx,
        entry: entry.clone(),
    };
    let record = hir::class_record(db, &resolved)?;
    let hir::ClassOrModuleStub::Class(stub) = record.as_ref() else {
        return None;
    };

    match use_kind {
        Use::Method => {
            let method = stub.methods.iter().find(|method| {
                interner.resolve(&method.name) == name
                    && arity.is_none_or(|arity| method.params.len() == arity)
            })?;
            let param_count = method.params.len();
            let return_ty = hir_ty::ty_from_library(db, &method.return_type);
            let return_type = return_ty.display(db).to_string();
            let params: Vec<String> = method
                .params
                .iter()
                .enumerate()
                .map(|(index, param)| {
                    let param_ty = hir_ty::ty_from_library(db, &param.param_type);
                    let ty = param_ty.display(db);
                    let name = param
                        .name
                        .map(|symbol| interner.resolve(&symbol).to_owned())
                        .or_else(|| {
                            source_parameter_name(db, source_file, name, param_count, index)
                        })
                        .unwrap_or_else(|| format!("arg{index}"));
                    format!("{ty} {name}")
                })
                .collect();
            Some(HoverInfo {
                value: format!("{return_type} {name}({})", params.join(", ")),
            })
        }
        Use::Field => {
            let field = stub
                .fields
                .iter()
                .find(|field| interner.resolve(&field.name) == name)?;
            let field_ty = hir_ty::ty_from_library(db, &field.field_type);
            let ty = field_ty.display(db);
            Some(HoverInfo {
                value: format!("{ty} {name}"),
            })
        }
    }
}

/// The declared name of parameter `index` of the method `name` in the loaded
/// library source file, when that declaration has the same parameter count.
fn source_parameter_name(
    db: &RootDatabase,
    source_file: Option<FileId>,
    name: &str,
    param_count: usize,
    index: usize,
) -> Option<String> {
    let file = source_file?;
    let tree = hir::file_item_tree(db, file);
    let item = member_item(db, file, &tree, name, Use::Method, Some(param_count))?;
    match tree.data(item) {
        ItemData::Method(method) => method
            .sig
            .params
            .get(index)
            .map(|param| param.name.as_str().to_owned()),
        _ => None,
    }
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

/// The hover at `offset`: the merged signature of a resolved library member,
/// the type of the expression or local the offset falls on, or the signature
/// of the declaration it falls inside.
pub fn hover(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<HoverInfo> {
    let tree = hir::file_item_tree(db, file);
    let bodies = hir::file_body_tree(db, file);
    let symbols = hir::file_symbols(db, file);

    // A resolved library reference outranks everything below: its signature is
    // what the user is asking about. When its declaring source is not loaded
    // yet, hover answers `None` *without* consulting the fallbacks, so the LSP
    // layer materializes the file and the retried hover shows the merged
    // signature — answering the expression's type (or the bytecode-only
    // rendering) here would hide the merge on the first, and most likely only,
    // hover.
    match resolve_at(db, file, offset).into_iter().next() {
        Some(Resolution::Pending(_)) => return None,
        Some(Resolution::LibraryMember {
            library,
            owner_fqn,
            name,
            use_kind,
            arity,
            decl,
        }) => {
            let source_file = match decl {
                hir::LibrarySourceDecl::Loaded { file, .. } => file,
                hir::LibrarySourceDecl::Pending { .. } => return None,
            };
            return library_member_signature(
                db,
                library,
                &owner_fqn,
                &name,
                use_kind,
                arity,
                Some(source_file),
            );
        }
        Some(Resolution::Decl { .. }) | None => {}
    }

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
