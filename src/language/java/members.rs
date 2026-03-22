use rust_asm::constants::{ACC_PRIVATE, ACC_PROTECTED, ACC_PUBLIC, ACC_STATIC, ACC_VARARGS};
use std::sync::{Arc, OnceLock};
use tree_sitter::{Node, Query};
use tree_sitter_utils::traversal::{ancestor_of_kind, first_child_of_kind, first_child_of_kinds};
use tree_sitter_utils::{Handler, HandlerExt, Input};

use crate::{
    index::{AnnotationSummary, FieldSummary, MethodParams, MethodSummary, intern_str},
    language::{
        java::{
            JavaContextExtractor,
            type_ctx::{SourceTypeCtx, build_java_descriptor, extract_param_type, split_params},
            utils::{
                extract_generic_signature, extract_type_parameters_prefix, parse_java_modifiers,
            },
        },
        ts_utils::{capture_text, run_query},
    },
    semantic::{context::CurrentClassMember, types::parse_return_type_from_descriptor},
};

fn annotation_query() -> &'static (Query, u32) {
    static QUERY: OnceLock<(Query, u32)> = OnceLock::new();
    QUERY.get_or_init(|| {
        let query = Query::new(
            &tree_sitter_java::LANGUAGE.into(),
            r#"
        (marker_annotation) @a
        (annotation) @a
    "#,
        )
        .expect("annotation query must compile");
        let idx = query
            .capture_index_for_name("a")
            .expect("annotation capture must exist");
        (query, idx)
    })
}

fn annotation_name_query() -> &'static (Query, u32) {
    static QUERY: OnceLock<(Query, u32)> = OnceLock::new();
    QUERY.get_or_init(|| {
        let query = Query::new(
            &tree_sitter_java::LANGUAGE.into(),
            r#"
        (marker_annotation name: (identifier) @n)
        (marker_annotation name: (scoped_identifier) @n)
        (annotation name: (identifier) @n)
        (annotation name: (scoped_identifier) @n)
    "#,
        )
        .expect("annotation name query must compile");
        let idx = query
            .capture_index_for_name("n")
            .expect("annotation name capture must exist");
        (query, idx)
    })
}

#[rustfmt::skip]
pub fn is_java_keyword(name: &str) -> bool {
    matches!(
        name,
        "public" | "private" | "protected" | "static" | "final" | "abstract"
            | "synchronized" | "volatile" | "transient" | "native" | "strictfp"
            | "void" | "int" | "long" | "double" | "float" | "boolean"
            | "byte" | "short" | "char"
            | "class" | "interface" | "enum" | "extends" | "implements"
            | "return" | "new" | "this" | "super" | "null" | "true" | "false"
            | "if" | "else" | "for" | "while" | "do" | "switch" | "case"
            | "break" | "continue" | "default" | "try" | "catch" | "finally"
            | "throw" | "throws" | "import" | "package" | "instanceof" | "assert"
    )
}

fn is_method_return_type_kind(kind: &str) -> bool {
    matches!(
        kind,
        "void_type"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
            | "type_identifier"
            | "scoped_type_identifier"
            | "array_type"
            | "generic_type"
    )
}

/// Extract members from a class body using two-phase approach:
/// Phase 1: Extract valid members only (no ERROR nodes)
/// Phase 2: Perform shallow error recovery on ERROR nodes
pub fn extract_class_members_from_body(
    ctx: &JavaContextExtractor,
    body: Node,
    type_ctx: &SourceTypeCtx,
) -> Vec<CurrentClassMember> {
    // PHASE 1: Extract valid members only (no ERROR recovery)
    let mut members = extract_valid_members_only(ctx, body, type_ctx);

    // PHASE 2: Shallow error recovery from ERROR nodes
    let error_members = extract_error_recovery_members_from_body(ctx, body, type_ctx, &members);
    members.extend(error_members);

    members
}

/// Extract members from valid AST structure only.
/// ERROR nodes are completely skipped - no error recovery is performed.
/// This ensures accurate extraction without risk of infinite recursion.
pub fn extract_valid_members_only(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
) -> Vec<CurrentClassMember> {
    let mut members = Vec::new();
    collect_valid_members_impl(ctx, node, type_ctx, &mut members, false);
    members
}

/// Scan a class body for ERROR nodes and perform shallow error recovery.
/// Only processes direct ERROR children - no recursive traversal.
fn extract_error_recovery_members_from_body(
    ctx: &JavaContextExtractor,
    body: Node,
    type_ctx: &SourceTypeCtx,
    already_found: &[CurrentClassMember],
) -> Vec<CurrentClassMember> {
    let mut result = Vec::new();
    let mut cursor = body.walk();

    for child in body.children(&mut cursor) {
        if child.kind() == "ERROR" {
            result.extend(extract_error_recovery_members(
                ctx,
                child,
                type_ctx,
                already_found,
            ));
        }
    }

    result
}

/// Perform shallow error recovery on ERROR nodes.
/// Only scans direct children of ERROR nodes - no recursive traversal.
/// This is a best-effort extraction that supplements valid member extraction.
pub fn extract_error_recovery_members(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
    already_found: &[CurrentClassMember],
) -> Vec<CurrentClassMember> {
    let mut result = Vec::new();

    // Only process if this is an ERROR node
    if node.kind() != "ERROR" {
        return result;
    }

    // Shallow scan: only look at direct children using parse_partial_methods_from_error
    result.extend(parse_partial_methods_from_error(
        ctx,
        type_ctx,
        node,
        already_found,
    ));

    // Optionally: scan direct children for nested ERROR nodes
    // But DO NOT recurse into their children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "ERROR" {
            result.extend(parse_partial_methods_from_error(
                ctx,
                type_ctx,
                child,
                already_found,
            ));
        }
    }

    result
}

