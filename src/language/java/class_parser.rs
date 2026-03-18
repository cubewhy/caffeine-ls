use rust_asm::constants::{ACC_ANNOTATION, ACC_ENUM, ACC_INTERFACE, ACC_PUBLIC, ACC_SUPER};
use std::sync::Arc;
use tree_sitter::{Node, Query};
use tree_sitter_utils::{
    Handler, Input, NodePredicate,
    constructors::Always,
    dispatch_on_kind, kind_is,
    traversal::{any_child_of_kind, first_child_of_kind},
};

use crate::index::{ClassMetadata, ClassOrigin};
use crate::jvm::descriptor::consume_one_descriptor_type;
use crate::language::java::type_ctx::{SourceTypeCtx, build_java_descriptor};
use crate::language::java::utils::extract_generic_signature;
use crate::{
    index::{IndexView, intern_str},
    language::{
        java::{
            JavaContextExtractor, make_java_parser,
            members::{
                extract_class_members_from_body, extract_javadoc, parse_annotations_in_node,
            },
            scope::extract_package,
            synthetic::{self, SyntheticDefinitionKind},
            utils::parse_java_modifiers,
        },
        ts_utils::{capture_text, run_query},
    },
    semantic::{context::CurrentClassMember, types::generics::parse_class_type_parameters},
};

pub fn parse_java_source(
    source: &str,
    origin: ClassOrigin,
    name_table: Option<Arc<crate::index::NameTable>>,
) -> Vec<ClassMetadata> {
    let ctx = JavaContextExtractor::for_indexing(source, name_table.clone());
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
    collect_java_classes(
        &ctx,
        root,
        &package,
        None,
        None,
        &origin,
        &type_ctx,
        &mut results,
    );

    // Generate synthetic nested classes (e.g., Builder classes from @Builder)
    let synthetic_nested = generate_synthetic_nested_classes(&results, &type_ctx, &origin);
    results.extend(synthetic_nested);

    // Generate synthetic nested classes (e.g., Builder classes from @Builder)
    let synthetic_nested = generate_synthetic_nested_classes(&results, &type_ctx, &origin);
    results.extend(synthetic_nested);

    results
}

fn parse_java_class(
    ctx: &JavaContextExtractor,
    node: Node,
    package: &Option<Arc<str>>,
    outer_internal: Option<Arc<str>>,
    outer_simple: Option<Arc<str>>,
    origin: &ClassOrigin,
    type_ctx: &SourceTypeCtx,
) -> Option<ClassMetadata> {
    let name_node = node.child_by_field_name("name")?;
    let class_name = ctx.node_text(name_node);
    if class_name.is_empty() {
        return None;
    }

    let name: Arc<str> = Arc::from(class_name);
    let internal_name: Arc<str> = match (package, &outer_internal) {
        (Some(_pkg), Some(outer)) => Arc::from(format!("{}${}", outer, class_name).as_str()),
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
    let ctx = JavaContextExtractor::for_indexing(full_source, ctx.name_table.clone());
    if let Some(b) = body {
        for member in extract_class_members_from_body(&ctx, b, type_ctx) {
            match member {
                CurrentClassMember::Method(m) => methods.push((*m).clone()),
                CurrentClassMember::Field(f) => fields.push((*f).clone()),
            }
        }
    }
    let synthetic = synthetic::synthesize_for_type(
        &ctx,
        node,
        Some(internal_name.as_ref()),
        type_ctx,
        &methods,
        &fields,
    );
    methods.extend(synthetic.methods);
    fields.extend(synthetic.fields);
    let access_flags = extract_java_access_flags(&ctx, node);

    // Use traversal utility to find modifiers
    let annos = first_child_of_kind(node, "modifiers")
        .map(|modifiers| parse_annotations_in_node(&ctx, modifiers, type_ctx))
        .unwrap_or_default();

    let class_generic_signature =
        extract_generic_signature(node, ctx.bytes(), "Ljava/lang/Object;");
    if let Some(sig) = class_generic_signature.as_deref() {
        let class_type_params = parse_class_type_parameters(sig);
        if !class_type_params.is_empty() {
            for method in &mut methods {
                if method.generic_signature.is_none() {
                    let desc = method.desc();
                    if let Some(synth) =
                        synthesize_class_typevar_method_signature(&desc, &class_type_params)
                    {
                        method.generic_signature = Some(synth);
                    }
                }
            }
        }
    }

    if methods.iter().any(|m| m.name.as_ref() == "add") {
        tracing::debug!(
            class_name = %name,
            internal_name = %internal_name,
            origin = ?origin,
            class_generic_signature = ?class_generic_signature,
            add_overloads = ?methods
                .iter()
                .filter(|m| m.name.as_ref() == "add")
                .map(|m| format!(
                    "desc={} gs={:?} ret={:?} params={:?} names={:?}",
                    m.desc(),
                    m.generic_signature,
                    m.return_type,
                    m.params.items.iter().map(|p| p.descriptor.as_ref()).collect::<Vec<_>>(),
                    m.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                ))
                .collect::<Vec<_>>(),
            "class_parser::parse_java_class: class metadata extracted"
        );
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
        inner_class_of: outer_simple,
        generic_signature: class_generic_signature,
        origin: origin.clone(),
    })
}

fn synthesize_class_typevar_method_signature(
    method_desc: &str,
    class_type_params: &[String],
) -> Option<Arc<str>> {
    let (l, r) = method_desc.find('(').zip(method_desc.find(')'))?;
    let params = &method_desc[l + 1..r];
    let ret = &method_desc[r + 1..];

    let mut changed = false;
    let params_out = map_desc_part_type_vars(params, class_type_params, &mut changed);
    let ret_out = map_desc_part_type_vars(ret, class_type_params, &mut changed);
    if !changed {
        return None;
    }
    Some(Arc::from(format!("({}){}", params_out, ret_out).as_str()))
}

