use std::sync::Arc;
use tree_sitter::Node;
use tree_sitter_utils::traversal::first_child_of_kind;

use crate::{
    index::{AnnotationSummary, FieldSummary, MethodParam, MethodParams, MethodSummary},
    language::java::{
        JavaContextExtractor,
        lombok::{
            config::LombokConfig,
            types::{AccessLevel, LombokConstructorType, annotations},
            utils::{find_lombok_annotation, get_bool_param, get_string_param},
        },
        members::parse_annotations_in_node,
        synthetic::{
            SyntheticDefinition, SyntheticDefinitionKind, SyntheticInput, SyntheticMemberRule,
            SyntheticMemberSet, SyntheticOrigin,
        },
        type_ctx::SourceTypeCtx,
    },
};

pub struct ConstructorRule;

impl SyntheticMemberRule for ConstructorRule {
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
            "class_declaration" | "enum_declaration" | "record_declaration"
        ) {
            return;
        }

        let is_enum = input.decl.kind() == "enum_declaration";

        // Load Lombok configuration
        let _config = LombokConfig::new();

        // Extract class-level annotations
        let class_annotations = extract_class_annotations(input.ctx, input.decl, input.type_ctx);

        // Check for @NoArgsConstructor
        if let Some(anno) =
            find_lombok_annotation(&class_annotations, annotations::NO_ARGS_CONSTRUCTOR)
        {
            generate_no_args_constructor(anno, explicit_methods, explicit_fields, is_enum, out);
        }

        // Check for @RequiredArgsConstructor
        if let Some(anno) =
            find_lombok_annotation(&class_annotations, annotations::REQUIRED_ARGS_CONSTRUCTOR)
        {
            generate_required_args_constructor(
                anno,
                explicit_methods,
                explicit_fields,
                is_enum,
                out,
            );
        }

        // Check for @AllArgsConstructor
        if let Some(anno) =
            find_lombok_annotation(&class_annotations, annotations::ALL_ARGS_CONSTRUCTOR)
        {
            generate_all_args_constructor(anno, explicit_methods, explicit_fields, is_enum, out);
        }
    }
}

/// Extract class-level annotations
fn extract_class_annotations(
    ctx: &JavaContextExtractor,
    decl: Node,
    type_ctx: &SourceTypeCtx,
) -> Vec<AnnotationSummary> {
    first_child_of_kind(decl, "modifiers")
        .map(|modifiers| parse_annotations_in_node(ctx, modifiers, type_ctx))
        .unwrap_or_default()
}

/// Generate no-args constructor
fn generate_no_args_constructor(
    annotation: &AnnotationSummary,
    explicit_methods: &[MethodSummary],
    explicit_fields: &[FieldSummary],
    is_enum: bool,
    out: &mut SyntheticMemberSet,
) {
    // Parse parameters
    let force = get_bool_param(annotation, "force", false);
    let access = parse_constructor_access_level(annotation);
    let static_name = get_string_param(annotation, "staticName");

    // Enums always have private constructors
    let access = if is_enum {
        AccessLevel::Private
    } else {
        access
    };

    // Check if constructor already exists
    let descriptor = "()V";
    if constructor_exists(explicit_methods, descriptor) {
        return;
    }

    // If there are final fields and force is not true, we can't generate
    // (In real Lombok, this would be a compile error, but we'll just skip)
    if !force && has_uninitialized_final_fields(explicit_fields) {
        // Skip generation - would cause compile error
        return;
    }

    let access_flags = access.to_access_flags();

    // Generate the constructor
    let constructor = MethodSummary {
        name: Arc::from("<init>"),
        params: MethodParams::empty(),
        annotations: Vec::new(),
        access_flags,
        is_synthetic: false,
        generic_signature: None,
        return_type: None,
    };

    out.methods.push(constructor);

    // Add synthetic definition for go-to-definition
    out.definitions.push(SyntheticDefinition {
        kind: SyntheticDefinitionKind::Method,
        name: Arc::from("<init>"),
        descriptor: Some(Arc::from(descriptor)),
        origin: SyntheticOrigin::LombokConstructor {
            constructor_type: LombokConstructorType::NoArgs,
        },
    });

    // Generate static factory method if staticName is specified
    if let Some(static_name) = static_name {
        if !static_name.is_empty() {
            generate_static_factory_method(
                &static_name,
                &MethodParams::empty(),
                None, // No class name needed for return type inference
                explicit_methods,
                out,
                LombokConstructorType::NoArgs,
            );
        }
    }
}

