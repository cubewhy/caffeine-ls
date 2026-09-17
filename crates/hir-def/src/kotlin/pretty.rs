//! A stable, human-readable rendering of a lowered [`KotlinItemTree`]. This is
//! the snapshot surface used by `hir-def`'s tests: because items are allocated
//! in CST order and every field renders through fixed helpers, the output is
//! deterministic for a given source file.
//!
//! Every item line ends in its source range (`@start..end`), which the
//! renderer resolves from the file's [`AstIdMap`] — the item tree itself
//! carries no offsets.

use rowan::TextRange;
use syntax::SourceFile;
use syntax::stub::{TypeBound, TypeRef};

use hir_expand::{
    ast_id_map::AstIdMap,
    body::{BodyTree, ExprData, ExprId, LocalId, StmtData, StmtId, WhenCondition},
    name::Name,
};

use super::item_tree::{
    ConstructorData, ItemId, KotlinAnnotationRef, KotlinItemData, KotlinItemTree, KotlinParam,
    KotlinSuperType, KotlinTypeParam, TypeAliasData,
};
use crate::jvm::decl::{ItemAnnotationArg, ItemAnnotationRef, ItemAnnotationValue, ItemTypeRef};
use crate::kotlin::modifiers::KotlinModifiers;

/// The stable, human-readable rendering of a lowered [`KotlinItemTree`] plus
/// the source ranges resolved from the current syntax tree — the snapshot
/// surface used by `hir-def`'s tests.
pub fn pretty_print(tree: &KotlinItemTree, map: &AstIdMap, source: &SourceFile) -> String {
    let mut out = String::new();

    out.push_str(&format!("file ({})", tree.language.name()));
    if let Some(package) = &tree.package {
        out.push_str(&format!(" package {package}"));
    }
    out.push('\n');

    for annotation in &tree.file_annotations {
        out.push_str(&format!(
            "file annotation {}\n",
            render_annotation(
                annotation,
                node_range(map, source, annotation.annotation.node)
            )
        ));
    }

    for import in &tree.imports {
        out.push_str(&format!(
            "import {}{}{}; {}\n",
            import.path,
            if import.is_asterisk { ".*" } else { "" },
            import
                .alias
                .as_ref()
                .map(|alias| format!(" as {alias}"))
                .unwrap_or_default(),
            fmt_range(node_range(map, source, import.ast)),
        ));
    }

    for id in &tree.top {
        render_item(tree, map, source, *id, 0, &mut out);
    }

    // The local declarations of the file ([KLS
    // `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)):
    // they are members of no classifier, so they are listed here with the
    // declaration whose body declares them — the item tree's `parent`, which
    // is what `local_types_of` walks — and with their own range, which pins
    // that the item *is* the declaration and not its neighbour.
    if !tree.local_types.is_empty() {
        out.push_str("locals:\n");
        for &item in &tree.local_types {
            let data = tree.data(item);
            out.push_str(&format!(
                "  {} {}{} parent {}\n",
                data.label(),
                data.name().map(|name| name.to_string()).unwrap_or_default(),
                fmt_range(item_range(tree, map, source, item)),
                match tree.parent_of(item) {
                    Some(parent) => format!("item{}", parent.0.0),
                    None => "none".to_owned(),
                },
            ));
            // A local classifier — a local class, an object declaration or an
            // object literal — declares supertypes and members exactly as a
            // top-level one does, so the listing renders them instead of
            // stopping at the item's own line.
            if let KotlinItemData::Class(data) = data {
                render_class_declarations(tree, map, source, data, 2, &mut out);
            }
        }
    }

    out
}

/// Renders a resolved source range as the `@{range:?}` snapshot form; an
/// unresolvable range prints `@None`.
fn fmt_range(range: Option<TextRange>) -> String {
    match range {
        Some(range) => format!("@{range:?}"),
        None => "@None".to_owned(),
    }
}

/// The source range of an anchored syntax node.
fn node_range<N>(
    map: &AstIdMap,
    source: &SourceFile,
    id: hir_expand::ast_id_map::FileAstId<N>,
) -> Option<TextRange> {
    super::ranges::ast_node_range(map, source, id)
}

/// The item's own source range: the range of the syntax node it anchored.
fn item_range(
    tree: &KotlinItemTree,
    map: &AstIdMap,
    source: &SourceFile,
    id: ItemId,
) -> Option<TextRange> {
    super::ranges::item_range(map, source, tree, id)
}

