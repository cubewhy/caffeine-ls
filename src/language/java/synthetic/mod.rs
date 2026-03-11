mod common;
mod enum_rule;
mod record_rule;

pub use common::{
    SyntheticDefinition, SyntheticDefinitionKind, SyntheticInput, SyntheticMemberRule,
    SyntheticMemberSet, SyntheticOrigin, extract_type_members_with_synthetics,
    resolve_synthetic_definition, synthesize_for_type,
};
pub use enum_rule::enum_constant_names;
pub use record_rule::{RecordComponent, record_components};
