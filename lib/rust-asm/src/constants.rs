/// Java 1.1 ClassFile version 45
pub const V1_1: u16 = 45;
/// Java 1.2 ClassFile version 46
pub const V1_2: u16 = 46;
/// Java 1.3 ClassFile version 47
pub const V1_3: u16 = 47;
/// Java 1.4 ClassFile version 48
pub const V1_4: u16 = 48;
/// Java 1.5 ClassFile version 49
pub const V1_5: u16 = 49;
/// Java 1.6 ClassFile version 50
pub const V1_6: u16 = 50;
/// Java 1.7 ClassFile version 51
pub const V1_7: u16 = 51;
/// Java 1.8 ClassFile version 52
pub const V1_8: u16 = 52;
/// Java 9 ClassFile version 53
pub const V9: u16 = 53;
/// Java 10 ClassFile version 54
pub const V10: u16 = 54;
/// Java 11 ClassFile version 55
pub const V11: u16 = 55;
/// Java 12 ClassFile version 56
pub const V12: u16 = 56;
/// Java 13 ClassFile version 57
pub const V13: u16 = 57;
/// Java 14 ClassFile version 58
pub const V14: u16 = 58;
/// Java 15 ClassFile version 59
pub const V15: u16 = 59;
/// Java 16 ClassFile version 60
pub const V16: u16 = 60;
/// Java 17 ClassFile version 61
pub const V17: u16 = 61;
/// Java 18 ClassFile version 62
pub const V18: u16 = 62;
/// Java 19 ClassFile version 63
pub const V19: u16 = 63;
/// Java 20 ClassFile version 64
pub const V20: u16 = 64;
/// Java 21 ClassFile version 65
pub const V21: u16 = 65;
/// Java 22 ClassFile version 66
pub const V22: u16 = 66;
/// Java 23 ClassFile version 67
pub const V23: u16 = 67;
/// Java 24 ClassFile version 68
pub const V24: u16 = 68;
/// Java 25 ClassFile version 69
pub const V25: u16 = 69;

pub const ACC_PUBLIC: u16 = 0x0001;
pub const ACC_PRIVATE: u16 = 0x0002;
pub const ACC_PROTECTED: u16 = 0x0004;
pub const ACC_STATIC: u16 = 0x0008;
pub const ACC_FINAL: u16 = 0x0010;
pub const ACC_SUPER: u16 = 0x0020;
pub const ACC_SYNCHRONIZED: u16 = 0x0020;
pub const ACC_VOLATILE: u16 = 0x0040;
pub const ACC_BRIDGE: u16 = 0x0040;
pub const ACC_TRANSIENT: u16 = 0x0080;
pub const ACC_VARARGS: u16 = 0x0080;
pub const ACC_NATIVE: u16 = 0x0100;
pub const ACC_INTERFACE: u16 = 0x0200;
pub const ACC_ABSTRACT: u16 = 0x0400;
pub const ACC_STRICT: u16 = 0x0800;
pub const ACC_SYNTHETIC: u16 = 0x1000;
pub const ACC_ANNOTATION: u16 = 0x2000;
pub const ACC_ENUM: u16 = 0x4000;
pub const ACC_MODULE: u16 = 0x8000;

// JPMS-specific aliases. The same raw bit values are context dependent.
pub const ACC_OPEN: u16 = 0x0020;
pub const ACC_TRANSITIVE: u16 = 0x0020;
pub const ACC_STATIC_PHASE: u16 = 0x0040;
pub const ACC_MANDATED: u16 = 0x8000;

