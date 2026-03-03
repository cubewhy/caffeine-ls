use rust_asm::constants::ACC_PUBLIC;
use std::sync::Arc;
use tree_sitter::{Node, Query};

use crate::index::{ClassMetadata, ClassOrigin};
use crate::language::java::type_ctx::{SourceTypeCtx, build_java_descriptor};
use crate::language::java::utils::extract_generic_signature;
use crate::{
    completion::context::CurrentClassMember,
    index::{GlobalIndex, intern_str},
    language::{
        java::{
            JavaContextExtractor, make_java_parser,
            members::{
                extract_class_members_from_body, extract_javadoc, parse_annotations_in_node,
            },
            scope::extract_package,
            utils::parse_java_modifiers,
        },
        ts_utils::{capture_text, run_query},
    },
};

pub fn parse_java_source(
    source: &str,
    origin: ClassOrigin,
    name_table: Option<Arc<crate::index::NameTable>>,
) -> Vec<ClassMetadata> {
    let ctx = JavaContextExtractor::for_indexing(source);
    let mut parser = make_java_parser();
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let root = tree.root_node();

    let package = extract_package(&ctx, root);
    let imports = crate::language::java::scope::extract_imports(&ctx, root);
    let type_ctx = Arc::new(SourceTypeCtx::new(package.clone(), imports, name_table));
    let mut results = Vec::new();
    collect_java_classes(&ctx, root, &package, None, &origin, &type_ctx, &mut results);

    results
}

fn parse_java_class(
    ctx: &JavaContextExtractor,
    node: Node,
    package: &Option<Arc<str>>,
    outer_class: Option<Arc<str>>,
    origin: &ClassOrigin,
    type_ctx: &SourceTypeCtx,
) -> Option<ClassMetadata> {
    let name_node = node.child_by_field_name("name")?;
    let class_name = ctx.node_text(name_node);
    if class_name.is_empty() {
        return None;
    }

    let name: Arc<str> = Arc::from(class_name);
    let internal_name: Arc<str> = match (package, &outer_class) {
        (Some(pkg), Some(outer)) => Arc::from(format!("{}/{}${}", pkg, outer, class_name).as_str()),
        (Some(pkg), None) => Arc::from(format!("{}/{}", pkg, class_name).as_str()),
        (None, Some(outer)) => Arc::from(format!("{}${}", outer, class_name).as_str()),
        (None, None) => Arc::clone(&name),
    };

    // super class
    let super_name = node
        .child_by_field_name("superclass")
        .and_then(|superclass_node| {
            superclass_node
                .named_children(&mut superclass_node.walk())
                .find(|c| c.kind() == "type_identifier")
                .map(|c| intern_str(&type_ctx.resolve_simple(ctx.node_text(c))))
        });

    // interfaces
    let interfaces: Vec<Arc<str>> = node
        .child_by_field_name("interfaces")
        .map(|iface_node| {
            let q_src = r#"(type_identifier) @t"#;
            if let Ok(q) = Query::new(&tree_sitter_java::LANGUAGE.into(), q_src) {
                let idx = q.capture_index_for_name("t").unwrap();
                run_query(&q, iface_node, ctx.bytes(), None)
                    .into_iter()
                    .filter_map(|caps| {
                        capture_text(&caps, idx, ctx.bytes())
                            .map(|s| intern_str(&type_ctx.resolve_simple(s)))
                    })
                    .collect()
            } else {
                vec![]
            }
        })
        .unwrap_or_default();

    // methods & fields
    let mut methods = Vec::new();
    let mut fields = Vec::new();

    let body = node.child_by_field_name("body");
    let full_source = std::str::from_utf8(ctx.bytes()).unwrap_or("");
    let ctx = JavaContextExtractor::for_indexing(full_source);
    if let Some(b) = body {
        for member in extract_class_members_from_body(&ctx, b, type_ctx) {
            match member {
                CurrentClassMember::Method(m) => methods.push((*m).clone()),
                CurrentClassMember::Field(f) => fields.push((*f).clone()),
            }
        }
    }
    let access_flags = extract_java_access_flags(&ctx, node);

    let mut annos = vec![];
    let mut wc = node.walk();
    for child in node.children(&mut wc) {
        if child.kind() == "modifiers" {
            annos = parse_annotations_in_node(&ctx, child, type_ctx);
            break;
        }
    }

    Some(ClassMetadata {
        package: package.clone(),
        name,
        internal_name,
        super_name,
        interfaces,
        annotations: annos,
        methods,
        fields,
        access_flags,
        inner_class_of: outer_class,
        generic_signature: extract_generic_signature(node, ctx.bytes(), "Ljava/lang/Object;"),
        origin: origin.clone(),
    })
}

