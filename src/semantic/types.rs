use self::generics::{JvmType, split_internal_name, substitute_type};
use self::type_name::TypeName;
use super::context::{LocalVar, SemanticContext};
use crate::{
    index::{IndexView, MethodSummary},
    jvm::descriptor::{consume_one_descriptor_type, split_param_descriptors},
};
use std::collections::HashSet;
use std::sync::Arc;

pub mod generics;
pub mod symbol_resolver;
pub mod type_name;

pub trait SymbolProvider {
    /// Strict query: Given internal_name (e.g., "java/util/Map$Entry")
    // If it exists in the index, return its absolutely correct source code name (e.g., "java.util.Map.Entry")
    // If it does not exist, strictly return None
    fn resolve_source_name(&self, internal_name: &str) -> Option<String>;
}

pub struct TypeResolver<'idx> {
    view: &'idx IndexView,
}

impl<'idx> TypeResolver<'idx> {
    pub fn new(view: &'idx IndexView) -> Self {
        Self { view }
    }

    pub fn resolve(
        &self,
        expr: &str,
        locals: &[LocalVar],
        enclosing: Option<&Arc<str>>,
    ) -> Option<TypeName> {
        let expr = expr.trim();

        // `new` constructor: new pkg.Cls(...), new Cls(...), new int[3], new String[]{...}
        if let Some(rest) = expr.strip_prefix("new ") {
            let rest = rest.trim();

            // Find type boundaries: stop when encountering '(', '<', '[', '{')
            let boundary = rest.find(['(', '<', '[', '{']).unwrap_or(rest.len());
            let raw_ty = rest[..boundary].trim();

            if raw_ty.is_empty() {
                return None;
            }

            // primitive new int[...]
            let mut base = match raw_ty {
                "byte" | "short" | "int" | "long" | "float" | "double" | "boolean" | "char" => {
                    TypeName::new(raw_ty)
                }
                _ => {
                    if raw_ty.contains('.') {
                        let internal = raw_ty.replace('.', "/");
                        if self.view.get_class(&internal).is_some() {
                            TypeName::new(internal)
                        } else {
                            return None;
                        }
                    } else {
                        TypeName::new(raw_ty) // will be expanded later
                    }
                }
            };

            // matrix: new String[3][] / new int[3]
            let after = rest[boundary..].trim_start();
            if after.starts_with('[') || after.starts_with('{') {
                let brace_idx = after.find('{').unwrap_or(after.len());
                let dims = after[..brace_idx].matches('[').count();
                for _ in 0..dims {
                    base = base.wrap_array();
                }
            }

            return Some(base);
        }

        // array init
        if expr.ends_with(']')
            && let Some(bracket_idx) = expr.rfind('[')
        {
            let array_expr = expr[..bracket_idx].trim();
            if !array_expr.is_empty()
                && let Some(array_type) = self.resolve(array_expr, locals, enclosing)
            {
                return array_type.element_type();
            }
        }

        // `this`
        if expr == "this" {
            return enclosing.map(|arc| TypeName::new(arc.to_string()));
        }

        // Strings
        if expr.starts_with('"') {
            return Some(TypeName::new("java/lang/String"));
        }

        // boolean
        if expr == "true" || expr == "false" {
            return Some(TypeName::new("boolean"));
        }

        // Local variables take precedence over literals in the evaluation
        if let Some(lv) = locals.iter().find(|lv| lv.name.as_ref() == expr) {
            return Some(lv.type_internal.clone());
        }

        if let Some(enc) = enclosing {
            for class in self.view.mro(enc) {
                if let Some(f) = class.fields.iter().find(|f| f.name.as_ref() == expr) {
                    if let Some(ty) = singleton_descriptor_to_type(&f.descriptor) {
                        return Some(TypeName::new(ty));
                    } else {
                        return parse_single_type_to_internal(&f.descriptor);
                    }
                }
            }
        }

        // Class name (index lookup)
        if self.view.get_class(expr).is_some() {
            return Some(TypeName::new(expr));
        }

        // Literal checks should be placed last, with strict numeric prefix validation.
        if expr.parse::<i64>().is_ok() {
            return Some(TypeName::new("int"));
        }
        if let Some(prefix) = expr.strip_suffix('L').or_else(|| expr.strip_suffix('l'))
            && prefix.chars().all(|c| c.is_ascii_digit())
            && !prefix.is_empty()
        {
            return Some(TypeName::new("long"));
        }
        if let Some(prefix) = expr.strip_suffix('f').or_else(|| expr.strip_suffix('F'))
            && prefix.chars().all(|c| c.is_ascii_digit() || c == '.')
            && !prefix.is_empty()
        {
            return Some(TypeName::new("float"));
        }
        if let Some(prefix) = expr.strip_suffix('d').or_else(|| expr.strip_suffix('D'))
            && prefix.chars().all(|c| c.is_ascii_digit() || c == '.')
            && !prefix.is_empty()
        {
            return Some(TypeName::new("double"));
        }

        // Pure decimal (no suffix)
        if expr.contains('.')
            && expr.chars().all(|c| c.is_ascii_digit() || c == '.')
            && !expr.starts_with('.')
            && !expr.ends_with('.')
        {
            return Some(TypeName::new("double"));
        }

        None
    }