/// Extract valid members only - no ERROR node processing.
/// This is the core structural traversal that processes well-formed AST nodes.
fn collect_valid_members_impl(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
    members: &mut Vec<CurrentClassMember>,
    allow_nested_types: bool,
) {
    use tree_sitter_utils::handler_fn;

    type MemberCtx<'a> = (&'a JavaContextExtractor, &'a SourceTypeCtx, bool);

    // Build a handler chain that returns Vec<CurrentClassMember> for each child
    // Key difference from old implementation: NO ERROR handler
    let member_handler = handler_fn(|inp: Input<MemberCtx>| -> Vec<CurrentClassMember> {
        let (ctx, type_ctx, allow_nested_types) = inp.ctx;
        // Only extract constructors when NOT allowing nested types (i.e., in class body context)
        if !allow_nested_types && let Some(m) = parse_constructor_node(ctx, type_ctx, inp.node) {
            return vec![m];
        }
        vec![]
    })
    .for_kinds(&["constructor_declaration", "compact_constructor_declaration"])
    .or(
        // Method declarations - with safe ERROR recovery in body
        handler_fn(|inp: Input<MemberCtx>| -> Vec<CurrentClassMember> {
            let (ctx, type_ctx, _allow_nested_types) = inp.ctx;
            let mut result = Vec::new();

            // Always extract the method itself
            if let Some(m) = parse_method_node(ctx, type_ctx, inp.node) {
                result.push(m);
            }

            // Safe error recovery: scan method body for ERROR nodes and misread methods
            // This handles cases where tree-sitter puts methods inside ERROR nodes in method bodies
            if let Some(body) = inp.node.child_by_field_name("body") {
                let body_children: Vec<Node> = {
                    let mut body_cursor = body.walk();
                    body.children(&mut body_cursor).collect()
                };

                let mut i = 0;
                while i < body_children.len() {
                    let child = body_children[i];

                    match child.kind() {
                        "ERROR" => {
                            // Shallow error recovery - no recursion
                            let snapshot = result.clone();
                            result.extend(parse_partial_methods_from_error(
                                ctx, type_ctx, child, &snapshot,
                            ));

                            // Also check for method_declaration nodes inside ERROR (one level only)
                            let mut error_cursor = child.walk();
                            for error_child in child.children(&mut error_cursor) {
                                if error_child.kind() == "method_declaration" {
                                    if let Some(m) = parse_method_node(ctx, type_ctx, error_child) {
                                        result.push(m);
                                    }
                                } else if error_child.kind() == "field_declaration" {
                                    result.extend(parse_field_node(ctx, type_ctx, error_child));
                                }
                            }
                        }
                        "local_variable_declaration" => {
                            // Check for misread method pattern: local_variable_declaration followed by ERROR with '('
                            if let Some(next_node) = body_children.get(i + 1)
                                && next_node.kind() == "ERROR"
                                && ctx.source[next_node.start_byte()..next_node.end_byte()]
                                    .trim_start()
                                    .starts_with('(')
                                && let Some(m) =
                                    parse_misread_method(ctx, type_ctx, child, *next_node)
                            {
                                result.push(m);
                                i += 1; // Skip the ERROR node we just processed
                            }
                        }
                        "method_declaration" => {
                            // Method inside method body (from ERROR recovery)
                            if let Some(m) = parse_method_node(ctx, type_ctx, child) {
                                result.push(m);
                            }
                        }
                        "field_declaration" => {
                            // Field inside method body (from ERROR recovery)
                            result.extend(parse_field_node(ctx, type_ctx, child));
                        }
                        _ => {}
                    }

                    i += 1;
                }
            }

            result
        })
        .for_kinds(&["method_declaration"]),
    )
    .or(
        // Field declarations
        handler_fn(|inp: Input<MemberCtx>| -> Vec<CurrentClassMember> {
            let (ctx, type_ctx, _) = inp.ctx;
            parse_field_node(ctx, type_ctx, inp.node)
        })
        .for_kinds(&["field_declaration"]),
    )
    .or(
        // Nested type declarations (only when allowing nested types)
        handler_fn(|inp: Input<MemberCtx>| -> Vec<CurrentClassMember> {
            let (ctx, type_ctx, allow_nested_types) = inp.ctx;
            if allow_nested_types {
                let mut result = Vec::new();
                collect_valid_members_impl(
                    ctx,
                    inp.node,
                    type_ctx,
                    &mut result,
                    allow_nested_types,
                );
                return result;
            }
            vec![]
        })
        .for_kinds(&[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "annotation_type_declaration",
            "record_declaration",
        ]),
    )
    .or(
        // Body containers - recurse
        handler_fn(|inp: Input<MemberCtx>| -> Vec<CurrentClassMember> {
            let (ctx, type_ctx, allow_nested_types) = inp.ctx;
            let mut result = Vec::new();
            collect_valid_members_impl(ctx, inp.node, type_ctx, &mut result, allow_nested_types);
            result
        })
        .for_kinds(&[
            "class_body",
            "interface_body",
            "enum_body",
            "enum_body_declarations",
            "program",
        ]),
    );
    // NOTE: NO ERROR handler - ERROR nodes are completely skipped

    // Apply handler to all children and collect results
    let ctx_tuple = (ctx, type_ctx, allow_nested_types);
    if let Some(child_members) = member_handler
        .for_children()
        .handle(Input::new(node, ctx_tuple, None))
    {
        for child_vec in child_members {
            members.extend(child_vec);
        }
    }
}

/// Collect members from a node using two-phase approach.
/// This is used for fallback error recovery when no type declaration is found.
pub fn collect_members_from_node(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
    members: &mut Vec<CurrentClassMember>,
) {
    // PHASE 1: Extract valid members only
    let valid_members = extract_valid_members_only_with_nested(ctx, node, type_ctx);
    members.extend(valid_members);

    // PHASE 2: Error recovery
    let error_members = extract_error_recovery_members(ctx, node, type_ctx, members);
    members.extend(error_members);
}

/// Extract valid members with nested types allowed.
fn extract_valid_members_only_with_nested(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
) -> Vec<CurrentClassMember> {
    let mut members = Vec::new();
    collect_valid_members_impl(ctx, node, type_ctx, &mut members, true);
    members
}

