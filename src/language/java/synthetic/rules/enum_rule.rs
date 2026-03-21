use rust_asm::constants::{ACC_FINAL, ACC_PUBLIC, ACC_STATIC};
use std::sync::Arc;
use tree_sitter::Node;

use crate::{
    index::{FieldSummary, MethodSummary},
    language::java::JavaContextExtractor,
};

use super::super::common::{
    SyntheticDefinition, SyntheticDefinitionKind, SyntheticInput, SyntheticMemberRule,
    SyntheticMemberSet, SyntheticOrigin,
};

pub struct EnumRule;

impl SyntheticMemberRule for EnumRule {
    fn synthesize(
        &self,
        input: &SyntheticInput<'_>,
        out: &mut SyntheticMemberSet,
        _explicit_methods: &[MethodSummary],
        explicit_fields: &[FieldSummary],
    ) {
        if input.decl.kind() != "enum_declaration" {
            return;
        }

        let Some(owner_internal) = input.owner_internal else {
            return;
        };
        let owner_descriptor: Arc<str> = Arc::from(format!("L{};", owner_internal));
        for const_name in enum_constant_names(input.ctx, input.decl) {
            if explicit_fields.iter().any(|field| field.name == const_name) {
                continue;
            }
            out.fields.push(FieldSummary {
                name: Arc::clone(&const_name),
                descriptor: Arc::clone(&owner_descriptor),
                access_flags: ACC_PUBLIC | ACC_STATIC | ACC_FINAL,
                annotations: vec![],
                is_synthetic: false,
                generic_signature: None,
            });
            out.definitions.push(SyntheticDefinition {
                kind: SyntheticDefinitionKind::Field,
                name: Arc::clone(&const_name),
                descriptor: None,
                origin: SyntheticOrigin::EnumConstant {
                    constant_name: const_name,
                },
            });
        }
    }
}

pub fn enum_constant_names(ctx: &JavaContextExtractor, decl: Node) -> Vec<Arc<str>> {
    if decl.kind() != "enum_declaration" {
        return vec![];
    }
    let Some(body) = decl.child_by_field_name("body") else {
        return vec![];
    };
    let mut out = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if !matches!(child.kind(), "enum_constant" | "enum_constant_declaration") {
            continue;
        }
        if let Some(name_node) = enum_constant_name_node(child) {
            out.push(Arc::from(ctx.node_text(name_node)));
        }
    }
    out
}

pub(crate) fn find_enum_constant_node<'a>(
    ctx: &JavaContextExtractor,
    decl: Node<'a>,
    constant_name: &str,
) -> Option<Node<'a>> {
    let body = decl.child_by_field_name("body")?;
    let mut cursor = body.walk();
    body.named_children(&mut cursor).find_map(|child| {
        if !matches!(child.kind(), "enum_constant" | "enum_constant_declaration") {
            return None;
        }
        let name_node = enum_constant_name_node(child)?;
        (ctx.node_text(name_node) == constant_name).then_some(name_node)
    })
}

fn enum_constant_name_node(node: Node) -> Option<Node> {
    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("identifier"))
        .or_else(|| {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find(|child| child.kind() == "identifier")
        })
}

#[cfg(test)]
mod tests {
    use crate::index::ClassOrigin;
    use crate::language::java::class_parser::parse_java_source;

    #[test]
    fn enum_rule_produces_semantic_constants() {
        let src = "enum Color { RED, GREEN, BLUE }";
        let synthetic = parse_java_source(src, ClassOrigin::Unknown, None)
            .into_iter()
            .find(|class| class.name.as_ref() == "Color")
            .expect("Color enum");
        let names: Vec<&str> = synthetic
            .fields
            .iter()
            .map(|field| field.name.as_ref())
            .collect();
        assert_eq!(names, vec!["RED", "GREEN", "BLUE"]);
    }
}
