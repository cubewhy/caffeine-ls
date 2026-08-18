//! A stable, human-readable rendering of a lowered [`ItemTree`]. This is the
//! snapshot surface used by `hir-def`'s tests: because items are allocated in
//! CST order and every field renders through fixed helpers, the output is
//! deterministic for a given source file.

use syntax::stub::{PrimitiveType, TypeBound, TypeParameter, TypeRef};

use crate::{
    item_tree::{ItemData, ItemTree, LanguageKind, ModuleData, Signature},
    modifiers::Modifiers,
    name::Name,
};

pub fn pretty_print(tree: &ItemTree) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "file ({})",
        match tree.language {
            LanguageKind::Java => "java",
            LanguageKind::Kotlin => "kotlin",
            LanguageKind::Unknown => "unknown",
        }
    ));
    if let Some(package) = &tree.package {
        out.push_str(&format!(" package {package}"));
    }
    out.push('\n');

    for import in &tree.imports {
        out.push_str("import ");
        if import.is_static {
            out.push_str("static ");
        }
        out.push_str(&import.name.to_string());
        if import.is_asterisk {
            out.push_str(".*");
        }
        out.push_str(&format!("; @{:?}\n", import.range));
    }

    for id in &tree.top {
        render_item(tree, *id, 0, &mut out);
    }

    out
}

fn render_item(tree: &ItemTree, id: crate::item_tree::ItemId, depth: usize, out: &mut String) {
    let item = tree.data(id);
    let indent = "  ".repeat(depth);

    match item {
        ItemData::Class(data) => {
            render_type_like(
                out,
                &indent,
                "class",
                &data.name,
                &data.modifiers,
                data.super_class.as_ref(),
                &data.interfaces,
                &data.type_params,
                data.range,
                tree,
                &data.body,
            );
        }
        ItemData::Interface(data) => {
            render_type_like(
                out,
                &indent,
                "interface",
                &data.name,
                &data.modifiers,
                None,
                &data.interfaces,
                &data.type_params,
                data.range,
                tree,
                &data.body,
            );
        }
        ItemData::Enum(data) => {
            out.push_str(&format!(
                "{indent}enum {}{} @{:?}\n",
                data.name,
                render_mods(&data.modifiers),
                data.range,
            ));
            if !data.interfaces.is_empty() {
                out.push_str(&format!(
                    "{}  implements {}\n",
                    indent,
                    render_join(data.interfaces.iter().map(render_type), ", ")
                ));
            }
            render_children(tree, &data.body, depth + 1, out);
        }
        ItemData::Record(data) => {
            out.push_str(&format!("{indent}record {}", data.name));
            if !data.type_params.is_empty() {
                out.push_str(&format!("<{}>", render_type_params(&data.type_params)));
            }
            out.push_str(&render_components(&data.components));
            if !data.interfaces.is_empty() {
                out.push_str(&format!(
                    " implements {}",
                    render_join(data.interfaces.iter().map(render_type), ", ")
                ));
            }
            out.push_str(&render_mods(&data.modifiers));
            out.push_str(&format!(" @{:?}\n", data.range));
            render_children(tree, &data.body, depth + 1, out);
        }
        ItemData::Annotation(data) => {
            out.push_str(&format!(
                "{indent}@interface {}{} @{:?}\n",
                data.name,
                render_mods(&data.modifiers),
                data.range,
            ));
            render_children(tree, &data.body, depth + 1, out);
        }
        ItemData::Module(data) => {
            render_module(out, &indent, data);
        }
        ItemData::Method(data) => {
            let label = if data.is_constructor {
                "constructor"
            } else {
                "method"
            };
            out.push_str(&format!(
                "{indent}{label} {}{}{} @{:?}\n",
                render_signature(&data.sig, &data.name),
                data.default_value
                    .map(|default| format!(" default @{default:?}"))
                    .unwrap_or_default(),
                render_mods(&data.modifiers),
                data.range,
            ));
        }
        ItemData::Field(data) => {
            out.push_str(&format!(
                "{indent}field {}: {}{}{} @{:?}\n",
                data.name,
                render_type(&data.ty),
                render_mods(&data.modifiers),
                data.initializer
                    .map(|init| format!(" initializer @{init:?}"))
                    .unwrap_or_default(),
                data.range,
            ));
        }
        ItemData::EnumConstant(data) => {
            out.push_str(&format!(
                "{indent}constant {}{} @{:?}\n",
                data.name,
                data.arguments
                    .map(|range| format!(" arguments @{range:?}"))
                    .unwrap_or_default(),
                data.range,
            ));
        }
        ItemData::StaticInit(data) => {
            out.push_str(&format!("{indent}static block @{:?}\n", data.range));
        }
        ItemData::InstanceInit(data) => {
            out.push_str(&format!("{indent}instance block @{:?}\n", data.range));
        }
    }
}

