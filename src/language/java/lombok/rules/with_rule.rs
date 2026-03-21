use std::sync::Arc;
use tree_sitter::Node;
use tree_sitter_utils::traversal::first_child_of_kind;

use crate::{
    index::{FieldSummary, MethodParam, MethodParams, MethodSummary},
    language::java::{
        JavaContextExtractor,
        lombok::{
            config::LombokConfig,
            types::{AccessLevel, annotations},
            utils::{
                find_lombok_annotation, is_field_non_null, parse_access_level, strip_field_prefix,
            },
        },
        members::parse_annotations_in_node,
        synthetic::{
            SyntheticDefinition, SyntheticDefinitionKind, SyntheticInput, SyntheticMemberRule,
            SyntheticMemberSet, SyntheticOrigin,
        },
        type_ctx::SourceTypeCtx,
    },
};

pub struct WithRule;

impl SyntheticMemberRule for WithRule {
    fn synthesize(
        &self,
        input: &SyntheticInput<'_>,
        out: &mut SyntheticMemberSet,
        explicit_methods: &[MethodSummary],
        explicit_fields: &[FieldSummary],
    ) {
        // Only process class-like declarations
        if !matches!(
            input.decl.kind(),
            "class_declaration" | "record_declaration" | "interface_declaration"
        ) {
            return;
        }

        // Load Lombok configuration
        let config = LombokConfig::new();

        // Check for class-level @With annotation
        let class_annotations = extract_class_annotations(input.ctx, input.decl, input.type_ctx);
        let class_with = find_lombok_annotation(&class_annotations, annotations::WITH);

        // Also check for deprecated @Wither
        let class_wither = find_lombok_annotation(&class_annotations, annotations::WITHER);
        let class_annotation = class_with.or(class_wither);

        // Process each field
        for field in explicit_fields {
            // Check for field-level @With or @Wither
            let field_with = find_lombok_annotation(&field.annotations, annotations::WITH);
            let field_wither = find_lombok_annotation(&field.annotations, annotations::WITHER);
            let field_annotation = field_with.or(field_wither);

            if should_generate_with_for_field(field, class_annotation, field_annotation) {
                generate_with_method(
                    field,
                    field_annotation.or(class_annotation),
                    &config,
                    explicit_methods,
                    input.owner_internal,
                    out,
                );
            }
        }
    }
}

/// Extract class-level annotations
fn extract_class_annotations(
    ctx: &JavaContextExtractor,
    decl: Node,
    type_ctx: &SourceTypeCtx,
) -> Vec<crate::index::AnnotationSummary> {
    first_child_of_kind(decl, "modifiers")
        .map(|modifiers| parse_annotations_in_node(ctx, modifiers, type_ctx))
        .unwrap_or_default()
}

/// Check if a with method should be generated for a field
fn should_generate_with_for_field(
    field: &FieldSummary,
    class_level_anno: Option<&crate::index::AnnotationSummary>,
    field_level_anno: Option<&crate::index::AnnotationSummary>,
) -> bool {
    use rust_asm::constants::ACC_STATIC;

    let is_static = (field.access_flags & ACC_STATIC) != 0;

    // Static fields are never processed
    if is_static {
        return false;
    }

    // Field-level annotation takes precedence
    if let Some(anno) = field_level_anno {
        let access = parse_access_level(anno);
        return access != AccessLevel::None;
    }

    // Check class-level annotation
    if let Some(anno) = class_level_anno {
        // Check for AccessLevel.NONE
        let access = parse_access_level(anno);
        return access != AccessLevel::None;
    }

    false
}