/// AST-based 精准符号范围查找 (供 Goto Definition 使用)
pub fn find_symbol_range(
    content: &str,
    target_internal: &str,
    member_name: Option<&str>,
    descriptor: Option<&str>,
    index: &GlobalIndex,
) -> Option<tower_lsp::lsp_types::Range> {
    let ctx = JavaContextExtractor::for_indexing(content);
    let mut parser = make_java_parser();
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let package = extract_package(&ctx, root);
    let imports = crate::language::java::scope::extract_imports(&ctx, root);
    let type_ctx = SourceTypeCtx::new(package, imports, Some(index.build_name_table()));

    let target_simple = target_internal
        .rsplit('/')
        .next()
        .unwrap_or(target_internal);
    let class_node = find_class_node(root, target_simple, ctx.bytes())?;

    let target_node = if let Some(m_name) = member_name {
        let body = class_node.child_by_field_name("body")?;
        find_member_node(&ctx, body, m_name, descriptor, &type_ctx)?
    } else {
        class_node
    };

    // 为了体验更好，跳转时光标应该落在 identifier 上，而不是包含注解的整个方法/类块上
    let focus_node = target_node
        .child_by_field_name("name")
        .unwrap_or(target_node);
    let start = focus_node.start_position();
    let end = focus_node.end_position();

    Some(tower_lsp::lsp_types::Range {
        start: tower_lsp::lsp_types::Position {
            line: start.row as u32,
            character: start.column as u32,
        },
        end: tower_lsp::lsp_types::Position {
            line: end.row as u32,
            character: end.column as u32,
        },
    })
}

fn collect_java_classes(
    ctx: &JavaContextExtractor,
    root_node: Node,
    package: &Option<Arc<str>>,
    initial_outer: Option<Arc<str>>,
    origin: &ClassOrigin,
    type_ctx: &Arc<SourceTypeCtx>,
    out: &mut Vec<ClassMetadata>,
) {
    let mut stack = vec![(root_node, initial_outer)];

    while let Some((node, outer_class)) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "annotation_type_declaration"
                | "record_declaration" => {
                    if let Some(meta) =
                        parse_java_class(ctx, child, package, outer_class.clone(), origin, type_ctx)
                    {
                        let inner_outer = Some(Arc::clone(&meta.name));
                        if let Some(body) = child.child_by_field_name("body") {
                            stack.push((body, inner_outer));
                        }
                        out.push(meta);
                    }
                }
                _ => {
                    // Continue searching downwards
                    stack.push((child, outer_class.clone()));
                }
            }
        }
    }
}

fn extract_java_access_flags(ctx: &JavaContextExtractor, node: Node) -> u16 {
    let mut flags: u16 = ACC_PUBLIC;
    let mut walker = node.walk();
    for child in node.children(&mut walker) {
        if child.kind() == "modifiers" {
            flags = parse_java_modifiers(ctx.node_text(child));
            break;
        }
    }
    flags
}

pub fn discover_java_names(source: &str) -> Vec<Arc<str>> {
    let ctx = JavaContextExtractor::for_indexing(source);
    let mut parser = make_java_parser();
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let root = tree.root_node();
    let package = extract_package(&ctx, root);

    let mut results = Vec::new();
    let mut stack: Vec<(Node, Option<Arc<str>>)> = vec![(root, None)]; // (Node, Option<OuterName>)

    while let Some((node, outer)) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "annotation_type_declaration"
                | "record_declaration" => {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let class_name = ctx.node_text(name_node);
                        if !class_name.is_empty() {
                            // 构建内部名称
                            let internal_name = match (&package, &outer) {
                                (Some(pkg), Some(out)) => format!("{}/{}${}", pkg, out, class_name),
                                (Some(pkg), None) => format!("{}/{}", pkg, class_name),
                                (None, Some(out)) => format!("{}${}", out, class_name),
                                (None, None) => class_name.to_string(),
                            };
                            let internal_name_arc: Arc<str> = Arc::from(internal_name.as_str());
                            results.push(internal_name_arc.clone());

                            // 处理嵌套类：递归入 body
                            if let Some(body) = child.child_by_field_name("body") {
                                // 嵌套类的 outer 名字是 "Outer$Inner" 这种形式中的当前级
                                let next_outer = match outer {
                                    Some(ref o) => format!("{}${}", o, class_name),
                                    None => class_name.to_string(),
                                };
                                stack.push((body, Some(Arc::from(next_outer.as_str()))));
                            }
                        }
                    }
                }
                "package_declaration" | "import_declaration" => continue,
                _ => {
                    // 对于层级较深的情况（如块定义里的类，虽然Java少见但合法）继续向下找
                    stack.push((child, outer.clone()));
                }
            }
        }
    }
    results
}

