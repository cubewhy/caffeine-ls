use std::{collections::HashSet, sync::Arc};

use tree_sitter::{Node, Query};

use crate::{
    language::{
        java::{
            JavaContextExtractor,
            type_ctx::{SourceTypeCtx, extract_param_type},
            utils::{find_ancestor, get_initializer_text, java_type_to_internal},
        },
        ts_utils::{find_method_by_offset, run_query},
    },
    semantic::{LocalVar, types::type_name::TypeName},
};

#[derive(Debug, Clone)]
struct RankedLocal {
    local: LocalVar,
    declaration_start: usize,
    visibility_scope: ScopeRange,
}

#[derive(Debug, Clone, Copy)]
struct ScopeRange {
    start: usize,
    end: usize,
}

pub fn extract_locals(
    ctx: &JavaContextExtractor,
    root: Node,
    cursor_node: Option<Node>,
) -> Vec<LocalVar> {
    extract_locals_with_type_ctx(ctx, root, cursor_node, None)
}

pub fn extract_locals_with_type_ctx(
    ctx: &JavaContextExtractor,
    root: Node,
    cursor_node: Option<Node>,
    type_ctx: Option<&SourceTypeCtx>,
) -> Vec<LocalVar> {
    let search_root = cursor_node
        .and_then(|n| find_ancestor(n, "method_declaration"))
        .or_else(|| find_method_by_offset(root, ctx.offset))
        .unwrap_or(root);
    let query_src = r#"
            (local_variable_declaration
                type: (_) @type
                declarator: (variable_declarator
                    name: (identifier) @name))
        "#;
    let q = match Query::new(&tree_sitter_java::LANGUAGE.into(), query_src) {
        Ok(q) => q,
        Err(e) => {
            tracing::debug!("local var query error: {}", e);
            return vec![];
        }
    };
    let type_idx = q.capture_index_for_name("type").unwrap();
    let name_idx = q.capture_index_for_name("name").unwrap();
    let mut vars: Vec<RankedLocal> = run_query(&q, search_root, ctx.bytes(), None)
        .into_iter()
        .filter_map(|captures| {
            let ty_node = captures.iter().find(|(idx, _)| *idx == type_idx)?.1;
            let name_node = captures.iter().find(|(idx, _)| *idx == name_idx)?.1;
            if ty_node.start_byte() >= ctx.offset {
                return None;
            }

            let declarator = name_node.parent()?; // variable_declarator
            let decl = declarator.parent()?; // local_variable_declaration
            let visibility_scope =
                local_visibility_scope(decl).unwrap_or_else(|| fallback_visibility_scope(ctx.offset));
            if !is_visible_local_declaration_before_cursor(ctx.offset, name_node, declarator, decl)
                || !scope_contains_offset(visibility_scope, ctx.offset)
            {
                return None;
            }

            // Pattern 1: The declarator contains an argument list (direct method calls are inserted into it)
            {
                let mut dc = declarator.walk();
                if declarator
                    .children(&mut dc)
                    .any(|c| c.kind() == "argument_list")
                {
                    return None;
                }
            }

            // Pattern 2: Zero-length semicolon + next sibling begins with `(` (method call after a newline)
            if let Some(next) = decl.next_sibling() {
                let next_text = &ctx.source[next.start_byte()..next.end_byte()];
                if next_text.trim_start().starts_with('(') {
                    return None;
                }
            }

            let ty = ty_node.utf8_text(ctx.bytes()).ok()?;
            let name = name_node.utf8_text(ctx.bytes()).ok()?;
            tracing::debug!(
                ty,
                name,
                start = ty_node.start_byte(),
                offset = ctx.offset,
                "extracted local var"
            );
            let raw_ty = ty.trim();

            if raw_ty == "var" {
                return Some(RankedLocal {
                    local: LocalVar {
                        name: Arc::from(name),
                        type_internal: TypeName::new("var"),
                        init_expr: get_initializer_text(ty_node, ctx.bytes()),
                    },
                    declaration_start: name_node.start_byte(),
                    visibility_scope,
                });
            }

            Some(RankedLocal {
                local: LocalVar {
                    name: Arc::from(name),
                    type_internal: resolve_declared_source_type(raw_ty, type_ctx),
                    init_expr: None,
                },
                declaration_start: name_node.start_byte(),
                visibility_scope,
            })
        })
        .collect();

    vars.extend(extract_misread_var_decls(ctx, search_root, type_ctx));
    vars.extend(extract_locals_from_error_nodes(ctx, search_root, type_ctx));
    vars.extend(extract_params(ctx, root, cursor_node, type_ctx));
    vars.extend(extract_lambda_params(ctx, cursor_node));

    normalize_visible_locals(vars)
}

