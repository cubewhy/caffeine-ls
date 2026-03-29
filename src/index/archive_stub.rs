use std::sync::Arc;

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::index::{
    AnnotationSummary, AnnotationValue, ClassMetadata, ClassOrigin, FieldSummary, MethodParams,
    MethodSummary,
};
use crate::semantic::types::parse_return_type_from_descriptor;

const TARGET_INTERNAL: &str = "java/lang/annotation/Target";
const TARGET_VALUE_TYPE: &str = "java/lang/annotation/ElementType";
const RETENTION_INTERNAL: &str = "java/lang/annotation/Retention";
const RETENTION_VALUE_TYPE: &str = "java/lang/annotation/RetentionPolicy";

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Archive, RkyvSerialize, RkyvDeserialize,
)]
#[archive(check_bytes)]
pub struct ArchiveMethodStub {
    pub name: Arc<str>,
    pub descriptor: Arc<str>,
    pub param_names: Vec<Arc<str>>,
    pub access_flags: u16,
    pub is_synthetic: bool,
    pub generic_signature: Option<Arc<str>>,
}

impl ArchiveMethodStub {
    fn from_method_summary(method: MethodSummary) -> Self {
        let descriptor = method.desc();
        Self {
            name: method.name,
            descriptor,
            param_names: method.params.param_names(),
            access_flags: method.access_flags,
            is_synthetic: method.is_synthetic,
            generic_signature: method.generic_signature,
        }
    }

    pub(crate) fn materialize(&self) -> MethodSummary {
        MethodSummary {
            name: Arc::clone(&self.name),
            params: MethodParams::from_descriptor_and_names(
                self.descriptor.as_ref(),
                &self.param_names,
            ),
            annotations: Vec::new(),
            access_flags: self.access_flags,
            is_synthetic: self.is_synthetic,
            generic_signature: self.generic_signature.clone(),
            return_type: parse_return_type_from_descriptor(&self.descriptor),
        }
    }
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Archive, RkyvSerialize, RkyvDeserialize,
)]
#[archive(check_bytes)]
pub struct ArchiveFieldStub {
    pub name: Arc<str>,
    pub descriptor: Arc<str>,
    pub access_flags: u16,
    pub is_synthetic: bool,
    pub generic_signature: Option<Arc<str>>,
}

impl ArchiveFieldStub {
    fn from_field_summary(field: FieldSummary) -> Self {
        Self {
            name: field.name,
            descriptor: field.descriptor,
            access_flags: field.access_flags,
            is_synthetic: field.is_synthetic,
            generic_signature: field.generic_signature,
        }
    }

    pub(crate) fn materialize(&self) -> FieldSummary {
        FieldSummary {
            name: Arc::clone(&self.name),
            descriptor: Arc::clone(&self.descriptor),
            access_flags: self.access_flags,
            annotations: Vec::new(),
            is_synthetic: self.is_synthetic,
            generic_signature: self.generic_signature.clone(),
        }
    }
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Archive, RkyvSerialize, RkyvDeserialize,
)]
#[archive(check_bytes)]
pub struct ArchiveClassStub {
    pub package: Option<Arc<str>>,
    pub name: Arc<str>,
    pub internal_name: Arc<str>,
    pub super_name: Option<Arc<str>>,
    pub interfaces: Vec<Arc<str>>,
    pub methods: Vec<ArchiveMethodStub>,
    pub fields: Vec<ArchiveFieldStub>,
    pub access_flags: u16,
    pub generic_signature: Option<Arc<str>>,
    pub inner_class_of: Option<Arc<str>>,
    pub origin: ClassOrigin,
    pub annotation_targets: Option<Vec<Arc<str>>>,
    pub annotation_retention: Option<Arc<str>>,
}

impl ArchiveClassStub {
    pub fn from_class_metadata(class: ClassMetadata) -> Self {
        let annotation_targets = class.annotation_targets();
        let annotation_retention = class.annotation_retention().map(Arc::from);

        Self {
            package: class.package,
            name: class.name,
            internal_name: class.internal_name,
            super_name: class.super_name,
            interfaces: class.interfaces,
            methods: class
                .methods
                .into_iter()
                .map(ArchiveMethodStub::from_method_summary)
                .collect(),
            fields: class
                .fields
                .into_iter()
                .map(ArchiveFieldStub::from_field_summary)
                .collect(),
            access_flags: class.access_flags,
            generic_signature: class.generic_signature,
            inner_class_of: class.inner_class_of,
            origin: class.origin,
            annotation_targets,
            annotation_retention,
        }
    }

    pub fn materialize(&self) -> ClassMetadata {
        ClassMetadata {
            package: self.package.clone(),
            name: Arc::clone(&self.name),
            internal_name: Arc::clone(&self.internal_name),
            super_name: self.super_name.clone(),
            interfaces: self.interfaces.clone(),
            annotations: self.materialize_annotations(),
            methods: self
                .methods
                .iter()
                .map(ArchiveMethodStub::materialize)
                .collect(),
            fields: self
                .fields
                .iter()
                .map(ArchiveFieldStub::materialize)
                .collect(),
            access_flags: self.access_flags,
            generic_signature: self.generic_signature.clone(),
            inner_class_of: self.inner_class_of.clone(),
            origin: self.origin.clone(),
        }
    }

    fn materialize_annotations(&self) -> Vec<AnnotationSummary> {
        let mut annotations = Vec::new();

        if let Some(targets) = &self.annotation_targets {
            let value = if targets.len() == 1 {
                AnnotationValue::Enum {
                    type_name: Arc::from(TARGET_VALUE_TYPE),
                    const_name: Arc::clone(&targets[0]),
                }
            } else {
                AnnotationValue::Array(
                    targets
                        .iter()
                        .map(|target| AnnotationValue::Enum {
                            type_name: Arc::from(TARGET_VALUE_TYPE),
                            const_name: Arc::clone(target),
                        })
                        .collect(),
                )
            };

            annotations.push(AnnotationSummary {
                internal_name: Arc::from(TARGET_INTERNAL),
                runtime_visible: true,
                elements: FxHashMap::from_iter([(Arc::from("value"), value)]),
            });
        }

        if let Some(retention) = &self.annotation_retention {
            annotations.push(AnnotationSummary {
                internal_name: Arc::from(RETENTION_INTERNAL),
                runtime_visible: true,
                elements: FxHashMap::from_iter([(
                    Arc::from("value"),
                    AnnotationValue::Enum {
                        type_name: Arc::from(RETENTION_VALUE_TYPE),
                        const_name: Arc::clone(retention),
                    },
                )]),
            });
        }

        annotations
    }
}
