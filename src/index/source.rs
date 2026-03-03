use rust_asm::constants::{ACC_ABSTRACT, ACC_PRIVATE, ACC_PROTECTED, ACC_PUBLIC};
use std::sync::Arc;
use tracing::debug;
use tree_sitter::{Node, Parser, Query};

use super::{
    ClassMetadata, ClassOrigin, FieldSummary, MethodSummary, parse_return_type_from_descriptor,
};
use crate::{
    index::{MethodParams, intern_str},
    language::{
        java::{
            class_parser::{discover_java_names, parse_java_source},
            type_ctx::split_params,
        },
        ts_utils::{capture_text, run_query},
    },
};

/// Parse the source file string and return all classes defined within it.
pub fn parse_source_str(
    source: &str,
    lang: &str,
    origin: ClassOrigin,
    name_table: Option<Arc<crate::index::NameTable>>,
) -> Vec<ClassMetadata> {
    match lang {
        "java" => parse_java_source(source, origin, name_table),
        "kotlin" => parse_kotlin_source(source, origin),
        other => {
            debug!("unsupported source lang: {}", other);
            vec![]
        }
    }
}

/// Parse from file path (automatically determines language)
pub fn parse_source_file(
    path: &std::path::Path,
    origin: ClassOrigin,
    name_table: Option<Arc<crate::index::NameTable>>,
) -> Vec<ClassMetadata> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            debug!("failed to read {:?}: {}", path, e);
            return vec![];
        }
    };
    let lang = match path.extension().and_then(|e| e.to_str()) {
        Some("java") => "java",
        Some("kt") => "kotlin",
        _ => return vec![],
    };
    parse_source_str(&content, lang, origin, name_table)
}

pub fn discover_internal_names_str(source: &str, lang: &str) -> Vec<Arc<str>> {
    match lang {
        "java" => discover_java_names(source),
        "kotlin" => discover_kotlin_names(source),
        _ => vec![],
    }
}

fn discover_kotlin_names(source: &str) -> Vec<Arc<str>> {
    // Kotlin 实现逻辑类似，鉴于 Kotlin 一个文件可以有多个顶层类，逻辑是一致的
    let mut parser = make_kotlin_parser();
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let bytes = source.as_bytes();
    let package = extract_kotlin_package(tree.root_node(), bytes);

    let mut results = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_declaration" | "object_declaration" | "companion_object" => {
                    if let Some(id_node) = child
                        .children(&mut child.walk())
                        .find(|n| n.kind() == "type_identifier")
                    {
                        let class_name = node_text(id_node, bytes);
                        let internal = match &package {
                            Some(pkg) => format!("{}/{}", pkg, class_name),
                            None => class_name.to_string(),
                        };
                        results.push(Arc::from(internal.as_str()));

                        if let Some(body) = child
                            .named_children(&mut child.walk())
                            .find(|n| n.kind() == "class_body")
                        {
                            stack.push(body);
                        }
                    }
                }
                _ => {
                    stack.push(child);
                }
            }
        }
    }
    results
}

pub fn parse_kotlin_source(source: &str, origin: ClassOrigin) -> Vec<ClassMetadata> {
    let mut parser = make_kotlin_parser();
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let root = tree.root_node();
    let bytes = source.as_bytes();

    let package = extract_kotlin_package(root, bytes);
    let mut results = Vec::new();
    collect_kotlin_classes(root, bytes, &package, None, &origin, &mut results);
    results
}

fn collect_kotlin_classes(
    root_node: Node,
    bytes: &[u8],
    package: &Option<Arc<str>>,
    initial_outer: Option<Arc<str>>,
    origin: &ClassOrigin,
    out: &mut Vec<ClassMetadata>,
) {
    let mut stack = vec![(root_node, initial_outer)];

    while let Some((node, outer_class)) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_declaration" | "object_declaration" | "companion_object" => {
                    if let Some(meta) =
                        parse_kotlin_class(child, bytes, package, outer_class.clone(), origin)
                    {
                        let inner_outer = Some(Arc::clone(&meta.name));
                        if let Some(body) = child
                            .named_children(&mut child.walk())
                            .find(|n| n.kind() == "class_body")
                        {
                            stack.push((body, inner_outer));
                        }
                        out.push(meta);
                    }
                }
                _ => {
                    stack.push((child, outer_class.clone()));
                }
            }
        }
    }
}

