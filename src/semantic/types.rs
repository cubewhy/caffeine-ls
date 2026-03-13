use self::generics::{
    JvmType, parse_class_type_parameters, parse_method_signature_types,
    parse_method_type_parameters, split_internal_name, substitute_type, substitute_type_vars,
};
use self::type_name::TypeName;
use super::context::{LocalVar, SemanticContext};
use crate::{
    index::{IndexView, MethodSummary},
    jvm::descriptor::{consume_one_descriptor_type, split_param_descriptors},
};
use rust_asm::constants::{ACC_ABSTRACT, ACC_STATIC, ACC_VARARGS};
use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionKind {
    Identity,
    WideningPrimitive,
    NarrowingPrimitive,
    Boxing,
    Unboxing,
    BoxingWideningPrimitive,
    UnboxingWideningPrimitive,
    ReferenceWidening,
    NullToReference,
    Unknown,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeCompatibility {
    pub kind: ConversionKind,
    pub score: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverloadInvocationMode {
    Fixed,
    Varargs,
}

#[derive(Debug, Clone, Copy)]
pub struct OverloadMatch<'a> {
    pub method: &'a MethodSummary,
    pub mode: OverloadInvocationMode,
    pub score: i32,
}

type QualifierResolver<'a> = &'a dyn Fn(&str) -> Option<TypeName>;

#[derive(Debug, Clone, Copy, Default)]
pub struct CallArgs<'a> {
    pub count: usize,
    pub types: &'a [TypeName],
    pub texts: &'a [String],
}

