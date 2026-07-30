use anyhow::Context;
use lasso::ThreadedRodeo;
use rust_asm::{
    class_reader::{Annotation, AttributeInfo, ClassReader, ElementValue},
    constant_pool::{ConstantPoolExt, CpInfo},
    constants::ACC_STATIC,
    nodes::{ClassNode, FieldNode, MethodNode, ModuleNode},
};

use crate::{
    ast::{
        AnnotationSig, AnnotationValue, ClassOrModuleStub, ClassStub, FieldStub, MethodStub,
        ModuleExports, ModuleOpens, ModuleProvides, ModuleRequires, ModuleStub, ParamData,
        PrimitiveType, PrimitiveValue, RecordComponentData, TypeRef,
    },
    class_parser::sig::{SigParser, get_signature},
};

mod sig;

pub struct ClassParser<'a> {
    interner: &'a ThreadedRodeo,
}

impl<'a> ClassParser<'a> {
    pub fn new(interner: &'a ThreadedRodeo) -> Self {
        Self { interner }
    }

    pub fn parse_cafebabe(&self, bytes: &[u8]) -> anyhow::Result<ClassOrModuleStub> {
        let node = ClassReader::new(bytes)
            .to_class_node()
            .context("Failed to parse class")?;

        let model = if let Some(module_node) = node.module {
            // module class
            let module = self.map_module(&module_node);
            ClassOrModuleStub::Module(module)
        } else {
            let class = self.map_class(&node);
            ClassOrModuleStub::Class(class)
        };

        Ok(model)
    }

    fn internal_name_to_type_ref(&self, name: &str) -> TypeRef {
        TypeRef::Reference {
            name: self.interner.get_or_intern(name.replace("/", ".")),
            generic_args: Vec::new(),
        }
    }

    fn map_module(&self, node: &ModuleNode) -> ModuleStub {
        ModuleStub {
            name: self.interner.get_or_intern(&node.name),
            flags: node.access_flags,
            version: node
                .version
                .as_deref()
                .map(|v| self.interner.get_or_intern(v)),
            requires: node
                .requires
                .iter()
                .map(|req| ModuleRequires {
                    module_name: self.interner.get_or_intern(&req.module),
                    flags: req.access_flags,
                    compiled_version: req
                        .version
                        .as_deref()
                        .map(|v| self.interner.get_or_intern(v)),
                })
                .collect(),
            exports: node
                .exports
                .iter()
                .map(|exp| ModuleExports {
                    package_name: self.interner.get_or_intern(&exp.package),
                    flags: exp.access_flags,
                    to_modules: exp
                        .modules
                        .iter()
                        .map(|m| self.interner.get_or_intern(m))
                        .collect(),
                })
                .collect(),
            opens: node
                .opens
                .iter()
                .map(|op| ModuleOpens {
                    package_name: self.interner.get_or_intern(&op.package),
                    flags: op.access_flags,
                    to_modules: op
                        .modules
                        .iter()
                        .map(|m| self.interner.get_or_intern(m))
                        .collect(),
                })
                .collect(),
            uses: node
                .uses
                .iter()
                .map(|u| self.internal_name_to_type_ref(u))
                .collect(),
            provides: node
                .provides
                .iter()
                .map(|prov| ModuleProvides {
                    service_interface: self.internal_name_to_type_ref(&prov.service),
                    with_implementations: prov
                        .providers
                        .iter()
                        .map(|p| self.internal_name_to_type_ref(p))
                        .collect(),
                })
                .collect(),
        }
    }

