use std::sync::Arc;
use tree_sitter::Node;

use crate::{
    index::{FieldSummary, MethodSummary},
    language::java::{
        JavaContextExtractor,
        lombok::rules::{ConstructorRule, EqualsAndHashCodeRule, GetterSetterRule, ToStringRule},
        members::extract_class_members_from_body,
        synthetic::rules::{enum_rule, record_rule},
        type_ctx::SourceTypeCtx,
    },
    semantic::context::CurrentClassMember,
};

use super::rules::{enum_rule::EnumRule, record_rule::RecordRule};

const SYNTHETIC_RULES: [&dyn SyntheticMemberRule; 6] = [
    &RecordRule,
    &EnumRule,
    &GetterSetterRule,
    &ToStringRule,
    &EqualsAndHashCodeRule,
    &ConstructorRule,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntheticOrigin {
    RecordComponentAccessor {
        component_name: Arc<str>,
    },
    RecordCanonicalConstructor,
    EnumConstant {
        constant_name: Arc<str>,
    },

    // Lombok origins
    LombokGetter {
        field_name: Arc<str>,
    },
    LombokSetter {
        field_name: Arc<str>,
    },
    LombokToString,
    LombokEquals,
    LombokHashCode,
    LombokConstructor {
        constructor_type: crate::language::java::lombok::LombokConstructorType,
    },
    LombokBuilder {
        builder_method: crate::language::java::lombok::LombokBuilderMethod,
    },
    LombokWith {
        field_name: Arc<str>,
    },
    LombokDelegate {
        field_name: Arc<str>,
    },
    LombokLog,
}

#[derive(Debug, Clone)]
pub struct SyntheticDefinition {
    pub kind: SyntheticDefinitionKind,
    pub name: Arc<str>,
    pub descriptor: Option<Arc<str>>,
    pub origin: SyntheticOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticDefinitionKind {
    Method,
    Field,
}

#[derive(Debug, Default, Clone)]
pub struct SyntheticMemberSet {
    pub methods: Vec<MethodSummary>,
    pub fields: Vec<FieldSummary>,
    pub definitions: Vec<SyntheticDefinition>,
}

pub trait SyntheticMemberRule {
    fn synthesize(
        &self,
        input: &SyntheticInput<'_>,
        out: &mut SyntheticMemberSet,
        explicit_methods: &[MethodSummary],
        explicit_fields: &[FieldSummary],
    );
}

pub struct SyntheticInput<'a> {
    pub ctx: &'a JavaContextExtractor,
    pub decl: Node<'a>,
    pub owner_internal: Option<&'a str>,
    pub type_ctx: &'a SourceTypeCtx,
}

pub fn synthesize_for_type(
    ctx: &JavaContextExtractor,
    decl: Node,
    owner_internal: Option<&str>,
    type_ctx: &SourceTypeCtx,
    explicit_methods: &[MethodSummary],
    explicit_fields: &[FieldSummary],
) -> SyntheticMemberSet {
    let input = SyntheticInput {
        ctx,
        decl,
        owner_internal,
        type_ctx,
    };
    let mut out = SyntheticMemberSet::default();
    for rule in SYNTHETIC_RULES {
        rule.synthesize(&input, &mut out, explicit_methods, explicit_fields);
    }
    out
}

pub fn extract_type_members_with_synthetics(
    ctx: &JavaContextExtractor,
    decl: Node,
    type_ctx: &SourceTypeCtx,
    owner_internal: Option<&str>,
) -> Vec<CurrentClassMember> {
    let explicit_members = decl
        .child_by_field_name("body")
        .map(|body| extract_class_members_from_body(ctx, body, type_ctx))
        .unwrap_or_default();

    let explicit_methods: Vec<MethodSummary> = explicit_members
        .iter()
        .filter_map(|member| match member {
            CurrentClassMember::Method(method) => Some((**method).clone()),
            CurrentClassMember::Field(_) => None,
        })
        .collect();
    let explicit_fields: Vec<FieldSummary> = explicit_members
        .iter()
        .filter_map(|member| match member {
            CurrentClassMember::Field(field) => Some((**field).clone()),
            CurrentClassMember::Method(_) => None,
        })
        .collect();

    let synthetic = synthesize_for_type(
        ctx,
        decl,
        owner_internal,
        type_ctx,
        &explicit_methods,
        &explicit_fields,
    );

    let mut merged: Vec<CurrentClassMember> = synthetic
        .methods
        .into_iter()
        .map(|method| CurrentClassMember::Method(Arc::new(method)))
        .collect();
    merged.extend(
        synthetic
            .fields
            .into_iter()
            .map(|field| CurrentClassMember::Field(Arc::new(field))),
    );
    merged.extend(explicit_members);
    merged
}

pub fn resolve_synthetic_definition<'a>(
    ctx: &'a JavaContextExtractor,
    decl: Node<'a>,
    type_ctx: &'a SourceTypeCtx,
    owner_internal: Option<&'a str>,
    kind: SyntheticDefinitionKind,
    name: &str,
    descriptor: Option<&str>,
) -> Option<Node<'a>> {
    let explicit_members = decl
        .child_by_field_name("body")
        .map(|body| extract_class_members_from_body(ctx, body, type_ctx))
        .unwrap_or_default();
    let explicit_methods: Vec<MethodSummary> = explicit_members
        .iter()
        .filter_map(|member| match member {
            CurrentClassMember::Method(method) => Some((**method).clone()),
            CurrentClassMember::Field(_) => None,
        })
        .collect();
    let explicit_fields: Vec<FieldSummary> = explicit_members
        .iter()
        .filter_map(|member| match member {
            CurrentClassMember::Field(field) => Some((**field).clone()),
            CurrentClassMember::Method(_) => None,
        })
        .collect();
    let synthetic = synthesize_for_type(
        ctx,
        decl,
        owner_internal,
        type_ctx,
        &explicit_methods,
        &explicit_fields,
    );
    synthetic.definitions.into_iter().find_map(|definition| {
        if definition.kind != kind || definition.name.as_ref() != name {
            return None;
        }
        if descriptor.is_some() && definition.descriptor.as_deref() != descriptor {
            return None;
        }
        match definition.origin {
            SyntheticOrigin::RecordComponentAccessor { component_name } => {
                record_rule::find_record_component_node(ctx, decl, component_name.as_ref())
            }
            SyntheticOrigin::RecordCanonicalConstructor => record_rule::record_parameter_node(decl)
                .or_else(|| decl.child_by_field_name("name")),
            SyntheticOrigin::EnumConstant { constant_name } => {
                enum_rule::find_enum_constant_node(ctx, decl, constant_name.as_ref())
            }
            // Lombok origins - resolve to field or class declaration
            SyntheticOrigin::LombokGetter { field_name } => {
                find_field_node(ctx, decl, field_name.as_ref())
            }
            SyntheticOrigin::LombokSetter { field_name } => {
                find_field_node(ctx, decl, field_name.as_ref())
            }
            SyntheticOrigin::LombokWith { field_name } => {
                find_field_node(ctx, decl, field_name.as_ref())
            }
            SyntheticOrigin::LombokDelegate { field_name } => {
                find_field_node(ctx, decl, field_name.as_ref())
            }
            SyntheticOrigin::LombokToString
            | SyntheticOrigin::LombokEquals
            | SyntheticOrigin::LombokHashCode
            | SyntheticOrigin::LombokConstructor { .. }
            | SyntheticOrigin::LombokBuilder { .. }
            | SyntheticOrigin::LombokLog => {
                // Resolve to class name
                decl.child_by_field_name("name")
            }
        }
    })
}

/// Find a field node by name in a class declaration
fn find_field_node<'a>(
    ctx: &JavaContextExtractor,
    decl: Node<'a>,
    field_name: &str,
) -> Option<Node<'a>> {
    let body = decl.child_by_field_name("body")?;
    let mut cursor = body.walk();

    for child in body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }

        // Look for declarator with matching name
        let mut field_cursor = child.walk();
        for declarator in child.named_children(&mut field_cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }

            if let Some(name_node) = declarator.child_by_field_name("name") {
                if ctx.node_text(name_node) == field_name {
                    return Some(name_node);
                }
            }
        }
    }

    None
}
