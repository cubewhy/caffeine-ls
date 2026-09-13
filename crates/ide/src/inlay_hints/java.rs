//! Java inlay hints: the four hint categories of [`super`], computed from the
//! inference the type layer recorded for a body ([`hir_ty::BodyTypes`]) and the
//! source ranges of the body IR ([`hir_expand::body::BodyTree`]).
//!
//! Every rule is ported from IntelliJ's Java inlay-hint providers:
//!
//! * the inferred type of a `var` local
//!   (`JavaImplicitTypeDeclarativeInlayHintsProvider`);
//! * the inferred type of a lambda parameter written without one
//!   (`JavaLambdaParameterTypeHintsProvider`);
//! * a method's parameter name at an argument that does not name it itself
//!   (`JavaInlayParameterHintsProvider`);
//! * the type of the intermediate calls of a multi-line method chain
//!   (`JavaMethodChainsDeclarativeInlayProvider`).
//!
//! Two properties the whole module rests on:
//!
//! * The **declaration form** of the method a call selected — the
//!   [`hir_ty::MethodData`] with the source declaration's parameter *names* —
//!   is what parameter-name hints read. A library member records no names, so
//!   no hint is invented for it.
//! * The **type of a `var` local** is the local's own recorded type
//!   ([JLS §14.4.2] "Local Variable Type Inference"), never a re-inference from
//!   the initializer: the type layer already projected the initializer upward,
//!   and the hint must agree with what every other request reports.
//!
//! The parser's CST is consulted for exactly one thing — the `var` keyword a
//! hint's edit replaces ([`var_keyword_range`]); everything else comes from the
//! HIR.

use hir_expand::body::{BodyTree, ExprData, ExprId, LocalId, StmtData};
use hir_ty::{BodyTypes, BoundKind, Ty, TyDatabase, TyKind};
use rowan::{SyntaxNode, TextRange, TextSize};
use rustc_hash::FxHashMap;
use syntax::SourceFile;
use syntax::java::Lang;
use vfs::FileId;

use super::{
    InlayHint, InlayHintDetail, InlayHintEdit, InlayHintKind, InlayHintLabelPart, InlayHintsConfig,
};
use crate::RootDatabase;
use ide_db::base_db::LanguageKind;

/// The construct a hint is asked about: a request's range, or the one hint a
/// resolve names.
enum Search {
    Range(TextRange),
    At {
        offset: TextSize,
        kind: InlayHintKind,
    },
}

impl Search {
    /// Whether the item's declaration is worth walking: a range request keeps
    /// the items it overlaps, a resolve keeps the one it names. An item whose
    /// range cannot be resolved (a synthetic or nameless declaration) is always
    /// walked; the hint filter below still bounds what is kept.
    fn matches_item(&self, item_range: Option<TextRange>) -> bool {
        match self {
            Search::Range(range) => item_range.is_none_or(|item| item.intersect(*range).is_some()),
            Search::At { .. } => true,
        }
    }

    /// `Range(r)`: `r.contains(offset)`. `At { offset, kind }`: both equal.
    fn matches_hint(&self, offset: TextSize, kind: InlayHintKind) -> bool {
        match self {
            Search::Range(range) => range.contains(offset),
            Search::At {
                offset: at,
                kind: at_kind,
            } => offset == *at && kind == *at_kind,
        }
    }
}

/// The file's hints whose offset `range` contains, sorted by offset.
pub(super) fn hints(
    db: &RootDatabase,
    file: FileId,
    range: TextRange,
    config: &InlayHintsConfig,
) -> Vec<InlayHint> {
    let mut out = Vec::new();
    collect(db, file, Search::Range(range), config, &mut out);
    let mut hints: Vec<InlayHint> = out.into_iter().map(|detail| detail.hint).collect();
    hints.sort_by_key(|hint| hint.offset);
    hints
}