    fn map_class(&self, node: &ClassNode) -> ClassStub {
        let mut type_params = Vec::new();
        let mut super_class = node
            .super_name
            .as_deref()
            .map(|name| self.internal_name_to_type_ref(name));
        let mut interfaces: Vec<TypeRef> = node
            .interfaces
            .iter()
            .map(|i| self.internal_name_to_type_ref(i))
            .collect();

        if let Some(sig) = get_signature(&node.attributes, &node.constant_pool) {
            let mut parser = SigParser::new(&sig, self.interner);
            let (tp, sc, ifs) = parser.parse_class_signature();
            type_params = tp;
            super_class = Some(sc);
            interfaces = ifs;
        }

        ClassStub {
            name: self.interner.get_or_intern(&node.name),
            flags: node.access_flags,
            super_class,
            interfaces,

            methods: node
                .methods
                .iter()
                .map(|method_node| self.map_method(method_node, &node.constant_pool))
                .collect(),
            fields: node
                .fields
                .iter()
                .map(|field_node| self.map_field(field_node, &node.constant_pool))
                .collect(),

            type_params,

            permitted_subclasses: node
                .permitted_subclasses
                .iter()
                .map(|s| self.internal_name_to_type_ref(s))
                .collect(),

            record_components: node
                .record_components
                .iter()
                .map(|rc| self.map_record_component(rc, &node.constant_pool))
                .collect(),

            annotations: self.map_annotations(&node.attributes, &node.constant_pool),
        }
    }

    fn map_record_component(
        &self,
        node: &rust_asm::nodes::RecordComponentNode,
        constant_pool: &[CpInfo],
    ) -> RecordComponentData {
        let mut chars = node.descriptor.chars().peekable();
        let mut component_type = self.parse_type_ref(&mut chars);

        if let Some(sig) = get_signature(&node.attributes, constant_pool) {
            let mut parser = SigParser::new(&sig, self.interner);
            component_type = parser.parse_reference_type_signature();
        }

        RecordComponentData {
            name: self.interner.get_or_intern(&node.name),
            component_type,
            annotations: self.map_annotations(&node.attributes, constant_pool),
        }
    }

    fn map_field(&self, node: &FieldNode, constant_pool: &[CpInfo]) -> FieldStub {
        let mut chars = node.descriptor.chars().peekable();
        let mut field_type = self.parse_type_ref(&mut chars);

        if let Some(sig) = get_signature(&node.attributes, constant_pool) {
            let mut parser = SigParser::new(&sig, self.interner);
            field_type = parser.parse_reference_type_signature();
        }

        let constant_value = node.attributes.iter().find_map(|attr| {
            if let AttributeInfo::ConstantValue {
                constantvalue_index,
            } = attr
            {
                match constant_pool.get(*constantvalue_index as usize)? {
                    CpInfo::Integer(v) => Some(AnnotationValue::Primitive(PrimitiveValue::Int(*v))),
                    CpInfo::Float(v) => Some(AnnotationValue::Primitive(PrimitiveValue::float(*v))),
                    CpInfo::Long(v) => Some(AnnotationValue::Primitive(PrimitiveValue::Long(*v))),
                    CpInfo::Double(v) => {
                        Some(AnnotationValue::Primitive(PrimitiveValue::double(*v)))
                    }
                    CpInfo::String { string_index } => constant_pool
                        .resolve_utf8(*string_index)
                        .map(|s| AnnotationValue::String(self.interner.get_or_intern(s))),
                    _ => None,
                }
            } else {
                None
            }
        });

        FieldStub {
            flags: node.access_flags,
            field_type,
            annotations: self.map_annotations(&node.attributes, constant_pool),
            constant_value,
        }
    }

