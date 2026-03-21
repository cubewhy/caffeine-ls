/// Incremental semantic analysis queries
///
/// These queries break down the expensive context extraction into
/// smaller, cacheable pieces that can be reused across completions.
///
/// Strategy: Return lightweight metadata (offsets, counts) from Salsa,
/// then reconstruct full objects on-demand. This avoids needing to make
/// complex types like LocalVar/TypeName hashable.
use crate::language::java::{
    JavaContextExtractor, members, scope, synthetic,
    type_ctx::{SourceTypeCtx, extract_param_type},
    utils::{get_initializer_text, java_type_to_internal},
};
use crate::language::ts_utils::run_query;
use crate::salsa_db::SourceFile;
use crate::semantic::{
    LocalVar,
    context::CurrentClassMember,
    types::{parse_single_type_to_internal, singleton_descriptor_to_type, type_name::TypeName},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tree_sitter::{Node, Query, Tree};
use tree_sitter_utils::traversal::{
    ancestor_of_kind, ancestor_of_kinds, any_child_of_kind, first_child_of_kind,
    first_child_of_kinds,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScopeRange {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
pub struct CachedMethodLocal {
    local: LocalVar,
    declaration_start: usize,
    visibility_scope: ScopeRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlowTypeOverrideData {
    pub local_name: Arc<str>,
    pub narrowed_type: Arc<str>,
}

pub fn materialize_flow_type_overrides(
    data: &[FlowTypeOverrideData],
) -> HashMap<Arc<str>, TypeName> {
    data.iter()
        .map(|override_data| {
            let narrowed_type = parse_single_type_to_internal(override_data.narrowed_type.as_ref())
                .or_else(|| {
                    singleton_descriptor_to_type(override_data.narrowed_type.as_ref())
                        .map(TypeName::new)
                })
                .unwrap_or_else(|| TypeName::new(Arc::clone(&override_data.narrowed_type)));
            (Arc::clone(&override_data.local_name), narrowed_type)
        })
        .collect()
}

pub fn materialize_current_class_members(
    members: impl IntoIterator<Item = CurrentClassMember>,
) -> HashMap<Arc<str>, CurrentClassMember> {
    members
        .into_iter()
        .filter(|member| !member.is_constructor_like())
        .map(|member| (member.name(), member))
        .collect()
}

pub fn extract_java_current_class_members(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
    workspace: Option<&crate::workspace::Workspace>,
) -> HashMap<Arc<str>, CurrentClassMember> {
    if let Some(workspace) = workspace {
        return materialize_current_class_members(
            extract_class_members_incremental(db, file, cursor_offset, workspace).into_values(),
        );
    }

    extract_java_current_class_members_from_source(db, file, cursor_offset)
}

pub fn extract_java_current_class_members_from_source(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
) -> HashMap<Arc<str>, CurrentClassMember> {
    if file.language_id(db).as_ref() != "java" {
        return HashMap::new();
    }

    let Some(tree) = parse_file_tree(db, file) else {
        return HashMap::new();
    };

    let root = tree.root_node();
    let content = file.content(db);
    let name_table = resolve_name_table_for_file(db, file);
    let ctx = JavaContextExtractor::new_with_overview(
        content.as_str().to_owned(),
        cursor_offset,
        name_table.clone(),
    );
    let cursor_node = ctx.find_cursor_node(root);
    let package = scope::extract_package(&ctx, root);
    let imports = scope::extract_imports(&ctx, root);
    let enclosing_class = scope::extract_enclosing_class(&ctx, cursor_node)
        .or_else(|| scope::extract_enclosing_class_by_offset(&ctx, root));
    let enclosing_internal_name =
        scope::extract_enclosing_internal_name(&ctx, cursor_node, package.as_ref()).or_else(|| {
            crate::language::java::utils::build_internal_name(&package, &enclosing_class)
        });
    let type_ctx = SourceTypeCtx::from_overview(package, imports, name_table);

    let members = cursor_node
        .and_then(scope::nearest_type_declaration)
        .map(|decl| {
            synthetic::extract_type_members_with_synthetics(
                &ctx,
                decl,
                &type_ctx,
                enclosing_internal_name.as_deref(),
            )
        })
        .or_else(|| {
            let error_node = crate::language::java::utils::find_top_error_node(root)?;
            let mut members = Vec::new();
            members::collect_members_from_node(&ctx, error_node, &type_ctx, &mut members);
            Some(members)
        })
        .unwrap_or_default();

    materialize_current_class_members(members)
}

fn parse_source_tree(content: &str, language_id: &str) -> Option<Tree> {
    crate::salsa_queries::parse::parse_tree_for_language(content, language_id)
}

fn parse_file_tree(db: &dyn crate::salsa_queries::Db, file: SourceFile) -> Option<Tree> {
    crate::salsa_queries::parse::parse_tree(db, file)
}

/// Extract parsed method locals from a method (uses cache).
///
/// This returns the cursor-agnostic method-local table represented by the
/// incremental cache.
pub fn extract_method_locals_incremental(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
    workspace: &crate::workspace::Workspace,
) -> Vec<LocalVar> {
    let Some((method_start, method_end)) = find_enclosing_method_bounds(db, file, cursor_offset)
    else {
        return Vec::new();
    };

    let cached = get_or_parse_method_locals(db, file, workspace, method_start, method_end);
    materialize_method_locals(cached)
}

/// Extract the local variables visible at the cursor.
///
/// This is the cursor-sensitive incremental replacement for the old
/// `extract_locals_with_type_ctx()`-style editor pipeline.
pub fn extract_visible_method_locals_incremental(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
    workspace: &crate::workspace::Workspace,
) -> Vec<LocalVar> {
    let Some((method_start, method_end)) = find_enclosing_method_bounds(db, file, cursor_offset)
    else {
        return extract_root_recovery_locals(db, file, cursor_offset);
    };

    let cached = get_or_parse_method_locals(db, file, workspace, method_start, method_end);

    let mut visible = filter_visible_locals(&cached, cursor_offset);
    visible.extend(extract_active_lambda_params_incremental(
        db,
        file,
        cursor_offset,
    ));
    normalize_visible_locals(visible)
}

/// Extract the visible local variables at the cursor without a workspace cache.
///
/// This uses the same semantic extraction pipeline as the incremental Salsa path,
/// but skips the workspace-backed memoization layer so it can be used in tests
/// and standalone source-based helpers.
pub fn extract_visible_method_locals_from_source(
    source: &str,
    cursor_offset: usize,
    type_ctx: Option<&SourceTypeCtx>,
) -> Vec<LocalVar> {
    let Some(tree) = parse_source_tree(source, "java") else {
        return vec![];
    };
    let root = tree.root_node();
    let ctx = JavaContextExtractor::new_with_overview(source.to_string(), cursor_offset, None);
    let cursor_node = ctx.find_cursor_node(root);
    extract_visible_locals_in_tree(root, &ctx, cursor_node, cursor_offset, type_ctx)
}

/// Extract the active lambda parameter names at the cursor without needing a workspace.
pub fn extract_active_lambda_param_names_from_source(
    source: &str,
    cursor_offset: usize,
) -> Vec<Arc<str>> {
    let Some(tree) = parse_source_tree(source, "java") else {
        return vec![];
    };
    let root = tree.root_node();
    let ctx = JavaContextExtractor::new_with_overview(source.to_string(), cursor_offset, None);
    extract_active_lambda_param_names(&ctx, ctx.find_cursor_node(root))
}

/// Extract the active lambda parameter names at the cursor using the Salsa-backed file snapshot.
pub fn extract_active_lambda_param_names_incremental(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
) -> Vec<Arc<str>> {
    let language_id = file.language_id(db);
    if language_id.as_ref() != "java" {
        return vec![];
    }

    let Some(tree) = parse_file_tree(db, file) else {
        return vec![];
    };
    let root = tree.root_node();
    let content: Arc<str> = Arc::from(file.content(db).as_str());
    let ctx = JavaContextExtractor::new_with_overview(Arc::clone(&content), cursor_offset, None);
    extract_active_lambda_param_names(&ctx, ctx.find_cursor_node(root))
}

/// Extract Java flow-sensitive narrowing facts at the cursor (CACHED).
#[salsa::tracked]
pub fn extract_java_flow_type_overrides(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
) -> Arc<Vec<FlowTypeOverrideData>> {
    use crate::language::java::{flow, scope};

    let content = file.content(db);
    let language_id = file.language_id(db);
    if language_id.as_ref() != "java" {
        return Arc::new(vec![]);
    }

    let Some(tree) = parse_file_tree(db, file) else {
        return Arc::new(vec![]);
    };
    let root = tree.root_node();
    let name_table = resolve_name_table_for_file(db, file);
    let ctx = JavaContextExtractor::new_with_overview(
        content.to_string(),
        cursor_offset,
        name_table.clone(),
    );
    let cursor_node = ctx.find_cursor_node(root);
    let package = scope::extract_package(&ctx, root);
    let imports = scope::extract_imports(&ctx, root);
    let type_ctx = SourceTypeCtx::from_overview(package, imports, name_table);
    let locals =
        extract_visible_locals_in_tree(root, &ctx, cursor_node, cursor_offset, Some(&type_ctx));

    let mut overrides: Vec<FlowTypeOverrideData> =
        flow::extract_instanceof_true_branch_overrides(&ctx, cursor_node, &type_ctx, &locals)
            .into_iter()
            .map(|(local_name, narrowed_type)| FlowTypeOverrideData {
                local_name,
                narrowed_type: Arc::from(narrowed_type.to_jvm_signature()),
            })
            .collect();
    overrides.sort_by(|a, b| {
        a.local_name
            .cmp(&b.local_name)
            .then_with(|| a.narrowed_type.cmp(&b.narrowed_type))
    });
    Arc::new(overrides)
}

fn get_or_parse_method_locals(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    workspace: &crate::workspace::Workspace,
    method_start: usize,
    method_end: usize,
) -> Vec<CachedMethodLocal> {
    let metadata = extract_method_locals_metadata(db, file, method_start, method_end);

    if let Some(cached) = workspace.get_cached_method_locals(metadata.content_hash) {
        tracing::debug!(
            content_hash = metadata.content_hash,
            local_count = cached.len(),
            "extract_method_locals_incremental: cache hit!"
        );
        return cached;
    }

    tracing::debug!(
        content_hash = metadata.content_hash,
        "extract_method_locals_incremental: cache miss, parsing..."
    );

    let locals = parse_method_locals(db, file, method_start, method_end);
    workspace.cache_method_locals(metadata.content_hash, locals.clone());
    locals
}

fn materialize_method_locals(mut locals: Vec<CachedMethodLocal>) -> Vec<LocalVar> {
    locals.sort_by(|a, b| {
        a.declaration_start
            .cmp(&b.declaration_start)
            .then_with(|| a.visibility_scope.start.cmp(&b.visibility_scope.start))
            .then_with(|| a.visibility_scope.end.cmp(&b.visibility_scope.end))
    });

    let mut seen: HashSet<(Arc<str>, usize, usize, usize)> = HashSet::new();
    let mut out = Vec::new();
    for local in locals {
        if seen.insert((
            Arc::clone(&local.local.name),
            local.declaration_start,
            local.visibility_scope.start,
            local.visibility_scope.end,
        )) {
            out.push(local.local);
        }
    }
    out
}

fn extract_visible_locals_in_tree<'a>(
    root: Node<'a>,
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node<'a>>,
    cursor_offset: usize,
    type_ctx: Option<&SourceTypeCtx>,
) -> Vec<LocalVar> {
    let mut visible =
        if let Some(method_node) = find_enclosing_method_node_in_tree(root, cursor_offset) {
            filter_visible_locals(
                &collect_method_locals(
                    ctx,
                    method_node,
                    type_ctx,
                    fallback_visibility_scope(method_node.end_byte()),
                ),
                cursor_offset,
            )
        } else {
            filter_visible_locals(
                &collect_method_locals(
                    ctx,
                    root,
                    type_ctx,
                    fallback_visibility_scope(cursor_offset),
                ),
                cursor_offset,
            )
        };

    visible.extend(extract_active_lambda_params(ctx, cursor_node, type_ctx));
    normalize_visible_locals(visible)
}

fn extract_root_recovery_locals(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
) -> Vec<LocalVar> {
    use crate::language::java::scope;

    let content = file.content(db);
    let language_id = file.language_id(db);
    if language_id.as_ref() != "java" {
        return vec![];
    }

    let Some(tree) = parse_file_tree(db, file) else {
        return vec![];
    };
    let root = tree.root_node();
    let name_table = resolve_name_table_for_file(db, file);
    let ctx = JavaContextExtractor::new_with_overview(
        content.to_string(),
        cursor_offset,
        name_table.clone(),
    );
    let package = scope::extract_package(&ctx, root);
    let imports = scope::extract_imports(&ctx, root);
    let type_ctx = SourceTypeCtx::from_overview(package, imports, name_table);

    let mut visible = filter_visible_locals(
        &collect_method_locals(
            &ctx,
            root,
            Some(&type_ctx),
            fallback_visibility_scope(cursor_offset),
        ),
        cursor_offset,
    );
    visible.extend(extract_active_lambda_params(
        &ctx,
        ctx.find_cursor_node(root),
        Some(&type_ctx),
    ));
    normalize_visible_locals(visible)
}

/// Parse method locals (called on cache miss)
fn parse_method_locals(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    method_start: usize,
    method_end: usize,
) -> Vec<CachedMethodLocal> {
    use crate::language::java::scope;

    let content = file.content(db);
    let language_id = file.language_id(db);

    // Only handle Java for now
    if language_id.as_ref() != "java" {
        return Vec::new();
    }

    // Parse tree
    let Some(tree) = parse_file_tree(db, file) else {
        return Vec::new();
    };

    let root = tree.root_node();

    let name_table = resolve_name_table_for_file(db, file);
    let ctx = JavaContextExtractor::new_with_overview(
        content.to_string(),
        method_start,
        name_table.clone(),
    );

    let package = scope::extract_package(&ctx, root);
    let imports = scope::extract_imports(&ctx, root);
    let type_ctx = SourceTypeCtx::from_overview(package, imports, name_table);

    let method_node = find_node_at_offset(
        root,
        method_start,
        &[
            "method_declaration",
            "constructor_declaration",
            "compact_constructor_declaration",
        ],
    );

    let Some(method_node) = method_node else {
        return Vec::new();
    };

    collect_method_locals(
        &ctx,
        method_node,
        Some(&type_ctx),
        fallback_visibility_scope(method_end),
    )
}

fn collect_method_locals(
    ctx: &JavaContextExtractor,
    method_node: Node,
    type_ctx: Option<&SourceTypeCtx>,
    fallback_scope: ScopeRange,
) -> Vec<CachedMethodLocal> {
    let mut locals = extract_declared_locals(ctx, method_node, type_ctx, fallback_scope);
    locals.extend(extract_enhanced_for_locals(
        ctx,
        method_node,
        type_ctx,
        fallback_scope,
    ));
    locals.extend(extract_catch_params(
        ctx,
        method_node,
        type_ctx,
        fallback_scope,
    ));
    locals.extend(extract_misread_var_decls(
        ctx,
        method_node,
        type_ctx,
        fallback_scope,
    ));
    locals.extend(extract_locals_from_error_nodes(
        ctx,
        method_node,
        type_ctx,
        fallback_scope,
    ));
    locals.extend(extract_method_params(
        ctx,
        method_node,
        type_ctx,
        fallback_scope,
    ));
    locals
}

fn extract_enhanced_for_locals(
    ctx: &JavaContextExtractor,
    root: Node,
    type_ctx: Option<&SourceTypeCtx>,
    fallback_scope: ScopeRange,
) -> Vec<CachedMethodLocal> {
    let mut vars = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "enhanced_for_statement"
            && let Some(name_node) = node.child_by_field_name("name")
        {
            let raw_ty = node
                .child_by_field_name("type")
                .and_then(|type_node| type_node.utf8_text(ctx.bytes()).ok())
                .map(str::trim)
                .filter(|ty| !ty.is_empty())
                .unwrap_or("Object");

            vars.push(CachedMethodLocal {
                local: LocalVar {
                    name: Arc::from(ctx.node_text(name_node)),
                    type_internal: resolve_declared_source_type(raw_ty, type_ctx),
                    init_expr: None,
                },
                declaration_start: name_node.start_byte(),
                visibility_scope: scope_range_for_owner(node).unwrap_or(fallback_scope),
            });
        }

        let children: Vec<Node> = {
            let mut cursor = node.walk();
            node.children(&mut cursor).collect()
        };
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    vars
}

fn extract_catch_params(
    ctx: &JavaContextExtractor,
    root: Node,
    type_ctx: Option<&SourceTypeCtx>,
    fallback_scope: ScopeRange,
) -> Vec<CachedMethodLocal> {
    let mut vars = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "catch_clause"
            && let Some(param_node) = first_child_of_kind(node, "catch_formal_parameter")
        {
            let Some(name_node) = param_node.child_by_field_name("name") else {
                continue;
            };
            let raw_ty = extract_param_type(ctx.node_text(param_node)).trim();
            let raw_ty = if raw_ty.is_empty() { "Object" } else { raw_ty };

            vars.push(CachedMethodLocal {
                local: LocalVar {
                    name: Arc::from(ctx.node_text(name_node)),
                    type_internal: resolve_declared_source_type(raw_ty, type_ctx),
                    init_expr: None,
                },
                declaration_start: name_node.start_byte(),
                visibility_scope: scope_range_for_owner(node).unwrap_or(fallback_scope),
            });
        }

        let children: Vec<Node> = {
            let mut cursor = node.walk();
            node.children(&mut cursor).collect()
        };
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    vars
}

fn extract_declared_locals(
    ctx: &JavaContextExtractor,
    root: Node,
    type_ctx: Option<&SourceTypeCtx>,
    fallback_scope: ScopeRange,
) -> Vec<CachedMethodLocal> {
    let query_src = r#"
        (local_variable_declaration
            type: (_) @type
            declarator: (variable_declarator
                name: (identifier) @name))
    "#;
    let q = match Query::new(&tree_sitter_java::LANGUAGE.into(), query_src) {
        Ok(q) => q,
        Err(err) => {
            tracing::debug!("local var query error: {}", err);
            return vec![];
        }
    };

    let type_idx = q.capture_index_for_name("type").unwrap();
    let name_idx = q.capture_index_for_name("name").unwrap();

    run_query(&q, root, ctx.bytes(), None)
        .into_iter()
        .filter_map(|captures| {
            let ty_node = captures.iter().find(|(idx, _)| *idx == type_idx)?.1;
            let name_node = captures.iter().find(|(idx, _)| *idx == name_idx)?.1;
            let declarator = name_node.parent()?;
            let decl = declarator.parent()?;

            if any_child_of_kind(declarator, "argument_list").is_some() {
                return None;
            }

            if let Some(next) = decl.next_sibling() {
                let next_text = ctx.byte_slice(next.start_byte(), next.end_byte());
                if next_text.trim_start().starts_with('(') {
                    return None;
                }
            }

            let name = name_node.utf8_text(ctx.bytes()).ok()?;
            let raw_ty = recovered_declared_type_text(ctx, decl, ty_node)?.to_string();
            let visibility_scope = local_visibility_scope(decl).unwrap_or(fallback_scope);

            let local = if raw_ty == "var" {
                LocalVar {
                    name: Arc::from(name),
                    type_internal: TypeName::new("var"),
                    init_expr: get_initializer_text(ty_node, ctx.bytes()),
                }
            } else {
                LocalVar {
                    name: Arc::from(name),
                    type_internal: resolve_declared_source_type(&raw_ty, type_ctx),
                    init_expr: None,
                }
            };

            Some(CachedMethodLocal {
                local,
                declaration_start: name_node.start_byte(),
                visibility_scope,
            })
        })
        .collect()
}

fn resolve_declared_source_type(raw_ty: &str, type_ctx: Option<&SourceTypeCtx>) -> TypeName {
    if let Some(type_ctx) = type_ctx
        && let Some(resolved) = type_ctx.resolve_type_name_relaxed(raw_ty.trim())
    {
        if raw_ty.contains('<') && resolved.ty.args.is_empty() {
            return TypeName::new(java_type_to_internal(raw_ty).as_str());
        }
        return resolved.ty;
    }
    TypeName::new(java_type_to_internal(raw_ty).as_str())
}

fn extract_misread_var_decls(
    ctx: &JavaContextExtractor,
    root: Node,
    type_ctx: Option<&SourceTypeCtx>,
    fallback_scope: ScopeRange,
) -> Vec<CachedMethodLocal> {
    let mut vars = Vec::new();
    collect_misread_var_decls(ctx, root, &mut vars, type_ctx, fallback_scope);
    vars
}

fn collect_misread_var_decls(
    ctx: &JavaContextExtractor,
    root: Node,
    vars: &mut Vec<CachedMethodLocal>,
    type_ctx: Option<&SourceTypeCtx>,
    fallback_scope: ScopeRange,
) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "variable_declarator"
            && let Some(assignment) = first_child_of_kind(node, "assignment_expression")
        {
            let lhs = assignment
                .child_by_field_name("left")
                .or_else(|| assignment.named_child(0));
            let rhs = assignment
                .child_by_field_name("right")
                .or_else(|| assignment.named_child(1));
            if let (Some(name_node), Some(init_node)) = (lhs, rhs) {
                if name_node.kind() != "identifier" {
                    continue;
                }

                let name = ctx.node_text(name_node);
                let type_name = find_type_in_error_sibling(ctx, node);
                let local = if type_name.as_deref() == Some("var") {
                    LocalVar {
                        name: Arc::from(name),
                        type_internal: TypeName::new("var"),
                        init_expr: Some(ctx.node_text(init_node).to_string()),
                    }
                } else {
                    let raw_ty = type_name.as_deref().unwrap_or("Object");
                    LocalVar {
                        name: Arc::from(name),
                        type_internal: resolve_declared_source_type(raw_ty, type_ctx),
                        init_expr: None,
                    }
                };

                vars.push(CachedMethodLocal {
                    local,
                    declaration_start: name_node.start_byte(),
                    visibility_scope: local_visibility_scope(node).unwrap_or(fallback_scope),
                });
            }
        }

        let children: Vec<Node> = {
            let mut cursor = node.walk();
            node.children(&mut cursor).collect()
        };
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
}

fn find_type_in_error_sibling(ctx: &JavaContextExtractor, declarator_node: Node) -> Option<String> {
    if let Some(error_child) = first_child_of_kind(declarator_node, "ERROR")
        && let Some(type_node) = first_child_of_kinds(
            error_child,
            &["type_identifier", "integral_type", "void_type"],
        )
    {
        return Some(ctx.node_text(type_node).to_string());
    }
    None
}

fn extract_method_params(
    ctx: &JavaContextExtractor,
    method_node: Node,
    type_ctx: Option<&SourceTypeCtx>,
    fallback_scope: ScopeRange,
) -> Vec<CachedMethodLocal> {
    let params_node = if method_node.kind() == "compact_constructor_declaration" {
        ancestor_of_kind(method_node, "record_declaration").and_then(|record| {
            record
                .child_by_field_name("parameters")
                .or_else(|| first_child_of_kind(record, "formal_parameters"))
        })
    } else {
        method_node.child_by_field_name("parameters")
    };

    let Some(params_node) = params_node else {
        return vec![];
    };

    let visibility_scope = method_like_body_scope(method_node).unwrap_or(fallback_scope);
    let mut vars = Vec::new();
    let mut cursor = params_node.walk();
    for param in params_node.named_children(&mut cursor) {
        if !matches!(param.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }

        let name_node = param
            .child_by_field_name("name")
            .or_else(|| first_child_of_kind(param, "identifier"))
            .or_else(|| {
                first_child_of_kind(param, "variable_declarator").and_then(|node| {
                    node.child_by_field_name("name")
                        .or_else(|| first_child_of_kind(node, "identifier"))
                })
            });

        let Some(name_node) = name_node else {
            continue;
        };

        let mut raw_ty = extract_param_type(ctx.node_text(param)).trim().to_string();
        if param.kind() == "spread_parameter" || raw_ty.ends_with("...") {
            raw_ty = if let Some(stripped) = raw_ty.strip_suffix("...") {
                format!("{}[]", stripped.trim())
            } else {
                format!("{}[]", raw_ty.trim())
            };
        }

        vars.push(CachedMethodLocal {
            local: LocalVar {
                name: Arc::from(ctx.node_text(name_node)),
                type_internal: resolve_declared_source_type(raw_ty.as_str(), type_ctx),
                init_expr: None,
            },
            declaration_start: name_node.start_byte(),
            visibility_scope,
        });
    }

    vars
}

fn extract_locals_from_error_nodes(
    ctx: &JavaContextExtractor,
    root: Node,
    type_ctx: Option<&SourceTypeCtx>,
    fallback_scope: ScopeRange,
) -> Vec<CachedMethodLocal> {
    let mut vars = Vec::new();
    collect_locals_in_errors(ctx, root, &mut vars, type_ctx, fallback_scope);
    vars
}

fn collect_locals_in_errors(
    ctx: &JavaContextExtractor,
    root: Node,
    vars: &mut Vec<CachedMethodLocal>,
    type_ctx: Option<&SourceTypeCtx>,
    fallback_scope: ScopeRange,
) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "ERROR" {
            let query_src = r#"
                (local_variable_declaration
                    type: (_) @type
                    declarator: (variable_declarator
                        name: (identifier) @name))
            "#;
            if let Ok(q) = Query::new(&tree_sitter_java::LANGUAGE.into(), query_src) {
                let type_idx = q.capture_index_for_name("type").unwrap();
                let name_idx = q.capture_index_for_name("name").unwrap();
                let found: Vec<CachedMethodLocal> = run_query(&q, node, ctx.bytes(), None)
                    .into_iter()
                    .filter_map(|captures| {
                        let ty_node = captures.iter().find(|(idx, _)| *idx == type_idx)?.1;
                        let name_node = captures.iter().find(|(idx, _)| *idx == name_idx)?.1;
                        let declarator = name_node.parent()?;
                        let decl = declarator.parent()?;
                        let raw_ty = recovered_declared_type_text(ctx, decl, ty_node)?.to_string();
                        let visibility_scope =
                            local_visibility_scope(decl).unwrap_or(fallback_scope);

                        let local = if raw_ty == "var" {
                            LocalVar {
                                name: Arc::from(name_node.utf8_text(ctx.bytes()).ok()?),
                                type_internal: TypeName::new("var"),
                                init_expr: get_initializer_text(ty_node, ctx.bytes()),
                            }
                        } else {
                            LocalVar {
                                name: Arc::from(name_node.utf8_text(ctx.bytes()).ok()?),
                                type_internal: resolve_declared_source_type(&raw_ty, type_ctx),
                                init_expr: None,
                            }
                        };

                        Some(CachedMethodLocal {
                            local,
                            declaration_start: name_node.start_byte(),
                            visibility_scope,
                        })
                    })
                    .collect();
                vars.extend(found);
            }
        }

        let children: Vec<Node> = {
            let mut cursor = node.walk();
            node.children(&mut cursor).collect()
        };
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
}

fn extract_active_lambda_params_incremental(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
) -> Vec<CachedMethodLocal> {
    use crate::language::java::scope;

    let content = file.content(db);
    let language_id = file.language_id(db);
    if language_id.as_ref() != "java" {
        return vec![];
    }

    let Some(tree) = parse_file_tree(db, file) else {
        return vec![];
    };
    let root = tree.root_node();

    let content: Arc<str> = Arc::from(content.as_str());
    let ctx = JavaContextExtractor::new_with_overview(Arc::clone(&content), cursor_offset, None);
    let cursor_node = ctx.find_cursor_node(root);

    if extract_active_lambda_param_names(&ctx, cursor_node).is_empty() {
        return vec![];
    }

    let name_table = resolve_name_table_for_file(db, file);
    let typed_ctx = JavaContextExtractor::new_with_overview(
        Arc::clone(&content),
        cursor_offset,
        name_table.clone(),
    );
    let package = scope::extract_package(&typed_ctx, root);
    let imports = scope::extract_imports(&typed_ctx, root);
    let type_ctx = SourceTypeCtx::from_overview(package, imports, name_table);

    extract_active_lambda_params(&typed_ctx, cursor_node, Some(&type_ctx))
}

fn extract_active_lambda_param_names(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
) -> Vec<Arc<str>> {
    let mut current = cursor_node;
    while let Some(node) = current {
        if node.kind() == "lambda_expression" {
            let Some(body) = node.child_by_field_name("body") else {
                return vec![];
            };
            if ctx.offset < body.start_byte() || ctx.offset > body.end_byte() {
                return vec![];
            }
            let Some(params) = node.child_by_field_name("parameters") else {
                return vec![];
            };
            return extract_lambda_param_names(ctx, params);
        }
        if matches!(node.kind(), "ERROR" | "block" | "program")
            && let Some(names) = extract_active_lambda_names_from_error_arrow_node(ctx, node)
        {
            return names;
        }
        current = node.parent();
    }
    vec![]
}

fn extract_active_lambda_params(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    type_ctx: Option<&SourceTypeCtx>,
) -> Vec<CachedMethodLocal> {
    let mut lambdas = Vec::new();
    let mut current = cursor_node;
    while let Some(node) = current {
        if node.kind() == "lambda_expression" {
            lambdas.push(node);
        }
        current = node.parent();
    }

    if lambdas.is_empty() {
        return extract_lambda_params_from_error_nodes_in_ancestry(ctx, cursor_node, type_ctx);
    }

    let mut vars = Vec::new();
    for lambda in lambdas.into_iter().rev() {
        let Some(body) = lambda.child_by_field_name("body") else {
            continue;
        };
        if ctx.offset < body.start_byte() || ctx.offset > body.end_byte() {
            continue;
        }
        let Some(params) = lambda.child_by_field_name("parameters") else {
            continue;
        };
        vars.extend(extract_lambda_params_from_node(ctx, params, type_ctx));
    }
    vars
}

fn extract_lambda_params_from_error_nodes_in_ancestry(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    type_ctx: Option<&SourceTypeCtx>,
) -> Vec<CachedMethodLocal> {
    let mut current = cursor_node;
    while let Some(node) = current {
        if (node.kind() == "ERROR" || node.kind() == "block" || node.kind() == "program")
            && let Some(result) = extract_lambda_params_from_error_arrow_node(ctx, node, type_ctx)
        {
            return result;
        }
        current = node.parent();
    }
    vec![]
}

fn extract_lambda_params_from_error_arrow_node(
    ctx: &JavaContextExtractor,
    container: Node,
    type_ctx: Option<&SourceTypeCtx>,
) -> Option<Vec<CachedMethodLocal>> {
    let children: Vec<Node> = {
        let mut walker = container.walk();
        container.children(&mut walker).collect()
    };
    let arrow_idx = children
        .iter()
        .rposition(|node| node.kind() == "->" && node.end_byte() <= ctx.offset)?;
    if arrow_idx == 0 {
        return None;
    }

    let arrow_end = children[arrow_idx].end_byte();
    if ctx.offset < arrow_end {
        return None;
    }

    let params_node = children[arrow_idx - 1];
    let visibility_scope = lambda_param_visibility_scope(params_node)
        .unwrap_or_else(|| fallback_visibility_scope(ctx.offset));

    let param_entries: Vec<(Arc<str>, TypeName, usize)> = match params_node.kind() {
        "identifier" => vec![(
            Arc::from(ctx.node_text(params_node)),
            TypeName::new("unknown"),
            params_node.start_byte(),
        )],
        "inferred_parameters" => {
            let mut walker = params_node.walk();
            params_node
                .named_children(&mut walker)
                .filter(|node| node.kind() == "identifier")
                .map(|node| {
                    (
                        Arc::from(ctx.node_text(node)),
                        TypeName::new("unknown"),
                        node.start_byte(),
                    )
                })
                .collect()
        }
        "formal_parameters" => {
            let mut walker = params_node.walk();
            params_node
                .named_children(&mut walker)
                .filter_map(|node| {
                    if matches!(node.kind(), "formal_parameter" | "spread_parameter") {
                        let name_node = node.child_by_field_name("name")?;
                        let ty = extract_lambda_formal_param_type(ctx, node, type_ctx);
                        Some((
                            Arc::from(ctx.node_text(name_node)),
                            ty,
                            name_node.start_byte(),
                        ))
                    } else {
                        None
                    }
                })
                .collect()
        }
        _ => return None,
    };

    if param_entries.is_empty() {
        return None;
    }

    Some(
        param_entries
            .into_iter()
            .map(
                |(name, type_internal, declaration_start)| CachedMethodLocal {
                    local: LocalVar {
                        name,
                        type_internal,
                        init_expr: None,
                    },
                    declaration_start,
                    visibility_scope,
                },
            )
            .collect(),
    )
}

fn extract_active_lambda_names_from_error_arrow_node(
    ctx: &JavaContextExtractor,
    container: Node,
) -> Option<Vec<Arc<str>>> {
    let children: Vec<Node> = {
        let mut walker = container.walk();
        container.children(&mut walker).collect()
    };
    let arrow_idx = children
        .iter()
        .rposition(|node| node.kind() == "->" && node.end_byte() <= ctx.offset)?;
    if arrow_idx == 0 {
        return None;
    }

    let arrow_end = children[arrow_idx].end_byte();
    if ctx.offset < arrow_end {
        return None;
    }

    let params_node = children[arrow_idx - 1];
    let names = extract_lambda_param_names(ctx, params_node);
    if names.is_empty() { None } else { Some(names) }
}

fn extract_lambda_params_from_node(
    ctx: &JavaContextExtractor,
    params: Node,
    type_ctx: Option<&SourceTypeCtx>,
) -> Vec<CachedMethodLocal> {
    let visibility_scope = lambda_param_visibility_scope(params)
        .unwrap_or_else(|| fallback_visibility_scope(ctx.offset));

    match params.kind() {
        "identifier" => vec![CachedMethodLocal {
            local: LocalVar {
                name: Arc::from(ctx.node_text(params)),
                type_internal: TypeName::new("unknown"),
                init_expr: None,
            },
            declaration_start: params.start_byte(),
            visibility_scope,
        }],
        "inferred_parameters" => {
            let mut vars = Vec::new();
            let mut walker = params.walk();
            for node in params.named_children(&mut walker) {
                if node.kind() != "identifier" {
                    continue;
                }
                vars.push(CachedMethodLocal {
                    local: LocalVar {
                        name: Arc::from(ctx.node_text(node)),
                        type_internal: TypeName::new("unknown"),
                        init_expr: None,
                    },
                    declaration_start: node.start_byte(),
                    visibility_scope,
                });
            }
            vars
        }
        "formal_parameters" => {
            let mut vars = Vec::new();
            let mut walker = params.walk();
            for node in params.named_children(&mut walker) {
                if !matches!(node.kind(), "formal_parameter" | "spread_parameter") {
                    continue;
                }
                let Some(name_node) = node.child_by_field_name("name") else {
                    continue;
                };
                vars.push(CachedMethodLocal {
                    local: LocalVar {
                        name: Arc::from(ctx.node_text(name_node)),
                        type_internal: extract_lambda_formal_param_type(ctx, node, type_ctx),
                        init_expr: None,
                    },
                    declaration_start: name_node.start_byte(),
                    visibility_scope,
                });
            }
            vars
        }
        _ => vec![],
    }
}

fn extract_lambda_param_names(ctx: &JavaContextExtractor, params: Node) -> Vec<Arc<str>> {
    match params.kind() {
        "identifier" => vec![Arc::from(ctx.node_text(params))],
        "inferred_parameters" => {
            let mut vars = Vec::new();
            let mut walker = params.walk();
            for node in params.named_children(&mut walker) {
                if node.kind() == "identifier" {
                    vars.push(Arc::from(ctx.node_text(node)));
                }
            }
            vars
        }
        "formal_parameters" => {
            let mut vars = Vec::new();
            let mut walker = params.walk();
            for node in params.named_children(&mut walker) {
                if !matches!(node.kind(), "formal_parameter" | "spread_parameter") {
                    continue;
                }
                if let Some(name_node) = node.child_by_field_name("name") {
                    vars.push(Arc::from(ctx.node_text(name_node)));
                }
            }
            vars
        }
        _ => vec![],
    }
}

fn extract_lambda_formal_param_type(
    ctx: &JavaContextExtractor,
    param_node: Node,
    type_ctx: Option<&SourceTypeCtx>,
) -> TypeName {
    let Some(type_node) = param_node.child_by_field_name("type") else {
        return TypeName::new("unknown");
    };
    let raw_ty = ctx.node_text(type_node).trim().to_string();
    if raw_ty.is_empty() || raw_ty == "var" {
        return TypeName::new("unknown");
    }
    resolve_declared_source_type(&raw_ty, type_ctx)
}

fn filter_visible_locals(
    locals: &[CachedMethodLocal],
    cursor_offset: usize,
) -> Vec<CachedMethodLocal> {
    locals
        .iter()
        .filter(|local| {
            local.declaration_start < cursor_offset
                && scope_contains_offset(local.visibility_scope, cursor_offset)
        })
        .cloned()
        .collect()
}

fn normalize_visible_locals(mut vars: Vec<CachedMethodLocal>) -> Vec<LocalVar> {
    vars.sort_by(|a, b| {
        b.declaration_start
            .cmp(&a.declaration_start)
            .then_with(|| b.visibility_scope.start.cmp(&a.visibility_scope.start))
            .then_with(|| b.visibility_scope.end.cmp(&a.visibility_scope.end))
    });

    let mut seen: HashSet<Arc<str>> = HashSet::new();
    let mut visible = Vec::new();
    for ranked in vars {
        if seen.insert(Arc::clone(&ranked.local.name)) {
            visible.push(ranked.local);
        }
    }
    visible
}

fn is_plausible_local_type_text(raw_ty: &str) -> bool {
    let trimmed = raw_ty.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains(';')
        || trimmed.contains("//")
        || trimmed.contains("/*")
        || trimmed.contains('=')
        || trimmed.contains('{')
        || trimmed.contains('}')
    {
        return false;
    }
    true
}

fn recovered_declared_type_text<'a>(
    ctx: &'a JavaContextExtractor,
    decl: Node<'a>,
    ty_node: Node<'a>,
) -> Option<&'a str> {
    let raw_ty = ty_node.utf8_text(ctx.bytes()).ok()?.trim();
    if is_plausible_local_type_text(raw_ty) {
        return Some(raw_ty);
    }
    recover_type_text_from_decl(ctx, decl)
}