    pub fn resolve_method_return(
        &self,
        receiver_internal: &str,
        method_name: &str,
        arg_count: i32,
        arg_types: &[TypeName],
    ) -> Option<TypeName> {
        tracing::debug!(
            receiver_internal,
            method_name,
            ?arg_types,
            "resolve_method_return"
        );
        let (base_receiver, _receiver_type_args) = split_internal_name(receiver_internal);

        // Use base_receiver to find MRO in the index
        for class in self.view.mro(base_receiver) {
            let candidates: Vec<&MethodSummary> = class
                .methods
                .iter()
                .filter(|m| m.name.as_ref() == method_name)
                .collect();
            if candidates.is_empty() {
                continue;
            }

            let method = self.select_overload(&candidates, arg_count, arg_types)?;
            let sig = method
                .generic_signature
                .clone()
                .unwrap_or_else(|| method.desc());

            let ret_idx = sig.find(')')?;
            let ret_jvm_str = &sig[ret_idx + 1..];
            let (ret_jvm_type, _) = JvmType::parse(&sig[ret_idx + 1..])?;

            if let Some(substituted) = substitute_type(
                receiver_internal,
                class.generic_signature.as_deref(),
                ret_jvm_str,
            ) {
                if substituted.erased_internal() == "void" {
                    return None;
                }
                return Some(substituted);
            }

            if let JvmType::Primitive('V') = ret_jvm_type {
                return None;
            }
            return Some(ret_jvm_type.to_type_name());
        }
        None
    }

    pub fn select_overload<'a>(
        &self,
        candidates: &[&'a MethodSummary],
        arg_count: i32,
        arg_types: &[TypeName],
    ) -> Option<&'a MethodSummary> {
        tracing::debug!(?candidates, ?arg_types, "select_overload");

        if candidates.is_empty() {
            return None;
        }

        if candidates.len() == 1 {
            return Some(candidates[0]);
        }

        let by_count: Vec<&MethodSummary> = candidates
            .iter()
            .copied()
            .filter(|m| m.params.len() == arg_count as usize)
            .collect();

        if by_count.is_empty() {
            tracing::warn!(
                arg_count,
                "no method matches parameter count, falling back to candidates[0]"
            );
            return Some(candidates[0]);
        }

        if by_count.len() == 1 {
            return Some(by_count[0]);
        }

        let mut best_score = -1;
        let mut best_match: Option<&MethodSummary> = None;

        for m in &by_count {
            let score = self.score_params(&m.desc(), arg_types);

            tracing::debug!(
                method = %m.name,
                desc = %m.desc(),
                score = score,
                "evaluating candidate"
            );

            if score > best_score {
                best_score = score;
                best_match = Some(*m);
            }
        }

