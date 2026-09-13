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

use hir_expand::body::{BodyTree, ExprData, ExprId, LocalId, StmtData, UnaryOp};
use hir_ty::{BodyTypes, BoundKind, MethodData, ResolvedMember, Ty, TyDatabase, TyKind};
use rowan::{SyntaxNode, TextRange, TextSize};
use rustc_hash::FxHashSet;
use syntax::SourceFile;
use syntax::java::Lang;
use vfs::FileId;

use super::{
    InlayHint, InlayHintDetail, InlayHintEdit, InlayHintKind, InlayHintLabelPart, InlayHintsConfig,
};
use crate::RootDatabase;
use crate::nav;
use ide_db::base_db::{LanguageKind, SourceDatabase};

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
    let mut pending = Vec::new();
    collect(
        db,
        file,
        Search::Range(range),
        config,
        &mut out,
        &mut pending,
    );
    let mut hints: Vec<InlayHint> = out.into_iter().map(|detail| detail.hint).collect();
    hints.sort_by_key(|hint| hint.offset);
    hints
}

/// The library files the hints over `range` need loaded before their parameter
/// names can be rendered: the declaring sources of the library members their
/// invocations selected, where the source is in the library's archive but not
/// materialized. The LSP layer materializes them and re-runs the request —
/// exactly the deferral goto-definition and hover drive — so a library member's
/// names render on the first request instead of only once its source happens to
/// be open.
///
/// Empty for a request the parameter-name category is off for, and for the
/// calls whose arguments would render no hint anyway.
pub(super) fn pending_library_files(
    db: &RootDatabase,
    file: FileId,
    range: TextRange,
    config: &InlayHintsConfig,
) -> Vec<nav::LibraryFileRef> {
    let mut out = Vec::new();
    let mut pending = Vec::new();
    collect(
        db,
        file,
        Search::Range(range),
        config,
        &mut out,
        &mut pending,
    );
    pending
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
    let mut pending = Vec::new();
    collect(
        db,
        file,
        Search::At { offset, kind },
        config,
        &mut out,
        &mut pending,
    );
    out.into_iter()
        .find(|detail| detail.hint.offset == offset && detail.hint.kind == kind)
}

/// Runs every collector over the file's body-carrying items, keeping the hints
/// `search` asks for and recording the library files a parameter-name hint
/// needs loaded first.
fn collect(
    db: &RootDatabase,
    file: FileId,
    search: Search,
    config: &InlayHintsConfig,
    out: &mut Vec<InlayHintDetail>,
    pending: &mut Vec<nav::LibraryFileRef>,
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
    // The locals a declaration form writes an initializer for: one pass over
    // the statement arena for the whole request, not one per item.
    let inits = initialized_locals(&bodies);
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
        parameter_name_hints(db, file, &bodies, &types, &search, config, out, pending);
        method_chain_hints(db, file, &bodies, &types, &search, config, out);
    }
}

// -- inferred `var` types -----------------------------------------------------------