fn recover_type_text_from_decl<'a>(
    ctx: &'a JavaContextExtractor,
    decl: Node<'a>,
) -> Option<&'a str> {
    let declarator = decl.child_by_field_name("declarator")?;
    let boundary = declarator.start_byte();
    let mut best = None;
    collect_last_type_like_before(decl, boundary, &mut best);
    best.and_then(|node| node.utf8_text(ctx.bytes()).ok().map(str::trim))
        .filter(|text| is_plausible_local_type_text(text))
}

fn collect_last_type_like_before<'a>(root: Node<'a>, boundary: usize, best: &mut Option<Node<'a>>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.start_byte() >= boundary {
            continue;
        }
        if node.end_byte() <= boundary && is_type_like_node(node.kind()) {
            *best = Some(node);
        }

        let children: Vec<Node> = {
            let mut cursor = node.walk();
            node.children(&mut cursor).collect()
        };
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
}

fn is_type_like_node(kind: &str) -> bool {
    matches!(
        kind,
        "type_identifier"
            | "scoped_type_identifier"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
            | "void_type"
            | "array_type"
            | "generic_type"
            | "annotated_type"
    )
}

fn local_visibility_scope(decl: Node) -> Option<ScopeRange> {
    let owner = nearest_local_scope_owner(decl)?;
    scope_range_for_owner(owner)
}