/// Generate required-args constructor (final fields + @NonNull fields)
fn generate_required_args_constructor(
    annotation: &AnnotationSummary,
    explicit_methods: &[MethodSummary],
    explicit_fields: &[FieldSummary],
    is_enum: bool,
    out: &mut SyntheticMemberSet,
) {
    let access = parse_constructor_access_level(annotation);
    let static_name = get_string_param(annotation, "staticName");

    // Enums always have private constructors
    let access = if is_enum {
        AccessLevel::Private
    } else {
        access
    };

    // Get required fields (final + @NonNull, non-static)
    let required_fields = get_required_fields(explicit_fields);

    // Build parameters
    let params = build_constructor_params(&required_fields);

    // Build descriptor
    let descriptor = build_constructor_descriptor(&params);

    // Check if constructor already exists
    if constructor_exists(explicit_methods, &descriptor) {
        return;
    }

    let access_flags = access.to_access_flags();

    // Generate the constructor
    let constructor = MethodSummary {
        name: Arc::from("<init>"),
        params,
        annotations: Vec::new(),
        access_flags,
        is_synthetic: false,
        generic_signature: None,
        return_type: None,
    };

    out.methods.push(constructor.clone());

    // Add synthetic definition
    out.definitions.push(SyntheticDefinition {
        kind: SyntheticDefinitionKind::Method,
        name: Arc::from("<init>"),
        descriptor: Some(Arc::from(descriptor.as_str())),
        origin: SyntheticOrigin::LombokConstructor {
            constructor_type: LombokConstructorType::RequiredArgs,
        },
    });

    // Generate static factory method if staticName is specified
    if let Some(static_name) = static_name {
        if !static_name.is_empty() {
            generate_static_factory_method(
                &static_name,
                &constructor.params,
                None,
                explicit_methods,
                out,
                LombokConstructorType::RequiredArgs,
            );
        }
    }
}

/// Generate all-args constructor (all non-static fields)
fn generate_all_args_constructor(
    annotation: &AnnotationSummary,
    explicit_methods: &[MethodSummary],
    explicit_fields: &[FieldSummary],
    is_enum: bool,
    out: &mut SyntheticMemberSet,
) {
    let access = parse_constructor_access_level(annotation);
    let static_name = get_string_param(annotation, "staticName");

    // Enums always have private constructors
    let access = if is_enum {
        AccessLevel::Private
    } else {
        access
    };

    // Get all non-static fields
    let all_fields = get_all_non_static_fields(explicit_fields);

    // Build parameters
    let params = build_constructor_params(&all_fields);

    // Build descriptor
    let descriptor = build_constructor_descriptor(&params);

    // Check if constructor already exists
    if constructor_exists(explicit_methods, &descriptor) {
        return;
    }

    let access_flags = access.to_access_flags();

    // Generate the constructor
    let constructor = MethodSummary {
        name: Arc::from("<init>"),
        params,
        annotations: Vec::new(),
        access_flags,
        is_synthetic: false,
        generic_signature: None,
        return_type: None,
    };

    out.methods.push(constructor.clone());

    // Add synthetic definition
    out.definitions.push(SyntheticDefinition {
        kind: SyntheticDefinitionKind::Method,
        name: Arc::from("<init>"),
        descriptor: Some(Arc::from(descriptor.as_str())),
        origin: SyntheticOrigin::LombokConstructor {
            constructor_type: LombokConstructorType::AllArgs,
        },
    });

    // Generate static factory method if staticName is specified
    if let Some(static_name) = static_name {
        if !static_name.is_empty() {
            generate_static_factory_method(
                &static_name,
                &constructor.params,
                None,
                explicit_methods,
                out,
                LombokConstructorType::AllArgs,
            );
        }
    }
}

