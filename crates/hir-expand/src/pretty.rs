//! A stable, human-readable rendering of a lowered [`ItemTree`]. This is the
//! snapshot surface used by `hir-def`'s tests: because items are allocated in
//! CST order and every field renders through fixed helpers, the output is
//! deterministic for a given source file.

use syntax::stub::{PrimitiveType, TypeBound, TypeRef};

use crate::{
    body::{
        AssignOp, BinaryOp, BodyTree, ExprData, ExprId, LambdaBody, Literal, LocalId, PatternData,
        PatternId, PostfixOp, StmtData, StmtId, SwitchLabel, UnaryOp,
    },
    item_tree::{
        ItemData, ItemId, ItemTree, LanguageKind, ModuleData, RecordComponent, Signature, TypeParam,
    },
    modifiers::Modifiers,
    name::Name,
    span::SpannedTypeRef,
};

pub fn pretty_print(tree: &ItemTree) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "file ({})",
        match tree.language {
            LanguageKind::Java => "java",
            LanguageKind::Kotlin | LanguageKind::KotlinScript => "kotlin",
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
                    render_join(data.interfaces.iter().map(|ty| render_type(ty)), ", ")
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
                    render_join(data.interfaces.iter().map(|ty| render_type(ty)), ", ")
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
    super_class: Option<&SpannedTypeRef>,
    interfaces: &[SpannedTypeRef],
    type_params: &[TypeParam],
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
            render_join(interfaces.iter().map(|ty| render_type(ty)), ", ")
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
            render_join(
                provide.implementations.iter().map(|ty| render_type(ty)),
                ", "
            ),
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
            render_join(sig.throws.iter().map(|ty| render_type(ty)), ", ")
        ));
    }
    out
}

fn render_type_params(params: &[TypeParam]) -> String {
    render_join(params.iter().map(render_type_param), ", ")
}

fn render_type_param(param: &TypeParam) -> String {
    if param.bounds.is_empty() {
        param.name.to_string()
    } else {
        format!(
            "{} extends {}",
            param.name,
            render_join(param.bounds.iter().map(|ty| render_type(ty)), " & ")
        )
    }
}

