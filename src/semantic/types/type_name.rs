use std::sync::Arc;

/// Structured internal type names, using JVM-internal base names plus generic args and array dims.
/// - Objects: base_internal = "java/lang/String"
/// - Primitive types: base_internal = "int"
/// - Arrays: array_dims > 0
/// - With generics: args = [TypeName], rendered as "java/util/List<Ljava/lang/String;>"
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeName {
    pub base_internal: Arc<str>,
    pub args: Vec<TypeName>,
    pub array_dims: usize,
}

impl TypeName {
    const INTERSECTION_INTERNAL: &'static str = "&";

    pub fn new(base_internal: impl Into<Arc<str>>) -> Self {
        let raw: Arc<str> = base_internal.into();
        let mut base = raw.as_ref().trim();
        let mut dims = 0usize;
        while let Some(stripped) = base.strip_suffix("[]") {
            dims += 1;
            base = stripped.trim_end();
        }
        TypeName {
            base_internal: Arc::from(base),
            args: Vec::new(),
            array_dims: dims,
        }
    }

    pub fn with_args(base_internal: impl Into<Arc<str>>, args: Vec<TypeName>) -> Self {
        TypeName {
            base_internal: base_internal.into(),
            args,
            array_dims: 0,
        }
    }

    pub fn intersection(bounds: Vec<TypeName>) -> Self {
        let mut flattened = Vec::new();
        for bound in bounds {
            if bound.is_intersection() {
                flattened.extend(bound.args);
            } else {
                flattened.push(bound);
            }
        }
        match flattened.len() {
            0 => TypeName::new(Self::INTERSECTION_INTERNAL),
            1 => flattened
                .into_iter()
                .next()
                .unwrap_or_else(|| TypeName::new("unknown")),
            _ => TypeName::with_args(Self::INTERSECTION_INTERNAL, flattened),
        }
    }

    pub fn with_array_dims(mut self, dims: usize) -> Self {
        self.array_dims = dims;
        self
    }

    pub fn is_array(&self) -> bool {
        self.array_dims > 0
    }

    pub fn is_primitive(&self) -> bool {
        if self.is_intersection() {
            return false;
        }
        matches!(
            self.base_internal.as_ref(),
            "int" | "long" | "short" | "byte" | "char" | "float" | "double" | "boolean" | "void"
        )
    }

    pub fn is_intersection(&self) -> bool {
        self.base_internal.as_ref() == Self::INTERSECTION_INTERNAL
    }

    pub fn has_generics(&self) -> bool {
        !self.args.is_empty()
    }

    /// Erase generic parameters and array dims for index lookup.
    pub fn erased_internal(&self) -> &str {
        if self.is_intersection() {
            if let Some(primary) = self.primary_bound() {
                return primary.erased_internal();
            }
        }
        &self.base_internal
    }

    /// Erase generics but keep array dims, e.g. "java/lang/String[]".
    pub fn erased_internal_with_arrays(&self) -> String {
        let mut s = if self.is_intersection() {
            self.primary_bound()
                .map(TypeName::erased_internal_with_arrays)
                .unwrap_or_else(|| self.base_internal.to_string())
        } else {
            self.base_internal.to_string()
        };
        if self.array_dims > 0 {
            s.push_str(&"[]".repeat(self.array_dims));
        }
        s
    }

    pub fn contains_slash(&self) -> bool {
        if self.is_intersection() {
            return self.args.iter().any(TypeName::contains_slash);
        }
        self.base_internal.contains('/')
    }

    pub fn wildcard_upper_bound(&self) -> Option<&TypeName> {
        if self.base_internal.as_ref() == "+" {
            return self.args.first();
        }
        None
    }

    pub fn primary_bound(&self) -> Option<&TypeName> {
        self.is_intersection().then(|| self.args.first()).flatten()
    }