fn render_children(
    tree: &ItemTree,
    body: &[crate::item_tree::ItemId],
    depth: usize,
    out: &mut String,
) {
    for id in body {
        render_item(tree, *id, depth, out);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_type_like(
    out: &mut String,
    indent: &str,
    label: &str,
    name: &Name,
    modifiers: &Modifiers,
    super_class: Option<&TypeRef<Name>>,
    interfaces: &[TypeRef<Name>],
    type_params: &[TypeParameter<Name>],
    range: rowan::TextRange,
    tree: &ItemTree,
    body: &[crate::item_tree::ItemId],
) {
    out.push_str(indent);
    out.push_str(label);
    out.push(' ');
    out.push_str(name.as_str());
    if !type_params.is_empty() {
        out.push_str(&format!("<{}>", render_type_params(type_params)));
    }
    if let Some(super_class) = super_class {
        out.push_str(&format!(" extends {}", render_type(super_class)));
    }
    if !interfaces.is_empty() {
        out.push_str(&format!(
            " implements {}",
            render_join(interfaces.iter().map(render_type), ", ")
        ));
    }
    out.push_str(&render_mods(modifiers));
    out.push_str(&format!(" @{range:?}\n"));
    render_children(tree, body, indent.len() / 2 + 1, out);
}

fn render_module(out: &mut String, indent: &str, data: &ModuleData) {
    out.push_str(&format!(
        "{indent}module {}{}{} @{:?}\n",
        data.name,
        if data.is_open { " [open]" } else { "" },
        render_mods(&data.modifiers),
        data.range,
    ));
    for req in &data.requires {
        let mods = [(req.transitive, "transitive"), (req.statik, "static")]
            .into_iter()
            .filter_map(|(set, name)| set.then_some(name))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!(
            "{}  requires {}{}\n",
            indent,
            if mods.is_empty() {
                String::new()
            } else {
                format!("{mods} ")
            },
            req.name,
        ));
    }
    for export in &data.exports {
        out.push_str(&format!("{}  exports {}", indent, export.package));
        if !export.to.is_empty() {
            out.push_str(&format!(
                " to {}",
                render_join(export.to.iter().map(|n| n.to_string()), ", ")
            ));
        }
        out.push('\n');
    }
    for opens in &data.opens {
        out.push_str(&format!("{}  opens {}", indent, opens.package));
        if !opens.to.is_empty() {
            out.push_str(&format!(
                " to {}",
                render_join(opens.to.iter().map(|n| n.to_string()), ", ")
            ));
        }
        out.push('\n');
    }
    for ty in &data.uses {
        out.push_str(&format!("{}  uses {}\n", indent, render_type(ty)));
    }
    for provide in &data.provides {
        out.push_str(&format!(
            "{}  provides {} with {}\n",
            indent,
            render_type(&provide.service),
            render_join(provide.implementations.iter().map(render_type), ", "),
        ));
    }
}

fn render_signature(sig: &Signature, name: &Name) -> String {
    let mut out = String::new();
    if !sig.type_params.is_empty() {
        out.push_str(&format!("<{}> ", render_type_params(&sig.type_params)));
    }
    out.push_str(name.as_str());
    out.push('(');
    let params: Vec<String> = sig
        .params
        .iter()
        .map(|param| {
            if param.varargs {
                format!("{}... {}", render_type(&param.ty), param.name)
            } else {
                format!("{} {}", render_type(&param.ty), param.name)
            }
        })
        .collect();
    out.push_str(&params.join(", "));
    out.push(')');
    if let Some(ret) = &sig.ret {
        out.push_str(&format!(" -> {}", render_type(ret)));
    }
    if !sig.throws.is_empty() {
        out.push_str(&format!(
            " throws {}",
            render_join(sig.throws.iter().map(render_type), ", ")
        ));
    }
    out
}

fn render_type_params(params: &[TypeParameter<Name>]) -> String {
    render_join(params.iter().map(render_type_param), ", ")
}

fn render_type_param(param: &TypeParameter<Name>) -> String {
    if param.bounds.is_empty() {
        param.name.to_string()
    } else {
        format!(
            "{} extends {}",
            param.name,
            render_join(param.bounds.iter().map(render_type), " & ")
        )
    }
}

fn render_components(components: &[syntax::stub::RecordComponentData<Name>]) -> String {
    format!(
        "({})",
        render_join(
            components.iter().map(|component| {
                let dots = if component.varargs { "..." } else { "" };
                format!(
                    "{}{} {}",
                    render_type(&component.component_type),
                    dots,
                    component.name
                )
            }),
            ", "
        )
    )
}

fn render_join(iter: impl Iterator<Item = String>, sep: &str) -> String {
    iter.collect::<Vec<_>>().join(sep)
}

fn render_mods(modifiers: &Modifiers) -> String {
    let names = modifiers.names().collect::<Vec<_>>();
    if names.is_empty() {
        String::new()
    } else {
        format!(" [{}]", names.join(" "))
    }
}

pub fn render_type(ty: &TypeRef<Name>) -> String {
    match ty {
        TypeRef::Primitive(prim) => render_primitive(*prim).to_owned(),
        TypeRef::Reference { name, generic_args } => {
            if generic_args.is_empty() {
                name.to_string()
            } else {
                format!(
                    "{name}<{}>",
                    render_join(generic_args.iter().map(render_type), ", ")
                )
            }
        }
        TypeRef::Wildcard { bound } => match bound {
            None => "?".to_owned(),
            Some(bound) => match &**bound {
                TypeBound::Upper(ty) => format!("? extends {}", render_type(ty)),
                TypeBound::Lower(ty) => format!("? super {}", render_type(ty)),
            },
        },
        TypeRef::TypeVariable(name) => name.to_string(),
        TypeRef::Array(inner) => format!("{}[]", render_type(inner)),
        TypeRef::Error => "<error>".to_owned(),
    }
}

pub fn render_primitive(prim: PrimitiveType) -> &'static str {
    match prim {
        PrimitiveType::Int => "int",
        PrimitiveType::Long => "long",
        PrimitiveType::Float => "float",
        PrimitiveType::Double => "double",
        PrimitiveType::Boolean => "boolean",
        PrimitiveType::Byte => "byte",
        PrimitiveType::Char => "char",
        PrimitiveType::Short => "short",
        PrimitiveType::Void => "void",
    }
}