fn lambda_param_visibility_scope(params: Node) -> Option<ScopeRange> {
    let parent = params.parent()?;
    if parent.kind() == "lambda_expression" {
        let body = parent.child_by_field_name("body")?;
        return Some(node_scope_range(body));
    }

    let children: Vec<Node> = {
        let mut walker = parent.walk();
        parent.children(&mut walker).collect()
    };
    let params_idx = children.iter().position(|node| node.id() == params.id())?;
    let arrow = children
        .get(params_idx + 1)
        .filter(|node| node.kind() == "->")?;
    Some(ScopeRange {
        start: arrow.end_byte(),
        end: parent.end_byte(),
    })
}

fn method_like_body_scope(method: Node) -> Option<ScopeRange> {
    scope_range_for_owner(method)
}

fn nearest_local_scope_owner(node: Node) -> Option<Node> {
    ancestor_of_kinds(
        node,
        &[
            "block",
            "for_statement",
            "enhanced_for_statement",
            "catch_clause",
            "switch_block_statement_group",
            "switch_rule",
            "method_declaration",
            "constructor_declaration",
            "compact_constructor_declaration",
            "lambda_expression",
            "static_initializer",
            "instance_initializer",
        ],
    )
}

fn scope_range_for_owner(owner: Node) -> Option<ScopeRange> {
    let scope_node = match owner.kind() {
        "method_declaration"
        | "constructor_declaration"
        | "compact_constructor_declaration"
        | "lambda_expression"
        | "catch_clause"
        | "enhanced_for_statement" => owner.child_by_field_name("body")?,
        _ => owner,
    };
    Some(node_scope_range(scope_node))
}