        match best_match {
            Some(m) if best_score >= 0 => {
                tracing::debug!(selected = %m.desc(), score = best_score, "selected best match");
                Some(m)
            }
            _ => {
                // If all count-matched candidates fail scoring, still prefer a count-matched fallback.
                tracing::warn!(
                    "all overloads failed type scoring, falling back to first count-matched method"
                );
                Some(by_count[0])
            }
        }
    }

    pub fn resolve_chain(
        &self,
        chain: &[ChainSegment],
        locals: &[LocalVar],
        enclosing_internal_name: Option<&Arc<str>>,
    ) -> Option<TypeName> {
        let mut current_type: Option<TypeName> = None;
        for (i, seg) in chain.iter().enumerate() {
            if i == 0 {
                if seg.arg_count.is_some() {
                    // Bare method call: receiver is the enclosing class
                    let recv = enclosing_internal_name?;
                    current_type = self.resolve_method_return(
                        recv.as_ref(),
                        &seg.name,
                        seg.arg_count.unwrap_or(-1),
                        &seg.arg_types,
                    );
                } else {
                    current_type = self.resolve(&seg.name, locals, enclosing_internal_name);
                }
            } else {
                let receiver = current_type.as_ref()?;

                // Handle chained index-only segments like `[0]`.
                if seg.arg_count.is_none() && seg.name.starts_with('[') && seg.name.ends_with(']') {
                    let dimensions = seg.name.matches('[').count();
                    let mut arr_ty = receiver.clone();
                    for _ in 0..dimensions {
                        arr_ty = arr_ty.element_type()?;
                    }
                    current_type = Some(arr_ty);
                    continue;
                }

                // Regular method/field access, possibly with trailing index suffix like `field[0]`.
                let bracket_idx = seg.name.find('[').unwrap_or(seg.name.len());
                let actual_name = &seg.name[..bracket_idx];
                let dimensions = seg.name[bracket_idx..].matches('[').count();

                if seg.arg_count.is_some() {
                    let receiver_internal = receiver.to_internal_with_generics();
                    current_type = self.resolve_method_return(
                        &receiver_internal,
                        actual_name,
                        seg.arg_count.unwrap_or(-1),
                        &seg.arg_types,
                    );
                } else {
                    let mut found_field: Option<TypeName> = None;
                    for class in self.view.mro(receiver.erased_internal()) {
                        if let Some(f) =
                            class.fields.iter().find(|f| f.name.as_ref() == actual_name)
                        {
                            if let Some(ty) = singleton_descriptor_to_type(&f.descriptor) {
                                found_field = Some(TypeName::new(ty));
                            } else {
                                found_field = parse_single_type_to_internal(&f.descriptor);
                            }
                            break;
                        }
                    }
                    current_type = found_field;
                }

                // Apply index suffix dimensional reduction.
                if dimensions > 0
                    && let Some(ty) = current_type
                {
                    let mut arr_ty = ty;
                    for _ in 0..dimensions {
                        arr_ty = arr_ty.element_type()?;
                    }
                    current_type = Some(arr_ty);
                }
            }
        }
        current_type
    }

    pub fn score_params(&self, descriptor: &str, arg_types: &[TypeName]) -> i32 {
        let param_descs = split_param_descriptors(descriptor);

        if param_descs.len() != arg_types.len() {
            tracing::debug!(
                desc_len = param_descs.len(),
                args_len = arg_types.len(),
                "score: -1 (length mismatch)"
            );
            return -1;
        }

        let mut total_score = 0;
        for (i, (desc, arg_ty)) in param_descs.iter().zip(arg_types.iter()).enumerate() {
            let ty_str = arg_ty.erased_internal_with_arrays();
            let score = self.score_single_descriptor(desc, &ty_str);
            if score < 0 {
                tracing::debug!(param_index = i, "score: -1 (param mismatch)");
                return -1;
            }
            total_score += score;
        }

        tracing::debug!(total_score = total_score, "score_params finished");
        total_score
    }

    pub fn score_single_descriptor(&self, desc: &str, ty: &str) -> i32 {
        let span = tracing::debug_span!("score_single", desc = desc, ty = ty);
        let _enter = span.enter();

        // parse array
        if let Some(desc_elem) = desc.strip_prefix('[') {
            if ty.ends_with("[]") {
                let ty_elem = ty.strip_suffix("[]").unwrap().trim();
                tracing::debug!("recursive array match: {} -> {}", desc_elem, ty_elem);
                return self.score_single_descriptor(desc_elem, ty_elem);
            } else {
                tracing::debug!("score: -1 (array vs non-array)");
                return -1;
            }
        }

        // Normalize descriptor form, e.g. `Ljava/lang/Object;` -> `java/lang/Object`.
        let resolved_desc = match desc {
            "B" => "byte",
            "C" => "char",
            "D" => "double",
            "F" => "float",
            "I" => "int",
            "J" => "long",
            "S" => "short",
            "Z" => "boolean",
            "V" => "void",
            _ if desc.starts_with('L') && desc.ends_with(';') => &desc[1..desc.len() - 1],
            _ => desc,
        };

        let normalized_ty = ty.replace('.', "/");
        tracing::debug!(resolved_desc = %resolved_desc, normalized_ty = %normalized_ty, "normalized types");

        let is_primitive = |t: &str| {
            matches!(
                t,
                "byte"
                    | "char"
                    | "double"
                    | "float"
                    | "int"
                    | "long"
                    | "short"
                    | "boolean"
                    | "void"
            )
        };

        // exact match
        if resolved_desc == normalized_ty {
            tracing::debug!("score: 10 (exact match)");
            return 10;
        }

        // wrapper type
        let is_wrapper_match = matches!(
            (resolved_desc, normalized_ty.as_str()),
            ("int", "java/lang/Integer")
                | ("java/lang/Integer", "int")
                | ("boolean", "java/lang/Boolean")
                | ("java/lang/Boolean", "boolean")
                | ("long", "java/lang/Long")
                | ("java/lang/Long", "long")
                | ("double", "java/lang/Double")
                | ("java/lang/Double", "double")
                | ("float", "java/lang/Float")
                | ("java/lang/Float", "float")
                | ("char", "java/lang/Character")
                | ("java/lang/Character", "char")
                | ("byte", "java/lang/Byte")
                | ("java/lang/Byte", "byte")
                | ("short", "java/lang/Short")
                | ("java/lang/Short", "short")
        );

        if is_wrapper_match {
            tracing::debug!("score: 8 (autoboxing/unboxing match)");
            return 8;
        }

        if resolved_desc == "java/lang/Object" {
            if !is_primitive(&normalized_ty) {
                tracing::debug!("score: 5 (reference type matched to Object)");
                return 5;
            } else {
                tracing::debug!("score: -1 (primitive type cannot match Object without boxing)");
                return -1;
            }
        }

        // fallback: simple name match
        let s1 = resolved_desc.rsplit('/').next().unwrap_or(resolved_desc);
        let s2 = normalized_ty.rsplit('/').next().unwrap_or(&normalized_ty);
        if s1 == s2 {
            tracing::debug!(s1 = %s1, s2 = %s2, "score: 10 (simple name match)");
            return 10;
        }

        tracing::debug!("score: -1 (no match found)");
        -1
    }
}