fn find_class_node<'a>(node: Node<'a>, target_name: &str, bytes: &[u8]) -> Option<Node<'a>> {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if matches!(
            n.kind(),
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ) && let Some(name_node) = n.child_by_field_name("name")
            && name_node.utf8_text(bytes).unwrap_or("") == target_name
        {
            return Some(n);
        }

        let mut cursor = n.walk();
        for child in n.children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn find_member_node<'a>(
    ctx: &JavaContextExtractor,
    body: Node<'a>,
    name: &str,
    descriptor: Option<&str>,
    type_ctx: &SourceTypeCtx,
) -> Option<Node<'a>> {
    let _span =
        tracing::debug_span!("find_member_node", name = %name, target_desc = ?descriptor).entered();

    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        let kind = child.kind();
        if kind == "method_declaration" || kind == "constructor_declaration" {
            let m_name = child
                .child_by_field_name("name")
                .map(|n| ctx.node_text(n))
                .unwrap_or("");
            if m_name == name || (child.kind() == "constructor_declaration" && name == "<init>") {
                tracing::debug!(kind = %kind, method_name = %m_name, "potential name match found");

                if let Some(target_desc) = descriptor {
                    // 重载判定：利用 AST 即时构造当前遍历方法的 Descriptor 进行等值对比
                    let mut ret_type = "void";
                    let mut params_node = None;
                    let mut wc = child.walk();
                    for c in child.children(&mut wc) {
                        match c.kind() {
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
                    let params_text = params_node.map(|n| ctx.node_text(n)).unwrap_or("()");
                    let actual_desc = build_java_descriptor(params_text, ret_type, type_ctx);

                    tracing::debug!(
                        actual = %actual_desc,
                        target = %target_desc,
                        params = %params_text,
                        ret = %ret_type,
                        match_ok = (actual_desc == target_desc),
                        "descriptor comparison"
                    );

                    if actual_desc == target_desc {
                        return Some(child);
                    }
                } else {
                    return Some(child);
                }
            }
        } else if child.kind() == "field_declaration" && descriptor.is_none() {
            let text = child.utf8_text(ctx.bytes()).unwrap_or("");
            if text.contains(name) {
                return Some(child);
            }
        }
    }
    None
}

// TODO: rewrite with an efficient index
#[deprecated]
pub fn get_javadoc_on_the_fly(
    origin: &ClassOrigin,
    target_internal: &str,
    member_name: Option<&str>,
    descriptor: Option<&str>,
) -> Option<String> {
    let content = match origin {
        ClassOrigin::SourceFile(uri) => {
            let path = uri.strip_prefix("file://").unwrap_or(uri);
            std::fs::read_to_string(path).ok()?
        }
        ClassOrigin::ZipSource {
            zip_path,
            entry_name,
        } => {
            let file = std::fs::File::open(zip_path.as_ref()).ok()?;
            let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
            let mut entry = archive.by_name(entry_name.as_ref()).ok()?;
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut entry, &mut buf).ok()?;
            buf
        }
        _ => return None,
    };

    let ctx = JavaContextExtractor::for_indexing(&content);
    let mut parser = make_java_parser();
    let tree = parser.parse(&content, None)?;
    let root = tree.root_node();
    let package = extract_package(&ctx, root);
    let imports = crate::language::java::scope::extract_imports(&ctx, root);
    let type_ctx = SourceTypeCtx::new(package, imports, None);

    let target_simple = target_internal
        .rsplit('/')
        .next()
        .unwrap_or(target_internal);
    let class_node = find_class_node(root, target_simple, ctx.bytes())?;

    let target_node = if let Some(m_name) = member_name {
        let body = class_node.child_by_field_name("body")?;
        find_member_node(&ctx, body, m_name, descriptor, &type_ctx)?
    } else {
        class_node
    };

    extract_javadoc(target_node, ctx.bytes()).map(|s| clean_javadoc(&s))
}

