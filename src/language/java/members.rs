use rust_asm::constants::{ACC_PRIVATE, ACC_PROTECTED, ACC_PUBLIC, ACC_STATIC};
use std::sync::Arc;
use tree_sitter::{Node, Query};

use crate::{
    index::{AnnotationSummary, FieldSummary, MethodParams, MethodSummary, intern_str},
    language::{
        java::{
            JavaContextExtractor,
            type_ctx::{SourceTypeCtx, build_java_descriptor},
            utils::{extract_generic_signature, parse_java_modifiers},
        },
        ts_utils::{capture_text, run_query},
    },
    semantic::{context::CurrentClassMember, types::parse_return_type_from_descriptor},
};

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

pub fn extract_class_members_from_body(
    ctx: &JavaContextExtractor,
    body: Node,
    type_ctx: &SourceTypeCtx,
) -> Vec<CurrentClassMember> {
    let mut members = Vec::new();
    collect_members_from_node_impl(ctx, body, type_ctx, &mut members, false);

    members
}

pub fn collect_members_from_node(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
    members: &mut Vec<CurrentClassMember>,
) {
    collect_members_from_node_impl(ctx, node, type_ctx, members, true);
}

fn collect_members_from_node_impl(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
    members: &mut Vec<CurrentClassMember>,
    allow_nested_types: bool,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "method_declaration" => {
                if let Some(m) = parse_method_node(ctx, type_ctx, child) {
                    members.push(m);
                }
                if let Some(block) = child.child_by_field_name("body") {
                    let mut bc = block.walk();
                    let block_children: Vec<Node> = block.children(&mut bc).collect();
                    let mut i = 0;
                    while i < block_children.len() {
                        let bc = block_children[i];
                        if bc.kind() == "ERROR" {
                            if let Some(m) = parse_method_node(ctx, type_ctx, bc) {
                                members.push(m);
                            }
                            members.extend(parse_field_node(ctx, type_ctx, bc));
                            collect_members_from_node_impl(
                                ctx,
                                bc,
                                type_ctx,
                                members,
                                allow_nested_types,
                            );
                            let snapshot = members.clone();
                            members.extend(parse_partial_methods_from_error(
                                ctx, type_ctx, bc, &snapshot,
                            ));
                        } else if bc.kind() == "local_variable_declaration" {
                            let next = block_children.get(i + 1);
                            if let Some(next_node) = next
                                && next_node.kind() == "ERROR"
                                && ctx.source[next_node.start_byte()..next_node.end_byte()]
                                    .trim_start()
                                    .starts_with('(')
                                && let Some(m) = parse_misread_method(ctx, type_ctx, bc, *next_node)
                            {
                                members.push(m);
                                i += 1;
                            }
                        } else if bc.kind() == "method_declaration" {
                            if let Some(mut m) = parse_method_node(ctx, type_ctx, bc) {
                                // Collect any annotation siblings that preceded this declaration
                                let pre_annos: Vec<_> = block_children[..i]
                                    .iter()
                                    .rev()
                                    .take_while(|n| {
                                        matches!(n.kind(), "marker_annotation" | "annotation")
                                    })
                                    .flat_map(|n| parse_annotations_in_node(ctx, *n, type_ctx))
                                    .collect();
                                if !pre_annos.is_empty()
                                    && let CurrentClassMember::Method(ref arc) = m
                                {
                                    let mut ms = (**arc).clone();
                                    // prepend so ordering is natural
                                    let mut merged = pre_annos;
                                    merged.append(&mut ms.annotations);
                                    ms.annotations = merged;
                                    m = CurrentClassMember::Method(Arc::new(ms));
                                }
                                members.push(m);
                            }
                        } else if bc.kind() == "field_declaration" {
                            members.extend(parse_field_node(ctx, type_ctx, bc));
                        }
                        i += 1;
                    }
                }
            }
            "field_declaration" => {
                members.extend(parse_field_node(ctx, type_ctx, child));
            }
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "annotation_type_declaration"
            | "record_declaration"
                if allow_nested_types =>
            {
                collect_members_from_node_impl(ctx, child, type_ctx, members, allow_nested_types);
            }
            "class_body" | "interface_body" | "enum_body" | "program" => {
                collect_members_from_node_impl(ctx, child, type_ctx, members, allow_nested_types);
            }
            "ERROR" => {
                collect_members_from_node_impl(ctx, child, type_ctx, members, allow_nested_types);
                let snapshot = members.clone();
                members.extend(parse_partial_methods_from_error(
                    ctx, type_ctx, child, &snapshot,
                ));
            }
            _ => {}
        }
    }

    if node.kind() == "ERROR" {
        let snapshot = members.clone();
        members.extend(parse_partial_methods_from_error(
            ctx, type_ctx, node, &snapshot,
        ));
    }
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
            .find(|n| {
                matches!(
                    n.kind(),
                    "void_type"
                        | "integral_type"
                        | "floating_point_type"
                        | "boolean_type"
                        | "type_identifier"
                        | "array_type"
                        | "generic_type"
                )
            })
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
    let mut name: Option<&str> = None;
    let mut flags = 0;
    let mut ret_type = "void";
    let mut params_node: Option<Node> = None;
    let mut method_annos: Vec<AnnotationSummary> = Vec::new();

    let mut wc = node.walk();
    for c in node.children(&mut wc) {
        match c.kind() {
            "modifiers" => {
                flags = parse_java_modifiers(ctx.node_text(c));
                method_annos = parse_annotations_in_node(ctx, c, type_ctx);
            }
            "identifier" if name.is_none() => name = Some(ctx.node_text(c)),
            "void_type"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
            | "type_identifier"
            | "array_type"
            | "generic_type" => {
                ret_type = ctx.node_text(c);
            }
            "formal_parameters" => params_node = Some(c),
            _ => {}
        }
    }
    if flags == 0 {
        flags = ACC_PUBLIC;
    }

    let name = name.filter(|n| *n != "<init>" && *n != "<clinit>" && !is_java_keyword(n))?;
    let params_text = params_node.map(|n| ctx.node_text(n)).unwrap_or("()");
    let descriptor = build_java_descriptor(params_text, ret_type, type_ctx);

    let generic_signature = extract_generic_signature(node, ctx.bytes(), &descriptor);

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
    let q_src = r#"
        (marker_annotation) @a
        (annotation) @a
    "#;

    let q = match Query::new(&tree_sitter_java::LANGUAGE.into(), q_src) {
        Ok(q) => q,
        Err(_) => return vec![],
    };
    let idx = match q.capture_index_for_name("a") {
        Some(i) => i,
        None => return vec![],
    };

    let mut out = Vec::new();
    for caps in run_query(&q, node, ctx.bytes(), None) {
        let anno_node = match caps.iter().find(|(i, _)| *i == idx) {
            Some((_, n)) => *n,
            None => continue,
        };
        if let Some(s) = parse_single_annotation(ctx, anno_node, type_ctx) {
            out.push(s);
        }
    }
    out
}