fn resolve_declared_source_type(raw_ty: &str, type_ctx: Option<&SourceTypeCtx>) -> TypeName {
    if let Some(type_ctx) = type_ctx
        && let Some(resolved) = type_ctx.resolve_type_name_relaxed(raw_ty.trim())
    {
        // Avoid committing a lossy base-only relaxed result for generic source types.
        // Keep raw fallback so later scoped expansion can recover argument structure.
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
) -> Vec<RankedLocal> {
    let mut result = Vec::new();
    collect_misread_decls(ctx, root, &mut result, type_ctx);
    result
}

fn collect_misread_decls(
    ctx: &JavaContextExtractor,
    node: Node,
    vars: &mut Vec<RankedLocal>,
    type_ctx: Option<&SourceTypeCtx>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Pattern: variable_declarator containing assignment_expression
        // where the declarator's ERROR sibling contains a type keyword
        if child.kind() == "variable_declarator" {
            // Look for assignment_expression inside this declarator
            let mut vc = child.walk();
            for vchild in child.children(&mut vc) {
                if vchild.kind() == "assignment_expression" {
                    // Left side is the misread variable name
                    let lhs = vchild.child_by_field_name("left").or_else(|| {
                        let mut wc = vchild.walk();
                        vchild.named_children(&mut wc).next()
                    });
                    let rhs = vchild.child_by_field_name("right").or_else(|| {
                        let mut wc = vchild.walk();
                        vchild.named_children(&mut wc).nth(1)
                    });
                    if let (Some(name_node), Some(init_node)) = (lhs, rhs) {
                        if name_node.kind() != "identifier" {
                            continue;
                        }
                        if name_node.start_byte() >= ctx.offset {
                            continue;
                        }
                        let name = ctx.node_text(name_node);
                        // Find type from ERROR sibling (contains "var" or type_identifier)
                        let type_name = find_type_in_error_sibling(ctx, child);
                        let init_text = ctx.node_text(init_node).to_string();
                        let lv = if type_name.as_deref() == Some("var") {
                            LocalVar {
                                name: Arc::from(name),
                                type_internal: TypeName::new("var"),
                                init_expr: Some(init_text),
                            }
                        } else {
                            let raw_ty = type_name.as_deref().unwrap_or("Object");
                            LocalVar {
                                name: Arc::from(name),
                                type_internal: resolve_declared_source_type(raw_ty, type_ctx),
                                init_expr: None,
                            }
                        };
                        vars.push(RankedLocal {
                            local: lv,
                            declaration_start: name_node.start_byte(),
                            visibility_scope: local_visibility_scope(child)
                                .unwrap_or_else(|| fallback_visibility_scope(ctx.offset)),
                        });
                    }
                }
            }
        }
        collect_misread_decls(ctx, child, vars, type_ctx);
    }
}

fn find_type_in_error_sibling(ctx: &JavaContextExtractor, declarator_node: Node) -> Option<String> {
    // Look inside ERROR children of the declarator for type_identifier
    let mut cursor = declarator_node.walk();
    for child in declarator_node.children(&mut cursor) {
        if child.kind() == "ERROR" {
            let mut ec = child.walk();
            for ec_child in child.children(&mut ec) {
                if ec_child.kind() == "type_identifier"
                    || ec_child.kind() == "integral_type"
                    || ec_child.kind() == "void_type"
                {
                    return Some(ctx.node_text(ec_child).to_string());
                }
            }
        }
    }
    None
}