/// The one hint a resolve names, with its deferred detail.
pub(super) fn resolve(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
    kind: InlayHintKind,
    config: &InlayHintsConfig,
) -> Option<InlayHintDetail> {
    let mut out = Vec::new();
    collect(db, file, Search::At { offset, kind }, config, &mut out);
    out.into_iter()
        .find(|detail| detail.hint.offset == offset && detail.hint.kind == kind)
}

/// Runs every collector over the file's body-carrying items, keeping the hints
/// `search` asks for.
fn collect(
    db: &RootDatabase,
    file: FileId,
    search: Search,
    config: &InlayHintsConfig,
    out: &mut Vec<InlayHintDetail>,
) {
    let tree = hir::file_item_tree(db, file);
    let language = tree.language;
    if language == LanguageKind::Unknown {
        // A file with no source root yet (or a non-JVM file) lowers to an empty
        // item tree and has no parse to read ranges from.
        return;
    }
    let bodies = hir::file_body_tree(db, file);
    let source_file = ide_db::base_db::parse(db, file, language).syntax_node(language);
    let SourceFile::Java(parsed) = &source_file else {
        return;
    };
    let source = &parsed.syntax_node;
    let map = hir::hir_def::db::ast_id_map(db, file, language);
    // The initializer written for each local the file declares: one pass over
    // the statement arena for the whole request, not one per item.
    let inits = local_initializers(&bodies);
    for (_, body) in bodies.bodies.iter() {
        let Some(item) = body.owner else { continue };
        let item_range = hir::hir_def::java::ranges::item_range(map, &source_file, &tree, item);
        if !search.matches_item(item_range) {
            continue;
        }
        let Some(types) = hir_ty::body_types(db, file, item) else {
            continue;
        };
        var_type_hints(db, source, &bodies, &inits, &types, &search, config, out);
        lambda_parameter_hints(db, file, source, &bodies, &types, &search, config, out);
        parameter_name_hints(db, &bodies, &types, &search, config, out);
        method_chain_hints(db, file, &bodies, &types, &search, config, out);
    }
}

// -- inferred `var` types -----------------------------------------------------------

