use ropey::Rope;
use std::ops::Range;
use tree_sitter::Node;
use tree_sitter_utils::{Handler, HandlerExt, Input};

use crate::index::IndexView;
use crate::language::java::editor_semantics::{
    JavaInvocationSite, intersects_range, render_type_for_ui, resolve_invocation,
    semantic_context_at_offset,
};
use crate::language::java::type_ctx::SourceTypeCtx;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaInlayHintKind {
    Type,
    Parameter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaInlayHint {
    pub offset: usize,
    pub label: String,
    pub kind: JavaInlayHintKind,
}

pub fn collect_java_inlay_hints(
    source: &str,
    rope: &Rope,
    root: Node,
    view: &IndexView,
    byte_range: Range<usize>,
) -> Vec<JavaInlayHint> {
    let mut hints = Vec::new();
    collect_var_hints(source, rope, root, root, view, &byte_range, &mut hints);
    collect_parameter_hints(source, rope, root, root, view, &byte_range, &mut hints);
    hints.sort_by_key(|hint| hint.offset);
    hints
}

#[allow(clippy::too_many_arguments)]
fn collect_var_hints(
    source: &str,
    rope: &Rope,
    root: Node,
    node: Node,
    view: &IndexView,
    byte_range: &Range<usize>,
    out: &mut Vec<JavaInlayHint>,
) {
    if !intersects_range(node, byte_range) {
        return;
    }
    if node.kind() == "local_variable_declaration"
        && let Some(type_node) = node.child_by_field_name("type")
        && type_node.utf8_text(source.as_bytes()).ok() == Some("var")
    {
        let mut walker = node.walk();
        for declarator in node.named_children(&mut walker) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = declarator.child_by_field_name("name") else {
                continue;
            };
            if !intersects_range(name_node, byte_range) {
                continue;
            }
            let Some(ctx) =
                semantic_context_at_offset(source, rope, root, name_node.end_byte(), view)
            else {
                continue;
            };
            let Some(local) = name_node
                .utf8_text(source.as_bytes())
                .ok()
                .and_then(|name| {
                    ctx.local_variables
                        .iter()
                        .rev()
                        .find(|lv| lv.name.as_ref() == name)
                })
            else {
                continue;
            };
            if local.type_internal.erased_internal() == "var"
                || local.type_internal.erased_internal() == "unknown"
            {
                continue;
            }
            out.push(JavaInlayHint {
                offset: name_node.end_byte(),
                label: format!(": {}", render_type_for_ui(&local.type_internal, view, &ctx)),
                kind: JavaInlayHintKind::Type,
            });
        }
    }

    let mut walker = node.walk();
    for child in node.children(&mut walker) {
        collect_var_hints(source, rope, root, child, view, byte_range, out);
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_parameter_hints(
    source: &str,
    rope: &Rope,
    root: Node,
    node: Node,
    view: &IndexView,
    byte_range: &Range<usize>,
    out: &mut Vec<JavaInlayHint>,
) {
    if !intersects_range(node, byte_range) {
        return;
    }

    if let Some(site) = invocation_site_for_node(source, node)
        && let Some(arguments) = invocation_arguments(node)
    {
        let arg_nodes = named_argument_nodes(arguments);
        if !arg_nodes.is_empty() {
            let ctx_offset = arguments.start_byte().saturating_add(1);
            if let Some(ctx) = semantic_context_at_offset(source, rope, root, ctx_offset, view)
                && let Some(type_ctx) = ctx.extension::<SourceTypeCtx>()
                && let Some(call) = resolve_invocation(&ctx, view, type_ctx, &site, None)
            {
                for (arg_index, arg_node) in arg_nodes.into_iter().enumerate() {
                    if !intersects_range(arg_node, byte_range) {
                        continue;
                    }
                    let Some(param_name) = call.parameter_name_for_argument(arg_index) else {
                        continue;
                    };
                    if should_skip_parameter_hint(param_name.as_ref(), arg_node, source) {
                        continue;
                    }
                    out.push(JavaInlayHint {
                        offset: arg_node.start_byte(),
                        label: format!("{param_name}:"),
                        kind: JavaInlayHintKind::Parameter,
                    });
                }
            }
        }
    }

    let mut walker = node.walk();
    for child in node.children(&mut walker) {
        collect_parameter_hints(source, rope, root, child, view, byte_range, out);
    }
}

fn invocation_site_for_node(source: &str, node: Node) -> Option<JavaInvocationSite> {
    // Dispatch on node kind to build the appropriate invocation site.
    let handler = (|inp: Input<&&[u8]>| -> Option<JavaInvocationSite> {
        let bytes = *inp.ctx;
        let method_name = inp
            .node
            .child_by_field_name("name")?
            .utf8_text(bytes)
            .ok()?
            .to_owned();
        let receiver_expr = inp
            .node
            .child_by_field_name("object")
            .and_then(|r| r.utf8_text(bytes).ok())
            .unwrap_or("this")
            .to_owned();
        let arg_texts = invocation_arguments(inp.node)
            .map(named_argument_nodes)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|arg| arg.utf8_text(bytes).ok().map(ToOwned::to_owned))
            .collect();
        Some(JavaInvocationSite::Method {
            receiver_expr,
            method_name,
            arg_texts,
        })
    })
    .for_kinds(&["method_invocation"])
    .or((|inp: Input<&&[u8]>| -> Option<JavaInvocationSite> {
        let bytes = *inp.ctx;
        let call_text = inp.node.utf8_text(bytes).ok()?.to_owned();
        let arg_texts = invocation_arguments(inp.node)
            .map(named_argument_nodes)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|arg| arg.utf8_text(bytes).ok().map(ToOwned::to_owned))
            .collect();
        Some(JavaInvocationSite::Constructor {
            call_text,
            arg_texts,
        })
    })
    .for_kinds(&["object_creation_expression"]));

    let bytes_ref: &[u8] = source.as_bytes();
    handler.handle(Input::new(node, &bytes_ref, None))
}

fn invocation_arguments(node: Node) -> Option<Node> {
    node.child_by_field_name("arguments")
}

fn named_argument_nodes(arguments: Node) -> Vec<Node> {
    let mut walker = arguments.walk();
    arguments.named_children(&mut walker).collect()
}

fn should_skip_parameter_hint(param_name: &str, arg_node: Node, source: &str) -> bool {
    match arg_node.kind() {
        "identifier" => arg_node
            .utf8_text(source.as_bytes())
            .ok()
            .is_some_and(|text| text == param_name),
        _ => false,
    }
}