fn node_scope_range(node: Node) -> ScopeRange {
    ScopeRange {
        start: node.start_byte(),
        end: node.end_byte(),
    }
}

fn scope_contains_offset(scope: ScopeRange, offset: usize) -> bool {
    scope.start <= offset && offset < scope.end
}

fn fallback_visibility_scope(offset: usize) -> ScopeRange {
    ScopeRange {
        start: 0,
        end: offset.saturating_add(1),
    }
}

/// Metadata about a file's structure (cached by Salsa)
///
/// This tracks high-level structure so we can detect when
/// specific parts of the file change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileStructureMetadata {
    /// Package name
    pub package: Option<Arc<str>>,
    /// Number of imports
    pub import_count: usize,
    /// Number of static imports
    pub static_import_count: usize,
    /// Number of top-level classes
    pub class_count: usize,
    /// Hash of the file structure
    pub structure_hash: u64,
}

/// Extract file structure metadata (cached by Salsa)
///
/// This is very cheap - just counts and hashes, no deep parsing.
/// When this changes, we know we need to re-parse.
#[salsa::tracked]
pub fn extract_file_structure(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
) -> FileStructureMetadata {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let package = crate::salsa_queries::extract_package(db, file);
    let imports = crate::salsa_queries::extract_imports(db, file);
    let language_id = file.language_id(db);

    let static_import_count = match language_id.as_ref() {
        "java" => crate::salsa_queries::java::extract_java_static_imports(db, file).len(),
        _ => 0,
    };

    let class_count = if let Some(tree) = parse_file_tree(db, file) {
        count_top_level_type_declarations(tree.root_node(), language_id.as_ref())
    } else {
        0
    };

    let mut hasher = DefaultHasher::new();
    package.hash(&mut hasher);
    imports.len().hash(&mut hasher);
    for import in imports.iter() {
        import.hash(&mut hasher);
    }
    static_import_count.hash(&mut hasher);
    class_count.hash(&mut hasher);

    FileStructureMetadata {
        package,
        import_count: imports.len(),
        static_import_count,
        class_count,
        structure_hash: hasher.finish(),
    }
}