pub fn descriptor_to_source_type(desc: &str, provider: &impl SymbolProvider) -> Option<String> {
    let mut array_depth = 0;
    let mut s = desc;
    while s.starts_with('[') {
        array_depth += 1;
        s = &s[1..];
    }

    let base_type = match s {
        "B" => "byte".to_string(),
        "C" => "char".to_string(),
        "D" => "double".to_string(),
        "F" => "float".to_string(),
        "I" => "int".to_string(),
        "J" => "long".to_string(),
        "S" => "short".to_string(),
        "Z" => "boolean".to_string(),
        "V" => "void".to_string(),
        _ if s.starts_with('L') && s.ends_with(';') => {
            let internal = &s[1..s.len() - 1];
            provider.resolve_source_name(internal)?
        }
        _ => return None, // Invalid descriptor shape.
    };

    let mut result = String::with_capacity(base_type.len() + array_depth * 2);
    result.push_str(&base_type);
    for _ in 0..array_depth {
        result.push_str("[]");
    }
    Some(result)
}

/// Return: Option<(parameters, return_type)>
pub fn parse_strict_method_signature(
    descriptor: &str,
    provider: &impl SymbolProvider,
) -> Option<(Vec<String>, String)> {
    let l_paren = descriptor.find('(')?;
    let r_paren = descriptor.find(')')?;

    let params_str = &descriptor[l_paren + 1..r_paren];
    let return_str = &descriptor[r_paren + 1..];

    let mut params = Vec::new();
    let mut s = params_str;
    while !s.is_empty() {
        let (one_desc, rest) = consume_one_descriptor_type(s);
        if one_desc.is_empty() {
            break;
        }
        // Fail fast if any parameter cannot be resolved.
        let resolved_param = descriptor_to_source_type(one_desc, provider)?;
        params.push(resolved_param);
        s = rest;
    }

    // Fail fast if return type cannot be resolved.
    let resolved_return = descriptor_to_source_type(return_str, provider)?;

    Some((params, resolved_return))
}

