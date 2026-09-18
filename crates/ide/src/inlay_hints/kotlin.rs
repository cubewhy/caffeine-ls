//! Kotlin inlay hints.
//!
//! The two hints a Kotlin file answers:
//!
//! * the *inferred type* of a local that writes none — `val x = 1` renders
//!   `: Int` after the name ([KLS
//!   `type-inference.html#local-type-inference`](https://kotlinlang.org/spec/type-inference.html#local-type-inference)),
//!   which is what `kotlinc`'s probe reports as the expression's type
//!   (`val probe: String = x` → `actual 'Int'`);
//! * the *parameter name* at a call argument
//!   ([KLS `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters):
//!   a parameter's name is part of a call site's readability), taken from the
//!   candidate the arguments select.
//!
//! Both read the language-neutral body IR and the inference, so the shapes are
//! the ones [`super::java`]'s collectors produce; only the *filters* differ —
//! Kotlin has no `var` keyword, a `val` with a written type needs no hint, an
//! argument that is written *named* already says its parameter's name — and
//! both are decided here.
//!
//! The rendered type goes through [`hir_ty::display_kotlin`], so the label is
//! the Kotlin spelling (`String?`, `List<out Number>`), never Java's.

use hir_expand::body::{BodyTree, ExprData, ExprId, UnaryOp};
use hir_ty::KotlinResolvedMember;
use rowan::{TextRange, TextSize};
use vfs::FileId;

use super::{InlayHint, InlayHintDetail, InlayHintKind, InlayHintLabelPart, InlayHintsConfig};
use crate::RootDatabase;
use crate::nav;

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
    /// Whether the *type* hint of a local declared over `declared` and anchored
    /// at `name_end` is asked for: a range request keeps the locals whose whole
    /// declaration it contains, a resolve keeps the one hint it names.
    fn matches_type_hint(&self, declared: TextRange, name_end: TextSize) -> bool {
        match self {
            Search::Range(range) => range.contains_range(declared),
            Search::At {
                offset,
                kind: InlayHintKind::Type,
            } => name_end == *offset,
            Search::At { .. } => false,
        }
    }

    /// Whether a *parameter* hint anchored at `offset` is asked for.
    fn matches_parameter_hint(&self, offset: TextSize) -> bool {
        match self {
            Search::Range(range) => range.contains(offset),
            Search::At {
                offset: at,
                kind: InlayHintKind::Parameter,
            } => offset == *at,
            Search::At { .. } => false,
        }
    }
}

