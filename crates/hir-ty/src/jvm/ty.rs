//! JVM type primitives shared by every source language's type layer: the
//! naming, boxing/unboxing and numeric-promotion tables over the JVM primitive
//! types ([JLS §5.1.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.7),
//! [§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8),
//! [§5.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2)).

use syntax::stub::PrimitiveType;

/// The display name of a primitive type.
pub fn primitive_name(p: PrimitiveType) -> &'static str {
    match p {
        PrimitiveType::Int => "int",
        PrimitiveType::Long => "long",
        PrimitiveType::Float => "float",
        PrimitiveType::Double => "double",
        PrimitiveType::Boolean => "boolean",
        PrimitiveType::Byte => "byte",
        PrimitiveType::Char => "char",
        PrimitiveType::Short => "short",
        PrimitiveType::Void => "void",
    }
}

/// The reference type a primitive boxes to ([JLS §5.1.7], table 5.1-D).
pub fn boxed_type(p: PrimitiveType) -> &'static str {
    match p {
        PrimitiveType::Boolean => "java.lang.Boolean",
        PrimitiveType::Byte => "java.lang.Byte",
        PrimitiveType::Short => "java.lang.Short",
        PrimitiveType::Char => "java.lang.Character",
        PrimitiveType::Int => "java.lang.Integer",
        PrimitiveType::Long => "java.lang.Long",
        PrimitiveType::Float => "java.lang.Float",
        PrimitiveType::Double => "java.lang.Double",
        PrimitiveType::Void => "java.lang.Void",
    }
}

/// The primitive a reference type unboxes to ([JLS §5.1.8], reverse of
/// [`boxed_type`]), or `None` for non-boxed reference types.
pub fn unboxed_primitive(fqn: &str) -> Option<PrimitiveType> {
    use PrimitiveType::*;
    match fqn {
        "java.lang.Boolean" => Some(Boolean),
        "java.lang.Byte" => Some(Byte),
        "java.lang.Short" => Some(Short),
        "java.lang.Character" => Some(Char),
        "java.lang.Integer" => Some(Int),
        "java.lang.Long" => Some(Long),
        "java.lang.Float" => Some(Float),
        "java.lang.Double" => Some(Double),
        "java.lang.Void" => Some(Void),
        _ => None,
    }
}

/// Unary numeric promotion ([§5.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.1)):
/// `byte`, `short` and `char` promote to `int`; the other numeric types keep
/// their type. Applied to the unboxed operand of a binary expression
/// ([§5.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2),
/// [§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8)),
/// so `Character + Character` promotes to `int`.
pub fn numeric_promotion(p: PrimitiveType) -> PrimitiveType {
    use PrimitiveType::*;
    match p {
        Byte | Short | Char => Int,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_names() {
        assert_eq!(primitive_name(PrimitiveType::Int), "int");
        assert_eq!(primitive_name(PrimitiveType::Void), "void");
    }

    #[test]
    fn boxing_round_trip() {
        for p in [
            PrimitiveType::Int,
            PrimitiveType::Long,
            PrimitiveType::Boolean,
            PrimitiveType::Char,
        ] {
            assert_eq!(unboxed_primitive(boxed_type(p)), Some(p));
        }
        assert_eq!(unboxed_primitive("java.lang.String"), None);
    }

    #[test]
    fn promotion() {
        assert_eq!(numeric_promotion(PrimitiveType::Byte), PrimitiveType::Int);
        assert_eq!(numeric_promotion(PrimitiveType::Char), PrimitiveType::Int);
        assert_eq!(numeric_promotion(PrimitiveType::Long), PrimitiveType::Long);
    }
}