impl<'a> CallArgs<'a> {
    pub fn new(count: usize, types: &'a [TypeName], texts: &'a [String]) -> Self {
        Self {
            count,
            types,
            texts,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct EvalContext<'a> {
    pub locals: &'a [LocalVar],
    pub enclosing: Option<&'a Arc<str>>,
    pub qualifier_resolver: Option<QualifierResolver<'a>>,
}

impl<'a> EvalContext<'a> {
    pub fn new(locals: &'a [LocalVar], enclosing: Option<&'a Arc<str>>) -> Self {
        Self {
            locals,
            enclosing,
            qualifier_resolver: None,
        }
    }

    pub fn with_qualifier(mut self, qr: Option<QualifierResolver<'a>>) -> Self {
        self.qualifier_resolver = qr;
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedMethodTarget<'a> {
    pub receiver_internal: &'a str,
    pub owner_internal: &'a str,
    pub summary: &'a MethodSummary,
    pub class_generic_signature: Option<&'a str>,
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
                    let mut resolved = None;
                    if raw_ty.contains('.') {
                        let resolve_head = |head: &str| {
                            if head.contains('.') {
                                let internal = head.replace('.', "/");
                                self.view
                                    .get_class(&internal)
                                    .map(|c| c.internal_name.clone())
                            } else if let Some(enclosing_internal) = enclosing {
                                self.view
                                    .resolve_scoped_inner_class(enclosing_internal, head)
                                    .map(|c| c.internal_name.clone())
                            } else {
                                None
                            }
                        };
                        resolved = self
                            .view
                            .resolve_qualified_type_path(raw_ty, &resolve_head)
                            .map(|c| c.internal_name.clone());
                    } else if let Some(enclosing_internal) = enclosing {
                        resolved = self
                            .view
                            .resolve_scoped_inner_class(enclosing_internal, raw_ty)
                            .map(|c| c.internal_name.clone());
                    }

                    match resolved {
                        Some(internal) => TypeName::new(internal.as_ref()),
                        None => TypeName::new(raw_ty), // unresolved source-like type; may be expanded later
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
        arg_count: usize,
        arg_types: &[TypeName],
    ) -> Option<TypeName> {
        let args = CallArgs::new(arg_count, arg_types, &[]);
        self.resolve_method_return_with_callsite_and_qualifier_resolver(
            receiver_internal,
            method_name,
            args,
            EvalContext::default(),
        )
    }

    pub fn resolve_method_return_with_callsite_and_qualifier_resolver(
        &self,
        receiver_internal: &str,
        method_name: &str,
        args: CallArgs,
        ctx: EvalContext,
    ) -> Option<TypeName> {
        let CallArgs {
            types: arg_types,
            count: arg_count,
            texts: arg_texts,
        } = args;

        tracing::debug!(
            receiver_internal,
            method_name,
            ?arg_types,
            ?arg_texts,
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

            let selected = self.select_overload_match(&candidates, arg_count, arg_types)?;
            return self.resolve_selected_method_return_with_callsite_and_qualifier_resolver(
                receiver_internal,
                selected.method,
                class.internal_name.as_ref(),
                class.generic_signature.as_deref(),
                args,
                ctx,
            );
        }
        None
    }

    pub fn resolve_selected_method_return_with_callsite_and_qualifier_resolver(
        &self,
        receiver_internal: &str,
        method: &MethodSummary,
        owner_internal: &str,
        class_generic_signature: Option<&str>,
        args: CallArgs,
        ctx: EvalContext,
    ) -> Option<TypeName> {
        let sig = method
            .generic_signature
            .clone()
            .unwrap_or_else(|| method.desc());

        let (param_jvm_types, ret_jvm_type) = parse_method_signature_types(&sig)?;
        let mut resolved_ret = ret_jvm_type.clone();

        let method_bindings = self.infer_method_type_bindings_shallow(
            method,
            &sig,
            &param_jvm_types,
            &ret_jvm_type,
            receiver_internal,
            class_generic_signature,
            args,
            ctx,
        );
        if !method_bindings.is_empty() {
            resolved_ret = substitute_type_vars(&resolved_ret, &method_bindings);
        }

        let ret_jvm_str = resolved_ret.to_signature_string();

        if let Some(substituted) =
            substitute_type(receiver_internal, class_generic_signature, &ret_jvm_str)
        {
            let substituted = self.canonicalize_type_in_owner_scope(substituted, owner_internal);
            if substituted.erased_internal() == "void" {
                return None;
            }
            return Some(substituted);
        }

        if let JvmType::Primitive('V') = resolved_ret {
            return None;
        }
        Some(self.canonicalize_type_in_owner_scope(resolved_ret.to_type_name(), owner_internal))
    }

    fn canonicalize_type_in_owner_scope(&self, ty: TypeName, owner_internal: &str) -> TypeName {
        let canonical_args: Vec<TypeName> = ty
            .args
            .into_iter()
            .map(|a| self.canonicalize_type_in_owner_scope(a, owner_internal))
            .collect();
        let mut ty = TypeName {
            base_internal: ty.base_internal,
            args: canonical_args,
            array_dims: ty.array_dims,
        };

        if ty.contains_slash()
            || matches!(ty.base_internal.as_ref(), "+" | "-" | "?" | "*" | "capture")
        {
            return ty;
        }

        let base = ty.erased_internal();
        if base.is_empty() {
            return ty;
        }

        // Type variables / placeholders should remain as-is.
        if base.chars().all(|c| c.is_ascii_uppercase()) {
            return ty;
        }

        let owner_simple = owner_internal
            .rsplit('/')
            .next()
            .unwrap_or(owner_internal)
            .rsplit('$')
            .next()
            .unwrap_or(owner_internal);
        if base == owner_simple {
            ty.base_internal = Arc::from(owner_internal);
            return ty;
        }

        if let Some(dollar_idx) = owner_internal.rfind('$') {
            let outer = &owner_internal[..dollar_idx];
            let candidate = format!("{outer}${base}");
            if self.view.get_class(&candidate).is_some() {
                ty.base_internal = Arc::from(candidate);
                return ty;
            }
        }

        if let Some(last_slash) = owner_internal.rfind('/') {
            let pkg = &owner_internal[..last_slash];
            let candidate = format!("{pkg}/{base}");
            if self.view.get_class(&candidate).is_some() {
                ty.base_internal = Arc::from(candidate);
                return ty;
            }
        }

        let java_lang_candidate = format!("java/lang/{base}");
        if self.view.get_class(&java_lang_candidate).is_some() {
            ty.base_internal = Arc::from(java_lang_candidate);
            return ty;
        }

        let globals = self.view.get_classes_by_simple_name(base);
        if globals.len() == 1 {
            ty.base_internal = Arc::clone(&globals[0].internal_name);
            return ty;
        }

        ty
    }

    #[allow(clippy::too_many_arguments)]
    fn infer_method_type_bindings_shallow(
        &self,
        selected: &MethodSummary,
        method_signature: &str,
        param_jvm_types: &[JvmType],
        ret_jvm_type: &JvmType,
        receiver_internal: &str,
        class_generic_signature: Option<&str>,
        args: CallArgs,
        ctx: EvalContext,
    ) -> HashMap<String, JvmType> {
        let CallArgs {
            types: arg_types,
            texts: arg_texts,
            ..
        } = args;
        let EvalContext {
            enclosing,
            locals,
            qualifier_resolver,
        } = ctx;

        let method_type_params = parse_method_type_parameters(method_signature);
        if method_type_params.is_empty() {
            return HashMap::new();
        }

        let mut return_type_vars = HashSet::new();
        collect_type_vars(ret_jvm_type, &mut return_type_vars);
        if return_type_vars.is_empty() {
            return HashMap::new();
        }

        let class_type_params = class_generic_signature
            .map(parse_class_type_parameters)
            .unwrap_or_default();
        let (_, receiver_type_args) = split_internal_name(receiver_internal);
        let mut bindings = self.infer_method_type_bindings_from_non_lambda_args(
            method_signature,
            param_jvm_types,
            receiver_internal,
            selected,
            usize::MAX,
            OverloadInvocationMode::Fixed,
            args,
        );

        let mut conflicted = HashSet::new();

        for (idx, param_ty) in param_jvm_types.iter().enumerate() {
            let param_ty_substituted =
                if !class_type_params.is_empty() && !receiver_type_args.is_empty() {
                    param_ty.substitute(&class_type_params, &receiver_type_args)
                } else {
                    param_ty.clone()
                };
            let param_ty_substituted = if !bindings.is_empty() {
                substitute_type_vars(&param_ty_substituted, &bindings)
            } else {
                param_ty_substituted
            };
            let arg_ty = arg_types.get(idx);
            if let JvmType::TypeVar(name) = &param_ty_substituted
                && return_type_vars.contains(name)
            {
                if let Some(arg_ty) = arg_ty
                    && let Some(jvm) = type_name_to_jvm_type(arg_ty)
                {
                    bind_type_var(name, jvm, &mut bindings, &mut conflicted);
                }
                continue;
            }

            self.bind_return_type_var_from_functional_param(
                selected.name.as_ref(),
                idx,
                &param_ty_substituted,
                arg_texts.get(idx).map(|s| s.as_str()),
                arg_ty,
                EvalContext::new(locals, enclosing).with_qualifier(qualifier_resolver),
                &return_type_vars,
                &mut bindings,
                &mut conflicted,
            );
        }

        for key in conflicted {
            bindings.remove(&key);
        }
        bindings
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_return_type_var_from_functional_param(
        &self,
        _method_name: &str,
        _arg_index: usize,
        param_ty: &JvmType,
        arg_text: Option<&str>,
        arg_ty: Option<&TypeName>,
        ctx: EvalContext,
        return_type_vars: &HashSet<String>,
        bindings: &mut HashMap<String, JvmType>,
        conflicted: &mut HashSet<String>,
    ) {
        let JvmType::Object(_, type_args) = param_ty else {
            return;
        };

        // Prefer `? extends R` / covariant slot when present.
        let covariant_ret_var = type_args.iter().find_map(|arg| match arg {
            JvmType::WildcardBound('+', inner) => match inner.as_ref() {
                JvmType::TypeVar(name) if return_type_vars.contains(name) => Some(name.clone()),
                _ => None,
            },
            JvmType::TypeVar(name) if return_type_vars.contains(name) => Some(name.clone()),
            _ => None,
        });
        let Some(ret_var) = covariant_ret_var else {
            return;
        };

        let inferred = arg_ty
            .filter(|ty| ty.erased_internal() != "unknown")
            .and_then(type_name_to_jvm_type)
            .or_else(|| {
                arg_text.and_then(|txt| {
                    self.infer_functional_arg_return_shallow(txt, ctx, Some(param_ty))
                })
            });
        if let Some(jvm) = inferred {
            bind_type_var(&ret_var, jvm, bindings, conflicted);
        }
    }

    fn infer_functional_arg_return_shallow(
        &self,
        arg_text: &str,
        ctx: EvalContext,
        target_param_ty: Option<&JvmType>,
    ) -> Option<JvmType> {
        let EvalContext {
            enclosing,
            locals,
            qualifier_resolver,
        } = ctx;

        let text = arg_text.trim();
        if text.is_empty() {
            return None;
        }

        if let Some((qualifier, member, is_constructor)) = parse_method_reference_text(text) {
            if is_constructor {
                let owner =
                    self.resolve_owner_from_text_with_context(qualifier, qualifier_resolver)?;
                let specialized = target_param_ty
                    .and_then(extract_functional_input_type)
                    .filter(is_concrete_jvm_type)
                    .and_then(|input| self.specialize_constructor_owner_return(&owner, input));
                return Some(specialized.unwrap_or_else(|| JvmType::Object(owner, vec![])));
            }

            if is_likely_type_qualifier(qualifier) {
                let owner =
                    self.resolve_owner_from_text_with_context(qualifier, qualifier_resolver)?;
                let ret = self.resolve_method_reference_return_on_owner(&owner, member)?;
                return Some(ret);
            }

            let owner_ty = self.resolve(qualifier, locals, enclosing)?;
            let owner = if owner_ty.contains_slash() {
                owner_ty.erased_internal().to_string()
            } else {
                self.resolve_owner_from_text(owner_ty.erased_internal())?
            };
            let ret = self.resolve_method_reference_return_on_owner(&owner, member)?;
            return Some(ret);
        }

        if let Some(body) = parse_lambda_expression_body(text) {
            let lambda_locals = self
                .build_lambda_param_locals_from_target(text, target_param_ty)
                .unwrap_or_default();
            let mut merged_locals = lambda_locals;
            merged_locals.extend_from_slice(locals);
            let chain = crate::completion::parser::parse_chain_from_expr(body);
            let body_ty = self.resolve(body, &merged_locals, enclosing).or_else(|| {
                if chain.is_empty() {
                    None
                } else {
                    self.resolve_chain(&chain, &merged_locals, enclosing)
                }
            })?;
            let body_ty = self.canonicalize_type_in_owner_scope(
                body_ty,
                enclosing.map(|e| e.as_ref()).unwrap_or(""),
            );
            let body_jvm = type_name_to_jvm_type(&body_ty)?;
            return Some(box_primitive_jvm_type(body_jvm));
        }

        None
    }

    fn build_lambda_param_locals_from_target(
        &self,
        lambda_text: &str,
        target_param_ty: Option<&JvmType>,
    ) -> Option<Vec<LocalVar>> {
        let target_param_ty = target_param_ty?;
        let (param_names, _) = parse_lambda_text_parts(lambda_text)?;
        let (sam_params, _) =
            self.extract_sam_signature_from_functional_jvm_type(target_param_ty)?;
        if param_names.len() != sam_params.len() {
            return None;
        }
        let mut out = Vec::with_capacity(param_names.len());
        for (name, ty) in param_names.into_iter().zip(sam_params.into_iter()) {
            out.push(LocalVar {
                name: Arc::from(name),
                type_internal: ty.to_type_name(),
                init_expr: None,
            });
        }
        Some(out)
    }

    fn extract_sam_signature_from_functional_jvm_type(
        &self,
        ty: &JvmType,
    ) -> Option<(Vec<JvmType>, JvmType)> {
        let ty_name = ty.to_type_name();
        let owner = ty_name.erased_internal();
        let class = self.view.get_class(owner)?;
        let class_type_params = class
            .generic_signature
            .as_deref()
            .map(parse_class_type_parameters)
            .unwrap_or_default();
        let type_args: Vec<JvmType> = ty_name
            .args
            .iter()
            .map(type_name_to_jvm_type)
            .collect::<Option<Vec<_>>>()?;
        let (methods, _) = self.view.collect_inherited_members(owner);
        let mut abstract_methods = methods.iter().filter(|m| {
            let flags = m.access_flags;
            (flags & ACC_ABSTRACT) != 0
                && (flags & ACC_STATIC) == 0
                && m.name.as_ref() != "equals"
                && m.name.as_ref() != "hashCode"
                && m.name.as_ref() != "toString"
        });
        let method = abstract_methods.next()?;
        if abstract_methods.next().is_some() {
            return None;
        }
        let fallback_desc = method.desc();
        let sig = method
            .generic_signature
            .as_deref()
            .unwrap_or(fallback_desc.as_ref());
        let (mut params, mut ret) = parse_method_signature_types(sig)?;
        if !class_type_params.is_empty() && class_type_params.len() == type_args.len() {
            params = params
                .into_iter()
                .map(|p| p.substitute(&class_type_params, &type_args))
                .collect();
            ret = ret.substitute(&class_type_params, &type_args);
        }
        Some((params, ret))
    }

    fn specialize_constructor_owner_return(
        &self,
        owner_internal: &str,
        input_ty: JvmType,
    ) -> Option<JvmType> {
        let class = self.view.get_class(owner_internal)?;
        let type_params = class
            .generic_signature
            .as_deref()
            .map(parse_class_type_parameters)
            .unwrap_or_default();
        if type_params.len() != 1 {
            return None;
        }
        Some(JvmType::Object(owner_internal.to_string(), vec![input_ty]))
    }

    fn resolve_method_reference_return_on_owner(
        &self,
        owner_internal: &str,
        member_name: &str,
    ) -> Option<JvmType> {
        let mut fallback: Option<JvmType> = None;
        for class in self.view.mro(owner_internal) {
            for method in class
                .methods
                .iter()
                .filter(|m| m.name.as_ref() == member_name)
            {
                let desc = method
                    .generic_signature
                    .clone()
                    .unwrap_or_else(|| method.desc());
                let (_, ret) = parse_method_signature_types(&desc)?;
                if let JvmType::Primitive('V') = ret {
                    continue;
                }
                if method.params.is_empty() {
                    return Some(ret);
                }
                if fallback.is_none() {
                    fallback = Some(ret);
                }
            }
        }
        fallback
    }

    fn resolve_owner_from_text(&self, raw: &str) -> Option<String> {
        let mut text = raw.trim();
        if let Some(i) = text.find('<') {
            text = &text[..i];
        }
        if text.is_empty() {
            return None;
        }
        if text.contains('/') {
            return self.view.get_class(text).map(|_| text.to_string());
        }
        if text.contains('.') {
            let candidate = text.replace('.', "/");
            return self.view.get_class(&candidate).map(|_| candidate);
        }

        let lang = format!("java/lang/{text}");
        if self.view.get_class(&lang).is_some() {
            return Some(lang);
        }

        let mut found: Option<String> = None;
        for class in self.view.iter_all_classes() {
            if class.name.as_ref() == text {
                if found.is_some() {
                    return None;
                }
                found = Some(class.internal_name.to_string());
            }
        }
        found
    }

    fn resolve_owner_from_text_with_context(
        &self,
        raw: &str,
        qualifier_resolver: Option<QualifierResolver>,
    ) -> Option<String> {
        if let Some(resolve_qualifier) = qualifier_resolver
            && let Some(ty) = resolve_qualifier(raw)
        {
            if ty.contains_slash() {
                return Some(ty.erased_internal().to_string());
            }
            if let Some(owner) = self.resolve_owner_from_text(ty.erased_internal()) {
                return Some(owner);
            }
        }
        self.resolve_owner_from_text(raw)
    }

    pub fn select_overload<'a>(
        &self,
        candidates: &[&'a MethodSummary],
        arg_count: usize,
        arg_types: &[TypeName],
    ) -> Option<&'a MethodSummary> {
        self.select_overload_match(candidates, arg_count, arg_types)
            .map(|m| m.method)
    }

    pub fn select_overload_match<'a>(
        &self,
        candidates: &[&'a MethodSummary],
        arg_count: usize,
        arg_types: &[TypeName],
    ) -> Option<OverloadMatch<'a>> {
        tracing::debug!(?candidates, ?arg_types, arg_count, "select_overload_match");

        if candidates.is_empty() {
            return None;
        }

        let normalized_args = normalize_arg_types(arg_count, arg_types);

        let mut best: Option<OverloadMatch<'a>> = None;
        for method in candidates.iter().copied() {
            if let Some(score) = self.score_fixed_applicability(method, &normalized_args) {
                let m = OverloadMatch {
                    method,
                    mode: OverloadInvocationMode::Fixed,
                    score,
                };
                if better_overload_match(best, m) {
                    best = Some(m);
                }
            }
            if let Some(score) = self.score_varargs_applicability(method, &normalized_args) {
                let m = OverloadMatch {
                    method,
                    mode: OverloadInvocationMode::Varargs,
                    score,
                };
                if better_overload_match(best, m) {
                    best = Some(m);
                }
            }
        }
        best
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
                    current_type = self.resolve_method_return_with_callsite_and_qualifier_resolver(
                        recv.as_ref(),
                        &seg.name,
                        CallArgs::new(seg.arg_count.unwrap_or(0), &seg.arg_types, &seg.arg_texts),
                        EvalContext::new(locals, enclosing_internal_name),
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
                    let args =
                        CallArgs::new(seg.arg_count.unwrap_or(0), &seg.arg_types, &seg.arg_texts);
                    let ctx = EvalContext::new(locals, enclosing_internal_name);
                    current_type = self.resolve_method_return_with_callsite_and_qualifier_resolver(
                        &receiver_internal,
                        actual_name,
                        args,
                        ctx,
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

    pub fn resolve_selected_param_type_from_generic_signature(
        &self,
        receiver_internal: &str,
        selected: &MethodSummary,
        arg_index: usize,
        mode: OverloadInvocationMode,
    ) -> Option<(TypeName, bool)> {
        let sig = selected.generic_signature.as_deref()?;
        let (params, _) = parse_method_signature_types(sig)?;
        let (mapped_index, vararg_element) =
            map_argument_index_to_parameter(selected, arg_index, mode, params.len())?;
        let mut param = params.get(mapped_index)?.clone();
        if vararg_element {
            let JvmType::Array(inner) = param else {
                return None;
            };
            param = *inner;
        }

        let (receiver_owner, receiver_args) = split_internal_name(receiver_internal);
        if !receiver_args.is_empty()
            && let Some(class_sig) =
                self.find_declaring_class_generic_signature(receiver_owner, selected)
        {
            let class_params = parse_class_type_parameters(class_sig.as_ref());
            if !class_params.is_empty() {
                param = param.substitute(&class_params, &receiver_args);
            }
        }

        // If class/receiver substitution still leaves a bare type variable (e.g. `E`),
        // treat this as unresolved so callers can use their existing fallback path.
        if matches!(param, JvmType::TypeVar(_)) {
            return None;
        }

        let exact = is_concrete_jvm_type(&param);
        Some((param.to_type_name(), exact))
    }

    pub fn resolve_selected_param_type_with_callsite_inference(
        &self,
        receiver_internal: &str,
        selected: &MethodSummary,
        arg_index: usize,
        mode: OverloadInvocationMode,
        args: CallArgs,
        _ctx: EvalContext,
    ) -> Option<(TypeName, bool)> {
        let sig = selected.generic_signature.as_deref()?;
        let (params, _) = parse_method_signature_types(sig)?;
        let (mapped_index, vararg_element) =
            map_argument_index_to_parameter(selected, arg_index, mode, params.len())?;
        let method_bindings = self.infer_method_type_bindings_from_non_lambda_args(
            sig,
            &params,
            receiver_internal,
            selected,
            arg_index,
            mode,
            args,
        );

        let mut param = params.get(mapped_index)?.clone();
        if vararg_element {
            let JvmType::Array(inner) = param else {
                return None;
            };
            param = *inner;
        }

        let (receiver_owner, receiver_args) = split_internal_name(receiver_internal);
        if !receiver_args.is_empty()
            && let Some(class_sig) =
                self.find_declaring_class_generic_signature(receiver_owner, selected)
        {
            let class_params = parse_class_type_parameters(class_sig.as_ref());
            if !class_params.is_empty() {
                param = param.substitute(&class_params, &receiver_args);
            }
        }

        if !method_bindings.is_empty() {
            param = substitute_type_vars(&param, &method_bindings);
        }

        if matches!(param, JvmType::TypeVar(_)) {
            return None;
        }

        let exact = is_concrete_jvm_type(&param);
        Some((param.to_type_name(), exact))
    }

    pub fn resolve_selected_param_descriptor_for_call(
        &self,
        selected: &MethodSummary,
        arg_index: usize,
        mode: OverloadInvocationMode,
    ) -> Option<Arc<str>> {
        let params = &selected.params.items;
        let (mapped_index, vararg_element) =
            map_argument_index_to_parameter(selected, arg_index, mode, params.len())?;
        let desc = params.get(mapped_index)?.descriptor.clone();
        if !vararg_element {
            return Some(desc);
        }
        if let Some(elem) = desc.strip_prefix('[') {
            return Some(Arc::from(elem));
        }
        None
    }

    fn find_declaring_class_generic_signature(
        &self,
        receiver_owner: &str,
        selected: &MethodSummary,
    ) -> Option<Arc<str>> {
        for class in self.view.mro(receiver_owner) {
            let matched = class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == selected.name.as_ref() && m.desc() == selected.desc());
            if matched {
                return class.generic_signature.clone();
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn infer_method_type_bindings_from_non_lambda_args(
        &self,
        method_signature: &str,
        param_jvm_types: &[JvmType],
        receiver_internal: &str,
        selected: &MethodSummary,
        skip_arg_index: usize,
        mode: OverloadInvocationMode,
        args: CallArgs,
    ) -> HashMap<String, JvmType> {
        let CallArgs {
            count: arg_count,
            types: arg_types,
            texts: arg_texts,
        } = args;
        let arg_texts = arg_texts.to_vec();

        let method_type_params = parse_method_type_parameters(method_signature);
        if method_type_params.is_empty() {
            return HashMap::new();
        }
        let method_type_param_set: HashSet<String> = method_type_params.into_iter().collect();

        let (class_type_params, receiver_args) = {
            let (receiver_owner, receiver_args) = split_internal_name(receiver_internal);
            self.find_declaring_class_generic_signature(receiver_owner, selected)
                .map(|sig| (parse_class_type_parameters(sig.as_ref()), receiver_args))
                .unwrap_or_else(|| (Vec::new(), Vec::new()))
        };

        let mut bindings: HashMap<String, JvmType> = HashMap::new();
        let mut conflicted = HashSet::new();

        for arg_idx in 0..arg_count {
            if arg_idx == skip_arg_index {
                continue;
            }
            let arg_text = arg_texts.get(arg_idx).map(|s| s.trim()).unwrap_or("");
            if arg_text.contains("->") || arg_text.contains("::") {
                continue;
            }
            let Some(arg_ty) = arg_types.get(arg_idx) else {
                continue;
            };
            if arg_ty.erased_internal() == "unknown" {
                continue;
            }
            let Some((mapped_index, vararg_element)) =
                map_argument_index_to_parameter(selected, arg_idx, mode, param_jvm_types.len())
            else {
                continue;
            };
            let mut formal = match param_jvm_types.get(mapped_index) {
                Some(p) => p.clone(),
                None => continue,
            };
            if vararg_element {
                let JvmType::Array(inner) = formal else {
                    continue;
                };
                formal = *inner;
            }
            if !class_type_params.is_empty() && !receiver_args.is_empty() {
                formal = formal.substitute(&class_type_params, &receiver_args);
            }
            let Some(actual) = type_name_to_jvm_type(arg_ty) else {
                continue;
            };
            bind_method_type_vars(
                &formal,
                &actual,
                &method_type_param_set,
                &mut bindings,
                &mut conflicted,
            );
        }

        for key in conflicted {
            bindings.remove(&key);
        }
        bindings
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

    fn score_fixed_applicability(
        &self,
        method: &MethodSummary,
        arg_types: &[TypeName],
    ) -> Option<i32> {
        if method.params.len() != arg_types.len() {
            return None;
        }
        self.score_descriptor_list(
            &method
                .params
                .items
                .iter()
                .map(|p| p.descriptor.as_ref())
                .collect::<Vec<_>>(),
            arg_types,
        )
    }

    fn score_varargs_applicability(
        &self,
        method: &MethodSummary,
        arg_types: &[TypeName],
    ) -> Option<i32> {
        if !is_varargs_method(method) {
            return None;
        }
        let params = &method.params.items;
        if params.is_empty() {
            return None;
        }
        let fixed_prefix = params.len() - 1;
        if arg_types.len() < fixed_prefix {
            return None;
        }
        let mut total = 0;
        for (i, p) in params.iter().take(fixed_prefix).enumerate() {
            total += self.score_argument_against_descriptor(&p.descriptor, &arg_types[i])?;
        }
        let vararg_desc = params.last()?.descriptor.as_ref();
        let vararg_elem = vararg_desc.strip_prefix('[')?;
        for arg in arg_types.iter().skip(fixed_prefix) {
            total += self.score_argument_against_descriptor(vararg_elem, arg)?;
        }
        // Prefer fixed invocation when both are otherwise applicable.
        Some(total - 1)
    }

    fn score_descriptor_list(&self, param_descs: &[&str], arg_types: &[TypeName]) -> Option<i32> {
        if param_descs.len() != arg_types.len() {
            return None;
        }
        let mut total = 0;
        for (desc, arg) in param_descs.iter().zip(arg_types.iter()) {
            total += self.score_argument_against_descriptor(desc, arg)?;
        }
        Some(total)
    }

    fn score_argument_against_descriptor(&self, desc: &str, arg_ty: &TypeName) -> Option<i32> {
        if is_type_variable_descriptor_token(desc) {
            return Some(0);
        }
        if arg_ty.erased_internal() == "unknown" {
            return Some(0);
        }
        let ty_str = arg_ty.erased_internal_with_arrays();
        let score = self.score_single_descriptor(desc, &ty_str);
        (score >= 0).then_some(score)
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

        if are_boxing_compatible_type_names(resolved_desc, normalized_ty.as_str()) {
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

    pub fn type_compatibility(&self, source: &TypeName, target: &TypeName) -> TypeCompatibility {
        let kind = self.classify_conversion(source, target);
        let score = match kind {
            ConversionKind::Identity => 120,
            ConversionKind::WideningPrimitive => 95,
            ConversionKind::UnboxingWideningPrimitive => 85,
            ConversionKind::Unboxing | ConversionKind::Boxing => 80,
            ConversionKind::BoxingWideningPrimitive => 75,
            ConversionKind::ReferenceWidening => 70,
            ConversionKind::NullToReference => 65,
            ConversionKind::Unknown => 0,
            ConversionKind::NarrowingPrimitive => -20,
            ConversionKind::Incompatible => -100,
        };
        TypeCompatibility { kind, score }
    }

    pub fn classify_conversion(&self, source: &TypeName, target: &TypeName) -> ConversionKind {
        let source_erased = source.erased_internal_with_arrays();
        let target_erased = target.erased_internal_with_arrays();
        if source_erased == target_erased {
            return ConversionKind::Identity;
        }
        if source.erased_internal() == "unknown" || target.erased_internal() == "unknown" {
            return ConversionKind::Unknown;
        }

        if source.erased_internal() == "null" {
            return if is_reference_type_name(target) {
                ConversionKind::NullToReference
            } else {
                ConversionKind::Incompatible
            };
        }

        let source_prim = unboxed_primitive_type_name(source.erased_internal());
        let target_prim = unboxed_primitive_type_name(target.erased_internal());

        if let (Some(src), Some(dst)) = (source_prim, target_prim) {
            if src == dst {
                if source.is_primitive() && !target.is_primitive() {
                    return ConversionKind::Boxing;
                }
                if !source.is_primitive() && target.is_primitive() {
                    return ConversionKind::Unboxing;
                }
                return ConversionKind::Identity;
            }
            if is_widening_primitive(src, dst) {
                if source.is_primitive() && target.is_primitive() {
                    return ConversionKind::WideningPrimitive;
                }
                if source.is_primitive() && !target.is_primitive() {
                    return ConversionKind::BoxingWideningPrimitive;
                }
                if !source.is_primitive() && target.is_primitive() {
                    return ConversionKind::UnboxingWideningPrimitive;
                }
                return ConversionKind::Incompatible;
            }
            return ConversionKind::NarrowingPrimitive;
        }

        if are_boxing_compatible_type_names(source.erased_internal(), target.erased_internal()) {
            if source.is_primitive() && !target.is_primitive() {
                return ConversionKind::Boxing;
            }
            if !source.is_primitive() && target.is_primitive() {
                return ConversionKind::Unboxing;
            }
            return ConversionKind::Identity;
        }

        if is_reference_widening(self.view, source, target) {
            return ConversionKind::ReferenceWidening;
        }

        ConversionKind::Incompatible
    }
}

fn is_reference_type_name(ty: &TypeName) -> bool {
    if ty.is_primitive() {
        return false;
    }
    ty.array_dims > 0
        || ty.contains_slash()
        || matches!(ty.erased_internal(), "*" | "+" | "-" | "capture")
}

fn is_reference_widening(view: &IndexView, source: &TypeName, target: &TypeName) -> bool {
    if source.array_dims > 0 || target.array_dims > 0 {
        return target.array_dims == source.array_dims
            || (target.array_dims == 0 && target.erased_internal() == "java/lang/Object");
    }
    if source.is_primitive() || target.is_primitive() {
        return false;
    }
    let src = source.erased_internal();
    let dst = target.erased_internal();
    if src == dst {
        return true;
    }
    view.mro(src)
        .iter()
        .any(|c| c.internal_name.as_ref() == dst)
}

fn is_widening_primitive(source: &str, target: &str) -> bool {
    matches!(
        (source, target),
        ("byte", "short")
            | ("byte", "int")
            | ("byte", "long")
            | ("byte", "float")
            | ("byte", "double")
            | ("short", "int")
            | ("short", "long")
            | ("short", "float")
            | ("short", "double")
            | ("char", "int")
            | ("char", "long")
            | ("char", "float")
            | ("char", "double")
            | ("int", "long")
            | ("int", "float")
            | ("int", "double")
            | ("long", "float")
            | ("long", "double")
            | ("float", "double")
    )
}

pub(crate) fn primitive_wrapper_type_name(primitive: &str) -> Option<&'static str> {
    match primitive {
        "boolean" => Some("java/lang/Boolean"),
        "byte" => Some("java/lang/Byte"),
        "char" => Some("java/lang/Character"),
        "short" => Some("java/lang/Short"),
        "int" => Some("java/lang/Integer"),
        "long" => Some("java/lang/Long"),
        "float" => Some("java/lang/Float"),
        "double" => Some("java/lang/Double"),
        _ => None,
    }
}

pub(crate) fn unboxed_primitive_type_name(ty: &str) -> Option<&'static str> {
    let normalized = ty.replace('.', "/");
    match normalized.as_str() {
        "boolean" => Some("boolean"),
        "byte" => Some("byte"),
        "char" => Some("char"),
        "short" => Some("short"),
        "int" => Some("int"),
        "long" => Some("long"),
        "float" => Some("float"),
        "double" => Some("double"),
        "java/lang/Boolean" | "Boolean" => Some("boolean"),
        "java/lang/Byte" | "Byte" => Some("byte"),
        "java/lang/Character" | "Character" => Some("char"),
        "java/lang/Short" | "Short" => Some("short"),
        "java/lang/Integer" | "Integer" => Some("int"),
        "java/lang/Long" | "Long" => Some("long"),
        "java/lang/Float" | "Float" => Some("float"),
        "java/lang/Double" | "Double" => Some("double"),
        _ => None,
    }
}

pub(crate) fn are_boxing_compatible_type_names(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let left_normalized = left.replace('.', "/");
    let right_normalized = right.replace('.', "/");
    if let Some(wrapper) = primitive_wrapper_type_name(left_normalized.as_str())
        && wrapper == right_normalized
    {
        return true;
    }
    if let Some(wrapper) = primitive_wrapper_type_name(right_normalized.as_str())
        && wrapper == left_normalized
    {
        return true;
    }
    false
}

pub(crate) fn promoted_numeric_result_type_name(left: &str, right: &str) -> Option<&'static str> {
    let left = unboxed_primitive_type_name(left)?;
    let right = unboxed_primitive_type_name(right)?;
    if !matches!(
        left,
        "byte" | "short" | "char" | "int" | "long" | "float" | "double"
    ) || !matches!(
        right,
        "byte" | "short" | "char" | "int" | "long" | "float" | "double"
    ) {
        return None;
    }

    if left == "double" || right == "double" {
        return Some("double");
    }
    if left == "float" || right == "float" {
        return Some("float");
    }
    if left == "long" || right == "long" {
        return Some("long");
    }
    Some("int")
}

pub(crate) fn promoted_integral_result_type_name(left: &str, right: &str) -> Option<&'static str> {
    let left = unboxed_primitive_type_name(left)?;
    let right = unboxed_primitive_type_name(right)?;
    if !matches!(left, "byte" | "short" | "char" | "int" | "long")
        || !matches!(right, "byte" | "short" | "char" | "int" | "long")
    {
        return None;
    }
    if left == "long" || right == "long" {
        Some("long")
    } else {
        Some("int")
    }
}

pub(crate) fn promoted_shift_result_type_name(left: &str, right: &str) -> Option<&'static str> {
    let left = unboxed_primitive_type_name(left)?;
    let right = unboxed_primitive_type_name(right)?;
    if !matches!(left, "byte" | "short" | "char" | "int" | "long")
        || !matches!(right, "byte" | "short" | "char" | "int" | "long")
    {
        return None;
    }
    if left == "long" {
        Some("long")
    } else {
        Some("int")
    }
}

pub(crate) fn promoted_unary_integral_result_type_name(operand: &str) -> Option<&'static str> {
    let operand = unboxed_primitive_type_name(operand)?;
    if !matches!(operand, "byte" | "short" | "char" | "int" | "long") {
        return None;
    }
    if operand == "long" {
        Some("long")
    } else {
        Some("int")
    }
}

fn normalize_arg_types(arg_count: usize, arg_types: &[TypeName]) -> Vec<TypeName> {
    if arg_types.len() >= arg_count {
        return arg_types[..arg_count].to_vec();
    }
    let mut out = arg_types.to_vec();
    while out.len() < arg_count {
        out.push(TypeName::new("unknown"));
    }
    out
}

fn is_varargs_method(method: &MethodSummary) -> bool {
    if (method.access_flags & ACC_VARARGS) == 0 || method.params.is_empty() {
        return false;
    }
    method
        .params
        .items
        .last()
        .is_some_and(|p| p.descriptor.starts_with('['))
}

fn better_overload_match(current: Option<OverloadMatch<'_>>, candidate: OverloadMatch<'_>) -> bool {
    let Some(current) = current else {
        return true;
    };
    let mode_rank = |m: OverloadInvocationMode| match m {
        OverloadInvocationMode::Fixed => 1,
        OverloadInvocationMode::Varargs => 0,
    };
    if mode_rank(candidate.mode) != mode_rank(current.mode) {
        return mode_rank(candidate.mode) > mode_rank(current.mode);
    }
    candidate.score > current.score
}

fn map_argument_index_to_parameter(
    selected: &MethodSummary,
    arg_index: usize,
    mode: OverloadInvocationMode,
    param_len: usize,
) -> Option<(usize, bool)> {
    if param_len == 0 {
        return None;
    }
    match mode {
        OverloadInvocationMode::Fixed => {
            if arg_index >= param_len {
                None
            } else {
                Some((arg_index, false))
            }
        }
        OverloadInvocationMode::Varargs => {
            if !is_varargs_method(selected) {
                return None;
            }
            let fixed_prefix = param_len - 1;
            if arg_index < fixed_prefix {
                Some((arg_index, false))
            } else {
                Some((param_len - 1, true))
            }
        }
    }
}

fn is_type_variable_descriptor_token(desc: &str) -> bool {
    if desc.starts_with('T') && desc.ends_with(';') {
        return true;
    }
    if !(desc.starts_with('L') && desc.ends_with(';')) {
        return false;
    }
    let inner = &desc[1..desc.len() - 1];
    if inner.is_empty() || inner.contains('/') || inner.contains('.') || inner.contains('$') {
        return false;
    }
    inner.len() <= 3 && inner.chars().all(|c| c.is_ascii_uppercase())
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

fn jvm_type_to_source_type(ty: &JvmType, provider: &impl SymbolProvider) -> String {
    match ty {
        JvmType::Primitive(c) => match c {
            'B' => "byte".to_string(),
            'C' => "char".to_string(),
            'D' => "double".to_string(),
            'F' => "float".to_string(),
            'I' => "int".to_string(),
            'J' => "long".to_string(),
            'S' => "short".to_string(),
            'Z' => "boolean".to_string(),
            'V' => "void".to_string(),
            _ => "unknown".to_string(),
        },
        JvmType::TypeVar(name) => name.clone(),
        JvmType::Array(inner) => format!("{}[]", jvm_type_to_source_type(inner, provider)),
        JvmType::Wildcard => "?".to_string(),
        JvmType::WildcardBound('+', inner) => {
            format!("? extends {}", jvm_type_to_source_type(inner, provider))
        }
        JvmType::WildcardBound('-', inner) => {
            format!("? super {}", jvm_type_to_source_type(inner, provider))
        }
        JvmType::WildcardBound(_, inner) => jvm_type_to_source_type(inner, provider),
        JvmType::Object(internal, args) => {
            let base = provider
                .resolve_source_name(internal)
                .unwrap_or_else(|| internal.replace('/', "."));
            if args.is_empty() {
                base
            } else {
                let rendered_args: Vec<String> = args
                    .iter()
                    .map(|a| jvm_type_to_source_type(a, provider))
                    .collect();
                format!("{base}<{}>", rendered_args.join(", "))
            }
        }
    }
}

/// Render a JVM descriptor/signature token into source-style text.
/// Accepts plain descriptors (e.g. `Ljava/lang/String;`) and generic signatures
/// (e.g. `Ljava/util/Map<TK;Ljava/util/List<TV;>;>;`).
pub fn signature_to_source_type(sig: &str, provider: &impl SymbolProvider) -> Option<String> {
    let (ty, rest) = JvmType::parse(sig)?;
    if !rest.is_empty() {
        return None;
    }
    Some(jvm_type_to_source_type(&ty, provider))
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
    pub arg_count: Option<usize>,
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

    pub fn method(name: impl Into<String>, arg_count: usize) -> Self {
        Self {
            name: name.into(),
            arg_count: Some(arg_count),
            arg_types: vec![],
            arg_texts: vec![],
        }
    }

    pub fn method_with_types(
        name: impl Into<String>,
        arg_count: usize,
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

fn collect_type_vars(ty: &JvmType, out: &mut HashSet<String>) {
    match ty {
        JvmType::TypeVar(name) => {
            out.insert(name.clone());
        }
        JvmType::Object(_, args) => {
            for arg in args {
                collect_type_vars(arg, out);
            }
        }
        JvmType::Array(inner) | JvmType::WildcardBound(_, inner) => {
            collect_type_vars(inner, out);
        }
        JvmType::Primitive(_) | JvmType::Wildcard => {}
    }
}

fn bind_type_var(
    name: &str,
    candidate: JvmType,
    bindings: &mut HashMap<String, JvmType>,
    conflicted: &mut HashSet<String>,
) {
    if conflicted.contains(name) {
        return;
    }
    if let Some(existing) = bindings.get(name) {
        if existing != &candidate {
            conflicted.insert(name.to_string());
        }
        return;
    }
    bindings.insert(name.to_string(), candidate);
}

fn bind_method_type_vars(
    formal: &JvmType,
    actual: &JvmType,
    method_type_params: &HashSet<String>,
    bindings: &mut HashMap<String, JvmType>,
    conflicted: &mut HashSet<String>,
) {
    match formal {
        JvmType::TypeVar(name) if method_type_params.contains(name) => {
            bind_type_var(name, actual.clone(), bindings, conflicted);
        }
        JvmType::Array(formal_inner) => {
            if let JvmType::Array(actual_inner) = actual {
                bind_method_type_vars(
                    formal_inner,
                    actual_inner,
                    method_type_params,
                    bindings,
                    conflicted,
                );
            }
        }
        JvmType::WildcardBound(_, formal_inner) => {
            bind_method_type_vars(
                formal_inner,
                actual,
                method_type_params,
                bindings,
                conflicted,
            );
        }
        JvmType::Object(_, formal_args) => {
            if let JvmType::Object(_, actual_args) = actual {
                for (formal_arg, actual_arg) in formal_args.iter().zip(actual_args.iter()) {
                    bind_method_type_vars(
                        formal_arg,
                        actual_arg,
                        method_type_params,
                        bindings,
                        conflicted,
                    );
                }
            }
        }
        JvmType::Primitive(_) | JvmType::Wildcard | JvmType::TypeVar(_) => {}
    }
}

fn extract_functional_input_type(param_ty: &JvmType) -> Option<JvmType> {
    let JvmType::Object(_, type_args) = param_ty else {
        return None;
    };
    let first = type_args.first()?;
    match first {
        JvmType::WildcardBound('-', inner) => Some(inner.as_ref().clone()),
        JvmType::WildcardBound('+', inner) => Some(inner.as_ref().clone()),
        other => Some(other.clone()),
    }
}

fn is_concrete_jvm_type(ty: &JvmType) -> bool {
    match ty {
        JvmType::TypeVar(_) | JvmType::Wildcard | JvmType::WildcardBound(_, _) => false,
        JvmType::Primitive(_) => true,
        JvmType::Array(inner) => is_concrete_jvm_type(inner),
        JvmType::Object(_, args) => args.iter().all(is_concrete_jvm_type),
    }
}

pub fn is_concrete_type_name(ty: &TypeName) -> bool {
    type_name_to_jvm_type(ty).is_some_and(|parsed| is_concrete_jvm_type(&parsed))
}

fn type_name_to_jvm_type(ty: &TypeName) -> Option<JvmType> {
    let sig = ty.to_jvm_signature();
    let (parsed, rest) = JvmType::parse(&sig)?;
    if rest.is_empty() { Some(parsed) } else { None }
}

fn box_primitive_jvm_type(ty: JvmType) -> JvmType {
    match ty {
        JvmType::Primitive('B') => JvmType::Object("java/lang/Byte".to_string(), vec![]),
        JvmType::Primitive('S') => JvmType::Object("java/lang/Short".to_string(), vec![]),
        JvmType::Primitive('I') => JvmType::Object("java/lang/Integer".to_string(), vec![]),
        JvmType::Primitive('J') => JvmType::Object("java/lang/Long".to_string(), vec![]),
        JvmType::Primitive('F') => JvmType::Object("java/lang/Float".to_string(), vec![]),
        JvmType::Primitive('D') => JvmType::Object("java/lang/Double".to_string(), vec![]),
        JvmType::Primitive('C') => JvmType::Object("java/lang/Character".to_string(), vec![]),
        JvmType::Primitive('Z') => JvmType::Object("java/lang/Boolean".to_string(), vec![]),
        other => other,
    }
}

fn parse_method_reference_text(text: &str) -> Option<(&str, &str, bool)> {
    let idx = text.find("::")?;
    let qualifier = text[..idx].trim();
    let member = text[idx + 2..].trim();
    if qualifier.is_empty() || member.is_empty() {
        return None;
    }
    let is_constructor = member == "new";
    Some((qualifier, member, is_constructor))
}

fn parse_lambda_expression_body(text: &str) -> Option<&str> {
    let idx = text.find("->")?;
    let body = text[idx + 2..].trim();
    if body.is_empty() || body.starts_with('{') {
        return None;
    }
    Some(body)
}

fn parse_lambda_text_parts(text: &str) -> Option<(Vec<String>, &str)> {
    let idx = text.find("->")?;
    let params_text = text[..idx].trim();
    let body = text[idx + 2..].trim();
    if body.is_empty() {
        return None;
    }
    let params = if params_text.is_empty() {
        return None;
    } else if params_text == "()" {
        Vec::new()
    } else if params_text.starts_with('(') && params_text.ends_with(')') {
        let inner = &params_text[1..params_text.len() - 1];
        if inner.trim().is_empty() {
            Vec::new()
        } else {
            inner
                .split(',')
                .filter_map(|part| {
                    let trimmed = part.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        trimmed.split_whitespace().last().map(str::to_string)
                    }
                })
                .collect()
        }
    } else {
        vec![params_text.to_string()]
    };
    Some((params, body))
}

fn is_likely_type_qualifier(qualifier: &str) -> bool {
    let q = qualifier.trim();
    if q.is_empty() {
        return false;
    }
    if q.contains('.') || q.contains('/') {
        return true;
    }
    q.chars().next().is_some_and(|c| c.is_ascii_uppercase())
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
        language::java::render,
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

    fn make_functional_binding_fixture() -> (IndexView, Vec<LocalVar>) {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Box"),
                internal_name: Arc::from("Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("map"),
                        params: MethodParams::from_method_descriptor(
                            "(Ljava/util/function/Function;)LBox;",
                        ),
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        annotations: vec![],
                        generic_signature: Some(Arc::from(
                            "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)LBox<TR;>;",
                        )),
                        return_type: Some(Arc::from("LBox;")),
                    },
                    MethodSummary {
                        name: Arc::from("get"),
                        params: MethodParams::empty(),
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        annotations: vec![],
                        generic_signature: Some(Arc::from("()TT;")),
                        return_type: Some(Arc::from("Ljava/lang/Object;")),
                    },
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("size"),
                    params: MethodParams::empty(),
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    annotations: vec![],
                    generic_signature: None,
                    return_type: Some(Arc::from("I")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("ArrayList"),
                internal_name: Arc::from("java/util/ArrayList"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
        ]);

        let box_t = TypeName::with_args(
            "Box",
            vec![TypeName::with_args(
                "java/util/List",
                vec![TypeName::new("java/lang/String")],
            )],
        );
        let locals = vec![LocalVar {
            name: Arc::from("box"),
            type_internal: box_t,
            init_expr: None,
        }];
        (idx.view(root_scope()), locals)
    }

    fn make_functional_binding_fixture_with_ambiguous_list() -> (IndexView, Vec<LocalVar>) {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Box"),
                internal_name: Arc::from("Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("map"),
                        params: MethodParams::from_method_descriptor(
                            "(Ljava/util/function/Function;)LBox;",
                        ),
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        annotations: vec![],
                        generic_signature: Some(Arc::from(
                            "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)LBox<TR;>;",
                        )),
                        return_type: Some(Arc::from("LBox;")),
                    },
                    MethodSummary {
                        name: Arc::from("get"),
                        params: MethodParams::empty(),
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        annotations: vec![],
                        generic_signature: Some(Arc::from("()TT;")),
                        return_type: Some(Arc::from("Ljava/lang/Object;")),
                    },
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("size"),
                    params: MethodParams::empty(),
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    annotations: vec![],
                    generic_signature: None,
                    return_type: Some(Arc::from("I")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/awt")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/awt/List"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("size"),
                    params: MethodParams::empty(),
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    annotations: vec![],
                    generic_signature: None,
                    return_type: Some(Arc::from("I")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
        ]);

        let box_t = TypeName::with_args(
            "Box",
            vec![TypeName::with_args(
                "java/util/List",
                vec![TypeName::new("java/lang/String")],
            )],
        );
        let locals = vec![LocalVar {
            name: Arc::from("box"),
            type_internal: box_t,
            init_expr: None,
        }];
        (idx.view(root_scope()), locals)
    }

    fn make_functional_binding_trim_constructor_fixture() -> (IndexView, Vec<LocalVar>) {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Box"),
                internal_name: Arc::from("Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("map"),
                        params: MethodParams::from_method_descriptor(
                            "(Ljava/util/function/Function;)LBox;",
                        ),
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        annotations: vec![],
                        generic_signature: Some(Arc::from(
                            "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)LBox<TR;>;",
                        )),
                        return_type: Some(Arc::from("LBox;")),
                    },
                    MethodSummary {
                        name: Arc::from("get"),
                        params: MethodParams::empty(),
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        annotations: vec![],
                        generic_signature: Some(Arc::from("()TT;")),
                        return_type: Some(Arc::from("Ljava/lang/Object;")),
                    },
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("String"),
                internal_name: Arc::from("java/lang/String"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("trim"),
                    params: MethodParams::empty(),
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    annotations: vec![],
                    generic_signature: None,
                    return_type: Some(Arc::from("Ljava/lang/String;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("ArrayList"),
                internal_name: Arc::from("java/util/ArrayList"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("<init>"),
                    params: MethodParams::empty(),
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    annotations: vec![],
                    generic_signature: None,
                    return_type: None,
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
        ]);

        let locals = vec![
            LocalVar {
                name: Arc::from("strBox"),
                type_internal: TypeName::with_args("Box", vec![TypeName::new("java/lang/String")]),
                init_expr: None,
            },
            LocalVar {
                name: Arc::from("s"),
                type_internal: TypeName::with_args("Box", vec![TypeName::new("java/lang/String")]),
                init_expr: None,
            },
        ];
        (idx.view(root_scope()), locals)
    }

    struct SnapshotProvider;
    impl SymbolProvider for SnapshotProvider {
        fn resolve_source_name(&self, internal_name: &str) -> Option<String> {
            Some(internal_name.replace('/', "."))
        }
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
    fn test_functional_method_reference_binds_generic_return_type() {
        let (view, locals) = make_functional_binding_fixture();
        let resolver = TypeResolver::new(&view);
        let chain = parse_chain_from_expr("box.map(List::size).get()");

        let result = resolver.resolve_chain(&chain, &locals, None);
        assert_eq!(
            result.as_ref().map(|t| t.erased_internal()),
            Some("int"),
            "List::size should bind map<R> to int"
        );
    }

    #[test]
    fn test_functional_constructor_reference_binds_generic_return_type() {
        let (view, locals) = make_functional_binding_fixture();
        let resolver = TypeResolver::new(&view);
        let chain = parse_chain_from_expr("box.map(ArrayList::new).get()");

        let result = resolver.resolve_chain(&chain, &locals, None);
        assert_eq!(
            result.as_ref().map(|t| t.erased_internal()),
            Some("java/util/ArrayList"),
            "Type::new should bind map<R> to constructed owner type"
        );
        assert_eq!(
            result.as_ref().map(|t| t.args.len()),
            Some(1),
            "constructor reference should specialize generic owner when target input is concrete"
        );
        assert_eq!(
            result
                .as_ref()
                .and_then(|t| t.args.first())
                .map(|a| a.to_internal_with_generics()),
            Some("java/util/List<Ljava/lang/String;>".to_string()),
            "ArrayList::new should specialize to ArrayList<List<String>> in this fixture"
        );
    }

    #[test]
    fn test_functional_constructor_reference_falls_back_for_ambiguous_owner_generic_shape() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Box"),
                internal_name: Arc::from("Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("map"),
                        params: MethodParams::from_method_descriptor(
                            "(Ljava/util/function/Function;)LBox;",
                        ),
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        annotations: vec![],
                        generic_signature: Some(Arc::from(
                            "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)LBox<TR;>;",
                        )),
                        return_type: Some(Arc::from("LBox;")),
                    },
                    MethodSummary {
                        name: Arc::from("get"),
                        params: MethodParams::empty(),
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        annotations: vec![],
                        generic_signature: Some(Arc::from("()TT;")),
                        return_type: Some(Arc::from("Ljava/lang/Object;")),
                    },
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: None,
                name: Arc::from("Pair"),
                internal_name: Arc::from("Pair"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("<init>"),
                    params: MethodParams::empty(),
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    annotations: vec![],
                    generic_signature: None,
                    return_type: None,
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from(
                    "<A:Ljava/lang/Object;B:Ljava/lang/Object;>Ljava/lang/Object;",
                )),
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("s"),
            type_internal: TypeName::with_args("Box", vec![TypeName::new("java/lang/String")]),
            init_expr: None,
        }];
        let chain = parse_chain_from_expr("s.map(Pair::new).get()");
        let result = resolver
            .resolve_chain(&chain, &locals, None)
            .expect("resolved chain");

        assert_eq!(result.erased_internal(), "Pair");
        assert!(
            result.args.is_empty(),
            "ambiguous owner generic arity should conservatively fall back to raw owner type"
        );
    }

    #[test]
    fn test_functional_lambda_inference_stays_unresolved_when_not_trivial() {
        let (view, locals) = make_functional_binding_fixture();
        let resolver = TypeResolver::new(&view);
        let chain = parse_chain_from_expr("box.map(x -> x + 1).get()");

        let result = resolver.resolve_chain(&chain, &locals, None);
        assert_eq!(
            result.as_ref().map(|t| t.erased_internal()),
            Some("R"),
            "non-trivial lambda body should stay conservatively unresolved"
        );
    }

    #[test]
    fn test_functional_lambda_return_inference_boxes_primitive_result() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::{ACC_ABSTRACT, ACC_PUBLIC};

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("String"),
                internal_name: Arc::from("java/lang/String"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("length"),
                    params: MethodParams::empty(),
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    annotations: vec![],
                    generic_signature: None,
                    return_type: Some(Arc::from("I")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util/function")),
                name: Arc::from("Function"),
                internal_name: Arc::from("java/util/function/Function"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("apply"),
                    params: MethodParams::from([("Ljava/lang/Object;", "t")]),
                    access_flags: ACC_PUBLIC | ACC_ABSTRACT,
                    is_synthetic: false,
                    annotations: vec![],
                    generic_signature: Some(Arc::from("(TT;)TR;")),
                    return_type: Some(Arc::from("Ljava/lang/Object;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from(
                    "<T:Ljava/lang/Object;R:Ljava/lang/Object;>Ljava/lang/Object;",
                )),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("StringBuilder"),
                internal_name: Arc::from("java/lang/StringBuilder"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let target = JvmType::Object(
            "java/util/function/Function".to_string(),
            vec![
                JvmType::Object("java/lang/String".to_string(), vec![]),
                JvmType::TypeVar("R".to_string()),
            ],
        );

        let inferred = resolver.infer_functional_arg_return_shallow(
            "s -> s.length()",
            EvalContext::default(),
            Some(&target),
        );
        assert_eq!(
            inferred.map(|t| t.to_signature_string()),
            Some("Ljava/lang/Integer;".to_string())
        );
        assert_eq!(
            resolver
                .infer_functional_arg_return_shallow(
                    "s -> new StringBuilder(s)",
                    EvalContext::default(),
                    Some(&target)
                )
                .map(|t| t.to_signature_string()),
            Some("Ljava/lang/StringBuilder;".to_string())
        );
    }

    #[test]
    fn test_source_derived_map_signature_keeps_bindable_r_for_chain_resolution() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use crate::language::java::class_parser::parse_java_source;
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        let scope = root_scope();
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            public class Demo<T> {
                public <R> Demo<R> map(Function<? super T, ? extends R> fn) { return null; }
                public T get() { return null; }
            }
        "#};
        let origin = ClassOrigin::SourceFile(Arc::from("file:///tmp/provenance/Demo.java"));
        let classes = parse_java_source(src, origin.clone(), None);
        idx.update_source(scope, origin, classes);
        idx.add_classes(vec![ClassMetadata {
            package: None,
            name: Arc::from("List"),
            internal_name: Arc::from("List"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("size"),
                params: MethodParams::empty(),
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                annotations: vec![],
                generic_signature: None,
                return_type: Some(Arc::from("I")),
            }],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            inner_class_of: None,
            generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
            origin: ClassOrigin::Unknown,
        }]);

        let view = idx.view(scope);
        let resolver = TypeResolver::new(&view);
        let locals = vec![LocalVar {
            name: Arc::from("box"),
            type_internal: TypeName::with_args(
                "Demo",
                vec![TypeName::with_args(
                    "List",
                    vec![TypeName::new("java/lang/String")],
                )],
            ),
            init_expr: None,
        }];
        let chain = parse_chain_from_expr("box.map(List::size).get()");
        let result = resolver.resolve_chain(&chain, &locals, None);

        assert_eq!(
            result.as_ref().map(|t| t.erased_internal()),
            Some("int"),
            "source-derived generic_signature for map should preserve bindable R"
        );
    }

    #[test]
    fn test_snapshot_provenance_map_list_size_with_ambiguous_simple_list_owner() {
        let (view, locals) = make_functional_binding_fixture_with_ambiguous_list();
        let resolver = TypeResolver::new(&view);
        let chain = parse_chain_from_expr("box.map(List::size).get()");
        let map_seg = chain.get(1).expect("map segment");

        let receiver = resolver
            .resolve("box", &locals, None)
            .expect("box receiver");
        let receiver_internal = receiver.to_internal_with_generics();
        let (methods, _) = view.collect_inherited_members(receiver.erased_internal());
        let map_candidates: Vec<_> = methods
            .iter()
            .filter(|m| m.name.as_ref() == "map")
            .map(|m| m.as_ref())
            .collect();
        let selected = resolver
            .select_overload(&map_candidates, map_seg.arg_count.unwrap_or(0), &[])
            .expect("selected map overload");
        let selected_sig = selected
            .generic_signature
            .clone()
            .unwrap_or_else(|| selected.desc());
        let (param_jvm, ret_jvm) =
            parse_method_signature_types(&selected_sig).expect("parsed map signature");
        let mut return_type_vars = std::collections::HashSet::new();
        collect_type_vars(&ret_jvm, &mut return_type_vars);
        let mut return_type_vars_sorted: Vec<_> = return_type_vars.iter().cloned().collect();
        return_type_vars_sorted.sort();

        let inferred_direct = resolver.infer_functional_arg_return_shallow(
            "List::size",
            EvalContext::new(&locals, None),
            None,
        );
        let inferred_bindings = resolver.infer_method_type_bindings_shallow(
            selected,
            &selected_sig,
            &param_jvm,
            &ret_jvm,
            &receiver_internal,
            view.get_class(receiver.erased_internal())
                .and_then(|c| c.generic_signature.clone())
                .as_deref(),
            CallArgs::new(map_seg.arg_count.unwrap_or(0), &[], &map_seg.arg_texts),
            EvalContext::new(&locals, None),
        );
        let mut inferred_bindings_sorted: Vec<_> = inferred_bindings
            .iter()
            .map(|(k, v)| (k.clone(), v.to_signature_string()))
            .collect();
        inferred_bindings_sorted.sort_by(|a, b| a.0.cmp(&b.0));

        let map_result = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
            &receiver_internal,
            "map",
            CallArgs::new(map_seg.arg_count.unwrap_or(0), &[], &map_seg.arg_texts),
            EvalContext::new(&locals, None),
        );
        let get_receiver = map_result
            .as_ref()
            .map(TypeName::to_internal_with_generics)
            .unwrap_or_else(|| "<none>".to_string());
        let get_result = map_result.as_ref().and_then(|r| {
            let get_receiver_internal = r.to_internal_with_generics();
            resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                &get_receiver_internal,
                "get",
                CallArgs::new(0, &[], &[]),
                EvalContext::new(&locals, None),
            )
        });
        let chain_result = resolver.resolve_chain(&chain, &locals, None);

        let mut out = String::new();
        out.push_str("method_reference_arg:\nList::size\n\n");
        out.push_str(&format!(
            "selected_map:\nname={}\ndesc={}\ngeneric_signature={:?}\nreturn_type={:?}\n\n",
            selected.name,
            selected.desc(),
            selected.generic_signature,
            selected.return_type,
        ));
        out.push_str(&format!(
            "parsed_signature:\nparams={:?}\nreturn={}\nreturn_type_vars={:?}\n\n",
            param_jvm
                .iter()
                .map(JvmType::to_signature_string)
                .collect::<Vec<_>>(),
            ret_jvm.to_signature_string(),
            return_type_vars_sorted
        ));
        out.push_str(&format!(
            "method_ref_owner_resolution:\nresolve_owner_from_text(\"List\")={:?}\nresolve_owner_from_text(\"java/util/List\")={:?}\nresolve_owner_from_text(\"java/awt/List\")={:?}\n\n",
            resolver.resolve_owner_from_text("List"),
            resolver.resolve_owner_from_text("java/util/List"),
            resolver.resolve_owner_from_text("java/awt/List"),
        ));
        out.push_str(&format!(
            "inference_bridge:\ninfer_functional_arg_return_shallow={:?}\ninferred_bindings={:?}\n\n",
            inferred_direct.map(|t| t.to_signature_string()),
            inferred_bindings_sorted
        ));
        out.push_str(&format!(
            "chain_propagation:\nreceiver_before_map={}\nmap_result={:?}\nget_receiver={}\nget_result={:?}\nfinal_chain_result={:?}\n",
            receiver_internal,
            map_result.as_ref().map(TypeName::to_internal_with_generics),
            get_receiver,
            get_result.as_ref().map(TypeName::to_internal_with_generics),
            chain_result.as_ref().map(TypeName::to_internal_with_generics),
        ));

        insta::assert_snapshot!(
            "functional_binding_provenance_map_list_size_ambiguous_list_owner",
            out
        );
    }

    #[test]
    fn test_snapshot_functional_chain_concretization_trim_and_constructor_new() {
        let (view, locals) = make_functional_binding_trim_constructor_fixture();
        let resolver = TypeResolver::new(&view);

        let trace_case = |var_name: &str, functional_arg: &str| {
            let mut out = String::new();
            let expr = format!("{var_name}.map({functional_arg}).get()");
            let chain = parse_chain_from_expr(&expr);
            let map_seg = chain.get(1).expect("map segment");
            let receiver = resolver
                .resolve(var_name, &locals, None)
                .expect("receiver local");
            let receiver_internal = receiver.to_internal_with_generics();

            let (methods, _) = view.collect_inherited_members(receiver.erased_internal());
            let map_candidates: Vec<_> = methods
                .iter()
                .filter(|m| m.name.as_ref() == "map")
                .map(|m| m.as_ref())
                .collect();
            let selected_map = resolver
                .select_overload(&map_candidates, map_seg.arg_count.unwrap_or(0), &[])
                .expect("selected map");
            let map_sig = selected_map
                .generic_signature
                .clone()
                .unwrap_or_else(|| selected_map.desc());
            let (map_params, map_ret) =
                parse_method_signature_types(&map_sig).expect("parsed map sig");

            let mut return_type_vars = std::collections::HashSet::new();
            collect_type_vars(&map_ret, &mut return_type_vars);
            let mut return_type_vars_sorted: Vec<_> = return_type_vars.iter().cloned().collect();
            return_type_vars_sorted.sort();

            let inferred_functional_ret = resolver.infer_functional_arg_return_shallow(
                functional_arg,
                EvalContext::new(&locals, None),
                None,
            );
            let bindings = resolver.infer_method_type_bindings_shallow(
                selected_map,
                &map_sig,
                &map_params,
                &map_ret,
                &receiver_internal,
                view.get_class(receiver.erased_internal())
                    .and_then(|c| c.generic_signature.clone())
                    .as_deref(),
                CallArgs::new(0, &[], &map_seg.arg_texts),
                EvalContext::new(&locals, None),
            );
            let mut bindings_sorted: Vec<_> = bindings
                .iter()
                .map(|(k, v)| (k.clone(), v.to_signature_string()))
                .collect();
            bindings_sorted.sort_by(|a, b| a.0.cmp(&b.0));

            let map_result = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                &receiver_internal,
                "map",
                CallArgs::new(map_seg.arg_count.unwrap_or(0), &[], &map_seg.arg_texts),
                EvalContext::new(&locals, None),
            );
            let get_receiver = map_result
                .as_ref()
                .map(TypeName::to_internal_with_generics)
                .unwrap_or_else(|| "<none>".to_string());
            let get_result = map_result.as_ref().and_then(|r| {
                resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                    &r.to_internal_with_generics(),
                    "get",
                    CallArgs::default(),
                    EvalContext::new(&locals, None),
                )
            });
            let chain_result = resolver.resolve_chain(&chain, &locals, None);

            let (box_methods, _) = view.collect_inherited_members("Box");
            let get_method = box_methods
                .iter()
                .find(|m| m.name.as_ref() == "get")
                .expect("get method");
            let box_meta = view.get_class("Box").expect("Box class");
            let rendered_get_detail = render::method_detail(
                &get_receiver,
                &box_meta,
                get_method.as_ref(),
                &SnapshotProvider,
            );

            out.push_str(&format!("expr={expr}\n"));
            out.push_str(&format!(
                "selected_map: desc={} generic_signature={:?} return_type={:?}\n",
                selected_map.desc(),
                selected_map.generic_signature,
                selected_map.return_type
            ));
            out.push_str(&format!(
                "parsed_map: params={:?} return={} return_type_vars={:?}\n",
                map_params
                    .iter()
                    .map(JvmType::to_signature_string)
                    .collect::<Vec<_>>(),
                map_ret.to_signature_string(),
                return_type_vars_sorted
            ));
            out.push_str(&format!(
                "functional_inference: inferred_return={:?} inferred_bindings={:?}\n",
                inferred_functional_ret.map(|t| t.to_signature_string()),
                bindings_sorted
            ));
            out.push_str(&format!(
                "chain_types: receiver_before_map={} map_result={:?} get_receiver={} get_result={:?} final_chain={:?}\n",
                receiver_internal,
                map_result.as_ref().map(TypeName::to_internal_with_generics),
                get_receiver,
                get_result.as_ref().map(TypeName::to_internal_with_generics),
                chain_result.as_ref().map(TypeName::to_internal_with_generics),
            ));
            out.push_str(&format!("rendered_get_detail={rendered_get_detail}\n"));
            out
        };

        let out = format!(
            "case_trim:\n{}\ncase_constructor:\n{}",
            trace_case("strBox", "String::trim"),
            trace_case("s", "ArrayList::new")
        );
        insta::assert_snapshot!(
            "functional_chain_concretization_trim_and_constructor_new",
            out
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
    fn test_shared_boxing_and_numeric_promotion_helpers() {
        assert!(are_boxing_compatible_type_names("int", "java/lang/Integer"));
        assert!(are_boxing_compatible_type_names(
            "java/lang/Double",
            "double"
        ));
        assert_eq!(
            unboxed_primitive_type_name("java/lang/Integer"),
            Some("int")
        );
        assert_eq!(unboxed_primitive_type_name("Double"), Some("double"));
        assert_eq!(
            promoted_numeric_result_type_name("java/lang/Integer", "double"),
            Some("double")
        );
        assert_eq!(
            promoted_numeric_result_type_name("java/lang/Double", "int"),
            Some("double")
        );
        assert_eq!(
            promoted_numeric_result_type_name("java/lang/Integer", "int"),
            Some("int")
        );
        assert_eq!(
            promoted_integral_result_type_name("java/lang/Integer", "int"),
            Some("int")
        );
        assert_eq!(
            promoted_integral_result_type_name("java/lang/Long", "int"),
            Some("long")
        );
        assert_eq!(
            promoted_shift_result_type_name("java/lang/Long", "int"),
            Some("long")
        );
        assert_eq!(
            promoted_shift_result_type_name("java/lang/Integer", "long"),
            Some("int")
        );
        assert_eq!(
            promoted_unary_integral_result_type_name("java/lang/Integer"),
            Some("int")
        );
    }

    #[test]
    fn test_type_compatibility_identity_widening_unboxing_and_incompatible() {
        let (view, _) = make_resolver();
        let resolver = TypeResolver::new(&view);

        assert_eq!(
            resolver
                .type_compatibility(&TypeName::new("double"), &TypeName::new("double"))
                .kind,
            ConversionKind::Identity
        );
        assert_eq!(
            resolver
                .type_compatibility(&TypeName::new("int"), &TypeName::new("double"))
                .kind,
            ConversionKind::WideningPrimitive
        );
        assert_eq!(
            resolver
                .type_compatibility(
                    &TypeName::new("java/lang/Integer"),
                    &TypeName::new("double")
                )
                .kind,
            ConversionKind::UnboxingWideningPrimitive
        );
        assert_eq!(
            resolver
                .type_compatibility(&TypeName::new("java/lang/String"), &TypeName::new("double"))
                .kind,
            ConversionKind::Incompatible
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
    fn test_overload_resolution_no_applicable_returns_none() {
        use crate::index::MethodSummary;
        use rust_asm::constants::ACC_PUBLIC;

        let noarg = MethodSummary {
            name: Arc::from("println"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };
        let one_string = MethodSummary {
            name: Arc::from("println"),
            params: MethodParams::from_method_descriptor("(Ljava/lang/String;)V"),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let candidates = vec![&noarg, &one_string];
        let args = vec![
            TypeName::new("java/lang/String"),
            TypeName::new("java/lang/String"),
            TypeName::new("java/lang/String"),
        ];
        assert!(
            resolver
                .select_overload_match(&candidates, 3, &args)
                .is_none(),
            "no applicable overload should yield None"
        );
    }

    #[test]
    fn test_varargs_applicability_zero_one_many_and_array_form() {
        use crate::index::MethodSummary;
        use rust_asm::constants::{ACC_PUBLIC, ACC_VARARGS};

        let varargs = MethodSummary {
            name: Arc::from("join"),
            params: MethodParams::from_method_descriptor("([Ljava/lang/String;)V"),
            annotations: vec![],
            access_flags: ACC_PUBLIC | ACC_VARARGS,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let candidates = vec![&varargs];

        let m0 = resolver
            .select_overload_match(&candidates, 0, &[])
            .expect("varargs zero args");
        assert_eq!(m0.mode, OverloadInvocationMode::Varargs);

        let m1 = resolver
            .select_overload_match(&candidates, 1, &[TypeName::new("java/lang/String")])
            .expect("varargs one arg");
        assert_eq!(m1.mode, OverloadInvocationMode::Varargs);

        let m3 = resolver
            .select_overload_match(
                &candidates,
                3,
                &[
                    TypeName::new("java/lang/String"),
                    TypeName::new("java/lang/String"),
                    TypeName::new("java/lang/String"),
                ],
            )
            .expect("varargs many args");
        assert_eq!(m3.mode, OverloadInvocationMode::Varargs);

        let arr = resolver
            .select_overload_match(&candidates, 1, &[TypeName::new("java/lang/String[]")])
            .expect("single-array form");
        assert_eq!(
            arr.mode,
            OverloadInvocationMode::Fixed,
            "single-array form should resolve as fixed invocation mode"
        );
    }

    #[test]
    fn test_overload_resolution_prefers_fixed_over_varargs_when_both_applicable() {
        use crate::index::MethodSummary;
        use rust_asm::constants::{ACC_PUBLIC, ACC_VARARGS};

        let fixed = MethodSummary {
            name: Arc::from("m"),
            params: MethodParams::from_method_descriptor("(Ljava/lang/String;)V"),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };
        let varargs = MethodSummary {
            name: Arc::from("m"),
            params: MethodParams::from_method_descriptor("([Ljava/lang/String;)V"),
            annotations: vec![],
            access_flags: ACC_PUBLIC | ACC_VARARGS,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let candidates = vec![&varargs, &fixed];
        let selected = resolver
            .select_overload_match(&candidates, 1, &[TypeName::new("java/lang/String")])
            .expect("resolved");
        assert_eq!(selected.method.desc().as_ref(), "(Ljava/lang/String;)V");
        assert_eq!(selected.mode, OverloadInvocationMode::Fixed);
    }

    #[test]
    fn test_varargs_trailing_expected_type_maps_to_element_descriptor() {
        use crate::index::MethodSummary;
        use rust_asm::constants::{ACC_PUBLIC, ACC_VARARGS};

        let method = MethodSummary {
            name: Arc::from("addAll"),
            params: MethodParams::from_method_descriptor("(I[Ljava/lang/String;)V"),
            annotations: vec![],
            access_flags: ACC_PUBLIC | ACC_VARARGS,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let desc = resolver
            .resolve_selected_param_descriptor_for_call(&method, 3, OverloadInvocationMode::Varargs)
            .expect("vararg trailing descriptor");
        assert_eq!(desc.as_ref(), "Ljava/lang/String;");
    }

    #[test]
    fn test_constructor_varargs_uses_same_shared_overload_matcher() {
        use crate::index::MethodSummary;
        use rust_asm::constants::{ACC_PUBLIC, ACC_VARARGS};

        let ctor_fixed = MethodSummary {
            name: Arc::from("<init>"),
            params: MethodParams::from_method_descriptor("(Ljava/lang/String;)V"),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };
        let ctor_varargs = MethodSummary {
            name: Arc::from("<init>"),
            params: MethodParams::from_method_descriptor("([Ljava/lang/String;)V"),
            annotations: vec![],
            access_flags: ACC_PUBLIC | ACC_VARARGS,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let candidates = vec![&ctor_varargs, &ctor_fixed];
        let one = resolver
            .select_overload_match(&candidates, 1, &[TypeName::new("java/lang/String")])
            .expect("constructor one arg");
        assert_eq!(one.method.desc().as_ref(), "(Ljava/lang/String;)V");
        let many = resolver
            .select_overload_match(
                &candidates,
                3,
                &[
                    TypeName::new("java/lang/String"),
                    TypeName::new("java/lang/String"),
                    TypeName::new("java/lang/String"),
                ],
            )
            .expect("constructor varargs");
        assert_eq!(many.method.desc().as_ref(), "([Ljava/lang/String;)V");
        assert_eq!(many.mode, OverloadInvocationMode::Varargs);
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
