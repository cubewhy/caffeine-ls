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

use hir_expand::{ast_id_map::AstIdMap, name::Name};

use super::item_tree::{
    ConstructorData, ItemAnnotationArg, ItemAnnotationRef, ItemAnnotationValue, ItemId,
    ItemTypeRef, KotlinItemData, KotlinItemTree, KotlinTypeParam, Param, TypeAliasData,
};
use crate::item_tree::language_name;
use crate::kotlin::modifiers::KotlinModifiers;

/// The stable, human-readable rendering of a lowered [`KotlinItemTree`] plus
/// the source ranges resolved from the current syntax tree — the snapshot
/// surface used by `hir-def`'s tests.
pub fn pretty_print(tree: &KotlinItemTree, map: &AstIdMap, source: &SourceFile) -> String {
    let mut out = String::new();

    out.push_str(&format!("file ({})", language_name(tree.language)));
    if let Some(package) = &tree.package {
        out.push_str(&format!(" package {package}"));
    }
    out.push('\n');

    for &annotation in &tree.file_annotations {
        out.push_str(&format!(
            "file annotation {}\n",
            fmt_range(node_range(map, source, annotation))
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
            if !data.super_types.is_empty() {
                out.push_str(&format!(
                    "{indent}  : {}\n",
                    render_join(data.super_types.iter().map(render_item_type))
                ));
            }
            if let Some(constructor) = data.primary_constructor {
                // The primary constructor is a declaration of its own (a
                // navigation and member-resolution target), rendered here
                // because it hangs off the header rather than off the body.
                let data = match tree.data(constructor) {
                    KotlinItemData::Constructor(data) => data,
                    _ => unreachable!("a primary constructor is a constructor item"),
                };
                out.push_str(&format!(
                    "{indent}  primary constructor{}{}\n",
                    render_params(&data.params),
                    suffix(
                        data.modifiers.names(),
                        item_range(tree, map, source, constructor)
                    ),
                ));
                render_annotations(out, &format!("{indent}  "), &data.annotations);
            }
            for &member in &data.body {
                render_item(tree, map, source, member, depth + 1, out);
            }
        }
        KotlinItemData::Constructor(data) => {
            render_constructor(out, &indent, map, source, data, range);
        }
        KotlinItemData::Function(data) => {
            out.push_str(&format!(
                "{indent}fun {}{}{}{}{}\n",
                render_type_params_spaced(&data.type_params),
                render_receiver(&data.receiver, &data.name),
                render_params(&data.params),
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
    let delegation = match data.delegation {
        Some(delegation) => format!(
            " delegation {}",
            fmt_range(node_range(map, source, delegation))
        ),
        None => String::new(),
    };
    out.push_str(&format!(
        "{indent}constructor{}{delegation}{}\n",
        render_params(&data.params),
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

fn render_annotations(out: &mut String, indent: &str, annotations: &[ItemAnnotationRef]) {
    for annotation in annotations {
        out.push_str(&format!(
            "{indent}  @{}{}\n",
            annotation.name,
            render_args(&annotation.args)
        ));
    }
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

fn render_params(params: &[Param]) -> String {
    format!(
        "({})",
        render_join(params.iter().map(|param| {
            format!(
                "{}{}{}: {}",
                if param.varargs { "vararg " } else { "" },
                render_prefix_annotations(&param.annotations),
                param.name,
                render_item_type(&param.ty)
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
            text.push_str(&render_prefix_annotations(&param.annotations));
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
