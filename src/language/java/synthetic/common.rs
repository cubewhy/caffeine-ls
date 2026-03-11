use std::sync::Arc;
use tree_sitter::Node;

use crate::{
    index::{FieldSummary, MethodSummary},
    language::java::{
        JavaContextExtractor, members::extract_class_members_from_body, type_ctx::SourceTypeCtx,
    },
    semantic::context::CurrentClassMember,
};

use super::{enum_rule::EnumRule, record_rule::RecordRule};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntheticOrigin {
    RecordComponentAccessor { component_name: Arc<str> },
    RecordCanonicalConstructor,
    EnumConstant { constant_name: Arc<str> },
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
    let rules: [&dyn SyntheticMemberRule; 2] = [&RecordRule, &EnumRule];
    for rule in rules {
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
                super::record_rule::find_record_component_node(ctx, decl, component_name.as_ref())
            }
            SyntheticOrigin::RecordCanonicalConstructor => {
                super::record_rule::record_parameter_node(decl)
                    .or_else(|| decl.child_by_field_name("name"))
            }
            SyntheticOrigin::EnumConstant { constant_name } => {
                super::enum_rule::find_enum_constant_node(ctx, decl, constant_name.as_ref())
            }
        }
    })
}