    pub fn bounds_for_lookup(&self) -> Vec<&TypeName> {
        if self.is_intersection() {
            return self.args.iter().collect();
        }
        vec![self]
    }

    /// "java/lang/String[][]" → Some("java/lang/String[]")
    pub fn element_type(&self) -> Option<TypeName> {
        if self.array_dims == 0 {
            return None;
        }
        let mut next = self.clone();
        next.array_dims -= 1;
        Some(next)
    }

    /// "java/lang/String" → "java/lang/String[]"
    pub fn wrap_array(&self) -> TypeName {
        let mut next = self.clone();
        next.array_dims += 1;
        next
    }

    /// Internal style with generics, e.g. "java/util/List<Ljava/lang/String;>".
    pub fn to_internal_with_generics(&self) -> String {
        if self.is_intersection() {
            let mut rendered = self
                .args
                .iter()
                .map(TypeName::to_internal_with_generics)
                .collect::<Vec<_>>()
                .join(" & ");
            if self.array_dims > 0 {
                rendered.push_str(&"[]".repeat(self.array_dims));
            }
            return rendered;
        }

        let base = self.base_internal.as_ref();
        let mut s = if self.args.is_empty() {
            base.to_string()
        } else {
            let arg_sigs: Vec<String> = self.args.iter().map(|a| a.to_jvm_signature()).collect();
            format!("{}<{}>", base, arg_sigs.join(""))
        };
        if self.array_dims > 0 {
            s.push_str(&"[]".repeat(self.array_dims));
        }
        s
    }

    /// Internal style with generics for substitution/rendering paths.
    /// Unlike `to_internal_with_generics`, this keeps non-slash class-like names
    /// as object signatures in generic arguments (e.g. `LBox;`), while retaining
    /// likely type variables (e.g. `TR;`).
    pub fn to_internal_with_generics_for_substitution(&self) -> String {
        if self.is_intersection() {
            let mut rendered = self
                .args
                .iter()
                .map(TypeName::to_internal_with_generics_for_substitution)
                .collect::<Vec<_>>()
                .join(" & ");
            if self.array_dims > 0 {
                rendered.push_str(&"[]".repeat(self.array_dims));
            }
            return rendered;
        }

        fn is_likely_type_var(name: &str) -> bool {
            let mut chars = name.chars();
            let Some(first) = chars.next() else {
                return false;
            };
            if !first.is_ascii_uppercase() {
                return false;
            }
            if name.len() == 1 {
                return true;
            }
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
                && matches!(
                    first,
                    'E' | 'K' | 'R' | 'T' | 'U' | 'V' | 'W' | 'X' | 'Y' | 'Z'
                )
        }

        fn to_jvm_sig_for_substitution(ty: &TypeName) -> String {
            let mut sig = match ty.base_internal.as_ref() {
                "byte" => "B".to_string(),
                "char" => "C".to_string(),
                "double" => "D".to_string(),
                "float" => "F".to_string(),
                "int" => "I".to_string(),
                "long" => "J".to_string(),
                "short" => "S".to_string(),
                "boolean" => "Z".to_string(),
                "void" => "V".to_string(),
                "*" => "*".to_string(),
                "+" | "-" => {
                    if let Some(inner) = ty.args.first() {
                        format!("{}{}", ty.base_internal, to_jvm_sig_for_substitution(inner))
                    } else {
                        ty.base_internal.to_string()
                    }
                }
                base if base.contains('/') => {
                    if ty.args.is_empty() {
                        format!("L{};", base)
                    } else {
                        let arg_sigs: Vec<String> =
                            ty.args.iter().map(to_jvm_sig_for_substitution).collect();
                        format!("L{}<{}>;", base, arg_sigs.join(""))
                    }
                }
                other if is_likely_type_var(other) => format!("T{};", other),
                other => {
                    if ty.args.is_empty() {
                        format!("L{};", other)
                    } else {
                        let arg_sigs: Vec<String> =
                            ty.args.iter().map(to_jvm_sig_for_substitution).collect();
                        format!("L{}<{}>;", other, arg_sigs.join(""))
                    }
                }
            };

            if ty.array_dims > 0 {
                sig = format!("{}{}", "[".repeat(ty.array_dims), sig);
            }
            sig
        }

        let base = self.base_internal.as_ref();
        let mut s = if self.args.is_empty() {
            base.to_string()
        } else {
            let arg_sigs: Vec<String> = self.args.iter().map(to_jvm_sig_for_substitution).collect();
            format!("{}<{}>", base, arg_sigs.join(""))
        };
        if self.array_dims > 0 {
            s.push_str(&"[]".repeat(self.array_dims));
        }
        s
    }

