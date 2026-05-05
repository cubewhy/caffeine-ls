use crate::constant_pool::CpInfo;
use crate::error::ClassReadError;
use crate::insn::{
    AbstractInsnNode, FieldInsnNode, IincInsnNode, Insn, InsnList, InsnNode, IntInsnNode,
    InvokeDynamicInsnNode, InvokeInterfaceInsnNode, JumpInsnNode, LabelNode, LdcInsnNode, LdcValue,
    LineNumberInsnNode, LookupSwitchInsnNode, MemberRef, MethodInsnNode, MultiANewArrayInsnNode,
    TableSwitchInsnNode, TryCatchBlockNode, TypeInsnNode, VarInsnNode,
};
use crate::types::Type;
use crate::{constants, opcodes};

/// Represents a constant value loadable by the `LDC` (Load Constant) instruction.
///
/// This enum wraps various types of constants that can be stored in the constant pool
/// and pushed onto the operand stack.
#[derive(Debug, Clone)]
pub enum LdcConstant {
    /// A 32-bit integer constant.
    Integer(i32),
    /// A 32-bit floating-point constant.
    Float(f32),
    /// A 64-bit integer constant.
    Long(i64),
    /// A 64-bit floating-point constant.
    Double(f64),
    /// A string literal constant.
    String(String),
    /// A class constant (e.g., `String.class`).
    Class(String),
    /// A method type constant (MethodDescriptor).
    MethodType(String),
    /// A method handle constant.
    MethodHandle {
        reference_kind: u8,
        reference_index: u16,
    },
    /// A dynamic constant (computed via `invokedynamic` bootstrap methods).
    Dynamic,
}

/// A visitor to visit a Java field.
///
/// The methods of this trait must be called in the following order:
/// `visit_end`.
pub trait FieldVisitor {
    /// Visits the end of the field.
    ///
    /// This method, which is the last one to be called, is used to inform the
    /// visitor that all the annotations and attributes of the field have been visited.
    fn visit_end(&mut self) {}
}

pub trait MethodVisitor {
    /// Starts the visit of the method's code.
    fn visit_code(&mut self) {}

    /// Visits a zero-operand instruction.
    ///
    /// # Arguments
    /// * `opcode` - The opcode of the instruction to be visited.
    fn visit_insn(&mut self, _opcode: u8) {}

    /// Visits an instruction with a single int operand.
    fn visit_int_insn(&mut self, _opcode: u8, _operand: i32) {}

    /// Visits a local variable instruction.
    fn visit_var_insn(&mut self, _opcode: u8, _var_index: u16) {}

    /// Visits a type instruction.
    ///
    /// # Arguments
    /// * `opcode` - The opcode of the instruction.
    /// * `type_name` - The internal name of the object or array class.
    fn visit_type_insn(&mut self, _opcode: u8, _type_name: &str) {}

    /// Visits a field instruction.
    ///
    /// # Arguments
    /// * `opcode` - The opcode of the instruction.
    /// * `owner` - The internal name of the field's owner class.
    /// * `name` - The field's name.
    /// * `desc` - The field's descriptor.
    fn visit_field_insn(&mut self, _opcode: u8, _owner: &str, _name: &str, _desc: &str) {}
    fn visit_method_insn(
        &mut self,
        _opcode: u8,
        _owner: &str,
        _name: &str,
        _desc: &str,
        _is_interface: bool,
    ) {
    }
    fn visit_invoke_dynamic_insn(&mut self, _name: &str, _desc: &str) {}
    /// Visits a jump instruction.
    ///
    /// # Arguments
    /// * `opcode` - The opcode of the instruction.
    /// * `target_offset` - The offset of the target instruction relative to the current instruction.
    fn visit_jump_insn(&mut self, _opcode: u8, _target_offset: i32) {}

    /// Visits an `LDC` instruction.
    fn visit_ldc_insn(&mut self, _value: LdcConstant) {}
    fn visit_iinc_insn(&mut self, _var_index: u16, _increment: i16) {}
    fn visit_table_switch(&mut self, _default: i32, _low: i32, _high: i32, _targets: &[i32]) {}
    fn visit_lookup_switch(&mut self, _default: i32, _pairs: &[(i32, i32)]) {}
    fn visit_multi_anewarray_insn(&mut self, _type_name: &str, _dims: u8) {}
    fn visit_maxs(&mut self, _max_stack: u16, _max_locals: u16) {}
    fn visit_end(&mut self) {}
}

/// A visitor to visit a JPMS module descriptor.
pub trait ModuleVisitor {
    fn visit_main_class(&mut self, _main_class: &str) {}
    fn visit_package(&mut self, _package: &str) {}
    fn visit_require(&mut self, _module: &str, _access_flags: u16, _version: Option<&str>) {}
    fn visit_export(&mut self, _package: &str, _access_flags: u16, _modules: &[String]) {}
    fn visit_open(&mut self, _package: &str, _access_flags: u16, _modules: &[String]) {}
    fn visit_use(&mut self, _service: &str) {}
    fn visit_provide(&mut self, _service: &str, _providers: &[String]) {}
    fn visit_end(&mut self) {}
}

/// A visitor to visit a Java class.
///
/// The methods of this trait must be called in the following order:
/// `visit` -> `visit_source` -> `visit_module` -> (`visit_field` | `visit_method`)* -> `visit_end`.
pub trait ClassVisitor {
    /// Visits the header of the class.
    ///
    /// # Arguments
    /// * `major` - The major version number of the class file.
    /// * `minor` - The minor version number of the class file.
    /// * `access_flags` - The class's access flags (see `Opcodes`).
    /// * `name` - The internal name of the class.
    /// * `super_name` - The internal name of the super class (e.g., `java/lang/String`, `a/b/c`).
    ///   Use `None` for `Object`.
    /// * `interfaces` - The internal names of the class's interfaces.
    fn visit(
        &mut self,
        _major: u16,
        _minor: u16,
        _access_flags: u16,
        _name: &str,
        _super_name: Option<&str>,
        _interfaces: &[String],
    ) {
    }

    /// Visits the source file name of the class.
    fn visit_source(&mut self, _source: &str) {}

    /// Visits the JPMS module descriptor of this class, if this is a `module-info.class`.
    fn visit_module(
        &mut self,
        _name: &str,
        _access_flags: u16,
        _version: Option<&str>,
    ) -> Option<Box<dyn ModuleVisitor>> {
        None
    }

    /// Visits a field of the class.
    ///
    /// Returns an optional `FieldVisitor` to visit the field's content.
    fn visit_field(
        &mut self,
        _access_flags: u16,
        _name: &str,
        _descriptor: &str,
    ) -> Option<Box<dyn FieldVisitor>> {
        None
    }

    /// Visits a method of the class.
    ///
    /// Returns an optional `MethodVisitor` to visit the method's code.
    fn visit_method(
        &mut self,
        _access_flags: u16,
        _name: &str,
        _descriptor: &str,
    ) -> Option<Box<dyn MethodVisitor>> {
        None
    }

    /// Visits the end of the class.
    fn visit_end(&mut self) {}
}

/// A parser to make a [`ClassVisitor`] visit a `ClassFile` structure.
///
/// This class parses a byte array conforming to the Java class file format and
/// calls the appropriate methods of a given class visitor for each field, method,
/// and bytecode instruction encountered.
pub struct ClassReader {
    bytes: Vec<u8>,
}

impl ClassReader {
    /// Constructs a new `ClassReader` with the given class file bytes.
    ///
    /// # Arguments
    ///
    /// * `bytes` - A byte slice containing the JVM class file data.
    pub fn new(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
        }
    }

    /// Makes the given visitor visit the Java class of this `ClassReader`.
    ///
    /// This method parses the class file data and drives the visitor events.
    ///
    /// # Arguments
    ///
    /// * `visitor` - The visitor that must visit this class.
    /// * `_options` - Option flags (currently unused, reserve for future parsing options like skipping debug info).
    ///
    /// # Errors
    ///
    /// Returns a [`ClassReadError`] if the class file is malformed or contains unsupported versions.
    pub fn accept(
        &self,
        visitor: &mut dyn ClassVisitor,
        _options: u32,
    ) -> Result<(), ClassReadError> {
        let class_file = read_class_file(&self.bytes)?;
        let name = class_file.class_name(class_file.this_class)?.to_string();
        let super_name = if class_file.super_class == 0 {
            None
        } else {
            Some(class_file.class_name(class_file.super_class)?.to_string())
        };
        let mut interfaces = Vec::with_capacity(class_file.interfaces.len());
        for index in &class_file.interfaces {
            interfaces.push(class_file.class_name(*index)?.to_string());
        }

        visitor.visit(
            class_file.major_version,
            class_file.minor_version,
            class_file.access_flags,
            &name,
            super_name.as_deref(),
            &interfaces,
        );

        for attr in &class_file.attributes {
            if let AttributeInfo::SourceFile { sourcefile_index } = attr {
                let source = class_file.cp_utf8(*sourcefile_index)?;
                visitor.visit_source(source);
            }
        }

        if let Some(module_attr) = class_file.attributes.iter().find_map(|attr| match attr {
            AttributeInfo::Module(module) => Some(module),
            _ => None,
        }) {
            let version = if module_attr.module_version_index == 0 {
                None
            } else {
                Some(class_file.cp_utf8(module_attr.module_version_index)?)
            };
            if let Some(mut mv) = visitor.visit_module(
                class_file.module_name(module_attr.module_name_index)?,
                module_attr.module_flags,
                version,
            ) {
                if let Some(main_class_index) =
                    class_file.attributes.iter().find_map(|attr| match attr {
                        AttributeInfo::ModuleMainClass { main_class_index } => {
                            Some(*main_class_index)
                        }
                        _ => None,
                    })
                {
                    mv.visit_main_class(class_file.class_name(main_class_index)?);
                }
                if let Some(package_index_table) =
                    class_file.attributes.iter().find_map(|attr| match attr {
                        AttributeInfo::ModulePackages {
                            package_index_table,
                        } => Some(package_index_table.as_slice()),
                        _ => None,
                    })
                {
                    for package_index in package_index_table {
                        mv.visit_package(class_file.package_name(*package_index)?);
                    }
                }
                for require in &module_attr.requires {
                    let version = if require.requires_version_index == 0 {
                        None
                    } else {
                        Some(class_file.cp_utf8(require.requires_version_index)?)
                    };
                    mv.visit_require(
                        class_file.module_name(require.requires_index)?,
                        require.requires_flags,
                        version,
                    );
                }
                for export in &module_attr.exports {
                    let modules = export
                        .exports_to_index
                        .iter()
                        .map(|index| class_file.module_name(*index).map(str::to_string))
                        .collect::<Result<Vec<_>, _>>()?;
                    mv.visit_export(
                        class_file.package_name(export.exports_index)?,
                        export.exports_flags,
                        &modules,
                    );
                }
                for open in &module_attr.opens {
                    let modules = open
                        .opens_to_index
                        .iter()
                        .map(|index| class_file.module_name(*index).map(str::to_string))
                        .collect::<Result<Vec<_>, _>>()?;
                    mv.visit_open(
                        class_file.package_name(open.opens_index)?,
                        open.opens_flags,
                        &modules,
                    );
                }
                for uses_index in &module_attr.uses_index {
                    mv.visit_use(class_file.class_name(*uses_index)?);
                }
                for provide in &module_attr.provides {
                    let providers = provide
                        .provides_with_index
                        .iter()
                        .map(|index| class_file.class_name(*index).map(str::to_string))
                        .collect::<Result<Vec<_>, _>>()?;
                    mv.visit_provide(class_file.class_name(provide.provides_index)?, &providers);
                }
                mv.visit_end();
            }
        }

        for field in &class_file.fields {
            let field_name = class_file.cp_utf8(field.name_index)?;
            let field_desc = class_file.cp_utf8(field.descriptor_index)?;
            if let Some(mut fv) = visitor.visit_field(field.access_flags, field_name, field_desc) {
                fv.visit_end();
            }
        }

        for method in &class_file.methods {
            let method_name = class_file.cp_utf8(method.name_index)?;
            let method_desc = class_file.cp_utf8(method.descriptor_index)?;
            if let Some(mut mv) =
                visitor.visit_method(method.access_flags, method_name, method_desc)
            {
                let code = method.attributes.iter().find_map(|attr| match attr {
                    AttributeInfo::Code(code) => Some(code),
                    _ => None,
                });
                if let Some(code) = code {
                    mv.visit_code();
                    let instructions = parse_code_instructions_with_offsets(&code.code)?;
                    for instruction in instructions {
                        visit_instruction(
                            &class_file.constant_pool,
                            instruction.offset as i32,
                            instruction.insn,
                            &mut *mv,
                        )?;
                    }
                    mv.visit_maxs(code.max_stack, code.max_locals);
                }
                mv.visit_end();
            }
        }

        visitor.visit_end();
        Ok(())
    }

    /// Converts the read class data directly into a `ClassNode`.
    ///
    /// This is a convenience method that parses the bytes and builds a
    /// complete object model of the class.
    pub fn to_class_node(&self) -> Result<crate::nodes::ClassNode, ClassReadError> {
        let class_file = read_class_file(&self.bytes)?;
        class_file.to_class_node()
    }
}

#[derive(Debug, Clone)]
pub struct ClassFile {
    pub minor_version: u16,
    pub major_version: u16,
    pub constant_pool: Vec<CpInfo>,
    pub access_flags: u16,
    pub this_class: u16,
    pub super_class: u16,
    pub interfaces: Vec<u16>,
    pub fields: Vec<FieldInfo>,
    pub methods: Vec<MethodInfo>,
    pub attributes: Vec<AttributeInfo>,
}