fn count_top_level_type_declarations(root: Node, language_id: &str) -> usize {
    let target_kinds: &[&str] = match language_id {
        "java" => &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
            "annotation_type_declaration",
        ],
        "kotlin" => &["class_declaration", "object_declaration"],
        _ => &[],
    };

    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .filter(|node| target_kinds.contains(&node.kind()))
        .count()
}

/// Metadata about a method's local variables (cached by Salsa)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodLocalsMetadata {
    /// Method start offset
    pub method_start: usize,
    /// Method end offset
    pub method_end: usize,
    /// Number of local variables
    pub local_count: usize,
    /// Hash of method content
    pub content_hash: u64,
}

/// Extract method locals metadata (cached by Salsa)
///
/// This is keyed by method offsets, so it only recomputes when
/// the specific method changes.
#[salsa::tracked]
pub fn extract_method_locals_metadata(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    method_start: usize,
    method_end: usize,
) -> MethodLocalsMetadata {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let content = file.content(db);

    // Hash the method content for change detection
    let method_content = if method_end <= content.len() {
        &content[method_start..method_end]
    } else {
        ""
    };

    let mut hasher = DefaultHasher::new();
    method_content.hash(&mut hasher);
    let content_hash = hasher.finish();

    // Parse the tree and count the same cursor-agnostic locals we cache for completions.
    let local_count = if let Some(tree) = parse_file_tree(db, file) {
        count_method_locals(tree.root_node(), content, method_start, method_end)
    } else {
        0
    };

    tracing::debug!(
        file_uri = file.file_id(db).as_str(),
        method_start = method_start,
        method_end = method_end,
        local_count = local_count,
        content_hash = content_hash,
        "extract_method_locals_metadata: counted locals"
    );

    MethodLocalsMetadata {
        method_start,
        method_end,
        local_count,
        content_hash,
    }
}