bitflags::bitflags! {
    /// The JVM access flags of a class, field or method, as specified by the
    /// JVM ClassFile format ([JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1)
    /// classes, [§4.5](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.5)
    /// fields, [§4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6)
    /// methods). A single 16-bit word matching the ClassFile representation.
    ///
    /// Several bits are reused across contexts: `ACC_SUPER`/`ACC_SYNCHRONIZED`
    /// share `0x0020`, `ACC_VOLATILE`/`ACC_BRIDGE` share `0x0040`, and
    /// `ACC_TRANSIENT`/`ACC_VARARGS` share `0x0080` ([JVMS §4.6]). The same
    /// bit values appear again on JPMS `module-info` attributes (`ACC_OPEN`,
    /// `ACC_TRANSITIVE`, `ACC_STATIC_PHASE`, `ACC_MANDATED`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct JvmAccessFlags: u16 {
        const PUBLIC = ACC_PUBLIC;
        const PRIVATE = ACC_PRIVATE;
        const PROTECTED = ACC_PROTECTED;
        const STATIC = ACC_STATIC;
        const FINAL = ACC_FINAL;
        /// Class flag ([JVMS §4.1]); equals `SYNCHRONIZED` on methods.
        const SUPER = ACC_SUPER;
        /// Method flag ([JVMS §4.6]); equals `SUPER` on classes.
        const SYNCHRONIZED = ACC_SYNCHRONIZED;
        /// Field flag ([JVMS §4.5]); equals `BRIDGE` on methods.
        const VOLATILE = ACC_VOLATILE;
        /// Method flag; equals `VOLATILE` on fields.
        const BRIDGE = ACC_BRIDGE;
        /// Field flag; equals `VARARGS` on methods.
        const TRANSIENT = ACC_TRANSIENT;
        /// Method flag; equals `TRANSIENT` on fields.
        const VARARGS = ACC_VARARGS;
        const NATIVE = ACC_NATIVE;
        const INTERFACE = ACC_INTERFACE;
        const ABSTRACT = ACC_ABSTRACT;
        const STRICT = ACC_STRICT;
        const SYNTHETIC = ACC_SYNTHETIC;
        const ANNOTATION = ACC_ANNOTATION;
        const ENUM = ACC_ENUM;
        const MODULE = ACC_MODULE;
        /// Module flag ([JVMS §4.7.25]); equals `SUPER`/`SYNCHRONIZED`.
        const OPEN = ACC_OPEN;
        /// Module-requires flag; equals `SUPER`/`SYNCHRONIZED`.
        const TRANSITIVE = ACC_TRANSITIVE;
        /// Module-requires flag; equals `VOLATILE`/`BRIDGE`.
        const STATIC_PHASE = ACC_STATIC_PHASE;
        /// Module-requires/opens/exports flag; equals `MODULE`.
        const MANDATED = ACC_MANDATED;
    }
}

impl JvmAccessFlags {
    /// Whether the member is `public`.
    pub fn is_public(self) -> bool {
        self.contains(JvmAccessFlags::PUBLIC)
    }

    /// Whether the member is `protected`.
    pub fn is_protected(self) -> bool {
        self.contains(JvmAccessFlags::PROTECTED)
    }

    /// Whether the member is `private`.
    pub fn is_private(self) -> bool {
        self.contains(JvmAccessFlags::PRIVATE)
    }

    /// Whether the member is `static`.
    pub fn is_static(self) -> bool {
        self.contains(JvmAccessFlags::STATIC)
    }

    /// Whether the member is `final`.
    pub fn is_final(self) -> bool {
        self.contains(JvmAccessFlags::FINAL)
    }

    /// Whether the member is `abstract`.
    pub fn is_abstract(self) -> bool {
        self.contains(JvmAccessFlags::ABSTRACT)
    }

    /// Whether the type is an interface ([JVMS §4.1]).
    pub fn is_interface(self) -> bool {
        self.contains(JvmAccessFlags::INTERFACE)
    }

    /// Whether the type is an annotation type ([JVMS §4.1]).
    pub fn is_annotation(self) -> bool {
        self.contains(JvmAccessFlags::ANNOTATION)
    }

    /// Whether the type is an enum ([JVMS §4.1]).
    pub fn is_enum(self) -> bool {
        self.contains(JvmAccessFlags::ENUM)
    }

    /// Whether the type is a module (`module-info`, [JVMS §4.1]).
    pub fn is_module(self) -> bool {
        self.contains(JvmAccessFlags::MODULE)
    }

    /// Whether the member was synthesized by the compiler rather than written
    /// in source ([JVMS §4.7.8](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.8)).
    pub fn is_synthetic(self) -> bool {
        self.contains(JvmAccessFlags::SYNTHETIC)
    }

    /// Whether the method is a varargs method ([JVMS §4.6]).
    pub fn is_varargs(self) -> bool {
        self.contains(JvmAccessFlags::VARARGS)
    }