fn map_desc_part_type_vars(s: &str, class_type_params: &[String], changed: &mut bool) -> String {
    let mut out = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        let (one, next) = consume_one_descriptor_type(rest);
        if one.is_empty() {
            out.push_str(rest);
            break;
        }
        out.push_str(&map_single_type_var_descriptor(
            one,
            class_type_params,
            changed,
        ));
        rest = next;
    }
    out
}

fn map_single_type_var_descriptor(
    desc: &str,
    class_type_params: &[String],
    changed: &mut bool,
) -> String {
    let mut dims = 0usize;
    let mut base = desc;
    while let Some(rest) = base.strip_prefix('[') {
        dims += 1;
        base = rest;
    }

    let mapped_base = if base.starts_with('L') && base.ends_with(';') {
        let inner = &base[1..base.len() - 1];
        if class_type_params.iter().any(|p| p == inner) {
            *changed = true;
            format!("T{};", inner)
        } else {
            base.to_string()
        }
    } else {
        base.to_string()
    };

    format!("{}{}", "[".repeat(dims), mapped_base)
}

/// AST-based precise symbol range lookup
pub fn find_symbol_range(
    content: &str,
    target_internal: &str,
    member_name: Option<&str>,
    descriptor: Option<&str>,
    index: &IndexView,
) -> Option<tower_lsp::lsp_types::Range> {
    let ctx = JavaContextExtractor::for_indexing(content, Some(index.build_name_table()));
    let mut parser = make_java_parser();
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let package = extract_package(&ctx, root);
    let imports = crate::language::java::scope::extract_imports(&ctx, root);
    let type_ctx = SourceTypeCtx::new(package, imports, Some(index.build_name_table()));

    // For nested classes, we need to look up the class metadata to get the simple name
    // We can't just split by '$' because '$' is a valid character in Java identifiers
    let target_simple_owned = if let Some(meta) = index.get_class(target_internal) {
        // Use the name field from ClassMetadata which is the simple name
        Some(meta.name.to_string())
    } else {
        None
    };

    let target_simple = target_simple_owned.as_deref().unwrap_or_else(|| {
        // Fallback: extract from internal name (after last '/')
        target_internal
            .rsplit('/')
            .next()
            .unwrap_or(target_internal)
    });

    let class_node = find_class_node(root, target_simple, ctx.bytes())?;

    let target_node = if let Some(m_name) = member_name {
        let body = class_node.child_by_field_name("body")?;
        find_member_node(&ctx, body, m_name, descriptor, &type_ctx).or_else(|| {
            synthetic::resolve_synthetic_definition(
                &ctx,
                class_node,
                &type_ctx,
                Some(target_internal),
                if descriptor.is_some() {
                    SyntheticDefinitionKind::Method
                } else {
                    SyntheticDefinitionKind::Field
                },
                m_name,
                descriptor,
            )
        })?
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

/// Node kinds that represent a Java type declaration.
const CLASS_DECL_KINDS: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "annotation_type_declaration",
    "record_declaration",
];

fn collect_java_classes(
    ctx: &JavaContextExtractor,
    root_node: Node,
    package: &Option<Arc<str>>,
    initial_outer_internal: Option<Arc<str>>,
    initial_outer_simple: Option<Arc<str>>,
    origin: &ClassOrigin,
    type_ctx: &Arc<SourceTypeCtx>,
    out: &mut Vec<ClassMetadata>,
) {
    let is_class_decl = kind_is(CLASS_DECL_KINDS);
    let mut stack = vec![(root_node, initial_outer_internal, initial_outer_simple)];

    while let Some((node, outer_internal, outer_simple)) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if is_class_decl.test(Input::new(child, (), None)) {
                if let Some(meta) = parse_java_class(
                    ctx,
                    child,
                    package,
                    outer_internal.clone(),
                    outer_simple.clone(),
                    origin,
                    type_ctx,
                ) {
                    let inner_outer_internal = Some(Arc::clone(&meta.internal_name));
                    let inner_outer_simple = Some(Arc::clone(&meta.name));
                    if let Some(body) = child.child_by_field_name("body") {
                        stack.push((body, inner_outer_internal, inner_outer_simple));
                    }
                    out.push(meta);
                }
            } else {
                // Continue searching downwards
                stack.push((child, outer_internal.clone(), outer_simple.clone()));
            }
        }
    }
}

fn extract_java_access_flags(ctx: &JavaContextExtractor, node: Node) -> u16 {
    let mut flags = any_child_of_kind(node, "modifiers")
        .map(|m| parse_java_modifiers(ctx.node_text(m)))
        .unwrap_or(ACC_PUBLIC);

    // Map declaration kind to the extra JVM access flags it always carries.
    // `dispatch_on_kind` returns `None` for unrecognised kinds, which maps
    // cleanly to the existing "no extra flags" default.
    static KIND_FLAGS: &[(&str, &dyn Handler<(), u16>)] = &[
        ("class_declaration", &Always::new_const(ACC_SUPER)),
        ("record_declaration", &Always::new_const(ACC_SUPER)),
        ("enum_declaration", &Always::new_const(ACC_ENUM | ACC_SUPER)),
        ("interface_declaration", &Always::new_const(ACC_INTERFACE)),
        (
            "annotation_type_declaration",
            &Always::new_const(ACC_INTERFACE | ACC_ANNOTATION),
        ),
    ];
    if let Some(extra) = dispatch_on_kind(KIND_FLAGS).handle(Input::new(node, (), None)) {
        flags |= extra;
    }

    flags
}