/// The locals a declaration form writes an *initializer* for: a local variable
/// declaration ([JLS §14.4]), an enhanced-`for` variable ([§14.14.2]) and a
/// resource ([§14.20.3]). Every declarator of a multi-declarator statement is a
/// [`StmtData::Decl`] of the same arena ([`StmtData::DeclGroup`]'s members are
/// ordinary entries), so it is covered too.
///
/// This is what tells a `var` declaration from every other [`Local`]: the three
/// forms are the ones whose type the lowerer may leave absent for the type
/// layer to infer, while a pattern binding states its type in the pattern
/// ([§14.30.1]), a parameter states it in the signature and a catch parameter
/// in the clause.
fn initialized_locals(bodies: &BodyTree) -> FxHashSet<LocalId> {
    let mut out = FxHashSet::default();
    for (_, stmt) in bodies.stmts.iter() {
        match stmt {
            StmtData::Decl {
                local,
                initializer: Some(_),
            } => {
                out.insert(*local);
            }
            StmtData::ForEach { var, .. } => {
                out.insert(*var);
            }
            StmtData::Try { resources, .. } => {
                for resource in resources {
                    if resource.initializer.is_some() {
                        out.insert(resource.local);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The inferred type of every `var` local of the body, rendered after the
/// declaration's name (`var x: List<String> = ...`).
///
/// Every initializer form is hinted, a *self-describing* one (`var s = "x"`,
/// `var o = new Foo()`) included: the hint states the type the compiler
/// inferred, which is what a reader asks it for — a client that finds the
/// redundant cases noisy turns the category off. (IntelliJ's Java provider
/// skips those four initializer shapes; this feature deliberately does not.)
#[allow(clippy::too_many_arguments)]
fn var_type_hints(
    db: &dyn TyDatabase,
    source: &SyntaxNode<Lang>,
    bodies: &BodyTree,
    inits: &FxHashSet<LocalId>,
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
        // Only a declaration form that writes an initializer can be a `var`
        // declaration: the type layer infers its type *from* that initializer.
        if !inits.contains(&local_id) || !is_renderable(db, ty) {
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
    db: &dyn TyDatabase,
    file: FileId,
    source: &SyntaxNode<Lang>,
    bodies: &BodyTree,
    types: &BodyTypes,
    search: &Search,
    config: &InlayHintsConfig,
    out: &mut Vec<InlayHintDetail>,
) {
    if !config.lambda_parameter_types {
        return;
    }
    let scope = hir_ty::scope_for_file(db, file);
    // The expression arena is in lowering order, so the hints come out in
    // source order without a hash iteration order to sort out afterwards.
    for (id, expr) in bodies.exprs.iter() {
        let ExprData::Lambda { params, .. } = expr else {
            continue;
        };
        // The type layer stores the lambda's *target* functional interface
        // ([§15.27.3]) in the expression's type slot.
        let Some(target) = types.exprs.get(&ExprId(id)) else {
            continue;
        };
        if !is_renderable(db, target) {
            continue;
        }
        // §9.8/[§15.27.3]: the parameter types are the single abstract
        // method's, and a lambda whose parameter count already disagrees with
        // the SAM reports a diagnostic instead of guessing.
        let Some(sam) = hir_ty::single_abstract_method(db, &scope, target) else {
            continue;
        };
        if sam.params.len() != params.len() {
            continue;
        }
        for (param, formal) in params.iter().zip(&sam.params) {
            // A parameter that writes its type states it itself
            // ([JLS §15.27.1]); only an inferred one is hinted.
            if param.ty.is_some() {
                continue;
            }
            let ty = hir_ty::inferred_lambda_parameter_ty(db, formal);
            if !is_renderable(db, &ty) {
                continue;
            }
            let offset = param.range.start();
            if !search.matches_hint(offset, InlayHintKind::Type) {
                continue;
            }
            let mut label = Vec::new();
            push_type_label(db, &ty, &mut label);
            let canonical = ty.display(db).to_string();
            // A `(var x)` parameter replaces its `var` token; a concise `(x)`
            // parameter gets the type inserted before the name.
            let edits = match var_keyword_range(source, param.range) {
                Some(range) => vec![InlayHintEdit {
                    range,
                    new_text: canonical.clone(),
                }],
                None => vec![InlayHintEdit {
                    range: TextRange::empty(offset),
                    new_text: format!("{canonical} "),
                }],
            };
            out.push(InlayHintDetail {
                hint: InlayHint {
                    offset,
                    label,
                    kind: InlayHintKind::Type,
                    padding_left: false,
                    padding_right: true,
                },
                tooltip: canonical,
                edits,
            });
        }
    }
}

// -- method parameter names ----------------------------------------------------------

/// A method's parameter names at the arguments it is passed, where the argument
/// does not already name it (`foo(size: 3)`).
///
/// A library member whose declaring source is not loaded yet is not hinted —
/// instead its file is recorded in `pending`, which a caller turns into the
/// same deferral goto-definition and hover drive (see
/// [`pending_library_files`]).
#[allow(clippy::too_many_arguments)]
fn parameter_name_hints(
    db: &RootDatabase,
    file: FileId,
    bodies: &BodyTree,
    types: &BodyTypes,
    search: &Search,
    config: &InlayHintsConfig,
    out: &mut Vec<InlayHintDetail>,
    pending: &mut Vec<nav::LibraryFileRef>,
) {
    if !config.parameter_names {
        return;
    }
    // The expression arena is in lowering order, so the hints come out in
    // source order without a hash iteration order to sort out afterwards.
    for (id, expr) in bodies.exprs.iter() {
        let expr_id = ExprId(id);
        // The calls a parameter list belongs to: an invocation, a class
        // instance creation ([JLS §15.9]) and an explicit constructor
        // invocation ([§8.8.7.1]) — IntelliJ's `PsiCall` set.
        let (args, constructor): (&[ExprId], bool) = match expr {
            ExprData::MethodCall { args, .. } => (args, false),
            ExprData::New { args, .. } => (args, true),
            ExprData::CtorCall { args, .. } => (args, true),
            _ => continue,
        };
        // The *declaration form* the invocation selected. An unresolved or
        // ambiguous invocation names no single declaration to read names from.
        let Some(ResolvedMember::Method(method)) = types.resolved.get(&expr_id) else {
            continue;
        };
        // A call that renders no hint whatever the parameter names are never
        // needs a declaring source materialized: no parameter to name, an
        // argument the parser could not lower (error recovery), an argument
        // list the parameters cannot absorb, or arguments that all speak for
        // themselves — in a well-typed call an argument of any other shape was
        // inferred *against* the parameter it was selected for, so its type
        // already names it. This runs before the names are consulted so the
        // pending set names only the sources a hint actually waits on.
        if method.params.is_empty()
            || args
                .iter()
                .any(|arg| matches!(bodies.expr(*arg), ExprData::Missing))
            || (!method.varargs && args.len() > method.params.len())
            || !args
                .iter()
                .any(|&arg| is_unclear_argument(bodies, types, arg))
        {
            continue;
        }
        // §8.4.1: the names are the selected declaration's own. A source
        // declaration carries them in its item tree; a *library* member records
        // none (a classfile writes them only in a `MethodParameters` attribute
        // this server does not read), so they are read back from its declaring
        // source when that is loaded. A source that is not loaded yet is
        // recorded as pending, for the caller to materialize; a library that
        // ships none, a decompiled declaring view, or a synthesized implicit
        // member gets no hint rather than an invented `arg0`-style name
        // ([`nav::declared_parameter_names`], [`nav::pending_parameter_names`]).
        // A name list that does not line up with the parameter list is equally
        // unusable.
        let names: Vec<String> = match method.param_names.clone() {
            Some(names) => names,
            None => match nav::declared_parameter_names(db, file, method, constructor) {
                Some(names) => names,
                None => {
                    if let Some(pending_file) =
                        nav::pending_parameter_names(db, file, method, constructor)
                        && !pending.contains(&pending_file)
                    {
                        pending.push(pending_file);
                    }
                    continue;
                }
            },
        };
        let names = names.as_slice();
        if names.len() != method.params.len() {
            continue;
        }
        if names_say_nothing(method, names) {
            continue;
        }
        // §8.4.1: the varargs formal is the *array* of its element, so it is
        // the last parameter and the arguments from its index on are its
        // elements.
        let regular = if method.varargs {
            method.params.len() - 1
        } else {
            method.params.len()
        };
        for (index, name) in names.iter().enumerate().take(regular) {
            let Some(&arg) = args.get(index) else {
                continue;
            };
            if argument_names_parameter(bodies, name, arg)
                || !is_unclear_argument(bodies, types, arg)
            {
                continue;
            }
            push_parameter_hint(db, method, search, bodies, arg, format!("{name}:"), out);
        }
        // The trailing elements of a varargs call get *one* hint for the
        // whole group, at the first of them.
        if method.varargs && args.len() > regular {
            let trailing = &args[regular..];
            if trailing
                .iter()
                .any(|arg| is_unclear_argument(bodies, types, *arg))
                && let Some(&first) = trailing.first()
            {
                let name = &names[regular];
                push_parameter_hint(
                    db,
                    method,
                    search,
                    bodies,
                    first,
                    format!("...{name}:"),
                    out,
                );
            }
        }
    }
}

/// Whether the declaration's parameter names already say nothing worth
/// rendering: a lone parameter named after the method it belongs to
/// (`setName(String name)`), or a run of numbered names sharing one prefix
/// (`arg0, arg1`, `p1, p2, p3`).
fn names_say_nothing(method: &MethodData, names: &[String]) -> bool {
    if names.len() == 1
        && names[0].len() > 1
        && method
            .name
            .to_lowercase()
            .contains(&names[0].to_lowercase())
    {
        return true;
    }
    are_numbered_parameters(names)
}

/// Whether every name is `{prefix}{n}` for one shared prefix with consecutive
/// numbers starting at 0 or 1 — a naming scheme that describes position, not
/// role.
fn are_numbered_parameters(names: &[String]) -> bool {
    let mut prefix: Option<&str> = None;
    let mut previous: Option<u32> = None;
    for name in names {
        let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        // A name with no digits, or nothing but digits, is not this scheme.
        if digits == 0 || digits == name.len() {
            return false;
        }
        let (this_prefix, number) = name.split_at(name.len() - digits);
        let Ok(number) = number.parse::<u32>() else {
            return false;
        };
        match prefix {
            // The run starts at 0 or 1.
            None if number > 1 => return false,
            None => prefix = Some(this_prefix),
            Some(prefix) if prefix != this_prefix => return false,
            Some(_) if previous.is_some_and(|prev| number != prev + 1) => return false,
            Some(_) => {}
        }
        previous = Some(number);
    }
    true
}

/// Whether the argument already says the parameter's name — IntelliJ hides a
/// hint whose value names it itself. Only a bare variable reference or a
/// called method's name is read (never a field access), and a name shorter
/// than three characters is too short to mean anything either way.
fn argument_names_parameter(bodies: &BodyTree, parameter: &str, arg: ExprId) -> bool {
    let Some(argument) = (match bodies.expr(arg) {
        ExprData::Var(name) => Some(name.as_str()),
        ExprData::MethodCall { name, .. } => Some(name.as_str()),
        _ => None,
    }) else {
        return false;
    };
    let argument = argument.to_lowercase();
    let parameter = parameter.to_lowercase();
    argument.len() >= 3
        && parameter.len() >= 3
        && (argument.contains(&parameter) || parameter.contains(&argument))
}

/// Whether an argument is one whose purpose a reader cannot infer from the
/// value alone — IntelliJ's `shouldShowHintsForExpression`: a literal, `null`,
/// `this`, a polyadic expression, a signed numeric literal, or
/// `java.util.Optional.empty()`.
///
/// The complement — an argument of any other shape — gets no hint: in a
/// well-typed call it was inferred *against* the parameter it was selected
/// for, so its type already names it.
fn is_unclear_argument(bodies: &BodyTree, types: &BodyTypes, arg: ExprId) -> bool {
    match bodies.expr(arg) {
        ExprData::Literal(_) | ExprData::Null | ExprData::This { .. } | ExprData::Binary { .. } => {
            true
        }
        ExprData::Unary {
            op: UnaryOp::Plus | UnaryOp::Minus,
            expr,
        } => matches!(bodies.expr(*expr), ExprData::Literal(_)),
        ExprData::MethodCall { name, args, .. } => {
            name.as_str() == "empty"
                && args.is_empty()
                && matches!(
                    types.resolved.get(&arg),
                    Some(ResolvedMember::Method(method))
                        if method.name == "empty"
                            && method
                                .owner
                                .as_fqn()
                                .is_some_and(|fqn| fqn.as_str() == "java.util.Optional")
                )
        }
        _ => false,
    }
}

/// Records one parameter-name hint at the argument's own start
/// (`foo(size: 3)`).
fn push_parameter_hint(
    db: &dyn TyDatabase,
    method: &MethodData,
    search: &Search,
    bodies: &BodyTree,
    arg: ExprId,
    value: String,
    out: &mut Vec<InlayHintDetail>,
) {
    let Some(range) = bodies.expr_range(arg) else {
        return;
    };
    let offset = range.start();
    if !search.matches_hint(offset, InlayHintKind::Parameter) {
        return;
    }
    out.push(InlayHintDetail {
        hint: InlayHint {
            offset,
            label: vec![part(value)],
            kind: InlayHintKind::Parameter,
            padding_left: false,
            padding_right: true,
        },
        tooltip: method.display(db).to_string(),
        edits: Vec::new(),
    });
}

// -- method chain types ---------------------------------------------------------------

/// The fewest distinct types a chain must produce before any of its calls is
/// annotated — IntelliJ's Java default.
const MIN_CHAIN_UNIQUE_TYPES: usize = 2;

/// The type of each call of a multi-line method chain whose type changes
/// (`list.stream()` \n `.filter(...)` \n `.map(...)`).
///
/// The whole chain's own type is what the expression already reads as, so the
/// *outermost* call is never annotated (IntelliJ: "except last to avoid
/// `builder.build()` which has obvious type"), and a run of calls returning the
/// same type is annotated once, at its innermost element.
fn method_chain_hints(
    db: &RootDatabase,
    file: FileId,
    bodies: &BodyTree,
    types: &BodyTypes,
    search: &Search,
    config: &InlayHintsConfig,
    out: &mut Vec<InlayHintDetail>,
) {
    if !config.method_chains {
        return;
    }
    // The item's method calls, in lowering order.
    let calls: Vec<ExprId> = bodies
        .exprs
        .iter()
        .map(|(id, _)| ExprId(id))
        .filter(|expr| matches!(bodies.expr(*expr), ExprData::MethodCall { .. }))
        .collect();
    // The calls that are another call's receiver. A *topmost* call — one no
    // other call receives — is where a chain is read from, outermost first.
    let mut receivers: FxHashSet<ExprId> = FxHashSet::default();
    for &call in &calls {
        if let ExprData::MethodCall { receiver, .. } = bodies.expr(call)
            && let Some(inner) = call_receiver(bodies, *receiver)
        {
            receivers.insert(inner);
        }
    }
    // The file's text, for the line-break test below: the ranges are the only
    // syntax↔HIR bridge, so whether a break follows one is read there.
    let text = db.file_text(file).text(db).to_owned();
    for &top in &calls {
        if receivers.contains(&top) {
            continue;
        }
        // A chain never crosses items: every receiver of a call is an
        // expression of the same body.
        let mut chain = vec![top];
        while let ExprData::MethodCall { receiver, .. } =
            bodies.expr(*chain.last().expect("non-empty"))
            && let Some(inner) = call_receiver(bodies, *receiver)
        {
            chain.push(inner);
        }
        chain_hints(db, &text, bodies, types, search, &chain, out);
    }
}

/// The call an expression is the receiver of: peels the parentheses and
/// postfix operators IntelliJ's `skipParenthesesAndPostfixOperatorsDown` does,
/// and answers the peeled expression when it is a method call.
fn call_receiver(bodies: &BodyTree, receiver: Option<ExprId>) -> Option<ExprId> {
    let mut current = receiver?;
    loop {
        match bodies.expr(current) {
            ExprData::Paren(inner) | ExprData::Postfix { expr: inner, .. } => current = *inner,
            ExprData::MethodCall { .. } => return Some(current),
            _ => return None,
        }
    }
}

/// Whether a line break directly follows `end` in `text`: the element stands on
/// a line of its own, which is what makes a call a *link* of a chain to a
/// reader (IntelliJ's `nextSibling` whitespace test). The run of whitespace is
/// what is inspected, so the next non-whitespace character never has to be
/// found.
fn line_break_after(text: &str, end: TextSize) -> bool {
    text.get(end.into()..).is_some_and(|rest| {
        rest.chars()
            .take_while(|c| c.is_whitespace())
            .any(|c| c == '\n')
    })
}

/// The hints of one chain, which is `[outermost, …, innermost]`.
fn chain_hints(
    db: &dyn TyDatabase,
    text: &str,
    bodies: &BodyTree,
    types: &BodyTypes,
    search: &Search,
    chain: &[ExprId],
    out: &mut Vec<InlayHintDetail>,
) {
    // The outermost call is dropped: its type is the expression's own.
    let mut values: Vec<ExprId> = chain[1..]
        .iter()
        .copied()
        .filter(|element| {
            bodies
                .expr_range(*element)
                .is_some_and(|range| line_break_after(text, range.end()))
        })
        .collect();
    // `takeWhile`: a call whose type the layer does not know — or cannot
    // render — ends the chain, and every call inside it with it.
    let renderable = values
        .iter()
        .take_while(|element| {
            types
                .exprs
                .get(element)
                .is_some_and(|ty| is_renderable(db, ty))
        })
        .count();
    values.truncate(renderable);
    let mut distinct: FxHashSet<Ty> = FxHashSet::default();
    for element in &values {
        distinct.insert(types.exprs[element]);
    }
    // A chain that never changes type has nothing to say.
    if distinct.len() < MIN_CHAIN_UNIQUE_TYPES {
        return;
    }
    // Sources order: the decided set is read outermost first, so it is walked
    // backwards.
    for (index, &element) in values.iter().enumerate().rev() {
        let ty = types.exprs[&element];
        // A run of calls returning the same type is annotated once, at its
        // innermost element.
        if let Some(next) = values.get(index + 1)
            && types.exprs[next] == ty
        {
            continue;
        }
        let Some(range) = bodies.expr_range(element) else {
            continue;
        };
        let offset = range.end();
        if !search.matches_hint(offset, InlayHintKind::Type) {
            continue;
        }
        let mut label = Vec::new();
        push_type_label(db, &ty, &mut label);
        out.push(InlayHintDetail {
            hint: InlayHint {
                offset,
                label,
                kind: InlayHintKind::Type,
                padding_left: true,
                padding_right: false,
            },
            tooltip: ty.display(db).to_string(),
            edits: Vec::new(),
        });
    }
}
