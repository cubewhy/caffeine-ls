use std::sync::Arc;

use tree_sitter::Node;
use tree_sitter_utils::traversal::first_child_of_kind;

use crate::language::java::JavaContextExtractor;
use crate::semantic::CursorLocation;
use crate::semantic::context::JavaModuleContextKind;

pub(crate) const MODULE_DIRECTIVE_KEYWORDS: &[&str] =
    &["requires", "exports", "opens", "uses", "provides"];
pub(crate) const REQUIRES_MODIFIERS: &[&str] = &["static", "transitive"];

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct JavaModuleRequires {
    pub module_name: Arc<str>,
    pub is_static: bool,
    pub is_transitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct JavaModulePackageDirective {
    pub package_name: Arc<str>,
    pub target_modules: Vec<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct JavaModuleProvides {
    pub service: Arc<str>,
    pub implementations: Vec<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct JavaModuleDescriptor {
    pub name: Arc<str>,
    pub is_open: bool,
    pub requires: Vec<JavaModuleRequires>,
    pub exports: Vec<JavaModulePackageDirective>,
    pub opens: Vec<JavaModulePackageDirective>,
    pub uses: Vec<Arc<str>>,
    pub provides: Vec<JavaModuleProvides>,
}

pub fn extract_module_descriptor_from_class_node(
    class_node: &rust_asm::nodes::ClassNode,
) -> Option<Arc<JavaModuleDescriptor>> {
    let module = class_node.module.as_ref()?;

    Some(Arc::new(JavaModuleDescriptor {
        name: Arc::from(module.name.as_str()),
        is_open: (module.access_flags & rust_asm::constants::ACC_OPEN) != 0,
        requires: module
            .requires
            .iter()
            .map(|require| JavaModuleRequires {
                module_name: Arc::from(require.module.as_str()),
                is_static: (require.access_flags & rust_asm::constants::ACC_STATIC_PHASE) != 0,
                is_transitive: (require.access_flags & rust_asm::constants::ACC_TRANSITIVE) != 0,
            })
            .collect(),
        exports: module
            .exports
            .iter()
            .map(|export| JavaModulePackageDirective {
                package_name: slash_name_to_dotted_arc(&export.package),
                target_modules: export
                    .modules
                    .iter()
                    .map(|module| Arc::from(module.as_str()))
                    .collect(),
            })
            .collect(),
        opens: module
            .opens
            .iter()
            .map(|open| JavaModulePackageDirective {
                package_name: slash_name_to_dotted_arc(&open.package),
                target_modules: open
                    .modules
                    .iter()
                    .map(|module| Arc::from(module.as_str()))
                    .collect(),
            })
            .collect(),
        uses: module
            .uses
            .iter()
            .map(|service| slash_name_to_dotted_arc(service))
            .collect(),
        provides: module
            .provides
            .iter()
            .map(|provide| JavaModuleProvides {
                service: slash_name_to_dotted_arc(&provide.service),
                implementations: provide
                    .providers
                    .iter()
                    .map(|provider| slash_name_to_dotted_arc(provider))
                    .collect(),
            })
            .collect(),
    }))
}

pub fn render_module_descriptor_source(descriptor: &JavaModuleDescriptor) -> String {
    let mut lines = vec![format!(
        "{}module {} {{",
        if descriptor.is_open { "open " } else { "" },
        descriptor.name
    )];

    for require in &descriptor.requires {
        let mut line = String::from("    requires");
        if require.is_static {
            line.push_str(" static");
        }
        if require.is_transitive {
            line.push_str(" transitive");
        }
        line.push(' ');
        line.push_str(require.module_name.as_ref());
        line.push(';');
        lines.push(line);
    }

    for export in &descriptor.exports {
        let mut line = format!("    exports {}", export.package_name);
        if !export.target_modules.is_empty() {
            line.push_str(" to ");
            line.push_str(
                &export
                    .target_modules
                    .iter()
                    .map(|module| module.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        line.push(';');
        lines.push(line);
    }

    for open in &descriptor.opens {
        let mut line = format!("    opens {}", open.package_name);
        if !open.target_modules.is_empty() {
            line.push_str(" to ");
            line.push_str(
                &open
                    .target_modules
                    .iter()
                    .map(|module| module.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        line.push(';');
        lines.push(line);
    }

    for service in &descriptor.uses {
        lines.push(format!("    uses {};", service));
    }

    for provide in &descriptor.provides {
        let implementations = provide
            .implementations
            .iter()
            .map(|implementation| implementation.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "    provides {} with {};",
            provide.service, implementations
        ));
    }

    lines.push("}".to_string());
    lines.join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModuleCompletionMatch {
    pub location: CursorLocation,
    pub query: String,
    pub context: JavaModuleContextKind,
}

impl ModuleCompletionMatch {
    fn expression(prefix: String, context: JavaModuleContextKind) -> Self {
        Self {
            location: CursorLocation::Expression {
                prefix: prefix.clone(),
            },
            query: prefix,
            context,
        }
    }

    fn type_annotation(prefix: String, context: JavaModuleContextKind) -> Self {
        Self {
            location: CursorLocation::TypeAnnotation {
                prefix: prefix.clone(),
            },
            query: prefix,
            context,
        }
    }
}

pub fn module_declaration_node(root: Node<'_>) -> Option<Node<'_>> {
    first_child_of_kind(root, "module_declaration")
}

pub fn module_declaration_name_node(root: Node<'_>) -> Option<Node<'_>> {
    module_declaration_node(root)?.child_by_field_name("name")
}

pub fn extract_module_descriptor_from_root(
    source: &str,
    root: Node<'_>,
) -> Option<Arc<JavaModuleDescriptor>> {
    let module_decl = module_declaration_node(root)?;
    let ctx = JavaContextExtractor::for_indexing(source, None);
    let name_node = module_decl.child_by_field_name("name")?;
    let name = ctx.node_text(name_node).trim();
    if name.is_empty() {
        return None;
    }

    let is_open = source[module_decl.start_byte()..name_node.start_byte()].contains("open");
    let body = module_decl.child_by_field_name("body")?;
    let mut requires = Vec::new();
    let mut exports = Vec::new();
    let mut opens = Vec::new();
    let mut uses = Vec::new();
    let mut provides = Vec::new();

    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        let text = ctx.node_text(child).trim();
        match child.kind() {
            "requires_module_directive" => {
                if let Some(requirement) = parse_requires_directive(text) {
                    requires.push(requirement);
                }
            }
            "exports_module_directive" => {
                if let Some(export) = parse_package_directive(text, "exports") {
                    exports.push(export);
                }
            }
            "opens_module_directive" => {
                if let Some(open) = parse_package_directive(text, "opens") {
                    opens.push(open);
                }
            }
            "uses_module_directive" => {
                if let Some(uses_name) = parse_uses_directive(text) {
                    uses.push(uses_name);
                }
            }
            "provides_module_directive" => {
                if let Some(provides_item) = parse_provides_directive(text) {
                    provides.push(provides_item);
                }
            }
            _ => {}
        }
    }

    Some(Arc::new(JavaModuleDescriptor {
        name: Arc::from(name),
        is_open,
        requires,
        exports,
        opens,
        uses,
        provides,
    }))
}

pub fn extract_module_descriptor_from_source(source: &str) -> Option<Arc<JavaModuleDescriptor>> {
    let mut parser = crate::language::java::make_java_parser();
    let tree = parser.parse(source, None)?;
    extract_module_descriptor_from_root(source, tree.root_node())
}

fn slash_name_to_dotted_arc(name: &str) -> Arc<str> {
    Arc::from(name.replace('/', ".").as_str())
}

pub(crate) fn infer_module_completion_context(
    ctx: &JavaContextExtractor,
    root: Node<'_>,
    cursor_node: Option<Node<'_>>,
) -> Option<ModuleCompletionMatch> {
    let module_decl = module_declaration_node(root)?;
    let body = module_decl.child_by_field_name("body")?;

    let directive = cursor_node
        .and_then(find_module_directive_ancestor)
        .or_else(|| find_module_directive_at_offset(body, ctx.offset));

    if let Some(directive) = directive {
        return infer_directive_completion_context(ctx, directive);
    }

    if contains_offset(body, ctx.offset) {
        if let Some(fragment_match) = infer_broken_directive_completion_context(ctx, body) {
            return Some(fragment_match);
        }
        let prefix = identifier_prefix_before_cursor(ctx.source_str(), ctx.offset);
        return Some(ModuleCompletionMatch::expression(
            prefix,
            JavaModuleContextKind::DirectiveKeyword,
        ));
    }

    None
}

fn infer_broken_directive_completion_context(
    ctx: &JavaContextExtractor,
    body: Node<'_>,
) -> Option<ModuleCompletionMatch> {
    let offset = ctx.offset.min(body.end_byte());
    if offset < body.start_byte() {
        return None;
    }

    let before = ctx.byte_slice(body.start_byte(), offset);
    let fragment = current_module_statement_fragment(before);
    infer_directive_completion_context_from_text(ctx, fragment)
}

fn infer_directive_completion_context(
    ctx: &JavaContextExtractor,
    directive: Node<'_>,
) -> Option<ModuleCompletionMatch> {
    let offset = ctx.offset.min(directive.end_byte());
    if offset < directive.start_byte() {
        return None;
    }

    let before = ctx.byte_slice(directive.start_byte(), offset);
    match directive.kind() {
        "requires_module_directive" => Some(ModuleCompletionMatch::expression(
            dotted_prefix_before_cursor(ctx.source_str(), ctx.offset),
            classify_requires_context(before),
        )),
        "exports_module_directive" => Some(infer_package_directive_completion_context(
            ctx,
            before,
            "exports",
            JavaModuleContextKind::ExportsPackage,
        )),
        "opens_module_directive" => Some(infer_package_directive_completion_context(
            ctx,
            before,
            "opens",
            JavaModuleContextKind::OpensPackage,
        )),
        "uses_module_directive" => Some(ModuleCompletionMatch::type_annotation(
            dotted_prefix_before_cursor(ctx.source_str(), ctx.offset),
            JavaModuleContextKind::UsesType,
        )),
        "provides_module_directive" => {
            let after_keyword = before
                .strip_prefix("provides")
                .unwrap_or(before)
                .trim_start();
            let context = if contains_with_separator(after_keyword) {
                JavaModuleContextKind::ProvidesImplementation
            } else {
                JavaModuleContextKind::ProvidesService
            };
            Some(ModuleCompletionMatch::type_annotation(
                dotted_prefix_before_cursor(ctx.source_str(), ctx.offset),
                context,
            ))
        }
        _ => None,
    }
}

fn infer_directive_completion_context_from_text(
    ctx: &JavaContextExtractor,
    before: &str,
) -> Option<ModuleCompletionMatch> {
    if starts_with_directive_keyword(before, "requires") {
        return Some(ModuleCompletionMatch::expression(
            dotted_prefix_before_cursor(ctx.source_str(), ctx.offset),
            classify_requires_context(before),
        ));
    }
    if starts_with_directive_keyword(before, "exports") {
        return Some(infer_package_directive_completion_context(
            ctx,
            before,
            "exports",
            JavaModuleContextKind::ExportsPackage,
        ));
    }
    if starts_with_directive_keyword(before, "opens") {
        return Some(infer_package_directive_completion_context(
            ctx,
            before,
            "opens",
            JavaModuleContextKind::OpensPackage,
        ));
    }
    if starts_with_directive_keyword(before, "uses") {
        return Some(ModuleCompletionMatch::type_annotation(
            dotted_prefix_before_cursor(ctx.source_str(), ctx.offset),
            JavaModuleContextKind::UsesType,
        ));
    }
    if starts_with_directive_keyword(before, "provides") {
        let after_keyword = before
            .strip_prefix("provides")
            .unwrap_or(before)
            .trim_start();
        let context = if contains_with_separator(after_keyword) {
            JavaModuleContextKind::ProvidesImplementation
        } else {
            JavaModuleContextKind::ProvidesService
        };
        return Some(ModuleCompletionMatch::type_annotation(
            dotted_prefix_before_cursor(ctx.source_str(), ctx.offset),
            context,
        ));
    }
    None
}

fn infer_package_directive_completion_context(
    ctx: &JavaContextExtractor,
    before: &str,
    keyword: &str,
    package_context: JavaModuleContextKind,
) -> ModuleCompletionMatch {
    let after_keyword = before.strip_prefix(keyword).unwrap_or(before).trim_start();
    let context = if contains_to_separator(after_keyword) {
        JavaModuleContextKind::TargetModule
    } else {
        package_context
    };
    ModuleCompletionMatch::expression(
        dotted_prefix_before_cursor(ctx.source_str(), ctx.offset),
        context,
    )
}

fn classify_requires_context(before: &str) -> JavaModuleContextKind {
    let after_keyword = before
        .strip_prefix("requires")
        .unwrap_or(before)
        .trim_start();
    if after_keyword.is_empty() {
        return JavaModuleContextKind::RequiresModifier;
    }

    let ends_with_whitespace = after_keyword
        .chars()
        .last()
        .is_some_and(char::is_whitespace);
    let tokens = after_keyword
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return JavaModuleContextKind::RequiresModifier;
    }

    let (completed_tokens, active_token) = if ends_with_whitespace {
        (&tokens[..], "")
    } else {
        (
            &tokens[..tokens.len().saturating_sub(1)],
            tokens[tokens.len() - 1],
        )
    };

    if completed_tokens
        .iter()
        .all(|token| is_requires_modifier(token))
        && (active_token.is_empty() || is_requires_modifier_prefix(active_token))
        && !active_token.contains('.')
    {
        JavaModuleContextKind::RequiresModifier
    } else {
        JavaModuleContextKind::RequiresModule
    }
}

fn find_module_directive_ancestor(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if is_module_directive_kind(candidate.kind()) {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

fn find_module_directive_at_offset(body: Node<'_>, offset: usize) -> Option<Node<'_>> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .find(|child| is_module_directive_kind(child.kind()) && contains_offset(*child, offset))
}

fn contains_offset(node: Node<'_>, offset: usize) -> bool {
    node.start_byte() <= offset && offset <= node.end_byte()
}

fn is_module_directive_kind(kind: &str) -> bool {
    matches!(
        kind,
        "requires_module_directive"
            | "exports_module_directive"
            | "opens_module_directive"
            | "uses_module_directive"
            | "provides_module_directive"
    )
}

fn dotted_prefix_before_cursor(source: &str, offset: usize) -> String {
    let bytes = source.as_bytes();
    let mut start = offset.min(bytes.len());
    while start > 0 {
        let byte = bytes[start - 1];
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.' {
            start -= 1;
        } else {
            break;
        }
    }
    source[start..offset.min(source.len())].to_string()
}

fn identifier_prefix_before_cursor(source: &str, offset: usize) -> String {
    let bytes = source.as_bytes();
    let mut start = offset.min(bytes.len());
    while start > 0 {
        let byte = bytes[start - 1];
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    source[start..offset.min(source.len())].to_string()
}

fn is_requires_modifier(token: &str) -> bool {
    REQUIRES_MODIFIERS.contains(&token)
}

fn is_requires_modifier_prefix(token: &str) -> bool {
    REQUIRES_MODIFIERS
        .iter()
        .any(|modifier| modifier.starts_with(token))
}

fn contains_to_separator(text: &str) -> bool {
    text.split_whitespace().any(|token| token == "to")
}

fn contains_with_separator(text: &str) -> bool {
    text.split_whitespace().any(|token| token == "with")
}

fn current_module_statement_fragment(before: &str) -> &str {
    let start = before
        .rfind(|c: char| c == ';' || c == '{' || c == '}')
        .map_or(0, |index| index + 1);
    before[start..].trim_start()
}

fn starts_with_directive_keyword(text: &str, keyword: &str) -> bool {
    text.strip_prefix(keyword)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

fn parse_requires_directive(text: &str) -> Option<JavaModuleRequires> {
    let body = text
        .trim()
        .trim_end_matches(';')
        .strip_prefix("requires")?
        .trim();
    let tokens = body
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let module_name = tokens
        .iter()
        .find(|token| !is_requires_modifier(token))
        .copied()?;
    Some(JavaModuleRequires {
        module_name: Arc::from(module_name),
        is_static: tokens.contains(&"static"),
        is_transitive: tokens.contains(&"transitive"),
    })
}

fn parse_package_directive(text: &str, keyword: &str) -> Option<JavaModulePackageDirective> {
    let body = collapse_whitespace(
        text.trim()
            .trim_end_matches(';')
            .strip_prefix(keyword)?
            .trim(),
    );
    let (package_name, target_modules) =
        if let Some((package_name, targets)) = body.split_once(" to ") {
            (
                package_name.trim().to_string(),
                targets
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(Arc::from)
                    .collect(),
            )
        } else {
            (body, Vec::new())
        };

    if package_name.is_empty() {
        return None;
    }

    Some(JavaModulePackageDirective {
        package_name: Arc::from(package_name.as_str()),
        target_modules,
    })
}

fn parse_uses_directive(text: &str) -> Option<Arc<str>> {
    let type_name = text
        .trim()
        .trim_end_matches(';')
        .strip_prefix("uses")?
        .trim();
    (!type_name.is_empty()).then(|| Arc::from(type_name))
}

fn parse_provides_directive(text: &str) -> Option<JavaModuleProvides> {
    let body = collapse_whitespace(
        text.trim()
            .trim_end_matches(';')
            .strip_prefix("provides")?
            .trim(),
    );
    let (service, implementations) = body.split_once(" with ")?;
    let implementations = implementations
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(Arc::from)
        .collect::<Vec<_>>();
    if service.trim().is_empty() || implementations.is_empty() {
        return None;
    }
    Some(JavaModuleProvides {
        service: Arc::from(service.trim()),
        implementations,
    })
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_cursor_marker(src: &str) -> (String, usize) {
        let offset = src.find('|').expect("cursor marker");
        (src.replacen('|', "", 1), offset)
    }

    #[test]
    fn extracts_module_descriptor_from_source() {
        let src = indoc::indoc! {r#"
            open module com.example.app {
                requires static java.logging;
                requires transitive com.example.shared;
                exports com.example.api;
                opens com.example.internal to com.example.tests, com.example.tools;
                uses com.example.spi.Service;
                provides com.example.spi.Service with
                    com.example.impl.ServiceImpl,
                    com.example.impl.SecondImpl;
            }
        "#};

        let descriptor = extract_module_descriptor_from_source(src).expect("module descriptor");

        assert_eq!(descriptor.name.as_ref(), "com.example.app");
        assert!(descriptor.is_open);
        assert_eq!(descriptor.requires.len(), 2);
        assert_eq!(descriptor.requires[0].module_name.as_ref(), "java.logging");
        assert!(descriptor.requires[0].is_static);
        assert!(descriptor.requires[1].is_transitive);
        assert_eq!(
            descriptor.exports[0].package_name.as_ref(),
            "com.example.api"
        );
        assert_eq!(descriptor.opens[0].target_modules.len(), 2);
        assert_eq!(descriptor.uses[0].as_ref(), "com.example.spi.Service");
        assert_eq!(descriptor.provides[0].implementations.len(), 2);
    }

    #[test]
    fn infers_requires_modifier_completion_context() {
        let (source, offset) = strip_cursor_marker("module demo { requires trans| }");
        let mut parser = crate::language::java::make_java_parser();
        let tree = parser.parse(&source, None).expect("tree");
        let ctx = JavaContextExtractor::new(source.as_str(), offset, None);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let context =
            infer_module_completion_context(&ctx, tree.root_node(), cursor_node).expect("context");

        assert_eq!(context.context, JavaModuleContextKind::RequiresModifier);
        assert_eq!(context.query, "trans");
    }

    #[test]
    fn infers_requires_module_completion_context_for_broken_trailing_dot_directive() {
        let (source, offset) = strip_cursor_marker("module demo { requires java.| }");
        let mut parser = crate::language::java::make_java_parser();
        let tree = parser.parse(&source, None).expect("tree");
        let ctx = JavaContextExtractor::new(source.as_str(), offset, None);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let context =
            infer_module_completion_context(&ctx, tree.root_node(), cursor_node).expect("context");

        assert_eq!(context.context, JavaModuleContextKind::RequiresModule);
        assert_eq!(context.query, "java.");
    }

    #[test]
    fn infers_target_module_completion_context() {
        let (source, offset) =
            strip_cursor_marker("module demo { exports com.example.api to com.example.te|; }");
        let mut parser = crate::language::java::make_java_parser();
        let tree = parser.parse(&source, None).expect("tree");
        let ctx = JavaContextExtractor::new(source.as_str(), offset, None);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let context =
            infer_module_completion_context(&ctx, tree.root_node(), cursor_node).expect("context");

        assert_eq!(context.context, JavaModuleContextKind::TargetModule);
        assert_eq!(context.query, "com.example.te");
    }

    #[test]
    fn infers_provides_implementation_completion_context() {
        let (source, offset) =
            strip_cursor_marker("module demo { provides a.b.Service with a.b.impl.Im|; }");
        let mut parser = crate::language::java::make_java_parser();
        let tree = parser.parse(&source, None).expect("tree");
        let ctx = JavaContextExtractor::new(source.as_str(), offset, None);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let context =
            infer_module_completion_context(&ctx, tree.root_node(), cursor_node).expect("context");

        assert_eq!(
            context.context,
            JavaModuleContextKind::ProvidesImplementation
        );
        match context.location {
            CursorLocation::TypeAnnotation { prefix } => assert_eq!(prefix, "a.b.impl.Im"),
            other => panic!("expected TypeAnnotation, got {other:?}"),
        }
    }
}