fn count_method_locals(root: Node, source: &str, method_start: usize, method_end: usize) -> usize {
    let method_node = find_node_at_offset(
        root,
        method_start,
        &[
            "method_declaration",
            "constructor_declaration",
            "compact_constructor_declaration",
        ],
    );

    let Some(method_node) = method_node else {
        return 0;
    };

    let ctx = JavaContextExtractor::new_with_overview(source.to_string(), method_start, None);
    collect_method_locals(
        &ctx,
        method_node,
        None,
        fallback_visibility_scope(method_end),
    )
    .len()
}

/// Metadata about a class's members (cached by Salsa)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClassMembersMetadata {
    /// Class start offset
    pub class_start: usize,
    /// Class end offset
    pub class_end: usize,
    /// Number of methods
    pub method_count: usize,
    /// Number of fields
    pub field_count: usize,
    /// Hash of class content
    pub content_hash: u64,
}

/// Extract class members metadata (cached by Salsa)
///
/// This is keyed by class offsets, so it only recomputes when
/// the specific class changes.
#[salsa::tracked]
pub fn extract_class_members_metadata(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    class_start: usize,
    class_end: usize,
) -> ClassMembersMetadata {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let content = file.content(db);

    // Hash the class content for change detection
    let class_content = if class_end <= content.len() {
        &content[class_start..class_end]
    } else {
        ""
    };

    let mut hasher = DefaultHasher::new();
    class_content.hash(&mut hasher);
    let content_hash = hasher.finish();

    // Parse the tree and count methods/fields
    let (method_count, field_count) = if let Some(tree) = parse_file_tree(db, file) {
        count_members_in_range(tree.root_node(), class_start, class_end)
    } else {
        (0, 0)
    };

    tracing::debug!(
        file_uri = file.file_id(db).as_str(),
        class_start = class_start,
        class_end = class_end,
        method_count = method_count,
        field_count = field_count,
        content_hash = content_hash,
        "extract_class_members_metadata: counted members"
    );

    ClassMembersMetadata {
        class_start,
        class_end,
        method_count,
        field_count,
        content_hash,
    }
}

/// Count methods and fields in a specific range
fn count_members_in_range(root: tree_sitter::Node, start: usize, end: usize) -> (usize, usize) {
    let mut method_count = 0;
    let mut field_count = 0;

    // Find the node that contains our range
    let mut cursor = root.walk();
    let mut current = root;

    // Navigate to the node at start position
    loop {
        let mut found_child = false;
        let children: Vec<_> = current.children(&mut cursor).collect();
        for child in children {
            if child.start_byte() <= start && end <= child.end_byte() {
                current = child;
                found_child = true;
                break;
            }
        }

        if !found_child {
            break;
        }
    }

    // Now traverse from this node
    traverse_and_count_members(current, start, end, &mut method_count, &mut field_count);

    (method_count, field_count)
}

/// Iterative traversal to count methods and fields.
///
/// This avoids hard recursion limits for deeply nested blocks such as
/// large chains of `if` statements inside a method body.
fn traverse_and_count_members(
    root: tree_sitter::Node,
    start: usize,
    end: usize,
    method_count: &mut usize,
    field_count: &mut usize,
) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.end_byte() < start || node.start_byte() > end {
            continue;
        }

        match node.kind() {
            "method_declaration" | "constructor_declaration" => {
                *method_count += 1;
            }
            "field_declaration" => {
                let mut child_cursor = node.walk();
                let declarator_count = node
                    .children(&mut child_cursor)
                    .filter(|n| n.kind() == "variable_declarator")
                    .count();
                *field_count += declarator_count;
            }
            _ => {}
        }

        let children: Vec<_> = {
            let mut cursor = node.walk();
            node.children(&mut cursor).collect()
        };
        for child in children.into_iter().rev() {
            if child.end_byte() < start || child.start_byte() > end {
                continue;
            }
            stack.push(child);
        }
    }
}

/// Find the enclosing method bounds for a cursor position (cached by Salsa)
///
/// Returns (method_start, method_end) if cursor is inside a method.
/// This is cached per (file, cursor_offset) so it's very fast.
#[salsa::tracked]
pub fn find_enclosing_method_bounds(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
) -> Option<(usize, usize)> {
    // Parse the tree
    let tree = parse_file_tree(db, file)?;
    let root = tree.root_node();

    let method_node = find_enclosing_method_node_in_tree(root, cursor_offset)?;

    let start = method_node.start_byte();
    let end = method_node.end_byte();

    tracing::debug!(
        file_uri = file.file_id(db).as_str(),
        cursor_offset = cursor_offset,
        method_start = start,
        method_end = end,
        "find_enclosing_method_bounds: found method"
    );

    Some((start, end))
}

fn find_enclosing_method_node_in_tree<'a>(
    root: tree_sitter::Node<'a>,
    cursor_offset: usize,
) -> Option<tree_sitter::Node<'a>> {
    use tree_sitter_utils::traversal::{ancestor_of_kinds, find_node_by_offset};

    let node_at_cursor = find_node_by_offset(root, "identifier", cursor_offset)
        .or_else(|| find_node_by_offset(root, "block", cursor_offset))
        .or_else(|| find_deepest_node_at_offset(root, cursor_offset))?;

    ancestor_of_kinds(
        node_at_cursor,
        &[
            "method_declaration",
            "constructor_declaration",
            "compact_constructor_declaration",
        ],
    )
}

/// Find the deepest node at a given offset (fallback when specific kinds don't match)
fn find_deepest_node_at_offset<'a>(
    root: tree_sitter::Node<'a>,
    offset: usize,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = root.walk();
    let mut node = root;

    loop {
        let mut found_child = false;
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in children {
            if child.start_byte() <= offset && offset < child.end_byte() {
                node = child;
                found_child = true;
                break;
            }
        }

        if !found_child {
            return Some(node);
        }
    }
}

fn recover_enclosing_type_from_error(
    root: tree_sitter::Node<'_>,
    content: &str,
    cursor_offset: usize,
) -> Option<(Arc<str>, usize, usize)> {
    fn dfs(
        node: tree_sitter::Node<'_>,
        offset: usize,
        bytes: &[u8],
        result: &mut Option<(Arc<str>, usize, usize)>,
    ) {
        if node.start_byte() > offset || node.end_byte() <= offset {
            return;
        }

        if node.kind() == "ERROR" {
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            for i in 0..children.len().saturating_sub(1) {
                let keyword = children[i].kind();
                if matches!(keyword, "class" | "interface" | "enum" | "record")
                    && children[i + 1].kind() == "identifier"
                    && let Ok(name) = children[i + 1].utf8_text(bytes)
                {
                    *result = Some((Arc::from(name), node.start_byte(), node.end_byte()));
                }
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            dfs(child, offset, bytes, result);
        }
    }

    let mut result = None;
    dfs(root, cursor_offset, content.as_bytes(), &mut result);
    result
}

/// Find a node of specific kinds at a given offset
fn find_node_at_offset<'a>(
    root: tree_sitter::Node<'a>,
    offset: usize,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    use tree_sitter_utils::traversal::find_node_by_offset;

    // Try each kind directly
    for kind in kinds {
        if let Some(node) = find_node_by_offset(root, kind, offset) {
            return Some(node);
        }
    }

    // Fallback: find any node at offset and walk up
    use tree_sitter_utils::traversal::ancestor_of_kinds;
    let node_at_offset = find_node_by_offset(root, "identifier", offset)
        .or_else(|| find_node_by_offset(root, "block", offset))
        .or_else(|| find_deepest_node_at_offset(root, offset))?;

    ancestor_of_kinds(node_at_offset, kinds)
}

/// Find the enclosing class bounds for a cursor position (cached by Salsa)
///
/// Returns (class_name, class_start, class_end) if cursor is inside a class.
/// This is cached per (file, cursor_offset) so it's very fast.
#[salsa::tracked]
pub fn find_enclosing_class_bounds(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
) -> Option<(Arc<str>, usize, usize)> {
    use tree_sitter_utils::traversal::{ancestor_of_kinds, find_node_by_offset};

    let content = file.content(db);

    // Parse the tree
    let tree = parse_file_tree(db, file)?;
    let root = tree.root_node();

    // Find any node at the cursor position first
    let node_at_cursor = find_node_by_offset(root, "identifier", cursor_offset)
        .or_else(|| find_node_by_offset(root, "block", cursor_offset))
        .or_else(|| find_deepest_node_at_offset(root, cursor_offset))?;

    // Walk up to find class/interface/enum/record. When the file is malformed,
    // tree-sitter may wrap the declaration in ERROR instead of producing a type
    // declaration node, so fall back to recovering the name from that subtree.
    let (class_name, start, end) = if let Some(class_node) = ancestor_of_kinds(
        node_at_cursor,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
        ],
    ) {
        let class_name = class_node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(content.as_bytes()).ok())
            .map(Arc::from)
            .unwrap_or_else(|| Arc::from("Unknown"));
        (class_name, class_node.start_byte(), class_node.end_byte())
    } else {
        recover_enclosing_type_from_error(root, content, cursor_offset)?
    };

    tracing::debug!(
        file_uri = file.file_id(db).as_str(),
        cursor_offset = cursor_offset,
        class_name = %class_name,
        class_start = start,
        class_end = end,
        "find_enclosing_class_bounds: found class"
    );

    Some((class_name, start, end))
}