/// The file's hints whose offset `range` contains, sorted by offset.
pub(crate) fn hints(
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
/// names can be rendered: the declaring sources of the classpath members their
/// invocations selected, where the source is in the library's archive but not
/// materialized. The LSP layer materializes them and re-runs the request —
/// exactly the deferral goto-definition and hover drive — so a classpath
/// member's names render on the first request instead of only once its source
/// happens to be open.
///
/// Empty for a request the parameter-name category is off for, and for the
/// calls whose arguments would render no hint anyway.
pub(crate) fn pending_library_files(
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
pub(crate) fn resolve(
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
    let Some(tree) = hir::hir_def::kotlin::plugin::model(&tree) else {
        return;
    };
    let bodies = hir::file_body_tree(db, file);
    for (id, _) in tree.items.iter() {
        let item = hir_expand::ids::ItemId(id);
        if tree.data(item).body_id().is_none() {
            continue;
        }
        let types = hir_ty::kotlin_declaration_types(db, file, item);
        type_hints(db, &bodies, &types, &search, config, out);
        parameter_name_hints(db, file, &bodies, &types, &search, config, out, pending);
    }
}

// -- inferred local types --------------------------------------------------------

/// The inferred type of every local a declaration form leaves untyped, rendered
/// after the declared name (`val x: Int = 1`).
fn type_hints(
    db: &RootDatabase,
    bodies: &BodyTree,
    types: &hir_ty::kotlin::infer::KotlinBodyTypes,
    search: &Search,
    config: &InlayHintsConfig,
    out: &mut Vec<InlayHintDetail>,
) {
    if !config.var_types {
        return;
    }
    for (local, _) in bodies.locals.iter() {
        let local = hir_expand::body::LocalId(local);
        // Only a local this item's body declares.
        if types.local_ty(db, local) == hir_ty::Ty::error(db) {
            continue;
        }
        let Some(declared_range) = bodies.local_range(local) else {
            continue;
        };
        // A local that writes its type needs no hint, and neither does a
        // parameter: its type is part of its declaration's syntax. A
        // destructured binding writes none either, but its type is a
        // component of the initializer, which the hint would only repeat.
        let is_parameter = types
            .body
            .is_some_and(|body| bodies.body(body).params.contains(&local));
        if bodies.local(local).ty.is_some() || is_parameter {
            continue;
        }
        let Some(name_range) = bodies.local_name_range(local) else {
            continue;
        };
        if !search.matches_type_hint(declared_range, name_range.end()) {
            continue;
        }
        let ty = types.local_ty(db, local);
        out.push(InlayHintDetail {
            hint: InlayHint {
                offset: name_range.end(),
                label: vec![InlayHintLabelPart {
                    value: format!(": {}", hir_ty::display_kotlin(db, ty)),
                    class: None,
                }],
                kind: InlayHintKind::Type,
                padding_left: false,
                padding_right: false,
            },
            tooltip: hir_ty::display_kotlin(db, ty).to_string(),
            edits: Vec::new(),
        });
    }
}

// -- parameter names -------------------------------------------------------------

/// The shape of the callable a call resolved to, read before any parameter name
/// is: the name it is declared under, how many parameters it writes and whether
/// its last parameter is variable-arity ([KLS
/// `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration),
/// `declarations.html#variable-arity-parameters`](https://kotlinlang.org/spec/declarations.html#variable-arity-parameters)).
///
/// The cheap filters of [`parameter_name_hints`] run on this, so a classpath
/// member's declaring source is only materialized for a call that could render
/// a hint at all.
struct CallShape {
    /// The declared name of the callable.
    name: String,
    /// How many parameters the declaration writes.
    arity: usize,
    /// Whether the last parameter is variable-arity.
    varargs: bool,
}

/// The shape of the declaration a name of a Kotlin body resolved to, when it is
/// a callable. A local or a field has no parameter list, and a member the
/// language declares on a built-in classifier has no declaration item at all.
///
/// `written` is the name the call used, which is the declaration's own name
/// except for a constructor call — a constructor declares no name, and the call
/// names the class it constructs.
fn call_shape(
    db: &RootDatabase,
    written: &str,
    member: &KotlinResolvedMember,
) -> Option<CallShape> {
    match member {
        KotlinResolvedMember::Kotlin { file, item } => {
            let tree = hir::hir_def::kotlin::plugin::tree(db, *file)?;
            let data = tree.data(*item);
            let (name, params) = match data {
                hir::hir_def::kotlin::item_tree::KotlinItemData::Function(data) => {
                    (&data.name, &data.params)
                }
                hir::hir_def::kotlin::item_tree::KotlinItemData::Constructor(data) => {
                    return Some(CallShape {
                        name: written.to_owned(),
                        arity: data.params.len(),
                        varargs: is_varargs(&data.params),
                    });
                }
                _ => return None,
            };
            Some(CallShape {
                name: name.as_str().to_owned(),
                arity: params.len(),
                varargs: is_varargs(params),
            })
        }
        KotlinResolvedMember::Java(method) => Some(CallShape {
            name: method.name.clone(),
            arity: method.params.len(),
            varargs: method.varargs,
        }),
        KotlinResolvedMember::Local(_) | KotlinResolvedMember::JavaField(_) => None,
    }
}

/// Whether the last of `params` is variable-arity
/// ([KLS `declarations.html#variable-arity-parameters`](https://kotlinlang.org/spec/declarations.html#variable-arity-parameters)).
fn is_varargs(params: &[hir::hir_def::kotlin::item_tree::KotlinParam]) -> bool {
    params.last().is_some_and(|param| param.param.varargs)
}

/// The parameter names of the declaration the call resolved to, in parameter
/// order, and the signature the hint's tooltip renders.
///
/// A Kotlin source declaration carries them in its item tree. A Java member
/// carries the names a classfile records — none, unless the member is a
/// *source* one — so the declaring source is read back, exactly as the Java
/// language arm does ([`nav::declared_parameter_names`]); when it is in the
/// library's archive but not materialized, the file to load first is recorded
/// in `pending` and no name is invented.
fn declared_names(
    db: &RootDatabase,
    file: FileId,
    member: &KotlinResolvedMember,
    pending: &mut Vec<nav::LibraryFileRef>,
) -> Option<(Vec<String>, String)> {
    match member {
        KotlinResolvedMember::Kotlin { file, item } => {
            let tree = hir::hir_def::kotlin::plugin::tree(db, *file)?;
            let names = match tree.data(*item) {
                hir::hir_def::kotlin::item_tree::KotlinItemData::Function(data) => {
                    parameter_names(&data.params)
                }
                hir::hir_def::kotlin::item_tree::KotlinItemData::Constructor(data) => {
                    parameter_names(&data.params)
                }
                _ => return None,
            };
            Some((names, nav::kotlin::render_signature(&tree, *item)))
        }
        KotlinResolvedMember::Java(method) => {
            let names = match method.param_names.clone() {
                Some(names) => names,
                None => {
                    // A classfile constructor is declared `<init>`
                    // ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6));
                    // a source one is declared under the class's own name and
                    // carries its names already, so the flag is only ever read
                    // for the classfile spelling.
                    let constructor = method.name == "<init>";
                    match nav::declared_parameter_names(db, file, method, constructor) {
                        Some(names) => names,
                        None => {
                            if let Some(pending_file) =
                                nav::pending_parameter_names(db, file, method, constructor)
                                && !pending.contains(&pending_file)
                            {
                                pending.push(pending_file);
                            }
                            return None;
                        }
                    }
                }
            };
            Some((names, method.display(db).to_string()))
        }
        KotlinResolvedMember::Local(_) | KotlinResolvedMember::JavaField(_) => None,
    }
}

/// The declared name of every formal parameter, in order.
fn parameter_names(params: &[hir::hir_def::kotlin::item_tree::KotlinParam]) -> Vec<String> {
    params
        .iter()
        .map(|param| param.param.name.as_str().to_owned())
        .collect()
}

/// A callable's parameter names at the arguments it is passed, where the
/// argument does not already say the name itself (`foo(size: 3)`).
///
/// The names are the *declaration's own*
/// ([KLS `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)),
/// so a call that selects no declaration, or a declaration whose names cannot
/// be read, gets no hint rather than an invented `arg0`-style one. The
/// filters — whether an argument is one a reader cannot infer the purpose of,
/// whether it repeats the parameter's name, whether it is written *named* —
/// are [`super::java`]'s, decided per language.
#[allow(clippy::too_many_arguments)]
fn parameter_name_hints(
    db: &RootDatabase,
    file: FileId,
    bodies: &BodyTree,
    types: &hir_ty::kotlin::infer::KotlinBodyTypes,
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
        // The calls a parameter list belongs to: an invocation and a
        // constructor call ([KLS
        // `expressions.html#function-calls-and-property-accesses`](https://kotlinlang.org/spec/expressions.html#function-calls-and-property-accesses)).
        let ExprData::MethodCall {
            name,
            args,
            arg_names,
            ..
        } = expr
        else {
            continue;
        };
        // The *declaration* the call selected. An unresolved call names no
        // declaration to read names from.
        let Some(member) = types.resolved.get(&expr_id) else {
            continue;
        };
        let Some(shape) = call_shape(db, name.as_str(), member) else {
            continue;
        };
        // A call that renders no hint whatever the parameter names are never
        // needs a declaring source materialized: no parameter to name, an
        // argument the parser could not lower (error recovery), an argument
        // list the parameters cannot absorb, or arguments that all speak for
        // themselves — in a well-typed call an argument of any other shape was
        // inferred *against* the parameter it was selected for, so its type
        // already names it.
        if shape.arity == 0
            || args
                .iter()
                .any(|arg| matches!(bodies.expr(*arg), ExprData::Missing))
            || (!shape.varargs && args.len() > shape.arity)
            || !args
                .iter()
                .any(|&arg| is_unclear_argument(bodies, types, arg))
        {
            continue;
        }
        let Some((names, signature)) = declared_names(db, file, member, pending) else {
            // The declaration's names are not readable yet (the pending
            // source was recorded) or will never be.
            continue;
        };
        let names = names.as_slice();
        if names.len() != shape.arity {
            continue;
        }
        if super::java::names_say_nothing(&shape.name, names) {
            continue;
        }
        // The vararg formal is the *array* of its element
        // ([KLS `declarations.html#variable-arity-parameters`](https://kotlinlang.org/spec/declarations.html#variable-arity-parameters)),
        // so the arguments from its index on are its elements.
        let regular = if shape.varargs {
            shape.arity - 1
        } else {
            shape.arity
        };
        for (index, name) in names.iter().enumerate().take(regular) {
            let Some(&arg) = args.get(index) else {
                continue;
            };
            // A named argument already says the parameter's name, as the
            // written argument's own name does in Java.
            if arg_names.get(index).is_some_and(Option::is_some) {
                continue;
            }
            if super::java::argument_names_parameter(bodies, name, arg)
                || !is_unclear_argument(bodies, types, arg)
            {
                continue;
            }
            push_parameter_hint(search, bodies, arg, format!("{name}:"), &signature, out);
        }
        // The trailing elements of a varargs call get *one* hint for the whole
        // group, at the first of them.
        if shape.varargs && args.len() > regular {
            let trailing = &args[regular..];
            if trailing
                .iter()
                .any(|arg| is_unclear_argument(bodies, types, *arg))
                && let Some(&first) = trailing.first()
            {
                let name = &names[regular];
                push_parameter_hint(
                    search,
                    bodies,
                    first,
                    format!("...{name}:"),
                    &signature,
                    out,
                );
            }
        }
    }
}

/// Whether an argument is one whose purpose a reader cannot infer from the
/// value alone — IntelliJ's `shouldShowHintsForExpression`: a literal, `null`,
/// `this`, a polyadic expression, a signed numeric literal, or
/// `Optional.empty()`.
///
/// The complement — an argument of any other shape — gets no hint: in a
/// well-typed call it was inferred *against* the parameter it was selected
/// for, so its type already names it.
fn is_unclear_argument(
    bodies: &BodyTree,
    types: &hir_ty::kotlin::infer::KotlinBodyTypes,
    arg: ExprId,
) -> bool {
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
                    Some(KotlinResolvedMember::Java(method))
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
    search: &Search,
    bodies: &BodyTree,
    arg: ExprId,
    value: String,
    signature: &str,
    out: &mut Vec<InlayHintDetail>,
) {
    let Some(range) = bodies.expr_range(arg) else {
        return;
    };
    let offset = range.start();
    if !search.matches_parameter_hint(offset) {
        return;
    }
    out.push(InlayHintDetail {
        hint: InlayHint {
            offset,
            label: vec![InlayHintLabelPart { value, class: None }],
            kind: InlayHintKind::Parameter,
            padding_left: false,
            padding_right: true,
        },
        tooltip: signature.to_owned(),
        edits: Vec::new(),
    });
}