/// Generate static factory method wrapper
fn generate_static_factory_method(
    static_name: &str,
    params: &MethodParams,
    _class_internal_name: Option<&str>,
    explicit_methods: &[MethodSummary],
    out: &mut SyntheticMemberSet,
    constructor_type: LombokConstructorType,
) {
    use rust_asm::constants::{ACC_PUBLIC, ACC_STATIC};

    // Build descriptor for the static method
    let mut descriptor = String::from("(");
    for param in &params.items {
        descriptor.push_str(&param.descriptor);
    }
    // Return type is the class itself - we'll use Object as placeholder
    // In real usage, the type resolver will infer the correct return type
    descriptor.push_str(")Ljava/lang/Object;");

    // Check if method already exists
    if explicit_methods
        .iter()
        .any(|m| m.name.as_ref() == static_name && m.params.len() == params.len())
    {
        return;
    }

    // Generate the static factory method
    let factory_method = MethodSummary {
        name: Arc::from(static_name),
        params: params.clone(),
        annotations: Vec::new(),
        access_flags: ACC_PUBLIC | ACC_STATIC,
        is_synthetic: false,
        generic_signature: None,
        return_type: Some(Arc::from("java/lang/Object")), // Placeholder
    };

    out.methods.push(factory_method);

    // Add synthetic definition
    out.definitions.push(SyntheticDefinition {
        kind: SyntheticDefinitionKind::Method,
        name: Arc::from(static_name),
        descriptor: Some(Arc::from(descriptor.as_str())),
        origin: SyntheticOrigin::LombokConstructor { constructor_type },
    });
}

/// Check if a constructor with the given descriptor already exists
fn constructor_exists(explicit_methods: &[MethodSummary], descriptor: &str) -> bool {
    explicit_methods
        .iter()
        .any(|m| m.name.as_ref() == "<init>" && m.desc().as_ref() == descriptor)
}

/// Get required fields for @RequiredArgsConstructor
/// Includes: non-initialized final fields + @NonNull fields
fn get_required_fields(fields: &[FieldSummary]) -> Vec<&FieldSummary> {
    use rust_asm::constants::{ACC_FINAL, ACC_STATIC};

    fields
        .iter()
        .filter(|f| {
            // Skip static fields
            if (f.access_flags & ACC_STATIC) != 0 {
                return false;
            }

            // Include final fields (we assume they're not initialized in field declaration)
            let is_final = (f.access_flags & ACC_FINAL) != 0;

            // Include @NonNull fields
            let has_nonnull = has_nonnull_annotation(f);

            is_final || has_nonnull
        })
        .collect()
}

/// Get all non-static fields for @AllArgsConstructor
fn get_all_non_static_fields(fields: &[FieldSummary]) -> Vec<&FieldSummary> {
    use rust_asm::constants::ACC_STATIC;

    fields
        .iter()
        .filter(|f| (f.access_flags & ACC_STATIC) == 0)
        .collect()
}

/// Check if field has @NonNull annotation
fn has_nonnull_annotation(field: &FieldSummary) -> bool {
    field.annotations.iter().any(|a| {
        let name = a.internal_name.as_ref();
        // Check for various @NonNull annotations
        name == "lombok/NonNull"
            || name == "NonNull"
            || name.ends_with("/NonNull")
            || name.ends_with("/NotNull")
            || name == "NotNull"
            || name.ends_with("/Nonnull")
            || name == "Nonnull"
    })
}

/// Check if there are uninitialized final fields
fn has_uninitialized_final_fields(fields: &[FieldSummary]) -> bool {
    use rust_asm::constants::{ACC_FINAL, ACC_STATIC};

    fields.iter().any(|f| {
        let is_final = (f.access_flags & ACC_FINAL) != 0;
        let is_static = (f.access_flags & ACC_STATIC) != 0;
        // Assume all non-static final fields are uninitialized
        // (detecting initialization would require AST analysis)
        is_final && !is_static
    })
}

/// Build constructor parameters from fields
fn build_constructor_params(fields: &[&FieldSummary]) -> MethodParams {
    let items = fields
        .iter()
        .map(|f| MethodParam {
            descriptor: f.descriptor.clone(),
            name: f.name.clone(),
            annotations: f
                .annotations
                .iter()
                .filter(|a| is_copyable_annotation(a))
                .cloned()
                .collect(),
        })
        .collect();

    MethodParams { items }
}

/// Check if annotation should be copied to constructor parameter
fn is_copyable_annotation(annotation: &AnnotationSummary) -> bool {
    let name = annotation.internal_name.as_ref();
    // Copy @NonNull and similar annotations
    name.contains("NonNull")
        || name.contains("NotNull")
        || name.contains("Nonnull")
        || name.contains("Nullable")
}

