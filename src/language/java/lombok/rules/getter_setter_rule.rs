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
                compute_getter_name, compute_setter_name, find_lombok_annotation, get_bool_param,
                is_field_final, parse_access_level, should_generate_for_field,
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

pub struct GetterSetterRule;

impl SyntheticMemberRule for GetterSetterRule {
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
            "class_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "interface_declaration"
        ) {
            return;
        }

        // Load Lombok configuration (for now, use defaults)
        let config = LombokConfig::new();

        // Check for class-level @Getter and @Setter annotations
        let class_annotations = extract_class_annotations(input.ctx, input.decl, input.type_ctx);
        let class_getter = find_lombok_annotation(&class_annotations, annotations::GETTER);
        let class_setter = find_lombok_annotation(&class_annotations, annotations::SETTER);

        // Process each field
        for field in explicit_fields {
            // Check for field-level @Getter
            let field_getter = find_lombok_annotation(&field.annotations, annotations::GETTER);

            if should_generate_for_field(field, class_getter, field_getter) {
                generate_getter(
                    field,
                    field_getter.or(class_getter),
                    &config,
                    explicit_methods,
                    out,
                );
            }

            // Check for field-level @Setter
            let field_setter = find_lombok_annotation(&field.annotations, annotations::SETTER);

            // Don't generate setter for final fields
            if !is_field_final(field)
                && should_generate_for_field(field, class_setter, field_setter)
            {
                generate_setter(
                    field,
                    field_setter.or(class_setter),
                    &config,
                    explicit_methods,
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

/// Generate a getter method for a field
fn generate_getter(
    field: &FieldSummary,
    annotation: Option<&crate::index::AnnotationSummary>,
    config: &LombokConfig,
    explicit_methods: &[MethodSummary],
    out: &mut SyntheticMemberSet,
) {
    let access_level = annotation
        .map(parse_access_level)
        .unwrap_or(AccessLevel::Public);

    if access_level == AccessLevel::None {
        return;
    }

    let getter_name = compute_getter_name(field.name.as_ref(), field.descriptor.as_ref(), config);

    let descriptor = Arc::from(format!("(){}", field.descriptor));

    // Check if method already exists
    if has_method(explicit_methods, &getter_name, &descriptor) {
        return;
    }

    // Check for lazy getter
    let is_lazy = annotation
        .map(|a| get_bool_param(a, "lazy", false))
        .unwrap_or(false);

    if is_lazy && !is_field_final(field) {
        // Lazy getters require final fields
        return;
    }

    // Determine if the getter should be static (if the field is static)
    let is_static = (field.access_flags & rust_asm::constants::ACC_STATIC) != 0;
    let mut access_flags = access_level.to_access_flags();
    if is_static {
        access_flags |= rust_asm::constants::ACC_STATIC;
    }

    out.methods.push(MethodSummary {
        name: Arc::from(getter_name.clone()),
        params: MethodParams::empty(),
        annotations: vec![], // TODO: Copy annotations based on onMethod parameter
        access_flags,
        is_synthetic: false,
        generic_signature: None,
        return_type: Some(Arc::clone(&field.descriptor)),
    });

    out.definitions.push(SyntheticDefinition {
        kind: SyntheticDefinitionKind::Method,
        name: Arc::from(getter_name),
        descriptor: Some(descriptor),
        origin: SyntheticOrigin::LombokGetter {
            field_name: Arc::clone(&field.name),
        },
    });
}

/// Generate a setter method for a field
fn generate_setter(
    field: &FieldSummary,
    annotation: Option<&crate::index::AnnotationSummary>,
    config: &LombokConfig,
    explicit_methods: &[MethodSummary],
    out: &mut SyntheticMemberSet,
) {
    let access_level = annotation
        .map(parse_access_level)
        .unwrap_or(AccessLevel::Public);

    if access_level == AccessLevel::None {
        return;
    }

    let setter_name = compute_setter_name(field.name.as_ref(), config);
    let descriptor = Arc::from(format!("({})V", field.descriptor));

    // Check if method already exists
    if has_method(explicit_methods, &setter_name, &descriptor) {
        return;
    }

    // Determine return type based on chain configuration
    let return_type = if config.accessors_chain() {
        // Fluent setters return 'this' - but we don't know the class type here
        // For now, return void; proper implementation needs owner type
        None
    } else {
        None // void
    };

    // Determine if the setter should be static (if the field is static)
    let is_static = (field.access_flags & rust_asm::constants::ACC_STATIC) != 0;
    let mut access_flags = access_level.to_access_flags();
    if is_static {
        access_flags |= rust_asm::constants::ACC_STATIC;
    }

    out.methods.push(MethodSummary {
        name: Arc::from(setter_name.clone()),
        params: MethodParams {
            items: vec![MethodParam {
                descriptor: Arc::clone(&field.descriptor),
                name: Arc::clone(&field.name),
                annotations: vec![], // TODO: Copy annotations based on onParam parameter
            }],
        },
        annotations: vec![],
        access_flags,
        is_synthetic: false,
        generic_signature: None,
        return_type,
    });

    out.definitions.push(SyntheticDefinition {
        kind: SyntheticDefinitionKind::Method,
        name: Arc::from(setter_name),
        descriptor: Some(descriptor),
        origin: SyntheticOrigin::LombokSetter {
            field_name: Arc::clone(&field.name),
        },
    });
}

/// Check if a method with the given name and descriptor already exists
fn has_method(methods: &[MethodSummary], name: &str, descriptor: &str) -> bool {
    methods
        .iter()
        .any(|method| method.name.as_ref() == name && method.desc().as_ref() == descriptor)
}

#[cfg(test)]
mod tests {
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
    fn test_getter_generated_for_field_with_annotation() {
        let src = r#"
            import lombok.Getter;
            class Person {
                @Getter
                private String name;
            }
        "#;
        let synthetic = parse_class(src);

        assert!(
            synthetic.methods.iter().any(
                |m| m.name.as_ref() == "getName" && m.desc().as_ref() == "()Ljava/lang/String;"
            ),
            "Expected getName() method to be generated"
        );
    }

    #[test]
    fn test_setter_generated_for_non_final_field() {
        let src = r#"
            import lombok.Setter;
            class Person {
                @Setter
                private String name;
            }
        "#;
        let synthetic = parse_class(src);

        assert!(
            synthetic
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setName"
                    && m.desc().as_ref() == "(Ljava/lang/String;)V"),
            "Expected setName(String) method to be generated"
        );
    }

    #[test]
    fn test_setter_not_generated_for_final_field() {
        let src = r#"
            import lombok.Setter;
            class Person {
                @Setter
                private final String name = "John";
            }
        "#;
        let synthetic = parse_class(src);

        assert!(
            !synthetic
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setName"),
            "Setter should not be generated for final field"
        );
    }

    #[test]
    fn test_boolean_field_uses_is_prefix() {
        let src = r#"
            import lombok.Getter;
            class Person {
                @Getter
                private boolean active;
            }
        "#;
        let synthetic = parse_class(src);

        assert!(
            synthetic
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "isActive" && m.desc().as_ref() == "()Z"),
            "Expected isActive() method for boolean field"
        );
    }
}