/// Converts a singleton descriptor to an internal type name
pub(crate) fn singleton_descriptor_to_type(desc: &str) -> Option<&str> {
    match desc {
        "B" => Some("byte"),
        "C" => Some("char"),
        "D" => Some("double"),
        "F" => Some("float"),
        "I" => Some("int"),
        "J" => Some("long"),
        "S" => Some("short"),
        "Z" => Some("boolean"),
        "V" => Some("void"),
        _ if desc.starts_with('L') && desc.ends_with(';') => Some(&desc[1..desc.len() - 1]),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct ChainSegment {
    /// Variable name or method name
    pub name: String,
    /// If it's a method call, specify the number of arguments; if it's a field/variable, specify None.
    pub arg_count: Option<i32>,
    /// Inferred types of arguments (internal names), used for overload resolution.
    pub arg_types: Vec<TypeName>,
    pub arg_texts: Vec<String>, // raw text of each argument
}

impl ChainSegment {
    pub fn variable(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            arg_count: None,
            arg_types: vec![],
            arg_texts: vec![],
        }
    }
    pub fn method(name: impl Into<String>, arg_count: i32) -> Self {
        Self {
            name: name.into(),
            arg_count: Some(arg_count),
            arg_types: vec![],
            arg_texts: vec![],
        }
    }
    pub fn method_with_types(
        name: impl Into<String>,
        arg_count: i32,
        arg_types: Vec<TypeName>,
        arg_texts: Vec<String>,
    ) -> Self {
        Self {
            name: name.into(),
            arg_count: Some(arg_count),
            arg_types,
            arg_texts,
        }
    }
}

/// Extract the internal name of the return type from the method descriptor or generic signature
///
// Input example:
/// - `"(I)Ljava/util/List;"` → `Some("java/util/List")`
/// - `"(I)Ljava/util/List<Ljava/lang/String;>;"` → `Some("java/util/List")` (after generic erasure)
/// - `"()V"` → `None` (void)
/// - `"()I"` → `None` (primitive type, no further dot chaining)
/// - `"()[Ljava/lang/String;"` → `Some("[Ljava/lang/String;")` (array type)
pub fn extract_return_internal_name(descriptor: &str) -> Option<JvmType> {
    let ret_idx = descriptor.find(')')?;
    let ret_str = &descriptor[ret_idx + 1..];

    let (jvm_type, _) = JvmType::parse(ret_str)?;

    if let JvmType::Primitive('V') = jvm_type {
        return None;
    }

    Some(jvm_type)
}

/// Convert a single type descriptor to an internal name (remove generic parameters, retain the primitive type)
///
/// - `"Ljava/util/List;"` → `Some("java/util/List")`
/// - `"Ljava/util/List<Ljava/lang/String;>;"` → `Some("java/util/List")`
/// - `"[Ljava/lang/String;"` → `Some("[Ljava/lang/String;")` (Arrays retain complete descriptors)
/// - Primitive types such as `"V"` / `"I"` / `"Z"` → `None`
pub fn parse_single_type_to_internal(ty: &str) -> Option<TypeName> {
    let (jvm_type, _) = JvmType::parse(ty)?;
    match jvm_type {
        JvmType::Primitive(_) => None,
        _ => Some(jvm_type.to_type_name()),
    }
}

/// Counts the number of parameters in the method descriptor
///
/// `"(ILjava/lang/String;[B)V"` → 3
pub fn count_params(descriptor: &str) -> usize {
    split_param_descriptors(descriptor).len()
}

/// Extract return type from descriptor
pub fn parse_return_type_from_descriptor(descriptor: &str) -> Option<Arc<str>> {
    extract_return_internal_name(descriptor).map(|t| t.to_signature_string().into())
}

/// Convert Java source type text to JVM internal form, preserving generic structure.
///
/// # Examples
/// - `List<String>` -> `java/util/List<Ljava/lang/String;>`
/// - `Map<String, List<User>>` -> `java/util/Map<Ljava/lang/String;Ljava/util/List<Luser/User;>;>`
/// - `String[]` -> `[Ljava/lang/String;`
pub fn java_source_type_to_jvm_generic(
    source_ty: &str,
    resolve_simple_name: &impl Fn(&str) -> String,
    type_params: &HashSet<&str>,
) -> String {
    let mut ty = source_ty.trim();

    if type_params.contains(ty) {
        return format!("T{};", ty);
    }

    // 1. Parse array suffixes (String[] -> array_depth = 1).
    let mut array_depth = 0;
    while let Some(stripped) = ty.strip_suffix("[]") {
        array_depth += 1;
        ty = stripped.trim();
    }

    // 2. Parse generic arguments.
    let mut result = if let Some(pos) = ty.find('<') {
        if ty.ends_with('>') {
            let base = &ty[..pos];
            let args_str = &ty[pos + 1..ty.len() - 1];
            // Resolve base class name, e.g. List -> java/util/List.
            let base_internal = resolve_simple_name(base.trim());

            // Split generic args by top-level commas only.
            let mut args = Vec::new();
            let mut depth = 0;
            let mut start = 0;
            for (i, c) in args_str.char_indices() {
                match c {
                    '<' => depth += 1,
                    '>' => depth -= 1,
                    ',' if depth == 0 => {
                        args.push(&args_str[start..i]);
                        start = i + 1;
                    }
                    _ => {}
                }
            }
            args.push(&args_str[start..]);

            // Convert nested args recursively and wrap object args as `L...;`.
            let resolved_args: Vec<_> = args
                .into_iter()
                .map(|a| {
                    let arg_ty = a.trim();
                    if arg_ty == "?" {
                        return "*".to_string(); // Wildcard.
                    }
                    let inner =
                        java_source_type_to_jvm_generic(arg_ty, resolve_simple_name, type_params);

                    // Do not wrap arrays with `L...;`.
                    if inner.starts_with('[') {
                        inner
                    } else {
                        format!("L{};", inner)
                    }
                })
                .collect();

            format!("{}<{}>", base_internal, resolved_args.join(""))
        } else {
            resolve_simple_name(ty)
        }
    } else {
        resolve_simple_name(ty)
    };

    // 3. Re-apply array wrappers.
    for _ in 0..array_depth {
        if !result.starts_with('[') {
            result = format!("L{};", result);
        }
        result = format!("[{}", result);
    }

    result
}

/// Resolver that combines global index data with file-local context.
/// Follows strict JLS visibility rules and avoids heuristic guessing.
pub struct ContextualResolver<'a> {
    pub view: &'a IndexView,
    pub ctx: &'a SemanticContext,
}

impl<'a> ContextualResolver<'a> {
    pub fn new(view: &'a IndexView, ctx: &'a SemanticContext) -> Self {
        Self { view, ctx }
    }
}