impl ClassFile {
    pub fn cp_utf8(&self, index: u16) -> Result<&str, ClassReadError> {
        match self
            .constant_pool
            .get(index as usize)
            .ok_or(ClassReadError::InvalidIndex(index))?
        {
            CpInfo::Utf8(value) => Ok(value.as_str()),
            _ => Err(ClassReadError::InvalidIndex(index)),
        }
    }

    pub fn class_name(&self, index: u16) -> Result<&str, ClassReadError> {
        match self
            .constant_pool
            .get(index as usize)
            .ok_or(ClassReadError::InvalidIndex(index))?
        {
            CpInfo::Class { name_index } => self.cp_utf8(*name_index),
            _ => Err(ClassReadError::InvalidIndex(index)),
        }
    }

    pub fn module_name(&self, index: u16) -> Result<&str, ClassReadError> {
        match self
            .constant_pool
            .get(index as usize)
            .ok_or(ClassReadError::InvalidIndex(index))?
        {
            CpInfo::Module { name_index } => self.cp_utf8(*name_index),
            _ => Err(ClassReadError::InvalidIndex(index)),
        }
    }

    pub fn package_name(&self, index: u16) -> Result<&str, ClassReadError> {
        match self
            .constant_pool
            .get(index as usize)
            .ok_or(ClassReadError::InvalidIndex(index))?
        {
            CpInfo::Package { name_index } => self.cp_utf8(*name_index),
            _ => Err(ClassReadError::InvalidIndex(index)),
        }
    }

