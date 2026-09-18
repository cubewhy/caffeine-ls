//! Kotlin navigation: the declaration-side half of the Kotlin surface.
//!
//! A Kotlin *declaration* names three kinds of thing, and those are what this
//! module resolves ([KLS
//! `scopes-and-identifiers.html#scopes-and-identifiers`](https://kotlinlang.org/spec/scopes-and-identifiers.html#scopes-and-identifiers),
//! KLS `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)):
//!
//! * the declaration's own name — a *declaration*, not a reference to itself,
//!   which still has a definition ([`self_target`]);
//! * a name inside an `importHeader` — the imported declaration, resolved as a
//!   qualified name ([`import_resolution`]);
//! * a name inside a `userType` of the declaration skeleton — a supertype, a
//!   declared type, a receiver type, a type-parameter bound, a type alias's
//!   target or an annotation's name ([`type_resolution`]).
//!
//! A name inside a function body, a property initializer or an annotation
//! argument is an *expression* reference instead: it is resolved by the type
//! layer — the member bridge, the local scope and the inference — and read back
//! from the record that resolution left
//! ([`hir_ty::KotlinBodyTypes::resolved`], [`recorded_reference`]). The
//! parameter-name surface ([`super::declared_parameter_names`]) reads the same
//! resolved callables: a Kotlin source one carries its names in the item tree,
//! a classpath one in its declaring source.
//!
//! # Name resolution
//!
//! A written type name is resolved against the file's scope, in the language's
//! order ([`Scope`]): an enclosing type parameter, the file's explicit imports,
//! the enclosing classifiers (innermost first), the file's own package, then
//! the star imports. Each candidate is turned into a declaration by
//! [`declaration_by_fqn`]: a class-like declaration through
//! [`hir::fqn_resolve`] — which honors classpath order and finds library
//! classes — and any other source declaration (a function, a property, a type
//! alias) through the workspace symbol index.
//!
//! The *default* imports (`kotlin.*`, `java.lang.*`, ...) are deliberately not
//! seeded here: they are the type layer's resolver's business
//! (`hir_ty::kotlin::resolve`), which lands with the Kotlin type model.

use rowan::{NodeOrToken, TextRange, TextSize};
use rustc_hash::FxHashSet;
use syntax::kotlin::{Lang, SyntaxKind as K};

use hir::hir_def::kotlin::item_tree::{KotlinImportItem, KotlinItemData, KotlinItemTree};
use hir::hir_def::kotlin::ranges;
use ide_db::base_db::parse;
use vfs::FileId;

use super::{HoverInfo, LibraryFileRef, NavigationTarget, ReferenceTarget, RootDatabase};

/// What a declaration-side name in a Kotlin file resolves to.
enum Resolution {
    /// A declaration of a file in the database: a workspace file, or an
    /// already-loaded library source.
    Decl {
        file: FileId,
        range: TextRange,
        name: String,
    },
    /// A name with no declaration item of its own — a type parameter — at the
    /// range of its own name token.
    Local {
        file: FileId,
        range: TextRange,
        name: String,
    },
    /// A library class whose declaring source is not loaded yet: the LSP layer
    /// materializes it and re-runs the request.
    Pending(LibraryFileRef),
}

/// The declarations the reference at `offset` resolves to.
pub(crate) fn definition(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
) -> Vec<NavigationTarget> {
    let resolutions = resolutions(db, file, offset);
    if resolutions.is_empty() {
        // Nothing resolved: the offset is on a declaration's own name, which is
        // a declaration rather than a reference to itself.
        return self_target(db, file, offset).into_iter().collect();
    }
    targets(db, resolutions)
}

/// The declaration-side reference sites of the declaration(s) the offset
/// names: every identifier token in the workspace's Kotlin files that resolves
/// — through the same forward pipeline [`definition`] uses — to one of those
/// declarations.
///
/// The query's own answer is the invariant of the feature: a site counts when
/// the declarations [`resolutions`] produces for it are ones the query
/// resolves to, so `references` can never disagree with `definition`.
///
/// The sweep visits every identifier token of every Kotlin file, so a site is
/// whatever the forward pipeline answers for at that token — the declaration's
/// own name (when `include_declaration`), the names of its supertypes and
/// declared types, the import paths that bind it, the annotation names that
/// apply it, and a *body* name the type layer resolved (a local, a member of
/// the enclosing classifier, a call's callee, another file's declaration).
pub(crate) fn references(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
    include_declaration: bool,
) -> Vec<ReferenceTarget> {
    let found = definition(db, file, offset);
    if found.is_empty() {
        return Vec::new();
    }
    let decls: FxHashSet<(FileId, TextRange)> = found
        .iter()
        .map(|target| (target.file, target.range))
        .collect();
    // The written token of a reference is the declaration's *simple* name (a
    // qualified name is written segment by segment, and each segment resolves
    // on its own), so the candidate filter is the set of simple names.
    let names: FxHashSet<String> = found
        .iter()
        .map(|target| {
            target
                .name
                .rsplit('.')
                .next()
                .unwrap_or(&target.name)
                .to_owned()
        })
        .collect();

    let mut files = db.source_files();
    files.push(file);
    files.sort_unstable();
    files.dedup();

    let mut hits = sweep(db, &files, &names, &decls);
    if include_declaration {
        hits.extend(found.iter().map(|target| ReferenceTarget {
            file: target.file,
            range: target.range,
        }));
    }
    hits.sort_by_key(|hit| (hit.file, hit.range.start()));
    hits.dedup();
    hits
}