/// The modifiers *and* the range, in the position every renderer puts them.
fn suffix<'a>(modifiers: impl Iterator<Item = &'a str>, range: Option<TextRange>) -> String {
    let mut out = String::new();
    for modifier in modifiers {
        out.push(' ');
        out.push_str(modifier);
    }
    out.push(' ');
    out.push_str(&fmt_range(range));
    out
}

/// The declarations a classifier carries past its header — the supertypes it
/// delegates to, its primary constructor and the members of its body — each on
/// its own line, indented for members at `depth`.
///
/// A *local* classifier carries exactly the same declarations, which is why the
/// renderer of the `locals` listing reaches for this too: a local class, an
/// `object` declaration and an object literal are lowered by the same function
/// and differ only in where the item hangs.
fn render_class_declarations(
    tree: &KotlinItemTree,
    map: &AstIdMap,
    source: &SourceFile,
    data: &super::item_tree::ClassData,
    depth: usize,
    out: &mut String,
) {
    let indent = "  ".repeat(depth);
    if !data.super_types.is_empty() {
        out.push_str(&format!(
            "{indent}: {}\n",
            render_join(data.super_types.iter().map(render_super_type))
        ));
    }
    if let Some(constructor) = data.primary_constructor {
        // The primary constructor is a declaration of its own (a navigation and
        // member-resolution target), rendered here because it hangs off the
        // header rather than off the body.
        let data = match tree.data(constructor) {
            KotlinItemData::Constructor(data) => data,
            _ => unreachable!("a primary constructor is a constructor item"),
        };
        out.push_str(&format!(
            "{indent}primary constructor{}{}\n",
            render_params_with_defaults(&data.params, Some(&data.defaults)),
            suffix(
                data.modifiers.names(),
                item_range(tree, map, source, constructor)
            ),
        ));
        render_annotations(out, &indent, &data.annotations);
    }
    for &member in &data.body {
        render_item(tree, map, source, member, depth, out);
    }
}

fn render_item(
    tree: &KotlinItemTree,
    map: &AstIdMap,
    source: &SourceFile,
    id: ItemId,
    depth: usize,
    out: &mut String,
) {
    let item = tree.data(id);
    let indent = "  ".repeat(depth);
    let range = item_range(tree, map, source, id);
    match item {
        KotlinItemData::Class(data) => {
            out.push_str(&format!(
                "{indent}{} {}{}{}\n",
                // `KotlinClassKind::keyword` spells `enum class` and
                // `annotation class`, so those two modifiers would repeat the
                // label.
                data.kind.keyword(),
                data.name,
                render_type_params(&data.type_params),
                suffix(
                    data.modifiers
                        .names()
                        .filter(|modifier| !matches!(*modifier, "enum" | "annotation")),
                    range
                ),
            ));
            render_annotations(out, &indent, &data.annotations);
            render_class_declarations(tree, map, source, data, depth + 1, out);
        }
        KotlinItemData::Constructor(data) => {
            render_constructor(out, &indent, map, source, data, range);
        }
        KotlinItemData::Function(data) => {
            out.push_str(&format!(
                "{indent}fun {}{}{}{}{}\n",
                render_type_params_spaced(&data.type_params),
                render_receiver(&data.receiver, &data.name),
                render_params_with_defaults(&data.params, Some(&data.defaults)),
                render_ret(&data.ret),
                suffix(data.modifiers.names(), range),
            ));
            render_annotations(out, &indent, &data.annotations);
        }
        KotlinItemData::Accessor(data) => {
            let label = if data.is_setter { "set" } else { "get" };
            out.push_str(&format!(
                "{indent}{label}{}{}\n",
                render_params(&data.params),
                suffix(data.modifiers.names(), range),
            ));
            render_annotations(out, &indent, &data.annotations);
        }
        KotlinItemData::Property(data) => {
            out.push_str(&format!(
                "{indent}{} {}{}{}{}{}\n",
                if data.is_var { "var" } else { "val" },
                render_type_params_spaced(&data.type_params),
                render_receiver(&data.receiver, &data.name),
                data.ty
                    .as_ref()
                    .map(|ty| format!(": {}", render_item_type(ty)))
                    .unwrap_or_default(),
                if data.delegate_expr.is_some() {
                    " by".to_owned()
                } else {
                    String::new()
                },
                suffix(data.modifiers.names(), range),
            ));
            render_annotations(out, &indent, &data.annotations);
            for &accessor in &data.accessors {
                render_item(tree, map, source, accessor, depth + 1, out);
            }
        }
        KotlinItemData::AnonymousInitializer(_) => {
            out.push_str(&format!("{indent}init {}\n", fmt_range(range)));
        }
        KotlinItemData::EnumEntry(data) => {
            out.push_str(&format!(
                "{indent}entry {}{} {}\n",
                data.name,
                if data.argument_exprs.is_empty() {
                    String::new()
                } else {
                    "(…)".to_owned()
                },
                fmt_range(range),
            ));
            render_annotations(out, &indent, &data.annotations);
            for &member in &data.body {
                render_item(tree, map, source, member, depth + 1, out);
            }
        }
        KotlinItemData::TypeAlias(data) => {
            render_type_alias(out, &indent, data, range);
        }
    }
}