impl<'a> SymbolProvider for ContextualResolver<'a> {
    fn resolve_source_name(&self, type_name: &str) -> Option<String> {
        // 1) Internal names from bytecode (contains `/`) go directly to index lookup.
        if type_name.contains('/') {
            return self.view.get_source_type_name(type_name);
        }

        // 2) Otherwise treat as a simple source name from AST.
        let simple_name = type_name;

        // Rule A: exact imports, e.g. `import java.util.List;`.
        for imp in &self.ctx.existing_imports {
            // Must not be wildcard import; match class name exactly.
            if !imp.ends_with(".*")
                && (imp.as_ref() == simple_name || imp.ends_with(&format!(".{}", simple_name)))
            {
                let internal = imp.replace('.', "/");
                if let Some(source_name) = self.view.get_source_type_name(&internal) {
                    return Some(source_name);
                }
            }
        }

        // Rule B: same-package visibility.
        if let Some(pkg) = self.ctx.effective_package() {
            let internal = format!("{}/{}", pkg.replace('.', "/"), simple_name);
            if let Some(source_name) = self.view.get_source_type_name(&internal) {
                return Some(source_name);
            }
        }

        // Rule C: implicit `java.lang.*`.
        let lang_internal = format!("java/lang/{}", simple_name);
        if let Some(source_name) = self.view.get_source_type_name(&lang_internal) {
            return Some(source_name);
        }

        // Rule D: wildcard imports, e.g. `import java.util.*`.
        for imp in &self.ctx.existing_imports {
            if imp.ends_with(".*") {
                let pkg = imp.trim_end_matches(".*").replace('.', "/");
                let internal = format!("{}/{}", pkg, simple_name);
                if let Some(source_name) = self.view.get_source_type_name(&internal) {
                    return Some(source_name);
                }
            }
        }

        // No strict rule matched.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion::parser::parse_chain_from_expr,
        index::{IndexScope, IndexView, MethodParams, ModuleId, WorkspaceIndex},
    };

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_resolver() -> (IndexView, Vec<LocalVar>) {
        let idx = WorkspaceIndex::new();
        let locals = vec![
            LocalVar {
                name: Arc::from("cl"),
                type_internal: TypeName::new("RandomClass"),
                init_expr: None,
            },
            LocalVar {
                name: Arc::from("sf"),
                type_internal: TypeName::new("float"),
                init_expr: None,
            },
            LocalVar {
                name: Arc::from("result"),
                type_internal: TypeName::new("java/lang/String"),
                init_expr: None,
            },
            LocalVar {
                name: Arc::from("myList"),
                type_internal: TypeName::new("java/util/List"),
                init_expr: None,
            },
        ];
        (idx.view(root_scope()), locals)
    }

