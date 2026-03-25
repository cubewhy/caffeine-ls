use std::sync::Arc;

use tree_sitter::{Node, Query};
use tree_sitter_utils::{Handler, HandlerExt, Input};

use crate::language::{
    java::JavaContextExtractor,
    java::utils::{
        is_in_name_position, statement_label_target_kind, unwrap_labeled_statement_target,
    },
    ts_utils::{capture_text, run_query},
};
use crate::semantic::context::StatementLabel;

pub fn extract_package(ctx: &JavaContextExtractor, root: Node) -> Option<Arc<str>> {
    let q = Query::new(
        &tree_sitter_java::LANGUAGE.into(),
        r#"(package_declaration) @pkg"#,
    )
    .ok()?;
    let idx = q.capture_index_for_name("pkg")?;
    let results = run_query(&q, root, ctx.bytes(), None);
    let text = results
        .first()
        .and_then(|caps| capture_text(caps, idx, ctx.bytes()))?;
    let pkg = text
        .trim_start_matches("package")
        .trim()
        .trim_end_matches(';')
        .trim()
        .replace('.', "/");
    Some(Arc::from(pkg.as_str()))
}

pub fn extract_imports(ctx: &JavaContextExtractor, root: Node) -> Vec<Arc<str>> {
    let q = match Query::new(
        &tree_sitter_java::LANGUAGE.into(),
        r#"(import_declaration) @import"#,
    ) {
        Ok(q) => q,
        Err(_) => return vec![],
    };
    let idx = q.capture_index_for_name("import").unwrap();
    run_query(&q, root, ctx.bytes(), None)
        .into_iter()
        .filter_map(|caps| {
            let text = capture_text(&caps, idx, ctx.bytes())?;
            let cleaned = text
                .trim_start_matches("import")
                .trim()
                .trim_end_matches(';')
                .trim();
            if cleaned.starts_with("static ") {
                return None; // handled by extract_static_imports
            }
            if cleaned.is_empty() {
                None
            } else {
                Some(Arc::from(cleaned))
            }
        })
        .collect()
}

pub fn extract_static_imports(ctx: &JavaContextExtractor, root: Node) -> Vec<Arc<str>> {
    let q = match Query::new(
        &tree_sitter_java::LANGUAGE.into(),
        r#"(import_declaration) @import"#,
    ) {
        Ok(q) => q,
        Err(_) => return vec![],
    };
    let idx = q.capture_index_for_name("import").unwrap();
    run_query(&q, root, ctx.bytes(), None)
        .into_iter()
        .filter_map(|caps| {
            let text = capture_text(&caps, idx, ctx.bytes())?;
            let after_import = text.trim_start_matches("import").trim();
            if !after_import.starts_with("static ") {
                return None;
            }
            let cleaned = after_import
                .trim_start_matches("static")
                .trim()
                .trim_end_matches(';')
                .trim();
            if cleaned.is_empty() {
                None
            } else {
                Some(Arc::from(cleaned))
            }
        })
        .collect()
}

pub fn extract_enclosing_class(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
) -> Option<Arc<str>> {
    let class_node = cursor_node.and_then(nearest_type_declaration)?;
    let name_node = class_node.child_by_field_name("name")?;
    Some(Arc::from(ctx.node_text(name_node)))
}

pub(crate) fn extract_enclosing_class_chain(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
) -> Option<Vec<Arc<str>>> {
    let decl = cursor_node.and_then(nearest_type_declaration)?;
    Some(extract_type_declaration_chain(ctx, decl))
}