pub fn discover_java_names(source: &str) -> Vec<Arc<str>> {
    let ctx = JavaContextExtractor::for_indexing(source, None);
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
            if matches!(child.kind(), "package_declaration" | "import_declaration") {
                continue;
            }
            if kind_is(CLASS_DECL_KINDS).test(Input::new(child, (), None)) {
                if let Some(name_node) = first_child_of_kind(child, "identifier") {
                    let class_name = ctx.node_text(name_node);
                    if !class_name.is_empty() {
                        let internal_name = match (&package, &outer) {
                            (Some(pkg), Some(out)) => format!("{}/{}${}", pkg, out, class_name),
                            (Some(pkg), None) => format!("{}/{}", pkg, class_name),
                            (None, Some(out)) => format!("{}${}", out, class_name),
                            (None, None) => class_name.to_string(),
                        };
                        let internal_name_arc: Arc<str> = Arc::from(internal_name.as_str());
                        results.push(internal_name_arc.clone());
                        if let Some(body) = child.child_by_field_name("body") {
                            let next_outer = match outer {
                                Some(ref o) => format!("{}${}", o, class_name),
                                None => class_name.to_string(),
                            };
                            stack.push((body, Some(Arc::from(next_outer.as_str()))));
                        }
                    }
                }
            } else {
                // Descend into non-declaration nodes (anonymous classes, blocks, etc.)
                stack.push((child, outer.clone()));
            }
        }
    }
    results
}