    #[test]
    fn test_variable_ending_with_l_not_confused_with_long() {
        let (view, locals) = make_resolver();
        let r = TypeResolver::new(&view);
        // "cl" ends with 'l', but it's a local variable and shouldn't be evaluated as long.
        assert_eq!(
            r.resolve("cl", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("RandomClass"),
            "'cl' should resolve to RandomClass, not long"
        );
    }

    #[test]
    fn test_variable_ending_with_f_not_confused_with_float() {
        let (view, locals) = make_resolver();
        let r = TypeResolver::new(&view);
        // "sf" ends with 'f', but it's a local variable
        assert_eq!(
            r.resolve("sf", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("float"), // float is the variable's type, not a literal value.
            "'sf' should resolve to its declared type"
        );
    }

    #[test]
    fn test_long_literal_recognized() {
        let (view, locals) = make_resolver();
        let r = TypeResolver::new(&view);
        assert_eq!(
            r.resolve("123L", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("long")
        );
        assert_eq!(
            r.resolve("0l", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("long")
        );
        assert_eq!(
            r.resolve("999L", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("long")
        );
    }

    #[test]
    fn test_float_literal_recognized() {
        let (view, locals) = make_resolver();
        let r = TypeResolver::new(&view);
        assert_eq!(
            r.resolve("1.5f", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("float")
        );
        assert_eq!(
            r.resolve("3F", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("float")
        );
    }

    #[test]
    fn test_double_literal_recognized() {
        let (view, locals) = make_resolver();
        let r = TypeResolver::new(&view);
        assert_eq!(
            r.resolve("1.5d", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("double")
        );
        assert_eq!(
            r.resolve("3.14", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("double")
        );
    }

    #[test]
    fn test_int_literal_recognized() {
        let (view, locals) = make_resolver();
        let r = TypeResolver::new(&view);
        assert_eq!(
            r.resolve("42", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("int")
        );
        assert_eq!(
            r.resolve("0", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("int")
        );
    }

    #[test]
    fn test_string_literal_recognized() {
        let (view, locals) = make_resolver();
        let r = TypeResolver::new(&view);
        assert_eq!(
            r.resolve("\"hello\"", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("java/lang/String")
        );
    }

    #[test]
    fn test_this_resolves_to_enclosing() {
        let (view, locals) = make_resolver();
        let r = TypeResolver::new(&view);
        let enclosing = Arc::from("org/cubewhy/Main");
        assert_eq!(
            r.resolve("this", &locals, Some(&enclosing))
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("org/cubewhy/Main")
        );
    }

    #[test]
    fn test_unknown_expr_returns_none() {
        let (view, locals) = make_resolver();
        let r = TypeResolver::new(&view);
        assert_eq!(r.resolve("unknownVar", &locals, None), None);
    }

    #[test]
    fn test_local_var_takes_priority_over_literal_heuristic() {
        // Even if the variable name looks like a literal, local variable lookup takes precedence.
        let idx = WorkspaceIndex::new();
        let locals = vec![
            // Extreme case: The variable name is "123" (invalid in Java, but with test priority)
            LocalVar {
                name: Arc::from("myL"),
                type_internal: TypeName::new("SomeClass"),
                init_expr: None,
            },
        ];
        let view = idx.view(root_scope());
        let r = TypeResolver::new(&view);
        // "myL" ends with 'L' but is not a numeric prefix, and should not be recognized as long.
        assert_eq!(
            r.resolve("myL", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("SomeClass")
        );
    }

    #[test]
    fn test_resolve_method_return_overload_by_type_long() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: None,
            name: Arc::from("NestedClass"),
            internal_name: Arc::from("NestedClass"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                MethodSummary {
                    name: Arc::from("randomFunction"),
                    params: MethodParams::from_method_descriptor(
                        "(Ljava/lang/String;I)LRandomClass;",
                    ),
                    access_flags: ACC_PUBLIC,
                    annotations: vec![],
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("LRandomClass;")),
                },
                MethodSummary {
                    name: Arc::from("randomFunction"),
                    params: MethodParams::from_method_descriptor("(Ljava/lang/String;J)LMain2;"),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("LMain2;")),
                },
            ],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);

        // arg_types: String + long → should match (String, J) → Main2
        let result = resolver.resolve_method_return(
            "NestedClass",
            "randomFunction",
            2,
            &[TypeName::new("java/lang/String"), TypeName::new("long")],
        );
        assert_eq!(
            result.as_ref().map(|t| t.erased_internal()),
            Some("Main2"),
            "long arg should select Main2 overload"
        );

        // arg_types: String + int → RandomClass
        let result2 = resolver.resolve_method_return(
            "NestedClass",
            "randomFunction",
            2,
            &[TypeName::new("java/lang/String"), TypeName::new("int")],
        );
        assert_eq!(
            result2.as_ref().map(|t| t.erased_internal()),
            Some("RandomClass"),
            "int arg should select RandomClass overload"
        );
    }

    #[test]
    fn test_resolve_chain_bare_method_call() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        // Main has getMain2() returning Main2
        // Main2 has func()
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Main"),
                internal_name: Arc::from("Main"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("getMain2"),
                    params: MethodParams::empty(),
                    access_flags: ACC_PUBLIC,
                    annotations: vec![],
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("LMain2;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: None,
                name: Arc::from("Main2"),
                internal_name: Arc::from("Main2"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("func"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: None,
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let enclosing = Arc::from("Main");

        // "getMain2()" → chain = [method("getMain2", 0)]
        let chain = parse_chain_from_expr("getMain2()");
        let result = resolver.resolve_chain(&chain, &[], Some(&enclosing));
        assert_eq!(
            result.as_ref().map(|t| t.erased_internal()),
            Some("Main2"),
            "bare method call should resolve via enclosing class"
        );
    }

    #[test]
    fn test_generics_substitution_list_get() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/util")),
            name: Arc::from("List"),
            internal_name: Arc::from("java/util/List"),
            super_name: None,
            annotations: vec![],
            interfaces: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("get"),
                params: MethodParams::from([("I", "arg0")]),
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                annotations: vec![],
                // Generic method return type is `E`.
                generic_signature: Some(Arc::from("(I)TE;")),
                return_type: Some(Arc::from("Ljava/lang/Object;")),
            }],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            inner_class_of: None,
            generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
            origin: ClassOrigin::Unknown,
        }]);

        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);

        // Simulate resolving `myList.get()` with a generic receiver type.
        let result = resolver.resolve_method_return(
            "java/util/List<Ljava/lang/String;>",
            "get",
            1,
            &[TypeName::new("int")],
        );

        assert_eq!(
            result.as_ref().map(|t| t.erased_internal()),
            Some("java/lang/String"),
            "Generic type TE; should be correctly substituted to java/lang/String"
        );
    }

    #[test]
    fn test_resolve_variable_array_access() {
        // Simulate `var c = arr[0];`.
        let (view, locals) = make_resolver();
        let mut locals = locals;
        locals.push(LocalVar {
            name: Arc::from("arr"),
            type_internal: TypeName::new("char[]"),
            init_expr: None,
        });

        let resolver = TypeResolver::new(&view);
        let result = resolver.resolve("arr[0]", &locals, None);
        assert_eq!(result.as_ref().map(|t| t.erased_internal()), Some("char"));
    }

    #[test]
    fn test_resolve_array_after_method_call() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: None,
            name: Arc::from("Main"),
            internal_name: Arc::from("Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("getCharArr"),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: Some(Arc::from("[[C")),
            }],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let enclosing = Arc::from("Main");

        // Simulate chained access `getCharArr()[0]`.
        let chain = parse_chain_from_expr("getCharArr()[0]");
        let result = resolver.resolve_chain(&chain, &[], Some(&enclosing));

        // One index on char[][] yields char[].
        assert_eq!(
            result.as_ref().map(|t| t.erased_internal_with_arrays()),
            Some("char[]".to_string())
        );
    }

    #[test]
    fn test_scoring_system_primitive_match() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        assert_eq!(resolver.score_single_descriptor("I", "int"), 10);
        assert_eq!(resolver.score_single_descriptor("I", "long"), -1);
    }

    #[test]
    fn test_overload_resolution_prioritizes_specific_over_object() {
        use crate::index::MethodSummary;
        use rust_asm::constants::ACC_PUBLIC;

        let method_object = MethodSummary {
            name: Arc::from("println"),
            params: MethodParams::from_method_descriptor("(Ljava/lang/Object;)V"),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };

        let method_string = MethodSummary {
            name: Arc::from("println"),
            params: MethodParams::from_method_descriptor("(Ljava/lang/String;)V"),
            access_flags: ACC_PUBLIC,
            annotations: vec![],
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };

        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let candidates = vec![&method_object, &method_string];

        // String-specific overload should beat Object overload.
        let args = vec![TypeName::new("java/lang/String")];
        let best = resolver.select_overload(&candidates, 1, &args);

        assert_eq!(best.unwrap().desc().as_ref(), "(Ljava/lang/String;)V");
    }

    #[test]
    fn test_boolean_literal_recognized() {
        let (view, locals) = make_resolver();
        let r = TypeResolver::new(&view);
        assert_eq!(
            r.resolve("true", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("boolean")
        );
        assert_eq!(
            r.resolve("false", &locals, None)
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("boolean")
        );
    }

    #[test]
    fn test_scoring_system_autoboxing() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);

        // Primitive expected, wrapper provided (Unboxing)
        assert_eq!(
            resolver.score_single_descriptor("I", "java/lang/Integer"),
            8
        );
        assert_eq!(
            resolver.score_single_descriptor("Z", "java/lang/Boolean"),
            8
        );

        // Wrapper expected, primitive provided (Autoboxing)
        assert_eq!(
            resolver.score_single_descriptor("Ljava/lang/Integer;", "int"),
            8
        );
        assert_eq!(
            resolver.score_single_descriptor("Ljava/lang/Boolean;", "boolean"),
            8
        );
    }

    #[test]
    fn test_overload_resolution_prefers_exact_over_autoboxing() {
        use crate::index::MethodSummary;
        use rust_asm::constants::ACC_PUBLIC;

        let method_wrapper = MethodSummary {
            name: Arc::from("process"),
            params: MethodParams::from_method_descriptor("(Ljava/lang/Integer;)V"),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };

        let method_primitive = MethodSummary {
            name: Arc::from("process"),
            params: MethodParams::from([("I", "i")]),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };

        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let candidates = vec![&method_wrapper, &method_primitive];

        // Primitive input should prefer primitive overload.
        let args_prim = vec![TypeName::new("int")];
        let best_prim = resolver.select_overload(&candidates, 1, &args_prim);
        assert_eq!(best_prim.unwrap().desc().as_ref(), "(I)V");

        // Wrapper input should prefer wrapper overload.
        let args_wrapper = vec![TypeName::new("java/lang/Integer")];
        let best_wrapper = resolver.select_overload(&candidates, 1, &args_wrapper);
        assert_eq!(
            best_wrapper.unwrap().desc().as_ref(),
            "(Ljava/lang/Integer;)V"
        );
    }

    #[test]
    fn test_java_source_type_to_jvm_generic_typevar() {
        use std::collections::HashSet;
        let mut params = HashSet::new();
        params.insert("E");

        let resolve = |s: &str| s.to_string();
        let got = java_source_type_to_jvm_generic("E", &resolve, &params);
        assert_eq!(got, "TE;");
    }
}