fn extract_params(
    ctx: &JavaContextExtractor,
    root: Node,
    cursor_node: Option<Node>,
    type_ctx: Option<&SourceTypeCtx>,
) -> Vec<RankedLocal> {
    let method = match cursor_node
        .and_then(|n| find_ancestor(n, "method_declaration"))
        .or_else(|| find_method_by_offset(root, ctx.offset))
    {
        Some(m) => m,
        None => return vec![],
    };
    let Some(params_node) = method.child_by_field_name("parameters") else {
        return vec![];
    };
    let mut vars = Vec::new();
    let mut cursor = params_node.walk();
    for child in params_node.children(&mut cursor) {
        if !matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }
        let name_node = child.child_by_field_name("name").or_else(|| {
            let mut nc = child.walk();
            for n in child.named_children(&mut nc) {
                if n.kind() == "identifier" {
                    return Some(n);
                }
                if n.kind() == "variable_declarator" {
                    if let Some(named) = n.child_by_field_name("name") {
                        return Some(named);
                    }
                    let mut vc = n.walk();
                    if let Some(id) = n.named_children(&mut vc).find(|c| c.kind() == "identifier") {
                        return Some(id);
                    }
                }
            }
            None
        });
        let Some(name_node) = name_node else {
            continue;
        };
        let name = ctx.node_text(name_node).to_string();
        let mut raw_ty = extract_param_type(ctx.node_text(child)).trim().to_string();
        if child.kind() == "spread_parameter" || raw_ty.ends_with("...") {
            raw_ty = if let Some(stripped) = raw_ty.strip_suffix("...") {
                format!("{}[]", stripped.trim())
            } else {
                format!("{}[]", raw_ty.trim())
            };
        }
        vars.push(RankedLocal {
            local: LocalVar {
                name: Arc::from(name),
                type_internal: resolve_declared_source_type(raw_ty.as_str(), type_ctx),
                init_expr: None,
            },
            declaration_start: name_node.start_byte(),
            visibility_scope: method_like_body_scope(method)
                .unwrap_or_else(|| fallback_visibility_scope(ctx.offset)),
        });
    }
    vars
}

fn extract_lambda_params(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
) -> Vec<RankedLocal> {
    let mut lambdas = Vec::new();
    let mut current = cursor_node;
    while let Some(node) = current {
        if node.kind() == "lambda_expression" {
            lambdas.push(node);
        }
        current = node.parent();
    }
    lambdas.reverse();

    let mut vars = Vec::new();
    for lambda in lambdas {
        let Some(body) = lambda.child_by_field_name("body") else {
            continue;
        };
        if ctx.offset < body.start_byte() || ctx.offset > body.end_byte() {
            continue;
        }

        let Some(params) = lambda.child_by_field_name("parameters") else {
            continue;
        };
        vars.extend(extract_lambda_params_from_node(ctx, params));
    }
    vars
}

fn extract_lambda_params_from_node(ctx: &JavaContextExtractor, params: Node) -> Vec<RankedLocal> {
    extract_lambda_param_names_with_starts(ctx, params)
        .into_iter()
        .map(|(name, declaration_start)| RankedLocal {
            local: LocalVar {
                name,
                type_internal: TypeName::new("unknown"),
                init_expr: None,
            },
            declaration_start,
            visibility_scope: lambda_param_visibility_scope(params)
                .unwrap_or_else(|| fallback_visibility_scope(ctx.offset)),
        })
        .collect()
}

pub(crate) fn extract_active_lambda_param_names(
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
        current = node.parent();
    }
    vec![]
}