fn find_class_node<'a>(node: Node<'a>, target_name: &str, _bytes: &[u8]) -> Option<Node<'a>> {
    let is_class_decl = kind_is(CLASS_DECL_KINDS);
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if is_class_decl.test(Input::new(n, (), None))
            && first_child_of_kind(n, "identifier")
                .is_some_and(|name_node| name_node.utf8_text(_bytes).unwrap_or("") == target_name)
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
        if kind == "method_declaration"
            || kind == "constructor_declaration"
            || kind == "compact_constructor_declaration"
        {
            let m_name = child
                .child_by_field_name("name")
                .map(|n| ctx.node_text(n))
                .unwrap_or("");
            let is_constructor_match = (child.kind() == "constructor_declaration"
                || child.kind() == "compact_constructor_declaration")
                && name == "<init>";
            if m_name == name || is_constructor_match {
                tracing::debug!(kind = %kind, method_name = %m_name, "potential name match found");

                if let Some(target_desc) = descriptor {
                    // 重载判定：利用 AST 即时构造当前遍历方法的 Descriptor 进行等值对比
                    let mut ret_type = "void";
                    let mut params_node = None;

                    // For compact constructors, get parameters from parent record_declaration
                    if child.kind() == "compact_constructor_declaration" {
                        let mut parent = child.parent();
                        while let Some(p) = parent {
                            if p.kind() == "record_declaration" {
                                params_node = p.child_by_field_name("parameters").or_else(|| {
                                    let mut cursor = p.walk();
                                    p.children(&mut cursor)
                                        .find(|c| c.kind() == "formal_parameters")
                                });
                                break;
                            }
                            parent = p.parent();
                        }
                    } else {
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
        } else if matches!(child.kind(), "enum_body_declarations" | "ERROR")
            && let Some(found) = find_member_node(ctx, child, name, descriptor, type_ctx)
        {
            return Some(found);
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

    let ctx = JavaContextExtractor::for_indexing(&content, None);
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
    use crate::{
        index::{ClassOrigin, IndexScope, ModuleId, WorkspaceIndex},
        language::java::{
            JavaContextExtractor,
            class_parser::parse_java_source,
            make_java_parser, render,
            scope::{extract_imports, extract_package},
            type_ctx::SourceTypeCtx,
        },
        semantic::context::CurrentClassMember,
        semantic::types::{
            SymbolProvider, descriptor_to_source_type,
            generics::{JvmType, substitute_type},
            signature_to_source_type,
        },
    };
    use std::sync::Arc;
    use tracing_subscriber::{EnvFilter, fmt};
    use tree_sitter::Query;

    fn init_test_tracing() {
        let _ = fmt()
            .with_test_writer()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug")),
            )
            .try_init();
    }

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
    fn test_double_nested_class_internal_name_uses_full_owner_chain() {
        let src = r#"
package org.cubewhy.a;
public class Main {
    public static class Nested {
        public static class Leaf {}
    }
}
"#;
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let leaf = classes.iter().find(|c| c.name.as_ref() == "Leaf").unwrap();
        assert_eq!(
            leaf.internal_name.as_ref(),
            "org/cubewhy/a/Main$Nested$Leaf"
        );
        assert_eq!(leaf.inner_class_of.as_deref(), Some("Nested"));
    }

    #[test]
    fn test_outer_methods_exclude_nested_type_members() {
        let src = indoc::indoc! {r#"
            package org.cubewhy.a;
            public class Outer<E> {
                public boolean add(E e) { return true; }

                static class Nested<E> {
                    public void add(E e) {}
                }
            }
        "#};

        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let outer = classes.iter().find(|c| c.name.as_ref() == "Outer").unwrap();
        let nested = classes
            .iter()
            .find(|c| c.name.as_ref() == "Nested")
            .unwrap();

        let outer_add_descs: Vec<_> = outer
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "add")
            .map(|m| m.desc().to_string())
            .collect();
        assert_eq!(
            outer_add_descs,
            vec!["(LE;)Z"],
            "outer class should not contain nested add(E) -> void"
        );

        let nested_add_descs: Vec<_> = nested
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "add")
            .map(|m| m.desc().to_string())
            .collect();
        assert_eq!(
            nested_add_descs,
            vec!["(LE;)V"],
            "nested class should still index its own add(E) -> void"
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
    fn test_source_method_signature_preserves_functional_param_and_generic_return_shape() {
        let src = indoc::indoc! {r#"
            package org.example;
            import java.util.function.Function;
            public class Demo<T> {
                public <R> Demo<R> map(Function<? super T, ? extends R> fn) { return null; }
            }
        "#};
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let demo = classes
            .iter()
            .find(|c| c.internal_name.as_ref() == "org/example/Demo")
            .unwrap();
        let map = demo
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "map")
            .unwrap();
        let sig = map.generic_signature.as_deref().unwrap_or("");

        assert!(
            sig.starts_with("<R:Ljava/lang/Object;>"),
            "method type param should be preserved, sig={sig}"
        );
        assert!(
            sig.contains("Ljava/util/function/Function<-TT;+TR;>;")
                || sig.contains("LFunction<-TT;+TR;>;"),
            "functional parameter generic shape should be preserved, sig={sig}"
        );
        assert!(
            sig.ends_with("LDemo<TR;>;") || sig.ends_with("Lorg/example/Demo<TR;>;"),
            "generic return shape should preserve <TR;>, sig={sig}"
        );
    }

    #[test]
    fn test_trace_source_add_overloads_metadata() {
        init_test_tracing();

        let src = indoc::indoc! {r#"
            public class MyList<E> {
                public boolean add(E e) { return true; }
                public void add(int index, E element) {}
            }
        "#};
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let cls = classes
            .iter()
            .find(|c| c.name.as_ref() == "MyList")
            .unwrap();
        let adds: Vec<_> = cls
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "add")
            .collect();
        assert_eq!(adds.len(), 2);
        let add_bool = adds.iter().find(|m| m.desc().as_ref() == "(LE;)Z").unwrap();
        let add_void = adds
            .iter()
            .find(|m| m.desc().as_ref() == "(ILE;)V")
            .unwrap();
        assert_eq!(add_bool.generic_signature.as_deref(), Some("(TE;)Z"));
        assert_eq!(add_void.generic_signature.as_deref(), Some("(ITE;)V"));
    }

    struct TestProvider;
    impl SymbolProvider for TestProvider {
        fn resolve_source_name(&self, internal_name: &str) -> Option<String> {
            Some(internal_name.replace('/', "."))
        }
    }

    #[test]
    fn test_source_generic_method_detail_concretizes_receiver_type_args() {
        let src = indoc::indoc! {r#"
            package org.example;
            public class MyList<E> {
                public boolean add(E e) { return true; }
            }
        "#};
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let cls = classes
            .iter()
            .find(|c| c.internal_name.as_ref() == "org/example/MyList")
            .unwrap();
        let add = cls
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "add")
            .unwrap();

        let detail = render::method_detail(
            "org/example/MyList<Ljava/lang/String;>",
            cls,
            add,
            &TestProvider,
        );
        assert!(detail.contains("java.lang.String e"), "detail={}", detail);
        assert!(!detail.contains("TE;"), "detail={}", detail);
        assert!(!detail.contains("LE;"), "detail={}", detail);
    }

    #[test]
    fn test_snapshot_method_generic_metadata_provenance_map() {
        let src = indoc::indoc! {r#"
            package org.example;
            import java.util.function.Function;
            public class Demo<T> {
                public <R> Demo<R> map(Function<? super T, ? extends R> fn) { return null; }
            }
        "#};
        let expected_ideal = "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)Lorg/example/Demo<TR;>;";

        let mut parser = make_java_parser();
        let tree = parser.parse(src, None).expect("parse");
        let root = tree.root_node();
        let ctx = JavaContextExtractor::for_indexing(src, None);
        let package = extract_package(&ctx, root);
        let imports = extract_imports(&ctx, root);
        let type_ctx = SourceTypeCtx::new(package, imports, None);

        let q = Query::new(
            &tree_sitter_java::LANGUAGE.into(),
            "(method_declaration name: (identifier) @name) @m",
        )
        .unwrap();
        let m_idx = q.capture_index_for_name("m").unwrap();
        let n_idx = q.capture_index_for_name("name").unwrap();
        let mut extracted_method = None;
        for caps in crate::language::ts_utils::run_query(&q, root, ctx.bytes(), None) {
            let m = caps.iter().find(|(i, _)| *i == m_idx).map(|(_, n)| *n);
            let n = caps.iter().find(|(i, _)| *i == n_idx).map(|(_, n)| *n);
            if let (Some(method_node), Some(name_node)) = (m, n)
                && ctx.node_text(name_node) == "map"
            {
                extracted_method = match crate::language::java::members::parse_method_node(
                    &ctx,
                    &type_ctx,
                    method_node,
                ) {
                    Some(CurrentClassMember::Method(m)) => Some(m),
                    _ => None,
                };
                break;
            }
        }
        let extracted_method = extracted_method.expect("map method from parse_method_node");

        let origin = ClassOrigin::SourceFile(Arc::from("file:///tmp/provenance/Demo.java"));
        let parsed_classes = parse_java_source(src, origin.clone(), None);
        let parsed_demo = parsed_classes
            .iter()
            .find(|c| c.internal_name.as_ref() == "org/example/Demo")
            .expect("parsed Demo");
        let parsed_map = parsed_demo
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "map")
            .expect("parsed map method");
        let parsed_class_internal = parsed_demo.internal_name.to_string();
        let parsed_class_generic_signature = parsed_demo.generic_signature.clone();
        let parsed_class_origin = parsed_demo.origin.clone();
        let parsed_map_desc = parsed_map.desc().to_string();
        let parsed_map_generic_signature = parsed_map.generic_signature.clone();
        let parsed_map_return_type = parsed_map.return_type.clone();
        let parsed_map_param_names = parsed_map
            .params
            .items
            .iter()
            .map(|p| p.name.as_ref().to_string())
            .collect::<Vec<_>>();
        let parsed_map_param_descs = parsed_map
            .params
            .items
            .iter()
            .map(|p| p.descriptor.as_ref().to_string())
            .collect::<Vec<_>>();

        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.update_source(scope, origin.clone(), parsed_classes);
        let view = idx.view(scope);
        let indexed_demo = view.get_class("org/example/Demo").expect("indexed Demo");
        let indexed_map = indexed_demo
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "map")
            .expect("indexed map method");
        let (visible_methods, _) = view.collect_inherited_members("org/example/Demo");
        let visible_map = visible_methods
            .iter()
            .find(|m| m.name.as_ref() == "map")
            .expect("provider-visible map");

        let mut out = String::new();
        out.push_str("source_method_text:\n");
        out.push_str("public <R> Demo<R> map(Function<? super T, ? extends R> fn)\n\n");
        out.push_str(&format!("ideal_generic_signature:\n{}\n\n", expected_ideal));

        out.push_str("stage_parse_method_node:\n");
        out.push_str(&format!(
            "name={}\ndesc={}\ngeneric_signature={:?}\nreturn_type={:?}\nparam_names={:?}\nparam_descs={:?}\n\n",
            extracted_method.name,
            extracted_method.desc(),
            extracted_method.generic_signature,
            extracted_method.return_type,
            extracted_method.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
            extracted_method.params.items.iter().map(|p| p.descriptor.as_ref()).collect::<Vec<_>>(),
        ));

        out.push_str("stage_parse_java_source_class_metadata:\n");
        out.push_str(&format!(
            "class_internal={}\nclass_generic_signature={:?}\nclass_origin={:?}\nmap_desc={}\nmap_generic_signature={:?}\nmap_return_type={:?}\nmap_param_names={:?}\nmap_param_descs={:?}\n\n",
            parsed_class_internal,
            parsed_class_generic_signature,
            parsed_class_origin,
            parsed_map_desc,
            parsed_map_generic_signature,
            parsed_map_return_type,
            parsed_map_param_names,
            parsed_map_param_descs,
        ));

        out.push_str("stage_workspace_index_visible:\n");
        out.push_str(&format!(
            "class_internal={}\nclass_generic_signature={:?}\nclass_origin={:?}\nindexed_map_desc={}\nindexed_map_generic_signature={:?}\nindexed_map_return_type={:?}\nvisible_map_desc={}\nvisible_map_generic_signature={:?}\nvisible_map_return_type={:?}\n",
            indexed_demo.internal_name,
            indexed_demo.generic_signature,
            indexed_demo.origin,
            indexed_map.desc(),
            indexed_map.generic_signature,
            indexed_map.return_type,
            visible_map.desc(),
            visible_map.generic_signature,
            visible_map.return_type,
        ));

        insta::assert_snapshot!("method_generic_metadata_provenance_map", out);
    }

    #[test]
    fn test_snapshot_groupby_method_detail_rendering_provenance() {
        let src = indoc::indoc! {r#"
            package org.example;
            import java.util.List;
            import java.util.Map;
            import java.util.function.Function;
            public class Box<R> {
                public <K, V> Map<K, List<V>> groupBy(
                    Function<? super R, ? extends K> keyFn,
                    Function<? super R, ? extends V> valueFn
                ) { return null; }
            }
        "#};

        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let cls = classes
            .iter()
            .find(|c| c.internal_name.as_ref() == "org/example/Box")
            .expect("Box class");
        let group_by = cls
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "groupBy")
            .expect("groupBy method");

        let receiver_internal = "org/example/Box<Ljava/util/List<Ljava/lang/String;>;>";
        let sig_to_use = group_by
            .generic_signature
            .clone()
            .unwrap_or_else(|| group_by.desc());
        let base_return = group_by.return_type.as_deref().unwrap_or("V");
        let ret_jvm = sig_to_use
            .find(')')
            .map(|i| &sig_to_use[i + 1..])
            .unwrap_or(base_return);

        let substituted_return =
            substitute_type(receiver_internal, cls.generic_signature.as_deref(), ret_jvm);
        let rendered_return = substituted_return
            .as_ref()
            .map(|t| t.to_internal_with_generics())
            .or_else(|| signature_to_source_type(ret_jvm, &TestProvider))
            .or_else(|| descriptor_to_source_type(ret_jvm, &TestProvider));

        let mut param_rows = Vec::new();
        if let Some(start) = sig_to_use.find('(')
            && let Some(end) = sig_to_use.find(')')
        {
            let mut params = &sig_to_use[start + 1..end];
            while !params.is_empty() {
                if let Some((_, rest)) = JvmType::parse(params) {
                    let raw = &params[..params.len() - rest.len()];
                    let substituted =
                        substitute_type(receiver_internal, cls.generic_signature.as_deref(), raw);
                    let rendered = substituted
                        .as_ref()
                        .map(|t| t.to_internal_with_generics())
                        .or_else(|| signature_to_source_type(raw, &TestProvider))
                        .or_else(|| descriptor_to_source_type(raw, &TestProvider));
                    param_rows.push((raw.to_string(), substituted, rendered));
                    params = rest;
                } else {
                    break;
                }
            }
        }

        let detail = render::method_detail(receiver_internal, cls, group_by, &TestProvider);

        let mut out = String::new();
        out.push_str("method_summary:\n");
        out.push_str(&format!(
            "desc={}\ngeneric_signature={:?}\nreturn_type={:?}\nparam_descriptors={:?}\n\n",
            group_by.desc(),
            group_by.generic_signature,
            group_by.return_type,
            group_by
                .params
                .items
                .iter()
                .map(|p| p.descriptor.as_ref().to_string())
                .collect::<Vec<_>>(),
        ));
        out.push_str("render_inputs:\n");
        out.push_str(&format!(
            "receiver_internal={}\nclass_generic_signature={:?}\nsig_to_use={}\nbase_return={}\nret_jvm={}\nsubstituted_return={}\nrendered_return={:?}\n\n",
            receiver_internal,
            cls.generic_signature,
            sig_to_use,
            base_return,
            ret_jvm,
            substituted_return
                .as_ref()
                .map(|t| t.to_internal_with_generics())
                .unwrap_or_else(|| "<none>".to_string()),
            rendered_return,
        ));
        out.push_str("render_param_tokens:\n");
        for (idx, (raw, substituted, rendered)) in param_rows.iter().enumerate() {
            out.push_str(&format!(
                "#{idx}: raw={raw} | substituted={:?} | rendered={rendered:?}\n",
                substituted.as_ref().map(|t| t.to_internal_with_generics())
            ));
        }
        out.push_str("\nfinal_detail:\n");
        out.push_str(&detail);
        out.push('\n');

        assert!(
            !detail.contains("Ljava/") && !detail.contains("TK;") && !detail.contains("TV;"),
            "detail should be source-style, got: {detail}"
        );

        insta::assert_snapshot!("groupby_method_detail_rendering_provenance", out);
    }

    #[test]
    fn test_snapshot_method_detail_source_vs_index_consistency() {
        use crate::index::ClassMetadata;

        let src = indoc::indoc! {r#"
            package org.example;
            import java.util.List;
            import java.util.Map;
            import java.util.function.Function;
            public class Box<R> {
                public <K, V> Map<K, List<V>> groupBy(
                    Function<? super R, ? extends K> keyFn,
                    Function<? super R, ? extends V> valueFn
                ) { return null; }
            }
        "#};
        let parsed = parse_java_source(src, ClassOrigin::Unknown, None);
        let source_cls = parsed
            .iter()
            .find(|c| c.internal_name.as_ref() == "org/example/Box")
            .expect("source class");
        let source_method = source_cls
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "groupBy")
            .expect("source method");

        // Simulate index-visible metadata path by cloning source-derived class metadata.
        let indexed_cls: ClassMetadata = source_cls.clone();
        let indexed_method = indexed_cls
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "groupBy")
            .expect("indexed method");

        let receiver_internal = "org/example/Box<Ljava/util/List<Ljava/lang/String;>;>";
        let source_detail =
            render::method_detail(receiver_internal, source_cls, source_method, &TestProvider);
        let indexed_detail = render::method_detail(
            receiver_internal,
            &indexed_cls,
            indexed_method,
            &TestProvider,
        );

        let out = format!(
            "source_detail:\n{}\n\nindexed_detail:\n{}\n\nequal={}\n",
            source_detail,
            indexed_detail,
            source_detail == indexed_detail
        );
        insta::assert_snapshot!("method_detail_source_vs_index_consistency", out);
        assert_eq!(source_detail, indexed_detail);
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

    #[test]
    fn test_access_flags_interface() {
        let src = "public interface I { void f(); }";
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let i = classes.iter().find(|c| c.name.as_ref() == "I").unwrap();
        assert!(i.access_flags & rust_asm::constants::ACC_INTERFACE != 0);
    }

    #[test]
    fn test_access_flags_enum() {
        let src = "public enum E { A, B }";
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let e = classes.iter().find(|c| c.name.as_ref() == "E").unwrap();
        assert!(e.access_flags & rust_asm::constants::ACC_ENUM != 0);
    }

    #[test]
    fn test_access_flags_annotation_type() {
        let src = "public @interface Ann { }";
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let a = classes.iter().find(|c| c.name.as_ref() == "Ann").unwrap();
        assert!(a.access_flags & rust_asm::constants::ACC_ANNOTATION != 0);
        assert!(a.access_flags & rust_asm::constants::ACC_INTERFACE != 0);
    }

    #[test]
    fn test_access_flags_super_on_class_like_decls() {
        let src = r#"
public class C {}
public enum E { A }
public record R(int x) {}
public interface I {}
public @interface Ann {}
"#;
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);

        let c = classes.iter().find(|x| x.name.as_ref() == "C").unwrap();
        assert!(c.access_flags & rust_asm::constants::ACC_SUPER != 0);

        let e = classes.iter().find(|x| x.name.as_ref() == "E").unwrap();
        assert!(e.access_flags & rust_asm::constants::ACC_SUPER != 0);

        let r = classes.iter().find(|x| x.name.as_ref() == "R").unwrap();
        assert!(r.access_flags & rust_asm::constants::ACC_SUPER != 0);

        let i = classes.iter().find(|x| x.name.as_ref() == "I").unwrap();
        assert!(i.access_flags & rust_asm::constants::ACC_SUPER == 0);

        let ann = classes.iter().find(|x| x.name.as_ref() == "Ann").unwrap();
        assert!(ann.access_flags & rust_asm::constants::ACC_SUPER == 0);
    }

    #[test]
    fn test_record_synthetic_members_are_indexed() {
        let src = "record Point(int x, int y) {}";
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let point = classes.iter().find(|c| c.name.as_ref() == "Point").unwrap();
        assert!(
            point
                .methods
                .iter()
                .any(|method| method.name.as_ref() == "x" && method.desc().as_ref() == "()I")
        );
        assert!(
            point
                .methods
                .iter()
                .any(|method| method.name.as_ref() == "y" && method.desc().as_ref() == "()I")
        );
        assert!(point
            .methods
            .iter()
            .any(|method| method.name.as_ref() == "<init>" && method.desc().as_ref() == "(II)V"));
    }

    #[test]
    fn test_record_compact_constructor_is_indexed() {
        let src = r#"
record Point(int x, int y) {
    public Point {
        if (x < 0) throw new IllegalArgumentException();
    }
}
"#;
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let point = classes.iter().find(|c| c.name.as_ref() == "Point").unwrap();

        // Should have the canonical constructor with correct signature
        let canonical_ctor = point
            .methods
            .iter()
            .find(|method| method.name.as_ref() == "<init>" && method.desc().as_ref() == "(II)V");

        assert!(
            canonical_ctor.is_some(),
            "Compact constructor should be indexed as <init>(II)V, found: {:?}",
            point
                .methods
                .iter()
                .filter(|m| m.name.as_ref() == "<init>")
                .map(|m| m.desc())
                .collect::<Vec<_>>()
        );

        // Should still have accessor methods
        assert!(point.methods.iter().any(|m| m.name.as_ref() == "x"));
        assert!(point.methods.iter().any(|m| m.name.as_ref() == "y"));
    }

    #[test]
    fn test_record_explicit_constructor_is_indexed() {
        let src = r#"
record Point(int x, int y) {
    public Point(int x) {
        this(x, 0);
    }
}
"#;
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let point = classes.iter().find(|c| c.name.as_ref() == "Point").unwrap();

        // Should have both the canonical constructor (synthetic) and the custom one
        let canonical_ctor = point
            .methods
            .iter()
            .find(|method| method.name.as_ref() == "<init>" && method.desc().as_ref() == "(II)V");

        let custom_ctor = point
            .methods
            .iter()
            .find(|method| method.name.as_ref() == "<init>" && method.desc().as_ref() == "(I)V");

        assert!(
            canonical_ctor.is_some(),
            "Canonical constructor should be synthesized, found: {:?}",
            point
                .methods
                .iter()
                .filter(|m| m.name.as_ref() == "<init>")
                .map(|m| m.desc())
                .collect::<Vec<_>>()
        );

        assert!(
            custom_ctor.is_some(),
            "Custom constructor should be indexed, found: {:?}",
            point
                .methods
                .iter()
                .filter(|m| m.name.as_ref() == "<init>")
                .map(|m| m.desc())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_record_compact_constructor_overrides_synthetic() {
        let src = r#"
record Point(int x, int y) {
    public Point {
        if (x < 0) x = 0;
    }
}
"#;
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let point = classes.iter().find(|c| c.name.as_ref() == "Point").unwrap();

        // Should have exactly one canonical constructor (compact overrides synthetic)
        let canonical_ctors: Vec<_> = point
            .methods
            .iter()
            .filter(|method| method.name.as_ref() == "<init>" && method.desc().as_ref() == "(II)V")
            .collect();

        assert_eq!(
            canonical_ctors.len(),
            1,
            "Should have exactly one canonical constructor, found: {}",
            canonical_ctors.len()
        );
    }

    #[test]
    fn test_record_multiple_constructors() {
        let src = r#"
record Point(int x, int y) {
    public Point {
        if (x < 0) x = 0;
        if (y < 0) y = 0;
    }
    
    public Point(int x) {
        this(x, 0);
    }
    
    public Point() {
        this(0, 0);
    }
}
"#;
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let point = classes.iter().find(|c| c.name.as_ref() == "Point").unwrap();

        let constructors: Vec<_> = point
            .methods
            .iter()
            .filter(|method| method.name.as_ref() == "<init>")
            .collect();

        // Should have 3 constructors: (II)V, (I)V, ()V
        assert_eq!(
            constructors.len(),
            3,
            "Should have 3 constructors, found: {:?}",
            constructors.iter().map(|m| m.desc()).collect::<Vec<_>>()
        );

        assert!(constructors.iter().any(|m| m.desc().as_ref() == "(II)V"));
        assert!(constructors.iter().any(|m| m.desc().as_ref() == "(I)V"));
        assert!(constructors.iter().any(|m| m.desc().as_ref() == "()V"));
    }

    #[test]
    fn test_enum_constants_are_indexed_as_fields() {
        let src = "enum Color { RED, GREEN, BLUE }";
        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        let color = classes.iter().find(|c| c.name.as_ref() == "Color").unwrap();
        let names: Vec<&str> = color
            .fields
            .iter()
            .map(|field| field.name.as_ref())
            .collect();
        assert_eq!(names, vec!["RED", "GREEN", "BLUE"]);
    }

    #[test]
    fn test_find_symbol_range_resolves_record_accessor_to_component() {
        let src = "record Point(int x, int y) {}";
        let idx = crate::index::WorkspaceIndex::new();
        idx.add_classes(parse_java_source(src, ClassOrigin::Unknown, None));
        let view = idx.view(crate::index::IndexScope {
            module: crate::index::ModuleId::ROOT,
        });
        let range = super::find_symbol_range(src, "Point", Some("x"), Some("()I"), &view).unwrap();
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 17);
    }

    #[test]
    fn test_find_symbol_range_resolves_enum_constant_to_constant_declaration() {
        let src = "enum Color { RED, GREEN, BLUE }";
        let idx = crate::index::WorkspaceIndex::new();
        idx.add_classes(parse_java_source(src, ClassOrigin::Unknown, None));
        let view = idx.view(crate::index::IndexScope {
            module: crate::index::ModuleId::ROOT,
        });
        let range = super::find_symbol_range(src, "Color", Some("GREEN"), None, &view).unwrap();
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 18);
    }
}

/// Generate synthetic nested classes from Lombok annotations (e.g., @Builder)
fn generate_synthetic_nested_classes(
    classes: &[ClassMetadata],
    _type_ctx: &SourceTypeCtx,
    _origin: &ClassOrigin,
) -> Vec<ClassMetadata> {
    let mut synthetic_nested = Vec::new();

    for _class in classes {
        // For each class, check if it has annotations that generate nested classes
        // Currently, this is primarily @Builder

        // Extract synthetic nested classes from the class's synthetic member set
        // This requires re-parsing to get the tree node, which we'll optimize later
        // For now, we'll collect nested classes that were generated during synthesis

        // TODO: Implement actual nested class generation
        // This will be filled in by the BuilderRule
    }

    synthetic_nested
}

#[cfg(test)]
mod nested_class_navigation_tests {
    use super::*;
    use crate::index::{ClassOrigin, IndexScope, ModuleId, WorkspaceIndex};

    #[test]
    fn test_find_nested_class_by_internal_name() {
        let src = indoc::indoc! {"
            package com.example;
            
            public class Outer {
                private String outerField;
                
                public static class Inner {
                    private String innerField;
                    
                    public String getInnerField() {
                        return innerField;
                    }
                }
            }
        "};

        let classes = parse_java_source(src, ClassOrigin::Unknown, None);
        println!("\n=== Parsed {} classes ===", classes.len());
        for class in &classes {
            println!(
                "  Class: {} (internal: {})",
                class.name, class.internal_name
            );
            println!("    inner_class_of: {:?}", class.inner_class_of);
            println!(
                "    methods: {:?}",
                class
                    .methods
                    .iter()
                    .map(|m| m.name.as_ref())
                    .collect::<Vec<_>>()
            );
        }

        assert_eq!(
            classes.len(),
            2,
            "Should parse both Outer and Inner classes"
        );

        let outer = classes.iter().find(|c| c.name.as_ref() == "Outer").unwrap();
        let inner = classes.iter().find(|c| c.name.as_ref() == "Inner").unwrap();

        assert_eq!(outer.internal_name.as_ref(), "com/example/Outer");
        assert_eq!(inner.internal_name.as_ref(), "com/example/Outer$Inner");
        assert_eq!(inner.inner_class_of.as_deref(), Some("Outer"));

        // Test find_symbol_range for nested class
        let idx = WorkspaceIndex::new();
        idx.add_classes(classes);
        let view = idx.view(IndexScope {
            module: ModuleId::ROOT,
        });

        // Try to find the Inner class
        let range = find_symbol_range(src, "com/example/Outer$Inner", None, None, &view);
        println!("\n=== Find Inner class range: {:?} ===", range);
        assert!(range.is_some(), "Should find Inner class by internal name");

        // Try to find getInnerField method
        let range = find_symbol_range(
            src,
            "com/example/Outer$Inner",
            Some("getInnerField"),
            Some("()LString;"), // Use the actual descriptor format from parsing
            &view,
        );
        println!("=== Find getInnerField range: {:?} ===", range);
        assert!(
            range.is_some(),
            "Should find getInnerField method in Inner class"
        );
    }
}

#[test]
fn test_nested_class_completion_scenarios() {
    let src = indoc::indoc! {"
            package com.example;
            
            public class Outer {
                private String outerField;
                
                public static class StaticInner {
                    private String staticInnerField;
                    
                    public String getStaticInnerField() {
                        return staticInnerField;
                    }
                }
                
                public class InstanceInner {
                    private String instanceInnerField;
                    
                    public String getInstanceInnerField() {
                        return instanceInnerField;
                    }
                }
                
                public void testMethod() {
                    // Should be able to reference StaticInner by simple name
                    StaticInner si = new StaticInner();
                    
                    // Should be able to reference InstanceInner by simple name
                    InstanceInner ii = new InstanceInner();
                }
            }
        "};

    let classes = parse_java_source(src, ClassOrigin::Unknown, None);
    println!("\n=== Parsed {} classes ===", classes.len());
    for class in &classes {
        println!(
            "  Class: {} (internal: {})",
            class.name, class.internal_name
        );
        println!("    inner_class_of: {:?}", class.inner_class_of);
        println!("    access_flags: 0x{:x}", class.access_flags);
    }

    assert_eq!(
        classes.len(),
        3,
        "Should parse Outer, StaticInner, and InstanceInner"
    );

    let outer = classes.iter().find(|c| c.name.as_ref() == "Outer").unwrap();
    let static_inner = classes
        .iter()
        .find(|c| c.name.as_ref() == "StaticInner")
        .unwrap();
    let instance_inner = classes
        .iter()
        .find(|c| c.name.as_ref() == "InstanceInner")
        .unwrap();

    assert_eq!(outer.internal_name.as_ref(), "com/example/Outer");
    assert_eq!(
        static_inner.internal_name.as_ref(),
        "com/example/Outer$StaticInner"
    );
    assert_eq!(
        instance_inner.internal_name.as_ref(),
        "com/example/Outer$InstanceInner"
    );

    // Verify inner_class_of is set correctly
    assert_eq!(static_inner.inner_class_of.as_deref(), Some("Outer"));
    assert_eq!(instance_inner.inner_class_of.as_deref(), Some("Outer"));

    // Verify static flag
    use rust_asm::constants::ACC_STATIC;
    assert_ne!(
        static_inner.access_flags & ACC_STATIC,
        0,
        "StaticInner should have ACC_STATIC flag"
    );
    assert_eq!(
        instance_inner.access_flags & ACC_STATIC,
        0,
        "InstanceInner should NOT have ACC_STATIC flag"
    );
}

#[test]
fn test_nested_class_with_dollar_in_name() {
    use crate::index::{IndexScope, ModuleId, WorkspaceIndex};

    // Test that we handle nested classes correctly even when $ is in the class name
    let src = indoc::indoc! {"
            package com.example;
            
            public class Outer$Class {
                private String outerField;
                
                public static class Inner$Class {
                    private String innerField;
                    
                    public String getInnerField() {
                        return innerField;
                    }
                }
            }
        "};

    let classes = parse_java_source(src, ClassOrigin::Unknown, None);
    println!("\n=== Parsed {} classes ===", classes.len());
    for class in &classes {
        println!(
            "  Class: {} (internal: {})",
            class.name, class.internal_name
        );
        println!("    inner_class_of: {:?}", class.inner_class_of);
    }

    assert_eq!(
        classes.len(),
        2,
        "Should parse both Outer$Class and Inner$Class"
    );

    let outer = classes
        .iter()
        .find(|c| c.name.as_ref() == "Outer$Class")
        .unwrap();
    let inner = classes
        .iter()
        .find(|c| c.name.as_ref() == "Inner$Class")
        .unwrap();

    assert_eq!(outer.internal_name.as_ref(), "com/example/Outer$Class");
    assert_eq!(
        inner.internal_name.as_ref(),
        "com/example/Outer$Class$Inner$Class"
    );
    assert_eq!(inner.inner_class_of.as_deref(), Some("Outer$Class"));

    // Test find_symbol_range for nested class with $ in name
    let idx = WorkspaceIndex::new();
    idx.add_classes(classes);
    let view = idx.view(IndexScope {
        module: ModuleId::ROOT,
    });

    // Try to find the Inner$Class
    let range = find_symbol_range(
        src,
        "com/example/Outer$Class$Inner$Class",
        None,
        None,
        &view,
    );
    println!("\n=== Find Inner$Class range: {:?} ===", range);
    assert!(
        range.is_some(),
        "Should find Inner$Class by internal name using metadata lookup"
    );

    // Try to find getInnerField method
    let range = find_symbol_range(
        src,
        "com/example/Outer$Class$Inner$Class",
        Some("getInnerField"),
        Some("()LString;"),
        &view,
    );
    println!("=== Find getInnerField range: {:?} ===", range);
    assert!(
        range.is_some(),
        "Should find getInnerField method in Inner$Class"
    );
}