fn render_components(components: &[RecordComponent]) -> String {
    format!(
        "({})",
        render_join(
            components.iter().map(|component| {
                let dots = if component.varargs { "..." } else { "" };
                format!("{}{} {}", render_type(&component.ty), dots, component.name)
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

/// A stable, human-readable rendering of the lowered bodies of a file's
/// `ItemTree`, the snapshot surface for the body IR. Expression, statement and
/// local ids print via [`Display`] (`e5`, `s3`, `l0`); the arena is filled in
/// CST order, so the output is deterministic for a given source file.
pub fn pretty_body(tree: &ItemTree, bodies: &BodyTree) -> String {
    let mut out = String::new();
    for (_id, body) in bodies.bodies.iter() {
        out.push_str("body:\n");
        for &param in &body.params {
            let local = bodies.local(param);
            out.push_str(&format!(
                "  param {param}: {} {}\n",
                render_type(
                    &local
                        .ty
                        .clone()
                        .unwrap_or(SpannedTypeRef::synthetic(TypeRef::Error))
                ),
                local.name,
            ));
        }
        for &stmt in &body.stmts {
            render_stmt(&mut out, bodies, stmt, 1);
        }
    }
    for (local_id, local) in bodies.locals.iter() {
        let declared: Vec<_> = bodies
            .bodies
            .iter()
            .flat_map(|(_, b)| b.params.iter().copied())
            .collect();
        if !declared.contains(&LocalId(local_id)) {
            let id = LocalId(local_id);
            out.push_str(&format!(
                "  {id}: {} {}\n",
                render_type(
                    &local
                        .ty
                        .clone()
                        .unwrap_or(SpannedTypeRef::synthetic(TypeRef::Error))
                ),
                local.name,
            ));
        }
    }
    for &top in &tree.top {
        render_orphan_initializers(&mut out, tree, bodies, top, 0);
    }
    out
}

/// Renders expressions that live in the shared expr arena but belong to no
/// [`Body`]: field initializers, enum constant arguments and annotation
/// element defaults.
fn render_orphan_initializers(
    out: &mut String,
    tree: &ItemTree,
    bodies: &BodyTree,
    id: ItemId,
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    match tree.data(id) {
        ItemData::Field(f) => {
            if let Some(e) = f.initializer_expr {
                out.push_str(&format!("{indent}field {}: initializer ", f.name));
                render_expr(out, bodies, e);
                out.push('\n');
            }
        }
        ItemData::EnumConstant(c) => {
            if !c.argument_exprs.is_empty() {
                out.push_str(&format!("{indent}constant {}: arguments [", c.name));
                out.push_str(
                    &c.argument_exprs
                        .iter()
                        .map(|e| e.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                out.push_str("]\n");
                for e in &c.argument_exprs {
                    out.push_str(&format!("{indent}  "));
                    render_expr(out, bodies, *e);
                    out.push('\n');
                }
            }
        }
        ItemData::Method(m) => {
            if let Some(e) = m.default_expr {
                out.push_str(&format!("{indent}method {}: default ", m.name));
                render_expr(out, bodies, e);
                out.push('\n');
            }
        }
        _ => {}
    }
    for &child in tree.data(id).body() {
        render_orphan_initializers(out, tree, bodies, child, depth + 1);
    }
}

fn render_stmt(out: &mut String, bodies: &BodyTree, id: StmtId, depth: usize) {
    let indent = "  ".repeat(depth);
    let data = bodies.stmt(id);
    match data {
        StmtData::Empty => out.push_str(&format!("{indent}{id}: empty\n")),
        StmtData::Block(stmts) => {
            out.push_str(&format!("{indent}{id}: block\n"));
            for &s in stmts {
                render_stmt(out, bodies, s, depth + 1);
            }
        }
        StmtData::Decl { local, initializer } => {
            out.push_str(&format!(
                "{indent}{id}: decl {local} = {}\n",
                initializer
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "none".to_owned()),
            ));
        }
        StmtData::DeclGroup(stmts) => {
            out.push_str(&format!("{indent}{id}: decl-group\n"));
            for &s in stmts {
                render_stmt(out, bodies, s, depth + 1);
            }
        }
        StmtData::Expr(e) => {
            out.push_str(&format!("{indent}{id}: expr "));
            render_expr(out, bodies, *e);
            out.push('\n');
        }
        StmtData::Labeled { label, stmt } => {
            out.push_str(&format!("{indent}{id}: label {stmt} @{label}\n"));
            render_stmt(out, bodies, *stmt, depth + 1);
        }
        StmtData::If { cond, then, els } => {
            out.push_str(&format!("{indent}{id}: if {cond}\n"));
            render_stmt(out, bodies, *then, depth + 1);
            if let Some(els) = els {
                out.push_str(&format!("{indent}{id}: else\n"));
                render_stmt(out, bodies, *els, depth + 1);
            }
        }
        StmtData::While { cond, body } => {
            out.push_str(&format!("{indent}{id}: while {cond}\n"));
            render_stmt(out, bodies, *body, depth + 1);
        }
        StmtData::DoWhile { body, cond } => {
            out.push_str(&format!("{indent}{id}: do-while {cond}\n"));
            render_stmt(out, bodies, *body, depth + 1);
        }
        StmtData::For {
            init,
            cond,
            step,
            body,
        } => {
            out.push_str(&format!("{indent}{id}: for\n"));
            for &i in init {
                render_stmt(out, bodies, i, depth + 1);
            }
            out.push_str(&format!(
                "{indent}  cond: {}\n",
                cond.map(|e| e.to_string())
                    .unwrap_or_else(|| "none".to_owned())
            ));
            out.push_str(&format!(
                "{indent}  step: {}\n",
                step.iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            render_stmt(out, bodies, *body, depth + 1);
        }
        StmtData::ForEach {
            var,
            iterable,
            body,
        } => {
            out.push_str(&format!("{indent}{id}: for-each {var} in {iterable}\n"));
            render_stmt(out, bodies, *body, depth + 1);
        }
        StmtData::Switch { scrutinee, arms } => {
            out.push_str(&format!("{indent}{id}: switch {scrutinee}\n"));
            for arm in arms {
                out.push_str(&format!(
                    "{indent}  case [{}] ->\n",
                    arm.labels
                        .iter()
                        .map(|label| match label {
                            SwitchLabel::Expr(e) => e.to_string(),
                            SwitchLabel::Pattern(p) => render_pattern(bodies, *p),
                            SwitchLabel::Guard(cond) => format!("when {cond}"),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                for &s in &arm.body {
                    render_stmt(out, bodies, s, depth + 2);
                }
            }
        }
        StmtData::Return(e) => {
            out.push_str(&format!(
                "{indent}{id}: return {}\n",
                e.map(|e| e.to_string())
                    .unwrap_or_else(|| "none".to_owned())
            ));
        }
        StmtData::Throw(e) => out.push_str(&format!("{indent}{id}: throw {e}\n")),
        StmtData::Break(l) => out.push_str(&format!(
            "{indent}{id}: break {}\n",
            l.map(|l| l.to_string())
                .unwrap_or_else(|| "unlabeled".to_owned())
        )),
        StmtData::Continue(l) => out.push_str(&format!(
            "{indent}{id}: continue {}\n",
            l.map(|l| l.to_string())
                .unwrap_or_else(|| "unlabeled".to_owned())
        )),
        StmtData::Yield(e) => out.push_str(&format!("{indent}{id}: yield {e}\n")),
        StmtData::Synchronized { expr, body } => {
            out.push_str(&format!("{indent}{id}: synchronized {expr}\n"));
            render_stmt(out, bodies, *body, depth + 1);
        }
        StmtData::Try {
            resources,
            body,
            catches,
            finally,
        } => {
            out.push_str(&format!(
                "{indent}{id}: try resources [{}]\n",
                resources
                    .iter()
                    .map(|resource| match resource.initializer {
                        Some(init) => format!("{} = {init}", resource.local),
                        None => resource.local.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            render_stmt(out, bodies, *body, depth + 1);
            for catch in catches {
                out.push_str(&format!("{indent}  catch {}\n", catch.param));
                render_stmt(out, bodies, catch.body, depth + 2);
            }
            if let Some(finally) = finally {
                out.push_str(&format!("{indent}{id}: finally\n"));
                render_stmt(out, bodies, *finally, depth + 1);
            }
        }
        StmtData::Assert { cond, msg } => {
            out.push_str(&format!(
                "{indent}{id}: assert {cond} msg {}\n",
                msg.map(|e| e.to_string())
                    .unwrap_or_else(|| "none".to_owned())
            ));
        }
        StmtData::LocalClass { name } => {
            out.push_str(&format!("{indent}{id}: local-class {}\n", name.as_str()))
        }
        StmtData::Missing => out.push_str(&format!("{indent}{id}: <missing>\n")),
    }
}

fn render_expr(out: &mut String, bodies: &BodyTree, id: ExprId) {
    let data = bodies.expr(id);
    match data {
        ExprData::Literal(lit) => out.push_str(&format!("{id}: literal {}", render_literal(lit))),
        ExprData::Null => out.push_str(&format!("{id}: null")),
        ExprData::This { qualifier } => out.push_str(&format!(
            "{id}: this {}",
            qualifier
                .as_ref()
                .map(|ty| render_type(ty))
                .unwrap_or_default()
        )),
        ExprData::Super { qualifier } => out.push_str(&format!(
            "{id}: super {}",
            qualifier
                .as_ref()
                .map(|ty| render_type(ty))
                .unwrap_or_default()
        )),
        ExprData::ClassLit(ty) => out.push_str(&format!("{id}: class-lit {}", render_type(ty))),
        ExprData::Var(name) => out.push_str(&format!("{id}: var {name}")),
        ExprData::NamePath(name) => out.push_str(&format!("{id}: name-path {name}")),
        ExprData::FieldAccess { target, name } => out.push_str(&format!(
            "{id}: field-access {}.{name}",
            target
                .map(|e| e.to_string())
                .unwrap_or_else(|| "implicit".to_owned())
        )),
        ExprData::ArrayAccess { array, index } => {
            out.push_str(&format!("{id}: array-access {array}[{index}]"))
        }
        ExprData::MethodCall {
            receiver,
            name,
            args,
            type_args: _,
        } => out.push_str(&format!(
            "{id}: call {}{name}({})",
            receiver.map(|e| format!("{e}.")).unwrap_or_default(),
            args.iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        ExprData::New {
            ty,
            args,
            diamond,
            members,
            receiver,
        } => {
            out.push_str(&format!(
                "{id}: {}new {}{}({})",
                receiver.map(|r| format!("e{r}.")).unwrap_or_default(),
                render_type(ty),
                if *diamond { "<>" } else { "" },
                args.iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            if !members.is_empty() {
                out.push_str(&format!(
                    " {{ {} }}",
                    members
                        .iter()
                        .map(|m| format!("{}({})", m.name.as_str(), m.params))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        ExprData::CtorCall { args, target } => out.push_str(&format!(
            "{id}: ctor-call({})({})",
            match target {
                crate::body::CtorCallTarget::This => "this",
                crate::body::CtorCallTarget::Super => "super",
            },
            args.iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        ExprData::NewArray {
            ty,
            dims,
            initializer,
        } => {
            let mut text = format!("{id}: new-array {}", render_type(ty));
            if !dims.is_empty() {
                text.push_str(&format!(
                    "[{}]",
                    dims.iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if let Some(elems) = initializer {
                text.push_str(&format!(
                    " = {{{}}}",
                    elems
                        .iter()
                        .map(|e| e.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            out.push_str(&text);
        }
        ExprData::ArrayInit(elems) => out.push_str(&format!(
            "{id}: array-init {{{}}}",
            elems
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        ExprData::Unary { op, expr } => {
            out.push_str(&format!("{id}: unary {} {expr}", render_unary(*op)))
        }
        ExprData::Postfix { op, expr } => {
            out.push_str(&format!("{id}: postfix {} {expr}", render_postfix(*op)))
        }
        ExprData::Binary { op, lhs, rhs } => {
            out.push_str(&format!("{id}: binary {} {lhs} {rhs}", render_binary(*op)))
        }
        ExprData::Assign { op, lhs, rhs } => {
            out.push_str(&format!("{id}: assign {} {lhs} {rhs}", render_assign(*op)))
        }
        ExprData::Cast { ty, expr } => {
            out.push_str(&format!("{id}: cast ({}) {expr}", render_type(ty)))
        }
        ExprData::InstanceOf { expr, ty, pattern } => {
            out.push_str(&format!("{id}: instanceof {expr}"));
            match pattern {
                Some(p) => out.push_str(&format!(" pat {p}")),
                None => out.push_str(&format!(
                    " {}",
                    render_type(
                        &ty.clone()
                            .unwrap_or(SpannedTypeRef::synthetic(TypeRef::Error))
                    )
                )),
            }
        }
        ExprData::Conditional { cond, then, els } => {
            out.push_str(&format!("{id}: conditional {cond} ? {then} : {els}"))
        }
        ExprData::Lambda { params, body } => out.push_str(&format!(
            "{id}: lambda ({}) -> {}",
            params
                .iter()
                .map(|(name, ty)| ty
                    .as_ref()
                    .map(|t| format!("{} {}", render_type(t), name))
                    .unwrap_or_else(|| name.to_string()))
                .collect::<Vec<_>>()
                .join(", "),
            match body {
                LambdaBody::Expr(e) => e.to_string(),
                LambdaBody::Block(s) => s.to_string(),
            }
        )),
        ExprData::MethodRef {
            qualifier: _,
            type_name,
            name,
        } => out.push_str(&format!(
            "{id}: method-ref {}\\::{name}",
            type_name
                .as_ref()
                .map(|ty| render_type(ty))
                .unwrap_or_default()
        )),
        ExprData::Switch { scrutinee, arms } => out.push_str(&format!(
            "{id}: switch-expr {scrutinee} ({} arm(s))",
            arms.len()
        )),
        ExprData::Paren(e) => out.push_str(&format!("{id}: paren {e}")),
        ExprData::Template { args } => out.push_str(&format!(
            "{id}: template [{}]",
            args.iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        ExprData::Missing => out.push_str(&format!("{id}: <missing>")),
    }
}

/// Renders a pattern of the body IR
/// ([JLS §14.30](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.30)):
/// a type pattern `Foo f`, a record pattern `Point(int x, int y)` or the
/// match-all `_`.
fn render_pattern(bodies: &BodyTree, id: PatternId) -> String {
    let data = bodies.pattern(id);
    match data {
        PatternData::Type(tp) => {
            let binding = tp
                .binding
                .map(|b| format!(" {}", bodies.local(b).name))
                .unwrap_or_default();
            format!("{} {}{}", id, render_type(&tp.ty), binding)
        }
        PatternData::Record(rp) => format!(
            "{} {}({})",
            id,
            render_type(&rp.ty),
            rp.components
                .iter()
                .map(|&c| render_pattern(bodies, c))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        PatternData::MatchAll => format!("{id} _"),
    }
}

fn render_literal(lit: &Literal) -> &'static str {
    match lit {
        Literal::Int(_) => "int",
        Literal::Long(_) => "long",
        Literal::Char(_) => "char",
        Literal::Float => "float",
        Literal::Double => "double",
        Literal::Boolean(_) => "boolean",
        Literal::Str(_) => "string",
    }
}

fn render_unary(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Plus => "+",
        UnaryOp::Minus => "-",
        UnaryOp::BitNot => "~",
        UnaryOp::Not => "!",
        UnaryOp::Inc => "++",
        UnaryOp::Dec => "--",
    }
}

fn render_postfix(op: PostfixOp) -> &'static str {
    match op {
        PostfixOp::Inc => "++",
        PostfixOp::Dec => "--",
    }
}

fn render_binary(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Rem => "%",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Shl => "<<",
        BinaryOp::Shr => ">>",
        BinaryOp::UShr => ">>>",
        BinaryOp::Lt => "<",
        BinaryOp::Gt => ">",
        BinaryOp::Le => "<=",
        BinaryOp::Ge => ">=",
        BinaryOp::Eq => "==",
        BinaryOp::Ne => "!=",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitXor => "^",
        BinaryOp::BitOr => "|",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
}

fn render_assign(op: AssignOp) -> &'static str {
    match op {
        AssignOp::Assign => "=",
        AssignOp::Mul => "*=",
        AssignOp::Div => "/=",
        AssignOp::Rem => "%=",
        AssignOp::Add => "+=",
        AssignOp::Sub => "-=",
        AssignOp::Shl => "<<=",
        AssignOp::Shr => ">>=",
        AssignOp::UShr => ">>>=",
        AssignOp::BitAnd => "&=",
        AssignOp::BitXor => "^=",
        AssignOp::BitOr => "|=",
    }
}