/// Extract actual class members from a class (uses cache)
///
/// This is the incremental version that uses the PSI-style cache.
/// Call this instead of extract_type_members_with_synthetics().
pub fn extract_class_members_incremental(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
    workspace: &crate::workspace::Workspace,
) -> std::collections::HashMap<Arc<str>, crate::semantic::context::CurrentClassMember> {
    use std::collections::HashMap;

    // Step 1: Find class bounds (Salsa cached)
    let Some((class_name, class_start, class_end)) =
        find_enclosing_class_bounds(db, file, cursor_offset)
    else {
        return HashMap::new();
    };

    // Step 2: Get metadata (Salsa cached)
    let metadata = extract_class_members_metadata(db, file, class_start, class_end);

    // Step 3: Check PSI cache
    if let Some(cached) = workspace.get_cached_class_members(metadata.content_hash) {
        tracing::debug!(
            content_hash = metadata.content_hash,
            member_count = cached.len(),
            class_name = %class_name,
            "extract_class_members_incremental: cache hit!"
        );
        return cached;
    }

    // Step 4: Cache miss - parse members
    tracing::debug!(
        content_hash = metadata.content_hash,
        class_name = %class_name,
        "extract_class_members_incremental: cache miss, parsing..."
    );

    let members = parse_class_members(db, file, class_start, class_end);

    // Step 5: Cache the result
    workspace.cache_class_members(metadata.content_hash, members.clone());

    members
}

/// Parse class members (called on cache miss)
fn parse_class_members(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    class_start: usize,
    _class_end: usize,
) -> std::collections::HashMap<Arc<str>, crate::semantic::context::CurrentClassMember> {
    use crate::language::java::synthetic::extract_type_members_with_synthetics;
    use crate::language::java::type_ctx::SourceTypeCtx;
    use crate::language::java::{JavaContextExtractor, scope};
    use std::collections::HashMap;

    let content = file.content(db);
    let language_id = file.language_id(db);

    // Only handle Java for now
    if language_id.as_ref() != "java" {
        return HashMap::new();
    }

    // Parse tree
    let Some(tree) = parse_file_tree(db, file) else {
        return HashMap::new();
    };

    let root = tree.root_node();

    let name_table = resolve_name_table_for_file(db, file);

    // Create a minimal context extractor for parsing
    let ctx = JavaContextExtractor::for_indexing_with_overview(content, name_table.clone());

    // Extract package and imports for type resolution
    let package = scope::extract_package(&ctx, root);
    let imports = scope::extract_imports(&ctx, root);
    let type_ctx = SourceTypeCtx::from_overview(package, imports, name_table);

    // Find the class node at class_start
    let class_node = find_node_at_offset(
        root,
        class_start,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
        ],
    );

    let Some(class_node) = class_node else {
        return HashMap::new();
    };

    // Extract members with synthetics (Lombok, etc.)
    let members = extract_type_members_with_synthetics(&ctx, class_node, &type_ctx, None);

    // Convert Vec to HashMap keyed by member name
    members.into_iter().map(|m| (m.name(), m)).collect()
}