fn parse_single_annotation(
    ctx: &JavaContextExtractor,
    node: Node,
    type_ctx: &SourceTypeCtx,
) -> Option<AnnotationSummary> {
    let name_q_src = r#"
        (marker_annotation name: (identifier) @n)
        (marker_annotation name: (scoped_identifier) @n)
        (annotation name: (identifier) @n)
        (annotation name: (scoped_identifier) @n)
    "#;
    let q = Query::new(&tree_sitter_java::LANGUAGE.into(), name_q_src).ok()?;
    let n_idx = q.capture_index_for_name("n")?;

    let name = run_query(&q, node, ctx.bytes(), None)
        .first()
        .and_then(|caps| capture_text(caps, n_idx, ctx.bytes()))?;

    let resolved = type_ctx.resolve_simple(name);
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
                type_name: Arc::from(object),
                const_name: Arc::from(field),
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
        if let Some(n) = child.child_by_field_name("name") {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn setup(source: &str) -> (JavaContextExtractor, tree_sitter::Tree) {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("failed to load java grammar");
        let tree = parser.parse(source, None).unwrap();

        let ctx = JavaContextExtractor::new(source, source.len(), None);
        (ctx, tree)
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
        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);
        let mut members = Vec::new();
        collect_members_from_node(&ctx, tree.root_node(), &type_ctx, &mut members);

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
    fn test_ignore_constructors() {
        let src = indoc::indoc! {r#"
        class A {
            public A() {}
            static { }
            void normalMethod() {}
        }
        "#};
        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);
        let mut members = Vec::new();
        collect_members_from_node(&ctx, tree.root_node(), &type_ctx, &mut members);

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
        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);
        let mut members = Vec::new();
        collect_members_from_node(&ctx, tree.root_node(), &type_ctx, &mut members);

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
        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);
        let mut members = Vec::new();
        collect_members_from_node(&ctx, tree.root_node(), &type_ctx, &mut members);

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
        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);
        let mut members = Vec::new();
        collect_members_from_node(&ctx, tree.root_node(), &type_ctx, &mut members);

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

        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);
        let mut members = Vec::new();
        collect_members_from_node(&ctx, tree.root_node(), &type_ctx, &mut members);

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

        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);
        let mut members = Vec::new();
        collect_members_from_node(&ctx, tree.root_node(), &type_ctx, &mut members);

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
        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);

        let mut members = Vec::new();
        collect_members_from_node(&ctx, tree.root_node(), &type_ctx, &mut members);

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
        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);
        let mut members = Vec::new();
        collect_members_from_node(&ctx, tree.root_node(), &type_ctx, &mut members);

        // 在注解声明节点上直接调 parse_annotations_in_node
        let root = tree.root_node();
        let annos = parse_annotations_in_node(&ctx, root, &type_ctx);
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
        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);
        let annos = parse_annotations_in_node(&ctx, tree.root_node(), &type_ctx);
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
        let (ctx, tree) = setup(src);
        let type_ctx = SourceTypeCtx::new(None, vec![], None);
        let annos = parse_annotations_in_node(&ctx, tree.root_node(), &type_ctx);
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
        use crate::index::NameTable;
        use crate::index::codebase::index_source_text;

        let name_table = NameTable::from_names(vec![
            Arc::from("java/lang/annotation/Target"),
            Arc::from("java/lang/annotation/Retention"),
            Arc::from("java/lang/annotation/ElementType"),
            Arc::from("java/lang/annotation/RetentionPolicy"),
        ]);

        let src = r#"
import java.lang.annotation.*;
@Target({ElementType.METHOD, ElementType.FIELD})
@Retention(RetentionPolicy.RUNTIME)
public @interface MyAnno {}
"#;
        let classes = index_source_text("file:///MyAnno.java", src, "java", Some(name_table));
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