    /// JVM signature, e.g. "Ljava/util/List<Ljava/lang/String;>;" or "[I".
    pub fn to_jvm_signature(&self) -> String {
        if self.is_intersection() {
            if let Some(primary) = self.primary_bound() {
                let mut sig = primary.to_jvm_signature();
                if self.array_dims > 0 {
                    sig = format!("{}{}", "[".repeat(self.array_dims), sig);
                }
                return sig;
            }
            return "Ljava/lang/Object;".to_string();
        }

        let mut sig = match self.base_internal.as_ref() {
            "byte" => "B".to_string(),
            "char" => "C".to_string(),
            "double" => "D".to_string(),
            "float" => "F".to_string(),
            "int" => "I".to_string(),
            "long" => "J".to_string(),
            "short" => "S".to_string(),
            "boolean" => "Z".to_string(),
            "void" => "V".to_string(),
            "*" => "*".to_string(),
            "+" | "-" => {
                if let Some(inner) = self.args.first() {
                    format!("{}{}", self.base_internal, inner.to_jvm_signature())
                } else {
                    self.base_internal.to_string()
                }
            }
            base if base.contains('/') => {
                if self.args.is_empty() {
                    format!("L{};", base)
                } else {
                    let arg_sigs: Vec<String> =
                        self.args.iter().map(|a| a.to_jvm_signature()).collect();
                    format!("L{}<{}>;", base, arg_sigs.join(""))
                }
            }
            // Treat other identifiers as type variables (e.g. "T", "E").
            other => format!("T{};", other),
        };

        if self.array_dims > 0 {
            sig = format!("{}{}", "[".repeat(self.array_dims), sig);
        }
        sig
    }
}

impl From<&str> for TypeName {
    fn from(s: &str) -> Self {
        TypeName::new(s)
    }
}

impl From<String> for TypeName {
    fn from(s: String) -> Self {
        TypeName::new(s)
    }
}

impl From<Arc<str>> for TypeName {
    fn from(arc: Arc<str>) -> Self {
        TypeName::new(arc)
    }
}

impl std::fmt::Display for TypeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_internal_with_generics())
    }
}

#[cfg(test)]
mod tests {
    use super::TypeName;

    #[test]
    fn test_erased_internal_keeps_base_and_preserves_array_via_array_helper() {
        let ty = TypeName::new("int[]");
        assert_eq!(ty.erased_internal(), "int");
        assert_eq!(ty.erased_internal_with_arrays(), "int[]");
    }

    #[test]
    fn test_to_internal_with_generics_preserves_array_dims() {
        let ty = TypeName::new("java/lang/String[][]");
        assert_eq!(ty.to_internal_with_generics(), "java/lang/String[][]");
    }

    #[test]
    fn test_intersection_erases_to_primary_bound() {
        let ty = TypeName::intersection(vec![
            TypeName::new("java/io/Closeable"),
            TypeName::new("java/lang/Runnable"),
        ]);
        assert!(ty.is_intersection());
        assert_eq!(ty.erased_internal(), "java/io/Closeable");
        assert_eq!(
            ty.to_internal_with_generics(),
            "java/io/Closeable & java/lang/Runnable"
        );
    }
}