fn parse_kotlin_class(
    node: Node,
    bytes: &[u8],
    package: &Option<Arc<str>>,
    outer_class: Option<Arc<str>>,
    origin: &ClassOrigin,
) -> Option<ClassMetadata> {
    // The class name for class_declaration / object_declaration is type_identifier.
    let name_node = node
        .children(&mut node.walk())
        .find(|n| n.kind() == "type_identifier")?;

    let class_name = node_text(name_node, bytes);
    if class_name.is_empty() {
        return None;
    }

    let name: Arc<str> = Arc::from(class_name);
    let internal_name: Arc<str> = match package {
        Some(pkg) => Arc::from(format!("{}/{}", pkg, class_name).as_str()),
        None => Arc::clone(&name),
    };

    // Parent class: the first type_identifier in delegation_specifiers
    let super_name = node
        .named_children(&mut node.walk())
        .find(|n| n.kind() == "delegation_specifiers")
        .and_then(|ds| {
            ds.named_children(&mut ds.walk())
                .find(|n| n.kind() == "constructor_invocation" || n.kind() == "user_type")
                .map(|n| intern_str(node_text(n, bytes)))
        });

    // Interface: entries of type user_type in delegation_specifiers (not the first constructor_invocation)
    let interfaces: Vec<Arc<str>> = node
        .named_children(&mut node.walk())
        .find(|n| n.kind() == "delegation_specifiers")
        .map(|ds| {
            ds.named_children(&mut ds.walk())
                .filter(|n| n.kind() == "user_type")
                .map(|n| intern_str(node_text(n, bytes)))
                .collect()
        })
        .unwrap_or_default();

    let body = node
        .named_children(&mut node.walk())
        .find(|n| n.kind() == "class_body");

    let (methods, fields) = body
        .map(|b| {
            let methods = extract_kotlin_methods(b, bytes);
            let fields = extract_kotlin_fields(b, bytes);
            (methods, fields)
        })
        .unwrap_or_default();

    let access_flags = extract_kotlin_access_flags(node, bytes);

    Some(ClassMetadata {
        package: package.clone(),
        name,
        internal_name,
        super_name,
        interfaces,
        annotations: vec![],
        methods,
        fields,
        access_flags,
        generic_signature: extract_generic_signature(node, bytes, "Ljava/lang/Object;"),
        inner_class_of: outer_class,
        origin: origin.clone(),
    })
}

fn extract_kotlin_methods(body: Node, bytes: &[u8]) -> Vec<MethodSummary> {
    // AST: function_declaration
    //   simple_identifier @name
    //   function_value_parameters @params
    //   (type_reference / user_type)? @return_type
    let q_src = r#"
        (function_declaration
            (simple_identifier) @name
            (function_value_parameters) @params)
    "#;
    let q = match Query::new(&tree_sitter_kotlin::LANGUAGE.into(), q_src) {
        Ok(q) => q,
        Err(_) => return vec![],
    };
    let name_idx = q.capture_index_for_name("name").unwrap();
    let params_idx = q.capture_index_for_name("params").unwrap();

    let mut result = Vec::new();

    for caps in run_query(&q, body, bytes, None) {
        let name_node = match caps.iter().find(|(idx, _)| *idx == name_idx) {
            Some(n) => n.1,
            None => continue,
        };
        let name = node_text(name_node, bytes);
        let params_text = capture_text(&caps, params_idx, bytes).unwrap_or("()");

        // Get the corresponding node from captures, then find its type_reference
        // Use the node position to find the function declaration node and then get the return type
        let descriptor = build_kotlin_descriptor(params_text);
        let return_type = parse_return_type_from_descriptor(&descriptor);

        let func_node = name_node.parent().unwrap();
        let generic_signature = extract_generic_signature(func_node, bytes, &descriptor);
        let param_names: Vec<Arc<str>> = {
            let params_node = caps
                .iter()
                .find(|(idx, _)| *idx == params_idx)
                .map(|(_, n)| *n);
            if let Some(pn) = params_node {
                pn.named_children(&mut pn.walk())
                    .filter(|n| n.kind() == "parameter" || n.kind() == "function_value_parameter")
                    .filter_map(|n| {
                        n.children(&mut n.walk())
                            .find(|c| c.kind() == "simple_identifier")
                            .map(|c| Arc::from(node_text(c, bytes)))
                    })
                    .collect()
            } else {
                vec![]
            }
        };

        let params = MethodParams::from_descriptor_and_names(&descriptor, &param_names);

        result.push(MethodSummary {
            name: Arc::from(name),
            descriptor: Arc::from(descriptor.as_str()),
            params, // ✅ replace param_names
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature,
            return_type,
        });
    }
    result
}

