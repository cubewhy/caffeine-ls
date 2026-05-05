use crate::class_reader::{AttributeInfo, ExceptionTableEntry};
use crate::class_reader::{LineNumber, LocalVariable, MethodParameter};
use crate::constant_pool::CpInfo;
use crate::insn::InsnList;
use crate::insn::{AbstractInsnNode, TryCatchBlockNode};

/// Represents a parsed Java Class File.
///
/// This structure holds the complete object model of a `.class` file, including
/// its header information, constant pool, interfaces, fields, methods, and attributes.
///
/// # See Also
/// * [JVM Specification: ClassFile Structure](https://docs.oracle.com/javase/specs/jvms/se17/html/jvms-4.html#jvms-4.1)
#[derive(Debug, Clone)]
pub struct ClassNode {
    /// The minor version of the class file format.
    pub minor_version: u16,

    /// The major version of the class file format (e.g., 52 for Java 8, 61 for Java 17).
    pub major_version: u16,

    /// A bitmask of access flags used to denote access permissions to and properties of this class
    /// (e.g., `ACC_PUBLIC`, `ACC_FINAL`, `ACC_INTERFACE`).
    pub access_flags: u16,

    /// The raw constant pool containing heterogeneous constants (strings, integers, method references, etc.).
    /// Index 0 is reserved/unused.
    pub constant_pool: Vec<CpInfo>,

    /// The index into the constant pool pointing to a `CONSTANT_Class_info` structure representing this class.
    pub this_class: u16,

    /// The internal name of the class (e.g., `java/lang/String`).
    pub name: String,

    /// The internal name of the superclass (e.g., `java/lang/String`, `a/b/c`).
    /// Returns `None` if this class is `java.lang.Object`.
    pub super_name: Option<String>,

    /// The name of the source file from which this class was compiled, if the `SourceFile` attribute was present.
    pub source_file: Option<String>,

    /// A list of internal names of the direct superinterfaces of this class or interface.
    pub interfaces: Vec<String>,

    /// A list of indices into the constant pool representing the direct superinterfaces.
    pub interface_indices: Vec<u16>,

    /// The fields declared by this class or interface.
    pub fields: Vec<FieldNode>,

    /// The methods declared by this class or interface.
    pub methods: Vec<MethodNode>,

    /// Global attributes associated with the class (e.g., `SourceFile`, `InnerClasses`, `EnclosingMethod`).
    pub attributes: Vec<AttributeInfo>,

    /// The inner class entries associated with this class file.
    ///
    /// This is a decoded view of the `InnerClasses` attribute.
    pub inner_classes: Vec<InnerClassNode>,

    /// The internal name of the enclosing class, if known.
    ///
    /// This value is empty when no enclosing class information is available.
    pub outer_class: String,

    /// Decoded JPMS module descriptor data for `module-info.class`, if present.
    pub module: Option<ModuleNode>,

    pub permitted_subclasses: Vec<String>,

    pub record_components: Vec<RecordComponentNode>,
}

/// Represents an inner class entry in the `InnerClasses` attribute.
#[derive(Debug, Clone)]
pub struct InnerClassNode {
    /// The internal name of the inner class (e.g., `a/b/Outer$Inner`).
    pub name: String,

    /// The internal name of the enclosing class, if any.
    pub outer_name: Option<String>,

    /// The simple (unqualified) name of the inner class, if any.
    pub inner_name: Option<String>,

    /// The access flags of the inner class as declared in source.
    pub access_flags: u16,
}

/// Decoded JPMS module descriptor from the `Module` attribute family.
#[derive(Debug, Clone)]
pub struct ModuleNode {
    /// Module name, e.g. `java.base` or `com.example.app`.
    pub name: String,

    /// Raw module access flags from the `Module` attribute header.
    pub access_flags: u16,

    /// Optional module version string.
    pub version: Option<String>,

    /// `requires` directives.
    pub requires: Vec<ModuleRequireNode>,

    /// `exports` directives.
    pub exports: Vec<ModuleExportNode>,

    /// `opens` directives.
    pub opens: Vec<ModuleOpenNode>,

    /// `uses` directives as internal class names.
    pub uses: Vec<String>,