/// The library files a Kotlin reference needs before it can be answered, in
/// resolution order (see [`super::pending_library_files`]).
pub(crate) fn pending_library_files(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
) -> Vec<LibraryFileRef> {
    let mut seen: FxHashSet<(hir::LibraryId, String)> = FxHashSet::default();
    let mut out = Vec::new();
    for resolution in resolutions(db, file, offset) {
        let Resolution::Pending(pending) = resolution else {
            continue;
        };
        let key = match &pending {
            LibraryFileRef::Source { library, entry, .. } => (*library, entry.to_string()),
            LibraryFileRef::Decompile { library, class, .. } => (*library, class.to_string()),
        };
        if seen.insert(key) {
            out.push(pending);
        }
    }
    out
}

/// The hover at `offset`: the declaration the offset names — or is written
/// inside, for a reference — rendered as its Kotlin signature, plus its KDoc.
///
/// A reference can name a declaration of the *other* language (a Kotlin file
/// references Java classes as readily as Kotlin ones), so the rendering
/// dispatches on the declaring file's language: a Kotlin declaration renders
/// from the item tree, a Java one through [`super::java::hover`].
pub(crate) fn hover(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<HoverInfo> {
    let ctx = Ctx::new(db, file)?;
    let token = identifier_at(&ctx, offset)?;
    match site_of(&token) {
        Site::Import { name } => hover_resolution(db, &import_resolution(db, &ctx, &name)?),
        Site::Type(name) => {
            let scope = Scope::of(&ctx, &token)?;
            hover_resolution(db, &type_resolution(db, &ctx, &scope, &name, offset)?)
        }
        // A declaration's own name, or — past its header — a name inside its
        // body, which `site_of` cannot tell apart from the declaration it is
        // written in (the first declaration ancestor is the same node).
        Site::Declaration => match declaration_item_at(&ctx, offset) {
            Some(item) => Some(HoverInfo {
                value: render_signature(&ctx.tree, item),
                docs: crate::docs::hover_docs(db, file, item),
            }),
            None => expression_hover(db, file, offset),
        },
        // A name in a body: the type the inference recorded for the expression
        // written there — the hover a client shows over a variable or a call.
        Site::Other => expression_hover(db, file, offset),
    }
}

/// The hover of an expression inside a body: the innermost expression whose
/// range covers `offset` and whose type the inference recorded, rendered in
/// the Kotlin spelling ([`hir_ty::display_kotlin`]).
///
/// The body inference is memoized per item, so this walks the item that owns
/// the expression and asks for the innermost match — the same "innermost wins"
/// rule the Java hover uses for its own expression answers.
fn expression_hover(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<HoverInfo> {
    let ctx = Ctx::new(db, file)?;
    let bodies = hir::file_body_tree(db, file);
    // A declaration's *initializer* expressions carry types too — a property
    // writes one in place of a body — so every item is asked, and the
    // candidates are the expressions *of this item* ([`KotlinBodyTypes::exprs`]
    // is per declaration, while the body arena's ids are the file's). A `.kts`
    // script's implicit `main` is asked last: no item owns it, and its
    // inference is memoized per *file*
    // ([`hir_def::kotlin::item_tree::KotlinItemTree::script_body`]).
    let mut types_of: Vec<_> = ctx
        .tree
        .items
        .iter()
        .map(|(id, _)| hir_ty::kotlin_declaration_types(db, file, hir_expand::ids::ItemId(id)))
        .collect();
    if ctx.tree.script_body.is_some() {
        types_of.push(hir_ty::kotlin_script_body_types(db, file));
    }
    for types in types_of {
        let innermost = types
            .exprs
            .keys()
            .filter_map(|&expr| {
                let range = bodies.expr_range(expr)?;
                range.contains(offset).then_some((range.len(), expr))
            })
            .min_by_key(|(len, _)| *len);
        if let Some((_, expr)) = innermost {
            let ty = types.expr_ty(db, expr);
            return Some(HoverInfo {
                value: hir_ty::display_kotlin(db, ty).to_string(),
                docs: None,
            });
        }
    }
    None
}

/// The hover of a resolved declaration.
fn hover_resolution(db: &RootDatabase, resolution: &Resolution) -> Option<HoverInfo> {
    match resolution {
        // A type parameter declares nothing beyond its name, and no
        // documentation attaches to it.
        Resolution::Local { name, .. } => Some(HoverInfo {
            value: name.clone(),
            docs: None,
        }),
        // A library class whose source is not loaded: the LSP layer's deferral
        // loads it (see [`pending_library_files`]) and re-runs the request,
        // which then answers through the declaration's own language.
        Resolution::Pending(_) => None,
        Resolution::Decl { file, range, .. } => match Ctx::new(db, *file) {
            Some(ctx) => {
                let item = item_of_range(&ctx, *range)?;
                Some(HoverInfo {
                    value: render_signature(&ctx.tree, item),
                    docs: crate::docs::hover_docs(db, *file, item),
                })
            }
            None => super::java::hover(db, *file, range.start()),
        },
    }
}

/// The rendered Kotlin signature of a declaration: `fun f(x: Int): String`,
/// `val x: Int`, `class Point`, with its modifiers. Types render through the
/// item tree's own display ([`hir::hir_def::kotlin::pretty::display_type`]),
/// so the text is the Kotlin spelling of the declared type.
///
/// Shared with the inlay-hint layer, which renders a callable's signature as
/// the tooltip of its parameter-name hints.
pub(crate) fn render_signature(
    tree: &KotlinItemTree,
    item: hir::hir_def::kotlin::item_tree::ItemId,
) -> String {
    use hir::hir_def::kotlin::pretty::display_type;

    fn params(params: &[hir::hir_def::kotlin::item_tree::KotlinParam]) -> String {
        params
            .iter()
            .map(|parameter| {
                let param = &parameter.param;
                format!(
                    "{}{}{}{}: {}",
                    if param.varargs { "vararg " } else { "" },
                    if parameter.noinline { "noinline " } else { "" },
                    if parameter.crossinline {
                        "crossinline "
                    } else {
                        ""
                    },
                    param.name,
                    display_type(&param.ty)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
    fn modifiers(data: &KotlinItemData) -> String {
        match data.modifiers() {
            Some(modifiers) => modifiers
                .names()
                .map(|name| format!("{name} "))
                .collect::<String>(),
            None => String::new(),
        }
    }

    let data = tree.data(item);
    let modifiers = modifiers(data);
    match data {
        KotlinItemData::Class(data) => {
            format!("{modifiers}{} {}", data.kind.keyword(), data.name)
        }
        KotlinItemData::Constructor(data) => {
            format!("{modifiers}constructor({})", params(&data.params))
        }
        KotlinItemData::Function(data) => {
            let receiver = data
                .receiver
                .as_ref()
                .map(|receiver| format!("{}.", display_type(receiver)))
                .unwrap_or_default();
            let ret = data
                .ret
                .as_ref()
                .map(|ret| format!(": {}", display_type(ret)))
                .unwrap_or_default();
            format!(
                "{modifiers}fun {receiver}{}({}){ret}",
                data.name,
                params(&data.params)
            )
        }
        KotlinItemData::Property(data) => {
            let ty = data
                .ty
                .as_ref()
                .map(|ty| format!(": {}", display_type(ty)))
                .unwrap_or_default();
            format!(
                "{modifiers}{} {}{ty}",
                if data.is_var { "var" } else { "val" },
                data.name
            )
        }
        KotlinItemData::Accessor(data) => {
            if data.is_setter {
                "set".to_owned()
            } else {
                "get".to_owned()
            }
        }
        KotlinItemData::TypeAlias(data) => format!(
            "{modifiers}typealias {} = {}",
            data.name,
            display_type(&data.target)
        ),
        KotlinItemData::EnumEntry(data) => format!("entry {}", data.name),
        KotlinItemData::AnonymousInitializer(_) => "init".to_owned(),
    }
}

/// The declaration of the class-like type `fqn` in `file`'s scope — the
/// declaration a click on an inlay hint's type label navigates to.
pub(crate) fn class_declaration(
    db: &RootDatabase,
    file: FileId,
    fqn: &str,
) -> Option<NavigationTarget> {
    // A rendered Kotlin type spells its projections and nullability
    // (`List<out Number>`, `String?`); the class the hint names is the outer
    // classifier, so the rendering is stripped down to its name.
    let fqn = fqn.split('<').next().unwrap_or(fqn).trim();
    let fqn = fqn.strip_suffix('?').unwrap_or(fqn).trim();
    let fqn = fqn.strip_suffix(" & Any").unwrap_or(fqn).trim();
    match declaration_by_fqn(db, file, fqn)? {
        Resolution::Decl { file, range, name } => Some(NavigationTarget { file, range, name }),
        // A library class is not loaded yet: the caller's deferral (see
        // [`pending_library_files`]) is what materializes it, and this request
        // carries no offset to defer from. A type parameter is not a
        // class-like declaration.
        Resolution::Local { .. } | Resolution::Pending(_) => None,
    }
}

/// The item of `ctx` whose declared *name* is `range`.
fn item_of_range(ctx: &Ctx, range: TextRange) -> Option<hir::hir_def::kotlin::item_tree::ItemId> {
    all_items(ctx)
        .into_iter()
        .find(|(_, item)| ctx.name_range(*item) == Some(range))
        .map(|(_, item)| item)
}

// -- the forward pipeline ----------------------------------------------------

/// The resolutions of the declaration-side name at `offset`; empty when the
/// offset names nothing this module resolves.
fn resolutions(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    let Some(ctx) = Ctx::new(db, file) else {
        return Vec::new();
    };
    let Some(token) = identifier_at(&ctx, offset) else {
        return Vec::new();
    };
    match site_of(&token) {
        Site::Import { name } => import_resolution(db, &ctx, &name).into_iter().collect(),
        Site::Type(name) => {
            let Some(scope) = Scope::of(&ctx, &token) else {
                return Vec::new();
            };
            let _ = offset;
            type_resolution(db, &ctx, &scope, &name, offset)
                .into_iter()
                .collect()
        }
        // A declaration's own name is not a reference; `self_target` answers
        // it. Anything else inside the declaration is a *body* name, which the
        // inference's own record answers — `site_of` cannot tell the two apart,
        // since the first declaration ancestor of both is the declaration.
        Site::Declaration => match declaration_item_at(&ctx, offset) {
            Some(_) => Vec::new(),
            None => recorded_reference(db, file, offset),
        },
        // A name inside a body: the declaration the inference resolved it to
        // ([`hir_ty::KotlinBodyTypes::resolved`]).
        Site::Other => recorded_reference(db, file, offset),
    }
}

/// The declaration the name at `offset` resolved to, from the body inference's
/// own record ([`hir_ty::KotlinBodyTypes::resolved`]).
///
/// The navigation layer asked `infer` for the name instead of re-resolving it:
/// the member bridge ([`hir_ty::kotlin::method`]) decides what a name means —
/// a local, an enclosing classifier's member, a top-level declaration of
/// another file, a Java or classfile member through its JVM view — and this
/// reads that decision back.
fn recorded_reference(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    let Some(ctx) = Ctx::new(db, file) else {
        return Vec::new();
    };
    let bodies = hir::file_body_tree(db, file);
    // A *local* binding's own name is a declaration of the body — it is no item
    // of the file, so `self_target` cannot answer it.
    if let Some((local, range)) = bodies
        .locals
        .iter()
        .filter_map(|(id, _)| {
            let local = hir_expand::body::LocalId(id);
            let range = bodies.local_name_ranges.get(id.0 as usize).copied()?;
            Some((local, range))
        })
        .find(|(_, range)| range.contains(offset))
    {
        return vec![Resolution::Decl {
            file,
            range,
            name: bodies.local(local).name.to_string(),
        }];
    }
    // The innermost expression that *has* a resolution: a call's callee
    // name and a receiver are expressions of their own, and the name at
    // the offset may be one of them, so the search walks outwards to the
    // expression the inference recorded a target for — the call itself, or
    // the access.
    let mut candidates: Vec<(TextSize, hir_expand::body::ExprId)> = bodies
        .exprs
        .iter()
        .filter_map(|(id, _)| {
            let expr = hir_expand::body::ExprId(id);
            let range = bodies.expr_range(expr)?;
            range.contains(offset).then_some((range.len(), expr))
        })
        .collect();
    candidates.sort_by_key(|(len, _)| *len);

    // Every body of the file that can hold the expression, in tree order: the
    // items with a body, and — for a `.kts` script — the body of the implicit
    // `main`, which no item owns and whose inference the type layer memoizes
    // per *file* ([`hir_def::kotlin::item_tree::KotlinItemTree::script_body`]).
    let mut types_of = Vec::new();
    for (id, _) in ctx.tree.items.iter() {
        let item = hir_expand::ids::ItemId(id);
        if ctx.tree.data(item).body_id().is_some() {
            types_of.push(hir_ty::kotlin_declaration_types(db, file, item));
        }
    }
    if ctx.tree.script_body.is_some() {
        types_of.push(hir_ty::kotlin_script_body_types(db, file));
    }

    for types in types_of {
        let Some(resolved) = candidates
            .iter()
            .find_map(|&(_, expr)| types.resolved.get(&expr))
        else {
            continue;
        };
        return match resolved {
            hir_ty::KotlinResolvedMember::Local(local) => {
                let Some(range) = bodies.local_name_ranges.get(local.0.0 as usize).copied() else {
                    return Vec::new();
                };
                vec![Resolution::Decl {
                    file,
                    range,
                    name: bodies.local(*local).name.to_string(),
                }]
            }
            hir_ty::KotlinResolvedMember::Kotlin { file, item } => {
                match declaration_name_range(db, *file, *item) {
                    Some(range) => vec![Resolution::Decl {
                        file: *file,
                        range,
                        name: ctx
                            .tree
                            .data(*item)
                            .name()
                            .map(|name| name.to_string())
                            .unwrap_or_default(),
                    }],
                    None => Vec::new(),
                }
            }
            // A Java or classfile member: the declaration the JVM view points
            // at, in the file that declares it.
            hir_ty::KotlinResolvedMember::Java(method) => {
                java_member_resolution(db, method.owner_file, method.decl_item)
            }
            hir_ty::KotlinResolvedMember::JavaField(field) => {
                java_member_resolution(db, field.owner_file, field.decl_item)
            }
        };
    }
    Vec::new()
}

/// The resolution of a *Java* member's declaration, from the `(file, item)` the
/// JVM view carries.
fn java_member_resolution(
    db: &RootDatabase,
    file: Option<FileId>,
    item: Option<hir::hir_def::jvm::ids::ItemId>,
) -> Vec<Resolution> {
    let (Some(file), Some(item)) = (file, item) else {
        return Vec::new();
    };
    if hir::hir_def::kotlin::plugin::tree(db, file).is_some() {
        return declaration_name_range(db, file, item)
            .map(|range| {
                vec![Resolution::Decl {
                    file,
                    range,
                    name: String::new(),
                }]
            })
            .unwrap_or_default();
    }
    let tree = hir::hir_def::java::plugin::tree(db, file);
    let language = hir::file_item_tree(db, file).language();
    let parse = parse(db, file, language);
    let source = parse.syntax_node(language);
    let map = hir::hir_def::db::ast_id_map(db, file, language);
    hir::hir_def::java::ranges::item_name_range(map, &source, &tree, item)
        .map(|range| {
            vec![Resolution::Decl {
                file,
                range,
                name: String::new(),
            }]
        })
        .unwrap_or_default()
}

/// The declarations `resolution` names, for a caller that has already filtered
/// out the ones it cannot answer.
fn targets(db: &RootDatabase, resolutions: Vec<Resolution>) -> Vec<NavigationTarget> {
    resolutions
        .into_iter()
        .filter_map(|resolution| match resolution {
            Resolution::Decl { file, range, name } => {
                let _ = db;
                Some(NavigationTarget { file, range, name })
            }
            Resolution::Local { file, range, name } => Some(NavigationTarget { file, range, name }),
            Resolution::Pending(_) => None,
        })
        .collect()
}

/// Every reference site of `names` in `files`, one worker per chunk of files.
///
/// A `RootDatabase` is `Send` but not `Sync`, so each rayon worker runs on its
/// own clone (the shape the Java sweep and
/// [`crate::workspace::workspace_reports`] use); the clones share salsa's memo
/// tables, so the parses and lowerings the sweep resolves against are computed
/// once.
fn sweep(
    db: &RootDatabase,
    files: &[FileId],
    names: &FxHashSet<String>,
    decls: &FxHashSet<(FileId, TextRange)>,
) -> Vec<ReferenceTarget> {
    use rayon::prelude::*;

    let num_workers = rayon::current_num_threads().max(1);
    let chunk_size = files.len().div_ceil(num_workers);
    let chunks: Vec<&[FileId]> = files.chunks(chunk_size.max(1)).collect();
    let databases: Vec<RootDatabase> = (0..chunks.len()).map(|_| db.clone()).collect();
    chunks
        .into_par_iter()
        .zip(databases.into_par_iter())
        .flat_map_iter(|(chunk, db)| {
            chunk
                .iter()
                .flat_map(move |&file| file_references(&db, file, names, decls))
        })
        .collect()
}

/// The reference sites of `names` inside one Kotlin file: every `IDENTIFIER`
/// token that names one of the query's declarations and whose resolution is
/// one of them.
///
/// The token walk is the candidate source because enumerating sites from type
/// references and imports would duplicate each step of the forward pipeline and
/// drift from it; a reference can only be written as an identifier token of the
/// declaration's name, so the filter is sound, and it bounds the number of
/// [`resolutions`] calls to the tokens that could possibly answer.
fn file_references(
    db: &RootDatabase,
    file: FileId,
    names: &FxHashSet<String>,
    decls: &FxHashSet<(FileId, TextRange)>,
) -> Vec<ReferenceTarget> {
    let Some(ctx) = Ctx::new(db, file) else {
        return Vec::new();
    };
    // Every identifier token is a candidate — a body name resolves through the
    // type layer like a type reference does, so the sweep cannot skip a body.
    let mut out = Vec::new();
    for element in ctx.root.descendants_with_tokens() {
        let Some(token) = element.as_token() else {
            continue;
        };
        if token.kind() != K::IDENTIFIER || !names.contains(token.text()) {
            continue;
        }
        let range = token.text_range();
        if targets(db, resolutions(db, file, range.start()))
            .iter()
            .any(|target| decls.contains(&(target.file, target.range)))
        {
            out.push(ReferenceTarget { file, range });
        }
    }
    out
}

/// The declaration the offset is a *reference* to, resolved as a name: an
/// import's path, or a `userType` of the declaration skeleton.
fn import_resolution(db: &RootDatabase, ctx: &Ctx, name: &str) -> Option<Resolution> {
    // A star import binds the *members* of the named classifier or package;
    // the path itself still names a declaration when it is a classifier.
    declaration_by_fqn(db, ctx.file, name)
}

/// The resolution of the written type name `name` in `scope`
/// ([KLS `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing),
/// [KLS `scopes-and-identifiers.html#scopes-and-identifiers`](https://kotlinlang.org/spec/scopes-and-identifiers.html#scopes-and-identifiers)).
fn type_resolution(
    db: &RootDatabase,
    ctx: &Ctx,
    scope: &Scope,
    name: &str,
    _offset: TextSize,
) -> Option<Resolution> {
    let segments: Vec<&str> = name.split('.').collect();
    let simple = *segments.first()?;

    // A type parameter shadows every other declaration of that name: it is
    // declared closest ([KLS
    // `scopes-and-identifiers.html#scopes-and-identifiers`](https://kotlinlang.org/spec/scopes-and-identifiers.html#scopes-and-identifiers)).
    if let Some((_, range)) = scope.type_param(simple) {
        return Some(Resolution::Local {
            file: ctx.file,
            range,
            name: simple.to_owned(),
        });
    }

    for candidate in scope.candidates(name) {
        if let Some(resolution) = declaration_by_fqn(db, ctx.file, &candidate) {
            return Some(resolution);
        }
    }
    None
}

/// The declaration of the qualified name `fqn` in `file`'s scope.
///
/// A class-like declaration resolves through [`hir::fqn_resolve`], which walks
/// the classpath in order and finds library classes; any other source
/// declaration (a top-level function or property, a type alias, a member) is
/// looked up in the workspace symbol index by its exact qualified name.
fn declaration_by_fqn(db: &RootDatabase, file: FileId, fqn: &str) -> Option<Resolution> {
    if fqn.is_empty() {
        return None;
    }
    let scope = hir_ty::scope_for_file(db, file);
    if let Some(resolved) = hir::fqn_resolve(db, &scope, fqn) {
        return match &resolved {
            hir::Resolved::Source(class) => Some(Resolution::Decl {
                file: class.file,
                range: declaration_name_range(db, class.file, class.item)?,
                name: simple_name(fqn),
            }),
            // A Kotlin file's facade class is synthesized: it has no
            // declaration to navigate to.
            hir::Resolved::Facade { .. } => None,
            hir::Resolved::Library(class) => {
                library_class_resolution(db, class.library, resolved.fqn(db).as_str())
            }
        };
    }

    // A non-class declaration: the workspace symbol index by exact qualified
    // name. A *member* (a function, a property, a type alias) is indexed under
    // `Enclosing.simple`, so its FQN is exactly what an import writes.
    for source_set in registered_source_sets(db) {
        let index = hir::source_set_symbols(db, source_set);
        for reference in index.lookup_fqn_substring(fqn) {
            if reference.symbol.name.as_str() != fqn {
                continue;
            }
            let range = declaration_name_range(db, reference.file, reference.symbol.item)?;
            return Some(Resolution::Decl {
                file: reference.file,
                range,
                name: simple_name(fqn),
            });
        }
    }
    None
}

/// The resolution of a library class: a loaded source, a pending archive entry
/// or a class the decompiler has to produce.
fn library_class_resolution(
    db: &RootDatabase,
    library: hir::LibraryId,
    fqn: &str,
) -> Option<Resolution> {
    match hir::library_source_decl(db, library, fqn)? {
        hir::LibrarySourceDecl::Loaded { file, item } => Some(Resolution::Decl {
            file,
            range: declaration_name_range(db, file, item)?,
            name: simple_name(fqn),
        }),
        hir::LibrarySourceDecl::Pending { entry, path } => {
            let archive = hir::library_sources(db, library).map(|sources| sources.archive)?;
            Some(Resolution::Pending(LibraryFileRef::Source {
                library,
                archive,
                entry,
                path,
            }))
        }
        hir::LibrarySourceDecl::Decompiled { class, path } => {
            Some(Resolution::Pending(LibraryFileRef::Decompile {
                library,
                class,
                path,
            }))
        }
    }
}

/// Goto-definition on a declaration's own name answers with the declaration
/// itself: `Point` in `class Point`, `distance` in `fun distance()`, `x` in
/// `val x: Int`. These names are declarations, not references, so no step
/// above resolves them.
///
/// A declaration that introduces itself with a keyword instead of a name —
/// `constructor(...)`, `get`/`set` and `init` — is answered the same way: its
/// name range is that keyword ([`ranges::item_name_range`]), and the label the
/// keyword spells ([`KotlinItemData::label`]) stands in for the name the
/// declaration does not have.
fn self_target(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<NavigationTarget> {
    let ctx = Ctx::new(db, file)?;
    let item = declaration_item_at(&ctx, offset)?;
    let range = ctx.name_range(item)?;
    let data = ctx.tree.data(item);
    let name = data
        .name()
        .map_or_else(|| data.label().to_owned(), |name| name.as_str().to_owned());
    Some(NavigationTarget { file, range, name })
}

/// The *innermost* declaration whose declared name carries `offset`.
fn declaration_item_at(
    ctx: &Ctx,
    offset: TextSize,
) -> Option<hir::hir_def::kotlin::item_tree::ItemId> {
    all_items(ctx)
        .into_iter()
        .filter(|(_, item)| ctx.name_range(*item).is_some_and(|r| r.contains(offset)))
        .min_by_key(|(range, _)| range.len())
        .map(|(_, item)| item)
}

/// Every item of the file with its declaration range, in tree order.
fn all_items(ctx: &Ctx) -> Vec<(TextRange, hir::hir_def::kotlin::item_tree::ItemId)> {
    let mut out = Vec::new();
    for &top in &ctx.tree.top {
        walk(ctx, top, &mut out);
    }
    out
}

fn walk(
    ctx: &Ctx,
    item: hir::hir_def::kotlin::item_tree::ItemId,
    out: &mut Vec<(TextRange, hir::hir_def::kotlin::item_tree::ItemId)>,
) {
    let data = ctx.tree.data(item);
    if let Some(range) = ctx.item_range(item) {
        out.push((range, item));
    }
    // A classifier's primary constructor hangs off the classifier header, not
    // off its body ([`ClassData::primary_constructor`] is a field of its own),
    // so it is visited before the members.
    if let KotlinItemData::Class(class) = data
        && let Some(constructor) = class.primary_constructor
    {
        walk(ctx, constructor, out);
    }
    // A property's accessors hang off the property, not off a classifier body.
    if let KotlinItemData::Property(property) = data {
        for &accessor in &property.accessors {
            walk(ctx, accessor, out);
        }
    }
    for &child in data.body() {
        walk(ctx, child, out);
    }
}

// -- the file's syntax context ----------------------------------------------

/// The database-derived context of one Kotlin file: its item tree, its
/// declaration-skeleton id map and its current syntax tree.
struct Ctx {
    file: FileId,
    tree: triomphe::Arc<KotlinItemTree>,
    map: hir_expand::ast_id_map::AstIdMap,
    source: syntax::SourceFile,
    root: rowan::SyntaxNode<Lang>,
}

impl Ctx {
    fn new(db: &RootDatabase, file: FileId) -> Option<Ctx> {
        let file_tree = hir::file_item_tree(db, file);
        let tree = hir::hir_def::kotlin::plugin::tree(db, file)?;
        let language = file_tree.language();
        let parse = parse(db, file, language);
        let source = parse.syntax_node(language);
        let root = ranges::kotlin_root(&source)?.clone();
        let map = hir::hir_def::db::ast_id_map(db, file, language).clone();
        Some(Ctx {
            file,
            tree,
            map,
            source,
            root,
        })
    }

    /// The declared range of `item`.
    fn item_range(&self, item: hir::hir_def::kotlin::item_tree::ItemId) -> Option<TextRange> {
        ranges::item_range(&self.map, &self.source, &self.tree, item)
    }

    /// The declared-*name* range of `item`.
    fn name_range(&self, item: hir::hir_def::kotlin::item_tree::ItemId) -> Option<TextRange> {
        ranges::item_name_range(&self.map, &self.source, &self.tree, item)
    }
}

/// The declared-name range of a declaration in `file`, in whichever language
/// declares it: the target's own language resolves it.
fn declaration_name_range(
    db: &RootDatabase,
    file: FileId,
    item: hir::hir_def::jvm::ids::ItemId,
) -> Option<TextRange> {
    crate::lang::for_file(db, file)?.declaration_name_range(db, file, item)
}

/// The declared-name range of a *Kotlin* declaration.
pub(crate) fn kotlin_declaration_name_range(
    db: &RootDatabase,
    file: FileId,
    item: hir::hir_def::jvm::ids::ItemId,
) -> Option<TextRange> {
    Ctx::new(db, file)?.name_range(item)
}

/// The identifier token written at `offset`.
///
/// `token_at_offset` answers `Between` for an offset at a token boundary — a
/// client puts the cursor at a token's *first* byte — so the token that starts
/// there is the one the offset is on; both sides are identifiers only when the
/// offset is at the *end* of one, where the left side wins.
fn identifier_at(ctx: &Ctx, offset: TextSize) -> Option<rowan::SyntaxToken<Lang>> {
    use rowan::TokenAtOffset;
    let token = match ctx.root.token_at_offset(offset) {
        TokenAtOffset::Single(token) => token,
        TokenAtOffset::Between(left, right) => {
            if right.text_range().start() == offset {
                right
            } else {
                left
            }
        }
        TokenAtOffset::None => return None,
    };
    (token.kind() == K::IDENTIFIER).then_some(token)
}

/// What the identifier token at the offset is written in.
enum Site {
    /// An import's path: the qualified name it imports. A star import's path
    /// names a classifier or a package, and only the former is a declaration,
    /// so the two need no distinction here.
    Import { name: String },
    /// A name of a declaration's type.
    Type(String),
    /// A declaration's own name.
    Declaration,
    /// Anything else — a body value, an annotation argument, a modifier.
    Other,
}

fn site_of(token: &rowan::SyntaxToken<Lang>) -> Site {
    for node in token.parent_ancestors() {
        match node.kind() {
            K::IMPORT_PATH => {
                // The imported name is the path's `QUALIFIED_NAME` child; the
                // path node itself holds only that node and the `.*` of a star
                // import.
                let name = node
                    .children()
                    .find(|child| child.kind() == K::QUALIFIED_NAME)
                    .map(|qualified| user_type_name(&qualified))
                    .unwrap_or_default();
                return Site::Import { name };
            }
            K::USER_TYPE => {
                let name = user_type_name(&node);
                return Site::Type(name);
            }
            K::CLASS_DECL
            | K::OBJECT_DECL
            | K::COMPANION_OBJECT
            | K::FUNCTION_DECL
            | K::PROPERTY_DECL
            | K::CLASS_PARAMETER
            | K::VALUE_PARAMETER
            | K::TYPE_ALIAS
            | K::ENUM_ENTRY
            | K::PRIMARY_CONSTRUCTOR
            | K::SECONDARY_CONSTRUCTOR
            | K::GETTER
            | K::SETTER
            | K::ANONYMOUS_INITIALIZER => return Site::Declaration,
            _ => {}
        }
    }
    Site::Other
}

/// The dotted name of a `USER_TYPE` node, type arguments excluded.
fn user_type_name(node: &rowan::SyntaxNode<Lang>) -> String {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|token| token.kind() == K::IDENTIFIER)
        .map(|token| token.text().to_owned())
        .collect::<Vec<_>>()
        .join(".")
}

/// The file's name-resolution scope ([KLS
/// `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)).
struct Scope {
    package: Option<String>,
    imports: Vec<KotlinImportItem>,
    /// The enclosing classifiers, innermost first: their names (for the FQN a
    /// nested classifier is reached through) and their declared type
    /// parameters.
    enclosing: Vec<EnclosingDecl>,
}

struct EnclosingDecl {
    name: String,
    type_params: Vec<(String, TextRange)>,
}

impl Scope {
    fn of(ctx: &Ctx, token: &rowan::SyntaxToken<Lang>) -> Option<Scope> {
        let mut enclosing = Vec::new();
        for node in token.parent_ancestors() {
            if !matches!(
                node.kind(),
                K::CLASS_DECL | K::OBJECT_DECL | K::COMPANION_OBJECT
            ) {
                continue;
            }
            let name = node
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .skip_while(|token| {
                    !matches!(token.kind(), K::CLASS_KW | K::INTERFACE_KW | K::OBJECT_KW)
                })
                .skip(1)
                .find(|token| token.kind() == K::IDENTIFIER)
                .map(|token| token.text().to_owned())?;
            let type_params = node
                .children()
                .filter(|child| child.kind() == K::TYPE_PARAMETERS)
                .flat_map(|params| params.children().collect::<Vec<_>>())
                .filter(|param| param.kind() == K::TYPE_PARAMETER)
                .filter_map(|param| {
                    let name = param
                        .children_with_tokens()
                        .filter_map(NodeOrToken::into_token)
                        .find(|token| {
                            token.kind() == K::IDENTIFIER
                                && !matches!(token.text(), "in" | "out" | "reified")
                        })?;
                    Some((name.text().to_owned(), name.text_range()))
                })
                .collect();
            enclosing.push(EnclosingDecl { name, type_params });
        }
        Some(Scope {
            package: ctx
                .tree
                .package
                .as_ref()
                .map(|name| name.as_str().to_owned()),
            imports: ctx.tree.imports.clone(),
            enclosing,
        })
    }

    /// The declared type parameter `name` names, innermost first.
    fn type_param(&self, name: &str) -> Option<(&str, TextRange)> {
        self.enclosing.iter().find_map(|decl| {
            decl.type_params
                .iter()
                .find(|(param, _)| param == name)
                .map(|(param, range)| (param.as_str(), *range))
        })
    }

    /// The candidate qualified names of the written type name `name`, in
    /// resolution order.
    fn candidates(&self, name: &str) -> Vec<String> {
        let segments: Vec<&str> = name.split('.').collect();
        let simple = segments[0];
        let mut out = Vec::new();

        if segments.len() == 1 {
            // An explicit import binds the name, by its alias or by its last
            // segment ([KLS
            // `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)).
            for import in &self.imports {
                if import.is_asterisk {
                    continue;
                }
                let bound = import
                    .alias
                    .as_ref()
                    .map(|alias| alias.as_str().to_owned())
                    .unwrap_or_else(|| simple_name(import.path.as_str()));
                if bound == simple {
                    out.push(import.path.as_str().to_owned());
                }
            }
        } else if let Some(import) = self.imports.iter().find(|import| {
            !import.is_asterisk
                && import
                    .alias
                    .as_ref()
                    .is_some_and(|alias| alias.as_str() == simple)
        }) {
            // A qualified name whose first segment is an imported alias.
            out.push(format!("{}.{}", import.path, segments[1..].join(".")));
        }

        // The enclosing classifiers, innermost first: a nested classifier is
        // reached through its enclosing classifier, and a classifier through
        // its own name.
        for (index, _) in self.enclosing.iter().enumerate() {
            let prefix: Vec<&str> = self.enclosing[index..]
                .iter()
                .rev()
                .map(|decl| decl.name.as_str())
                .collect();
            let prefix = prefix.join(".");
            if segments.len() == 1 {
                out.push(format!("{}.{}", prefix, simple));
            }
            out.push(format!("{}.{}", prefix, name));
        }

        // The file's own package, then the star imports.
        let in_package = |fqn: &str| match &self.package {
            Some(package) => format!("{package}.{fqn}"),
            None => fqn.to_owned(),
        };
        out.push(in_package(name));
        for import in self.imports.iter().filter(|import| import.is_asterisk) {
            out.push(format!("{}.{}", import.path, name));
        }
        out.dedup();
        out
    }
}

/// The source sets of the project graph, in registration order.
fn registered_source_sets(db: &RootDatabase) -> Vec<hir::SourceSetId> {
    hir::project_graph(db)
        .map(|graph| graph.source_sets(db).keys().cloned().collect())
        .unwrap_or_default()
}

fn simple_name(fqn: &str) -> String {
    fqn.rsplit('.').next().unwrap_or(fqn).to_owned()
}