/// Generate a with method for a field
fn generate_with_method(
    field: &FieldSummary,
    annotation: Option<&crate::index::AnnotationSummary>,
    config: &LombokConfig,
    explicit_methods: &[MethodSummary],
    owner_internal: Option<&str>,
    out: &mut SyntheticMemberSet,
) {
    let access_level = annotation
        .map(parse_access_level)
        .unwrap_or(AccessLevel::Public);

    if access_level == AccessLevel::None {
        return;
    }

    let with_name = compute_with_name(field.name.as_ref(), config);

    // Build return type descriptor (same as the class type)
    let return_descriptor = if let Some(internal_name) = owner_internal {
        Arc::from(format!("L{};", internal_name))
    } else {
        // Fallback: return Object
        Arc::from("Ljava/lang/Object;")
    };

    let descriptor = Arc::from(format!("({}){}", field.descriptor, return_descriptor));

    // Check if method already exists
    if has_method(explicit_methods, &with_name, &descriptor) {
        return;
    }

    let access_flags = access_level.to_access_flags();

    // Build parameter annotations (copy @NonNull if present)
    let mut param_annotations = vec![];
    if is_field_non_null(field) {
        param_annotations.push(crate::index::AnnotationSummary {
            internal_name: Arc::from(annotations::NON_NULL),
            runtime_visible: true,
            elements: rustc_hash::FxHashMap::default(),
        });
    }

    out.methods.push(MethodSummary {
        name: Arc::from(with_name.clone()),
        params: MethodParams {
            items: vec![MethodParam {
                descriptor: Arc::clone(&field.descriptor),
                name: Arc::clone(&field.name),
                annotations: param_annotations,
            }],
        },
        annotations: vec![],
        access_flags,
        is_synthetic: false,
        generic_signature: None,
        return_type: Some(return_descriptor),
    });

    out.definitions.push(SyntheticDefinition {
        kind: SyntheticDefinitionKind::Method,
        name: Arc::from(with_name),
        descriptor: Some(descriptor),
        origin: SyntheticOrigin::LombokWith {
            field_name: Arc::clone(&field.name),
        },
    });
}

/// Compute with method name for a field
fn compute_with_name(field_name: &str, config: &LombokConfig) -> String {
    let base_name = strip_field_prefix(field_name, config);

    // Capitalize first character
    let capitalized = capitalize_first(base_name);

    format!("with{}", capitalized)
}

/// Capitalize first character of a string
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            let mut result = first.to_uppercase().to_string();
            result.push_str(chars.as_str());
            result
        }
    }
}

/// Check if a method with the given name and descriptor already exists
fn has_method(methods: &[MethodSummary], name: &str, descriptor: &str) -> bool {
    methods
        .iter()
        .any(|method| method.name.as_ref() == name && method.desc().as_ref() == descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ClassOrigin;
    use crate::language::java::class_parser::parse_java_source_with_test_jdk;

    fn parse_class(src: &str) -> crate::index::ClassMetadata {
        parse_java_source_with_test_jdk(
            src,
            ClassOrigin::Unknown,
            &["java/lang/Object", "java/lang/String"],
        )
        .into_iter()
        .find(|class| class.name.as_ref() == "Person")
        .expect("Person class")
    }

    #[test]
    fn test_with_generated_for_field_with_annotation() {
        let src = r#"
            import lombok.With;
            class Person {
                @With
                private final String name;
            }
        "#;
        let synthetic = parse_class(src);

        assert!(
            synthetic
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "withName"
                    && m.params.items.len() == 1
                    && m.params.items[0].descriptor.as_ref() == "Ljava/lang/String;"),
            "Expected withName(String) method to be generated"
        );
    }

    #[test]
    fn test_with_not_generated_for_static_field() {
        let src = r#"
            import lombok.With;
            class Person {
                @With
                private static final String DEFAULT_NAME = "John";
            }
        "#;
        let synthetic = parse_class(src);

        assert!(
            !synthetic
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "withDEFAULT_NAME"),
            "With method should not be generated for static field"
        );
    }

    #[test]
    fn test_with_respects_access_level() {
        let src = r#"
            import lombok.With;
            import lombok.AccessLevel;
            class Person {
                @With(AccessLevel.PROTECTED)
                private final String name;
            }
        "#;
        let synthetic = parse_class(src);

        let with_method = synthetic
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "withName");
        assert!(with_method.is_some(), "withName method should be generated");

        let with_method = with_method.unwrap();
        assert_eq!(
            with_method.access_flags & rust_asm::constants::ACC_PROTECTED,
            rust_asm::constants::ACC_PROTECTED,
            "withName should be protected"
        );
    }

    #[test]
    fn test_compute_with_name() {
        let config = LombokConfig::new();

        assert_eq!(compute_with_name("name", &config), "withName");
        assert_eq!(compute_with_name("age", &config), "withAge");
        assert_eq!(compute_with_name("isActive", &config), "withIsActive");
    }
}
