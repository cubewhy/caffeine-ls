use ropey::Rope;
use std::ops::Range;
use std::sync::Arc;
use tree_sitter::Node;
use tree_sitter_utils::{Handler, HandlerExt, Input};

use crate::index::IndexView;
use crate::language::java::editor_semantics::{
    JavaInvocationSite, JavaSemanticRequestContext, intersects_range, render_type_for_ui,
    resolve_invocation,
};
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::workspace::Workspace;

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

#[allow(clippy::too_many_arguments)]
pub fn collect_java_inlay_hints(
    source: &str,
    rope: &Rope,
    root: Node,
    view: &IndexView,
    byte_range: Range<usize>,
    request: Option<Arc<crate::lsp::request_context::RequestContext>>,
    request_scope: Option<crate::language::SemanticRequestScope>,
    salsa_db: Option<&dyn crate::salsa_queries::Db>,
    workspace: Option<&Workspace>,
    salsa_file: Option<crate::salsa_db::SourceFile>,
) -> crate::lsp::request_cancellation::RequestResult<Vec<JavaInlayHint>> {
    let collect_started = std::time::Instant::now();
    let semantic = if salsa_db.is_some() || salsa_file.is_some() {
        JavaSemanticRequestContext::new_with_salsa(
            source,
            rope,
            root,
            view,
            request,
            request_scope,
            salsa_db,
            salsa_file,
        )
    } else {
        JavaSemanticRequestContext::new(source, rope, root, view, request, request_scope)
    };
    let mut hints = Vec::new();
    semantic.check_cancelled("inlay.collect.before_var_hints")?;
    collect_var_hints(
        source,
        root,
        &semantic,
        workspace,
        salsa_file,
        &byte_range,
        &mut hints,
    )?;
    semantic.check_cancelled("inlay.collect.before_parameter_hints")?;
    collect_parameter_hints(
        source,
        root,
        &semantic,
        workspace,
        salsa_file,
        &byte_range,
        &mut hints,
    )?;
    hints.sort_by_key(|hint| hint.offset);
    if let Some(metrics) = semantic.metrics() {
        metrics.record_phase_duration("inlay.collect_total", collect_started.elapsed());
    }
    Ok(hints)
}

#[allow(clippy::too_many_arguments)]
fn collect_var_hints(
    source: &str,
    node: Node,
    semantic: &JavaSemanticRequestContext<'_>,
    workspace: Option<&Workspace>,
    salsa_file: Option<crate::salsa_db::SourceFile>,
    byte_range: &Range<usize>,
    out: &mut Vec<JavaInlayHint>,
) -> crate::lsp::request_cancellation::RequestResult<()> {
    semantic.check_cancelled("inlay.collect_var_hints")?;
    if !intersects_range(node, byte_range) {
        return Ok(());
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
                semantic_context_for_inlay(semantic, workspace, salsa_file, name_node.end_byte())?
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
            if local.decl_kind == crate::semantic::LocalVarDeclKind::VarSyntax
                || local.type_internal.is_unknown()
            {
                continue;
            }
            let render_started = std::time::Instant::now();
            let label = format!(
                ": {}",
                render_type_for_ui(&local.type_internal, semantic.view(), &ctx)
            );
            if let Some(metrics) = semantic.metrics() {
                metrics.record_phase_duration_at(
                    "inlay.render_var_hint",
                    Some(name_node.end_byte()),
                    render_started.elapsed(),
                );
            }
            out.push(JavaInlayHint {
                offset: name_node.end_byte(),
                label,
                kind: JavaInlayHintKind::Type,
            });
        }
    }

    let mut walker = node.walk();
    for child in node.children(&mut walker) {
        // TODO: remove recursion
        collect_var_hints(
            source, child, semantic, workspace, salsa_file, byte_range, out,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect_parameter_hints(
    source: &str,
    node: Node,
    semantic: &JavaSemanticRequestContext<'_>,
    workspace: Option<&Workspace>,
    salsa_file: Option<crate::salsa_db::SourceFile>,
    byte_range: &Range<usize>,
    out: &mut Vec<JavaInlayHint>,
) -> crate::lsp::request_cancellation::RequestResult<()> {
    semantic.check_cancelled("inlay.collect_parameter_hints")?;
    if !intersects_range(node, byte_range) {
        return Ok(());
    }

    if let Some(site) = invocation_site_for_node(source, node)
        && let Some(arguments) = invocation_arguments(node)
    {
        let arg_nodes = named_argument_nodes(arguments);
        if !arg_nodes.is_empty() {
            let ctx_offset = arguments.start_byte().saturating_add(1);
            if let Some(ctx) =
                semantic_context_for_inlay(semantic, workspace, salsa_file, ctx_offset)?
                && let Some(type_ctx) = ctx.extension::<SourceTypeCtx>()
            {
                let resolve_started = std::time::Instant::now();
                let resolved_call =
                    resolve_invocation(&ctx, semantic.view(), type_ctx, &site, None);
                if let Some(metrics) = semantic.metrics() {
                    metrics.record_phase_duration_at(
                        "inlay.resolve_invocation",
                        Some(ctx_offset),
                        resolve_started.elapsed(),
                    );
                }
                if let Some(call) = resolved_call {
                    for (arg_index, arg_node) in arg_nodes.into_iter().enumerate() {
                        if arg_index % 16 == 0 {
                            semantic.check_cancelled("inlay.parameter_args")?;
                        }
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
    }

    let mut walker = node.walk();
    for child in node.children(&mut walker) {
        collect_parameter_hints(
            source, child, semantic, workspace, salsa_file, byte_range, out,
        )?;
    }
    Ok(())
}

fn semantic_context_for_inlay(
    semantic: &JavaSemanticRequestContext<'_>,
    workspace: Option<&Workspace>,
    salsa_file: Option<crate::salsa_db::SourceFile>,
    offset: usize,
) -> crate::lsp::request_cancellation::RequestResult<Option<crate::semantic::SemanticContext>> {
    match (workspace, salsa_file) {
        (Some(workspace), Some(salsa_file)) => {
            semantic.inlay_context_at_offset(workspace, salsa_file, offset)
        }
        _ => semantic.semantic_context_at_offset(offset),
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