fn clean_javadoc(raw: &str) -> String {
    let mut cleaned = String::new();
    for line in raw.lines() {
        let stripped = line
            .trim()
            .strip_prefix("/**")
            .unwrap_or(line.trim())
            .strip_prefix("*/")
            .unwrap_or_else(|| line.trim().strip_prefix('*').unwrap_or(line.trim()));
        if !stripped.trim().is_empty() || !cleaned.is_empty() {
            cleaned.push_str(stripped.trim());
            cleaned.push('\n');
        }
    }
    cleaned.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use crate::{index::ClassOrigin, language::java::class_parser::parse_java_source};

    #[test]
    fn test_nested_class_internal_name_with_package() {
        let src = r#"
package org.cubewhy.a;
public class Main {
    public static class NestedClass {
        public void randomFunction(String arg1) {}
    }
}
"#;
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let nested = classes
            .iter()
            .find(|c| c.name.as_ref() == "NestedClass")
            .unwrap();
        assert_eq!(
            nested.internal_name.as_ref(),
            "org/cubewhy/a/Main$NestedClass",
            "nested class internal name should use $ separator"
        );
        assert_eq!(nested.inner_class_of.as_deref(), Some("Main"));
    }

    #[test]
    fn test_nested_class_internal_name_without_package() {
        let src = r#"
public class Main {
    public static class NestedClass {
        public void randomFunction() {}
    }
}
"#;
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let nested = classes
            .iter()
            .find(|c| c.name.as_ref() == "NestedClass")
            .unwrap();
        assert_eq!(nested.internal_name.as_ref(), "Main$NestedClass");
    }

    #[test]
    fn test_nested_class_methods_indexed() {
        let src = r#"
package org.cubewhy.a;
public class Main {
    public static class NestedClass {
        public void randomFunction(String arg1) {}
    }
}
"#;
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let nested = classes
            .iter()
            .find(|c| c.name.as_ref() == "NestedClass")
            .unwrap();
        assert!(
            nested
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "randomFunction"),
            "randomFunction should be indexed"
        );
    }

    #[test]
    fn test_super_name_no_extends_keyword() {
        let src = "public class Child extends Parent implements Runnable, Serializable {}";
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let child = classes.iter().find(|c| c.name.as_ref() == "Child").unwrap();
        assert_eq!(
            child.super_name.as_deref(),
            Some("Parent"),
            "super_name should be 'Parent', not 'extends Parent'"
        );
        assert!(
            child.interfaces.contains(&"Runnable".into()),
            "interfaces should contain Runnable"
        );
        assert!(
            child.interfaces.contains(&"Serializable".into()),
            "interfaces should contain Serializable"
        );
    }

    #[test]
    fn test_super_name_strips_extends_keyword() {
        let src = "public class Child extends Parent implements Runnable {}";
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let child = classes.iter().find(|c| c.name.as_ref() == "Child").unwrap();
        assert_eq!(
            child.super_name.as_deref(),
            Some("Parent"),
            "super_name should be 'Parent' not 'extends Parent', got {:?}",
            child.super_name
        );
        assert!(child.interfaces.contains(&"Runnable".into()));
    }

    #[test]
    fn test_extract_java_class_generic_signature() {
        let src = "public class MyMap<K, V> { }";
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let meta = classes.first().unwrap();

        assert_eq!(
            meta.generic_signature.as_deref(),
            Some("<K:Ljava/lang/Object;V:Ljava/lang/Object;>Ljava/lang/Object;")
        );
    }

    #[test]
    fn test_extract_java_method_generic_signature() {
        let src = "public class Utils { public <T> T getFirst(List<T> list) { return null; } }";
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let method = classes.first().unwrap().methods.first().unwrap();

        // 验证方法上的泛型 T 被正确抓取，并且携带了后续的 descriptor
        let sig = method.generic_signature.as_deref().unwrap();
        assert!(sig.starts_with("<T:Ljava/lang/Object;>"));
    }

    #[test]
    fn test_class_level_annotations() {
        let src = r#"
package com.example;

@Deprecated
@SuppressWarnings("all")
public class Foo {
    @Override
    public void bar() {}
}
"#;
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let foo = classes.iter().find(|c| c.name.as_ref() == "Foo").unwrap();

        assert!(
            foo.annotations
                .iter()
                .any(|a| a.internal_name.as_ref().contains("Deprecated")),
            "class should have @Deprecated annotation"
        );
        assert!(
            foo.annotations
                .iter()
                .any(|a| a.internal_name.as_ref().contains("SuppressWarnings")),
            "class should have @SuppressWarnings annotation"
        );

        // 方法级注解不应混入类级别
        assert!(
            !foo.annotations
                .iter()
                .any(|a| a.internal_name.as_ref().contains("Override")),
            "@Override should only be on method, not class"
        );
        let bar = foo
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "bar")
            .unwrap();
        assert!(
            bar.annotations
                .iter()
                .any(|a| a.internal_name.as_ref().contains("Override")),
            "method should have @Override annotation"
        );
    }
}