    /// Whether the method is `native`.
    pub fn is_native(self) -> bool {
        self.contains(JvmAccessFlags::NATIVE)
    }

    /// Whether the method is `strictfp`.
    pub fn is_strictfp(self) -> bool {
        self.contains(JvmAccessFlags::STRICT)
    }

    /// Whether the method is `synchronized`.
    pub fn is_synchronized(self) -> bool {
        self.contains(JvmAccessFlags::SYNCHRONIZED)
    }

    /// Whether the field is `transient`.
    pub fn is_transient(self) -> bool {
        self.contains(JvmAccessFlags::TRANSIENT)
    }

    /// Whether the field is `volatile`.
    pub fn is_volatile(self) -> bool {
        self.contains(JvmAccessFlags::VOLATILE)
    }
}

#[cfg(test)]
mod tests {
    use super::JvmAccessFlags;

    #[test]
    fn predicates() {
        let flags = JvmAccessFlags::PUBLIC | JvmAccessFlags::STATIC | JvmAccessFlags::FINAL;
        assert!(flags.is_public());
        assert!(flags.is_static());
        assert!(flags.is_final());
        assert!(!flags.is_abstract());
        assert!(!flags.is_private());
    }
}

//method handle info
pub const REF_GET_FIELD: u8 = 1;
pub const REF_GET_STATIC: u8 = 2;
pub const REF_PUT_FIELD: u8 = 3;
pub const REF_PUT_STATIC: u8 = 4;
pub const REF_INVOKE_VIRTUAL: u8 = 5;
pub const REF_INVOKE_STATIC: u8 = 6;
pub const REF_INVOKE_SPECIAL: u8 = 7;
pub const REF_NEW_INVOKE_SPECIAL: u8 = 8;
pub const REF_INVOKE_INTERFACE: u8 = 9;

pub const TA_TARGET_CLASS_TYPE_PARAMETER: u8 = 0x00;
pub const TA_TARGET_METHOD_TYPE_PARAMETER: u8 = 0x01;
pub const TA_TARGET_CLASS_EXTENDS: u8 = 0x10;
pub const TA_TARGET_CLASS_TYPE_PARAMETER_BOUND: u8 = 0x11;
pub const TA_TARGET_METHOD_TYPE_PARAMETER_BOUND: u8 = 0x12;
pub const TA_TARGET_FIELD: u8 = 0x13;
pub const TA_TARGET_METHOD_RETURN: u8 = 0x14;
pub const TA_TARGET_METHOD_RECEIVER: u8 = 0x15;
pub const TA_TARGET_METHOD_FORMAL_PARAMETER: u8 = 0x16;
pub const TA_TARGET_THROWS: u8 = 0x17;
pub const TA_TARGET_LOCAL_VARIABLE: u8 = 0x40;
pub const TA_TARGET_RESOURCE_VARIABLE: u8 = 0x41;
pub const TA_TARGET_EXCEPTION_PARAMETER: u8 = 0x42;
pub const TA_TARGET_INSTANCEOF: u8 = 0x43;
pub const TA_TARGET_NEW: u8 = 0x44;
pub const TA_TARGET_CONSTRUCTOR_REFERENCE_RECEIVER: u8 = 0x45;
pub const TA_TARGET_METHOD_REFERENCE_RECEIVER: u8 = 0x46;
pub const TA_TARGET_CAST: u8 = 0x47;
pub const TA_TARGET_CONSTRUCTOR_INVOCATION_TYPE_ARGUMENT: u8 = 0x48;
pub const TA_TARGET_METHOD_INVOCATION_TYPE_ARGUMENT: u8 = 0x49;
pub const TA_TARGET_CONSTRUCTOR_REFERENCE_TYPE_ARGUMENT: u8 = 0x4A;
pub const TA_TARGET_METHOD_REFERENCE_TYPE_ARGUMENT: u8 = 0x4B;

pub const TA_TYPE_PATH_ARRAY: u8 = 0;
pub const TA_TYPE_PATH_INNER_TYPE: u8 = 1;
pub const TA_TYPE_PATH_WILDCARD: u8 = 2;
pub const TA_TYPE_PATH_TYPE_ARGUMENT: u8 = 3;
