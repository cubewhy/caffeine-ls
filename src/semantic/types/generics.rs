use crate::semantic::types::type_name::TypeName;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub enum JvmType {
    Object(String, Vec<JvmType>),      // e.g. "java/util/List", [String]
    TypeVar(String),                   // e.g. "T", "E"
    Array(Box<JvmType>),               // e.g. String[]
    Primitive(char),                   // e.g. 'I'
    Wildcard,                          // e.g. *
    WildcardBound(char, Box<JvmType>), // e.g. + (extends) or - (super)
}

impl JvmType {
    /// Parse the JVM signature, for example, `Ljava/util/List<Ljava/lang/String;>;`
    pub fn parse(s: &str) -> Option<(Self, &str)> {
        let first = s.chars().next()?;
        match first {
            'L' => {
                let mut i = 1;
                let bytes = s.as_bytes();
                while i < bytes.len() && bytes[i] != b'<' && bytes[i] != b';' {
                    i += 1;
                }
                let internal_name = &s[1..i];
                let mut args = Vec::new();
                let mut rest = &s[i..];

                if rest.starts_with('<') {
                    rest = &rest[1..];
                    while !rest.starts_with('>') {
                        let (arg, next_rest) = JvmType::parse(rest)?;
                        args.push(arg);
                        rest = next_rest;
                    }
                    rest = &rest[1..]; // Consume '>'
                }
                if rest.starts_with(';') {
                    rest = &rest[1..]; // Consume ';'
                }
                Some((JvmType::Object(internal_name.to_string(), args), rest))
            }
            'T' => {
                let end = s.find(';')?;
                Some((JvmType::TypeVar(s[1..end].to_string()), &s[end + 1..]))
            }
            '[' => {
                let (inner, rest) = JvmType::parse(&s[1..])?;
                Some((JvmType::Array(Box::new(inner)), rest))
            }
            '*' => Some((JvmType::Wildcard, &s[1..])),
            '+' | '-' => {
                let (inner, rest) = JvmType::parse(&s[1..])?;
                Some((JvmType::WildcardBound(first, Box::new(inner)), rest))
            }
            'V' | 'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' => {
                Some((JvmType::Primitive(first), &s[1..]))
            }
            _ => None,
        }
    }

    /// Replace generic variables, for example, replace `T` with the actual `Ljava/lang/String;`
    pub fn substitute(&self, type_params: &[String], type_args: &[JvmType]) -> Self {
        match self {
            JvmType::TypeVar(name) => {
                if let Some(pos) = type_params.iter().position(|p| p == name)
                    && pos < type_args.len()
                {
                    return type_args[pos].clone();
                }
                self.clone()
            }
            JvmType::Object(name, args) => {
                let new_args = args
                    .iter()
                    .map(|a| {
                        let substituted = a.substitute(type_params, type_args);
                        box_jvm_primitive(&substituted)
                    })
                    .collect();
                JvmType::Object(name.clone(), new_args)
            }
            JvmType::Array(inner) => {
                JvmType::Array(Box::new(inner.substitute(type_params, type_args)))
            }
            JvmType::WildcardBound(c, inner) => {
                JvmType::WildcardBound(*c, Box::new(inner.substitute(type_params, type_args)))
            }
            _ => self.clone(),
        }
    }

    /// Convert to the format used internally by TypeResolver: `java/util/List<Ljava/lang/String;>`
    pub fn to_type_name(&self) -> TypeName {
        match self {
            JvmType::Object(name, args) => {
                let inner_args: Vec<TypeName> = args.iter().map(|a| a.to_type_name()).collect();
                TypeName::with_args(name.as_str(), inner_args)
            }
            JvmType::TypeVar(name) => TypeName::new(name.as_str()),
            JvmType::Array(inner) => inner.to_type_name().wrap_array(),
            JvmType::Wildcard => TypeName::new("*"),
            JvmType::WildcardBound(c, inner) => {
                TypeName::with_args(c.to_string(), vec![inner.to_type_name()])
            }
            JvmType::Primitive(c) => TypeName::new(java_primitive_char_to_name(*c)),
        }
    }

    pub fn to_internal_name_string(&self) -> String {
        self.to_type_name().to_internal_with_generics()
    }

    /// Convert to the standard JVM signature format: `Ljava/util/List<Ljava/lang/String;>;`
    pub fn to_signature_string(&self) -> String {
        match self {
            JvmType::Object(name, args) => {
                if args.is_empty() {
                    format!("L{};", name)
                } else {
                    let arg_strs: Vec<_> = args.iter().map(|a| a.to_signature_string()).collect();
                    format!("L{}<{}>;", name, arg_strs.join(""))
                }
            }
            JvmType::TypeVar(name) => format!("T{};", name),
            JvmType::Array(inner) => format!("[{}", inner.to_signature_string()),
            JvmType::Wildcard => "*".to_string(),
            JvmType::WildcardBound(c, inner) => format!("{}{}", c, inner.to_signature_string()),
            JvmType::Primitive(c) => c.to_string(),
        }
    }

