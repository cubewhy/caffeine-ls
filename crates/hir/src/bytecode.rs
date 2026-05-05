use anyhow::Context;
use rust_asm::{
    class_reader::{Annotation, AttributeInfo, ClassReader, ElementValue},
    constant_pool::{ConstantPoolExt, CpInfo},
    constants::ACC_STATIC,
    nodes::{ClassNode, FieldNode, MethodNode, ModuleNode},
};
use smol_str::SmolStr;

use crate::{
    AnnotationSignature, AnnotationValue, ClassData, ClassOrModuleData, FieldData, MethodData,
    ModuleData, ModuleExports, ModuleOpens, ModuleProvides, ModuleRequires, ParamData,
    PrimitiveType, PrimitiveValue, TypeRef,
    bytecode::sig::{SigParser, get_signature},
};

pub mod sig;

pub fn parse_cafebabe(bytes: &[u8]) -> anyhow::Result<ClassOrModuleData> {
    let node = ClassReader::new(bytes)
        .to_class_node()
        .context("Failed to parse class")?;

    let model = if let Some(module_node) = node.module {
        // module class
        let module = map_module(&module_node);
        ClassOrModuleData::Module(module)
    } else {
        let class = map_class(&node);
        ClassOrModuleData::Class(class)
    };

    Ok(model)
}

fn internal_name_to_type_ref(name: &str) -> TypeRef {
    TypeRef::Reference {
        name: SmolStr::new(name.replace("/", ".")),
        generic_args: Vec::new(),
    }
}

fn map_module(node: &ModuleNode) -> ModuleData {
    ModuleData {
        name: SmolStr::new(&node.name),
        flags: node.access_flags,
        version: node.version.as_deref().map(SmolStr::new),
        requires: node
            .requires
            .iter()
            .map(|req| ModuleRequires {
                module_name: SmolStr::new(&req.module),
                flags: req.access_flags,
                compiled_version: req.version.as_deref().map(SmolStr::new),
            })
            .collect(),
        exports: node
            .exports
            .iter()
            .map(|exp| ModuleExports {
                package_name: SmolStr::new(&exp.package),
                flags: exp.access_flags,
                to_modules: exp.modules.iter().map(SmolStr::new).collect(),
            })
            .collect(),
        opens: node
            .opens
            .iter()
            .map(|op| ModuleOpens {
                package_name: SmolStr::new(&op.package),
                flags: op.access_flags,
                to_modules: op.modules.iter().map(SmolStr::new).collect(),
            })
            .collect(),
        uses: node
            .uses
            .iter()
            .map(|u| internal_name_to_type_ref(u))
            .collect(),
        provides: node
            .provides
            .iter()
            .map(|prov| ModuleProvides {
                service_interface: internal_name_to_type_ref(&prov.service),
                with_implementations: prov
                    .providers
                    .iter()
                    .map(|p| internal_name_to_type_ref(p))
                    .collect(),
            })
            .collect(),
    }
}

fn map_class(node: &ClassNode) -> ClassData {
    let mut type_params = Vec::new();
    let mut super_class = node.super_name.as_deref().map(internal_name_to_type_ref);
    let mut interfaces: Vec<TypeRef> = node
        .interfaces
        .iter()
        .map(|i| internal_name_to_type_ref(i))
        .collect();

    if let Some(sig) = get_signature(&node.attributes, &node.constant_pool) {
        let mut parser = SigParser::new(&sig);
        let (tp, sc, ifs) = parser.parse_class_signature();
        type_params = tp;
        super_class = Some(sc);
        interfaces = ifs;
    }

    ClassData {
        name: SmolStr::new(&node.name),
        flags: node.access_flags,
        super_class,
        interfaces,

        methods: node
            .methods
            .iter()
            .map(|method_node| map_method(method_node, &node.constant_pool))
            .collect(),
        fields: node
            .fields
            .iter()
            .map(|field_node| map_field(field_node, &node.constant_pool))
            .collect(),

        type_params,
        // TODO: sealed classes and record components
        permitted_subclasses: Vec::new(),
        record_components: Vec::new(),
        annotations: map_annotations(&node.attributes, &node.constant_pool),
    }
}

