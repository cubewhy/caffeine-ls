use std::collections::HashMap;
use std::sync::Arc;

use tree_sitter::Node;

use crate::language::java::JavaContextExtractor;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::LocalVar;
use crate::semantic::types::type_name::TypeName;

type NarrowingFacts = HashMap<Arc<str>, TypeName>;

#[derive(Default)]
struct FlowFacts {
    when_true: NarrowingFacts,
    when_false: NarrowingFacts,
}

pub fn extract_instanceof_true_branch_overrides(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    type_ctx: &SourceTypeCtx,
    locals: &[LocalVar],
) -> NarrowingFacts {
    let mut out: NarrowingFacts = HashMap::new();
    let mut current = cursor_node;

    while let Some(node) = current {
        if node.kind() == "if_statement"
            && let Some(condition) = node.child_by_field_name("condition")
            && let Some(condition_expr) = unwrap_parenthesized_expression(condition)
        {
            // Inside `if (...) { ... }` true branch body: apply condition true-facts.
            if let Some(consequence) = node.child_by_field_name("consequence")
                && node_contains_offset(consequence, ctx.offset)
            {
                merge_facts(
                    &mut out,
                    &facts_for_expr(condition_expr, ctx, type_ctx, locals).when_true,
                );
            }

            // Inside condition expression itself: apply short-circuit facts available
            // at cursor position (e.g. RHS of && / RHS of || from false LHS).
            if node_contains_offset(condition, ctx.offset) {
                let at_cursor = facts_available_at_expr_position(
                    condition_expr,
                    ctx.offset,
                    ctx,
                    type_ctx,
                    locals,
                );
                merge_facts(&mut out, &at_cursor);
            }
        }
        current = node.parent();
    }

    out
}

fn facts_for_expr(
    expr: Node,
    ctx: &JavaContextExtractor,
    type_ctx: &SourceTypeCtx,
    locals: &[LocalVar],
) -> FlowFacts {
    let expr = peel_wrappers(expr);

    if let Some((name, ty)) = parse_instanceof_narrowing(ctx, expr, type_ctx, locals) {
        let mut when_true = HashMap::new();
        when_true.insert(name, ty);
        return FlowFacts {
            when_true,
            when_false: HashMap::new(),
        };
    }

    // !(x instanceof T): false implies x instanceof T.
    if expr.kind() == "unary_expression"
        && unary_operator(expr, ctx).as_deref() == Some("!")
        && let Some(operand) = unary_operand(expr)
    {
        let operand_facts = facts_for_expr(operand, ctx, type_ctx, locals);
        return FlowFacts {
            when_true: operand_facts.when_false,
            when_false: operand_facts.when_true,
        };
    }

    if expr.kind() == "binary_expression"
        && let Some(op) = binary_operator(expr)
        && let (Some(left), Some(right)) = (binary_left(expr), binary_right(expr))
    {
        let left_facts = facts_for_expr(left, ctx, type_ctx, locals);
        let right_facts = facts_for_expr(right, ctx, type_ctx, locals);

        return match op.as_str() {
            "&&" => {
                // (A && B) true => A true and B true.
                // (A && B) false is not used in this scoped implementation.
                let mut when_true = left_facts.when_true;
                merge_facts(&mut when_true, &right_facts.when_true);
                FlowFacts {
                    when_true,
                    when_false: HashMap::new(),
                }
            }
            "||" => {
                // (A || B) false => A false and B false.
                // We intentionally do not infer whole-branch true facts for ||.
                let mut when_false = left_facts.when_false;
                merge_facts(&mut when_false, &right_facts.when_false);
                FlowFacts {
                    when_true: HashMap::new(),
                    when_false,
                }
            }
            _ => FlowFacts::default(),
        };
    }

    FlowFacts::default()
}

fn facts_available_at_expr_position(
    expr: Node,
    offset: usize,
    ctx: &JavaContextExtractor,
    type_ctx: &SourceTypeCtx,
    locals: &[LocalVar],
) -> NarrowingFacts {
    let expr = peel_wrappers(expr);
    if !node_contains_offset(expr, offset) {
        return HashMap::new();
    }

    if expr.kind() == "binary_expression"
        && let Some(op) = binary_operator(expr)
        && let (Some(left), Some(right)) = (binary_left(expr), binary_right(expr))
    {
        if node_contains_offset(right, offset) {
            let mut rhs_facts =
                facts_available_at_expr_position(right, offset, ctx, type_ctx, locals);
            let left_facts = facts_for_expr(left, ctx, type_ctx, locals);
            match op.as_str() {
                "&&" => merge_facts(&mut rhs_facts, &left_facts.when_true),
                "||" => merge_facts(&mut rhs_facts, &left_facts.when_false),
                _ => {}
            }
            return rhs_facts;
        }

        if node_contains_offset(left, offset) {
            return facts_available_at_expr_position(left, offset, ctx, type_ctx, locals);
        }
    }

    if expr.kind() == "unary_expression"
        && let Some(operand) = unary_operand(expr)
        && node_contains_offset(operand, offset)
    {
        return facts_available_at_expr_position(operand, offset, ctx, type_ctx, locals);
    }

    HashMap::new()
}

fn parse_instanceof_narrowing(
    ctx: &JavaContextExtractor,
    expr: Node,
    type_ctx: &SourceTypeCtx,
    locals: &[LocalVar],
) -> Option<(Arc<str>, TypeName)> {
    if expr.kind() != "instanceof_expression" {
        return None;
    }

    let left = expr.child_by_field_name("left")?;
    if left.kind() != "identifier" {
        return None;
    }
    let var_name = ctx.node_text(left).trim();
    if var_name.is_empty() || !locals.iter().any(|lv| lv.name.as_ref() == var_name) {
        return None;
    }

    let right = expr.child_by_field_name("right")?;
    let right_text = ctx.node_text(right).trim();
    if right_text.is_empty() {
        return None;
    }
    let narrowed = type_ctx.resolve_type_name_relaxed(right_text)?.ty;

    Some((Arc::from(var_name), narrowed))
}