    /// `provides ... with ...` directives.
    pub provides: Vec<ModuleProvideNode>,

    /// Optional `ModulePackages` attribute contents.
    pub packages: Vec<String>,

    /// Optional `ModuleMainClass` attribute contents.
    pub main_class: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModuleRequireNode {
    pub module: String,
    pub access_flags: u16,
    pub version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModuleExportNode {
    pub package: String,
    pub access_flags: u16,
    pub modules: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ModuleOpenNode {
    pub package: String,
    pub access_flags: u16,
    pub modules: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ModuleProvideNode {
    pub service: String,
    pub providers: Vec<String>,
}

/// Represents a field (member variable) within a class.
///
/// # See Also
/// * [JVM Specification: field_info](https://docs.oracle.com/javase/specs/jvms/se17/html/jvms-4.html#jvms-4.5)
#[derive(Debug, Clone)]
pub struct FieldNode {
    /// A bitmask of access flags (e.g., `ACC_PUBLIC`, `ACC_STATIC`, `ACC_FINAL`).
    pub access_flags: u16,

    /// The name of the field.
    pub name: String,

    /// The field descriptor (e.g., `Ljava/lang/String;` or `I`).
    pub descriptor: String,

    /// Attributes associated with this field (e.g., `ConstantValue`, `Synthetic`, `Deprecated`, `Signature`).
    pub attributes: Vec<AttributeInfo>,
}

/// Represents a method within a class.
///
/// # See Also
/// * [JVM Specification: method_info](https://docs.oracle.com/javase/specs/jvms/se17/html/jvms-4.html#jvms-4.6)
#[derive(Debug, Clone)]
pub struct MethodNode {
    /// A bitmask of access flags (e.g., `ACC_PUBLIC`, `ACC_STATIC`, `ACC_SYNCHRONIZED`).
    pub access_flags: u16,

    /// The name of the method.
    pub name: String,

    /// The method descriptor describing parameter types and return type.
    pub descriptor: String,

    /// Whether this method has a `Code` attribute.
    /// This is `false` for `native` or `abstract` methods.
    pub has_code: bool,

    /// The maximum stack size required by the method's bytecode.
    pub max_stack: u16,

    /// The maximum number of local variables required by the method's bytecode.
    pub max_locals: u16,

    /// Decoded JVM instructions in an `InsnList`.
    pub instructions: InsnList,

    /// Original bytecode offsets corresponding to entries in `instructions`.
    pub instruction_offsets: Vec<u16>,

    /// Decoded JVM instructions as tree-style nodes, preserving labels and line markers.
    pub insn_nodes: Vec<AbstractInsnNode>,

    /// Exception handlers (raw entries in the code attribute).
    pub exception_table: Vec<ExceptionTableEntry>,

    /// Decoded try/catch blocks keyed by labels in `insn_nodes`.
    pub try_catch_blocks: Vec<TryCatchBlockNode>,

    /// Decoded line number entries from the `LineNumberTable`.
    pub line_numbers: Vec<LineNumber>,

    /// Decoded local variable entries from the `LocalVariableTable`.
    pub local_variables: Vec<LocalVariable>,

    /// Decoded method parameters from the `MethodParameters` attribute.
    pub method_parameters: Vec<MethodParameter>,

    /// Internal names of declared checked exceptions from the `Exceptions` attribute.
    pub exceptions: Vec<String>,

    /// Generic signature string from the `Signature` attribute, if present.
    pub signature: Option<String>,

    /// Attributes associated with the `Code` attribute (e.g., `LineNumberTable`, `LocalVariableTable`).
    pub code_attributes: Vec<AttributeInfo>,

    /// Other attributes associated with this method (e.g., `Exceptions`, `Synthetic`, `Deprecated`, `Signature`).
    pub attributes: Vec<AttributeInfo>,
}

#[derive(Debug, Clone)]
pub struct RecordComponentNode {
    /// The name of the record component
    pub name: String,

    /// The record component descriptor (e.g., `Ljava/lang/String;` or `I`).
    pub descriptor: String,

    /// Attributes associated with this component (e.g., `ConstantValue`, `Synthetic`, `Deprecated`, `Signature`).
    pub attributes: Vec<AttributeInfo>,
}