fn map_field(node: &FieldNode, constant_pool: &[CpInfo]) -> FieldData {
    // A Field's descriptor looks like "I" or "Ljava/lang/String;"
    let mut chars = node.descriptor.chars().peekable();
    let mut field_type = parse_type_ref(&mut chars);

    // Apply specific generic arguments if signature is present
    if let Some(sig) = get_signature(&node.attributes, constant_pool) {
        let mut parser = SigParser::new(&sig);
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
                CpInfo::Double(v) => Some(AnnotationValue::Primitive(PrimitiveValue::double(*v))),
                CpInfo::String { string_index } => constant_pool
                    .resolve_utf8(*string_index)
                    .map(|s| AnnotationValue::String(SmolStr::new(s))),
                _ => None,
            }
        } else {
            None
        }
    });

    FieldData {
        flags: node.access_flags,
        field_type,
        annotations: map_annotations(&node.attributes, constant_pool),
        constant_value,
    }
}

fn map_method(node: &MethodNode, constant_pool: &[CpInfo]) -> MethodData {
    let (mut params, mut return_type) = parse_method_descriptor(&node.descriptor);
    let mut type_params = Vec::new();
    let mut throws_list: Vec<TypeRef> = node
        .exceptions
        .iter()
        .map(|e| internal_name_to_type_ref(e))
        .collect();

    // Augment with rich generic data if a Signature attribute is present
    if let Some(sig) = get_signature(&node.attributes, constant_pool) {
        let mut parser = SigParser::new(&sig);
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

    // // Apply names and flags from MethodParameters attribute if present
    for (i, param) in params.iter_mut().enumerate() {
        if let Some(method_param) = node.method_parameters.get(i) {
            param.flags = method_param.access_flags;
            if method_param.name_index != 0
                && let Some(name) = constant_pool.resolve_utf8(method_param.name_index)
            {
                param.name = Some(SmolStr::new(name));
            }
        }
    }

    // Fallback to LocalVariableTable for names if MethodParameters wasn't present/complete
    // NOTE: Parameter local variables usually start at index 1 for instance methods (0 is 'this'),
    // and index 0 for static methods.
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
            param.name = Some(SmolStr::new(name));
        }
        // Increment by 1 or 2 depending on parameter type (Double/Long take 2 slots in JVM locals)
        match &param.param_type {
            TypeRef::Primitive(PrimitiveType::Double) | TypeRef::Primitive(PrimitiveType::Long) => {
                local_var_index += 2;
            }
            _ => {
                local_var_index += 1;
            }
        }
    }

    MethodData {
        flags: node.access_flags,
        name: SmolStr::new(&node.name),
        return_type,
        params,
        throws_list,

        type_params,
        annotations: map_annotations(&node.attributes, constant_pool),
        default_value: None,
    }
}

/// Parses a JVM Field Descriptor (e.g., `[Ljava/lang/String;` or `I`)
fn parse_type_ref(chars: &mut std::iter::Peekable<std::str::Chars>) -> TypeRef {
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
                chars.next(); // consume
                if c == ';' {
                    break;
                }
                name.push(c);
            }
            TypeRef::Reference {
                name: SmolStr::new(name.replace("/", ".")),
                // Generic arguments are parsed from the `Signature` attribute, not the descriptor
                // so keep it empty
                generic_args: Vec::new(),
            }
        }
        Some('[') => TypeRef::Array(Box::new(parse_type_ref(chars))),
        _ => TypeRef::Error,
    }
}