fn extract_lambda_param_names(ctx: &JavaContextExtractor, params: Node) -> Vec<Arc<str>> {
    extract_lambda_param_names_with_starts(ctx, params)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

fn extract_lambda_param_names_with_starts(
    ctx: &JavaContextExtractor,
    params: Node,
) -> Vec<(Arc<str>, usize)> {
    match params.kind() {
        "identifier" => vec![(Arc::from(ctx.node_text(params)), params.start_byte())],
        "inferred_parameters" => {
            let mut vars = Vec::new();
            let mut cursor = params.walk();
            for child in params.named_children(&mut cursor) {
                if child.kind() != "identifier" {
                    continue;
                }
                vars.push((Arc::from(ctx.node_text(child)), child.start_byte()));
            }
            vars
        }
        "formal_parameters" => {
            let mut vars = Vec::new();
            let mut cursor = params.walk();
            for child in params.named_children(&mut cursor) {
                if !matches!(child.kind(), "formal_parameter" | "spread_parameter") {
                    continue;
                }
                let Some(name_node) = child.child_by_field_name("name") else {
                    continue;
                };
                vars.push((Arc::from(ctx.node_text(name_node)), name_node.start_byte()));
            }
            vars
        }
        _ => vec![],
    }
}

fn normalize_visible_locals(mut vars: Vec<RankedLocal>) -> Vec<LocalVar> {
    // Normalize the visible local table for editor recovery:
    // declarations nearest to the caret win duplicate-name conflicts, regardless
    // of which extractor produced them.
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

fn extract_locals_from_error_nodes(
    ctx: &JavaContextExtractor,
    root: Node,
    type_ctx: Option<&SourceTypeCtx>,
) -> Vec<RankedLocal> {
    let mut result = Vec::new();
    collect_locals_in_errors(ctx, root, &mut result, type_ctx);
    result
}

fn collect_locals_in_errors(
    ctx: &JavaContextExtractor,
    node: Node,
    vars: &mut Vec<RankedLocal>,
    type_ctx: Option<&SourceTypeCtx>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "ERROR" {
            // ERROR may contain local_variable_declaration
            let q_src = r#"
                (local_variable_declaration
                    type: (_) @type
                    declarator: (variable_declarator
                        name: (identifier) @name))
            "#;
            if let Ok(q) = Query::new(&tree_sitter_java::LANGUAGE.into(), q_src) {
                let type_idx = q.capture_index_for_name("type").unwrap();
                let name_idx = q.capture_index_for_name("name").unwrap();
                let found: Vec<RankedLocal> = run_query(&q, child, ctx.bytes(), None)
                    .into_iter()
                    .filter_map(|captures| {
                        let ty_node = captures.iter().find(|(idx, _)| *idx == type_idx)?.1;
                        let name_node = captures.iter().find(|(idx, _)| *idx == name_idx)?.1;
                        if ty_node.start_byte() >= ctx.offset {
                            return None;
                        }
                        let declarator = name_node.parent()?;
                        let decl = declarator.parent()?;
                        let visibility_scope = local_visibility_scope(decl)
                            .unwrap_or_else(|| fallback_visibility_scope(ctx.offset));
                        if !is_visible_local_declaration_before_cursor(
                            ctx.offset, name_node, declarator, decl,
                        ) || !scope_contains_offset(visibility_scope, ctx.offset)
                        {
                            return None;
                        }
                        let ty = ty_node.utf8_text(ctx.bytes()).ok()?;
                        let name = name_node.utf8_text(ctx.bytes()).ok()?;
                        let raw_ty = ty.trim();

                        if raw_ty == "var" {
                            return Some(RankedLocal {
                                local: LocalVar {
                                    name: Arc::from(name),
                                    type_internal: TypeName::new("var"),
                                    init_expr: get_initializer_text(ty_node, ctx.bytes()),
                                },
                                declaration_start: name_node.start_byte(),
                                visibility_scope,
                            });
                        }

                        Some(RankedLocal {
                            local: LocalVar {
                                name: Arc::from(name),
                                type_internal: resolve_declared_source_type(raw_ty, type_ctx),
                                init_expr: None,
                            },
                            declaration_start: name_node.start_byte(),
                            visibility_scope,
                        })
                    })
                    .collect();
                vars.extend(found);
            }
            // Recursive entry into nested ERROR
            collect_locals_in_errors(ctx, child, vars, type_ctx);
        } else {
            collect_locals_in_errors(ctx, child, vars, type_ctx);
        }
    }
}

fn is_visible_local_declaration_before_cursor(
    offset: usize,
    name_node: Node,
    declarator: Node,
    decl: Node,
) -> bool {
    // Local becomes in-scope only after its declarator name is present.
    // This blocks malformed/in-progress declarations (e.g. partial type token)
    // from polluting locals with bogus type/name pairs.
    name_node.start_byte() < offset
        && declarator.start_byte() < offset
        && decl.start_byte() < offset
}

fn local_visibility_scope(decl: Node) -> Option<ScopeRange> {
    let owner = nearest_local_scope_owner(decl)?;
    scope_range_for_owner(owner)
}

fn lambda_param_visibility_scope(params: Node) -> Option<ScopeRange> {
    let lambda = params.parent()?;
    if lambda.kind() != "lambda_expression" {
        return None;
    }
    let body = lambda.child_by_field_name("body")?;
    Some(node_scope_range(body))
}

fn method_like_body_scope(method: Node) -> Option<ScopeRange> {
    scope_range_for_owner(method)
}

fn nearest_local_scope_owner(node: Node) -> Option<Node> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if is_local_scope_owner(candidate.kind()) {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

fn is_local_scope_owner(kind: &str) -> bool {
    matches!(
        kind,
        "block"
            | "for_statement"
            | "enhanced_for_statement"
            | "catch_clause"
            | "switch_block_statement_group"
            | "switch_rule"
            | "method_declaration"
            | "constructor_declaration"
            | "lambda_expression"
            | "static_initializer"
            | "instance_initializer"
    )
}

fn scope_range_for_owner(owner: Node) -> Option<ScopeRange> {
    let scope_node = match owner.kind() {
        "method_declaration" | "constructor_declaration" | "lambda_expression" => {
            owner.child_by_field_name("body")?
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn setup(source: &str, offset: usize) -> (JavaContextExtractor, tree_sitter::Tree) {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("failed to load java grammar");
        let tree = parser.parse(source, None).unwrap();

        let ctx = JavaContextExtractor::new(source, offset, None);
        (ctx, tree)
    }

    #[test]
    fn test_extract_standard_locals() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                int a = 1;
                String b = "hello";
                List<String> c = new ArrayList<>();
                // cursor here
            }
        }
        "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        // 验证提取结果
        assert!(vars.iter().any(
            |v| v.name.as_ref() == "a" && v.type_internal.to_internal_with_generics() == "int"
        ));
        assert!(
            vars.iter().any(|v| v.name.as_ref() == "b"
                && v.type_internal.to_internal_with_generics() == "String")
        );
        assert!(
            vars.iter().any(|v| v.name.as_ref() == "c"
                && v.type_internal.to_internal_with_generics() == "List<String>"),
            "Should preserve generics. Found types: {:?}",
            vars.iter()
                .map(|v| v.type_internal.to_internal_with_generics())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_extract_params() {
        let src = indoc::indoc! {r#"
        class A {
            void f(int p1, String p2) {
                // cursor here
            }
        }
        "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(vars.iter().any(
            |v| v.name.as_ref() == "p1" && v.type_internal.to_internal_with_generics() == "int"
        ));
        assert!(
            vars.iter().any(|v| v.name.as_ref() == "p2"
                && v.type_internal.to_internal_with_generics() == "String")
        );
    }

    #[test]
    fn test_extract_varargs_params_as_array_type() {
        let src = indoc::indoc! {r#"
        class A {
            void f(String sep, int... numbers) {
                // cursor here
            }
        }
        "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            vars.iter().any(|v| v.name.as_ref() == "numbers"
                && v.type_internal.to_internal_with_generics() == "int[]"),
            "vars={:?}",
            vars.iter()
                .map(|v| format!("{}:{}", v.name, v.type_internal.to_internal_with_generics()))
                .collect::<Vec<_>>()
        );
        assert!(
            vars.iter().any(|v| v.name.as_ref() == "sep"
                && v.type_internal.to_internal_with_generics() == "String"),
            "vars={:?}",
            vars.iter()
                .map(|v| format!("{}:{}", v.name, v.type_internal.to_internal_with_generics()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_var_capture_init_expr() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                var map = new HashMap<String, String>();
                var list = new ArrayList<>();
                // cursor here
            }
        }
        "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        let map_var = vars
            .iter()
            .find(|v| v.name.as_ref() == "map")
            .expect("Should find map");
        assert_eq!(map_var.type_internal.erased_internal(), "var");
        assert!(map_var.init_expr.as_ref().unwrap().contains("new HashMap"));
    }

    #[test]
    fn test_var_inference_fallback() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                var unknown = someMethodCall();
                // cursor here
            }
        }
        "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        // 无法推断时，类型应为 "var"，并且携带初始化表达式以供后续分析（如果支持的话）
        let v = vars.iter().find(|v| v.name.as_ref() == "unknown").unwrap();
        assert_eq!(v.type_internal.erased_internal(), "var");
        assert_eq!(v.init_expr.as_deref(), Some("someMethodCall()"));
    }

    #[test]
    fn test_scope_visibility_ignore_future_vars() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                int visible = 1;
                // cursor here
                int invisible = 2;
            }
        }
        "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(vars.iter().any(|v| v.name.as_ref() == "visible"));
        assert!(!vars.iter().any(|v| v.name.as_ref() == "invisible"));
    }

    #[test]
    fn test_inner_block_local_does_not_leak_in_method_body() {
        let src = indoc::indoc! {r#"
        class T {
            void m() {
                {
                    String s1 = "";
                }
                s1
            }
        }
        "#};
        let offset = src.find("s1\n").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            !vars.iter().any(|v| v.name.as_ref() == "s1"),
            "inner-block local must not leak: {:?}",
            vars.iter().map(|v| v.name.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_inner_block_local_stays_visible_inside_own_block() {
        let src = indoc::indoc! {r#"
        class T {
            void m() {
                {
                    String s1 = "";
                    s1
                }
            }
        }
        "#};
        let offset = src.rfind("s1\n").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            vars.iter().any(|v| v.name.as_ref() == "s1"),
            "inner-block local should stay visible inside block: {:?}",
            vars.iter().map(|v| v.name.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_lambda_inner_block_local_does_not_leak_after_block() {
        let src = indoc::indoc! {r#"
        import java.util.function.Function;

        class T {
            void m() {
                Function<String, Void> f = s -> {
                    {
                        String s1 = s.trim();
                    }
                    s1
                    return null;
                };
            }
        }
        "#};
        let offset = src.find("s1\n").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            vars.iter().any(|v| v.name.as_ref() == "s"),
            "lambda parameter should remain visible: {:?}",
            vars.iter().map(|v| v.name.as_ref()).collect::<Vec<_>>()
        );
        assert!(
            !vars.iter().any(|v| v.name.as_ref() == "s1"),
            "expired inner-block local must not leak out of lambda block: {:?}",
            vars.iter().map(|v| v.name.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_misread_declaration_missing_semicolon() {
        // 这是 collect_misread_decls 的重点测试场景
        // Tree-sitter 经常把没有分号的 `String s = "v"` 解析为：
        // variable_declarator 内部包含了一个 assignment_expression
        // 且类型 `String` 变成了一个 ERROR 节点或游离的 identifier
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                String s = "incomplete"
                // cursor here
            }
        }
        "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        // 期望能从容错逻辑中提取出 s
        assert!(
            vars.iter().any(|v| v.name.as_ref() == "s"
                && v.type_internal.to_internal_with_generics() == "String"),
            "Should parse variable 's' even without semicolon. Found: {:?}",
            vars.iter().map(|v| v.name.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_misread_var_capture_init_expr() {
        // 测试在语法错误（缺分号）情况下的 var 推断
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                var x = new HashSet<>()
                // cursor here
            }
        }
        "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        // 注意：collect_misread_decls 内部逻辑是如果检测到 var，
        // type_internal 设为 "var"，init_expr 设为右值
        let x = vars
            .iter()
            .find(|v| v.name.as_ref() == "x")
            .expect("Should find 'x'");
        assert_eq!(x.type_internal.erased_internal(), "var");
        assert!(x.init_expr.as_ref().unwrap().contains("new HashSet"));
    }

    #[test]
    fn test_locals_inside_error_nodes() {
        // Recovery should keep locals from an open malformed block visible.
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                try {
                    String insideError = "ok";
                    call(
                // cursor
            }
        }
        "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);

        let vars = extract_locals_from_error_nodes(&ctx, tree.root_node(), None);

        assert!(
            vars.iter().any(|v| v.local.name.as_ref() == "insideError"),
            "Should recover locals from an open malformed block"
        );
    }

    #[test]
    fn test_no_false_local_from_misread_method_decl() {
        // str 缺分号 + 紧跟方法调用，TS 会把整体误读为
        // local_variable_declaration(type=str, declarator=func(...))
        // func 不应该出现在局部变量表里，str 也不应出现
        let src = indoc::indoc! {r#"
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
    "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            !vars.iter().any(|v| v.name.as_ref() == "func"),
            "`func` must not appear as a local variable (it's a method name misread as declarator). \
         Found: {:?}",
            vars.iter().map(|v| v.name.as_ref()).collect::<Vec<_>>()
        );
        assert!(
            !vars.iter().any(|v| v.name.as_ref() == "str"),
            "`str` must not appear as a local variable (it was a misread type annotation)"
        );
    }

    #[test]
    fn test_incomplete_next_declaration_does_not_pollute_locals() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                var b = make();
                b nums = makeNums();
            }
        }
        "#};
        let marker = "b nums";
        let offset = src.find(marker).unwrap() + 1; // cursor right after the partial type token `b`
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            vars.iter().any(|v| v.name.as_ref() == "b"),
            "previous declaration should still be visible"
        );
        assert!(
            !vars.iter().any(|v| v.name.as_ref() == "nums"),
            "in-progress next declaration must not leak as local"
        );
    }

    #[test]
    fn test_locals_from_error_nodes_are_included() {
        // The unified extractor should include recoverable locals from an open malformed block.
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            try {
                String trapped = "value";
                call(
            // cursor
        }
    }
    "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            vars.iter().any(|v| v.name.as_ref() == "trapped"),
            "extract_locals should include recoverable vars from ERROR nodes. \
         Found: {:?}",
            vars.iter().map(|v| v.name.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_misread_method_multiple_statements_before_cursor() {
        // 混合场景：正常变量 + 误读方法 + cursor，确保正常变量不受影响
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            int legit = 42;
            String also = "ok";
            badMethod   // 缺分号，下一行的调用会触发误读
            doSomething();
            // cursor here
        }
        void badMethod() {}
        void doSomething() {}
    }
    "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            vars.iter().any(|v| v.name.as_ref() == "legit"),
            "legit should be extracted"
        );
        assert!(
            vars.iter().any(|v| v.name.as_ref() == "also"),
            "also should be extracted"
        );
        // doSomething 不能作为变量名出现
        assert!(
            !vars.iter().any(|v| v.name.as_ref() == "doSomething"),
            "method name must not appear as local var"
        );
    }

    #[test]
    fn test_extract_standard_locals_with_generics() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                List<String> c = new ArrayList<>();
                // cursor here
            }
        }
        "#};
        let offset = src.find("// cursor").unwrap();
        let (ctx, tree) = setup(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            vars.iter().any(|v| v.name.as_ref() == "c"
                && v.type_internal.to_internal_with_generics() == "List<String>"),
            "Should preserve generics exactly as in source. Found types: {:?}",
            vars.iter()
                .map(|v| v.type_internal.to_internal_with_generics())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_extract_single_lambda_param_as_local() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                java.util.function.Function<String, Integer> fn = s -> s/*caret*/;
            }
        }
        "#};
        let offset = src.find("/*caret*/").unwrap();
        let src = src.replacen("/*caret*/", "", 1);
        let (ctx, tree) = setup(&src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            vars.iter().any(|v| {
                v.name.as_ref() == "s" && v.type_internal.to_internal_with_generics() == "unknown"
            }),
            "lambda param should be visible as a placeholder local: {:?}",
            vars.iter()
                .map(|v| format!("{}:{}", v.name, v.type_internal.to_internal_with_generics()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_extract_parenthesized_lambda_params_as_locals() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                java.util.function.BiFunction<String, String, Integer> fn = (left, right) -> left/*caret*/;
            }
        }
        "#};
        let offset = src.find("/*caret*/").unwrap();
        let src = src.replacen("/*caret*/", "", 1);
        let (ctx, tree) = setup(&src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            vars.iter().any(|v| v.name.as_ref() == "left"),
            "left should be visible: {:?}",
            vars.iter().map(|v| v.name.as_ref()).collect::<Vec<_>>()
        );
        assert!(
            vars.iter().any(|v| v.name.as_ref() == "right"),
            "right should be visible: {:?}",
            vars.iter().map(|v| v.name.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_zero_arg_lambda_does_not_add_fake_locals() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                Runnable r = () -> { System.out.println(); /*caret*/ };
            }
        }
        "#};
        let offset = src.find("/*caret*/").unwrap();
        let src = src.replacen("/*caret*/", "", 1);
        let (ctx, tree) = setup(&src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let vars = extract_locals(&ctx, tree.root_node(), cursor_node);

        assert!(
            vars.iter().all(|v| v.name.as_ref() != "()"),
            "zero-arg lambdas must not synthesize fake parameter locals: {:?}",
            vars.iter().map(|v| v.name.as_ref()).collect::<Vec<_>>()
        );
    }
}
