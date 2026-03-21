use std::sync::Arc;
use tree_sitter::Node;
use tree_sitter_utils::traversal::first_child_of_kind;

use crate::{
    index::FieldSummary,
    language::java::{
        JavaContextExtractor,
        lombok::{
            config::LombokConfig,
            types::annotations,
            utils::{find_lombok_annotation, get_string_param},
        },
        members::parse_annotations_in_node,
        synthetic::{
            SyntheticDefinition, SyntheticDefinitionKind, SyntheticInput, SyntheticMemberRule,
            SyntheticMemberSet, SyntheticOrigin,
        },
        type_ctx::SourceTypeCtx,
    },
};

pub struct LogRule;

/// Configuration for a specific log framework
struct LogFramework {
    annotation: &'static str,
    logger_type: &'static str,
    factory_method: LogFactoryMethod,
}

enum LogFactoryMethod {
    /// LoggerFactory.getLogger(Class)
    GetLogger(&'static str),
    /// Logger.getLogger(String) - uses class name
    GetLoggerByName(&'static str),
    /// FluentLogger.forEnclosingClass()
    ForEnclosingClass(&'static str),
}

impl LogRule {
    /// Get all supported log frameworks
    fn frameworks() -> &'static [LogFramework] {
        &[
            LogFramework {
                annotation: annotations::SLF4J,
                logger_type: "Lorg/slf4j/Logger;",
                factory_method: LogFactoryMethod::GetLogger("org/slf4j/LoggerFactory"),
            },
            LogFramework {
                annotation: annotations::LOG,
                logger_type: "Ljava/util/logging/Logger;",
                factory_method: LogFactoryMethod::GetLoggerByName("java/util/logging/Logger"),
            },
            LogFramework {
                annotation: annotations::LOG4J,
                logger_type: "Lorg/apache/log4j/Logger;",
                factory_method: LogFactoryMethod::GetLogger("org/apache/log4j/Logger"),
            },
            LogFramework {
                annotation: annotations::LOG4J2,
                logger_type: "Lorg/apache/logging/log4j/Logger;",
                factory_method: LogFactoryMethod::GetLogger("org/apache/logging/log4j/LogManager"),
            },
            LogFramework {
                annotation: annotations::COMMONS_LOG,
                logger_type: "Lorg/apache/commons/logging/Log;",
                factory_method: LogFactoryMethod::GetLogger(
                    "org/apache/commons/logging/LogFactory",
                ),
            },
            LogFramework {
                annotation: annotations::JBOSS_LOG,
                logger_type: "Lorg/jboss/logging/Logger;",
                factory_method: LogFactoryMethod::GetLogger("org/jboss/logging/Logger"),
            },
            LogFramework {
                annotation: annotations::FLOGGER,
                logger_type: "Lcom/google/common/flogger/FluentLogger;",
                factory_method: LogFactoryMethod::ForEnclosingClass(
                    "com/google/common/flogger/FluentLogger",
                ),
            },
            LogFramework {
                annotation: annotations::XSLF4J,
                logger_type: "Lorg/slf4j/ext/XLogger;",
                factory_method: LogFactoryMethod::GetLogger("org/slf4j/ext/XLoggerFactory"),
            },
        ]
    }

    /// Find which log framework annotation is present
    fn find_log_annotation(
        annotations: &[crate::index::AnnotationSummary],
    ) -> Option<(&crate::index::AnnotationSummary, &'static LogFramework)> {
        for framework in Self::frameworks() {
            if let Some(anno) = find_lombok_annotation(annotations, framework.annotation) {
                return Some((anno, framework));
            }
        }
        None
    }
}

impl SyntheticMemberRule for LogRule {
    fn synthesize(
        &self,
        input: &SyntheticInput<'_>,
        out: &mut SyntheticMemberSet,
        _explicit_methods: &[crate::index::MethodSummary],
        explicit_fields: &[FieldSummary],
    ) {
        // Only process class-like declarations
        if !matches!(
            input.decl.kind(),
            "class_declaration" | "enum_declaration" | "record_declaration"
        ) {
            return;
        }

        // Load Lombok configuration
        let config = LombokConfig::new();

        // Get configured field name (default: "log")
        let field_name = config.get("lombok.log.fieldName").unwrap_or("log");

        // Check if field already exists
        if explicit_fields
            .iter()
            .any(|f| f.name.as_ref() == field_name)
        {
            return;
        }

        // Check for class-level log annotations
        let class_annotations = extract_class_annotations(input.ctx, input.decl, input.type_ctx);

        if let Some((annotation, framework)) = LogRule::find_log_annotation(&class_annotations) {
            generate_log_field(framework, annotation, field_name, &config, out);
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

/// Generate a logger field
fn generate_log_field(
    framework: &LogFramework,
    annotation: &crate::index::AnnotationSummary,
    field_name: &str,
    config: &LombokConfig,
    out: &mut SyntheticMemberSet,
) {
    use rust_asm::constants::{ACC_FINAL, ACC_PRIVATE, ACC_STATIC};

    // Check if field should be static (default: true)
    let is_static = config
        .get("lombok.log.fieldIsStatic")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);

    // Get custom topic if specified
    let _topic = get_string_param(annotation, "topic");

    // Build access flags
    let mut access_flags = ACC_PRIVATE | ACC_FINAL;
    if is_static {
        access_flags |= ACC_STATIC;
    }

    // Generate the logger field
    out.fields.push(FieldSummary {
        name: Arc::from(field_name),
        descriptor: Arc::from(framework.logger_type),
        access_flags,
        annotations: vec![],
        is_synthetic: false,
        generic_signature: None,
    });

    out.definitions.push(SyntheticDefinition {
        kind: SyntheticDefinitionKind::Field,
        name: Arc::from(field_name),
        descriptor: Some(Arc::from(framework.logger_type)),
        origin: SyntheticOrigin::LombokLog,
    });
}

#[cfg(test)]
mod tests {
    use crate::index::ClassOrigin;
    use crate::language::java::class_parser::parse_java_source_with_test_jdk;
    use rust_asm::constants::{ACC_FINAL, ACC_PRIVATE, ACC_STATIC};

    fn parse_class(src: &str) -> crate::index::ClassMetadata {
        parse_java_source_with_test_jdk(
            src,
            ClassOrigin::Unknown,
            &["java/lang/Object", "java/lang/String"],
        )
        .into_iter()
        .find(|class| class.name.as_ref() == "MyService")
        .expect("MyService class")
    }

    #[test]
    fn test_slf4j_generates_log_field() {
        let src = r#"
            import lombok.extern.slf4j.Slf4j;
            @Slf4j
            class MyService {
            }
        "#;
        let class = parse_class(src);
        let log_field = class.fields.iter().find(|f| f.name.as_ref() == "log");
        assert!(log_field.is_some(), "Should generate log field");

        let log_field = log_field.unwrap();
        assert_eq!(
            log_field.descriptor.as_ref(),
            "Lorg/slf4j/Logger;",
            "Should be slf4j Logger type"
        );
        assert_eq!(
            log_field.access_flags & ACC_STATIC,
            ACC_STATIC,
            "Should be static"
        );
        assert_eq!(
            log_field.access_flags & ACC_FINAL,
            ACC_FINAL,
            "Should be final"
        );
        assert_eq!(
            log_field.access_flags & ACC_PRIVATE,
            ACC_PRIVATE,
            "Should be private"
        );
    }

    #[test]
    fn test_log4j2_generates_log_field() {
        let src = r#"
            import lombok.extern.log4j.Log4j2;
            @Log4j2
            class MyService {
            }
        "#;
        let class = parse_class(src);
        let log_field = class.fields.iter().find(|f| f.name.as_ref() == "log");
        assert!(log_field.is_some(), "Should generate log field");

        let log_field = log_field.unwrap();
        assert_eq!(
            log_field.descriptor.as_ref(),
            "Lorg/apache/logging/log4j/Logger;",
            "Should be log4j2 Logger type"
        );
    }

    #[test]
    fn test_java_util_logging_generates_log_field() {
        let src = r#"
            import lombok.extern.java.Log;
            @Log
            class MyService {
            }
        "#;
        let class = parse_class(src);
        let log_field = class.fields.iter().find(|f| f.name.as_ref() == "log");
        assert!(log_field.is_some(), "Should generate log field");

        let log_field = log_field.unwrap();
        assert_eq!(
            log_field.descriptor.as_ref(),
            "Ljava/util/logging/Logger;",
            "Should be java.util.logging Logger type"
        );
    }

    #[test]
    fn test_commons_log_generates_log_field() {
        let src = r#"
            import lombok.extern.apachecommons.CommonsLog;
            @CommonsLog
            class MyService {
            }
        "#;
        let class = parse_class(src);
        let log_field = class.fields.iter().find(|f| f.name.as_ref() == "log");
        assert!(log_field.is_some(), "Should generate log field");

        let log_field = log_field.unwrap();
        assert_eq!(
            log_field.descriptor.as_ref(),
            "Lorg/apache/commons/logging/Log;",
            "Should be commons logging Log type"
        );
    }

    #[test]
    fn test_log_field_not_generated_if_exists() {
        let src = r#"
            import lombok.extern.slf4j.Slf4j;
            @Slf4j
            class MyService {
                private static final org.slf4j.Logger log = null;
            }
        "#;
        let class = parse_class(src);
        assert_eq!(
            class
                .fields
                .iter()
                .filter(|f| f.name.as_ref() == "log")
                .count(),
            1,
            "Should preserve the explicit log field without generating a duplicate"
        );
    }

    #[test]
    fn test_flogger_generates_log_field() {
        let src = r#"
            import lombok.extern.flogger.Flogger;
            @Flogger
            class MyService {
            }
        "#;
        let class = parse_class(src);
        let log_field = class.fields.iter().find(|f| f.name.as_ref() == "log");
        assert!(log_field.is_some(), "Should generate log field");

        let log_field = log_field.unwrap();
        assert_eq!(
            log_field.descriptor.as_ref(),
            "Lcom/google/common/flogger/FluentLogger;",
            "Should be Flogger FluentLogger type"
        );
    }

    #[test]
    fn test_jboss_log_generates_log_field() {
        let src = r#"
            import lombok.extern.jbosslog.JBossLog;
            @JBossLog
            class MyService {
            }
        "#;
        let class = parse_class(src);
        let log_field = class.fields.iter().find(|f| f.name.as_ref() == "log");
        assert!(log_field.is_some(), "Should generate log field");

        let log_field = log_field.unwrap();
        assert_eq!(
            log_field.descriptor.as_ref(),
            "Lorg/jboss/logging/Logger;",
            "Should be JBoss Logger type"
        );
    }

    #[test]
    fn test_xslf4j_generates_log_field() {
        let src = r#"
            import lombok.extern.slf4j.XSlf4j;
            @XSlf4j
            class MyService {
            }
        "#;
        let class = parse_class(src);
        let log_field = class.fields.iter().find(|f| f.name.as_ref() == "log");
        assert!(log_field.is_some(), "Should generate log field");

        let log_field = log_field.unwrap();
        assert_eq!(
            log_field.descriptor.as_ref(),
            "Lorg/slf4j/ext/XLogger;",
            "Should be XSlf4j XLogger type"
        );
    }
}