fn render_constructor(
    out: &mut String,
    indent: &str,
    map: &AstIdMap,
    source: &SourceFile,
    data: &ConstructorData,
    range: Option<TextRange>,
) {
    let delegation = match &data.delegation {
        Some(delegation) => format!(
            " : {}({}) delegation {}",
            if delegation.is_super { "super" } else { "this" },
            delegation
                .args
                .iter()
                .map(|arg| arg.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            fmt_range(node_range(map, source, delegation.ast))
        ),
        None => String::new(),
    };
    out.push_str(&format!(
        "{indent}constructor{}{delegation}{}\n",
        render_params_with_defaults(&data.params, Some(&data.defaults)),
        suffix(data.modifiers.names(), range),
    ));
    render_annotations(out, indent, &data.annotations);
}

fn render_type_alias(
    out: &mut String,
    indent: &str,
    data: &TypeAliasData,
    range: Option<TextRange>,
) {
    out.push_str(&format!(
        "{indent}typealias {}{} = {}{}\n",
        data.name,
        render_type_params(&data.type_params),
        render_item_type(&data.target),
        suffix(data.modifiers.names(), range),
    ));
    render_annotations(out, indent, &data.annotations);
}

fn render_annotations(out: &mut String, indent: &str, annotations: &[KotlinAnnotationRef]) {
    for annotation in annotations {
        out.push_str(&format!(
            "{indent}  {}\n",
            render_annotation(annotation, None)
        ));
    }
}

/// One annotation application: its use-site target (`@get:`, `@file:`), the
/// annotation's name, its element values and — where the caller resolves one —
/// its source range.
fn render_annotation(annotation: &KotlinAnnotationRef, range: Option<TextRange>) -> String {
    let mut out = format!(
        "@{}{}{}",
        annotation
            .target
            .as_ref()
            .map(|target| format!("{target}:"))
            .unwrap_or_default(),
        annotation.annotation.name,
        render_args(&annotation.annotation.args)
    );
    if let Some(range) = range {
        out.push(' ');
        out.push_str(&fmt_range(Some(range)));
    }
    out
}

/// One supertype specifier: the type, the constructor arguments of
/// `class C : Base(1)` when it writes a call, and the delegate of
/// `interface I by impl` when it writes one.
fn render_super_type(super_type: &KotlinSuperType) -> String {
    let mut out = render_item_type(&super_type.ty);
    if !super_type.args.is_empty() {
        out.push_str(&format!(
            "({})",
            super_type
                .args
                .iter()
                .map(|arg| arg.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(delegate) = super_type.delegate {
        out.push_str(&format!(" by {delegate}"));
    }
    out
}

fn render_args(args: &[ItemAnnotationArg]) -> String {
    if args.is_empty() {
        return String::new();
    }
    format!(
        "({})",
        render_join(args.iter().map(|arg| format!(
            "{} = {}",
            arg.name,
            render_annotation_value(&arg.value)
        )))
    )
}

fn render_annotation_value(value: &ItemAnnotationValue) -> String {
    match value {
        ItemAnnotationValue::Literal(literal) => format!("{literal:?}"),
        ItemAnnotationValue::EnumConstant { qualifier, member } => match qualifier {
            Some(qualifier) => format!("{qualifier}.{member}"),
            None => member.to_string(),
        },
        ItemAnnotationValue::ClassLit(ty) => format!("{}::class", render_item_type(ty)),
        ItemAnnotationValue::Annotation(annotation) => {
            format!("@{}{}", annotation.name, render_args(&annotation.args))
        }
        ItemAnnotationValue::Array(values) => {
            format!(
                "[{}]",
                render_join(values.iter().map(render_annotation_value))
            )
        }
        ItemAnnotationValue::Expr(expr) => format!("expr {expr:?}"),
        ItemAnnotationValue::Unresolved { text } => text.clone(),
    }
}

fn render_receiver(receiver: &Option<ItemTypeRef>, name: &Name) -> String {
    match receiver {
        Some(receiver) => format!("{}.{}", render_item_type(receiver), name),
        None => name.to_string(),
    }
}

fn render_ret(ret: &Option<ItemTypeRef>) -> String {
    match ret {
        Some(ret) => format!(": {}", render_item_type(ret)),
        None => String::new(),
    }
}

fn render_params(params: &[KotlinParam]) -> String {
    render_params_with_defaults(params, None)
}

/// The parameters of a declaration with the `= n` of every default it declares
/// ([KLS
/// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)):
/// `defaults` is aligned with `params`, one entry per parameter.
fn render_params_with_defaults(
    params: &[KotlinParam],
    defaults: Option<&[Option<ExprId>]>,
) -> String {
    let defaults = defaults
        .map(|defaults| defaults.to_vec())
        .unwrap_or_else(|| vec![None; params.len()]);
    format!(
        "({})",
        render_join(params.iter().zip(defaults).map(|(parameter, default)| {
            let param = &parameter.param;
            format!(
                "{}{}{}{}{}: {}{}",
                if param.varargs { "vararg " } else { "" },
                if parameter.noinline { "noinline " } else { "" },
                if parameter.crossinline {
                    "crossinline "
                } else {
                    ""
                },
                render_prefix_annotations(&param.annotations),
                param.name,
                render_item_type(&param.ty),
                default
                    .map(|default| format!(" = {default}"))
                    .unwrap_or_default()
            )
        }))
    )
}

fn render_prefix_annotations(annotations: &[ItemAnnotationRef]) -> String {
    annotations
        .iter()
        .map(|annotation| format!("@{}{} ", annotation.name, render_args(&annotation.args)))
        .collect()
}

/// The type parameters of a *declaration* (`[typeParameters]`), which the
/// grammar separates from the name that follows it.
fn render_type_params_spaced(params: &[KotlinTypeParam]) -> String {
    match render_type_params(params) {
        params if params.is_empty() => params,
        params => format!("{params} "),
    }
}

fn render_type_params(params: &[KotlinTypeParam]) -> String {
    if params.is_empty() {
        return String::new();
    }
    format!(
        "<{}>",
        render_join(params.iter().map(|param| {
            let mut text = String::new();
            if let Some(variance) = param.variance {
                text.push_str(variance.keyword());
                text.push(' ');
            }
            if param.reified {
                text.push_str("reified ");
            }
            text.push_str(&render_prefix_annotations(
                &param
                    .annotations
                    .iter()
                    .map(|annotation| annotation.annotation.clone())
                    .collect::<Vec<_>>(),
            ));
            text.push_str(param.name.as_str());
            for bound in &param.bounds {
                text.push_str(&format!(" : {}", render_item_type(bound)));
            }
            text
        }))
    )
}

fn render_item_type(ty: &ItemTypeRef) -> String {
    render_type(&ty.ty)
}

/// Renders a declaration-side type reference the way a Kotlin *client* spells
/// it — the hover, outline and inlay-hint surface.
///
/// The spellings are the ones kotlinc 2.4.20 reports, checked with the
/// `val probe: String = <expr>` probe:
///
/// | type | kotlinc |
/// |---|---|
/// | `Int?` | `actual 'Int?'` |
/// | `(Int) -> String` | `actual '(Int) -> String'` |
/// | `List<out Number>` | `actual 'List<out Number>'` |
/// | `Map<Int, Int>` | `actual 'Map<Int, Int>'` |
///
/// A function type is spelled in its *sugar* form rather than as the
/// classifier the item tree stores: `kotlin.FunctionN<P1, …, PN, R>` is
/// `(P1, …, PN) -> R` ([KLS
/// `type-system.html#function-types`](https://kotlinlang.org/spec/type-system.html#function-types)),
/// and N is the number of parameters, so the last argument is the return type.
///
/// A recorded deviation: an *extension* function type, which kotlinc spells
/// `String.(Int) -> Boolean`, renders in the parameter form
/// (`(String, Int) -> Boolean`), because the lowering puts the receiver first
/// and the item tree carries no marker for it (see
/// [`crate::kotlin::lower::walk`]).
pub fn display_type(ty: &ItemTypeRef) -> String {
    render_client_type(&ty.ty)
}

/// [`display_type`]'s recursive rendering.
fn render_client_type(ty: &TypeRef<Name>) -> String {
    match ty {
        TypeRef::Reference { name, generic_args } if is_function_classifier(name) => {
            let (params, ret) = generic_args.split_at(generic_args.len().saturating_sub(1));
            let params = render_join(params.iter().map(render_client_type));
            let ret = match ret.first() {
                Some(ret) => render_client_type(ret),
                None => "Unit".to_owned(),
            };
            format!("({params}) -> {ret}")
        }
        TypeRef::Reference { name, generic_args } => {
            if generic_args.is_empty() {
                name.to_string()
            } else {
                format!(
                    "{name}<{}>",
                    render_join(generic_args.iter().map(render_client_type))
                )
            }
        }
        TypeRef::Nullable(inner) => format!("{}?", render_client_type(inner)),
        TypeRef::DefinitelyNonNull(inner) => format!("{} & Any", render_client_type(inner)),
        TypeRef::Wildcard { bound } => match bound {
            None => "*".to_owned(),
            Some(bound) => match &**bound {
                TypeBound::Upper(ty) => format!("out {}", render_client_type(ty)),
                TypeBound::Lower(ty) => format!("in {}", render_client_type(ty)),
            },
        },
        other => render_type(other),
    }
}

/// Whether a reference name is a `kotlin.FunctionN` classifier — the names the
/// function-type lowering produces.
fn is_function_classifier(name: &Name) -> bool {
    let simple = name.as_str().rsplit('.').next().unwrap_or(name.as_str());
    match simple.strip_prefix("Function") {
        Some(digits) => !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(ty: TypeRef<Name>) -> ItemTypeRef {
        ItemTypeRef::synthetic(ty)
    }

    #[test]
    fn client_spellings_match_kotlinc() {
        let named = |name: &str| TypeRef::Reference {
            name: Name::new(name),
            generic_args: Vec::new(),
        };
        // `Int?`
        assert_eq!(
            display_type(&item(TypeRef::Nullable(Box::new(named("Int"))))),
            "Int?"
        );
        // `(Int) -> String`
        assert_eq!(
            display_type(&item(TypeRef::Reference {
                name: Name::new("Function1"),
                generic_args: vec![named("Int"), named("String")],
            })),
            "(Int) -> String"
        );
        // `List<out Number>`
        assert_eq!(
            display_type(&item(TypeRef::Reference {
                name: Name::new("List"),
                generic_args: vec![TypeRef::Wildcard {
                    bound: Some(Box::new(TypeBound::Upper(named("Number")))),
                }],
            })),
            "List<out Number>"
        );
        // `Map<Int, Int>` — and a `Function`-prefixed name that is not a
        // classifier stays a reference.
        assert_eq!(
            display_type(&item(TypeRef::Reference {
                name: Name::new("Map"),
                generic_args: vec![named("Int"), named("Int")],
            })),
            "Map<Int, Int>"
        );
        assert_eq!(
            display_type(&item(TypeRef::Reference {
                name: Name::new("Functionx"),
                generic_args: vec![named("Int")],
            })),
            "Functionx<Int>"
        );
        assert_eq!(display_type(&item(named("Unit"))), "Unit");
    }
}

/// Renders a Kotlin source type: `T?`, `out T`, `*`, `Function2<A, B, R>`.
fn render_type(ty: &TypeRef<Name>) -> String {
    match ty {
        TypeRef::Primitive(primitive) => format!("{primitive:?}"),
        TypeRef::Reference { name, generic_args } => {
            if generic_args.is_empty() {
                name.to_string()
            } else {
                format!(
                    "{name}<{}>",
                    render_join(generic_args.iter().map(render_type))
                )
            }
        }
        TypeRef::Wildcard { bound } => match bound {
            None => "*".to_owned(),
            Some(bound) => match &**bound {
                TypeBound::Upper(ty) => format!("out {}", render_type(ty)),
                TypeBound::Lower(ty) => format!("in {}", render_type(ty)),
            },
        },
        TypeRef::TypeVariable(name) => name.to_string(),
        TypeRef::Array(inner) => format!("Array<{}>", render_type(inner)),
        // Kotlin spells the JVM `void` as its `Unit` classifier (KLS
        // `built-in-types-and-their-semantics.html`), which is also the type
        // the Kotlin type layer maps it to.
        TypeRef::Void => "Unit".to_owned(),
        TypeRef::Error => "<error>".to_owned(),
        TypeRef::Nullable(inner) => format!("{}?", render_type(inner)),
        TypeRef::DefinitelyNonNull(inner) => format!("{} & Any", render_type(inner)),
    }
}

fn render_join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(", ")
}

/// The declared modifiers of an item, when the renderer needs them separately
/// from its range.
#[allow(dead_code)]
fn render_mods(modifiers: &KotlinModifiers) -> String {
    modifiers
        .names()
        .map(|modifier| format!(" {modifier}"))
        .collect()
}

/// The stable, human-readable rendering of a lowered file's bodies: every
/// declaration that owns a body, in item order, with its statements and — in
/// arena order — every expression, local and pattern the body tree holds.
///
/// The rendering is deliberately flat: a statement names the expression ids it
/// uses, and each expression is rendered once, with its child ids (and its
/// operator/name), so a snapshot pins what the walker produced without
/// duplicating sub-trees.
pub fn pretty_body(tree: &KotlinItemTree, bodies: &BodyTree) -> String {
    let mut out = String::new();
    for (_, item) in tree.items.iter() {
        let Some(body) = item.body_id() else {
            continue;
        };
        out.push_str(&format!(
            "{} {} body {body}:\n",
            item.label(),
            item.name().map(|name| name.to_string()).unwrap_or_default(),
        ));
        for &param in &bodies.body(body).params {
            out.push_str(&format!(
                "  param {param}: {}\n",
                render_local(bodies, param)
            ));
        }
        for &stmt in &bodies.body(body).stmts {
            render_stmt(bodies, stmt, 1, &mut out);
        }
    }
    for (index, body) in bodies.bodies.iter() {
        if body.owner.is_none() {
            out.push_str(&format!("body b{} (anonymous):\n", index.0));
            for &stmt in &body.stmts {
                render_stmt(bodies, stmt, 1, &mut out);
            }
        }
    }
    for (id, local) in bodies.locals.iter() {
        let _ = local;
        out.push_str(&format!(
            "  local l{}: {}\n",
            id.0,
            render_local(bodies, LocalId(id))
        ));
    }
    for (id, expr) in bodies.exprs.iter() {
        out.push_str(&format!(
            "  expr e{}: {} {}\n",
            id.0,
            expr_label(expr),
            expr_children(expr),
        ));
    }
    for (id, pattern) in bodies.patterns.iter() {
        out.push_str(&format!("  pattern p{}: {pattern:?}\n", id.0));
    }
    out
}

fn render_local(bodies: &BodyTree, local: LocalId) -> String {
    let local = bodies.local(local);
    match &local.ty {
        Some(ty) => format!("{}: {}", local.name, render_type(&ty.ty)),
        None => local.name.to_string(),
    }
}

/// The label of an expression: its form and its immediate data.
fn expr_label(expr: &ExprData) -> String {
    match expr {
        ExprData::Literal(literal) => format!("literal {literal:?}"),
        ExprData::Null => "null".to_owned(),
        ExprData::Var(name) | ExprData::NamePath(name) => format!("var {name}"),
        // A qualified `this@label`/`super<Base>` renders its qualifier — the
        // label or the supertype. Both carry a one-segment reference, so a
        // label renders in the same `name` form.
        ExprData::This { qualifier } => match qualifier {
            Some(qualifier) => format!("this@{}", render_type(&qualifier.ty)),
            None => "this".to_owned(),
        },
        ExprData::Super { qualifier } => match qualifier {
            Some(qualifier) => format!("super<{}>", render_type(&qualifier.ty)),
            None => "super".to_owned(),
        },
        ExprData::FieldAccess { name, .. } => format!("field {name}"),
        ExprData::MethodCall { name, .. } => format!("call {name}"),
        ExprData::InfixCall { name, .. } => format!("infix {name}"),
        ExprData::New { ty, .. } => format!("new {}", render_type(&ty.ty)),
        ExprData::CtorCall { .. } => "ctor-call".to_owned(),
        ExprData::ArrayAccess { .. } => "index".to_owned(),
        ExprData::ArrayInit(_) => "array-init".to_owned(),
        ExprData::Unary { op, .. } => format!("unary {op:?}"),
        ExprData::Postfix { op, .. } => format!("postfix {op:?}"),
        ExprData::Binary { op, .. } => format!("binary {op:?}"),
        ExprData::Assign { op, .. } => format!("assign {op:?}"),
        ExprData::Cast { ty, safe, .. } => format!(
            "cast{}{}",
            if *safe { "?" } else { "" },
            render_type(&ty.ty)
        ),
        ExprData::InstanceOf { .. } => "is".to_owned(),
        ExprData::Conditional { .. } => "if".to_owned(),
        ExprData::When { arms, .. } => format!("when ({} arms)", arms.len()),
        ExprData::Try { catches, .. } => format!("try ({} catches)", catches.len()),
        ExprData::Elvis { .. } => "elvis".to_owned(),
        ExprData::SafeAccess { .. } => "safe-access".to_owned(),
        ExprData::NullAssert { .. } => "not-null".to_owned(),
        ExprData::Range { inclusive, .. } => {
            if *inclusive {
                "range".to_owned()
            } else {
                "range-until".to_owned()
            }
        }
        ExprData::ObjectLiteral { item } => format!("object-literal item{}", item.0.0),
        ExprData::CallableReference { name, .. } => format!("callable-ref {name}"),
        ExprData::Spread { .. } => "spread".to_owned(),
        ExprData::Jump { kind, .. } => format!("jump {kind:?}"),
        ExprData::Block(_) => "block".to_owned(),
        ExprData::Lambda { params, .. } => format!("lambda ({} params)", params.len()),
        ExprData::MethodRef { name, .. } => format!("method-ref {name}"),
        ExprData::Template { args } => format!("template ({} parts)", args.len()),
        ExprData::ClassLit(ty) => format!("class-literal {}", render_type(&ty.ty)),
        ExprData::Paren(_) => "paren".to_owned(),
        ExprData::Switch { arms, .. } => format!("switch ({} arms)", arms.len()),
        ExprData::NewArray { .. } => "new-array".to_owned(),
        ExprData::Missing => "<missing>".to_owned(),
    }
}

/// The child ids of an expression, in the order the lowering recorded them.
fn expr_children(expr: &ExprData) -> String {
    let ids: Vec<String> = match expr {
        ExprData::FieldAccess { target, .. } => target.iter().map(|e| e.to_string()).collect(),
        ExprData::MethodCall { receiver, args, .. } => receiver
            .iter()
            .map(|e| e.to_string())
            .chain(args.iter().map(|e| e.to_string()))
            .collect(),
        ExprData::InfixCall { receiver, arg, .. } => vec![receiver.to_string(), arg.to_string()],
        ExprData::New { args, .. } => args.iter().map(|e| e.to_string()).collect(),
        ExprData::CtorCall { args, .. } => args.iter().map(|e| e.to_string()).collect(),
        ExprData::ArrayAccess { array, index } => vec![array.to_string(), index.to_string()],
        ExprData::ArrayInit(items) => items.iter().map(|e| e.to_string()).collect(),
        ExprData::Unary { expr, .. }
        | ExprData::Postfix { expr, .. }
        | ExprData::NullAssert { expr }
        | ExprData::Spread { expr }
        | ExprData::Paren(expr) => vec![expr.to_string()],
        ExprData::Binary { lhs, rhs, .. } | ExprData::Assign { lhs, rhs, .. } => {
            vec![lhs.to_string(), rhs.to_string()]
        }
        ExprData::Cast { expr, .. } => vec![expr.to_string()],
        ExprData::InstanceOf { expr, .. } => vec![expr.to_string()],
        ExprData::Conditional { cond, then, els } => {
            vec![cond.to_string(), then.to_string(), els.to_string()]
        }
        ExprData::When { subject, arms } => subject
            .iter()
            .map(|e| e.to_string())
            .chain(arms.iter().flat_map(|arm| {
                arm.conditions
                    .iter()
                    .map(|condition| match condition {
                        WhenCondition::Value(value) => value.to_string(),
                        WhenCondition::TypeTest { expr, .. } => expr.to_string(),
                        WhenCondition::Containment { element, .. } => element.to_string(),
                    })
                    .chain(std::iter::once(arm.body.to_string()))
            }))
            .collect(),
        ExprData::Try {
            body,
            catches,
            finally,
        } => vec![body.to_string()]
            .into_iter()
            .chain(catches.iter().map(|catch| catch.body.to_string()))
            .chain(finally.iter().map(|stmt| stmt.to_string()))
            .collect(),
        ExprData::Elvis { lhs, rhs } => vec![lhs.to_string(), rhs.to_string()],
        ExprData::SafeAccess { receiver, member } => {
            vec![receiver.to_string(), member.to_string()]
        }
        ExprData::Range { lhs, rhs, .. } => vec![lhs.to_string(), rhs.to_string()],
        ExprData::CallableReference { receiver, .. } => {
            receiver.iter().map(|e| e.to_string()).collect()
        }
        ExprData::Jump { value, .. } => value.iter().map(|e| e.to_string()).collect(),
        ExprData::Block(stmt) => vec![stmt.to_string()],
        ExprData::Lambda { body, .. } => match body {
            hir_expand::body::LambdaBody::Expr(expr) => vec![expr.to_string()],
            hir_expand::body::LambdaBody::Block(stmt) => vec![stmt.to_string()],
        },
        ExprData::MethodRef { qualifier, .. } => qualifier.iter().map(|e| e.to_string()).collect(),
        ExprData::Template { args } => args.iter().map(|e| e.to_string()).collect(),
        ExprData::Switch { scrutinee, arms } => vec![scrutinee.to_string()]
            .into_iter()
            .chain(
                arms.iter()
                    .flat_map(|arm| arm.body.iter().map(|stmt| stmt.to_string())),
            )
            .collect(),
        ExprData::NewArray { dims, .. } => dims.iter().map(|e| e.to_string()).collect(),
        _ => Vec::new(),
    };
    if ids.is_empty() {
        String::new()
    } else {
        format!("[{}]", ids.join(", "))
    }
}

fn render_stmt(bodies: &BodyTree, id: StmtId, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match bodies.stmt(id) {
        StmtData::Block(stmts) => {
            out.push_str(&format!("{indent}{id}: block\n"));
            for &stmt in stmts {
                render_stmt(bodies, stmt, depth + 1, out);
            }
        }
        StmtData::Decl { local, initializer } => out.push_str(&format!(
            "{indent}{id}: decl {local} = {}\n",
            initializer
                .map(|e| e.to_string())
                .unwrap_or_else(|| "none".to_owned())
        )),
        StmtData::DeclDelegated { local, delegate } => {
            out.push_str(&format!("{indent}{id}: delegated {local} by {delegate}\n"));
        }
        StmtData::Destructuring {
            pattern,
            initializer,
        } => out.push_str(&format!(
            "{indent}{id}: destructure {pattern} = {initializer}\n"
        )),
        StmtData::Expr(expr) => out.push_str(&format!("{indent}{id}: expr {expr}\n")),
        StmtData::Return(value) => out.push_str(&format!(
            "{indent}{id}: return {}\n",
            value
                .map(|e| e.to_string())
                .unwrap_or_else(|| "none".to_owned())
        )),
        StmtData::Throw(expr) => out.push_str(&format!("{indent}{id}: throw {expr}\n")),
        StmtData::Break(label) => out.push_str(&format!("{indent}{id}: break {label:?}\n")),
        StmtData::Continue(label) => {
            out.push_str(&format!("{indent}{id}: continue {label:?}\n"));
        }
        StmtData::While { cond, body } => {
            out.push_str(&format!("{indent}{id}: while {cond}\n"));
            render_stmt(bodies, *body, depth + 1, out);
        }
        StmtData::DoWhile { body, cond } => {
            out.push_str(&format!("{indent}{id}: do-while {cond}\n"));
            render_stmt(bodies, *body, depth + 1, out);
        }
        StmtData::ForEach {
            var,
            pattern,
            iterable,
            body,
        } => {
            // The destructuring pattern of `for ((k, v) in xs)`: `var` is its
            // first component, and the pattern is what the type layer
            // destructures the element type into.
            let pattern = pattern
                .map(|pattern| format!(" pattern {pattern}"))
                .unwrap_or_default();
            out.push_str(&format!("{indent}{id}: for {var}{pattern} in {iterable}\n"));
            render_stmt(bodies, *body, depth + 1, out);
        }
        StmtData::Labeled { label, stmt } => {
            out.push_str(&format!("{indent}{id}: label {label}\n"));
            render_stmt(bodies, *stmt, depth + 1, out);
        }
        // A local declaration: the item the statement declares.
        StmtData::LocalClass { item } => {
            out.push_str(&format!("{indent}{id}: local class item{}\n", item.0.0));
        }
        StmtData::LocalFunction { item } => {
            out.push_str(&format!("{indent}{id}: local fun item{}\n", item.0.0));
        }
        StmtData::Missing => out.push_str(&format!("{indent}{id}: <missing>\n")),
        other => out.push_str(&format!("{indent}{id}: {other:?}\n")),
    }
}