    fn map_method(&self, node: &MethodNode, constant_pool: &[CpInfo]) -> MethodStub {
        let (mut params, mut return_type) = self.parse_method_descriptor(&node.descriptor);
        let mut type_params = Vec::new();
        let mut throws_list: Vec<TypeRef> = node
            .exceptions
            .iter()
            .map(|e| self.internal_name_to_type_ref(e))
            .collect();

        if let Some(sig) = get_signature(&node.attributes, constant_pool) {
            let mut parser = SigParser::new(&sig, self.interner);
            let (tp, param_types, ret_type, throws) = parser.parse_method_signature();
            type_params = tp;

            if param_types.len() == params.len() {
                for (p, p_type) in params.iter_mut().zip(param_types) {
                    p.param_type = p_type;
                }
            }
            return_type = ret_type;
            if !throws.is_empty() {
                throws_list = throws;
            }
        }

        for (i, param) in params.iter_mut().enumerate() {
            if let Some(method_param) = node.method_parameters.get(i) {
                param.flags = method_param.access_flags;
                if method_param.name_index != 0
                    && let Some(name) = constant_pool.resolve_utf8(method_param.name_index)
                {
                    param.name = Some(self.interner.get_or_intern(name));
                }
            }
        }

        let mut local_var_index = if (node.access_flags & ACC_STATIC) != 0 {
            0
        } else {
            1
        };

        for param in params.iter_mut() {
            if param.name.is_none()
                && let Some(local_var) = node
                    .local_variables
                    .iter()
                    .find(|lv| lv.index == local_var_index && lv.start_pc == 0)
                && let Some(name) = constant_pool.resolve_utf8(local_var.name_index)
            {
                param.name = Some(self.interner.get_or_intern(name));
            }
            match &param.param_type {
                TypeRef::Primitive(PrimitiveType::Double)
                | TypeRef::Primitive(PrimitiveType::Long) => {
                    local_var_index += 2;
                }
                _ => {
                    local_var_index += 1;
                }
            }
        }

        MethodStub {
            flags: node.access_flags,
            name: self.interner.get_or_intern(&node.name),
            return_type,
            params,
            throws_list,
            type_params,
            annotations: self.map_annotations(&node.attributes, constant_pool),
            default_value: None,
        }
    }