    pub fn to_java_like_string(&self) -> String {
        match self {
            JvmType::Object(name, args) => {
                if args.is_empty() {
                    name.clone()
                } else {
                    let rendered_args: Vec<_> =
                        args.iter().map(|a| a.to_java_like_string()).collect();
                    format!("{}<{}>", name, rendered_args.join(", "))
                }
            }
            JvmType::TypeVar(name) => name.clone(),
            JvmType::Array(inner) => format!("{}[]", inner.to_java_like_string()),
            JvmType::Primitive(c) => java_primitive_char_to_name(*c).to_string(),
            JvmType::Wildcard => "?".to_string(),
            JvmType::WildcardBound('+', inner) => {
                format!("? extends {}", inner.to_java_like_string())
            }
            JvmType::WildcardBound('-', inner) => {
                format!("? super {}", inner.to_java_like_string())
            }
            JvmType::WildcardBound(other, inner) => {
                format!("? ({}) {}", other, inner.to_java_like_string())
            }
        }
    }
}

// Extract generic parameters from the current variable type
// For example, extract base="java/util/List" and args=[String] from "java/util/List<Ljava/lang/String;>".
pub fn split_internal_name(internal: &str) -> (&str, Vec<JvmType>) {
    if let Some(pos) = internal.find('<') {
        let base = &internal[..pos];
        let args_str = &internal[pos + 1..internal.len() - 1]; // "Ljava/lang/String;"
        let mut args = Vec::new();
        let mut rest = args_str;
        while !rest.is_empty() {
            if let Some((ty, next_rest)) = JvmType::parse(rest) {
                args.push(ty);
                rest = next_rest;
            } else {
                // If parsing fails, you must exit to prevent an infinite loop.
                break;
            }
        }
        (base, args)
    } else {
        (internal, vec![])
    }
}