/// Build constructor descriptor from parameters
fn build_constructor_descriptor(params: &MethodParams) -> String {
    let mut descriptor = String::from("(");
    for param in &params.items {
        descriptor.push_str(&param.descriptor);
    }
    descriptor.push_str(")V");
    descriptor
}

/// Parse AccessLevel from constructor annotation (uses 'access' parameter, not 'value')
fn parse_constructor_access_level(annotation: &AnnotationSummary) -> AccessLevel {
    // Constructor annotations use 'access' parameter
    if let Some(value) = annotation.elements.get("access") {
        if let Some(level) = AccessLevel::from_annotation_value(value) {
            return level;
        }
    }

    // Fall back to 'value' parameter for compatibility
    if let Some(value) = annotation.elements.get("value") {
        if let Some(level) = AccessLevel::from_annotation_value(value) {
            return level;
        }
    }

    // Default to PUBLIC
    AccessLevel::Public
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_asm::constants::{ACC_FINAL, ACC_PRIVATE, ACC_PUBLIC, ACC_STATIC};

    fn create_test_field(name: &str, descriptor: &str, access_flags: u16) -> FieldSummary {
        FieldSummary {
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            access_flags,
            annotations: Vec::new(),
            is_synthetic: false,
            generic_signature: None,
        }
    }

    fn create_test_field_with_annotation(
        name: &str,
        descriptor: &str,
        access_flags: u16,
        annotation: &str,
    ) -> FieldSummary {
        FieldSummary {
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            access_flags,
            annotations: vec![AnnotationSummary {
                internal_name: Arc::from(annotation),
                runtime_visible: true,
                elements: Default::default(),
            }],
            is_synthetic: false,
            generic_signature: None,
        }
    }

    #[test]
    fn test_get_required_fields_final_only() {
        let fields = vec![
            create_test_field("name", "Ljava/lang/String;", ACC_PRIVATE | ACC_FINAL),
            create_test_field("age", "I", ACC_PRIVATE),
        ];

        let required = get_required_fields(&fields);
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].name.as_ref(), "name");
    }

    #[test]
    fn test_get_required_fields_nonnull() {
        let fields = vec![
            create_test_field_with_annotation(
                "name",
                "Ljava/lang/String;",
                ACC_PRIVATE,
                "lombok/NonNull",
            ),
            create_test_field("age", "I", ACC_PRIVATE),
        ];

        let required = get_required_fields(&fields);
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].name.as_ref(), "name");
    }

    #[test]
    fn test_get_required_fields_skips_static() {
        let fields = vec![
            create_test_field(
                "CONSTANT",
                "Ljava/lang/String;",
                ACC_PUBLIC | ACC_STATIC | ACC_FINAL,
            ),
            create_test_field("name", "Ljava/lang/String;", ACC_PRIVATE | ACC_FINAL),
        ];

        let required = get_required_fields(&fields);
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].name.as_ref(), "name");
    }

    #[test]
    fn test_get_all_non_static_fields() {
        let fields = vec![
            create_test_field("CONSTANT", "I", ACC_PUBLIC | ACC_STATIC | ACC_FINAL),
            create_test_field("name", "Ljava/lang/String;", ACC_PRIVATE),
            create_test_field("age", "I", ACC_PRIVATE),
        ];

        let all = get_all_non_static_fields(&fields);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name.as_ref(), "name");
        assert_eq!(all[1].name.as_ref(), "age");
    }

    #[test]
    fn test_build_constructor_descriptor() {
        let params = MethodParams {
            items: vec![
                MethodParam {
                    descriptor: Arc::from("Ljava/lang/String;"),
                    name: Arc::from("name"),
                    annotations: Vec::new(),
                },
                MethodParam {
                    descriptor: Arc::from("I"),
                    name: Arc::from("age"),
                    annotations: Vec::new(),
                },
            ],
        };

        let descriptor = build_constructor_descriptor(&params);
        assert_eq!(descriptor, "(Ljava/lang/String;I)V");
    }

    #[test]
    fn test_has_nonnull_annotation() {
        let field = create_test_field_with_annotation(
            "name",
            "Ljava/lang/String;",
            ACC_PRIVATE,
            "lombok/NonNull",
        );
        assert!(has_nonnull_annotation(&field));

        let field2 = create_test_field("age", "I", ACC_PRIVATE);
        assert!(!has_nonnull_annotation(&field2));
    }
}