    fn parse_type_ref(&self, chars: &mut std::iter::Peekable<std::str::Chars>) -> TypeRef {
        match chars.next() {
            Some('B') => TypeRef::Primitive(PrimitiveType::Byte),
            Some('C') => TypeRef::Primitive(PrimitiveType::Char),
            Some('D') => TypeRef::Primitive(PrimitiveType::Double),
            Some('F') => TypeRef::Primitive(PrimitiveType::Float),
            Some('I') => TypeRef::Primitive(PrimitiveType::Int),
            Some('J') => TypeRef::Primitive(PrimitiveType::Long),
            Some('S') => TypeRef::Primitive(PrimitiveType::Short),
            Some('Z') => TypeRef::Primitive(PrimitiveType::Boolean),
            Some('V') => TypeRef::Primitive(PrimitiveType::Void),
            Some('L') => {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c == ';' {
                        break;
                    }
                    name.push(c);
                }
                TypeRef::Reference {
                    name: self.interner.get_or_intern(name.replace("/", ".")),
                    generic_args: Vec::new(),
                }
            }
            Some('[') => TypeRef::Array(Box::new(self.parse_type_ref(chars))),
            _ => TypeRef::Error,
        }
    }

    fn parse_method_descriptor(&self, desc: &str) -> (Vec<ParamData>, TypeRef) {
        let mut chars = desc.chars().peekable();
        let mut params = Vec::new();

        if chars.next() != Some('(') {
            return (params, TypeRef::Error);
        }

        while let Some(&c) = chars.peek() {
            if c == ')' {
                chars.next();
                break;
            }

            let param_type = self.parse_type_ref(&mut chars);
            params.push(ParamData {
                flags: 0,
                name: None,
                param_type,
                annotations: Vec::new(),
            });
        }

        let return_type = self.parse_type_ref(&mut chars);

        (params, return_type)
    }

    fn map_annotations(
        &self,
        attributes: &[AttributeInfo],
        constant_pool: &[CpInfo],
    ) -> Vec<AnnotationSig> {
        let mut signatures = Vec::new();

        for attr in attributes {
            match attr {
                AttributeInfo::RuntimeVisibleAnnotations { annotations }
                | AttributeInfo::RuntimeInvisibleAnnotations { annotations } => {
                    for anno in annotations {
                        signatures.push(self.map_annotation(anno, constant_pool));
                    }
                }
                _ => {}
            }
        }
        signatures
    }

    pub fn map_annotation(&self, anno: &Annotation, cp: &[CpInfo]) -> AnnotationSig {
        let type_descriptor = cp
            .resolve_utf8(anno.type_descriptor_index)
            .unwrap_or("<missing_annotation_type>");

        let mut chars = type_descriptor.chars().peekable();
        let annotation_type = self.parse_type_ref(&mut chars);

        let arguments = anno
            .element_value_pairs
            .iter()
            .map(|pair| {
                let name = self.interner.get_or_intern(
                    cp.resolve_utf8(pair.element_name_index)
                        .unwrap_or("<missing_name>"),
                );
                let value = self.map_element_value(&pair.value, cp);
                (name, value)
            })
            .collect();

        AnnotationSig {
            annotation_type,
            arguments,
        }
    }

    pub fn map_element_value(&self, value: &ElementValue, cp: &[CpInfo]) -> AnnotationValue {
        match value {
            ElementValue::ConstValueIndex {
                tag,
                const_value_index,
            } => {
                let index = *const_value_index;

                match *tag as char {
                    'B' => AnnotationValue::Primitive(PrimitiveValue::Byte(
                        cp.get_int(index).unwrap_or(0) as i8,
                    )),
                    'C' => AnnotationValue::Primitive(PrimitiveValue::Char(
                        cp.get_int(index).unwrap_or(0) as u16,
                    )),
                    'D' => AnnotationValue::Primitive(PrimitiveValue::double(
                        cp.get_double(index).unwrap_or(0.0),
                    )),
                    'F' => AnnotationValue::Primitive(PrimitiveValue::float(
                        cp.get_float(index).unwrap_or(0.0),
                    )),
                    'I' => AnnotationValue::Primitive(PrimitiveValue::Int(
                        cp.get_int(index).unwrap_or(0),
                    )),
                    'J' => AnnotationValue::Primitive(PrimitiveValue::Long(
                        cp.get_long(index).unwrap_or(0),
                    )),
                    'S' => AnnotationValue::Primitive(PrimitiveValue::Short(
                        cp.get_int(index).unwrap_or(0) as i16,
                    )),
                    'Z' => AnnotationValue::Primitive(PrimitiveValue::Boolean(
                        cp.get_int(index).unwrap_or(0) != 0,
                    )),
                    's' => AnnotationValue::String(
                        self.interner
                            .get_or_intern(cp.resolve_utf8(index).unwrap_or("<missing_string>")),
                    ),
                    _ => AnnotationValue::String(self.interner.get_or_intern("<unknown_tag>")),
                }
            }
            ElementValue::EnumConstValue {
                type_name_index,
                const_name_index,
            } => {
                let type_name = cp
                    .resolve_utf8(*type_name_index)
                    .unwrap_or("<missing_enum_type>");
                let const_name = cp
                    .resolve_utf8(*const_name_index)
                    .unwrap_or("<missing_enum_const>");

                let mut chars = type_name.chars().peekable();
                let class_type = self.parse_type_ref(&mut chars);

                AnnotationValue::Enum {
                    class_type,
                    entry_name: self.interner.get_or_intern(const_name),
                }
            }
            ElementValue::ClassInfoIndex { class_info_index } => {
                let return_descriptor = cp
                    .resolve_utf8(*class_info_index)
                    .unwrap_or("<missing_class_info>");

                let mut chars = return_descriptor.chars().peekable();
                AnnotationValue::Class(self.parse_type_ref(&mut chars))
            }
            ElementValue::AnnotationValue(anno) => {
                AnnotationValue::Annotation(self.map_annotation(anno, cp))
            }
            ElementValue::ArrayValue(elements) => {
                let mapped_elements = elements
                    .iter()
                    .map(|e| self.map_element_value(e, cp))
                    .collect();
                AnnotationValue::Array(mapped_elements)
            }
        }
    }
}