/// Parses a JVM Method Descriptor (e.g., `(ILjava/lang/String;)[I`)
fn parse_method_descriptor(desc: &str) -> (Vec<ParamData>, TypeRef) {
    let mut chars = desc.chars().peekable();
    let mut params = Vec::new();

    // Consume '('
    if chars.next() != Some('(') {
        return (params, TypeRef::Error);
    }

    // Parse parameters until ')'
    while let Some(&c) = chars.peek() {
        if c == ')' {
            chars.next(); // consume ')'
            break;
        }

        let param_type = parse_type_ref(&mut chars);
        // NOTE: we parse parameter flags and name in [map_method], we only resolve the type from descriptor
        params.push(ParamData {
            flags: 0,
            name: None,
            param_type,
            annotations: Vec::new(),
        });
    }

    // The remainder is the return type
    let return_type = parse_type_ref(&mut chars);

    (params, return_type)
}

fn map_annotations(
    attributes: &[AttributeInfo],
    constant_pool: &[CpInfo],
) -> Vec<AnnotationSignature> {
    let mut signatures = Vec::new();

    for attr in attributes {
        match attr {
            AttributeInfo::RuntimeVisibleAnnotations { annotations }
            | AttributeInfo::RuntimeInvisibleAnnotations { annotations } => {
                for anno in annotations {
                    signatures.push(map_annotation(anno, constant_pool));
                }
            }
            _ => {}
        }
    }
    signatures
}

pub fn map_annotation(anno: &Annotation, cp: &[CpInfo]) -> AnnotationSignature {
    let type_descriptor = cp
        .resolve_utf8(anno.type_descriptor_index)
        .unwrap_or("<missing_annotation_type>");

    let mut chars = type_descriptor.chars().peekable();
    let annotation_type = parse_type_ref(&mut chars);

    let arguments = anno
        .element_value_pairs
        .iter()
        .map(|pair| {
            let name = SmolStr::new(
                cp.resolve_utf8(pair.element_name_index)
                    .unwrap_or("<missing_name>"),
            );
            let value = map_element_value(&pair.value, cp);
            (name, value)
        })
        .collect();

    AnnotationSignature {
        annotation_type,
        arguments,
    }
}

pub fn map_element_value(value: &ElementValue, cp: &[CpInfo]) -> AnnotationValue {
    match value {
        ElementValue::ConstValueIndex {
            tag,
            const_value_index,
        } => {
            let index = *const_value_index;

            // JVM Spec 4.7.16.1: The tag dictates the primitive type.
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
                'I' => {
                    AnnotationValue::Primitive(PrimitiveValue::Int(cp.get_int(index).unwrap_or(0)))
                }
                'J' => AnnotationValue::Primitive(PrimitiveValue::Long(
                    cp.get_long(index).unwrap_or(0),
                )),
                'S' => AnnotationValue::Primitive(PrimitiveValue::Short(
                    cp.get_int(index).unwrap_or(0) as i16,
                )),
                'Z' => AnnotationValue::Primitive(PrimitiveValue::Boolean(
                    cp.get_int(index).unwrap_or(0) != 0,
                )),
                's' => AnnotationValue::String(SmolStr::new(
                    cp.resolve_utf8(index).unwrap_or("<missing_string>"),
                )),
                _ => AnnotationValue::String(SmolStr::new("<unknown_tag>")),
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
            let class_type = parse_type_ref(&mut chars);

            AnnotationValue::Enum {
                class_type,
                entry_name: SmolStr::new(const_name),
            }
        }
        ElementValue::ClassInfoIndex { class_info_index } => {
            let return_descriptor = cp
                .resolve_utf8(*class_info_index)
                .unwrap_or("<missing_class_info>");

            let mut chars = return_descriptor.chars().peekable();
            AnnotationValue::Class(parse_type_ref(&mut chars))
        }
        ElementValue::AnnotationValue(anno) => {
            AnnotationValue::Annotation(map_annotation(anno, cp))
        }
        ElementValue::ArrayValue(elements) => {
            let mapped_elements = elements.iter().map(|e| map_element_value(e, cp)).collect();
            AnnotationValue::Array(mapped_elements)
        }
    }
}