pub fn parse_partial_methods_from_error(
    ctx: &JavaContextExtractor,
    type_ctx: &SourceTypeCtx,
    error_node: Node,
    already_found: &[CurrentClassMember],
) -> Vec<CurrentClassMember> {
    let found_names: std::collections::HashSet<Arc<str>> =
        already_found.iter().map(|m| m.name()).collect();

    let mut cursor = error_node.walk();
    let children: Vec<Node> = error_node.children(&mut cursor).collect();
    let mut result = Vec::new();

    for (param_pos, _) in children
        .iter()
        .enumerate()
        .filter(|(_, n)| n.kind() == "formal_parameters")
    {
        let params_node = children[param_pos];
        let name = match children[..param_pos]
            .iter()
            .rev()
            .find(|n| n.kind() == "identifier")
        {
            Some(n) => ctx.node_text(*n),
            None => continue,
        };
        if name == "<init>" || name == "<clinit>" || found_names.contains(name) {
            continue;
        }

        let mut method_annos: Vec<AnnotationSummary> = Vec::new();
        let mut flags = 0;
        if let Some(n) = children[..param_pos]
            .iter()
            .rev()
            .find(|n| n.kind() == "modifiers")
        {
            flags = parse_java_modifiers(ctx.node_text(*n));
            method_annos = parse_annotations_in_node(ctx, *n, type_ctx);
        } else {
            // fallback: scan ERROR nodes backward for annotations
            for prev in children[..param_pos].iter().rev() {
                if prev.kind() == "ERROR" {
                    let mut a = parse_annotations_in_node(ctx, *prev, type_ctx);
                    if !a.is_empty() {
                        method_annos.append(&mut a);
                        break;
                    }
                }
            }
        }

        let ret_type = children[..param_pos]
            .iter()
            .rev()
            .find(|n| is_method_return_type_kind(n.kind()))
            .map(|n| ctx.node_text(*n))
            .unwrap_or("void");

        let descriptor = build_java_descriptor(ctx.node_text(params_node), ret_type, type_ctx);

        if name == "add" {
            tracing::debug!(
                source = "error-formal-parameters",
                method_name = name,
                descriptor,
                ret_type,
                error_node_range = ?(error_node.start_byte(), error_node.end_byte()),
                "members::parse_partial_methods_from_error: synthesized add"
            );
        }

        if has_spread_parameter(params_node) {
            flags |= ACC_VARARGS;
        }

        result.push(CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from(name),
            params: parse_params(ctx, &descriptor, params_node, type_ctx),
            annotations: method_annos,
            access_flags: flags,
            is_synthetic: false,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(&descriptor),
        })));
    }

    for (mi_pos, mi_node) in children
        .iter()
        .enumerate()
        .filter(|(_, n)| n.kind() == "method_invocation")
    {
        let name = match mi_node.child_by_field_name("name") {
            Some(n) => ctx.node_text(n),
            None => continue,
        };
        if name == "<init>"
            || name == "<clinit>"
            || found_names.contains(name)
            || is_java_keyword(name)
        {
            continue;
        }

        let mut flags = 0;
        let mut method_annos: Vec<AnnotationSummary> = Vec::new();
        let mut ret_type = "void";

        for prev in children[..mi_pos].iter().rev() {
            match prev.kind() {
                "identifier" => {
                    method_annos = parse_annotations_in_node(ctx, *prev, type_ctx);
                    flags |= parse_java_modifiers(ctx.node_text(*prev));
                }
                "void_type"
                | "integral_type"
                | "floating_point_type"
                | "boolean_type"
                | "type_identifier"
                | "scoped_type_identifier"
                | "array_type"
                | "generic_type" => {
                    if ret_type == "void" {
                        ret_type = ctx.node_text(*prev);
                    }
                }
                "ERROR" => {
                    if method_annos.is_empty() {
                        let mut a = parse_annotations_in_node(ctx, *prev, type_ctx);
                        if !a.is_empty() {
                            method_annos.append(&mut a);
                        }
                    }
                    let mut pc = prev.walk();
                    for pchild in prev.children(&mut pc) {
                        match pchild.kind() {
                            "identifier" | "static" | "private" | "public" | "protected" => {
                                flags |= parse_java_modifiers(ctx.node_text(pchild));
                            }
                            "void_type"
                            | "integral_type"
                            | "floating_point_type"
                            | "boolean_type"
                            | "type_identifier"
                            | "scoped_type_identifier"
                            | "array_type"
                            | "generic_type" => {
                                if ret_type == "void" {
                                    ret_type = ctx.node_text(pchild);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if flags == 0 {
            flags = ACC_PUBLIC;
        }

        let args = mi_node
            .child_by_field_name("arguments")
            .map(|n| ctx.node_text(n))
            .unwrap_or("()");
        let descriptor = build_java_descriptor(args, ret_type, type_ctx);

        if name == "add" {
            tracing::debug!(
                source = "error-method-invocation",
                method_name = name,
                descriptor,
                args,
                ret_type,
                error_node_range = ?(error_node.start_byte(), error_node.end_byte()),
                "members::parse_partial_methods_from_error: synthesized add"
            );
        }

        result.push(CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from(name),
            params: MethodParams::empty(),
            annotations: method_annos,
            access_flags: flags,
            is_synthetic: false,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(&descriptor),
        })));
    }

    result
}

pub fn parse_method_node(
    ctx: &JavaContextExtractor,
    type_ctx: &SourceTypeCtx,
    node: Node,
) -> Option<CurrentClassMember> {
    let mut flags = 0;
    let mut ret_type = "void";
    let mut method_annos: Vec<AnnotationSummary> = Vec::new();

    // Use traversal utilities where applicable
    if let Some(modifiers) = first_child_of_kind(node, "modifiers") {
        flags = parse_java_modifiers(ctx.node_text(modifiers));
        method_annos = parse_annotations_in_node(ctx, modifiers, type_ctx);
    }

    let name = first_child_of_kind(node, "identifier").map(|n| ctx.node_text(n));

    if let Some(ret_node) = first_child_of_kinds(
        node,
        &[
            "void_type",
            "integral_type",
            "floating_point_type",
            "boolean_type",
            "type_identifier",
            "scoped_type_identifier",
            "array_type",
            "generic_type",
        ],
    ) {
        ret_type = ctx.node_text(ret_node);
    }

    let params_node = first_child_of_kind(node, "formal_parameters");

    if flags == 0 {
        flags = ACC_PUBLIC;
    }

    let name = name.filter(|n| *n != "<init>" && *n != "<clinit>" && !is_java_keyword(n))?;
    let params_text = params_node.map(|n| ctx.node_text(n)).unwrap_or("()");
    if let Some(params_node) = params_node
        && has_spread_parameter(params_node)
    {
        flags |= ACC_VARARGS;
    }
    let descriptor = build_java_descriptor(params_text, ret_type, type_ctx);

    let generic_signature =
        build_source_method_generic_signature(ctx, type_ctx, node, params_text, ret_type)
            .or_else(|| extract_generic_signature(node, ctx.bytes(), &descriptor));

    let params = params_node
        .map(|n| parse_params(ctx, &descriptor, n, type_ctx))
        .unwrap_or(MethodParams::empty());

    if name == "add" {
        tracing::debug!(
            method_name = name,
            descriptor,
            generic_signature = ?generic_signature,
            return_type = ?parse_return_type_from_descriptor(&descriptor),
            param_descriptors = ?params.items.iter().map(|p| p.descriptor.as_ref()).collect::<Vec<_>>(),
            param_names = ?params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
            "members::parse_method_node: source method extracted"
        );
    }

    Some(CurrentClassMember::Method(Arc::new(MethodSummary {
        name: Arc::from(name),
        params,
        annotations: method_annos,
        access_flags: flags,
        is_synthetic: false,
        generic_signature,
        return_type: parse_return_type_from_descriptor(&descriptor),
    })))
}

/// Parse constructor_declaration or compact_constructor_declaration nodes.
/// For compact constructors, we need to extract parameters from the parent record_declaration.
pub fn parse_constructor_node(
    ctx: &JavaContextExtractor,
    type_ctx: &SourceTypeCtx,
    node: Node,
) -> Option<CurrentClassMember> {
    let mut flags = 0;
    let mut params_node: Option<Node>;
    let mut method_annos: Vec<AnnotationSummary> = Vec::new();

    // Use traversal utilities
    if let Some(modifiers) = first_child_of_kind(node, "modifiers") {
        flags = parse_java_modifiers(ctx.node_text(modifiers));
        method_annos = parse_annotations_in_node(ctx, modifiers, type_ctx);
    }

    params_node = first_child_of_kind(node, "formal_parameters");

    // For compact constructors, find the parent record_declaration to get parameters
    if node.kind() == "compact_constructor_declaration"
        && let Some(record) = ancestor_of_kind(node, "record_declaration")
    {
        params_node = record
            .child_by_field_name("parameters")
            .or_else(|| first_child_of_kind(record, "formal_parameters"));
    }

    if flags == 0 {
        flags = ACC_PUBLIC;
    }

    let params_text = params_node.map(|n| ctx.node_text(n)).unwrap_or("()");
    if let Some(params_node) = params_node
        && has_spread_parameter(params_node)
    {
        flags |= ACC_VARARGS;
    }

    // Constructors always have void return type
    let descriptor = build_java_descriptor(params_text, "void", type_ctx);

    let generic_signature = extract_generic_signature(node, ctx.bytes(), &descriptor);

    let params = params_node
        .map(|n| parse_params(ctx, &descriptor, n, type_ctx))
        .unwrap_or(MethodParams::empty());

    Some(CurrentClassMember::Method(Arc::new(MethodSummary {
        name: Arc::from("<init>"),
        params,
        annotations: method_annos,
        access_flags: flags,
        is_synthetic: false,
        generic_signature,
        return_type: None,
    })))
}

fn build_source_method_generic_signature(
    ctx: &JavaContextExtractor,
    type_ctx: &SourceTypeCtx,
    node: Node,
    params_text: &str,
    ret_type: &str,
) -> Option<Arc<str>> {
    let type_params_prefix = extract_type_parameters_prefix(node, ctx.bytes())?;
    let inner = params_text
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');
    let param_sigs = if inner.trim().is_empty() {
        String::new()
    } else {
        split_params(inner)
            .into_iter()
            .map(|param| source_type_to_signature(type_ctx, extract_param_type(param.trim())))
            .collect::<Vec<_>>()
            .join("")
    };
    let ret_sig = source_type_to_signature(type_ctx, ret_type.trim());
    Some(Arc::from(
        format!("{}({}){}", type_params_prefix, param_sigs, ret_sig).as_str(),
    ))
}

fn source_type_to_signature(type_ctx: &SourceTypeCtx, ty: &str) -> String {
    fn split_generic_base_local(ty: &str) -> Option<(&str, Option<&str>)> {
        if let Some(start) = ty.find('<') {
            let mut depth = 0i32;
            for (i, c) in ty.char_indices().skip(start) {
                match c {
                    '<' => depth += 1,
                    '>' => {
                        depth -= 1;
                        if depth == 0 {
                            let base = ty[..start].trim();
                            let args = ty[start + 1..i].trim();
                            return Some((base, Some(args)));
                        }
                    }
                    _ => {}
                }
            }
            None
        } else {
            Some((ty.trim(), None))
        }
    }

    fn split_generic_args_local(s: &str) -> Vec<&str> {
        let mut result = Vec::new();
        let mut depth = 0i32;
        let mut start = 0usize;
        for (i, c) in s.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => depth -= 1,
                ',' if depth == 0 => {
                    result.push(s[start..i].trim());
                    start = i + 1;
                }
                _ => {}
            }
        }
        if start < s.len() {
            result.push(s[start..].trim());
        }
        result.into_iter().filter(|x| !x.is_empty()).collect()
    }

    fn strip_leading_modifiers(mut s: &str) -> &str {
        loop {
            s = s.trim_start();
            if let Some(rest) = s.strip_prefix("final ") {
                s = rest;
                continue;
            }
            if s.starts_with('@')
                && let Some(space) = s.find(' ')
            {
                s = &s[space + 1..];
                continue;
            }
            break;
        }
        s.trim()
    }

    let mut s = strip_leading_modifiers(ty.trim());
    if s == "?" {
        return "*".to_string();
    }
    if let Some(bound) = s.strip_prefix("? extends ") {
        return format!("+{}", source_type_to_signature(type_ctx, bound));
    }
    if let Some(bound) = s.strip_prefix("? super ") {
        return format!("-{}", source_type_to_signature(type_ctx, bound));
    }

    let mut dims = 0usize;
    if let Some(stripped) = s.strip_suffix("...") {
        s = stripped.trim();
        dims += 1;
    }
    while let Some(stripped) = s.strip_suffix("[]") {
        s = stripped.trim();
        dims += 1;
    }

    let (base, args) = split_generic_base_local(s).unwrap_or((s, None));
    let mut out = match base {
        "void" => "V".to_string(),
        "boolean" => "Z".to_string(),
        "byte" => "B".to_string(),
        "char" => "C".to_string(),
        "short" => "S".to_string(),
        "int" => "I".to_string(),
        "long" => "J".to_string(),
        "float" => "F".to_string(),
        "double" => "D".to_string(),
        other => {
            let resolved = type_ctx.resolve_simple(other);
            let internal = resolved.replace('.', "/");
            let is_type_var =
                !internal.contains('/') && internal.chars().all(|c| c.is_ascii_uppercase());
            if is_type_var {
                format!("T{};", internal)
            } else if let Some(arg_str) = args {
                let arg_sigs = split_generic_args_local(arg_str)
                    .into_iter()
                    .map(|a| source_type_to_signature(type_ctx, a))
                    .collect::<Vec<_>>()
                    .join("");
                format!("L{}<{}>;", internal, arg_sigs)
            } else {
                format!("L{};", internal)
            }
        }
    };

    for _ in 0..dims {
        out = format!("[{}", out);
    }
    out
}

fn parse_field_node(
    ctx: &JavaContextExtractor,
    type_ctx: &SourceTypeCtx,
    node: Node,
) -> Vec<CurrentClassMember> {
    let mut flags = 0;
    let mut field_type = "Object";
    let mut names = Vec::new();
    let mut wc = node.walk();
    let mut field_annos: Vec<AnnotationSummary> = Vec::new();

    for c in node.children(&mut wc) {
        match c.kind() {
            "modifiers" => {
                flags = parse_java_modifiers(ctx.node_text(c));
                field_annos = parse_annotations_in_node(ctx, c, type_ctx);
            }
            "void_type"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
            | "type_identifier"
            | "array_type"
            | "generic_type" => {
                field_type = ctx.node_text(c);
            }
            "variable_declarator" => {
                let mut vc = c.walk();
                for vchild in c.children(&mut vc) {
                    if vchild.kind() == "identifier" {
                        let n = ctx.node_text(vchild);
                        if !is_java_keyword(n) {
                            names.push(n.to_string());
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if flags == 0 {
        flags = ACC_PUBLIC;
    }

    names
        .into_iter()
        .map(|name| {
            let desc = type_ctx.to_descriptor(field_type);
            CurrentClassMember::Field(Arc::new(FieldSummary {
                name: Arc::from(name.as_str()),
                descriptor: Arc::from(desc.as_str()),
                access_flags: flags,
                annotations: field_annos.clone(),
                is_synthetic: false,
                generic_signature: None,
            }))
        })
        .collect()
}

fn parse_misread_method(
    ctx: &JavaContextExtractor,
    type_ctx: &SourceTypeCtx,
    decl_node: Node,
    error_node: Node,
) -> Option<CurrentClassMember> {
    let mut flags = 0;
    let mut ret_type = "void";
    let mut name: Option<&str> = None;
    let mut method_annos: Vec<AnnotationSummary> = Vec::new();

    let mut wc = decl_node.walk();
    for c in decl_node.named_children(&mut wc) {
        match c.kind() {
            "modifiers" => {
                let t = ctx.node_text(c);
                if t.contains("static") {
                    flags |= ACC_STATIC;
                }
                if t.contains("public") {
                    flags |= ACC_PUBLIC;
                }
                if t.contains("private") {
                    flags |= ACC_PRIVATE;
                }
                if t.contains("protected") {
                    flags |= ACC_PROTECTED;
                }
                method_annos = parse_annotations_in_node(ctx, c, type_ctx);
            }
            "type_identifier"
            | "scoped_type_identifier"
            | "void_type"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
            | "array_type"
            | "generic_type" => {
                ret_type = ctx.node_text(c);
            }
            "variable_declarator" => {
                if let Some(id_node) = c.child_by_field_name("name") {
                    name = Some(ctx.node_text(id_node));
                }
            }
            _ => {}
        }
    }

    let name = name.filter(|n| *n != "<init>" && *n != "<clinit>" && !is_java_keyword(n))?;
    let mut ec = error_node.walk();
    let params_node = error_node
        .children(&mut ec)
        .find(|c| c.kind() == "formal_parameters");
    let descriptor = build_java_descriptor(
        params_node.map(|n| ctx.node_text(n)).unwrap_or("()"),
        ret_type,
        type_ctx,
    );

    let params = params_node
        .map(|n| parse_params(ctx, &descriptor, n, type_ctx))
        .unwrap_or(MethodParams { items: vec![] });
    if let Some(pn) = params_node
        && has_spread_parameter(pn)
    {
        flags |= ACC_VARARGS;
    }

    Some(CurrentClassMember::Method(Arc::new(MethodSummary {
        name: Arc::from(name),
        params,
        annotations: method_annos,
        access_flags: flags,
        is_synthetic: false,
        generic_signature: None,
        return_type: parse_return_type_from_descriptor(&descriptor),
    })))
}

pub fn parse_annotations_in_node(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
) -> Vec<AnnotationSummary> {
    let (q, idx) = annotation_query();

    let mut out = Vec::new();
    for caps in run_query(q, node, ctx.bytes(), None) {
        let anno_node = match caps.iter().find(|(i, _)| *i == *idx) {
            Some((_, n)) => *n,
            None => continue,
        };
        if let Some(s) = parse_single_annotation(ctx, anno_node, type_ctx) {
            out.push(s);
        }
    }
    out
}

pub(crate) fn parse_single_annotation(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
) -> Option<AnnotationSummary> {
    let (q, n_idx) = annotation_name_query();

    let name = run_query(q, node, ctx.bytes(), None)
        .first()
        .and_then(|caps| capture_text(caps, *n_idx, ctx.bytes()))?;

    let resolved = if name.contains('.') {
        name.replace('.', "/")
    } else {
        type_ctx.resolve_simple(name)
    };
    let internal = resolved.replace('.', "/");

    let mut elements = rustc_hash::FxHashMap::default();
    if node.kind() == "annotation"
        && let Some(args) = node.child_by_field_name("arguments")
    {
        parse_annotation_arguments(ctx, args, &mut elements, type_ctx);
    }

    Some(AnnotationSummary {
        internal_name: intern_str(&internal),
        runtime_visible: true,
        elements,
    })
}

fn parse_annotation_arguments(
    ctx: &JavaContextExtractor,
    args: Node,
    elements: &mut rustc_hash::FxHashMap<Arc<str>, crate::index::AnnotationValue>,
    type_ctx: &SourceTypeCtx,
) {
    let mut wc = args.walk();
    let children: Vec<Node> = args.named_children(&mut wc).collect();
    if children.is_empty() {
        return;
    }

    let has_pairs = children.iter().any(|n| n.kind() == "element_value_pair");

    if has_pairs {
        for child in &children {
            if child.kind() != "element_value_pair" {
                continue;
            }
            let key = child
                .child_by_field_name("key")
                .map(|n| Arc::from(ctx.node_text(n)))
                .unwrap_or_else(|| Arc::from("value"));
            if let Some(vn) = child.child_by_field_name("value") {
                elements.insert(key, parse_element_value_node(ctx, vn, type_ctx));
            }
        }
    } else {
        // 单值简写：@Anno(X) 或 @Anno({X, Y})
        let val = if children.len() == 1 {
            parse_element_value_node(ctx, children[0], type_ctx)
        } else {
            crate::index::AnnotationValue::Array(
                children
                    .iter()
                    .map(|n| parse_element_value_node(ctx, *n, type_ctx))
                    .collect(),
            )
        };
        elements.insert(Arc::from("value"), val);
    }
}

fn parse_element_value_node(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
) -> crate::index::AnnotationValue {
    use crate::index::AnnotationValue;
    match node.kind() {
        "string_literal" => {
            let raw = ctx.node_text(node);
            let s = raw.trim_start_matches('"').trim_end_matches('"');
            AnnotationValue::String(Arc::from(s))
        }
        "field_access" => {
            let object = node
                .child_by_field_name("object")
                .map(|n| ctx.node_text(n))
                .unwrap_or("?");
            let field = node
                .child_by_field_name("field")
                .map(|n| ctx.node_text(n))
                .unwrap_or("?");
            AnnotationValue::Enum {
                type_name: Arc::from(type_ctx.resolve_simple(object)),
                const_name: Arc::from(field),
            }
        }
        "scoped_identifier" => {
            let text = ctx.node_text(node);
            if let Some((qualifier, field)) = text.rsplit_once('.') {
                AnnotationValue::Enum {
                    type_name: Arc::from(type_ctx.resolve_simple(qualifier)),
                    const_name: Arc::from(field),
                }
            } else {
                AnnotationValue::Unknown
            }
        }
        // @Target({...}) 里的数组用的是 element_value_array_initializer，不是 array_initializer
        "element_value_array_initializer" => {
            let mut wc = node.walk();
            AnnotationValue::Array(
                node.named_children(&mut wc)
                    .map(|child| parse_element_value_node(ctx, child, type_ctx))
                    .collect(),
            )
        }
        "true" => AnnotationValue::Boolean(true),
        "false" => AnnotationValue::Boolean(false),
        "decimal_integer_literal" => {
            let t = ctx.node_text(node).trim_end_matches(['l', 'L']);
            t.parse::<i32>()
                .map(AnnotationValue::Int)
                .unwrap_or(AnnotationValue::Unknown)
        }
        "hex_integer_literal" => {
            let t = ctx
                .node_text(node)
                .trim_end_matches(['l', 'L'])
                .trim_start_matches("0x")
                .trim_start_matches("0X");
            i32::from_str_radix(t, 16)
                .map(AnnotationValue::Int)
                .unwrap_or(AnnotationValue::Unknown)
        }
        "decimal_floating_point_literal" => {
            let t = ctx.node_text(node).trim_end_matches(['f', 'F', 'd', 'D']);
            t.parse::<f64>()
                .map(AnnotationValue::Double)
                .unwrap_or(AnnotationValue::Unknown)
        }
        "class_literal" => {
            let t = ctx.node_text(node).trim_end_matches(".class").trim();
            AnnotationValue::Class(Arc::from(t))
        }
        "identifier" => {
            // 无限定的常量引用，如直接写 METHOD（import static 后）
            AnnotationValue::Enum {
                type_name: Arc::from(""),
                const_name: Arc::from(ctx.node_text(node)),
            }
        }
        "marker_annotation" | "annotation" => parse_single_annotation(ctx, node, type_ctx)
            .map(|s| AnnotationValue::Nested(Box::new(s)))
            .unwrap_or(AnnotationValue::Unknown),
        _ => AnnotationValue::Unknown,
    }
}

/// Walk backwards to find a block comment starting with `/**`
pub fn extract_javadoc(node: Node, bytes: &[u8]) -> Option<Arc<str>> {
    let mut prev = node.prev_sibling();
    while let Some(n) = prev {
        if n.kind() == "block_comment" {
            let text = n.utf8_text(bytes).unwrap_or("");
            if text.starts_with("/**") {
                return Some(Arc::from(text));
            }
            break; // Standard block comment, not javadoc
        } else if n.kind() == "line_comment" {
            prev = n.prev_sibling(); // Skip over normal comments
        } else {
            break; // Found code, stop looking
        }
    }
    None
}

fn parse_params(
    ctx: &JavaContextExtractor,
    method_desc: &str,
    node: Node,
    type_ctx: &SourceTypeCtx,
) -> MethodParams {
    let mut out = MethodParams::from_method_descriptor(method_desc);

    let mut cursor = node.walk();
    let mut i = 0usize;

    for child in node.children(&mut cursor) {
        if !matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }

        if i >= out.items.len() {
            break; // AST 比 descriptor 多
        }

        // name
        if let Some(n) = extract_param_name_node(child) {
            out.items[i].name = Arc::from(ctx.node_text(n));
        }

        // annotations: prefer modifiers, fallback scan the param node itself
        let annos = if let Some(m) = child.child_by_field_name("modifiers") {
            parse_annotations_in_node(ctx, m, type_ctx)
        } else {
            parse_annotations_in_node(ctx, child, type_ctx)
        };
        out.items[i].annotations = annos;

        i += 1;
    }

    out
}

fn extract_param_name_node(param_node: Node) -> Option<Node> {
    if let Some(n) = param_node.child_by_field_name("name") {
        return Some(n);
    }
    let mut wc = param_node.walk();
    for child in param_node.named_children(&mut wc) {
        if child.kind() == "identifier" {
            return Some(child);
        }
        if child.kind() == "variable_declarator" {
            if let Some(n) = child.child_by_field_name("name") {
                return Some(n);
            }
            let mut vc = child.walk();
            if let Some(id) = child
                .named_children(&mut vc)
                .find(|c| c.kind() == "identifier")
            {
                return Some(id);
            }
        }
    }
    None
}

fn has_spread_parameter(formals_node: Node) -> bool {
    let mut cursor = formals_node.walk();
    formals_node
        .children(&mut cursor)
        .any(|c| c.kind() == "spread_parameter")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ClassOrigin;
    use crate::language::java::class_parser::{
        parse_java_source_via_tree_for_test, parse_java_source_with_view_via_tree_for_test,
        test_fixture_class,
    };
    use crate::salsa_db::{Database, FileId, SourceFile};
    use tower_lsp::lsp_types::Url;

    fn current_members(source: &str) -> Vec<CurrentClassMember> {
        let workspace_index =
            crate::index::WorkspaceIndexHandle::new(crate::index::WorkspaceIndex::new());
        workspace_index.update(|index| {
            index.add_jdk_classes(vec![
                test_fixture_class("java/lang/Object"),
                test_fixture_class("java/lang/String"),
            ]);
        });
        let db = Database::with_workspace_index(workspace_index);
        let file = SourceFile::new(
            &db,
            FileId::new(Url::parse("file:///test/A.java").expect("valid test uri")),
            source.to_string(),
            Arc::from("java"),
        );
        let cursor_offset = source
            .find("class A")
            .map(|idx| idx + "class A".len())
            .unwrap_or_else(|| source.len().saturating_sub(1));
        let mut members: Vec<CurrentClassMember> =
            crate::salsa_queries::extract_java_current_class_members_from_source(
                &db,
                file,
                cursor_offset,
            )
            .into_values()
            .collect();
        members.sort_by(|left, right| left.name().cmp(&right.name()));
        members
    }

    fn first_class(source: &str) -> crate::index::ClassMetadata {
        parse_java_source_via_tree_for_test(source, ClassOrigin::Unknown, None)
            .into_iter()
            .next()
            .expect("at least one parsed type")
    }

    #[test]
    fn test_parse_standard_members() {
        let src = indoc::indoc! {r#"
        class A {
            public int a;
            private static String b;

            public void methodA() {}
            private static Object methodB(int p) { return null; }
        }
        "#};
        let members = current_members(src);

        let a = members.iter().find(|m| m.name().as_ref() == "a").unwrap();
        assert!(!a.is_method() && !a.is_static() && !a.is_private());

        let b = members.iter().find(|m| m.name().as_ref() == "b").unwrap();
        assert!(!b.is_method() && b.is_static() && b.is_private());

        let ma = members
            .iter()
            .find(|m| m.name().as_ref() == "methodA")
            .unwrap();
        assert!(ma.is_method() && !ma.is_static() && !ma.is_private());

        let mb = members
            .iter()
            .find(|m| m.name().as_ref() == "methodB")
            .unwrap();
        assert!(mb.is_method() && mb.is_static() && mb.is_private());
    }

    #[test]
    fn test_parse_varargs_method_sets_acc_varargs() {
        let src = indoc::indoc! {r#"
        class A {
            public static String join(String separator, String... parts) { return ""; }
        }
        "#};
        let members = current_members(src);

        let join = members
            .iter()
            .find(|m| m.name().as_ref() == "join")
            .expect("join method");
        let CurrentClassMember::Method(join) = join else {
            panic!("join should be method");
        };
        assert_ne!(join.access_flags & ACC_VARARGS, 0);
        assert!(
            join.desc().as_ref().contains("[Ljava/lang/String;"),
            "expected varargs array descriptor in method signature, got {}",
            join.desc()
        );
        assert_eq!(
            join.params
                .param_names()
                .iter()
                .map(|n| n.as_ref())
                .collect::<Vec<_>>(),
            vec!["separator", "parts"],
            "varargs parameter names should be preserved"
        );
    }

    #[test]
    fn test_parse_scoped_object_return_type_not_void() {
        let src = indoc::indoc! {r#"
        class A {
            protected java.lang.Object clone() { return null; }
        }
        "#};
        let members = current_members(src);

        let clone = members
            .iter()
            .find(|m| m.name().as_ref() == "clone")
            .expect("clone method");
        let CurrentClassMember::Method(clone) = clone else {
            panic!("clone should be method");
        };
        assert!(
            clone.desc().as_ref().contains("Object"),
            "descriptor should preserve Object return type, got {}",
            clone.desc()
        );
        assert_ne!(
            clone.return_type.as_deref(),
            Some("V"),
            "scoped java.lang.Object return type must not collapse to void"
        );
        let ret = clone.return_type.as_deref().unwrap_or("");
        assert!(
            ret.contains("Object"),
            "return type should preserve Object, got {ret}"
        );
    }

    #[test]
    fn test_ignore_constructors() {
        let src = indoc::indoc! {r#"
        class A {
            public A() {}
            static { }
            void normalMethod() {}
        }
        "#};
        let members = current_members(src);

        assert!(members.iter().any(|m| m.name().as_ref() == "normalMethod"));
        assert!(!members.iter().any(|m| m.name().as_ref() == "<init>"));
        assert!(!members.iter().any(|m| m.name().as_ref() == "<clinit>"));
        assert!(!members.iter().any(|m| m.name().as_ref() == "A"));
    }

    #[test]
    fn test_multiple_field_declarators() {
        let src = indoc::indoc! {r#"
        class A {
            private static int x, y;
        }
        "#};
        let members = current_members(src);

        let x = members.iter().find(|m| m.name().as_ref() == "x").unwrap();
        assert!(!x.is_method() && x.is_static() && x.is_private());

        let y = members.iter().find(|m| m.name().as_ref() == "y").unwrap();
        assert!(!y.is_method() && y.is_static() && y.is_private());
    }

    #[test]
    fn test_swallowed_error_node_members() {
        let src = indoc::indoc! {r#"
        class A {
            void brokenMethod() {
                System.out.println("No closing brace"
            
            private static void swallowedMethod() {}
        }
        "#};
        let members = current_members(src);

        let swallowed = members
            .iter()
            .find(|m| m.name().as_ref() == "swallowedMethod");
        assert!(swallowed.is_some());
        let swallowed = swallowed.unwrap();
        assert!(swallowed.is_method() && swallowed.is_static() && swallowed.is_private());
    }

    #[test]
    fn test_misread_method_as_local_variable() {
        let src = indoc::indoc! {r#"
        class A {
            void brokenMethod() {
                int x = 1 // missing semicolon
            
            private static String misreadMethod(int a) {}
        }
        "#};
        let members = current_members(src);

        let misread = members
            .iter()
            .find(|m| m.name().as_ref() == "misreadMethod");
        assert!(misread.is_some());
        let misread = misread.unwrap();
        assert!(misread.is_method() && misread.is_static() && misread.is_private());
    }

    #[test]
    fn test_partial_methods_from_top_level_error() {
        let src = indoc::indoc! {r#"
        package org.example;
        
        public class A {
            void foo() {
                // missing braces mess up everything below
        
        public static void salvagedMethod(String arg) { }
        
        private Object anotherSalvaged() { return null; }
        "#};

        let members = current_members(src);

        let salvaged1 = members
            .iter()
            .find(|m| m.name().as_ref() == "salvagedMethod")
            .unwrap();
        assert!(salvaged1.is_method() && salvaged1.is_static() && !salvaged1.is_private());

        let salvaged2 = members
            .iter()
            .find(|m| m.name().as_ref() == "anotherSalvaged")
            .unwrap();
        assert!(salvaged2.is_method() && !salvaged2.is_static() && salvaged2.is_private());
    }

    #[test]
    fn test_java_annotations_on_method_field_param() {
        let src = indoc::indoc! {r#"
    class A {
        @Deprecated
        public @SuppressWarnings("x") int f;

        @Override
        public void m(@Deprecated int a, @SuppressWarnings("y") String b) {}
    }
    "#};

        let members = current_members(src);

        let f = members.iter().find(|m| m.name().as_ref() == "f").unwrap();
        let f = match f {
            CurrentClassMember::Field(x) => x,
            _ => panic!(),
        };
        assert!(
            f.annotations
                .iter()
                .any(|a| a.internal_name.as_ref() == "Deprecated")
        );

        let m = members.iter().find(|m| m.name().as_ref() == "m").unwrap();
        let m = match m {
            CurrentClassMember::Method(x) => x,
            _ => panic!(),
        };
        assert!(
            m.annotations
                .iter()
                .any(|a| a.internal_name.as_ref() == "Override")
        );

        assert_eq!(m.params.items.len(), 2);
        assert_eq!(m.params.items[0].name.as_ref(), "a");
        assert!(
            m.params.items[0]
                .annotations
                .iter()
                .any(|a| a.internal_name.as_ref() == "Deprecated")
        );
        assert_eq!(m.params.items[1].name.as_ref(), "b");
        assert!(
            m.params.items[1]
                .annotations
                .iter()
                .any(|a| a.internal_name.as_ref().contains("SuppressWarnings"))
        );
    }

    #[test]
    fn test_annotations_method_field_and_param() {
        let src = indoc::indoc! {r#"
    class A {
        @Deprecated
        public int a;

        @Override
        public void m(@Deprecated int x, @SuppressWarnings("y") String y) {}
    }
    "#};
        let members = current_members(src);

        let a = members.iter().find(|m| m.name().as_ref() == "a").unwrap();
        let a = match a {
            CurrentClassMember::Field(f) => f,
            _ => panic!(),
        };
        assert!(
            a.annotations
                .iter()
                .any(|x| x.internal_name.as_ref().contains("Deprecated"))
        );

        let m = members.iter().find(|m| m.name().as_ref() == "m").unwrap();
        let m = match m {
            CurrentClassMember::Method(mm) => mm,
            _ => panic!(),
        };
        assert!(
            m.annotations
                .iter()
                .any(|x| x.internal_name.as_ref().contains("Override"))
        );

        assert_eq!(m.params.items.len(), 2);
        assert_eq!(m.params.items[0].name.as_ref(), "x");
        assert!(
            m.params.items[0]
                .annotations
                .iter()
                .any(|x| x.internal_name.as_ref().contains("Deprecated"))
        );
        assert_eq!(m.params.items[1].name.as_ref(), "y");
        assert!(
            m.params.items[1]
                .annotations
                .iter()
                .any(|x| x.internal_name.as_ref().contains("SuppressWarnings"))
        );
    }

    // #[test]
    // fn test_swallowed_error_node_members_with_annotations() {
    //     let src = indoc::indoc! {r#"
    // class A {
    //     void brokenMethod() {
    //         System.out.println("No closing brace"
    //
    //     @Deprecated
    //     private static void swallowedMethod(@Deprecated int x) {}
    // }
    // "#};
    //     let (ctx, tree) = setup(src);
    //     let type_ctx = SourceTypeCtx::new(None, vec![], None);
    //
    //     let mut members = Vec::new();
    //     collect_members_from_node(&ctx, tree.root_node(), &type_ctx, &mut members);
    //
    //     println!("{members:?}");
    //
    //     let swallowed = members
    //         .iter()
    //         .find(|m| m.name().as_ref() == "swallowedMethod")
    //         .unwrap();
    //     let swallowed = match swallowed {
    //         CurrentClassMember::Method(m) => m,
    //         _ => panic!(),
    //     };
    //
    //     assert!(
    //         swallowed
    //             .annotations
    //             .iter()
    //             .any(|x| x.internal_name.as_ref().contains("Deprecated"))
    //     );
    //     assert_eq!(swallowed.params.items.len(), 1);
    //     assert_eq!(swallowed.params.items[0].name.as_ref(), "x");
    //     assert!(
    //         swallowed.params.items[0]
    //             .annotations
    //             .iter()
    //             .any(|x| x.internal_name.as_ref().contains("Deprecated"))
    //     );
    // }

    #[test]
    fn test_annotation_elements_single_enum() {
        let src = indoc::indoc! {r#"
    import java.lang.annotation.ElementType;
    import java.lang.annotation.Target;
    @Target(ElementType.METHOD)
    public @interface MyAnno {}
    "#};
        let annos = first_class(src).annotations;
        let target = annos
            .iter()
            .find(|a| a.internal_name.as_ref().contains("Target"))
            .unwrap();
        match target.elements.get("value").unwrap() {
            crate::index::AnnotationValue::Enum { const_name, .. } => {
                assert_eq!(const_name.as_ref(), "METHOD");
            }
            other => panic!("expected Enum, got {:?}", other),
        }
    }

    #[test]
    fn test_annotation_elements_array_enum() {
        let src = indoc::indoc! {r#"
    import java.lang.annotation.ElementType;
    import java.lang.annotation.Target;
    @Target({ElementType.METHOD, ElementType.FIELD})
    public @interface MyAnno {}
    "#};
        let annos = first_class(src).annotations;
        let target = annos
            .iter()
            .find(|a| a.internal_name.as_ref().contains("Target"))
            .unwrap();
        match target.elements.get("value").unwrap() {
            crate::index::AnnotationValue::Array(items) => {
                assert_eq!(items.len(), 2);
                let names: Vec<&str> = items
                    .iter()
                    .filter_map(|i| {
                        if let crate::index::AnnotationValue::Enum { const_name, .. } = i {
                            Some(const_name.as_ref())
                        } else {
                            None
                        }
                    })
                    .collect();
                assert!(names.contains(&"METHOD"));
                assert!(names.contains(&"FIELD"));
            }
            other => panic!("expected Array, got {:?}", other),
        }
    }

    #[test]
    fn test_annotation_elements_string() {
        let src = r#"@SuppressWarnings("unchecked") public class A {}"#;
        let annos = first_class(src).annotations;
        let sw = annos
            .iter()
            .find(|a| a.internal_name.as_ref().contains("SuppressWarnings"))
            .unwrap();
        assert!(matches!(
            sw.elements.get("value").unwrap(),
            crate::index::AnnotationValue::String(s) if s.as_ref() == "unchecked"
        ));
    }

    #[test]
    fn test_annotation_targets_via_source() {
        use crate::index::BucketIndex;

        use crate::index::{ClassMetadata, ClassOrigin, IndexView};

        let src = r#"
import java.lang.annotation.*;
@Target({ElementType.METHOD, ElementType.FIELD})
@Retention(RetentionPolicy.RUNTIME)
public @interface MyAnno {}
"#;

        let bucket = Arc::new(BucketIndex::new());
        bucket.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/lang/annotation")),
                name: Arc::from("Target"),
                internal_name: Arc::from("java/lang/annotation/Target"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang/annotation")),
                name: Arc::from("Retention"),
                internal_name: Arc::from("java/lang/annotation/Retention"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang/annotation")),
                name: Arc::from("ElementType"),
                internal_name: Arc::from("java/lang/annotation/ElementType"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang/annotation")),
                name: Arc::from("RetentionPolicy"),
                internal_name: Arc::from("java/lang/annotation/RetentionPolicy"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = IndexView::new(smallvec::smallvec![bucket]);
        let classes = parse_java_source_with_view_via_tree_for_test(
            src,
            ClassOrigin::SourceFile(Arc::from("file:///MyAnno.java")),
            Some(view.build_name_table()),
            Some(&view),
        );
        let meta = classes
            .iter()
            .find(|c| c.name.as_ref() == "MyAnno")
            .unwrap();
        let targets = meta.annotation_targets().unwrap();
        assert!(targets.iter().any(|t| t.as_ref() == "METHOD"));
        assert!(targets.iter().any(|t| t.as_ref() == "FIELD"));
        assert_eq!(meta.annotation_retention(), Some("RUNTIME"));
    }
}