fn extract_kotlin_fields(body: Node, bytes: &[u8]) -> Vec<FieldSummary> {
    // property_declaration
    //   (binding_pattern_kind val/var)
    //   variable_declaration
    //     simple_identifier @name
    //     user_type > type_identifier @type
    let q_src = r#"
        (property_declaration
            (variable_declaration
                (simple_identifier) @name
                (user_type (type_identifier) @type)))
    "#;
    let q = match Query::new(&tree_sitter_kotlin::LANGUAGE.into(), q_src) {
        Ok(q) => q,
        Err(_) => return vec![],
    };
    let name_idx = q.capture_index_for_name("name").unwrap();
    let type_idx = q.capture_index_for_name("type").unwrap();

    run_query(&q, body, bytes, None)
        .into_iter()
        .filter_map(|caps| {
            let name = capture_text(&caps, name_idx, bytes)?;
            let ty = capture_text(&caps, type_idx, bytes)?;
            let descriptor = kotlin_type_to_descriptor(ty);
            Some(FieldSummary {
                name: Arc::from(name),
                descriptor: Arc::from(descriptor.as_str()),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
            })
        })
        .collect()
}

fn extract_kotlin_package(root: Node, bytes: &[u8]) -> Option<Arc<str>> {
    // package_header > identifier
    let q_src = r#"(package_header (identifier) @pkg)"#;
    let q = Query::new(&tree_sitter_kotlin::LANGUAGE.into(), q_src).ok()?;
    let idx = q.capture_index_for_name("pkg")?;
    let results = run_query(&q, root, bytes, None);
    let pkg = results
        .first()
        .and_then(|caps| capture_text(caps, idx, bytes))?;
    Some(Arc::from(pkg.replace('.', "/").as_str()))
}

fn extract_kotlin_access_flags(node: Node, bytes: &[u8]) -> u16 {
    let mut flags: u16 = 0;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let text = node_text(child, bytes);
            if text.contains("public") {
                flags |= ACC_PUBLIC;
            }
            if text.contains("private") {
                flags |= ACC_PRIVATE;
            }
            if text.contains("protected") {
                flags |= ACC_PROTECTED;
            }
            if text.contains("open") { /* Kotlin open = not final */ }
            if text.contains("abstract") {
                flags |= ACC_ABSTRACT;
            }
            if text.contains("data") { /* data classes */ }
        }
    }
    // Kotlin 默认 public
    if flags & (ACC_PUBLIC | ACC_PRIVATE | ACC_PROTECTED) == 0 {
        flags |= ACC_PUBLIC;
    }
    flags
}

