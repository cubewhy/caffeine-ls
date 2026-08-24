use anyhow::Context;
use lasso::ThreadedRodeo;
use rust_asm::{
    class_reader::{Annotation, AttributeInfo, ClassReader, ElementValue},
    constant_pool::{ConstantPoolExt, CpInfo},
    constants::{ACC_MANDATED, ACC_STATIC, ACC_SYNTHETIC, ACC_VARARGS},
    nodes::{ClassNode, FieldNode, MethodNode, ModuleNode},
};

use crate::{
    class_parser::sig::{SigParser, get_signature},
    stub::{
        AnnotationSig, AnnotationValue, ClassOrModuleStub, ClassStub, FieldStub, MethodStub,
        ModuleExports, ModuleOpens, ModuleProvides, ModuleRequires, ModuleStub, ParamData,
        PrimitiveType, PrimitiveValue, RecordComponentData, Symbol, TypeRef,
    },
};

mod sig;

pub struct ClassParser<'a> {
    interner: &'a ThreadedRodeo,
}

impl<'a> ClassParser<'a> {
    pub fn new(interner: &'a ThreadedRodeo) -> Self {
        Self { interner }
    }

    pub fn parse_cafebabe(&self, bytes: &[u8]) -> anyhow::Result<ClassOrModuleStub<Symbol>> {
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

    fn internal_name_to_type_ref(&self, name: &str) -> TypeRef<Symbol> {
        TypeRef::Reference {
            name: self.interner.get_or_intern(name.replace("/", ".")),
            generic_args: Vec::new(),
        }
    }

    fn map_module(&self, node: &ModuleNode) -> ModuleStub<Symbol> {
        let dotted = |name: &str| self.interner.get_or_intern(name.replace('/', "."));
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
                    package_name: dotted(&exp.package),
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
                    package_name: dotted(&op.package),
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

    fn map_class(&self, node: &ClassNode) -> ClassStub<Symbol> {
        let mut type_params = Vec::new();
        let mut super_class = node
            .super_name
            .as_deref()
            .map(|name| self.internal_name_to_type_ref(name));
        let mut interfaces: Vec<TypeRef<Symbol>> = node
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

        let fqn_str = node.name.replace('/', ".");
        let simple_name = fqn_str
            .rsplit_once('.')
            .map(|(_, simple)| simple)
            .unwrap_or(&fqn_str);
        let is_record = node
            .attributes
            .iter()
            .any(|attr| matches!(attr, AttributeInfo::Record { .. }));

        // A variable-arity record component ([JLS §8.10.1]) is encoded as an
        // array descriptor with `ACC_VARARGS` on the canonical constructor
        // ([JVMS §4.7.22]); the `Record` attribute itself carries no varargs
        // flag, so the constructor's flag is the authoritative signal. Varargs
        // is always the last formal ([JLS §8.4.1]), so only a trailing
        // array-typed component is marked.
        let record_varargs = if is_record {
            node.methods.iter().any(|method_node| {
                method_node.name == "<init>"
                    && method_node.access_flags & ACC_VARARGS != 0
                    && self
                        .parse_method_descriptor(&method_node.descriptor)
                        .0
                        .len()
                        == node.record_components.len()
            })
        } else {
            false
        };
        let component_count = node.record_components.len();

        ClassStub {
            fqn: self.interner.get_or_intern(&fqn_str),
            name: self.interner.get_or_intern(simple_name),
            flags: node.access_flags,
            is_record,
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
                .enumerate()
                .map(|(i, rc)| {
                    let is_last = component_count > 0 && i == component_count - 1;
                    self.map_record_component(rc, &node.constant_pool, record_varargs && is_last)
                })
                .collect(),

            annotations: self.map_annotations(&node.attributes, &node.constant_pool),
        }
    }

    fn map_record_component(
        &self,
        node: &rust_asm::nodes::RecordComponentNode,
        constant_pool: &[CpInfo],
        varargs: bool,
    ) -> RecordComponentData<Symbol> {
        let mut chars = node.descriptor.chars().peekable();
        let mut component_type = self.parse_type_ref(&mut chars);

        if let Some(sig) = get_signature(&node.attributes, constant_pool) {
            let mut parser = SigParser::new(&sig, self.interner);
            component_type = parser.parse_reference_type_signature();
        }

        let varargs = varargs && matches!(&component_type, TypeRef::Array(_));
        RecordComponentData {
            name: self.interner.get_or_intern(&node.name),
            component_type,
            varargs,
            annotations: self.map_annotations(&node.attributes, constant_pool),
        }
    }

    fn map_field(&self, node: &FieldNode, constant_pool: &[CpInfo]) -> FieldStub<Symbol> {
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
            name: self.interner.get_or_intern(&node.name),
            flags: node.access_flags,
            field_type,
            annotations: self.map_annotations(&node.attributes, constant_pool),
            constant_value,
        }
    }

    fn map_method(&self, node: &MethodNode, constant_pool: &[CpInfo]) -> MethodStub<Symbol> {
        let (mut params, mut return_type) = self.parse_method_descriptor(&node.descriptor);
        let mut type_params = Vec::new();
        let mut throws_list: Vec<TypeRef<Symbol>> = node
            .exceptions
            .iter()
            .map(|e| self.internal_name_to_type_ref(e))
            .collect();

        if let Some(sig) = get_signature(&node.attributes, constant_pool) {
            let mut parser = SigParser::new(&sig, self.interner);
            let (tp, param_types, ret_type, throws) = parser.parse_method_signature();
            type_params = tp;

            // The generic `Signature` lists only the *declared* parameters; a
            // synthetic leading descriptor parameter — the implicit outer
            // instance of a non-static inner-class constructor ([JLS §8.1.3]) or
            // the implicit `name`/`ordinal` of an enum constructor ([JLS §8.9.2]) —
            // has no place in the signature. Align the signature types onto the
            // trailing (declared) descriptor parameters, counting the synthetic
            // prefix from the `MethodParameters` flags when present and falling
            // back to the length difference otherwise.
            let synthetic = leading_synthetic_params(node, params.len(), param_types.len());
            for (param, p_type) in params
                .iter_mut()
                .skip(synthetic)
                .take(param_types.len())
                .zip(param_types)
            {
                param.param_type = p_type;
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

        let default_value = node.attributes.iter().find_map(|attr| {
            if let AttributeInfo::AnnotationDefault { default_value } = attr {
                Some(self.map_element_value(default_value, constant_pool))
            } else {
                None
            }
        });

        MethodStub {
            flags: node.access_flags,
            name: self.interner.get_or_intern(&node.name),
            return_type,
            params,
            throws_list,
            type_params,
            annotations: self.map_annotations(&node.attributes, constant_pool),
            default_value,
        }
    }

    fn parse_type_ref(&self, chars: &mut std::iter::Peekable<std::str::Chars>) -> TypeRef<Symbol> {
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

    fn parse_method_descriptor(&self, desc: &str) -> (Vec<ParamData<Symbol>>, TypeRef<Symbol>) {
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
    ) -> Vec<AnnotationSig<Symbol>> {
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

    pub fn map_annotation(&self, anno: &Annotation, cp: &[CpInfo]) -> AnnotationSig<Symbol> {
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

    pub fn map_element_value(
        &self,
        value: &ElementValue,
        cp: &[CpInfo],
    ) -> AnnotationValue<Symbol> {
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

/// The number of synthetic leading parameters of a method: the implicit `Outer`
/// parameter of a non-static inner-class constructor or the implicit
/// `name`/`ordinal` parameters of an enum constructor ([JVMS §4.7.24]). Such
/// parameters appear in the descriptor but are absent from the generic
/// `Signature` ([JVMS §4.7.9.1]). The count is taken from the `MethodParameters`
/// flags when present, falling back to the descriptor/signature length
/// difference; the two signals agree for classfiles javac emits.
fn leading_synthetic_params(
    node: &MethodNode,
    descriptor_params: usize,
    sig_params: usize,
) -> usize {
    let flagged = node
        .method_parameters
        .iter()
        .take(descriptor_params)
        .take_while(|param| param.access_flags & (ACC_SYNTHETIC | ACC_MANDATED) != 0)
        .count();
    // Prefer the flags when the attribute covers the whole descriptor; otherwise
    // (missing or partial attribute) fall back to the length delta.
    if node.method_parameters.len() == descriptor_params && flagged > 0 {
        return flagged;
    }
    descriptor_params.saturating_sub(sig_params)
}

#[cfg(test)]
mod tests {
    use rust_asm::{
        class_reader::{AttributeInfo, ElementValue},
        class_writer::ClassWriter,
        constant_pool::CpInfo,
        constants::{ACC_PUBLIC, ACC_VARARGS},
        nodes::MethodNode,
    };

    use super::*;
    use crate::stub::ClassOrModuleStub;

    #[test]
    fn annotation_default_is_mapped() {
        let interner = ThreadedRodeo::default();
        let parser = ClassParser::new(&interner);
        let node = MethodNode {
            access_flags: 0x0001,
            name: "value".to_owned(),
            descriptor: "()Ljava/lang/String;".to_owned(),
            has_code: false,
            max_stack: 0,
            max_locals: 0,
            instructions: rust_asm::insn::InsnList::new(),
            instruction_offsets: Vec::new(),
            insn_nodes: Vec::new(),
            exception_table: Vec::new(),
            try_catch_blocks: Vec::new(),
            line_numbers: Vec::new(),
            local_variables: Vec::new(),
            method_parameters: Vec::new(),
            exceptions: Vec::new(),
            signature: None,
            code_attributes: Vec::new(),
            attributes: vec![AttributeInfo::AnnotationDefault {
                default_value: ElementValue::ConstValueIndex {
                    tag: b's',
                    const_value_index: 1,
                },
            }],
        };
        let cp = vec![CpInfo::Unusable, CpInfo::Utf8("default".to_owned())];
        let stub = parser.map_method(&node, &cp);
        assert_eq!(
            stub.default_value,
            Some(AnnotationValue::String(interner.get_or_intern("default")))
        );
    }

    #[test]
    fn no_annotation_default_yields_none() {
        let interner = ThreadedRodeo::default();
        let parser = ClassParser::new(&interner);
        let node = MethodNode {
            access_flags: 0x0001,
            name: "value".to_owned(),
            descriptor: "()I".to_owned(),
            has_code: false,
            max_stack: 0,
            max_locals: 0,
            instructions: rust_asm::insn::InsnList::new(),
            instruction_offsets: Vec::new(),
            insn_nodes: Vec::new(),
            exception_table: Vec::new(),
            try_catch_blocks: Vec::new(),
            line_numbers: Vec::new(),
            local_variables: Vec::new(),
            method_parameters: Vec::new(),
            exceptions: Vec::new(),
            signature: None,
            code_attributes: Vec::new(),
            attributes: Vec::new(),
        };
        let stub = parser.map_method(&node, &[]);
        assert_eq!(stub.default_value, None);
    }

    /// F4: a variable-arity record component is marked from the canonical
    /// constructor's `ACC_VARARGS` ([JLS §8.10.1], [JVMS §4.6]).
    #[test]
    fn record_varargs_from_canonical_constructor() {
        let mut cw = ClassWriter::new(0);
        cw.visit(
            52,
            0,
            ACC_PUBLIC,
            "com/example/R",
            Some("java/lang/Object"),
            &[],
        );
        let rcv = cw.visit_record_component("names", "[Ljava/lang/String;");
        rcv.visit_end(&mut cw);
        let ctor = cw.visit_method(ACC_PUBLIC | ACC_VARARGS, "<init>", "([Ljava/lang/String;)V");
        ctor.visit_end(&mut cw);

        let interner = ThreadedRodeo::default();
        let parser = ClassParser::new(&interner);
        let stub = parser.parse_cafebabe(&cw.to_bytes().unwrap()).unwrap();
        let ClassOrModuleStub::Class(class) = stub else {
            panic!("expected a class");
        };
        assert_eq!(class.record_components.len(), 1);
        assert!(class.record_components[0].varargs);
    }

    /// F4: a non-varargs canonical constructor leaves the array component
    /// unmarked.
    #[test]
    fn record_array_component_not_varargs_without_constructor_flag() {
        let mut cw = ClassWriter::new(0);
        cw.visit(
            52,
            0,
            ACC_PUBLIC,
            "com/example/R",
            Some("java/lang/Object"),
            &[],
        );
        let rcv = cw.visit_record_component("values", "[I");
        rcv.visit_end(&mut cw);
        let ctor = cw.visit_method(ACC_PUBLIC, "<init>", "([I)V");
        ctor.visit_end(&mut cw);

        let interner = ThreadedRodeo::default();
        let parser = ClassParser::new(&interner);
        let stub = parser.parse_cafebabe(&cw.to_bytes().unwrap()).unwrap();
        let ClassOrModuleStub::Class(class) = stub else {
            panic!("expected a class");
        };
        assert!(!class.record_components[0].varargs);
    }
}