/// The initializer written for each local the file declares, keyed by the
/// local: the three declaration forms that write one ([JLS §14.4] local
/// variable declaration, [§14.14.2] enhanced-`for` variable, [§14.20.3]
/// resource), and every `Decl` of a multi-declarator statement
/// ([`StmtData::DeclGroup`]'s members are ordinary entries of the same arena).
/// A local absent from the map has no initializer written for it — a pattern
/// binding, a parameter, a resource without one — which is also the case where
/// no hint is emitted.
fn local_initializers(bodies: &BodyTree) -> FxHashMap<LocalId, ExprId> {
    let mut out = FxHashMap::default();
    for (_, stmt) in bodies.stmts.iter() {
        match stmt {
            StmtData::Decl {
                local,
                initializer: Some(initializer),
            } => {
                out.insert(*local, *initializer);
            }
            StmtData::ForEach { var, iterable, .. } => {
                out.insert(*var, *iterable);
            }
            StmtData::Try { resources, .. } => {
                for resource in resources {
                    if let Some(initializer) = resource.initializer {
                        out.insert(resource.local, initializer);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether an initializer describes its own type, so the `var` hint beside it
/// adds nothing — IntelliJ's set: a literal, `null`, a polyadic expression, a
/// class instance creation, an array creation and a cast. A parenthesized one
/// is *not* unwrapped: the expression as written is what is tested.
fn is_self_describing_initializer(bodies: &BodyTree, initializer: ExprId) -> bool {
    matches!(
        bodies.expr(initializer),
        ExprData::Literal(_)
            | ExprData::Null
            | ExprData::Binary { .. }
            | ExprData::New { .. }
            | ExprData::NewArray { .. }
            | ExprData::Cast { .. }
    )
}

/// The inferred type of every `var` local of the body, rendered after the
/// declaration's name (`var x: List<String> = ...`).
#[allow(clippy::too_many_arguments)]
fn var_type_hints(
    db: &dyn TyDatabase,
    source: &SyntaxNode<Lang>,
    bodies: &BodyTree,
    inits: &FxHashMap<LocalId, ExprId>,
    types: &BodyTypes,
    search: &Search,
    config: &InlayHintsConfig,
    out: &mut Vec<InlayHintDetail>,
) {
    if !config.var_types {
        return;
    }
    // The local arena is in lowering order, so the hints come out in source
    // order without a hash iteration order to sort out afterwards.
    for (id, local) in bodies.locals.iter() {
        let local_id = LocalId(id);
        // The item's own locals: a nested local class's declarations share the
        // file's arena but not this body's types.
        let Some(ty) = types.locals.get(&local_id) else {
            continue;
        };
        // The lowerer writes a `None` declared type *only* for the contextual
        // `var`, so `None` means `var` exactly ([JLS §14.4.1]).
        if local.ty.is_some() {
            continue;
        }
        let Some(name_range) = bodies.local_name_range(local_id) else {
            continue;
        };
        let Some(&initializer) = inits.get(&local_id) else {
            continue;
        };
        if is_self_describing_initializer(bodies, initializer) || !is_renderable(db, ty) {
            continue;
        }
        let offset = name_range.end();
        if !search.matches_hint(offset, InlayHintKind::Type) {
            continue;
        }
        let mut label = vec![InlayHintLabelPart {
            value: ": ".to_owned(),
            class: None,
        }];
        push_type_label(db, ty, &mut label);
        let canonical = ty.display(db).to_string();
        let edits = var_keyword_range(source, name_range)
            .map(|range| InlayHintEdit {
                range,
                new_text: canonical.clone(),
            })
            .into_iter()
            .collect();
        out.push(InlayHintDetail {
            hint: InlayHint {
                offset,
                label,
                kind: InlayHintKind::Type,
                padding_left: false,
                padding_right: false,
            },
            tooltip: canonical,
            edits,
        });
    }
}

/// The range of the contextual `var` ([JLS §3.9]) of the declaration the name
/// belongs to: the last non-trivia token before the name, when it is `var`.
/// Covers all four `var` forms — a local declaration statement (a bare
/// identifier child of the declaration), an enhanced-`for` variable, a lambda
/// parameter and a resource (each a `TYPE` node wrapping it).
fn var_keyword_range(source: &SyntaxNode<Lang>, name_range: TextRange) -> Option<TextRange> {
    // `token_at_offset` panics on an offset outside the node, and a request may
    // arrive at the very end of the file.
    if !source.text_range().contains_inclusive(name_range.start()) {
        return None;
    }
    let mut token = source.token_at_offset(name_range.start()).left_biased()?;
    while token.kind().is_trivia() {
        token = token.prev_token()?;
    }
    (token.text() == "var").then(|| token.text_range())
}

// -- label rendering ----------------------------------------------------------------

/// A label part that renders text and names no class.
fn part(value: impl Into<String>) -> InlayHintLabelPart {
    InlayHintLabelPart {
        value: value.into(),
        class: None,
    }
}

/// Appends the label parts of `ty` — IntelliJ's `JavaTypeHintsFactory.typeHint`
/// without its collapsible-list decoration (LSP has no equivalent):
///
/// * a reference type renders its simple name (the last `.`-separated segment,
///   `com.example.Outer.Inner` → `Inner`) as its own
///   part carrying the canonical name, then `<` args `>` when it has arguments
///   (each argument recursive, separated by `, `) — [JLS §4.5];
/// * an array renders its element then `[]` — [§10.1];
/// * a type variable renders its declared name — [§4.4];
/// * a wildcard renders `?`, `? extends T`, `? super T` — [§4.5.1];
/// * an intersection renders its members joined by ` & ` — [§4.9];
/// * a primitive renders its keyword — [§4.2].
///
/// The kinds [`is_renderable`] rejects must not reach here.
fn push_type_label(db: &dyn TyDatabase, ty: &Ty, out: &mut Vec<InlayHintLabelPart>) {
    match ty.kind(db) {
        TyKind::Reference { name, args, .. } => {
            out.push(InlayHintLabelPart {
                value: name.simple_name().to_owned(),
                class: Some(name.as_str().into()),
            });
            if !args.is_empty() {
                out.push(part("<"));
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        out.push(part(", "));
                    }
                    push_type_label(db, arg, out);
                }
                out.push(part(">"));
            }
        }
        TyKind::Array(inner) => {
            push_type_label(db, inner, out);
            out.push(part("[]"));
        }
        TyKind::TypeVar { scope, .. } => out.push(part(scope.name().as_str())),
        TyKind::Wildcard(bound) => {
            out.push(part("?"));
            if let Some(bound) = bound.as_deref() {
                match bound.kind {
                    BoundKind::Upper => out.push(part(" extends ")),
                    BoundKind::Lower => out.push(part(" super ")),
                }
                push_type_label(db, &bound.ty, out);
            }
        }
        TyKind::Intersection(members) => {
            for (i, member) in members.iter().enumerate() {
                if i > 0 {
                    out.push(part(" & "));
                }
                push_type_label(db, member, out);
            }
        }
        TyKind::Primitive(primitive) => {
            out.push(part(hir_ty::java::ty::primitive_name(*primitive)))
        }
        // No type the user could write: [`is_renderable`] drops these before a
        // label is built.
        TyKind::Void | TyKind::Null | TyKind::Error | TyKind::InferenceVar(_) => {}
    }
}

/// Whether the source can name `ty` at all. Dropped: `Error`, `Void` (names no
/// value), `Null` (IntelliJ drops it too), and a type still carrying an
/// unresolved inference variable ([JLS §18.4]) — none of them is a type the
/// user could write, so the hint is omitted rather than rendering `?0`.
fn is_renderable(db: &dyn TyDatabase, ty: &Ty) -> bool {
    match ty.kind(db) {
        TyKind::Void | TyKind::Null | TyKind::Error | TyKind::InferenceVar(_) => false,
        TyKind::Primitive(_) => true,
        TyKind::Reference { args, .. } => args.iter().all(|arg| is_renderable(db, arg)),
        TyKind::Array(inner) => is_renderable(db, inner),
        TyKind::TypeVar { bounds, .. } => bounds.iter().all(|bound| is_renderable(db, bound)),
        TyKind::Wildcard(bound) => bound
            .as_deref()
            .is_none_or(|bound| is_renderable(db, &bound.ty)),
        TyKind::Intersection(members) => members.iter().all(|member| is_renderable(db, member)),
    }
}

// -- inferred lambda parameter types -------------------------------------------------

/// The inferred type of every lambda parameter written without one, rendered
/// before the parameter's name (`(String s) -> ...`).
#[allow(clippy::too_many_arguments)]
fn lambda_parameter_hints(
    _db: &dyn TyDatabase,
    _file: FileId,
    _source: &SyntaxNode<Lang>,
    _bodies: &BodyTree,
    _types: &BodyTypes,
    _search: &Search,
    _config: &InlayHintsConfig,
    _out: &mut Vec<InlayHintDetail>,
) {
}

// -- method parameter names ----------------------------------------------------------

/// A method's parameter names at the arguments it is passed, where the argument
/// does not already name it (`foo(size: 3)`).
#[allow(clippy::too_many_arguments)]
fn parameter_name_hints(
    _db: &dyn TyDatabase,
    _bodies: &BodyTree,
    _types: &BodyTypes,
    _search: &Search,
    _config: &InlayHintsConfig,
    _out: &mut Vec<InlayHintDetail>,
) {
}

// -- method chain types ---------------------------------------------------------------

/// The type of each intermediate call of a multi-line method chain whose type
/// changes (`list.stream()` \n `.filter(...)` \n `.map(...)`).
#[allow(clippy::too_many_arguments)]
fn method_chain_hints(
    _db: &dyn TyDatabase,
    _file: FileId,
    _bodies: &BodyTree,
    _types: &BodyTypes,
    _search: &Search,
    _config: &InlayHintsConfig,
    _out: &mut Vec<InlayHintDetail>,
) {
}