    pub fn to_class_node(&self) -> Result<crate::nodes::ClassNode, ClassReadError> {
        let name = self.class_name(self.this_class)?.to_string();
        let super_name = if self.super_class == 0 {
            None
        } else {
            Some(self.class_name(self.super_class)?.to_string())
        };
        let source_file = self.attributes.iter().find_map(|attr| match attr {
            AttributeInfo::SourceFile { sourcefile_index } => self
                .cp_utf8(*sourcefile_index)
                .ok()
                .map(|value| value.to_string()),
            _ => None,
        });
        let mut interfaces = Vec::with_capacity(self.interfaces.len());
        for index in &self.interfaces {
            interfaces.push(self.class_name(*index)?.to_string());
        }

        let mut fields = Vec::with_capacity(self.fields.len());
        for field in &self.fields {
            let name = self.cp_utf8(field.name_index)?.to_string();
            let descriptor = self.cp_utf8(field.descriptor_index)?.to_string();
            fields.push(crate::nodes::FieldNode {
                access_flags: field.access_flags,
                name,
                descriptor,
                attributes: field.attributes.clone(),
            });
        }

        let mut methods = Vec::with_capacity(self.methods.len());
        for method in &self.methods {
            let name = self.cp_utf8(method.name_index)?.to_string();
            let descriptor = self.cp_utf8(method.descriptor_index)?.to_string();
            let mut method_attributes = method.attributes.clone();
            method_attributes.retain(|attr| !matches!(attr, AttributeInfo::Code(_)));
            let code = method.attributes.iter().find_map(|attr| match attr {
                AttributeInfo::Code(code) => Some(code),
                _ => None,
            });

            let (
                has_code,
                max_stack,
                max_locals,
                instructions,
                instruction_offsets,
                insn_nodes,
                exception_table,
                try_catch_blocks,
                line_numbers,
                local_variables,
                code_attributes,
            ) = if let Some(code) = code {
                let mut list = InsnList::new();
                let mut instruction_offsets = Vec::with_capacity(code.instructions.len());
                for insn in &code.instructions {
                    list.add(insn.clone());
                }
                for node in parse_code_instructions_with_offsets(&code.code)? {
                    instruction_offsets.push(node.offset);
                }
                let line_numbers = code
                    .attributes
                    .iter()
                    .find_map(|attr| match attr {
                        AttributeInfo::LineNumberTable { entries } => Some(entries.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let local_variables = code
                    .attributes
                    .iter()
                    .find_map(|attr| match attr {
                        AttributeInfo::LocalVariableTable { entries } => Some(entries.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                (
                    true,
                    code.max_stack,
                    code.max_locals,
                    list,
                    instruction_offsets,
                    code.insn_nodes.clone(),
                    code.exception_table.clone(),
                    code.try_catch_blocks.clone(),
                    line_numbers,
                    local_variables,
                    code.attributes.clone(),
                )
            } else {
                (
                    false,
                    0,
                    0,
                    InsnList::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )
            };
            let method_parameters = method
                .attributes
                .iter()
                .find_map(|attr| match attr {
                    AttributeInfo::MethodParameters { parameters } => Some(parameters.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let exceptions = method
                .attributes
                .iter()
                .find_map(|attr| match attr {
                    AttributeInfo::Exceptions {
                        exception_index_table,
                    } => Some(exception_index_table),
                    _ => None,
                })
                .map(|entries| -> Result<Vec<String>, ClassReadError> {
                    let mut values = Vec::with_capacity(entries.len());
                    for index in entries {
                        values.push(self.class_name(*index)?.to_string());
                    }
                    Ok(values)
                })
                .transpose()?
                .unwrap_or_default();
            let signature = method.attributes.iter().find_map(|attr| match attr {
                AttributeInfo::Signature { signature_index } => self
                    .cp_utf8(*signature_index)
                    .ok()
                    .map(|value| value.to_string()),
                _ => None,
            });

            methods.push(crate::nodes::MethodNode {
                access_flags: method.access_flags,
                name,
                descriptor,
                has_code,
                max_stack,
                max_locals,
                instructions,
                instruction_offsets,
                insn_nodes,
                exception_table,
                try_catch_blocks,
                line_numbers,
                local_variables,
                method_parameters,
                exceptions,
                signature,
                code_attributes,
                attributes: method_attributes,
            });
        }

        let mut inner_classes = Vec::new();
        for attr in &self.attributes {
            if let AttributeInfo::InnerClasses { classes } = attr {
                for entry in classes {
                    let name = self.class_name(entry.inner_class_info_index)?.to_string();
                    let outer_name = if entry.outer_class_info_index == 0 {
                        None
                    } else {
                        Some(self.class_name(entry.outer_class_info_index)?.to_string())
                    };
                    let inner_name = if entry.inner_name_index == 0 {
                        None
                    } else {
                        Some(self.cp_utf8(entry.inner_name_index)?.to_string())
                    };
                    inner_classes.push(crate::nodes::InnerClassNode {
                        name,
                        outer_name,
                        inner_name,
                        access_flags: entry.inner_class_access_flags,
                    });
                }
            }
        }

        let mut outer_class = String::new();
        if let Some(class_index) = self.attributes.iter().find_map(|attr| match attr {
            AttributeInfo::EnclosingMethod { class_index, .. } => Some(*class_index),
            _ => None,
        }) {
            outer_class = self.class_name(class_index)?.to_string();
        }
        if outer_class.is_empty() {
            for attr in &self.attributes {
                if let AttributeInfo::InnerClasses { classes } = attr
                    && let Some(entry) = classes.iter().find(|entry| {
                        entry.inner_class_info_index == self.this_class
                            && entry.outer_class_info_index != 0
                    })
                {
                    outer_class = self.class_name(entry.outer_class_info_index)?.to_string();
                    break;
                }
            }
        }

        let module = self
            .attributes
            .iter()
            .find_map(|attr| match attr {
                AttributeInfo::Module(module) => Some(module),
                _ => None,
            })
            .map(|module| {
                let requires = module
                    .requires
                    .iter()
                    .map(|require| {
                        Ok(crate::nodes::ModuleRequireNode {
                            module: self.module_name(require.requires_index)?.to_string(),
                            access_flags: require.requires_flags,
                            version: if require.requires_version_index == 0 {
                                None
                            } else {
                                Some(self.cp_utf8(require.requires_version_index)?.to_string())
                            },
                        })
                    })
                    .collect::<Result<Vec<_>, ClassReadError>>()?;
                let exports = module
                    .exports
                    .iter()
                    .map(|export| {
                        Ok(crate::nodes::ModuleExportNode {
                            package: self.package_name(export.exports_index)?.to_string(),
                            access_flags: export.exports_flags,
                            modules: export
                                .exports_to_index
                                .iter()
                                .map(|index| self.module_name(*index).map(str::to_string))
                                .collect::<Result<Vec<_>, ClassReadError>>()?,
                        })
                    })
                    .collect::<Result<Vec<_>, ClassReadError>>()?;
                let opens = module
                    .opens
                    .iter()
                    .map(|open| {
                        Ok(crate::nodes::ModuleOpenNode {
                            package: self.package_name(open.opens_index)?.to_string(),
                            access_flags: open.opens_flags,
                            modules: open
                                .opens_to_index
                                .iter()
                                .map(|index| self.module_name(*index).map(str::to_string))
                                .collect::<Result<Vec<_>, ClassReadError>>()?,
                        })
                    })
                    .collect::<Result<Vec<_>, ClassReadError>>()?;
                let provides = module
                    .provides
                    .iter()
                    .map(|provide| {
                        Ok(crate::nodes::ModuleProvideNode {
                            service: self.class_name(provide.provides_index)?.to_string(),
                            providers: provide
                                .provides_with_index
                                .iter()
                                .map(|index| self.class_name(*index).map(str::to_string))
                                .collect::<Result<Vec<_>, ClassReadError>>()?,
                        })
                    })
                    .collect::<Result<Vec<_>, ClassReadError>>()?;
                let packages = self
                    .attributes
                    .iter()
                    .find_map(|attr| match attr {
                        AttributeInfo::ModulePackages {
                            package_index_table,
                        } => Some(package_index_table),
                        _ => None,
                    })
                    .map(|package_index_table| {
                        package_index_table
                            .iter()
                            .map(|index| self.package_name(*index).map(str::to_string))
                            .collect::<Result<Vec<_>, ClassReadError>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                let main_class = self
                    .attributes
                    .iter()
                    .find_map(|attr| match attr {
                        AttributeInfo::ModuleMainClass { main_class_index } => {
                            Some(*main_class_index)
                        }
                        _ => None,
                    })
                    .map(|index| self.class_name(index).map(str::to_string))
                    .transpose()?;

                Ok(crate::nodes::ModuleNode {
                    name: self.module_name(module.module_name_index)?.to_string(),
                    access_flags: module.module_flags,
                    version: if module.module_version_index == 0 {
                        None
                    } else {
                        Some(self.cp_utf8(module.module_version_index)?.to_string())
                    },
                    requires,
                    exports,
                    opens,
                    uses: module
                        .uses_index
                        .iter()
                        .map(|index| self.class_name(*index).map(str::to_string))
                        .collect::<Result<Vec<_>, ClassReadError>>()?,
                    provides,
                    packages,
                    main_class,
                })
            })
            .transpose()?;

        let permitted_subclasses = self
            .attributes
            .iter()
            .find_map(|attr| match attr {
                AttributeInfo::PermittedSubclasses { classes } => Some(classes),
                _ => None,
            })
            .map(|classes| {
                classes
                    .iter()
                    .map(|index| self.class_name(*index).map(str::to_string))
                    .collect::<Result<Vec<_>, ClassReadError>>()
            })
            .transpose()?
            .unwrap_or_default();

        let record_components = self
            .attributes
            .iter()
            .find_map(|attr| match attr {
                AttributeInfo::Record { components } => Some(components),
                _ => None,
            })
            .map(|components| {
                components
                    .iter()
                    .map(|comp| {
                        Ok(crate::nodes::RecordComponentNode {
                            name: self.cp_utf8(comp.name_index)?.to_string(),
                            descriptor: self.cp_utf8(comp.descriptor_index)?.to_string(),
                            attributes: comp.attributes.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, ClassReadError>>()
            })
            .transpose()?
            .unwrap_or_default();

        Ok(crate::nodes::ClassNode {
            minor_version: self.minor_version,
            major_version: self.major_version,
            access_flags: self.access_flags,
            constant_pool: self.constant_pool.clone(),
            this_class: self.this_class,
            name,
            super_name,
            source_file,
            interfaces,
            interface_indices: self.interfaces.clone(),
            fields,
            methods,
            attributes: self.attributes.clone(),
            inner_classes,
            outer_class,
            module,
            record_components,
            permitted_subclasses,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub access_flags: u16,
    pub name_index: u16,
    pub descriptor_index: u16,
    pub attributes: Vec<AttributeInfo>,
}

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub access_flags: u16,
    pub name_index: u16,
    pub descriptor_index: u16,
    pub attributes: Vec<AttributeInfo>,
}

#[derive(Debug, Clone)]
pub struct RecordComponent {
    pub name_index: u16,
    pub descriptor_index: u16,
    pub attributes: Vec<AttributeInfo>,
}

#[derive(Debug, Clone)]
pub enum AttributeInfo {
    Code(CodeAttribute),
    ConstantValue { constantvalue_index: u16 },
    Exceptions { exception_index_table: Vec<u16> },
    SourceFile { sourcefile_index: u16 },
    LineNumberTable { entries: Vec<LineNumber> },
    LocalVariableTable { entries: Vec<LocalVariable> },
    Signature { signature_index: u16 },
    StackMapTable { entries: Vec<StackMapFrame> },
    Deprecated,
    Synthetic,
    InnerClasses { classes: Vec<InnerClass> },
    EnclosingMethod { class_index: u16, method_index: u16 },
    Module(ModuleAttribute),
    ModulePackages { package_index_table: Vec<u16> },
    ModuleMainClass { main_class_index: u16 },
    BootstrapMethods { methods: Vec<BootstrapMethod> },
    MethodParameters { parameters: Vec<MethodParameter> },
    RuntimeVisibleAnnotations { annotations: Vec<Annotation> },
    RuntimeInvisibleAnnotations { annotations: Vec<Annotation> },
    RuntimeVisibleParameterAnnotations { parameters: ParameterAnnotations },
    RuntimeInvisibleParameterAnnotations { parameters: ParameterAnnotations },
    RuntimeVisibleTypeAnnotations { annotations: Vec<TypeAnnotation> },
    RuntimeInvisibleTypeAnnotations { annotations: Vec<TypeAnnotation> },
    PermittedSubclasses { classes: Vec<u16> },
    Record { components: Vec<RecordComponent> },
    Unknown { name: String, info: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleAttribute {
    pub module_name_index: u16,
    pub module_flags: u16,
    pub module_version_index: u16,
    pub requires: Vec<ModuleRequire>,
    pub exports: Vec<ModuleExport>,
    pub opens: Vec<ModuleOpen>,
    pub uses_index: Vec<u16>,
    pub provides: Vec<ModuleProvide>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRequire {
    pub requires_index: u16,
    pub requires_flags: u16,
    pub requires_version_index: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleExport {
    pub exports_index: u16,
    pub exports_flags: u16,
    pub exports_to_index: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleOpen {
    pub opens_index: u16,
    pub opens_flags: u16,
    pub opens_to_index: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleProvide {
    pub provides_index: u16,
    pub provides_with_index: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    pub type_descriptor_index: u16,
    pub element_value_pairs: Vec<ElementValuePair>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElementValuePair {
    pub element_name_index: u16,
    pub value: ElementValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElementValue {
    ConstValueIndex {
        tag: u8,
        const_value_index: u16,
    },
    EnumConstValue {
        type_name_index: u16,
        const_name_index: u16,
    },
    ClassInfoIndex {
        class_info_index: u16,
    },
    AnnotationValue(Box<Annotation>),
    ArrayValue(Vec<ElementValue>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParameterAnnotations {
    pub parameters: Vec<Vec<Annotation>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeAnnotation {
    pub target_type: u8,
    pub target_info: TypeAnnotationTargetInfo,
    pub target_path: TypePath,
    pub annotation: Annotation,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypePath {
    pub path: Vec<TypePathEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypePathEntry {
    pub type_path_kind: u8,
    pub type_argument_index: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeAnnotationTargetInfo {
    TypeParameter {
        type_parameter_index: u8,
    },
    Supertype {
        supertype_index: u16,
    },
    TypeParameterBound {
        type_parameter_index: u8,
        bound_index: u8,
    },
    Empty,
    FormalParameter {
        formal_parameter_index: u8,
    },
    Throws {
        throws_type_index: u16,
    },
    LocalVar {
        table: Vec<LocalVarTargetTableEntry>,
    },
    Catch {
        exception_table_index: u16,
    },
    Offset {
        offset: u16,
    },
    TypeArgument {
        offset: u16,
        type_argument_index: u8,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalVarTargetTableEntry {
    pub start_pc: u16,
    pub length: u16,
    pub index: u16,
}

#[derive(Debug, Clone)]
pub struct CodeAttribute {
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: Vec<u8>,
    pub instructions: Vec<Insn>,
    pub insn_nodes: Vec<AbstractInsnNode>,
    pub exception_table: Vec<ExceptionTableEntry>,
    pub try_catch_blocks: Vec<TryCatchBlockNode>,
    pub attributes: Vec<AttributeInfo>,
}

#[derive(Debug, Clone)]
pub struct ExceptionTableEntry {
    pub start_pc: u16,
    pub end_pc: u16,
    pub handler_pc: u16,
    pub catch_type: u16,
}

#[derive(Debug, Clone)]
pub struct LineNumber {
    pub start_pc: u16,
    pub line_number: u16,
}

#[derive(Debug, Clone)]
pub struct LocalVariable {
    pub start_pc: u16,
    pub length: u16,
    pub name_index: u16,
    pub descriptor_index: u16,
    pub index: u16,
}

#[derive(Debug, Clone)]
pub struct InnerClass {
    pub inner_class_info_index: u16,
    pub outer_class_info_index: u16,
    pub inner_name_index: u16,
    pub inner_class_access_flags: u16,
}

#[derive(Debug, Clone)]
pub struct BootstrapMethod {
    pub bootstrap_method_ref: u16,
    pub bootstrap_arguments: Vec<u16>,
}

#[derive(Debug, Clone)]
pub struct MethodParameter {
    pub name_index: u16,
    pub access_flags: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationTypeInfo {
    Top,
    Integer,
    Float,
    Long,
    Double,
    Null,
    UninitializedThis,
    Object { cpool_index: u16 },
    Uninitialized { offset: u16 },
}

#[derive(Debug, Clone)]
pub enum StackMapFrame {
    SameFrame {
        offset_delta: u16,
    },
    SameLocals1StackItemFrame {
        offset_delta: u16,
        stack: VerificationTypeInfo,
    },
    SameLocals1StackItemFrameExtended {
        offset_delta: u16,
        stack: VerificationTypeInfo,
    },
    ChopFrame {
        offset_delta: u16,
        k: u8,
    },
    SameFrameExtended {
        offset_delta: u16,
    },
    AppendFrame {
        offset_delta: u16,
        locals: Vec<VerificationTypeInfo>,
    },
    FullFrame {
        offset_delta: u16,
        locals: Vec<VerificationTypeInfo>,
        stack: Vec<VerificationTypeInfo>,
    },
}

pub fn read_class_file(bytes: &[u8]) -> Result<ClassFile, ClassReadError> {
    let mut reader = ByteReader::new(bytes);
    let magic = reader.read_u4()?;
    if magic != 0xCAFEBABE {
        return Err(ClassReadError::InvalidMagic(magic));
    }
    let minor_version = reader.read_u2()?;
    let major_version = reader.read_u2()?;
    if major_version > constants::V25 {
        return Err(ClassReadError::InvalidClassVersion(major_version));
    }
    let constant_pool = read_constant_pool(&mut reader)?;
    let access_flags = reader.read_u2()?;
    let this_class = reader.read_u2()?;
    let super_class = reader.read_u2()?;
    let interfaces = read_u2_table(&mut reader)?;
    let fields = read_fields(&mut reader, &constant_pool)?;
    let methods = read_methods(&mut reader, &constant_pool)?;
    let attributes = read_attributes(&mut reader, &constant_pool)?;

    Ok(ClassFile {
        minor_version,
        major_version,
        constant_pool,
        access_flags,
        this_class,
        super_class,
        interfaces,
        fields,
        methods,
        attributes,
    })
}

fn read_constant_pool(reader: &mut ByteReader<'_>) -> Result<Vec<CpInfo>, ClassReadError> {
    let count = reader.read_u2()? as usize;
    let mut pool = Vec::with_capacity(count);
    pool.push(CpInfo::Unusable);

    let mut index = 1;
    while index < count {
        let tag = reader.read_u1()?;
        let entry = match tag {
            1 => {
                let len = reader.read_u2()? as usize;
                let bytes = reader.read_bytes(len)?;
                let value = decode_modified_utf8(&bytes)?;
                CpInfo::Utf8(value)
            }
            3 => {
                let value = reader.read_u4()? as i32;
                CpInfo::Integer(value)
            }
            4 => {
                let value = f32::from_bits(reader.read_u4()?);
                CpInfo::Float(value)
            }
            5 => {
                let value = reader.read_u8()? as i64;
                CpInfo::Long(value)
            }
            6 => {
                let value = f64::from_bits(reader.read_u8()?);
                CpInfo::Double(value)
            }
            7 => CpInfo::Class {
                name_index: reader.read_u2()?,
            },
            8 => CpInfo::String {
                string_index: reader.read_u2()?,
            },
            9 => CpInfo::Fieldref {
                class_index: reader.read_u2()?,
                name_and_type_index: reader.read_u2()?,
            },
            10 => CpInfo::Methodref {
                class_index: reader.read_u2()?,
                name_and_type_index: reader.read_u2()?,
            },
            11 => CpInfo::InterfaceMethodref {
                class_index: reader.read_u2()?,
                name_and_type_index: reader.read_u2()?,
            },
            12 => CpInfo::NameAndType {
                name_index: reader.read_u2()?,
                descriptor_index: reader.read_u2()?,
            },
            15 => CpInfo::MethodHandle {
                reference_kind: reader.read_u1()?,
                reference_index: reader.read_u2()?,
            },
            16 => CpInfo::MethodType {
                descriptor_index: reader.read_u2()?,
            },
            17 => CpInfo::Dynamic {
                bootstrap_method_attr_index: reader.read_u2()?,
                name_and_type_index: reader.read_u2()?,
            },
            18 => CpInfo::InvokeDynamic {
                bootstrap_method_attr_index: reader.read_u2()?,
                name_and_type_index: reader.read_u2()?,
            },
            19 => CpInfo::Module {
                name_index: reader.read_u2()?,
            },
            20 => CpInfo::Package {
                name_index: reader.read_u2()?,
            },
            _ => return Err(ClassReadError::InvalidConstantPoolTag(tag)),
        };

        pool.push(entry);

        if tag == 5 || tag == 6 {
            pool.push(CpInfo::Unusable);
            index += 2;
        } else {
            index += 1;
        }
    }

    Ok(pool)
}

fn read_u2_table(reader: &mut ByteReader<'_>) -> Result<Vec<u16>, ClassReadError> {
    let count = reader.read_u2()? as usize;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(reader.read_u2()?);
    }
    Ok(values)
}

fn read_fields(
    reader: &mut ByteReader<'_>,
    cp: &[CpInfo],
) -> Result<Vec<FieldInfo>, ClassReadError> {
    let count = reader.read_u2()? as usize;
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let access_flags = reader.read_u2()?;
        let name_index = reader.read_u2()?;
        let descriptor_index = reader.read_u2()?;
        let attributes = read_attributes(reader, cp)?;
        fields.push(FieldInfo {
            access_flags,
            name_index,
            descriptor_index,
            attributes,
        });
    }
    Ok(fields)
}

fn read_methods(
    reader: &mut ByteReader<'_>,
    cp: &[CpInfo],
) -> Result<Vec<MethodInfo>, ClassReadError> {
    let count = reader.read_u2()? as usize;
    let mut methods = Vec::with_capacity(count);
    for _ in 0..count {
        let access_flags = reader.read_u2()?;
        let name_index = reader.read_u2()?;
        let descriptor_index = reader.read_u2()?;
        let attributes = read_attributes(reader, cp)?;
        methods.push(MethodInfo {
            access_flags,
            name_index,
            descriptor_index,
            attributes,
        });
    }
    Ok(methods)
}

fn read_attributes(
    reader: &mut ByteReader<'_>,
    cp: &[CpInfo],
) -> Result<Vec<AttributeInfo>, ClassReadError> {
    let count = reader.read_u2()? as usize;
    let mut attributes = Vec::with_capacity(count);
    for _ in 0..count {
        let name_index = reader.read_u2()?;
        let length = reader.read_u4()? as usize;
        let name = cp_utf8(cp, name_index)?;
        let info = reader.read_bytes(length)?;
        let attribute = parse_attribute(name, info, cp)?;
        attributes.push(attribute);
    }
    Ok(attributes)
}

fn parse_attribute(
    name: &str,
    info: Vec<u8>,
    cp: &[CpInfo],
) -> Result<AttributeInfo, ClassReadError> {
    let mut reader = ByteReader::new(&info);
    let attribute = match name {
        "Code" => {
            let max_stack = reader.read_u2()?;
            let max_locals = reader.read_u2()?;
            let code_length = reader.read_u4()? as usize;
            let code = reader.read_bytes(code_length)?;
            let instructions = parse_code_instructions(&code)?;
            let exception_table_length = reader.read_u2()? as usize;
            let mut exception_table = Vec::with_capacity(exception_table_length);
            for _ in 0..exception_table_length {
                exception_table.push(ExceptionTableEntry {
                    start_pc: reader.read_u2()?,
                    end_pc: reader.read_u2()?,
                    handler_pc: reader.read_u2()?,
                    catch_type: reader.read_u2()?,
                });
            }
            let attributes = read_attributes(&mut reader, cp)?;
            let (mut insn_nodes, try_catch_blocks, label_by_offset) =
                build_insn_nodes(&code, &exception_table, cp)?;
            let line_numbers = attributes.iter().find_map(|attr| match attr {
                AttributeInfo::LineNumberTable { entries } => Some(entries.as_slice()),
                _ => None,
            });
            if let Some(entries) = line_numbers {
                insn_nodes = attach_line_numbers(insn_nodes, entries, &label_by_offset);
            }
            AttributeInfo::Code(CodeAttribute {
                max_stack,
                max_locals,
                code,
                instructions,
                insn_nodes,
                exception_table,
                try_catch_blocks,
                attributes,
            })
        }
        "ConstantValue" => AttributeInfo::ConstantValue {
            constantvalue_index: reader.read_u2()?,
        },
        "Exceptions" => {
            let count = reader.read_u2()? as usize;
            let mut exception_index_table = Vec::with_capacity(count);
            for _ in 0..count {
                exception_index_table.push(reader.read_u2()?);
            }
            AttributeInfo::Exceptions {
                exception_index_table,
            }
        }
        "SourceFile" => AttributeInfo::SourceFile {
            sourcefile_index: reader.read_u2()?,
        },
        "LineNumberTable" => {
            let count = reader.read_u2()? as usize;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push(LineNumber {
                    start_pc: reader.read_u2()?,
                    line_number: reader.read_u2()?,
                });
            }
            AttributeInfo::LineNumberTable { entries }
        }
        "LocalVariableTable" => {
            let count = reader.read_u2()? as usize;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push(LocalVariable {
                    start_pc: reader.read_u2()?,
                    length: reader.read_u2()?,
                    name_index: reader.read_u2()?,
                    descriptor_index: reader.read_u2()?,
                    index: reader.read_u2()?,
                });
            }
            AttributeInfo::LocalVariableTable { entries }
        }
        "Signature" => AttributeInfo::Signature {
            signature_index: reader.read_u2()?,
        },
        "StackMapTable" => {
            let count = reader.read_u2()? as usize;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let frame_type = reader.read_u1()?;
                let frame = match frame_type {
                    0..=63 => StackMapFrame::SameFrame {
                        offset_delta: frame_type as u16,
                    },
                    64..=127 => StackMapFrame::SameLocals1StackItemFrame {
                        offset_delta: (frame_type - 64) as u16,
                        stack: parse_verification_type(&mut reader)?,
                    },
                    247 => StackMapFrame::SameLocals1StackItemFrameExtended {
                        offset_delta: reader.read_u2()?,
                        stack: parse_verification_type(&mut reader)?,
                    },
                    248..=250 => StackMapFrame::ChopFrame {
                        offset_delta: reader.read_u2()?,
                        k: 251 - frame_type,
                    },
                    251 => StackMapFrame::SameFrameExtended {
                        offset_delta: reader.read_u2()?,
                    },
                    252..=254 => {
                        let offset_delta = reader.read_u2()?;
                        let locals_count = (frame_type - 251) as usize;
                        let mut locals = Vec::with_capacity(locals_count);
                        for _ in 0..locals_count {
                            locals.push(parse_verification_type(&mut reader)?);
                        }
                        StackMapFrame::AppendFrame {
                            offset_delta,
                            locals,
                        }
                    }
                    255 => {
                        let offset_delta = reader.read_u2()?;
                        let locals_count = reader.read_u2()? as usize;
                        let mut locals = Vec::with_capacity(locals_count);
                        for _ in 0..locals_count {
                            locals.push(parse_verification_type(&mut reader)?);
                        }
                        let stack_count = reader.read_u2()? as usize;
                        let mut stack = Vec::with_capacity(stack_count);
                        for _ in 0..stack_count {
                            stack.push(parse_verification_type(&mut reader)?);
                        }
                        StackMapFrame::FullFrame {
                            offset_delta,
                            locals,
                            stack,
                        }
                    }
                    _ => {
                        return Err(ClassReadError::InvalidAttribute(
                            "StackMapTable".to_string(),
                        ));
                    }
                };
                entries.push(frame);
            }
            AttributeInfo::StackMapTable { entries }
        }
        "Deprecated" => AttributeInfo::Deprecated,
        "Synthetic" => AttributeInfo::Synthetic,
        "InnerClasses" => {
            let count = reader.read_u2()? as usize;
            let mut classes = Vec::with_capacity(count);
            for _ in 0..count {
                classes.push(InnerClass {
                    inner_class_info_index: reader.read_u2()?,
                    outer_class_info_index: reader.read_u2()?,
                    inner_name_index: reader.read_u2()?,
                    inner_class_access_flags: reader.read_u2()?,
                });
            }
            AttributeInfo::InnerClasses { classes }
        }
        "EnclosingMethod" => AttributeInfo::EnclosingMethod {
            class_index: reader.read_u2()?,
            method_index: reader.read_u2()?,
        },
        "Module" => AttributeInfo::Module(parse_module_attribute(&mut reader)?),
        "ModulePackages" => {
            let count = reader.read_u2()? as usize;
            let mut package_index_table = Vec::with_capacity(count);
            for _ in 0..count {
                package_index_table.push(reader.read_u2()?);
            }
            AttributeInfo::ModulePackages {
                package_index_table,
            }
        }
        "ModuleMainClass" => AttributeInfo::ModuleMainClass {
            main_class_index: reader.read_u2()?,
        },
        "BootstrapMethods" => {
            let count = reader.read_u2()? as usize;
            let mut methods = Vec::with_capacity(count);
            for _ in 0..count {
                let bootstrap_method_ref = reader.read_u2()?;
                let arg_count = reader.read_u2()? as usize;
                let mut bootstrap_arguments = Vec::with_capacity(arg_count);
                for _ in 0..arg_count {
                    bootstrap_arguments.push(reader.read_u2()?);
                }
                methods.push(BootstrapMethod {
                    bootstrap_method_ref,
                    bootstrap_arguments,
                });
            }
            AttributeInfo::BootstrapMethods { methods }
        }
        "MethodParameters" => {
            let count = reader.read_u1()? as usize;
            let mut parameters = Vec::with_capacity(count);
            for _ in 0..count {
                parameters.push(MethodParameter {
                    name_index: reader.read_u2()?,
                    access_flags: reader.read_u2()?,
                });
            }
            AttributeInfo::MethodParameters { parameters }
        }
        "RuntimeVisibleAnnotations" => {
            let annotations = parse_annotations(&mut reader)?;
            AttributeInfo::RuntimeVisibleAnnotations { annotations }
        }
        "RuntimeInvisibleAnnotations" => {
            let annotations = parse_annotations(&mut reader)?;
            AttributeInfo::RuntimeInvisibleAnnotations { annotations }
        }
        "RuntimeVisibleParameterAnnotations" => {
            let parameters = parse_parameter_annotations(&mut reader)?;
            AttributeInfo::RuntimeVisibleParameterAnnotations { parameters }
        }
        "RuntimeInvisibleParameterAnnotations" => {
            let parameters = parse_parameter_annotations(&mut reader)?;
            AttributeInfo::RuntimeInvisibleParameterAnnotations { parameters }
        }
        "RuntimeVisibleTypeAnnotations" => {
            let annotations = parse_type_annotations(&mut reader)?;
            AttributeInfo::RuntimeVisibleTypeAnnotations { annotations }
        }
        "RuntimeInvisibleTypeAnnotations" => {
            let annotations = parse_type_annotations(&mut reader)?;
            AttributeInfo::RuntimeInvisibleTypeAnnotations { annotations }
        }
        "PermittedSubclasses" => {
            let count = reader.read_u2()? as usize;
            let mut classes = Vec::with_capacity(count);
            for _ in 0..count {
                classes.push(reader.read_u2()?);
            }
            AttributeInfo::PermittedSubclasses { classes }
        }
        "Record" => {
            let count = reader.read_u2()? as usize;
            let mut components = Vec::with_capacity(count);
            for _ in 0..count {
                let name_index = reader.read_u2()?;
                let descriptor_index = reader.read_u2()?;
                let attributes = read_attributes(&mut reader, cp)?;
                components.push(RecordComponent {
                    name_index,
                    descriptor_index,
                    attributes,
                });
            }
            AttributeInfo::Record { components }
        }
        _ => {
            return Ok(AttributeInfo::Unknown {
                name: name.to_string(),
                info,
            });
        }
    };

    if reader.remaining() != 0 {
        return Err(ClassReadError::InvalidAttribute(name.to_string()));
    }

    Ok(attribute)
}

fn parse_module_attribute(reader: &mut ByteReader<'_>) -> Result<ModuleAttribute, ClassReadError> {
    let module_name_index = reader.read_u2()?;
    let module_flags = reader.read_u2()?;
    let module_version_index = reader.read_u2()?;

    let requires_count = reader.read_u2()? as usize;
    let mut requires = Vec::with_capacity(requires_count);
    for _ in 0..requires_count {
        requires.push(ModuleRequire {
            requires_index: reader.read_u2()?,
            requires_flags: reader.read_u2()?,
            requires_version_index: reader.read_u2()?,
        });
    }

    let exports_count = reader.read_u2()? as usize;
    let mut exports = Vec::with_capacity(exports_count);
    for _ in 0..exports_count {
        let exports_index = reader.read_u2()?;
        let exports_flags = reader.read_u2()?;
        let exports_to_count = reader.read_u2()? as usize;
        let mut exports_to_index = Vec::with_capacity(exports_to_count);
        for _ in 0..exports_to_count {
            exports_to_index.push(reader.read_u2()?);
        }
        exports.push(ModuleExport {
            exports_index,
            exports_flags,
            exports_to_index,
        });
    }

    let opens_count = reader.read_u2()? as usize;
    let mut opens = Vec::with_capacity(opens_count);
    for _ in 0..opens_count {
        let opens_index = reader.read_u2()?;
        let opens_flags = reader.read_u2()?;
        let opens_to_count = reader.read_u2()? as usize;
        let mut opens_to_index = Vec::with_capacity(opens_to_count);
        for _ in 0..opens_to_count {
            opens_to_index.push(reader.read_u2()?);
        }
        opens.push(ModuleOpen {
            opens_index,
            opens_flags,
            opens_to_index,
        });
    }

    let uses_count = reader.read_u2()? as usize;
    let mut uses_index = Vec::with_capacity(uses_count);
    for _ in 0..uses_count {
        uses_index.push(reader.read_u2()?);
    }

    let provides_count = reader.read_u2()? as usize;
    let mut provides = Vec::with_capacity(provides_count);
    for _ in 0..provides_count {
        let provides_index = reader.read_u2()?;
        let provides_with_count = reader.read_u2()? as usize;
        let mut provides_with_index = Vec::with_capacity(provides_with_count);
        for _ in 0..provides_with_count {
            provides_with_index.push(reader.read_u2()?);
        }
        provides.push(ModuleProvide {
            provides_index,
            provides_with_index,
        });
    }

    Ok(ModuleAttribute {
        module_name_index,
        module_flags,
        module_version_index,
        requires,
        exports,
        opens,
        uses_index,
        provides,
    })
}

fn parse_annotations(reader: &mut ByteReader) -> Result<Vec<Annotation>, ClassReadError> {
    let num = reader.read_u2()? as usize;
    let mut out = Vec::with_capacity(num);
    for _ in 0..num {
        out.push(parse_annotation(reader)?);
    }
    Ok(out)
}

fn parse_annotation(reader: &mut ByteReader) -> Result<Annotation, ClassReadError> {
    let type_descriptor_index = reader.read_u2()?;
    let num_pairs = reader.read_u2()? as usize;
    let mut element_value_pairs = Vec::with_capacity(num_pairs);
    for _ in 0..num_pairs {
        let element_name_index = reader.read_u2()?;
        let value = parse_element_value(reader)?;
        element_value_pairs.push(ElementValuePair {
            element_name_index,
            value,
        });
    }
    Ok(Annotation {
        type_descriptor_index,
        element_value_pairs,
    })
}

fn parse_parameter_annotations(
    reader: &mut ByteReader,
) -> Result<ParameterAnnotations, ClassReadError> {
    let num_params = reader.read_u1()? as usize;
    let mut parameters = Vec::with_capacity(num_params);
    for _ in 0..num_params {
        let num_ann = reader.read_u2()? as usize;
        let mut anns = Vec::with_capacity(num_ann);
        for _ in 0..num_ann {
            anns.push(parse_annotation(reader)?);
        }
        parameters.push(anns);
    }
    Ok(ParameterAnnotations { parameters })
}

fn parse_type_annotations(reader: &mut ByteReader) -> Result<Vec<TypeAnnotation>, ClassReadError> {
    let num = reader.read_u2()? as usize;
    let mut out = Vec::with_capacity(num);
    for _ in 0..num {
        out.push(parse_type_annotation(reader)?);
    }
    Ok(out)
}

fn parse_type_annotation(reader: &mut ByteReader) -> Result<TypeAnnotation, ClassReadError> {
    let target_type = reader.read_u1()?;
    let target_info = parse_type_annotation_target_info(reader, target_type)?;
    let target_path = parse_type_path(reader)?;
    let annotation = parse_annotation(reader)?;
    Ok(TypeAnnotation {
        target_type,
        target_info,
        target_path,
        annotation,
    })
}

fn parse_type_path(reader: &mut ByteReader) -> Result<TypePath, ClassReadError> {
    let path_length = reader.read_u1()? as usize;
    let mut path = Vec::with_capacity(path_length);
    for _ in 0..path_length {
        path.push(TypePathEntry {
            type_path_kind: reader.read_u1()?,
            type_argument_index: reader.read_u1()?,
        });
    }
    Ok(TypePath { path })
}

fn parse_type_annotation_target_info(
    reader: &mut ByteReader,
    target_type: u8,
) -> Result<TypeAnnotationTargetInfo, ClassReadError> {
    use crate::constants::*;

    let ti = match target_type {
        TA_TARGET_CLASS_TYPE_PARAMETER | TA_TARGET_METHOD_TYPE_PARAMETER => {
            TypeAnnotationTargetInfo::TypeParameter {
                type_parameter_index: reader.read_u1()?,
            }
        }

        TA_TARGET_CLASS_EXTENDS => TypeAnnotationTargetInfo::Supertype {
            supertype_index: reader.read_u2()?,
        },

        TA_TARGET_CLASS_TYPE_PARAMETER_BOUND | TA_TARGET_METHOD_TYPE_PARAMETER_BOUND => {
            TypeAnnotationTargetInfo::TypeParameterBound {
                type_parameter_index: reader.read_u1()?,
                bound_index: reader.read_u1()?,
            }
        }

        TA_TARGET_FIELD | TA_TARGET_METHOD_RETURN | TA_TARGET_METHOD_RECEIVER => {
            TypeAnnotationTargetInfo::Empty
        }

        TA_TARGET_METHOD_FORMAL_PARAMETER => TypeAnnotationTargetInfo::FormalParameter {
            formal_parameter_index: reader.read_u1()?,
        },

        TA_TARGET_THROWS => TypeAnnotationTargetInfo::Throws {
            throws_type_index: reader.read_u2()?,
        },

        TA_TARGET_LOCAL_VARIABLE | TA_TARGET_RESOURCE_VARIABLE => {
            let table_length = reader.read_u2()? as usize;
            let mut table = Vec::with_capacity(table_length);
            for _ in 0..table_length {
                table.push(LocalVarTargetTableEntry {
                    start_pc: reader.read_u2()?,
                    length: reader.read_u2()?,
                    index: reader.read_u2()?,
                });
            }
            TypeAnnotationTargetInfo::LocalVar { table }
        }

        TA_TARGET_EXCEPTION_PARAMETER => TypeAnnotationTargetInfo::Catch {
            exception_table_index: reader.read_u2()?,
        },

        TA_TARGET_INSTANCEOF
        | TA_TARGET_NEW
        | TA_TARGET_CONSTRUCTOR_REFERENCE_RECEIVER
        | TA_TARGET_METHOD_REFERENCE_RECEIVER => TypeAnnotationTargetInfo::Offset {
            offset: reader.read_u2()?,
        },

        TA_TARGET_CAST
        | TA_TARGET_CONSTRUCTOR_INVOCATION_TYPE_ARGUMENT
        | TA_TARGET_METHOD_INVOCATION_TYPE_ARGUMENT
        | TA_TARGET_CONSTRUCTOR_REFERENCE_TYPE_ARGUMENT
        | TA_TARGET_METHOD_REFERENCE_TYPE_ARGUMENT => TypeAnnotationTargetInfo::TypeArgument {
            offset: reader.read_u2()?,
            type_argument_index: reader.read_u1()?,
        },

        _ => {
            return Err(ClassReadError::InvalidAttribute(format!(
                "TypeAnnotationTargetInfo(target_type=0x{:02X})",
                target_type
            )));
        }
    };

    Ok(ti)
}

fn parse_element_value(reader: &mut ByteReader) -> Result<ElementValue, ClassReadError> {
    let tag = reader.read_u1()?;
    let v = match tag {
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b's' => {
            ElementValue::ConstValueIndex {
                tag,
                const_value_index: reader.read_u2()?,
            }
        }
        b'e' => ElementValue::EnumConstValue {
            type_name_index: reader.read_u2()?,
            const_name_index: reader.read_u2()?,
        },
        b'c' => ElementValue::ClassInfoIndex {
            class_info_index: reader.read_u2()?,
        },
        b'@' => ElementValue::AnnotationValue(Box::new(parse_annotation(reader)?)),
        b'[' => {
            let n = reader.read_u2()? as usize;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(parse_element_value(reader)?);
            }
            ElementValue::ArrayValue(items)
        }
        _ => {
            return Err(ClassReadError::InvalidAttribute(
                "AnnotationElementValue".to_string(),
            ));
        }
    };
    Ok(v)
}

fn parse_verification_type(
    reader: &mut ByteReader<'_>,
) -> Result<VerificationTypeInfo, ClassReadError> {
    let tag = reader.read_u1()?;
    let kind = match tag {
        0 => VerificationTypeInfo::Top,
        1 => VerificationTypeInfo::Integer,
        2 => VerificationTypeInfo::Float,
        3 => VerificationTypeInfo::Double,
        4 => VerificationTypeInfo::Long,
        5 => VerificationTypeInfo::Null,
        6 => VerificationTypeInfo::UninitializedThis,
        7 => VerificationTypeInfo::Object {
            cpool_index: reader.read_u2()?,
        },
        8 => VerificationTypeInfo::Uninitialized {
            offset: reader.read_u2()?,
        },
        _ => {
            return Err(ClassReadError::InvalidAttribute(
                "StackMapTable".to_string(),
            ));
        }
    };
    Ok(kind)
}

fn cp_utf8(cp: &[CpInfo], index: u16) -> Result<&str, ClassReadError> {
    match cp.get(index as usize) {
        Some(CpInfo::Utf8(value)) => Ok(value.as_str()),
        _ => Err(ClassReadError::InvalidIndex(index)),
    }
}

fn cp_class_name(cp: &[CpInfo], index: u16) -> Result<&str, ClassReadError> {
    match cp.get(index as usize) {
        Some(CpInfo::Class { name_index }) => cp_utf8(cp, *name_index),
        _ => Err(ClassReadError::InvalidIndex(index)),
    }
}

fn cp_name_and_type(cp: &[CpInfo], index: u16) -> Result<(&str, &str), ClassReadError> {
    match cp.get(index as usize) {
        Some(CpInfo::NameAndType {
            name_index,
            descriptor_index,
        }) => Ok((cp_utf8(cp, *name_index)?, cp_utf8(cp, *descriptor_index)?)),
        _ => Err(ClassReadError::InvalidIndex(index)),
    }
}

fn cp_field_ref(cp: &[CpInfo], index: u16) -> Result<(&str, &str, &str), ClassReadError> {
    match cp.get(index as usize) {
        Some(CpInfo::Fieldref {
            class_index,
            name_and_type_index,
        }) => {
            let owner = cp_class_name(cp, *class_index)?;
            let (name, desc) = cp_name_and_type(cp, *name_and_type_index)?;
            Ok((owner, name, desc))
        }
        _ => Err(ClassReadError::InvalidIndex(index)),
    }
}

fn cp_method_ref(cp: &[CpInfo], index: u16) -> Result<(&str, &str, &str, bool), ClassReadError> {
    match cp.get(index as usize) {
        Some(CpInfo::Methodref {
            class_index,
            name_and_type_index,
        }) => {
            let owner = cp_class_name(cp, *class_index)?;
            let (name, desc) = cp_name_and_type(cp, *name_and_type_index)?;
            Ok((owner, name, desc, false))
        }
        Some(CpInfo::InterfaceMethodref {
            class_index,
            name_and_type_index,
        }) => {
            let owner = cp_class_name(cp, *class_index)?;
            let (name, desc) = cp_name_and_type(cp, *name_and_type_index)?;
            Ok((owner, name, desc, true))
        }
        _ => Err(ClassReadError::InvalidIndex(index)),
    }
}

fn cp_invoke_dynamic(cp: &[CpInfo], index: u16) -> Result<(&str, &str), ClassReadError> {
    match cp.get(index as usize) {
        Some(CpInfo::InvokeDynamic {
            name_and_type_index,
            ..
        }) => cp_name_and_type(cp, *name_and_type_index),
        _ => Err(ClassReadError::InvalidIndex(index)),
    }
}

fn cp_ldc_constant(cp: &[CpInfo], index: u16) -> Result<LdcConstant, ClassReadError> {
    match cp.get(index as usize) {
        Some(CpInfo::Integer(value)) => Ok(LdcConstant::Integer(*value)),
        Some(CpInfo::Float(value)) => Ok(LdcConstant::Float(*value)),
        Some(CpInfo::Long(value)) => Ok(LdcConstant::Long(*value)),
        Some(CpInfo::Double(value)) => Ok(LdcConstant::Double(*value)),
        Some(CpInfo::String { string_index }) => {
            Ok(LdcConstant::String(cp_utf8(cp, *string_index)?.to_string()))
        }
        Some(CpInfo::Class { name_index }) => {
            Ok(LdcConstant::Class(cp_utf8(cp, *name_index)?.to_string()))
        }
        Some(CpInfo::MethodType { descriptor_index }) => Ok(LdcConstant::MethodType(
            cp_utf8(cp, *descriptor_index)?.to_string(),
        )),
        Some(CpInfo::MethodHandle {
            reference_kind,
            reference_index,
        }) => Ok(LdcConstant::MethodHandle {
            reference_kind: *reference_kind,
            reference_index: *reference_index,
        }),
        Some(CpInfo::Dynamic { .. }) => Ok(LdcConstant::Dynamic),
        _ => Err(ClassReadError::InvalidIndex(index)),
    }
}

fn decode_modified_utf8(bytes: &[u8]) -> Result<String, ClassReadError> {
    let mut code_units = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte & 0x80 == 0 {
            code_units.push(byte as u16);
            i += 1;
        } else if byte & 0xE0 == 0xC0 {
            if i + 1 >= bytes.len() {
                return Err(ClassReadError::Utf8Error("truncated 2-byte".to_string()));
            }
            let byte2 = bytes[i + 1];
            if byte2 & 0xC0 != 0x80 {
                return Err(ClassReadError::Utf8Error("invalid 2-byte".to_string()));
            }
            let value = (((byte & 0x1F) as u16) << 6) | ((byte2 & 0x3F) as u16);
            code_units.push(value);
            i += 2;
        } else if byte & 0xF0 == 0xE0 {
            if i + 2 >= bytes.len() {
                return Err(ClassReadError::Utf8Error("truncated 3-byte".to_string()));
            }
            let byte2 = bytes[i + 1];
            let byte3 = bytes[i + 2];
            if byte2 & 0xC0 != 0x80 || byte3 & 0xC0 != 0x80 {
                return Err(ClassReadError::Utf8Error("invalid 3-byte".to_string()));
            }
            let value = (((byte & 0x0F) as u16) << 12)
                | (((byte2 & 0x3F) as u16) << 6)
                | ((byte3 & 0x3F) as u16);
            code_units.push(value);
            i += 3;
        } else {
            return Err(ClassReadError::Utf8Error(
                "invalid leading byte".to_string(),
            ));
        }
    }

    String::from_utf16(&code_units)
        .map_err(|_| ClassReadError::Utf8Error("invalid utf16".to_string()))
}

fn parse_code_instructions(code: &[u8]) -> Result<Vec<Insn>, ClassReadError> {
    let mut reader = ByteReader::new(code);
    let mut insns = Vec::new();

    while reader.remaining() > 0 {
        let opcode_offset = reader.pos();
        let opcode = reader.read_u1()?;
        let insn = match opcode {
            opcodes::NOP..=opcodes::DCONST_1 => Insn::Simple(opcode.into()),
            opcodes::BIPUSH => Insn::Int(IntInsnNode {
                insn: opcode.into(),
                operand: reader.read_i1()? as i32,
            }),
            opcodes::SIPUSH => Insn::Int(IntInsnNode {
                insn: opcode.into(),
                operand: reader.read_i2()? as i32,
            }),
            opcodes::LDC => Insn::Ldc(LdcInsnNode {
                insn: opcode.into(),
                value: LdcValue::Index(reader.read_u1()? as u16),
            }),
            opcodes::LDC_W | opcodes::LDC2_W => Insn::Ldc(LdcInsnNode {
                insn: opcode.into(),
                value: LdcValue::Index(reader.read_u2()?),
            }),
            opcodes::ILOAD..=opcodes::ALOAD => Insn::Var(VarInsnNode {
                insn: opcode.into(),
                var_index: reader.read_u1()? as u16,
            }),
            opcodes::ILOAD_0..=opcodes::SALOAD => Insn::Simple(opcode.into()),
            opcodes::ISTORE..=opcodes::ASTORE => Insn::Var(VarInsnNode {
                insn: opcode.into(),
                var_index: reader.read_u1()? as u16,
            }),
            opcodes::ISTORE_0..=opcodes::SASTORE => Insn::Simple(opcode.into()),
            opcodes::POP..=opcodes::LXOR => Insn::Simple(opcode.into()),
            opcodes::IINC => Insn::Iinc(IincInsnNode {
                insn: opcode.into(),
                var_index: reader.read_u1()? as u16,
                increment: reader.read_i1()? as i16,
            }),
            opcodes::I2L..=opcodes::DCMPG => Insn::Simple(opcode.into()),
            opcodes::IFEQ..=opcodes::JSR => Insn::Jump(JumpInsnNode {
                insn: opcode.into(),
                offset: reader.read_i2()? as i32,
            }),
            opcodes::RET => Insn::Var(VarInsnNode {
                insn: opcode.into(),
                var_index: reader.read_u1()? as u16,
            }),
            opcodes::TABLESWITCH => read_table_switch(&mut reader, opcode_offset)?,
            opcodes::LOOKUPSWITCH => read_lookup_switch(&mut reader, opcode_offset)?,
            opcodes::IRETURN..=opcodes::RETURN => Insn::Simple(InsnNode { opcode }),
            opcodes::GETSTATIC..=opcodes::PUTFIELD => Insn::Field(FieldInsnNode {
                insn: opcode.into(),
                field_ref: MemberRef::Index(reader.read_u2()?),
            }),
            opcodes::INVOKEVIRTUAL..=opcodes::INVOKESTATIC => Insn::Method(MethodInsnNode {
                insn: opcode.into(),
                method_ref: MemberRef::Index(reader.read_u2()?),
            }),
            opcodes::INVOKEINTERFACE => {
                let method_index = reader.read_u2()?;
                let count = reader.read_u1()?;
                let _ = reader.read_u1()?;
                Insn::InvokeInterface(InvokeInterfaceInsnNode {
                    insn: opcode.into(),
                    method_index,
                    count,
                })
            }
            opcodes::INVOKEDYNAMIC => {
                let method_index = reader.read_u2()?;
                let _ = reader.read_u2()?;
                Insn::InvokeDynamic(InvokeDynamicInsnNode::from_index(method_index))
            }
            opcodes::NEW => Insn::Type(TypeInsnNode {
                insn: opcode.into(),
                type_index: reader.read_u2()?,
            }),
            opcodes::NEWARRAY => Insn::Int(IntInsnNode {
                insn: opcode.into(),
                operand: reader.read_u1()? as i32,
            }),
            opcodes::ANEWARRAY => Insn::Type(TypeInsnNode {
                insn: opcode.into(),
                type_index: reader.read_u2()?,
            }),
            opcodes::ARRAYLENGTH | opcodes::ATHROW => Insn::Simple(opcode.into()),
            opcodes::CHECKCAST | opcodes::INSTANCEOF => Insn::Type(TypeInsnNode {
                insn: opcode.into(),
                type_index: reader.read_u2()?,
            }),
            opcodes::MONITORENTER | opcodes::MONITOREXIT => Insn::Simple(opcode.into()),
            opcodes::WIDE => read_wide(&mut reader)?,
            opcodes::MULTIANEWARRAY => Insn::MultiANewArray(MultiANewArrayInsnNode {
                insn: opcode.into(),
                type_index: reader.read_u2()?,
                dimensions: reader.read_u1()?,
            }),
            opcodes::IFNULL | opcodes::IFNONNULL => Insn::Jump(JumpInsnNode {
                insn: opcode.into(),
                offset: reader.read_i2()? as i32,
            }),
            opcodes::GOTO_W | opcodes::JSR_W => Insn::Jump(JumpInsnNode {
                insn: opcode.into(),
                offset: reader.read_i4()?,
            }),
            opcodes::BREAKPOINT => Insn::Simple(opcode.into()),
            opcodes::IMPDEP1 | opcodes::IMPDEP2 => Insn::Simple(opcode.into()),
            _ => {
                return Err(ClassReadError::InvalidOpcode {
                    opcode,
                    offset: opcode_offset,
                });
            }
        };

        insns.push(insn);
    }

    Ok(insns)
}

pub(crate) fn parse_code_instructions_public(code: &[u8]) -> Result<Vec<Insn>, ClassReadError> {
    parse_code_instructions(code)
}

#[derive(Debug, Clone)]
struct ParsedInstruction {
    offset: u16,
    insn: Insn,
}

fn parse_code_instructions_with_offsets(
    code: &[u8],
) -> Result<Vec<ParsedInstruction>, ClassReadError> {
    let mut reader = ByteReader::new(code);
    let mut insns = Vec::new();

    while reader.remaining() > 0 {
        let opcode_offset = reader.pos();
        let opcode = reader.read_u1()?;
        let insn = match opcode {
            opcodes::NOP..=opcodes::DCONST_1 => Insn::Simple(opcode.into()),
            opcodes::BIPUSH => Insn::Int(IntInsnNode {
                insn: opcode.into(),
                operand: reader.read_i1()? as i32,
            }),
            opcodes::SIPUSH => Insn::Int(IntInsnNode {
                insn: opcode.into(),
                operand: reader.read_i2()? as i32,
            }),
            opcodes::LDC => Insn::Ldc(LdcInsnNode {
                insn: opcode.into(),
                value: LdcValue::Index(reader.read_u1()? as u16),
            }),
            opcodes::LDC_W | opcodes::LDC2_W => Insn::Ldc(LdcInsnNode {
                insn: opcode.into(),
                value: LdcValue::Index(reader.read_u2()?),
            }),
            opcodes::ILOAD..=opcodes::ALOAD => Insn::Var(VarInsnNode {
                insn: opcode.into(),
                var_index: reader.read_u1()? as u16,
            }),
            opcodes::ILOAD_0..=opcodes::SALOAD => Insn::Simple(opcode.into()),
            opcodes::ISTORE..=opcodes::ASTORE => Insn::Var(VarInsnNode {
                insn: opcode.into(),
                var_index: reader.read_u1()? as u16,
            }),
            opcodes::ISTORE_0..=opcodes::SASTORE => Insn::Simple(opcode.into()),
            opcodes::POP..=opcodes::LXOR => Insn::Simple(opcode.into()),
            opcodes::IINC => Insn::Iinc(IincInsnNode {
                insn: opcode.into(),
                var_index: reader.read_u1()? as u16,
                increment: reader.read_i1()? as i16,
            }),
            opcodes::I2L..=opcodes::DCMPG => Insn::Simple(opcode.into()),
            opcodes::IFEQ..=opcodes::JSR => Insn::Jump(JumpInsnNode {
                insn: opcode.into(),
                offset: reader.read_i2()? as i32,
            }),
            opcodes::RET => Insn::Var(VarInsnNode {
                insn: opcode.into(),
                var_index: reader.read_u1()? as u16,
            }),
            opcodes::TABLESWITCH => read_table_switch(&mut reader, opcode_offset)?,
            opcodes::LOOKUPSWITCH => read_lookup_switch(&mut reader, opcode_offset)?,
            opcodes::IRETURN..=opcodes::RETURN => Insn::Simple(opcode.into()),
            opcodes::GETSTATIC..=opcodes::PUTFIELD => Insn::Field(FieldInsnNode {
                insn: opcode.into(),
                field_ref: MemberRef::Index(reader.read_u2()?),
            }),
            opcodes::INVOKEVIRTUAL..=opcodes::INVOKESTATIC => Insn::Method(MethodInsnNode {
                insn: opcode.into(),
                method_ref: MemberRef::Index(reader.read_u2()?),
            }),
            opcodes::INVOKEINTERFACE => {
                let method_index = reader.read_u2()?;
                let count = reader.read_u1()?;
                let _ = reader.read_u1()?;
                Insn::InvokeInterface(InvokeInterfaceInsnNode {
                    insn: opcode.into(),
                    method_index,
                    count,
                })
            }
            opcodes::INVOKEDYNAMIC => {
                let method_index = reader.read_u2()?;
                let _ = reader.read_u2()?;
                Insn::InvokeDynamic(InvokeDynamicInsnNode::from_index(method_index))
            }
            opcodes::NEW => Insn::Type(TypeInsnNode {
                insn: opcode.into(),
                type_index: reader.read_u2()?,
            }),
            opcodes::NEWARRAY => Insn::Int(IntInsnNode {
                insn: opcode.into(),
                operand: reader.read_u1()? as i32,
            }),
            opcodes::ANEWARRAY => Insn::Type(TypeInsnNode {
                insn: opcode.into(),
                type_index: reader.read_u2()?,
            }),
            opcodes::ARRAYLENGTH | opcodes::ATHROW => Insn::Simple(opcode.into()),
            opcodes::CHECKCAST | opcodes::INSTANCEOF => Insn::Type(TypeInsnNode {
                insn: opcode.into(),
                type_index: reader.read_u2()?,
            }),
            opcodes::MONITORENTER | opcodes::MONITOREXIT => Insn::Simple(opcode.into()),
            opcodes::WIDE => read_wide(&mut reader)?,
            opcodes::MULTIANEWARRAY => Insn::MultiANewArray(MultiANewArrayInsnNode {
                insn: opcode.into(),
                type_index: reader.read_u2()?,
                dimensions: reader.read_u1()?,
            }),
            opcodes::IFNULL | opcodes::IFNONNULL => Insn::Jump(JumpInsnNode {
                insn: opcode.into(),
                offset: reader.read_i2()? as i32,
            }),
            opcodes::GOTO_W | opcodes::JSR_W => Insn::Jump(JumpInsnNode {
                insn: opcode.into(),
                offset: reader.read_i4()?,
            }),
            opcodes::BREAKPOINT => Insn::Simple(opcode.into()),
            opcodes::IMPDEP1 | opcodes::IMPDEP2 => Insn::Simple(opcode.into()),
            _ => {
                return Err(ClassReadError::InvalidOpcode {
                    opcode,
                    offset: opcode_offset,
                });
            }
        };

        insns.push(ParsedInstruction {
            offset: opcode_offset as u16,
            insn,
        });
    }

    Ok(insns)
}

fn build_insn_nodes(
    code: &[u8],
    exception_table: &[ExceptionTableEntry],
    cp: &[CpInfo],
) -> Result<
    (
        Vec<AbstractInsnNode>,
        Vec<TryCatchBlockNode>,
        std::collections::HashMap<u16, LabelNode>,
    ),
    ClassReadError,
> {
    let instructions = parse_code_instructions_with_offsets(code)?;
    let mut offsets = std::collections::HashSet::new();
    for instruction in &instructions {
        offsets.insert(instruction.offset);
        match &instruction.insn {
            Insn::Jump(node) => {
                offsets.insert((instruction.offset as i32 + node.offset) as u16);
            }
            Insn::TableSwitch(node) => {
                offsets.insert((instruction.offset as i32 + node.default_offset) as u16);
                for offset in &node.offsets {
                    offsets.insert((instruction.offset as i32 + *offset) as u16);
                }
            }
            Insn::LookupSwitch(node) => {
                offsets.insert((instruction.offset as i32 + node.default_offset) as u16);
                for (_, offset) in &node.pairs {
                    offsets.insert((instruction.offset as i32 + *offset) as u16);
                }
            }
            _ => {}
        }
    }
    for entry in exception_table {
        offsets.insert(entry.start_pc);
        offsets.insert(entry.end_pc);
        offsets.insert(entry.handler_pc);
    }
    offsets.insert(code.len() as u16);

    let mut label_by_offset = std::collections::HashMap::new();
    for (next_id, offset) in offsets.into_iter().enumerate() {
        label_by_offset.insert(offset, LabelNode { id: next_id });
    }

    let mut nodes = Vec::new();
    for instruction in instructions {
        if let Some(label) = label_by_offset.get(&{ instruction.offset }) {
            nodes.push(AbstractInsnNode::Label(*label));
        }
        nodes.push(AbstractInsnNode::Insn(instruction.insn));
    }
    if let Some(label) = label_by_offset.get(&(code.len() as u16)) {
        nodes.push(AbstractInsnNode::Label(*label));
    }

    let mut try_catch_blocks = Vec::new();
    for entry in exception_table {
        let start = *label_by_offset
            .get(&entry.start_pc)
            .ok_or_else(|| ClassReadError::InvalidAttribute("missing start label".to_string()))?;
        let end = *label_by_offset
            .get(&entry.end_pc)
            .ok_or_else(|| ClassReadError::InvalidAttribute("missing end label".to_string()))?;
        let handler = *label_by_offset
            .get(&entry.handler_pc)
            .ok_or_else(|| ClassReadError::InvalidAttribute("missing handler label".to_string()))?;
        let catch_type = if entry.catch_type == 0 {
            None
        } else {
            Some(cp_class_name(cp, entry.catch_type)?.to_string())
        };
        try_catch_blocks.push(TryCatchBlockNode {
            start,
            end,
            handler,
            catch_type,
        });
    }

    Ok((nodes, try_catch_blocks, label_by_offset))
}

pub(crate) fn build_insn_nodes_public(
    code: &[u8],
    exception_table: &[ExceptionTableEntry],
    cp: &[CpInfo],
) -> Result<(Vec<AbstractInsnNode>, Vec<TryCatchBlockNode>), ClassReadError> {
    let (nodes, try_catch_blocks, _) = build_insn_nodes(code, exception_table, cp)?;
    Ok((nodes, try_catch_blocks))
}

fn attach_line_numbers(
    nodes: Vec<AbstractInsnNode>,
    entries: &[LineNumber],
    label_by_offset: &std::collections::HashMap<u16, LabelNode>,
) -> Vec<AbstractInsnNode> {
    let mut lines_by_label = std::collections::HashMap::<usize, Vec<LineNumberInsnNode>>::new();
    for entry in entries {
        if let Some(label) = label_by_offset.get(&entry.start_pc) {
            lines_by_label
                .entry(label.id)
                .or_default()
                .push(LineNumberInsnNode {
                    line: entry.line_number,
                    start: *label,
                });
        }
    }

    let mut merged = Vec::with_capacity(nodes.len() + entries.len());
    for node in nodes {
        let label_id = match &node {
            AbstractInsnNode::Label(label) => Some(label.id),
            _ => None,
        };
        merged.push(node);
        if let Some(label_id) = label_id
            && let Some(lines) = lines_by_label.remove(&label_id)
        {
            for line in lines {
                merged.push(AbstractInsnNode::LineNumber(line));
            }
        }
    }
    merged
}

fn read_table_switch(
    reader: &mut ByteReader<'_>,
    opcode_offset: usize,
) -> Result<Insn, ClassReadError> {
    reader.align4(opcode_offset)?;
    let default_offset = reader.read_i4()?;
    let low = reader.read_i4()?;
    let high = reader.read_i4()?;
    let count = if high < low {
        0
    } else {
        (high - low + 1) as usize
    };
    let mut offsets = Vec::with_capacity(count);
    for _ in 0..count {
        offsets.push(reader.read_i4()?);
    }
    Ok(Insn::TableSwitch(TableSwitchInsnNode {
        insn: opcodes::TABLESWITCH.into(),
        default_offset,
        low,
        high,
        offsets,
    }))
}

fn read_lookup_switch(
    reader: &mut ByteReader<'_>,
    opcode_offset: usize,
) -> Result<Insn, ClassReadError> {
    reader.align4(opcode_offset)?;
    let default_offset = reader.read_i4()?;
    let npairs = reader.read_i4()? as usize;
    let mut pairs = Vec::with_capacity(npairs);
    for _ in 0..npairs {
        let key = reader.read_i4()?;
        let offset = reader.read_i4()?;
        pairs.push((key, offset));
    }
    Ok(Insn::LookupSwitch(LookupSwitchInsnNode {
        insn: opcodes::LOOKUPSWITCH.into(),
        default_offset,
        pairs,
    }))
}

fn read_wide(reader: &mut ByteReader<'_>) -> Result<Insn, ClassReadError> {
    let opcode = reader.read_u1()?;
    match opcode {
        opcodes::ILOAD..=opcodes::ALOAD | opcodes::ISTORE..=opcodes::ASTORE | opcodes::RET => {
            Ok(Insn::Var(VarInsnNode {
                insn: opcode.into(),
                var_index: reader.read_u2()?,
            }))
        }
        opcodes::IINC => Ok(Insn::Iinc(IincInsnNode {
            insn: opcode.into(),
            var_index: reader.read_u2()?,
            increment: reader.read_i2()?,
        })),
        _ => Err(ClassReadError::InvalidOpcode {
            opcode,
            offset: reader.pos().saturating_sub(1),
        }),
    }
}

fn visit_instruction(
    cp: &[CpInfo],
    offset: i32,
    insn: Insn,
    mv: &mut dyn MethodVisitor,
) -> Result<(), ClassReadError> {
    match insn {
        Insn::Simple(node) => {
            mv.visit_insn(node.opcode);
        }
        Insn::Int(node) => {
            mv.visit_int_insn(node.insn.opcode, node.operand);
        }
        Insn::Var(node) => {
            mv.visit_var_insn(node.insn.opcode, node.var_index);
        }
        Insn::Type(node) => {
            let type_name = cp_class_name(cp, node.type_index)?;
            mv.visit_type_insn(node.insn.opcode, type_name);
        }
        Insn::Field(node) => {
            let index = match node.field_ref {
                MemberRef::Index(index) => index,
                MemberRef::Symbolic { .. } => {
                    return Err(ClassReadError::InvalidIndex(0));
                }
            };
            let (owner, name, desc) = cp_field_ref(cp, index)?;
            mv.visit_field_insn(node.insn.opcode, owner, name, desc);
        }
        Insn::Method(node) => {
            let index = match node.method_ref {
                MemberRef::Index(index) => index,
                MemberRef::Symbolic { .. } => {
                    return Err(ClassReadError::InvalidIndex(0));
                }
            };
            let (owner, name, desc, is_interface) = cp_method_ref(cp, index)?;
            mv.visit_method_insn(node.insn.opcode, owner, name, desc, is_interface);
        }
        Insn::InvokeInterface(node) => {
            let (owner, name, desc, _is_interface) = cp_method_ref(cp, node.method_index)?;
            mv.visit_method_insn(node.insn.opcode, owner, name, desc, true);
        }
        Insn::InvokeDynamic(node) => {
            let (name, desc) = cp_invoke_dynamic(cp, node.method_index)?;
            mv.visit_invoke_dynamic_insn(name, desc);
        }
        Insn::Jump(node) => {
            let target = offset + node.offset;
            mv.visit_jump_insn(node.insn.opcode, target);
        }
        Insn::Ldc(node) => {
            let index = match node.value {
                LdcValue::Index(index) => index,
                LdcValue::String(value) => {
                    mv.visit_ldc_insn(LdcConstant::String(value));
                    return Ok(());
                }
                LdcValue::Type(value) => {
                    match value.clone() {
                        Type::Method { .. } => {
                            mv.visit_ldc_insn(LdcConstant::MethodType(value.get_descriptor()));
                        }
                        _ => {
                            mv.visit_ldc_insn(LdcConstant::Class(value.get_descriptor()));
                        }
                    }

                    return Ok(());
                }
                LdcValue::Int(value) => {
                    mv.visit_ldc_insn(LdcConstant::Integer(value));
                    return Ok(());
                }
                LdcValue::Float(value) => {
                    mv.visit_ldc_insn(LdcConstant::Float(value));
                    return Ok(());
                }
                LdcValue::Long(value) => {
                    mv.visit_ldc_insn(LdcConstant::Long(value));
                    return Ok(());
                }
                LdcValue::Double(value) => {
                    mv.visit_ldc_insn(LdcConstant::Double(value));
                    return Ok(());
                }
            };
            let constant = cp_ldc_constant(cp, index)?;
            mv.visit_ldc_insn(constant);
        }
        Insn::Iinc(node) => {
            mv.visit_iinc_insn(node.var_index, node.increment);
        }
        Insn::TableSwitch(node) => {
            let targets = node
                .offsets
                .iter()
                .map(|value| offset + *value)
                .collect::<Vec<_>>();
            mv.visit_table_switch(offset + node.default_offset, node.low, node.high, &targets);
        }
        Insn::LookupSwitch(node) => {
            let pairs = node
                .pairs
                .iter()
                .map(|(key, value)| (*key, offset + *value))
                .collect::<Vec<_>>();
            mv.visit_lookup_switch(offset + node.default_offset, &pairs);
        }
        Insn::MultiANewArray(node) => {
            let type_name = cp_class_name(cp, node.type_index)?;
            mv.visit_multi_anewarray_insn(type_name, node.dimensions);
        }
    }
    Ok(())
}

pub struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn align4(&mut self, opcode_offset: usize) -> Result<(), ClassReadError> {
        let mut padding = (4 - ((opcode_offset + 1) % 4)) % 4;
        while padding > 0 {
            self.read_u1()?;
            padding -= 1;
        }
        Ok(())
    }

    pub fn read_u1(&mut self) -> Result<u8, ClassReadError> {
        if self.pos >= self.data.len() {
            return Err(ClassReadError::UnexpectedEof);
        }
        let value = self.data[self.pos];
        self.pos += 1;
        Ok(value)
    }

    pub fn read_i1(&mut self) -> Result<i8, ClassReadError> {
        Ok(self.read_u1()? as i8)
    }

    pub fn read_u2(&mut self) -> Result<u16, ClassReadError> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub fn read_i2(&mut self) -> Result<i16, ClassReadError> {
        Ok(self.read_u2()? as i16)
    }

    pub fn read_u4(&mut self) -> Result<u32, ClassReadError> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub fn read_i4(&mut self) -> Result<i32, ClassReadError> {
        let bytes = self.read_bytes(4)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub fn read_u8(&mut self) -> Result<u64, ClassReadError> {
        let bytes = self.read_bytes(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    pub fn read_bytes(&mut self, len: usize) -> Result<Vec<u8>, ClassReadError> {
        if self.pos + len > self.data.len() {
            return Err(ClassReadError::UnexpectedEof);
        }
        let bytes = self.data[self.pos..self.pos + len].to_vec();
        self.pos += len;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use crate::class_writer::ClassWriter;
    use crate::constants::*;
    use crate::insn::{Label, LabelNode};
    use crate::opcodes;

    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    // A mock visitor to capture parsing results
    struct MockClassVisitor {
        pub visited_name: Rc<RefCell<Option<String>>>,
        pub visited_methods: Rc<RefCell<Vec<String>>>,
    }

    impl MockClassVisitor {
        fn new() -> Self {
            Self {
                visited_name: Rc::new(RefCell::new(None)),
                visited_methods: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    impl ClassVisitor for MockClassVisitor {
        fn visit(
            &mut self,
            _major: u16,
            _minor: u16,
            _access_flags: u16,
            name: &str,
            _super_name: Option<&str>,
            _interfaces: &[String],
        ) {
            *self.visited_name.borrow_mut() = Some(name.to_string());
        }

        fn visit_method(
            &mut self,
            _access_flags: u16,
            name: &str,
            _descriptor: &str,
        ) -> Option<Box<dyn MethodVisitor>> {
            self.visited_methods.borrow_mut().push(name.to_string());
            None
        }
    }

    /// Helper to generate a minimal valid class file byte array (Java 8).
    /// Class Name: "TestClass"
    fn generate_minimal_class() -> Vec<u8> {
        let mut w = Vec::new();
        // Magic
        w.extend_from_slice(&0xCAFEBABE_u32.to_be_bytes());
        // Version (Java 8 = 52.0)
        w.extend_from_slice(&0_u16.to_be_bytes()); // minor
        w.extend_from_slice(&52_u16.to_be_bytes()); // major

        // Constant Pool (Count: 5)
        // 1: UTF8 "TestClass"
        // 2: Class #1
        // 3: UTF8 "java/lang/Object"
        // 4: Class #3
        w.extend_from_slice(&5_u16.to_be_bytes()); // Count (N+1)

        // #1 UTF8
        w.push(1);
        let name = "TestClass";
        w.extend_from_slice(&(name.len() as u16).to_be_bytes());
        w.extend_from_slice(name.as_bytes());

        // #2 Class
        w.push(7);
        w.extend_from_slice(&1_u16.to_be_bytes());

        // #3 UTF8
        w.push(1);
        let obj = "java/lang/Object";
        w.extend_from_slice(&(obj.len() as u16).to_be_bytes());
        w.extend_from_slice(obj.as_bytes());

        // #4 Class
        w.push(7);
        w.extend_from_slice(&3_u16.to_be_bytes());

        // Access Flags (PUBLIC)
        w.extend_from_slice(&0x0021_u16.to_be_bytes());
        // This Class (#2)
        w.extend_from_slice(&2_u16.to_be_bytes());
        // Super Class (#4)
        w.extend_from_slice(&4_u16.to_be_bytes());

        // Interfaces Count
        w.extend_from_slice(&0_u16.to_be_bytes());
        // Fields Count
        w.extend_from_slice(&0_u16.to_be_bytes());
        // Methods Count
        w.extend_from_slice(&0_u16.to_be_bytes());
        // Attributes Count
        w.extend_from_slice(&0_u16.to_be_bytes());

        w
    }

    fn generate_module_info_class() -> Vec<u8> {
        let mut writer = ClassWriter::new(0);
        writer.visit(V9, 0, ACC_MODULE, "module-info", None, &[]);

        let mut module = writer.visit_module("com.example.app", ACC_OPEN, Some("1.0"));
        module.visit_main_class("com/example/app/Main");
        module.visit_package("com/example/api");
        module.visit_package("com/example/internal");
        module.visit_require("java.base", ACC_MANDATED, None);
        module.visit_require(
            "com.example.lib",
            ACC_TRANSITIVE | ACC_STATIC_PHASE,
            Some("2.1"),
        );
        module.visit_export("com/example/api", 0, &["com.example.consumer"]);
        module.visit_open("com/example/internal", 0, &["com.example.runtime"]);
        module.visit_use("com/example/spi/Service");
        module.visit_provide("com/example/spi/Service", &["com/example/impl/ServiceImpl"]);
        module.visit_end(&mut writer);

        writer.to_bytes().expect("module-info should encode")
    }

    #[test]
    fn test_class_reader_header() {
        let bytes = generate_minimal_class();
        let reader = ClassReader::new(&bytes);
        let mut visitor = MockClassVisitor::new();

        let result = reader.accept(&mut visitor, 0);

        assert!(result.is_ok(), "Should parse valid class file");
        assert_eq!(
            *visitor.visited_name.borrow(),
            Some("TestClass".to_string())
        );
    }

    #[test]
    fn test_invalid_magic() {
        // expected CA FE BA BE
        let bytes = vec![0x00, 0x00, 0x00, 0x00];
        let reader = ClassReader::new(&bytes);
        let mut visitor = MockClassVisitor::new();

        let result = reader.accept(&mut visitor, 0);
        assert!(matches!(result, Err(ClassReadError::InvalidMagic(_))));
    }

    #[test]
    fn test_code_reader_alignment() {
        // Test internal alignment logic for switch instructions
        let data = vec![0x00, 0x00, 0x00, 0x00]; // 4 bytes
        let mut reader = super::ByteReader::new(&data);

        // If we are at pos 1, padding to 4-byte boundary
        reader.pos = 1;
        // 1 -> align 4 -> skips 3 bytes -> pos 4
        assert!(reader.align4(0).is_ok());
        assert_eq!(reader.pos(), 4);
    }

    #[test]
    fn test_parse_runtime_visible_type_annotations_supertype() {
        // RuntimeVisibleTypeAnnotations:
        // u2 num_annotations = 1
        // type_annotation:
        //   u1 target_type = TA_TARGET_CLASS_EXTENDS
        //   u2 supertype_index = 5
        //   type_path: u1 path_len=0
        //   annotation:
        //     u2 type_descriptor_index=10
        //     u2 num_pairs=0
        let mut info = vec![];
        u2(1, &mut info);
        u1(TA_TARGET_CLASS_EXTENDS, &mut info);
        u2(5, &mut info);
        u1(0, &mut info);
        u2(10, &mut info);
        u2(0, &mut info);

        let cp: Vec<CpInfo> = vec![];
        let attr = parse_attribute("RuntimeVisibleTypeAnnotations", info, &cp).unwrap();

        match attr {
            AttributeInfo::RuntimeVisibleTypeAnnotations { annotations } => {
                assert_eq!(annotations.len(), 1);
                let a = &annotations[0];
                assert_eq!(a.target_type, TA_TARGET_CLASS_EXTENDS);
                assert!(matches!(
                    a.target_info,
                    TypeAnnotationTargetInfo::Supertype { supertype_index: 5 }
                ));
                assert_eq!(a.target_path.path.len(), 0);
                assert_eq!(a.annotation.type_descriptor_index, 10);
                assert_eq!(a.annotation.element_value_pairs.len(), 0);
            }
            other => panic!("unexpected attr: {:?}", other),
        }
    }

    #[test]
    fn test_parse_runtime_visible_type_annotations_formal_parameter_with_path() {
        // target_type = TA_TARGET_METHOD_FORMAL_PARAMETER
        // u1 formal_parameter_index = 2
        // type_path len=1 entry(kind=TA_TYPE_PATH_ARRAY, arg_index=0)
        // annotation: type_index=9, num_pairs=0
        let mut info = vec![];
        u2(1, &mut info);
        u1(TA_TARGET_METHOD_FORMAL_PARAMETER, &mut info);
        u1(2, &mut info);

        u1(1, &mut info); // path_length
        u1(TA_TYPE_PATH_ARRAY, &mut info);
        u1(0, &mut info);

        u2(9, &mut info);
        u2(0, &mut info);

        let cp: Vec<CpInfo> = vec![];
        let attr = parse_attribute("RuntimeVisibleTypeAnnotations", info, &cp).unwrap();

        match attr {
            AttributeInfo::RuntimeVisibleTypeAnnotations { annotations } => {
                let a = &annotations[0];
                assert_eq!(a.target_type, TA_TARGET_METHOD_FORMAL_PARAMETER);
                assert!(matches!(
                    a.target_info,
                    TypeAnnotationTargetInfo::FormalParameter {
                        formal_parameter_index: 2
                    }
                ));
                assert_eq!(a.target_path.path.len(), 1);
                assert_eq!(a.target_path.path[0].type_path_kind, TA_TYPE_PATH_ARRAY);
                assert_eq!(a.target_path.path[0].type_argument_index, 0);
            }
            other => panic!("unexpected attr: {:?}", other),
        }
    }

    #[test]
    fn test_parse_runtime_visible_type_annotations_localvar_table() {
        // target_type = TA_TARGET_LOCAL_VARIABLE
        // u2 table_length = 1
        // entry: start_pc=1 length=2 index=3
        // type_path len=0
        // annotation: type_index=8, num_pairs=0
        let mut info = vec![];
        u2(1, &mut info);
        u1(TA_TARGET_LOCAL_VARIABLE, &mut info);

        u2(1, &mut info); // table_length
        u2(1, &mut info);
        u2(2, &mut info);
        u2(3, &mut info);

        u1(0, &mut info); // path_length
        u2(8, &mut info);
        u2(0, &mut info);

        let cp: Vec<CpInfo> = vec![];
        let attr = parse_attribute("RuntimeVisibleTypeAnnotations", info, &cp).unwrap();

        match attr {
            AttributeInfo::RuntimeVisibleTypeAnnotations { annotations } => {
                let a = &annotations[0];
                assert_eq!(a.target_type, TA_TARGET_LOCAL_VARIABLE);
                match &a.target_info {
                    TypeAnnotationTargetInfo::LocalVar { table } => {
                        assert_eq!(table.len(), 1);
                        assert_eq!(table[0].start_pc, 1);
                        assert_eq!(table[0].length, 2);
                        assert_eq!(table[0].index, 3);
                    }
                    other => panic!("unexpected target_info: {:?}", other),
                }
            }
            other => panic!("unexpected attr: {:?}", other),
        }
    }

    #[test]
    fn test_parse_module_attribute_family() {
        let mut module_info = vec![];
        u2(1, &mut module_info);
        u2(ACC_OPEN, &mut module_info);
        u2(2, &mut module_info);
        u2(1, &mut module_info);
        u2(3, &mut module_info);
        u2(ACC_TRANSITIVE | ACC_STATIC_PHASE, &mut module_info);
        u2(4, &mut module_info);
        u2(1, &mut module_info);
        u2(5, &mut module_info);
        u2(ACC_MANDATED, &mut module_info);
        u2(2, &mut module_info);
        u2(6, &mut module_info);
        u2(7, &mut module_info);
        u2(1, &mut module_info);
        u2(8, &mut module_info);
        u2(0, &mut module_info);
        u2(1, &mut module_info);
        u2(9, &mut module_info);
        u2(1, &mut module_info);
        u2(10, &mut module_info);
        u2(1, &mut module_info);
        u2(11, &mut module_info);
        u2(2, &mut module_info);
        u2(12, &mut module_info);
        u2(13, &mut module_info);

        let attr = parse_attribute("Module", module_info, &[]).expect("module attr should parse");
        match attr {
            AttributeInfo::Module(module) => {
                assert_eq!(module.module_name_index, 1);
                assert_eq!(module.module_flags, ACC_OPEN);
                assert_eq!(module.module_version_index, 2);
                assert_eq!(
                    module.requires,
                    vec![ModuleRequire {
                        requires_index: 3,
                        requires_flags: ACC_TRANSITIVE | ACC_STATIC_PHASE,
                        requires_version_index: 4,
                    }]
                );
                assert_eq!(
                    module.exports,
                    vec![ModuleExport {
                        exports_index: 5,
                        exports_flags: ACC_MANDATED,
                        exports_to_index: vec![6, 7],
                    }]
                );
                assert_eq!(
                    module.opens,
                    vec![ModuleOpen {
                        opens_index: 8,
                        opens_flags: 0,
                        opens_to_index: vec![9],
                    }]
                );
                assert_eq!(module.uses_index, vec![10]);
                assert_eq!(
                    module.provides,
                    vec![ModuleProvide {
                        provides_index: 11,
                        provides_with_index: vec![12, 13],
                    }]
                );
            }
            other => panic!("unexpected attr: {:?}", other),
        }

        let mut packages_info = vec![];
        u2(2, &mut packages_info);
        u2(21, &mut packages_info);
        u2(22, &mut packages_info);
        let attr = parse_attribute("ModulePackages", packages_info, &[])
            .expect("module packages attr should parse");
        match attr {
            AttributeInfo::ModulePackages {
                package_index_table,
            } => {
                assert_eq!(package_index_table, vec![21, 22]);
            }
            other => panic!("unexpected attr: {:?}", other),
        }

        let mut main_class_info = vec![];
        u2(23, &mut main_class_info);
        let attr = parse_attribute("ModuleMainClass", main_class_info, &[])
            .expect("module main class attr should parse");
        match attr {
            AttributeInfo::ModuleMainClass { main_class_index } => {
                assert_eq!(main_class_index, 23);
            }
            other => panic!("unexpected attr: {:?}", other),
        }
    }

    fn u1(v: u8, out: &mut Vec<u8>) {
        out.push(v);
    }
    fn u2(v: u16, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    #[test]
    fn test_method_node_contains_offsets_and_line_numbers() {
        let mut writer = ClassWriter::new(0);
        writer.visit(
            52,
            0,
            ACC_PUBLIC,
            "TestNodeData",
            Some("java/lang/Object"),
            &[],
        );

        let mut ctor = writer.visit_method(ACC_PUBLIC, "<init>", "()V");
        ctor.visit_code();
        ctor.visit_var_insn(opcodes::ALOAD, 0);
        ctor.visit_method_insn(
            opcodes::INVOKESPECIAL,
            "java/lang/Object",
            "<init>",
            "()V",
            false,
        );
        ctor.visit_insn(opcodes::RETURN);
        ctor.visit_maxs(1, 1);
        ctor.visit_end(&mut writer);

        let mut method = writer.visit_method(ACC_PUBLIC | ACC_STATIC, "answer", "()I");
        let start = Label::new();
        method.visit_code();
        method.visit_label(start);
        method.visit_line_number(123, LabelNode::from_label(start));
        method.visit_insn(opcodes::ICONST_1);
        method.visit_insn(opcodes::IRETURN);
        method.visit_maxs(1, 0);
        method.visit_end(&mut writer);

        let bytes = writer.to_bytes().expect("class should encode");
        let class = ClassReader::new(&bytes)
            .to_class_node()
            .expect("class should decode");
        let method = class
            .methods
            .iter()
            .find(|method| method.name == "answer")
            .expect("method should exist");

        assert_eq!(method.instruction_offsets, vec![0, 1]);
        assert_eq!(method.line_numbers.len(), 1);
        assert_eq!(method.line_numbers[0].line_number, 123);
        assert!(
            method
                .insn_nodes
                .iter()
                .any(|node| matches!(node, AbstractInsnNode::LineNumber(_)))
        );
        assert!(
            method
                .insn_nodes
                .iter()
                .any(|node| matches!(node, AbstractInsnNode::Label(_)))
        );
        assert!(method.try_catch_blocks.is_empty());
    }

    #[test]
    fn test_method_node_contains_try_catch_blocks() {
        let mut writer = ClassWriter::new(0);
        writer.visit(
            52,
            0,
            ACC_PUBLIC,
            "TestTryCatchNode",
            Some("java/lang/Object"),
            &[],
        );

        let mut ctor = writer.visit_method(ACC_PUBLIC, "<init>", "()V");
        ctor.visit_code();
        ctor.visit_var_insn(opcodes::ALOAD, 0);
        ctor.visit_method_insn(
            opcodes::INVOKESPECIAL,
            "java/lang/Object",
            "<init>",
            "()V",
            false,
        );
        ctor.visit_insn(opcodes::RETURN);
        ctor.visit_maxs(1, 1);
        ctor.visit_end(&mut writer);

        let start = Label::new();
        let end = Label::new();
        let handler = Label::new();
        let mut method =
            writer.visit_method(ACC_PUBLIC | ACC_STATIC, "safeLen", "(Ljava/lang/String;)I");
        method.visit_code();
        method.visit_label(start);
        method.visit_var_insn(opcodes::ALOAD, 0);
        method.visit_method_insn(
            opcodes::INVOKEVIRTUAL,
            "java/lang/String",
            "length",
            "()I",
            false,
        );
        method.visit_insn(opcodes::IRETURN);
        method.visit_label(end);
        method.visit_label(handler);
        method.visit_var_insn(opcodes::ASTORE, 1);
        method.visit_insn(opcodes::ICONST_M1);
        method.visit_insn(opcodes::IRETURN);
        method.visit_try_catch_block(start, end, handler, Some("java/lang/RuntimeException"));
        method.visit_maxs(1, 2);
        method.visit_end(&mut writer);

        let bytes = writer.to_bytes().expect("class should encode");
        let class = ClassReader::new(&bytes)
            .to_class_node()
            .expect("class should decode");
        let method = class
            .methods
            .iter()
            .find(|method| method.name == "safeLen")
            .expect("method should exist");

        assert_eq!(method.exception_table.len(), 1);
        assert_eq!(method.try_catch_blocks.len(), 1);
        assert_eq!(
            method.try_catch_blocks[0].catch_type.as_deref(),
            Some("java/lang/RuntimeException")
        );
    }

    #[test]
    fn test_parse_runtime_visible_annotations_one_empty() {
        // u2 num_annotations=1
        // annotation: type=10, pairs=0
        let mut info = vec![];
        u2(1, &mut info);
        u2(10, &mut info);
        u2(0, &mut info);

        let cp: Vec<CpInfo> = vec![];
        let attr = parse_attribute("RuntimeVisibleAnnotations", info, &cp).unwrap();
        match attr {
            AttributeInfo::RuntimeVisibleAnnotations { annotations } => {
                assert_eq!(annotations.len(), 1);
                assert_eq!(annotations[0].type_descriptor_index, 10);
                assert_eq!(annotations[0].element_value_pairs.len(), 0);
            }
            other => panic!("unexpected attr: {:?}", other),
        }
    }

    #[test]
    fn test_parse_runtime_visible_parameter_annotations_two_params() {
        // u1 num_params=2
        // p0: u2 num_ann=1, annotation(type=10,pairs=0)
        // p1: u2 num_ann=0
        let mut info = vec![];
        u1(2, &mut info);
        u2(1, &mut info);
        u2(10, &mut info);
        u2(0, &mut info);
        u2(0, &mut info);

        let cp: Vec<CpInfo> = vec![];
        let attr = parse_attribute("RuntimeVisibleParameterAnnotations", info, &cp).unwrap();
        match attr {
            AttributeInfo::RuntimeVisibleParameterAnnotations { parameters } => {
                assert_eq!(parameters.parameters.len(), 2);
                assert_eq!(parameters.parameters[0].len(), 1);
                assert_eq!(parameters.parameters[1].len(), 0);
            }
            other => panic!("unexpected attr: {:?}", other),
        }
    }

    #[test]
    fn test_class_reader_decodes_module_info_node() {
        let bytes = generate_module_info_class();
        let class = ClassReader::new(&bytes)
            .to_class_node()
            .expect("module-info should decode");

        assert_eq!(class.name, "module-info");
        assert_eq!(class.access_flags, ACC_MODULE);

        let module = class.module.expect("module descriptor should be decoded");
        assert_eq!(module.name, "com.example.app");
        assert_eq!(module.access_flags, ACC_OPEN);
        assert_eq!(module.version.as_deref(), Some("1.0"));
        assert_eq!(module.main_class.as_deref(), Some("com/example/app/Main"));
        assert_eq!(
            module.packages,
            vec![
                "com/example/api".to_string(),
                "com/example/internal".to_string()
            ]
        );
        assert_eq!(module.requires.len(), 2);
        assert_eq!(module.requires[0].module, "java.base");
        assert_eq!(module.requires[0].access_flags, ACC_MANDATED);
        assert_eq!(module.requires[0].version, None);
        assert_eq!(module.requires[1].module, "com.example.lib");
        assert_eq!(
            module.requires[1].access_flags,
            ACC_TRANSITIVE | ACC_STATIC_PHASE
        );
        assert_eq!(module.requires[1].version.as_deref(), Some("2.1"));
        assert_eq!(module.exports.len(), 1);
        assert_eq!(module.exports[0].package, "com/example/api");
        assert_eq!(
            module.exports[0].modules,
            vec!["com.example.consumer".to_string()]
        );
        assert_eq!(module.opens.len(), 1);
        assert_eq!(module.opens[0].package, "com/example/internal");
        assert_eq!(
            module.opens[0].modules,
            vec!["com.example.runtime".to_string()]
        );
        assert_eq!(module.uses, vec!["com/example/spi/Service".to_string()]);
        assert_eq!(module.provides.len(), 1);
        assert_eq!(module.provides[0].service, "com/example/spi/Service");
        assert_eq!(
            module.provides[0].providers,
            vec!["com/example/impl/ServiceImpl".to_string()]
        );
    }
}