pub(crate) fn extract_enclosing_class_by_offset(
    ctx: &JavaContextExtractor,
    root: Node,
) -> Option<Arc<str>> {
    let mut result: Option<Arc<str>> = None;
    fn dfs<'a>(node: Node<'a>, offset: usize, bytes: &[u8], result: &mut Option<Arc<str>>) {
        if node.start_byte() > offset || node.end_byte() <= offset {
            return;
        }
        if node.kind() == "class_declaration"
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(bytes)
        {
            *result = Some(Arc::from(name));
        }
        // Handle top-level ERROR: find `class` keyword followed by identifier
        if node.kind() == "ERROR" {
            let children: Vec<Node> = {
                let mut cursor = node.walk();
                node.children(&mut cursor).collect()
            };
            for i in 0..children.len().saturating_sub(1) {
                if children[i].kind() == "class"
                    && children[i + 1].kind() == "identifier"
                    && let Ok(name) = children[i + 1].utf8_text(bytes)
                {
                    *result = Some(Arc::from(name));
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            dfs(child, offset, bytes, result);
        }
    }
    dfs(root, ctx.offset, ctx.bytes(), &mut result);
    result
}

pub(crate) fn extract_enclosing_class_chain_by_offset(
    ctx: &JavaContextExtractor,
    root: Node,
) -> Option<Vec<Arc<str>>> {
    let mut result: Option<Vec<Arc<str>>> = None;

    fn dfs<'a>(
        ctx: &JavaContextExtractor,
        node: Node<'a>,
        offset: usize,
        stack: &mut Vec<Arc<str>>,
        result: &mut Option<Vec<Arc<str>>>,
    ) {
        if node.start_byte() > offset || node.end_byte() <= offset {
            return;
        }

        let pushed = if is_type_declaration_kind(node.kind()) {
            if let Some(name_node) = node.child_by_field_name("name") {
                stack.push(Arc::from(ctx.node_text(name_node)));
                true
            } else {
                false
            }
        } else {
            false
        };

        if !stack.is_empty() {
            *result = Some(stack.clone());
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            dfs(ctx, child, offset, stack, result);
        }

        if pushed {
            stack.pop();
        }
    }

    let mut stack = Vec::new();
    dfs(ctx, root, ctx.offset, &mut stack, &mut result);
    result
}

pub(crate) fn extract_enclosing_internal_name(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    enclosing_package: Option<&Arc<str>>,
) -> Option<Arc<str>> {
    let decl = cursor_node.and_then(nearest_type_declaration)?;

    // Collect all type names from innermost to outermost
    let names: Vec<String> = {
        let mut result = Vec::new();
        let mut current = Some(decl);
        while let Some(node) = current {
            if let Some(name_node) = node.child_by_field_name("name") {
                result.push(ctx.node_text(name_node).to_string());
            }
            current = find_parent_type_declaration(node);
        }
        result
    };

    if names.is_empty() {
        return None;
    }

    let mut internal = String::new();
    if let Some(pkg) = enclosing_package {
        internal.push_str(pkg);
        internal.push('/');
    }
    internal.push_str(&names[names.len() - 1]);
    for nested in names.iter().rev().skip(1) {
        internal.push('$');
        internal.push_str(nested);
    }

    Some(Arc::from(internal))
}

fn extract_type_declaration_chain(ctx: &JavaContextExtractor, decl: Node) -> Vec<Arc<str>> {
    let mut names = Vec::new();
    let mut current = Some(decl);
    while let Some(node) = current {
        let Some(name_node) = node.child_by_field_name("name") else {
            break;
        };
        names.push(Arc::from(ctx.node_text(name_node)));
        current = find_parent_type_declaration(node);
    }
    names.reverse();
    names
}

pub(crate) fn is_cursor_in_class_member_position(cursor_node: Option<Node>) -> bool {
    let Some(cursor) = cursor_node else {
        return false;
    };

    let Some(type_body) = find_nearest_type_body(cursor) else {
        return false;
    };

    let mut current = Some(cursor);
    while let Some(node) = current {
        if node.id() == type_body.id() {
            break;
        }
        if is_executable_or_nested_body_context(node.kind())
            && !is_member_declaration_name_context(cursor, node, type_body)
        {
            return false;
        }
        current = node.parent();
    }

    let Some(owner_decl) = type_body.parent() else {
        return false;
    };
    if !is_type_declaration_kind(owner_decl.kind()) {
        return false;
    }

    // Local type declarations (declared inside executable bodies) are not valid override targets.
    let mut parent = owner_decl.parent();
    while let Some(node) = parent {
        if is_executable_or_nested_body_context(node.kind()) {
            return false;
        }
        parent = node.parent();
    }

    true
}