fn resolve_name_table_for_file(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
) -> Option<Arc<crate::index::NameTable>> {
    let workspace_index = db.workspace_index();
    let index = workspace_index.read();
    let _ = file;
    tracing::debug!(
        phase = "indexing",
        file = %file.file_id(db).as_str(),
        purpose = "incremental source parse/discovery",
        "constructing NameTable for semantic parse helper"
    );
    Some(index.build_name_table(crate::index::IndexScope {
        module: crate::index::ModuleId::ROOT,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ClassMetadata, ClassOrigin};
    use crate::salsa_db::{Database, FileId, SourceFile};
    use crate::workspace::Workspace;
    use indoc::indoc;
    use tower_lsp::lsp_types::Url;
    use tree_sitter::Parser;

    #[derive(Clone, Copy)]
    struct ParityCase {
        name: &'static str,
        source: &'static str,
        marker: &'static str,
        strip_marker: bool,
    }

    fn prepare_source(source: &str, marker: &str, strip_marker: bool) -> (String, usize) {
        let offset = source.find(marker).expect("marker");
        if strip_marker {
            (source.replacen(marker, "", 1), offset)
        } else {
            (source.to_string(), offset)
        }
    }

    fn setup_case(
        source: &str,
    ) -> (
        Database,
        Workspace,
        SourceFile,
        Arc<crate::index::NameTable>,
    ) {
        let db = Database::default();
        let workspace = Workspace::new();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(&db, FileId::new(uri), source.to_string(), Arc::from("java"));
        let name_table = resolve_name_table_for_file(&db, file).expect("name table");
        (db, workspace, file, name_table)
    }

    fn minimal_class(internal_name: &str) -> ClassMetadata {
        let (package, name) = internal_name
            .rsplit_once('/')
            .map(|(package, name)| (Some(Arc::from(package)), Arc::from(name)))
            .unwrap_or((None, Arc::from(internal_name)));
        ClassMetadata {
            package,
            name,
            internal_name: Arc::from(internal_name),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: 0,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    fn reference_locals(
        _db: &Database,
        _file: SourceFile,
        source: &str,
        offset: usize,
        name_table: Arc<crate::index::NameTable>,
    ) -> Vec<LocalVar> {
        let db = Database::default();
        let file = SourceFile::new(
            &db,
            FileId::new(Url::parse("file:///test/ReferenceLocals.java").unwrap()),
            source.to_string(),
            Arc::from("java"),
        );
        let package = crate::salsa_queries::java::extract_java_package(&db, file);
        let imports = crate::salsa_queries::java::extract_java_imports(&db, file);
        let type_ctx = SourceTypeCtx::from_overview(package, imports, Some(name_table));
        extract_visible_method_locals_from_source(source, offset, Some(&type_ctx))
    }

    fn locals_signature(locals: &[LocalVar]) -> Vec<(String, String, Option<String>)> {
        locals
            .iter()
            .map(|local| {
                (
                    local.name.to_string(),
                    local.type_internal.to_internal_with_generics(),
                    local.init_expr.clone(),
                )
            })
            .collect()
    }

    fn assert_incremental_matches_reference(case: ParityCase) {
        let (source, offset) = prepare_source(case.source, case.marker, case.strip_marker);
        let (db, workspace, file, name_table) = setup_case(&source);

        let expected = reference_locals(&db, file, &source, offset, name_table);
        let actual = extract_visible_method_locals_incremental(&db, file, offset, &workspace);

        let expected_sig = locals_signature(&expected);
        let actual_sig = locals_signature(&actual);
        assert_eq!(
            actual_sig, expected_sig,
            "{}\nexpected={expected_sig:?}\nactual={actual_sig:?}",
            case.name
        );
    }

    #[test]
    fn test_incremental_locals_match_reference_regular_cases() {
        let cases = [
            ParityCase {
                name: "standard locals preserve generics",
                source: indoc! {r#"
                    class A {
                        void f() {
                            int a = 1;
                            String b = "hello";
                            List<String> c = new ArrayList<>();
                            // cursor here
                        }
                    }
                "#},
                marker: "// cursor here",
                strip_marker: false,
            },
            ParityCase {
                name: "method params and varargs stay visible",
                source: indoc! {r#"
                    class A {
                        void f(String sep, int... numbers) {
                            // cursor here
                        }
                    }
                "#},
                marker: "// cursor here",
                strip_marker: false,
            },
            ParityCase {
                name: "var declarations keep initializer text",
                source: indoc! {r#"
                    class A {
                        void f() {
                            var map = new HashMap<String, String>();
                            var list = new ArrayList<>();
                            // cursor here
                        }
                    }
                "#},
                marker: "// cursor here",
                strip_marker: false,
            },
            ParityCase {
                name: "future declarations do not leak",
                source: indoc! {r#"
                    class A {
                        void f() {
                            int visible = 1;
                            // cursor here
                            int invisible = 2;
                        }
                    }
                "#},
                marker: "// cursor here",
                strip_marker: false,
            },
            ParityCase {
                name: "inner block locals expire outside the block",
                source: indoc! {r#"
                    class T {
                        void m() {
                            {
                                String s1 = "";
                            }
                            s1/*caret*/
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
            ParityCase {
                name: "misread declarations recover without semicolon",
                source: indoc! {r#"
                    class A {
                        void f() {
                            String s = "incomplete"
                            // cursor here
                        }
                    }
                "#},
                marker: "// cursor here",
                strip_marker: false,
            },
            ParityCase {
                name: "error nodes contribute recoverable locals",
                source: indoc! {r#"
                    class A {
                        void f() {
                            try {
                                String trapped = "value";
                                call(
                            // cursor
                        }
                    }
                "#},
                marker: "// cursor",
                strip_marker: false,
            },
            ParityCase {
                name: "misread method declarations do not pollute locals",
                source: indoc! {r#"
                    class A {
                        public static String str = "1234";

                        public static void func() {
                            str
                            func(
                                func("1234", 5678)
                            );
                            // cursor here
                        }

                        public static void func(Object o) {}
                        public static Object func(String s, int i) { return null; }
                    }
                "#},
                marker: "// cursor here",
                strip_marker: false,
            },
            ParityCase {
                name: "incomplete next declarations stay hidden",
                source: indoc! {r#"
                    class A {
                        void f() {
                            var b = make();
                            b/*caret*/ nums = makeNums();
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
            ParityCase {
                name: "broken member access does not pollute following type",
                source: indoc! {r#"
                    class T {
                        void m() {
                            RandomEnum.B.;

                            RandomRecord rc;
                            rc./*caret*/
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
        ];

        for case in cases {
            assert_incremental_matches_reference(case);
        }
    }

    #[test]
    fn test_incremental_locals_match_reference_lambda_cases() {
        let cases = [
            ParityCase {
                name: "single lambda parameter is visible",
                source: indoc! {r#"
                    class A {
                        void f() {
                            java.util.function.Function<String, Integer> fn = s -> s/*caret*/;
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
            ParityCase {
                name: "parenthesized inferred lambda parameters are visible",
                source: indoc! {r#"
                    class A {
                        void f() {
                            java.util.function.BiFunction<String, String, Integer> fn = (left, right) -> left/*caret*/;
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
            ParityCase {
                name: "zero arg lambdas do not synthesize locals",
                source: indoc! {r#"
                    class A {
                        void f() {
                            Runnable r = () -> { System.out.println(); /*caret*/ };
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
            ParityCase {
                name: "typed lambda params keep declared type",
                source: indoc! {r#"
                    import java.util.function.Function;
                    class T {
                        void m() {
                            Function<String, Integer> f = (String x) -> x/*caret*/;
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
            ParityCase {
                name: "var lambda params stay unknown",
                source: indoc! {r#"
                    import java.util.function.Function;
                    class T {
                        void m() {
                            Function<String, Integer> f = (var x) -> x/*caret*/;
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
            ParityCase {
                name: "lambda inner block locals expire after block",
                source: indoc! {r#"
                    import java.util.function.Function;

                    class T {
                        void m() {
                            Function<String, Void> f = s -> {
                                {
                                    String s1 = s.trim();
                                }
                                s1/*caret*/
                                return null;
                            };
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
        ];

        for case in cases {
            assert_incremental_matches_reference(case);
        }
    }

    #[test]
    fn test_incremental_lambda_param_names_match_source_helper() {
        let source = indoc! {r#"
            import java.util.function.BiFunction;

            class T {
                void m() {
                    BiFunction<String, String, Integer> f = (left, right) -> left/*caret*/;
                }
            }
        "#};
        let (source, offset) = prepare_source(source, "/*caret*/", true);
        let (db, _workspace, file, _name_table) = setup_case(&source);

        let incremental = extract_active_lambda_param_names_incremental(&db, file, offset);
        let from_source = extract_active_lambda_param_names_from_source(&source, offset);

        assert_eq!(incremental, from_source);
        assert_eq!(incremental, vec![Arc::from("left"), Arc::from("right")]);
    }

    #[test]
    fn test_incremental_locals_match_reference_constructor_cases() {
        let cases = [
            ParityCase {
                name: "compact constructor parameters stay visible",
                source: indoc! {r#"
                    record Point(int x, int y) {
                        public Point {
                            if (x < 0) x = 0;
                            /*caret*/
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
            ParityCase {
                name: "explicit constructor parameters are scoped correctly",
                source: indoc! {r#"
                    record Point(int x, int y) {
                        public Point(int x) {
                            this(x, 0);
                            /*caret*/
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
            ParityCase {
                name: "compact constructor preserves complex parameter types",
                source: indoc! {r#"
                    record Person(String name, int age) {
                        public Person {
                            if (name == null) throw new IllegalArgumentException();
                            /*caret*/
                        }
                    }
                "#},
                marker: "/*caret*/",
                strip_marker: true,
            },
        ];

        for case in cases {
            assert_incremental_matches_reference(case);
        }
    }

    #[test]
    fn test_incremental_locals_cache_remains_cursor_sensitive() {
        let source = indoc! {r#"
            class A {
                void f() {
                    int first = 1;
                    // early
                    int second = 2;
                    // late
                }
            }
        "#};
        let early_offset = source.find("// early").unwrap();
        let late_offset = source.find("// late").unwrap();
        let (db, workspace, file, _) = setup_case(source);

        let late = extract_visible_method_locals_incremental(&db, file, late_offset, &workspace);
        let early = extract_visible_method_locals_incremental(&db, file, early_offset, &workspace);

        let late_names: Vec<_> = late.iter().map(|local| local.name.as_ref()).collect();
        let early_names: Vec<_> = early.iter().map(|local| local.name.as_ref()).collect();

        assert!(late_names.contains(&"first"));
        assert!(late_names.contains(&"second"));
        assert!(early_names.contains(&"first"));
        assert!(
            !early_names.contains(&"second"),
            "cached locals must still be filtered by cursor: {early_names:?}"
        );
    }

    #[test]
    fn test_visible_incremental_locals_do_not_leak_catch_or_enhanced_for_vars() {
        let source_with_markers = indoc! {r#"
            class A {
                void f(Iterable<String> items) {
                    for (String item : items) {
                        item.trim();
                    }
                    item/*loop*/

                    try {
                    } catch (Exception e) {
                        e.printStackTrace();
                    }
                    e/*catch*/
                }
            }
        "#};
        let loop_marker = "/*loop*/";
        let catch_marker = "/*catch*/";
        let loop_offset = source_with_markers.find(loop_marker).unwrap();
        let catch_offset = source_with_markers.find(catch_marker).unwrap() - loop_marker.len();
        let source = source_with_markers
            .replacen(loop_marker, "", 1)
            .replacen(catch_marker, "", 1);
        let (db, workspace, file, _) = setup_case(&source);

        let loop_locals =
            extract_visible_method_locals_incremental(&db, file, loop_offset, &workspace);
        let catch_locals =
            extract_visible_method_locals_incremental(&db, file, catch_offset, &workspace);

        let loop_names: Vec<_> = loop_locals
            .iter()
            .map(|local| local.name.as_ref())
            .collect();
        let catch_names: Vec<_> = catch_locals
            .iter()
            .map(|local| local.name.as_ref())
            .collect();

        assert!(
            !loop_names.contains(&"item"),
            "enhanced-for variable leaked after loop: {loop_names:?}"
        );
        assert!(
            !catch_names.contains(&"e"),
            "catch variable leaked after catch block: {catch_names:?}"
        );
    }

    #[test]
    fn test_count_members_in_range_handles_deeply_nested_local_classes() {
        let nesting_depth = 501;
        let mut source = String::from("class Outer {\n    int top;\n    void outer() {\n");
        for _ in 0..nesting_depth {
            source.push_str("        if (true) {\n");
        }
        source.push_str(
            "            class DeepLocal {\n                int deepField;\n                void deepMethod() {}\n            }\n",
        );
        for _ in 0..nesting_depth {
            source.push_str("        }\n");
        }
        source.push_str("    }\n}\n");

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("java grammar");
        let tree = parser.parse(&source, None).expect("parsed");
        let root = tree.root_node();
        let class_node = {
            let mut cursor = root.walk();
            root.named_children(&mut cursor)
                .find(|node| node.kind() == "class_declaration")
                .expect("outer class")
        };

        let (method_count, field_count) =
            count_members_in_range(root, class_node.start_byte(), class_node.end_byte());

        assert_eq!(
            method_count, 2,
            "outer method and deep local-class method should both be counted"
        );
        assert_eq!(
            field_count, 2,
            "outer field and deep local-class field should both be counted"
        );
    }

    #[test]
    fn test_extract_java_flow_type_overrides_tracks_short_circuit_instanceof_fact() {
        let source = indoc! {r#"
            class Test {
                void demo() {
                    Object a = new StringBuilder();
                    if (a instanceof StringBuilder && a.appe/*caret*/) {
                    }
                }
            }
        "#};
        let offset = source.find("/*caret*/").expect("caret marker");
        let source = source.replacen("/*caret*/", "", 1);
        let workspace_index =
            Arc::new(parking_lot::RwLock::new(crate::index::WorkspaceIndex::new()));
        workspace_index.write().add_jdk_classes(vec![
            minimal_class("java/lang/Object"),
            minimal_class("java/lang/StringBuilder"),
        ]);
        let db = Database::with_workspace_index(Arc::clone(&workspace_index));
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));
        let name_table = resolve_name_table_for_file(&db, file).expect("name table");
        assert!(
            name_table.exists("java/lang/StringBuilder"),
            "seeded StringBuilder should be visible in the test name table"
        );
        assert!(
            SourceTypeCtx::from_overview(None, vec![], Some(name_table))
                .resolve_type_name_relaxed("StringBuilder")
                .is_some(),
            "StringBuilder should resolve before flow extraction"
        );

        let overrides = extract_java_flow_type_overrides(&db, file, offset);

        assert_eq!(
            overrides.as_ref(),
            &[FlowTypeOverrideData {
                local_name: Arc::from("a"),
                narrowed_type: Arc::from("Ljava/lang/StringBuilder;"),
            }]
        );
    }
}