/// Kotlin parameter list text → JVM descriptor (approximately, use V when there is no return type information)
fn build_kotlin_descriptor(params_text: &str) -> String {
    let inner = params_text
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');

    let mut desc = String::from("(");

    if !inner.trim().is_empty() {
        for param in split_params(inner) {
            let param = param.trim();
            let ty = param
                .split_once(':')
                .map(|x| x.1)
                .unwrap_or("")
                .trim()
                // Remove generics
                .split('<')
                .next()
                .unwrap_or("")
                .trim()
                // 去掉可空标记
                .trim_end_matches('?');
            desc.push_str(&kotlin_type_to_descriptor(ty));
        }
    }

    desc.push_str(")V"); // TODO: 返回类型暂用 V，实际需要 tree-sitter 进一步解析
    desc
}

/// Kotlin type name -> JVM descriptor
fn kotlin_type_to_descriptor(ty: &str) -> String {
    let ty = ty.trim().trim_end_matches('?');
    let base = ty.split('<').next().unwrap_or(ty).trim();
    match base {
        "Unit" => "V".into(),
        "Boolean" => "Z".into(),
        "Byte" => "B".into(),
        "Char" => "C".into(),
        "Short" => "S".into(),
        "Int" => "I".into(),
        "Long" => "J".into(),
        "Float" => "F".into(),
        "Double" => "D".into(),
        "String" => "Ljava/lang/String;".into(),
        "Any" => "Ljava/lang/Object;".into(),
        "List" | "MutableList" => "Ljava/util/List;".into(),
        "Map" | "MutableMap" => "Ljava/util/Map;".into(),
        "Set" | "MutableSet" => "Ljava/util/Set;".into(),
        "Array" => "[Ljava/lang/Object;".into(),
        other => format!("L{};", other.replace('.', "/")),
    }
}

fn node_text<'a>(node: Node, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

fn make_kotlin_parser() -> Parser {
    let mut p = Parser::new();
    p.set_language(&tree_sitter_kotlin::LANGUAGE.into())
        .expect("kotlin grammar");
    p
}

/// 提取类或方法的泛型参数，并构建成 JVM 规范的泛型签名。
/// 例如从 `class List<T, E>` 提取出 `<T:Ljava/lang/Object;E:Ljava/lang/Object;>Ljava/lang/Object;`
fn extract_generic_signature(node: Node, bytes: &[u8], suffix: &str) -> Option<Arc<str>> {
    // 兼容 Java (child_by_field_name) 和 Kotlin (直接找 kind)
    let tp_node = node.child_by_field_name("type_parameters").or_else(|| {
        node.children(&mut node.walk())
            .find(|n| n.kind() == "type_parameters")
    })?;

    let mut sig = String::from("<");
    let mut has_params = false;
    let mut walker = tp_node.walk();

    for child in tp_node.named_children(&mut walker) {
        if child.kind() == "type_parameter" {
            // Java 是 identifier，Kotlin 是 type_identifier
            if let Some(id_node) = child
                .children(&mut child.walk())
                .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
            {
                let name = node_text(id_node, bytes).trim();
                if !name.is_empty() {
                    sig.push_str(name);
                    // 统一擦除到 Object，因为我们的引擎目前只关心参数占位符的名称映射
                    sig.push_str(":Ljava/lang/Object;");
                    has_params = true;
                }
            }
        }
    }

    if !has_params {
        return None;
    }

    sig.push('>');
    sig.push_str(suffix);
    Some(Arc::from(sig))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::index::{ClassOrigin, source::parse_kotlin_source};

    #[test]
    fn test_extract_kotlin_class_generic_signature() {
        let src = "class Box<T>(val item: T) { }";
        let classes = parse_kotlin_source(src, ClassOrigin::Unknown);
        let meta = classes.first().unwrap();

        assert_eq!(
            meta.generic_signature.as_deref(),
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;")
        );
    }

    #[test]
    fn test_kotlin_param_names() {
        let src = "class Foo { fun greet(name: String, times: Int) {} }";
        let classes = parse_kotlin_source(src, ClassOrigin::Unknown);
        let method = classes[0]
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "greet")
            .unwrap();
        assert_eq!(
            method.params.param_names().as_slice(),
            &[Arc::from("name"), Arc::from("times")]
        );
    }
}