pub(crate) fn extract_enclosing_statement_labels(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
) -> Vec<StatementLabel> {
    let Some(start_node) = cursor_node else {
        return vec![];
    };

    // Build a handler that extracts labels from labeled_statement nodes
    let label_handler = (|inp: Input<&JavaContextExtractor>| -> Option<StatementLabel> {
        let ctx = inp.ctx;

        // Find the identifier child
        let name_node = {
            let mut cursor = inp.node.walk();
            inp.node
                .named_children(&mut cursor)
                .find(|child| child.kind() == "identifier")?
        };

        // Find the target (non-identifier child)
        let target_kind = {
            let mut cursor = inp.node.walk();
            inp.node
                .named_children(&mut cursor)
                .find(|child| child.kind() != "identifier")
                .map(unwrap_labeled_statement_target)
                .map(statement_label_target_kind)
                .unwrap_or_else(|| statement_label_target_kind(inp.node))
        };

        let name = ctx.node_text(name_node).trim();
        if !name.is_empty() {
            Some(StatementLabel {
                name: Arc::from(name),
                target_kind,
            })
        } else {
            None
        }
    })
    .for_kinds(&["labeled_statement"]);

    // Collect labels by climbing the tree
    let mut labels = Vec::new();
    let mut current = Some(start_node);

    while let Some(node) = current {
        if let Some(label) = label_handler.handle(Input::new(node, ctx, None)) {
            labels.push(label);
        }
        current = node.parent();
    }

    labels
}

fn is_member_declaration_name_context(cursor: Node, decl: Node, type_body: Node) -> bool {
    if decl.parent().map(|p| p.id()) != Some(type_body.id()) {
        return false;
    }

    match decl.kind() {
        "method_declaration"
        | "constructor_declaration"
        | "compact_constructor_declaration"
        | "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => decl
            .child_by_field_name("name")
            .is_some_and(|name| node_contains(name, cursor)),
        "field_declaration" => is_in_name_position(cursor, decl),
        _ => false,
    }
}

fn node_contains(container: Node, target: Node) -> bool {
    let mut current = Some(target);
    while let Some(node) = current {
        if node.id() == container.id() {
            return true;
        }
        current = node.parent();
    }
    false
}

fn find_nearest_type_body(start: Node) -> Option<Node> {
    let mut current = Some(start);
    while let Some(node) = current {
        if is_type_body_kind(node.kind()) {
            return Some(node);
        }
        current = node.parent();
    }
    None
}

pub(crate) fn nearest_type_declaration(start: Node) -> Option<Node> {
    let mut current = Some(start);
    while let Some(node) = current {
        if is_type_declaration_kind(node.kind()) {
            return Some(node);
        }
        current = node.parent();
    }
    None
}

fn find_parent_type_declaration(node: Node) -> Option<Node> {
    let mut parent = node.parent();
    while let Some(p) = parent {
        if is_type_declaration_kind(p.kind()) {
            return Some(p);
        }
        parent = p.parent();
    }
    None
}

fn is_type_body_kind(kind: &str) -> bool {
    matches!(kind, "class_body" | "interface_body" | "enum_body")
}

fn is_type_declaration_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration"
    )
}

fn is_executable_or_nested_body_context(kind: &str) -> bool {
    matches!(
        kind,
        "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "lambda_expression"
            | "static_initializer"
            | "instance_initializer"
            | "block"
    )
}
