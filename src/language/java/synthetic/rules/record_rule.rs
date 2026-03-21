use rust_asm::constants::ACC_PUBLIC;
use std::sync::Arc;
use tree_sitter::Node;

use crate::{
    index::{FieldSummary, MethodParam, MethodParams, MethodSummary},
    language::java::{JavaContextExtractor, type_ctx::SourceTypeCtx},
};

use super::super::common::{
    SyntheticDefinition, SyntheticDefinitionKind, SyntheticInput, SyntheticMemberRule,
    SyntheticMemberSet, SyntheticOrigin,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordComponent {
    pub name: Arc<str>,
    pub source_type: Arc<str>,
    pub descriptor: Arc<str>,
}

pub struct RecordRule;

impl SyntheticMemberRule for RecordRule {
    fn synthesize(
        &self,
        input: &SyntheticInput<'_>,
        out: &mut SyntheticMemberSet,
        explicit_methods: &[MethodSummary],
        _explicit_fields: &[FieldSummary],
    ) {
        if input.decl.kind() != "record_declaration" {
            return;
        }

        let components = record_components(input.ctx, input.decl, input.type_ctx);
        if components.is_empty() {
            return;
        }

        let ctor_desc = Arc::from(format!(
            "({})V",
            components
                .iter()
                .map(|component| component.descriptor.as_ref())
                .collect::<String>()
        ));
        if !has_method(explicit_methods, "<init>", &ctor_desc) {
            out.methods.push(MethodSummary {
                name: Arc::from("<init>"),
                params: MethodParams {
                    items: components
                        .iter()
                        .map(|component| MethodParam {
                            descriptor: Arc::clone(&component.descriptor),
                            name: Arc::clone(&component.name),
                            annotations: vec![],
                        })
                        .collect(),
                },
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: None,
            });
            out.definitions.push(SyntheticDefinition {
                kind: SyntheticDefinitionKind::Method,
                name: Arc::from("<init>"),
                descriptor: Some(Arc::clone(&ctor_desc)),
                origin: SyntheticOrigin::RecordCanonicalConstructor,
            });
        }

        for component in components {
            let desc = Arc::from(format!("(){}", component.descriptor));
            if has_method(explicit_methods, component.name.as_ref(), &desc) {
                continue;
            }
            out.methods.push(MethodSummary {
                name: Arc::clone(&component.name),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: Some(Arc::clone(&component.descriptor)),
            });
            out.definitions.push(SyntheticDefinition {
                kind: SyntheticDefinitionKind::Method,
                name: Arc::clone(&component.name),
                descriptor: Some(desc),
                origin: SyntheticOrigin::RecordComponentAccessor {
                    component_name: component.name,
                },
            });
        }
    }
}

pub fn record_components(
    ctx: &JavaContextExtractor,
    decl: Node,
    type_ctx: &SourceTypeCtx,
) -> Vec<RecordComponent> {
    if decl.kind() != "record_declaration" {
        return vec![];
    }

    let Some(params) = record_parameter_node(decl) else {
        return vec![];
    };

    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if !matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let type_text = child
            .child_by_field_name("type")
            .map(|n| ctx.node_text(n).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "java.lang.Object".to_string());
        out.push(RecordComponent {
            name: Arc::from(ctx.node_text(name_node)),
            descriptor: Arc::from(type_ctx.to_descriptor(&type_text)),
            source_type: Arc::from(type_text),
        });
    }
    out
}

pub(crate) fn record_parameter_node(decl: Node) -> Option<Node> {
    decl.child_by_field_name("parameters").or_else(|| {
        let mut cursor = decl.walk();
        decl.children(&mut cursor)
            .find(|child| child.kind() == "formal_parameters")
    })
}

pub(crate) fn find_record_component_node<'a>(
    ctx: &JavaContextExtractor,
    decl: Node<'a>,
    component_name: &str,
) -> Option<Node<'a>> {
    let params = record_parameter_node(decl)?;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if !matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        if ctx.node_text(name_node) == component_name {
            return Some(name_node);
        }
    }
    None
}

fn has_method(methods: &[MethodSummary], name: &str, descriptor: &str) -> bool {
    methods
        .iter()
        .any(|method| method.name.as_ref() == name && method.desc().as_ref() == descriptor)
}

#[cfg(test)]
mod tests {
    use super::has_method;
    use crate::index::ClassOrigin;
    use crate::language::java::class_parser::parse_java_source_with_test_jdk;

    fn parse_record(src: &str) -> crate::index::ClassMetadata {
        parse_java_source_with_test_jdk(
            src,
            ClassOrigin::Unknown,
            &["java/lang/Object", "java/lang/String"],
        )
        .into_iter()
        .find(|class| class.name.as_ref() == "Point")
        .expect("Point record")
    }

    #[test]
    fn record_components_are_recognized() {
        let src = "record Point(int x, String label) {}";
        let record = parse_record(src);
        assert!(has_method(&record.methods, "x", "()I"));
        assert!(has_method(&record.methods, "label", "()Ljava/lang/String;"));
        assert!(has_method(
            &record.methods,
            "<init>",
            "(ILjava/lang/String;)V"
        ));
    }

    #[test]
    fn record_rule_produces_accessors_and_ctor() {
        let src = "record Point(int x, int y) {}";
        let synthetic = parse_record(src);
        assert!(
            synthetic
                .methods
                .iter()
                .any(|method| method.name.as_ref() == "x" && method.desc().as_ref() == "()I")
        );
        assert!(synthetic
            .methods
            .iter()
            .any(|method| method.name.as_ref() == "<init>" && method.desc().as_ref() == "(II)V"));
    }
}
