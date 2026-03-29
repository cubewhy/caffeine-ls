use std::sync::Arc;

use crate::completion::parser::parse_chain_from_expr;
use crate::index::IndexView;
use crate::language::java::intrinsics::access::class_literal_result_type;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::LocalVar;
use crate::semantic::context::SamSignature;
use crate::semantic::enclosing::resolve_enclosing_owner_internal;
use crate::semantic::types::type_name::TypeName;
use crate::semantic::types::{
    CallArgs, ChainSegment, EvalContext, TypeResolver, parse_single_type_to_internal,
    promoted_integral_result_type_name, promoted_numeric_result_type_name,
    promoted_shift_result_type_name, promoted_unary_integral_result_type_name,
    singleton_descriptor_to_type, unboxed_primitive_type_name,
};
use tree_sitter::Node;

pub(crate) fn resolve_expression_type(
    expr: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    let mut resolving_vars = Vec::new();
    resolve_expression_type_with_target_inner(
        expr,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        &mut resolving_vars,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn resolve_expression_type_with_target(
    expr: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    expected_functional_interface: Option<&TypeName>,
    expected_sam: Option<&SamSignature>,
) -> Option<TypeName> {
    let mut resolving_vars = Vec::new();
    resolve_expression_type_with_target_inner(
        expr,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        &mut resolving_vars,
        expected_functional_interface,
        expected_sam,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_expression_type_with_target_inner(
    expr: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
    expected_functional_interface: Option<&TypeName>,
    expected_sam: Option<&SamSignature>,
) -> Option<TypeName> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }

    if let Some(ty) = resolve_expression_type_ast(
        expr,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
        expected_functional_interface,
        expected_sam,
    ) {
        return Some(ty);
    }

    if looks_like_array_access(expr) {
        return resolve_array_access_type(
            expr,
            locals,
            enclosing_internal,
            resolver,
            type_ctx,
            view,
            resolving_vars,
        );
    }

    let chain = parse_chain_from_expr(expr);
    if chain.is_empty() {
        return resolve_local_identifier_type(
            expr,
            locals,
            enclosing_internal,
            resolver,
            type_ctx,
            view,
            resolving_vars,
        )
        .or_else(|| resolver.resolve(expr, locals, enclosing_internal));
    }
    evaluate_chain_inner(
        &chain,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_expression_type_ast(
    expr: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
    expected_functional_interface: Option<&TypeName>,
    expected_sam: Option<&SamSignature>,
) -> Option<TypeName> {
    let wrapped = format!("class __ExprTyping {{ Object __e() {{ return {expr}; }} }}");
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(&wrapped, None)?;
    let root = tree.root_node();
    let return_stmt = find_first_node_of_kind(root, "return_statement")?;
    let expr_node = return_stmt
        .child_by_field_name("value")
        .or_else(|| first_named_child(return_stmt))?;

    resolve_ast_node_type(
        expr_node,
        wrapped.as_bytes(),
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
        expected_functional_interface,
        expected_sam,
    )
}

fn first_named_child<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn find_first_node_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_first_node_of_kind(child, kind) {
            return Some(found);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn resolve_ast_node_type(
    node: Node,
    bytes: &[u8],
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
    expected_functional_interface: Option<&TypeName>,
    expected_sam: Option<&SamSignature>,
) -> Option<TypeName> {
    match node.kind() {
        "parenthesized_expression" => {
            let inner = node
                .child_by_field_name("expression")
                .or_else(|| first_named_child(node))?;
            resolve_ast_node_type(
                inner,
                bytes,
                locals,
                enclosing_internal,
                resolver,
                type_ctx,
                view,
                resolving_vars,
                expected_functional_interface,
                expected_sam,
            )
        }
        "array_type" => {
            let text = node.utf8_text(bytes).ok()?.trim();
            resolve_source_type_name(text, enclosing_internal, type_ctx, view)
        }
        "identifier" => {
            let text = node.utf8_text(bytes).ok()?;
            let ident = text.trim();
            resolve_local_identifier_type(
                ident,
                locals,
                enclosing_internal,
                resolver,
                type_ctx,
                view,
                resolving_vars,
            )
            .or_else(|| resolver.resolve(ident, locals, enclosing_internal))
        }
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => {
            let text = node.utf8_text(bytes).ok()?;
            resolve_integer_literal_type(text)
        }
        "decimal_floating_point_literal" | "hex_floating_point_literal" => {
            let text = node.utf8_text(bytes).ok()?;
            resolve_floating_point_literal_type(text)
        }
        "boolean_literal" => Some(TypeName::new("boolean")),
        "character_literal" => Some(TypeName::new("char")),
        "null_literal" => Some(TypeName::new("null")),
        "string_literal" | "text_block" => Some(TypeName::new("java/lang/String")),
        "cast_expression" => {
            resolve_cast_expression_type(node, bytes, enclosing_internal, type_ctx, view)
        }
        "class_literal" => {
            resolve_class_literal_expression_type(node, bytes, enclosing_internal, type_ctx, view)
        }
        "assignment_expression" => resolve_assignment_expression_type(
            node,
            bytes,
            locals,
            enclosing_internal,
            resolver,
            type_ctx,
            view,
            resolving_vars,
            expected_functional_interface,
            expected_sam,
        ),
        "ternary_expression" => resolve_ternary_expression_type(
            node,
            bytes,
            locals,
            enclosing_internal,
            resolver,
            type_ctx,
            view,
            resolving_vars,
            expected_functional_interface,
            expected_sam,
        ),
        "unary_expression" => resolve_unary_expression_type(
            node,
            bytes,
            locals,
            enclosing_internal,
            resolver,
            type_ctx,
            view,
            resolving_vars,
            expected_functional_interface,
            expected_sam,
        ),
        "method_invocation" => {
            let text = node.utf8_text(bytes).ok()?;
            resolve_expression_via_existing_resolver_inner(
                text,
                locals,
                enclosing_internal,
                resolver,
                type_ctx,
                view,
                resolving_vars,
            )
        }
        "object_creation_expression" | "explicit_constructor_invocation" => {
            let text = node.utf8_text(bytes).ok()?;
            resolve_var_init_expr_inner(
                text,
                locals,
                enclosing_internal,
                resolver,
                type_ctx,
                view,
                resolving_vars,
            )
        }
        "field_access" => {
            let text = node.utf8_text(bytes).ok()?;
            resolve_expression_via_existing_resolver_inner(
                text,
                locals,
                enclosing_internal,
                resolver,
                type_ctx,
                view,
                resolving_vars,
            )
        }
        "lambda_expression" => {
            resolve_lambda_expression_type(node, expected_functional_interface, expected_sam)
        }
        "binary_expression" => resolve_binary_expression_type(
            node,
            bytes,
            locals,
            enclosing_internal,
            resolver,
            type_ctx,
            view,
            resolving_vars,
            expected_functional_interface,
            expected_sam,
        ),
        _ => None,
    }
}

fn resolve_class_literal_expression_type(
    node: Node,
    bytes: &[u8],
    enclosing_internal: Option<&Arc<str>>,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    let operand_node = class_literal_operand_node(node)?;
    if !is_legal_class_literal_operand_node(operand_node) {
        return None;
    }

    let operand_text = operand_node.utf8_text(bytes).ok()?.trim();
    let operand = resolve_source_type_name(operand_text, enclosing_internal, type_ctx, view)?;
    if operand.has_generics() {
        return None;
    }

    Some(class_literal_result_type(view, operand))
}

fn class_literal_operand_node(node: Node) -> Option<Node> {
    node.child_by_field_name("type").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|child| child.kind() != "identifier")
    })
}

fn is_legal_class_literal_operand_node(node: Node) -> bool {
    match node.kind() {
        "type_identifier"
        | "scoped_type_identifier"
        | "integral_type"
        | "floating_point_type"
        | "boolean_type"
        | "void_type" => true,
        "array_type" => array_type_is_legal_class_literal_operand(node),
        "generic_type" => false,
        _ => false,
    }
}

fn array_type_is_legal_class_literal_operand(node: Node) -> bool {
    if contains_parameterized_type_arguments(node) {
        return false;
    }

    let Some(element) = node
        .child_by_field_name("element")
        .or_else(|| node.child_by_field_name("type"))
        .or_else(|| first_named_child(node))
    else {
        return false;
    };

    is_legal_class_literal_operand_node(element)
}

fn contains_parameterized_type_arguments(node: Node) -> bool {
    if node.kind() == "generic_type" {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(contains_parameterized_type_arguments)
}

fn resolve_lambda_expression_type(
    node: Node,
    expected_functional_interface: Option<&TypeName>,
    expected_sam: Option<&SamSignature>,
) -> Option<TypeName> {
    let expected_ty = expected_functional_interface?;
    let sam = expected_sam?;
    let params = node.child_by_field_name("parameters")?;
    let param_count = match params.kind() {
        "identifier" => 1,
        "inferred_parameters" => {
            let mut cursor = params.walk();
            params
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "identifier")
                .count()
        }
        "formal_parameters" => {
            let mut cursor = params.walk();
            params
                .named_children(&mut cursor)
                .filter(|child| matches!(child.kind(), "formal_parameter" | "spread_parameter"))
                .count()
        }
        _ => return None,
    };
    if param_count != sam.param_types.len() {
        return None;
    }
    Some(expected_ty.clone())
}

fn resolve_expression_via_existing_resolver_inner(
    expr: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
) -> Option<TypeName> {
    let chain = parse_chain_from_expr(expr.trim());
    if chain.is_empty() {
        return resolve_local_identifier_type(
            expr,
            locals,
            enclosing_internal,
            resolver,
            type_ctx,
            view,
            resolving_vars,
        )
        .or_else(|| resolver.resolve(expr, locals, enclosing_internal));
    }
    evaluate_chain_inner(
        &chain,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
    )
}

fn intrinsic_method_return_type(
    receiver: &TypeName,
    method_name: &str,
    arg_count: usize,
) -> Option<TypeName> {
    if method_name == "getClass" && arg_count == 0 {
        // For arrays, return Class<T[]> where T[] is the array type
        // For regular types, return Class<? extends T> where T is the receiver type
        if receiver.is_array() {
            return Some(TypeName::with_args(
                "java/lang/Class",
                vec![receiver.clone()],
            ));
        } else {
            // Create a wildcard type with upper bound of the receiver type
            let wildcard = TypeName {
                base_internal: Arc::from("+"),
                kind: crate::semantic::types::type_name::TypeNameKind::WildcardExtends,
                args: vec![receiver.clone()],
                array_dims: 0,
            };
            return Some(TypeName::with_args("java/lang/Class", vec![wildcard]));
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn resolve_unary_expression_type(
    node: Node,
    bytes: &[u8],
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
    expected_functional_interface: Option<&TypeName>,
    expected_sam: Option<&SamSignature>,
) -> Option<TypeName> {
    let op = unary_operator(node, bytes)?;
    let operand = node.child_by_field_name("operand").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).next()
    })?;
    let operand_type = resolve_ast_node_type(
        operand,
        bytes,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
        expected_functional_interface,
        expected_sam,
    )?;
    match op.as_str() {
        "+" | "-" => {
            let promoted = unary_numeric_result_type_name(operand_type.erased_internal())?;
            Some(TypeName::new(promoted))
        }
        "!" => {
            if operand_type.erased_internal() == "boolean" {
                Some(TypeName::new("boolean"))
            } else {
                None
            }
        }
        "~" => {
            let promoted =
                promoted_unary_integral_result_type_name(operand_type.erased_internal())?;
            Some(TypeName::new(promoted))
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_binary_expression_type(
    node: Node,
    bytes: &[u8],
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
    expected_functional_interface: Option<&TypeName>,
    expected_sam: Option<&SamSignature>,
) -> Option<TypeName> {
    let op = binary_operator(node, bytes)?;
    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    let left_type = resolve_ast_node_type(
        left,
        bytes,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
        expected_functional_interface,
        expected_sam,
    )?;
    let right_type = resolve_ast_node_type(
        right,
        bytes,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
        expected_functional_interface,
        expected_sam,
    )?;

    match op.as_str() {
        "+" => {
            if is_string_type(&left_type) || is_string_type(&right_type) {
                Some(TypeName::new("java/lang/String"))
            } else {
                numeric_binary_result_type(&left_type, &right_type)
            }
        }
        "-" | "*" | "/" | "%" => numeric_binary_result_type(&left_type, &right_type),
        "<" | "<=" | ">" | ">=" => {
            numeric_binary_result_type(&left_type, &right_type).map(|_| TypeName::new("boolean"))
        }
        "==" | "!=" => equality_result_type(&left_type, &right_type),
        "&&" | "||" => logical_result_type(&left_type, &right_type),
        "&" | "|" | "^" => bitwise_result_type(&left_type, &right_type),
        "<<" | ">>" | ">>>" => shift_binary_result_type(&left_type, &right_type),
        _ => None,
    }
}

fn binary_operator<'a>(node: Node<'a>, bytes: &[u8]) -> Option<String> {
    if let Some(op) = node.child_by_field_name("operator") {
        return op.utf8_text(bytes).ok().map(|s| s.trim().to_string());
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "+" | "-"
                | "*"
                | "/"
                | "%"
                | "<"
                | "<="
                | ">"
                | ">="
                | "=="
                | "!="
                | "&&"
                | "||"
                | "&"
                | "|"
                | "^"
                | "<<"
                | ">>"
                | ">>>"
        ) {
            return Some(child.kind().to_string());
        }
    }
    None
}

fn unary_operator<'a>(node: Node<'a>, bytes: &[u8]) -> Option<String> {
    if let Some(op) = node.child_by_field_name("operator") {
        return op.utf8_text(bytes).ok().map(|s| s.trim().to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "~" | "+" | "-" | "!") {
            return Some(child.kind().to_string());
        }
    }
    None
}

fn resolve_integer_literal_type(text: &str) -> Option<TypeName> {
    let sanitized: String = text.trim().chars().filter(|c| *c != '_').collect();
    if sanitized.is_empty() {
        return None;
    }
    if let Some(without_suffix) = sanitized
        .strip_suffix('l')
        .or_else(|| sanitized.strip_suffix('L'))
        && !without_suffix.is_empty()
    {
        return Some(TypeName::new("long"));
    }
    Some(TypeName::new("int"))
}

fn resolve_floating_point_literal_type(text: &str) -> Option<TypeName> {
    let sanitized: String = text.trim().chars().filter(|c| *c != '_').collect();
    if sanitized.is_empty() {
        return None;
    }
    if sanitized.ends_with('f') || sanitized.ends_with('F') {
        return Some(TypeName::new("float"));
    }
    Some(TypeName::new("double"))
}

fn numeric_binary_result_type(left: &TypeName, right: &TypeName) -> Option<TypeName> {
    let promoted =
        promoted_numeric_result_type_name(left.erased_internal(), right.erased_internal())?;
    Some(TypeName::new(promoted))
}

fn integral_binary_result_type(left: &TypeName, right: &TypeName) -> Option<TypeName> {
    let promoted =
        promoted_integral_result_type_name(left.erased_internal(), right.erased_internal())?;
    Some(TypeName::new(promoted))
}

fn shift_binary_result_type(left: &TypeName, right: &TypeName) -> Option<TypeName> {
    let promoted =
        promoted_shift_result_type_name(left.erased_internal(), right.erased_internal())?;
    Some(TypeName::new(promoted))
}

fn equality_result_type(left: &TypeName, right: &TypeName) -> Option<TypeName> {
    if left.erased_internal() == "null" || right.erased_internal() == "null" {
        if !left.is_primitive() || !right.is_primitive() {
            return Some(TypeName::new("boolean"));
        }
        return None;
    }

    if left.erased_internal() == "boolean" && right.erased_internal() == "boolean" {
        return Some(TypeName::new("boolean"));
    }

    if numeric_binary_result_type(left, right).is_some() {
        return Some(TypeName::new("boolean"));
    }

    if !left.is_primitive() && !right.is_primitive() {
        return Some(TypeName::new("boolean"));
    }

    None
}

fn logical_result_type(left: &TypeName, right: &TypeName) -> Option<TypeName> {
    if left.erased_internal() == "boolean" && right.erased_internal() == "boolean" {
        Some(TypeName::new("boolean"))
    } else {
        None
    }
}

fn bitwise_result_type(left: &TypeName, right: &TypeName) -> Option<TypeName> {
    if left.erased_internal() == "boolean" && right.erased_internal() == "boolean" {
        return Some(TypeName::new("boolean"));
    }
    integral_binary_result_type(left, right)
}

fn unary_numeric_result_type_name(operand: &str) -> Option<&'static str> {
    let operand = unboxed_primitive_type_name(operand)?;
    match operand {
        "byte" | "short" | "char" | "int" => Some("int"),
        "long" => Some("long"),
        "float" => Some("float"),
        "double" => Some("double"),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_cast_expression_type(
    node: Node,
    bytes: &[u8],
    enclosing_internal: Option<&Arc<str>>,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    let value_node = node.child_by_field_name("value");
    let mut cursor = node.walk();
    let type_nodes = node
        .named_children(&mut cursor)
        .filter(|child| {
            child.kind().contains("type") && value_node.is_none_or(|value| child.id() != value.id())
        })
        .collect::<Vec<_>>();
    if type_nodes.is_empty() {
        return None;
    }

    let resolved = type_nodes
        .into_iter()
        .map(|cast_type_node| {
            let cast_text = cast_type_node.utf8_text(bytes).ok()?.trim();
            resolve_source_type_name(cast_text, enclosing_internal, type_ctx, view)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(TypeName::intersection(resolved))
}

#[allow(clippy::too_many_arguments)]
fn resolve_assignment_expression_type(
    node: Node,
    bytes: &[u8],
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
    expected_functional_interface: Option<&TypeName>,
    expected_sam: Option<&SamSignature>,
) -> Option<TypeName> {
    let left = node.child_by_field_name("left")?;
    resolve_ast_node_type(
        left,
        bytes,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
        expected_functional_interface,
        expected_sam,
    )
    .or_else(|| {
        let right = node.child_by_field_name("right")?;
        resolve_ast_node_type(
            right,
            bytes,
            locals,
            enclosing_internal,
            resolver,
            type_ctx,
            view,
            resolving_vars,
            expected_functional_interface,
            expected_sam,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn resolve_ternary_expression_type(
    node: Node,
    bytes: &[u8],
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
    expected_functional_interface: Option<&TypeName>,
    expected_sam: Option<&SamSignature>,
) -> Option<TypeName> {
    let consequence = node.child_by_field_name("consequence").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).nth(1)
    })?;
    let alternative = node.child_by_field_name("alternative").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).nth(2)
    })?;
    let left = resolve_ast_node_type(
        consequence,
        bytes,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
        expected_functional_interface,
        expected_sam,
    )?;
    let right = resolve_ast_node_type(
        alternative,
        bytes,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
        expected_functional_interface,
        expected_sam,
    )?;
    if left.erased_internal_with_arrays() == right.erased_internal_with_arrays() {
        return Some(left);
    }
    if left.erased_internal() == "null" && !right.is_primitive() {
        return Some(right);
    }
    if right.erased_internal() == "null" && !left.is_primitive() {
        return Some(left);
    }
    if let Some(promoted) = numeric_binary_result_type(&left, &right) {
        return Some(promoted);
    }
    if !left.is_primitive() && !right.is_primitive() {
        return Some(left);
    }
    None
}

fn resolve_source_type_name(
    type_name: &str,
    enclosing_internal: Option<&Arc<str>>,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    let ty = type_name.trim();
    if ty.is_empty() {
        return None;
    }
    if matches!(
        ty,
        "byte" | "short" | "int" | "long" | "float" | "double" | "boolean" | "char" | "void"
    ) {
        return Some(TypeName::new(ty));
    }
    if let Some(relaxed) = type_ctx.resolve_type_name_relaxed(ty) {
        return Some(relaxed.ty);
    }
    resolve_constructor_type_name(ty, enclosing_internal, type_ctx, view)
}

fn is_string_type(ty: &TypeName) -> bool {
    matches!(ty.erased_internal(), "java/lang/String")
}

pub(crate) fn resolve_var_init_expr(
    expr: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    let mut resolving_vars = Vec::new();
    resolve_var_init_expr_inner(
        expr,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        &mut resolving_vars,
    )
}

fn resolve_var_init_expr_inner(
    expr: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
) -> Option<TypeName> {
    let expr = expr.trim();
    if let Some(rest) = expr.strip_prefix("new ") {
        let mut boundary_idx = rest.find(['(', '[', '{']).unwrap_or(rest.len());
        if let Some(gen_start) = rest.find('<')
            && gen_start < boundary_idx
        {
            if let Some(gen_end) = find_matching_angle(rest, gen_start) {
                boundary_idx = gen_end + 1;
            } else {
                return None;
            }
        }
        let type_name = rest[..boundary_idx].trim();
        let resolved_base: TypeName = match type_name {
            "byte" | "short" | "int" | "long" | "float" | "double" | "boolean" | "char" => {
                TypeName::new(type_name)
            }
            _ => resolve_constructor_type_name(type_name, enclosing_internal, type_ctx, view)?,
        };
        let after_type = rest[boundary_idx..].trim_start();
        if after_type.starts_with('[') || after_type.starts_with('{') {
            let brace_idx = after_type.find('{').unwrap_or(after_type.len());
            let dimensions = after_type[..brace_idx].matches('[').count();
            let mut array_ty = resolved_base;
            for _ in 0..dimensions {
                array_ty = array_ty.wrap_array();
            }
            return Some(array_ty);
        }
        return Some(resolved_base);
    }

    resolve_expression_type_with_target_inner(
        expr,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
        None,
        None,
    )
}

fn resolve_constructor_type_name(
    type_name: &str,
    enclosing_internal: Option<&Arc<str>>,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    if let Some(strict) = type_ctx.resolve_type_name_strict(type_name) {
        return Some(strict);
    }
    let resolve_head = |head: &str| {
        if let Some(strict) = type_ctx.resolve_type_name_strict(head) {
            return Some(Arc::from(strict.erased_internal()));
        }
        if let Some(enclosing_internal) = enclosing_internal {
            if let Some(owner) = resolve_enclosing_owner_internal(
                Some(view),
                Some(enclosing_internal.as_ref()),
                None,
                &[],
                head,
            ) {
                return Some(owner);
            }
            return view
                .resolve_scoped_inner_class(enclosing_internal, head)
                .map(|c| c.internal_name.clone());
        }
        None
    };
    view.resolve_qualified_type_path(type_name, &resolve_head)
        .map(|c| TypeName::new(c.internal_name.as_ref()))
}

pub(crate) fn looks_like_array_access(expr: &str) -> bool {
    expr.contains('[') && expr.trim_end().ends_with(']')
}

fn find_matching_angle(s: &str, start: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices().skip(start) {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn resolve_array_access_type(
    expr: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
) -> Option<TypeName> {
    let bracket = expr.rfind('[')?;
    if !expr.trim_end().ends_with(']') {
        return None;
    }
    let array_expr = expr[..bracket].trim();
    if array_expr.is_empty() {
        return None;
    }
    let array_type = resolve_expression_type_with_target_inner(
        array_expr,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
        None,
        None,
    )?;
    array_type.element_type()
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn evaluate_chain(
    chain: &[ChainSegment],
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    let mut resolving_vars = Vec::new();
    evaluate_chain_inner(
        chain,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        &mut resolving_vars,
    )
}

fn evaluate_chain_inner(
    chain: &[ChainSegment],
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
) -> Option<TypeName> {
    let mut current: Option<TypeName> = None;
    let resolve_qualifier = |q: &str| type_ctx.resolve_type_name_strict(q);
    for (i, seg) in chain.iter().enumerate() {
        let bracket_idx = seg.name.find('[');
        let base_name = if let Some(idx) = bracket_idx {
            &seg.name[..idx]
        } else {
            &seg.name
        };
        let dimensions = seg.name.matches('[').count();

        if i == 0 {
            if seg.arg_count.is_some() {
                let recv_internal = enclosing_internal?;
                let arg_types: Vec<TypeName> = seg
                    .arg_texts
                    .iter()
                    .map(|t| {
                        resolve_expression_type(
                            t.trim(),
                            locals,
                            enclosing_internal,
                            resolver,
                            type_ctx,
                            view,
                        )
                        .unwrap_or_else(TypeName::unknown)
                    })
                    .collect();
                current = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                    recv_internal.as_ref(),
                    base_name,
                    CallArgs::new(seg.arg_count.unwrap_or(0), &arg_types, &seg.arg_texts),
                    EvalContext::new(locals, enclosing_internal)
                        .with_qualifier(Some(&resolve_qualifier)),
                );
                if current.is_none()
                    && let (Some(enclosing), Some(method)) =
                        (enclosing_internal, type_ctx.current_class_method(base_name))
                {
                    current = resolver
                        .resolve_selected_method_return_with_callsite_and_qualifier_resolver(
                            enclosing.as_ref(),
                            method.as_ref(),
                            enclosing.as_ref(),
                            None,
                            CallArgs::new(seg.arg_count.unwrap_or(0), &arg_types, &seg.arg_texts),
                            EvalContext::new(locals, enclosing_internal)
                                .with_qualifier(Some(&resolve_qualifier)),
                        );
                }
            } else {
                current = resolve_local_identifier_type(
                    base_name,
                    locals,
                    enclosing_internal,
                    resolver,
                    type_ctx,
                    view,
                    resolving_vars,
                )
                .or_else(|| resolver.resolve(base_name, locals, enclosing_internal));
                if current.is_none() {
                    if let Some(enclosing) = enclosing_internal {
                        let enclosing_simple = enclosing
                            .rsplit('/')
                            .next()
                            .unwrap_or(enclosing)
                            .rsplit('$')
                            .next()
                            .unwrap_or(enclosing);

                        if base_name == enclosing_simple {
                            current = Some(TypeName::new(enclosing.as_ref()));
                        }
                    }

                    if current.is_none() {
                        current = type_ctx.resolve_type_name_strict(base_name);
                    }
                }
            }
        } else {
            current = Some(resolve_chain_segment_on_receiver(
                current.as_ref()?,
                seg,
                locals,
                enclosing_internal,
                resolver,
                type_ctx,
                view,
                &resolve_qualifier,
                resolving_vars,
            )?);
        }

        current = apply_chain_dimensions(current, dimensions);
    }
    current
}

#[allow(clippy::too_many_arguments)]
fn resolve_chain_segment_on_receiver(
    recv: &TypeName,
    seg: &ChainSegment,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolve_qualifier: &dyn Fn(&str) -> Option<TypeName>,
    resolving_vars: &mut Vec<Arc<str>>,
) -> Option<TypeName> {
    let base_name = seg.name.split('[').next().unwrap_or(seg.name.as_str());
    if base_name.is_empty() {
        return Some(recv.clone());
    }

    let recv_full = canonicalize_chain_receiver_type(recv, type_ctx)?;

    if seg.arg_count.is_some() {
        if let Some(intrinsic) =
            intrinsic_method_return_type(&recv_full, base_name, seg.arg_count.unwrap_or(0))
        {
            Some(intrinsic)
        } else {
            let arg_types: Vec<TypeName> = seg
                .arg_texts
                .iter()
                .map(|t| {
                    resolve_expression_type_with_target_inner(
                        t.trim(),
                        locals,
                        enclosing_internal,
                        resolver,
                        type_ctx,
                        view,
                        resolving_vars,
                        None,
                        None,
                    )
                    .unwrap_or_else(TypeName::unknown)
                })
                .collect();
            let receiver_internal = recv_full.to_internal_with_generics();
            resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                &receiver_internal,
                base_name,
                CallArgs::new(seg.arg_count.unwrap_or(0), &arg_types, &seg.arg_texts),
                EvalContext::new(locals, enclosing_internal)
                    .with_qualifier(Some(resolve_qualifier)),
            )
        }
    } else if recv_full.is_array() && base_name == "length" {
        Some(TypeName::new("int"))
    } else {
        let (_methods, fields) = view.collect_inherited_members(recv_full.erased_internal());
        if let Some(field) = fields.iter().find(|f| f.name.as_ref() == base_name) {
            field_descriptor_type(field.descriptor.as_ref())
        } else {
            None
        }
    }
}

fn canonicalize_chain_receiver_type(recv: &TypeName, type_ctx: &SourceTypeCtx) -> Option<TypeName> {
    if recv.is_exact_class() || recv.is_primitive() {
        return Some(recv.clone());
    }

    let mut canonical = type_ctx.resolve_type_name_strict(recv.erased_internal())?;
    if !recv.args.is_empty() {
        canonical.args = recv.args.clone();
    }
    canonical.array_dims = recv.array_dims;
    Some(canonical)
}

fn apply_chain_dimensions(current: Option<TypeName>, dimensions: usize) -> Option<TypeName> {
    if dimensions == 0 {
        return current;
    }

    let mut ty = current?;
    for _ in 0..dimensions {
        ty = ty.element_type()?;
    }
    Some(ty)
}

fn resolve_local_identifier_type(
    ident: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolving_vars: &mut Vec<Arc<str>>,
) -> Option<TypeName> {
    let local = locals.iter().find(|lv| lv.name.as_ref() == ident)?;
    if local.decl_kind != crate::semantic::LocalVarDeclKind::VarSyntax {
        return Some(local.type_internal.clone());
    }

    let init_expr = local.init_expr.as_deref()?;
    if init_expr.trim() == ident {
        return Some(local.type_internal.clone());
    }
    if is_currently_resolving_var_local(ident, locals, resolving_vars) {
        return None;
    }

    resolving_vars.push(Arc::clone(&local.name));
    let resolved = resolve_var_init_expr_inner(
        init_expr,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
        resolving_vars,
    );
    resolving_vars.pop();
    resolved.or_else(|| Some(local.type_internal.clone()))
}

fn is_currently_resolving_var_local(
    ident: &str,
    locals: &[LocalVar],
    resolving_vars: &[Arc<str>],
) -> bool {
    resolving_vars.iter().any(|name| name.as_ref() == ident)
        && locals.iter().any(|lv| {
            lv.name.as_ref() == ident
                && lv.decl_kind == crate::semantic::LocalVarDeclKind::VarSyntax
                && lv.init_expr.is_some()
        })
}

fn field_descriptor_type(descriptor: &str) -> Option<TypeName> {
    if let Some(ty) = singleton_descriptor_to_type(descriptor) {
        Some(TypeName::new(ty))
    } else {
        parse_single_type_to_internal(descriptor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, IndexView, MethodParams, MethodSummary, ModuleId,
        WorkspaceIndex,
    };
    use crate::language::java::completion_context::ContextEnricher;
    use crate::semantic::context::JavaIntrinsicAccessKind;
    use crate::semantic::context::SamSignature;
    use crate::semantic::context::{CursorLocation, SemanticContext};
    use rust_asm::constants::ACC_PUBLIC;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Main"),
                internal_name: Arc::from("org/cubewhy/Main"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("getInt"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("I")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Class"),
                internal_name: Arc::from("java/lang/Class"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("String"),
                internal_name: Arc::from("java/lang/String"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("substring"),
                    params: MethodParams::from([("I", "beginIndex")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("Ljava/lang/String;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("Map"),
                internal_name: Arc::from("java/util/Map"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from(
                    "<K:Ljava/lang/Object;V:Ljava/lang/Object;>Ljava/lang/Object;",
                )),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Outer"),
                internal_name: Arc::from("org/cubewhy/Outer"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Inner"),
                internal_name: Arc::from("org/cubewhy/Outer$Inner"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/Outer")),
                origin: ClassOrigin::Unknown,
            },
        ]);
        idx
    }

    fn make_type_ctx(view: &IndexView) -> SourceTypeCtx {
        SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec![],
            Some(view.build_name_table()),
        )
    }

    fn function_type() -> TypeName {
        TypeName::with_args(
            "java/util/function/Function",
            vec![
                TypeName::new("java/lang/String"),
                TypeName::new("java/lang/Integer"),
            ],
        )
    }

    fn function_sam() -> SamSignature {
        SamSignature {
            method_name: Arc::from("apply"),
            param_types: vec![TypeName::new("java/lang/String")],
            return_type: Some(TypeName::new("java/lang/Integer")),
        }
    }

    fn consumer_type() -> TypeName {
        TypeName::with_args(
            "java/util/function/Consumer",
            vec![TypeName::new("java/lang/String")],
        )
    }

    fn consumer_sam() -> SamSignature {
        SamSignature {
            method_name: Arc::from("accept"),
            param_types: vec![TypeName::new("java/lang/String")],
            return_type: Some(TypeName::new("void")),
        }
    }

    fn supplier_type() -> TypeName {
        TypeName::with_args(
            "java/util/function/Supplier",
            vec![TypeName::new("java/lang/String")],
        )
    }

    fn supplier_sam() -> SamSignature {
        SamSignature {
            method_name: Arc::from("get"),
            param_types: vec![],
            return_type: Some(TypeName::new("java/lang/String")),
        }
    }

    fn make_packaged_box_map_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            root_scope(),
            vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Box"),
                internal_name: Arc::from("org/cubewhy/Box"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("map"),
                    params: MethodParams::from([("Ljava/util/function/Function;", "fn")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from(
                        "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)Lorg/cubewhy/Box<TR;>;",
                    )),
                    return_type: Some(Arc::from("Lorg/cubewhy/Box;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("String"),
                internal_name: Arc::from("java/lang/String"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("trim"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("Ljava/lang/String;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util/function")),
                name: Arc::from("Function"),
                internal_name: Arc::from("java/util/function/Function"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("apply"),
                    params: MethodParams::from([("Ljava/lang/Object;", "t")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TT;)TR;")),
                    return_type: Some(Arc::from("Ljava/lang/Object;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from(
                    "<T:Ljava/lang/Object;R:Ljava/lang/Object;>Ljava/lang/Object;",
                )),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ],
        );
        idx
    }

    #[test]
    fn test_var_init_expr_cast_and_numeric_promotion_infers_double() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);

        let ty = resolve_var_init_expr(
            "(double) getInt() + 1 + 1 * 100",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("type");

        assert_eq!(ty.erased_internal(), "double");
    }

    #[test]
    fn test_identifier_var_local_falls_back_to_initializer_type() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("m"),
            type_internal: TypeName::unknown(),
            decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
            init_expr: Some("new Main()".to_string()),
        }];

        let ty = resolve_expression_type(
            "m",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("resolved type");

        assert_eq!(ty.erased_internal(), "org/cubewhy/Main");
    }

    #[test]
    fn test_var_init_expr_method_call_on_unmaterialized_var_receiver_materializes_string() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        );
        let resolver = TypeResolver::new(&view);
        let locals = vec![
            LocalVar {
                name: Arc::from("s2"),
                type_internal: TypeName::unknown(),
                decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                init_expr: Some("s1.substring(1)".to_string()),
            },
            LocalVar {
                name: Arc::from("s1"),
                type_internal: TypeName::unknown(),
                decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                init_expr: Some("\"\"".to_string()),
            },
        ];

        let ty = resolve_var_init_expr(
            "s1.substring(1)",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("resolved type");

        assert_eq!(ty.erased_internal(), "java/lang/String");
    }

    #[test]
    fn test_var_init_expr_functional_method_reference_materializes_packaged_box() {
        let idx = make_packaged_box_map_index();
        let view = idx.view(root_scope());
        let type_ctx = SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec!["java.lang.*".into(), "java.util.function.*".into()],
            Some(view.build_name_table()),
        );
        let resolver = TypeResolver::new(&view);
        let resolve_qualifier = |q: &str| type_ctx.resolve_type_name_strict(q);
        let locals = vec![
            LocalVar {
                name: Arc::from("strBox"),
                type_internal: TypeName::with_args(
                    "org/cubewhy/Box",
                    vec![TypeName::new("java/lang/String")],
                ),
                decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                init_expr: None,
            },
            LocalVar {
                name: Arc::from("a"),
                type_internal: TypeName::unknown(),
                decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                init_expr: Some("strBox.map(String::trim)".to_string()),
            },
        ];

        let candidates = view.lookup_methods_in_hierarchy("org/cubewhy/Box", "map");
        let candidate_refs: Vec<&MethodSummary> = candidates.iter().map(|m| m.as_ref()).collect();
        let selected = resolver.select_overload_match_with_callsite(
            "org/cubewhy/Box<Ljava/lang/String;>",
            &candidate_refs,
            CallArgs::new(1, &[], &["String::trim".to_string()]),
            EvalContext::new(&locals, Some(&Arc::from("org/cubewhy/Demo")))
                .with_qualifier(Some(&resolve_qualifier)),
        );
        let owner = selected.as_ref().and_then(|selected| {
            view.find_declaring_method_owner(
                "org/cubewhy/Box",
                selected.method.name.as_ref(),
                selected.method.desc().as_ref(),
            )
        });
        let direct_map = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
            "org/cubewhy/Box<Ljava/lang/String;>",
            "map",
            CallArgs::new(1, &[], &["String::trim".to_string()]),
            EvalContext::new(&locals, Some(&Arc::from("org/cubewhy/Demo")))
                .with_qualifier(Some(&resolve_qualifier)),
        );
        let ty = resolve_var_init_expr(
            "strBox.map(String::trim)",
            &locals,
            Some(&Arc::from("org/cubewhy/Demo")),
            &resolver,
            &type_ctx,
            &view,
        );

        assert_eq!(
            selected.as_ref().map(|m| m.method.desc().to_string()),
            Some("(Ljava/util/function/Function;)Lorg/cubewhy/Box;".to_string()),
            "callsite selector should still pick the single map overload"
        );
        assert_eq!(
            owner.as_ref().map(|o| o.internal_name.to_string()),
            Some("org/cubewhy/Box".to_string()),
            "declaring owner lookup should find the packaged Box class"
        );
        assert_eq!(
            direct_map.as_ref().map(TypeName::to_internal_with_generics),
            Some("org/cubewhy/Box<Ljava/lang/String;>".to_string()),
            "direct resolver path should materialize map(String::trim)"
        );
        assert_eq!(
            ty.as_ref().map(TypeName::to_internal_with_generics),
            Some("org/cubewhy/Box<Ljava/lang/String;>".to_string()),
            "resolve_var_init_expr should match direct resolver path"
        );
    }

    #[test]
    fn test_assignment_expression_type_tracks_lhs() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("x"),
            type_internal: TypeName::new("double"),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "x = getInt()",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("type");
        assert_eq!(ty.erased_internal(), "double");
    }

    #[test]
    fn test_cast_expression_supports_intersection_types() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/io")),
                name: Arc::from("Closeable"),
                internal_name: Arc::from("java/io/Closeable"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Runnable"),
                internal_name: Arc::from("java/lang/Runnable"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(root_scope());
        let type_ctx = SourceTypeCtx::new(
            None,
            vec!["java.io.*".into(), "java.lang.*".into()],
            Some(view.build_name_table()),
        );
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("x"),
            type_internal: TypeName::new("java/lang/Object"),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "((Closeable & Runnable) x)",
            &locals,
            None,
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("type");

        assert!(ty.is_intersection());
        assert_eq!(ty.args.len(), 2);
        assert_eq!(ty.args[0].erased_internal(), "java/io/Closeable");
        assert_eq!(ty.args[1].erased_internal(), "java/lang/Runnable");
    }

    #[test]
    fn test_var_init_expr_resolves_qualified_nested_type_against_enclosing_owner() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/example")),
                name: Arc::from("Main"),
                internal_name: Arc::from("org/example/Main"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/example")),
                name: Arc::from("Test"),
                internal_name: Arc::from("org/example/Main$Test"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/example/Main")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/example")),
                name: Arc::from("Nested"),
                internal_name: Arc::from("org/example/Main$Test$Nested"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC | rust_asm::constants::ACC_STATIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/example/Main$Test")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let view = idx.view(root_scope());
        let type_ctx =
            SourceTypeCtx::from_view(Some(Arc::from("org/example")), vec![], view.clone());
        let resolver = TypeResolver::new(&view);
        let ty = resolve_var_init_expr(
            "new Test.Nested()",
            &[],
            Some(&Arc::from("org/example/Main$Test")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("nested type should resolve");

        assert_eq!(ty.erased_internal(), "org/example/Main$Test$Nested");
    }

    #[test]
    fn test_lambda_expression_type_uses_expected_functional_target_in_initializer_position() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let expected = function_type();
        let sam = function_sam();

        let ty = resolve_expression_type_with_target(
            "s -> s.length()",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
            Some(&expected),
            Some(&sam),
        )
        .expect("target-typed lambda");

        assert_eq!(ty, expected);
    }

    #[test]
    fn test_lambda_expression_type_uses_expected_functional_target_in_argument_position() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let expected = function_type();
        let sam = function_sam();

        let ty = resolve_expression_type_with_target(
            "s -> s.length()",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
            Some(&expected),
            Some(&sam),
        )
        .expect("argument-position lambda");

        assert_eq!(ty, expected);
    }

    #[test]
    fn test_block_bodied_lambda_expression_type_uses_expected_functional_target() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let expected = consumer_type();
        let sam = consumer_sam();

        let ty = resolve_expression_type_with_target(
            "value -> { System.out.println(value); }",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
            Some(&expected),
            Some(&sam),
        )
        .expect("block-bodied target-typed lambda");

        assert_eq!(ty, expected);
    }

    #[test]
    fn test_lambda_expression_type_uses_expected_functional_target_in_return_position() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let expected = supplier_type();
        let sam = supplier_sam();

        let ty = resolve_expression_type_with_target(
            "() -> \"x\"",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
            Some(&expected),
            Some(&sam),
        )
        .expect("return-position lambda");

        assert_eq!(ty, expected);
    }

    #[test]
    fn test_lambda_expression_type_without_target_stays_conservative() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);

        let ty = resolve_expression_type("s -> s.length()", &[], None, &resolver, &type_ctx, &view);

        assert!(ty.is_none(), "lambda without target should stay unresolved");
    }

    #[test]
    fn test_class_literal_string_type() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);

        let ty = resolve_expression_type(
            "String.class",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("class literal type");

        assert_eq!(
            ty,
            TypeName::with_args("java/lang/Class", vec![TypeName::new("java/lang/String")])
        );
    }

    #[test]
    fn test_class_literal_nested_type() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);

        let ty = resolve_expression_type(
            "Outer.Inner.class",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("nested class literal type");

        assert_eq!(
            ty,
            TypeName::with_args(
                "java/lang/Class",
                vec![TypeName::new("org/cubewhy/Outer$Inner")]
            )
        );
    }

    #[test]
    fn test_class_literal_array_type() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);

        let ty = resolve_expression_type(
            "String[].class",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("array class literal type");

        assert_eq!(
            ty,
            TypeName::with_args(
                "java/lang/Class",
                vec![TypeName::new("java/lang/String").with_array_dims(1)]
            )
        );
    }

    #[test]
    fn test_array_length_field_expression_type_is_int() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("s"),
            type_internal: TypeName::new("java/lang/String").with_array_dims(1),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "s.length",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("array length type");

        assert_eq!(ty, TypeName::new("int"));
    }

    #[test]
    fn test_array_get_class_expression_type_preserves_reference_array_identity() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("s"),
            type_internal: TypeName::new("java/lang/Integer").with_array_dims(1),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "s.getClass()",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("array getClass type");

        assert_eq!(
            ty,
            TypeName::with_args(
                "java/lang/Class",
                vec![TypeName::new("java/lang/Integer").with_array_dims(1)],
            )
        );
    }

    #[test]
    fn test_array_get_class_expression_type_preserves_multidimensional_array_identity() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("s"),
            type_internal: TypeName::new("java/lang/String").with_array_dims(2),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "s.getClass()",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("multidimensional array getClass type");

        assert_eq!(
            ty,
            TypeName::with_args(
                "java/lang/Class",
                vec![TypeName::new("java/lang/String").with_array_dims(2)],
            )
        );
    }

    #[test]
    fn test_array_get_class_expression_type_preserves_primitive_array_identity() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("s"),
            type_internal: TypeName::new("int").with_array_dims(1),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "s.getClass()",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("primitive array getClass type");

        assert_eq!(
            ty,
            TypeName::with_args(
                "java/lang/Class",
                vec![TypeName::new("int").with_array_dims(1)],
            )
        );
    }

    #[test]
    fn test_class_literal_primitive_and_void_types() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);

        let int_ty = resolve_expression_type(
            "int.class",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("primitive class literal type");
        let void_ty = resolve_expression_type(
            "void.class",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("void class literal type");

        assert_eq!(
            int_ty,
            TypeName::with_args("java/lang/Class", vec![TypeName::new("int")])
        );
        assert_eq!(
            void_ty,
            TypeName::with_args("java/lang/Class", vec![TypeName::new("void")])
        );
    }

    #[test]
    fn test_class_literal_rejects_parameterized_operands() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);

        assert_eq!(
            resolve_expression_type(
                "List<String>.class",
                &[],
                Some(&Arc::from("org/cubewhy/Main")),
                &resolver,
                &type_ctx,
                &view,
            ),
            None
        );
        assert_eq!(
            resolve_expression_type(
                "Map<String, Integer>.class",
                &[],
                Some(&Arc::from("org/cubewhy/Main")),
                &resolver,
                &type_ctx,
                &view,
            ),
            None
        );
        assert_eq!(
            resolve_expression_type(
                "Map.Entry<String, String>.class",
                &[],
                Some(&Arc::from("org/cubewhy/Main")),
                &resolver,
                &type_ctx,
                &view,
            ),
            None
        );
    }

    #[test]
    fn test_array_length_call_expression_is_rejected() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("s"),
            type_internal: TypeName::new("java/lang/String").with_array_dims(1),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        assert_eq!(
            resolve_expression_type(
                "s.length()",
                &locals,
                Some(&Arc::from("org/cubewhy/Main")),
                &resolver,
                &type_ctx,
                &view,
            ),
            None
        );
    }

    #[test]
    fn test_class_literal_classification_feeds_typed_expression_result() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "class".to_string(),
                receiver_expr: "String".to_string(),
                arguments: None,
            },
            "class",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/Main")),
            Some(Arc::from("org/cubewhy")),
            vec!["java.lang.*".into()],
        )
        .with_extension(Arc::new(type_ctx.clone()));

        ContextEnricher::new(&view).enrich(&mut ctx);

        assert_eq!(
            ctx.java_intrinsic_access.as_ref().map(|a| a.kind),
            Some(JavaIntrinsicAccessKind::ClassLiteral)
        );

        let ty = resolve_expression_type(
            "String.class",
            &[],
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("typed class literal");

        assert_eq!(
            ty,
            TypeName::with_args("java/lang/Class", vec![TypeName::new("java/lang/String")])
        );
    }

    #[test]
    fn test_get_class_returns_class_with_wildcard_extends_for_string() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("str"),
            type_internal: TypeName::new("java/lang/String"),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "str.getClass()",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("getClass type for String");

        // Should return Class<? extends String>
        let expected_wildcard = TypeName {
            base_internal: Arc::from("+"),
            kind: crate::semantic::types::type_name::TypeNameKind::WildcardExtends,
            args: vec![TypeName::new("java/lang/String")],
            array_dims: 0,
        };
        assert_eq!(
            ty,
            TypeName::with_args("java/lang/Class", vec![expected_wildcard])
        );
    }

    #[test]
    fn test_get_class_returns_class_with_wildcard_extends_for_custom_type() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("obj"),
            type_internal: TypeName::new("org/cubewhy/MyClass"),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "obj.getClass()",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("getClass type for custom type");

        // Should return Class<? extends MyClass>
        let expected_wildcard = TypeName {
            base_internal: Arc::from("+"),
            kind: crate::semantic::types::type_name::TypeNameKind::WildcardExtends,
            args: vec![TypeName::new("org/cubewhy/MyClass")],
            array_dims: 0,
        };
        assert_eq!(
            ty,
            TypeName::with_args("java/lang/Class", vec![expected_wildcard])
        );
    }

    #[test]
    fn test_get_class_returns_class_with_wildcard_extends_for_generic_type() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);

        // Test with a generic type like List<String>
        let list_of_string =
            TypeName::with_args("java/util/List", vec![TypeName::new("java/lang/String")]);
        let locals = vec![LocalVar {
            name: Arc::from("list"),
            type_internal: list_of_string.clone(),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "list.getClass()",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("getClass type for generic type");

        // Should return Class<? extends List<String>>
        let expected_wildcard = TypeName {
            base_internal: Arc::from("+"),
            kind: crate::semantic::types::type_name::TypeNameKind::WildcardExtends,
            args: vec![list_of_string],
            array_dims: 0,
        };
        assert_eq!(
            ty,
            TypeName::with_args("java/lang/Class", vec![expected_wildcard])
        );
    }

    #[test]
    fn test_get_class_on_object_type_returns_class_with_wildcard_extends() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("obj"),
            type_internal: TypeName::new("java/lang/Object"),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "obj.getClass()",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("getClass type for Object");

        // Should return Class<? extends Object>
        let expected_wildcard = TypeName {
            base_internal: Arc::from("+"),
            kind: crate::semantic::types::type_name::TypeNameKind::WildcardExtends,
            args: vec![TypeName::new("java/lang/Object")],
            array_dims: 0,
        };
        assert_eq!(
            ty,
            TypeName::with_args("java/lang/Class", vec![expected_wildcard])
        );
    }

    #[test]
    fn test_get_class_preserves_array_type_without_wildcard() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("arr"),
            type_internal: TypeName::new("java/lang/String").with_array_dims(1),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "arr.getClass()",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("getClass type for array");

        // Arrays should return Class<String[]> not Class<? extends String[]>
        assert_eq!(
            ty,
            TypeName::with_args(
                "java/lang/Class",
                vec![TypeName::new("java/lang/String").with_array_dims(1)]
            )
        );
    }

    #[test]
    fn test_get_class_in_constructor_parameter() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("str"),
            type_internal: TypeName::new("java/lang/String"),
            decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
            init_expr: None,
        }];

        let ty = resolve_expression_type(
            "str.getClass()",
            &locals,
            Some(&Arc::from("org/cubewhy/Main")),
            &resolver,
            &type_ctx,
            &view,
        )
        .expect("getClass type in constructor");

        // Should return Class<? extends String>
        let expected_wildcard = TypeName {
            base_internal: Arc::from("+"),
            kind: crate::semantic::types::type_name::TypeNameKind::WildcardExtends,
            args: vec![TypeName::new("java/lang/String")],
            array_dims: 0,
        };
        assert_eq!(
            ty,
            TypeName::with_args("java/lang/Class", vec![expected_wildcard])
        );
    }
}