fn find_type_parameter_prefix_end(signature: &str) -> Option<usize> {
    if !signature.starts_with('<') {
        return None;
    }

    let mut depth = 0;
    for (i, c) in signature.char_indices() {
        if c == '<' {
            depth += 1;
        } else if c == '>' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Extract the declared generic parameter names from the class's JVM signature.
/// Example: "<K:Ljava/lang/Object;V:Ljava/lang/Object;>Ljava/lang/Object;" -> ["K", "V"]
pub fn parse_class_type_parameters(signature: &str) -> Vec<String> {
    let mut params = Vec::new();
    let Some(end_angle) = find_type_parameter_prefix_end(signature) else {
        return params;
    };

    let mut rest = &signature[1..end_angle];
    while !rest.is_empty() {
        if let Some(colon_pos) = rest.find(':') {
            let param_name = rest[..colon_pos].trim();
            if !param_name.is_empty() {
                params.push(param_name.to_string());
            }

            rest = &rest[colon_pos + 1..];
            // 跳过泛型约束(Bounds)直到遇到 ';'
            let mut bound_depth = 0;
            let mut next_start = rest.len();
            for (i, c) in rest.char_indices() {
                match c {
                    '<' => bound_depth += 1,
                    '>' => bound_depth -= 1,
                    ';' if bound_depth == 0 => {
                        next_start = i + 1;
                        break;
                    }
                    _ => {}
                }
            }
            rest = &rest[next_start..];
            // 处理多重约束 (如 ::Ljava/lang/Comparable;)
            while rest.starts_with(':') {
                rest = &rest[1..];
                // 再次查找结束符...
                let mut bound_depth = 0;
                let mut inner_next = rest.len();
                for (i, c) in rest.char_indices() {
                    if c == '<' {
                        bound_depth += 1;
                    } else if c == '>' {
                        bound_depth -= 1;
                    } else if c == ';' && bound_depth == 0 {
                        inner_next = i + 1;
                        break;
                    }
                }
                rest = &rest[inner_next..];
            }
        } else {
            break;
        }
    }
    params
}

/// Extract declared type parameter bounds from a JVM signature prefix.
/// Example: `<T:Ljava/lang/Object;:Ljava/io/Closeable;>...` -> `T => [Object, Closeable]`
pub fn parse_type_parameter_bounds(signature: &str) -> HashMap<String, Vec<JvmType>> {
    let Some(end_angle) = find_type_parameter_prefix_end(signature) else {
        return HashMap::new();
    };

    let mut bounds = HashMap::new();
    let mut rest = &signature[1..end_angle];
    while !rest.is_empty() {
        let Some(colon_pos) = rest.find(':') else {
            break;
        };
        let param_name = rest[..colon_pos].trim();
        if param_name.is_empty() {
            break;
        }

        rest = &rest[colon_pos + 1..];
        let mut param_bounds = Vec::new();

        // Empty class bound (`::LIface;`) is valid; only parse when the slot is populated.
        if !rest.starts_with(':')
            && let Some((bound, next_rest)) = JvmType::parse(rest)
        {
            param_bounds.push(bound);
            rest = next_rest;
        }

        while rest.starts_with(':') {
            rest = &rest[1..];
            if rest.starts_with(':') {
                continue;
            }
            let Some((bound, next_rest)) = JvmType::parse(rest) else {
                break;
            };
            param_bounds.push(bound);
            rest = next_rest;
        }

        bounds.insert(param_name.to_string(), param_bounds);
    }
    bounds
}

fn is_likely_type_var_name(name: &str) -> bool {
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
}

fn expand_type_name_with_bounds_impl(
    ty: &TypeName,
    method_bounds: &HashMap<String, Vec<JvmType>>,
    class_bounds: &HashMap<String, Vec<JvmType>>,
    visiting: &mut HashSet<String>,
) -> Option<TypeName> {
    if ty.is_intersection() {
        let expanded_bounds = ty
            .args
            .iter()
            .map(|bound| {
                expand_type_name_with_bounds_impl(bound, method_bounds, class_bounds, visiting)
                    .unwrap_or_else(|| bound.clone())
            })
            .collect::<Vec<_>>();
        return Some(TypeName::intersection(expanded_bounds).with_array_dims(ty.array_dims));
    }

    let expanded_args = ty
        .args
        .iter()
        .map(|arg| {
            expand_type_name_with_bounds_impl(arg, method_bounds, class_bounds, visiting)
                .unwrap_or_else(|| arg.clone())
        })
        .collect::<Vec<_>>();

    let base = ty.base_internal.as_ref();
    if ty.args.is_empty() && !ty.contains_slash() && is_likely_type_var_name(base) {
        let bounds = method_bounds.get(base).or_else(|| class_bounds.get(base))?;
        if !visiting.insert(base.to_string()) {
            return None;
        }
        let expanded_bounds = bounds
            .iter()
            .map(|bound| {
                let bound_ty = bound.to_type_name();
                expand_type_name_with_bounds_impl(&bound_ty, method_bounds, class_bounds, visiting)
                    .unwrap_or(bound_ty)
            })
            .collect::<Vec<_>>();
        visiting.remove(base);
        if expanded_bounds.is_empty() {
            return None;
        }
        return Some(TypeName::intersection(expanded_bounds).with_array_dims(ty.array_dims));
    }

    Some(TypeName {
        base_internal: ty.base_internal.clone(),
        args: expanded_args,
        array_dims: ty.array_dims,
    })
}

pub fn expand_type_name_with_type_parameter_bounds(
    ty: &TypeName,
    method_signature: Option<&str>,
    class_signature: Option<&str>,
) -> Option<TypeName> {
    let method_bounds = method_signature
        .map(parse_type_parameter_bounds)
        .unwrap_or_default();
    let class_bounds = class_signature
        .map(parse_type_parameter_bounds)
        .unwrap_or_default();
    if method_bounds.is_empty() && class_bounds.is_empty() {
        return None;
    }
    expand_type_name_with_bounds_impl(ty, &method_bounds, &class_bounds, &mut HashSet::new())
}

/// Extract declared method type parameter names from a JVM method signature.
/// Example: `<R:Ljava/lang/Object;>(...)TR;` -> ["R"]
pub fn parse_method_type_parameters(signature: &str) -> Vec<String> {
    parse_class_type_parameters(signature)
}

/// Parse JVM method signature/descriptor into parameter and return JVM types.
/// Supports optional leading method type parameters.
pub fn parse_method_signature_types(signature: &str) -> Option<(Vec<JvmType>, JvmType)> {
    let mut s = signature;
    if s.starts_with('<') {
        let mut depth = 0i32;
        let mut end = None;
        for (i, c) in s.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        s = &s[end + 1..];
    }

    if !s.starts_with('(') {
        return None;
    }
    let close = s.find(')')?;
    let mut params_raw = &s[1..close];
    let mut params = Vec::new();
    while !params_raw.is_empty() {
        let (ty, rest) = JvmType::parse(params_raw)?;
        params.push(ty);
        params_raw = rest;
    }
    let (ret, _) = JvmType::parse(&s[close + 1..])?;
    Some((params, ret))
}

/// Substitute type variables in a JVM type using explicit bindings.
pub fn substitute_type_vars(ty: &JvmType, bindings: &HashMap<String, JvmType>) -> JvmType {
    match ty {
        JvmType::TypeVar(name) => bindings.get(name).cloned().unwrap_or_else(|| ty.clone()),
        JvmType::Object(name, args) => JvmType::Object(
            name.clone(),
            args.iter()
                .map(|a| substitute_type_vars(a, bindings))
                .collect(),
        ),
        JvmType::Array(inner) => JvmType::Array(Box::new(substitute_type_vars(inner, bindings))),
        JvmType::WildcardBound(c, inner) => {
            JvmType::WildcardBound(*c, Box::new(substitute_type_vars(inner, bindings)))
        }
        _ => ty.clone(),
    }
}

/// Perform type substitution. If receiver_internal contains generics (such as List<Ljava/lang/String;>), then attempt to replace target_jvm_type (such as TE;) with String.
pub fn substitute_type(
    receiver_internal: &str,
    class_generic_signature: Option<&str>,
    target_jvm_type_str: &str,
) -> Option<TypeName> {
    let (_, receiver_type_args) = split_internal_name(receiver_internal);
    if receiver_type_args.is_empty() {
        return None;
    }

    let class_type_params = class_generic_signature
        .map(parse_class_type_parameters)
        .unwrap_or_default();

    if class_type_params.is_empty() {
        return None;
    }

    let (mut ret_jvm_type, _) = JvmType::parse(target_jvm_type_str)?;
    ret_jvm_type = ret_jvm_type.substitute(&class_type_params, &receiver_type_args);

    Some(ret_jvm_type.to_type_name())
}

fn java_primitive_char_to_name(c: char) -> &'static str {
    match c {
        'I' => "int",
        'Z' => "boolean",
        'J' => "long",
        'F' => "float",
        'D' => "double",
        'B' => "byte",
        'C' => "char",
        'S' => "short",
        'V' => "void",
        _ => "unknown",
    }
}

pub fn box_jvm_primitive(ty: &JvmType) -> JvmType {
    match ty {
        JvmType::Primitive(c) => {
            let boxed = match c {
                'I' => "java/lang/Integer",
                'J' => "java/lang/Long",
                'D' => "java/lang/Double",
                'F' => "java/lang/Float",
                'Z' => "java/lang/Boolean",
                'B' => "java/lang/Byte",
                'S' => "java/lang/Short",
                'C' => "java/lang/Character",
                _ => return ty.clone(),
            };
            JvmType::Object(boxed.to_string(), vec![])
        }
        _ => ty.clone(),
    }
}

#[cfg(test)]
mod tests {
    use crate::semantic::types::generics::JvmType;

    #[test]
    fn test_wildcard_bound_display() {
        let ty = JvmType::WildcardBound(
            '-',
            Box::new(JvmType::Object("java/lang/String".to_string(), vec![])),
        );
        assert_eq!(ty.to_java_like_string(), "? super java/lang/String");
    }

    #[test]
    fn test_type_var_display() {
        let ty = JvmType::TypeVar("E".to_string());

        // Ensure that the raw JVM format "TE;" is not output.
        assert_eq!(ty.to_internal_name_string(), "E");
    }

    #[test]
    fn test_array_of_type_var_display() {
        // toArray(T[])
        let ty = JvmType::Array(Box::new(JvmType::TypeVar("T".to_string())));
        assert_eq!(ty.to_internal_name_string(), "T[]");
    }

    #[test]
    fn test_substitute_primitive_in_generic_arg_position_boxes_to_wrapper() {
        // Stream<R> where R = int -> Stream<Integer>
        let stream_ty = JvmType::Object(
            "java/util/stream/Stream".to_string(),
            vec![JvmType::TypeVar("R".to_string())],
        );
        let substituted = stream_ty.substitute(&["R".to_string()], &[JvmType::Primitive('I')]);
        assert_eq!(
            substituted,
            JvmType::Object(
                "java/util/stream/Stream".to_string(),
                vec![JvmType::Object("java/lang/Integer".to_string(), vec![])],
            )
        );
    }

    #[test]
    fn test_substitute_primitive_at_top_level_does_not_box() {
        // TypeVar T = int (at top level, not in Object args) stays int
        let ty = JvmType::TypeVar("T".to_string());
        let substituted = ty.substitute(&["T".to_string()], &[JvmType::Primitive('I')]);
        assert_eq!(substituted, JvmType::Primitive('I'));
    }

    #[test]
    fn test_substitute_double_in_list_generic_arg_boxes_to_double_wrapper() {
        let list_ty = JvmType::Object(
            "java/util/List".to_string(),
            vec![JvmType::TypeVar("E".to_string())],
        );
        let substituted = list_ty.substitute(&["E".to_string()], &[JvmType::Primitive('D')]);
        assert_eq!(
            substituted,
            JvmType::Object(
                "java/util/List".to_string(),
                vec![JvmType::Object("java/lang/Double".to_string(), vec![])],
            )
        );
    }
}