fn peel_wrappers(mut node: Node) -> Node {
    loop {
        if node.kind() == "parenthesized_expression"
            && let Some(inner) = unwrap_parenthesized_expression(node)
        {
            node = inner;
            continue;
        }
        if node.kind() == "expression_statement"
            && let Some(inner) = first_named_child(node)
        {
            node = inner;
            continue;
        }
        break node;
    }
}

fn unwrap_parenthesized_expression(node: Node) -> Option<Node> {
    if node.kind() == "parenthesized_expression" {
        node.child_by_field_name("expression")
            .or_else(|| first_named_child(node))
    } else {
        Some(node)
    }
}

fn unary_operator(node: Node, ctx: &JavaContextExtractor) -> Option<String> {
    if let Some(op) = node.child_by_field_name("operator") {
        return Some(ctx.node_text(op).trim().to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "!" | "~" | "+" | "-") {
            return Some(child.kind().to_string());
        }
    }
    None
}

fn unary_operand(node: Node) -> Option<Node> {
    node.child_by_field_name("operand").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).next()
    })
}

fn binary_operator(node: Node) -> Option<String> {
    if let Some(op) = node.child_by_field_name("operator") {
        match op.kind() {
            "&&" => return Some("&&".to_string()),
            "||" => return Some("||".to_string()),
            _ => {}
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "&&" => return Some("&&".to_string()),
            "||" => return Some("||".to_string()),
            _ => {}
        }
    }
    None
}

fn binary_left(node: Node) -> Option<Node> {
    node.child_by_field_name("left").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).next()
    })
}

fn binary_right(node: Node) -> Option<Node> {
    node.child_by_field_name("right").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).nth(1)
    })
}

fn merge_facts(dst: &mut NarrowingFacts, src: &NarrowingFacts) {
    for (name, ty) in src {
        if let Some(existing) = dst.get(name) {
            if existing.erased_internal() != ty.erased_internal() {
                // Conflicting facts are dropped for soundness.
                dst.remove(name);
            }
            continue;
        }
        dst.insert(name.clone(), ty.clone());
    }
}

fn node_contains_offset(node: Node, offset: usize) -> bool {
    node.start_byte() <= offset && offset <= node.end_byte()
}

fn first_named_child(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::index::{ClassMetadata, ClassOrigin};
    use crate::salsa_db::{Database, FileId, SourceFile};
    use crate::salsa_queries::{extract_java_flow_type_overrides, materialize_flow_type_overrides};
    use crate::semantic::types::type_name::TypeName;
    use tower_lsp::lsp_types::Url;

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

    fn overrides_at(src_with_cursor: &str) -> std::collections::HashMap<Arc<str>, TypeName> {
        let (src, cursor_offset) = if let Some(idx) = src_with_cursor.find("/*caret*/") {
            (src_with_cursor.replacen("/*caret*/", "", 1), idx)
        } else {
            let idx = src_with_cursor
                .find('|')
                .expect("expected | or /*caret*/ cursor marker");
            (src_with_cursor.replacen('|', "", 1), idx)
        };
        let workspace_index =
            Arc::new(parking_lot::RwLock::new(crate::index::WorkspaceIndex::new()));
        workspace_index.write().add_jdk_classes(vec![
            minimal_class("java/lang/Object"),
            minimal_class("java/lang/StringBuilder"),
            minimal_class("java/lang/String"),
        ]);
        let db = Database::with_workspace_index(workspace_index);
        let file = SourceFile::new(
            &db,
            FileId::new(Url::parse("file:///test/Flow.java").unwrap()),
            src,
            Arc::from("java"),
        );
        let overrides = extract_java_flow_type_overrides(&db, file, cursor_offset);
        materialize_flow_type_overrides(overrides.as_ref())
    }

    #[test]
    fn test_and_rhs_gets_lhs_true_instanceof_fact() {
        let src = r#"
            class T {
                void m(Object a) {
                    if (a instanceof StringBuilder && a.appe|) {
                    }
                }
            }
        "#;
        let facts = overrides_at(src);
        assert_eq!(
            facts.get("a").map(TypeName::erased_internal),
            Some("java/lang/StringBuilder")
        );
    }

    #[test]
    fn test_or_rhs_gets_false_lhs_fact_from_negated_instanceof() {
        let src = r#"
            class T {
                void m(Object x) {
                    if (!(x instanceof String) || x.subs/*caret*/cript(0)) {
                    }
                }
            }
        "#;
        let facts = overrides_at(src);
        assert_eq!(
            facts.get("x").map(TypeName::erased_internal),
            Some("java/lang/String")
        );
    }

    #[test]
    fn test_or_true_branch_does_not_collect_disjunctive_true_facts() {
        let src = r#"
            class T {
                void m(Object a, Object b) {
                    if (a instanceof String || b instanceof StringBuilder) {
                        a.appe/*caret*/
                    }
                }
            }
        "#;
        let facts = overrides_at(src);
        assert!(
            facts.is_empty(),
            "general || true-branch should not be over-narrowed, got: {:?}",
            facts
                .iter()
                .map(|(k, v)| format!("{k}:{}", v.erased_internal()))
                .collect::<Vec<_>>()
        );
    }
}
