use std::sync::Arc;

use tracing::instrument;

use crate::{
    index::{ClassMetadata, FieldSummary, MethodSummary},
    semantic::{
        context::CurrentClassMember,
        types::{
            SymbolProvider, descriptor_to_source_type,
            generics::{JvmType, substitute_type},
            signature_to_source_type,
            type_name::TypeName,
        },
    },
};

pub(crate) fn type_name_to_source_style(ty: &TypeName, provider: &impl SymbolProvider) -> String {
    if ty.is_intersection() {
        let mut rendered = ty
            .args
            .iter()
            .map(|bound| type_name_to_source_style(bound, provider))
            .collect::<Vec<_>>()
            .join(" & ");
        if ty.array_dims > 0 {
            rendered.push_str(&"[]".repeat(ty.array_dims));
        }
        return rendered;
    }

    let base = match ty.base_internal.as_ref() {
        "byte" | "char" | "double" | "float" | "int" | "long" | "short" | "boolean" | "void" => {
            ty.base_internal.to_string()
        }
        "*" => "?".to_string(),
        "+" => {
            if let Some(inner) = ty.args.first() {
                format!("? extends {}", type_name_to_source_style(inner, provider))
            } else {
                "?".to_string()
            }
        }
        "-" => {
            if let Some(inner) = ty.args.first() {
                format!("? super {}", type_name_to_source_style(inner, provider))
            } else {
                "?".to_string()
            }
        }
        internal => provider
            .resolve_source_name(internal)
            .unwrap_or_else(|| internal.replace('/', ".")),
    };

    let mut rendered = if ty.args.is_empty() || matches!(ty.base_internal.as_ref(), "+" | "-") {
        base
    } else {
        let rendered_args: Vec<String> = ty
            .args
            .iter()
            .map(|a| type_name_to_source_style(a, provider))
            .collect();
        format!("{base}<{}>", rendered_args.join(", "))
    };

    if ty.array_dims > 0 {
        rendered.push_str(&"[]".repeat(ty.array_dims));
    }
    rendered
}

pub fn local_variable_detail(
    local_name: &str,
    local_type: &TypeName,
    provider: &impl SymbolProvider,
) -> String {
    format!(
        "{} : {}",
        local_name,
        type_name_to_source_style(local_type, provider)
    )
}

fn render_param_type_for_display(method: &MethodSummary, idx: usize, rendered: String) -> String {
    let is_last = idx + 1 == method.params.items.len();
    if !(method.is_varargs() && is_last) {
        return rendered;
    }
    if let Some(base) = rendered.strip_suffix("[]") {
        return format!("{base}...");
    }
    rendered
}

fn short_class_name(receiver_internal: &str) -> &str {
    let base_class_name = receiver_internal
        .split('<')
        .next()
        .unwrap_or(receiver_internal);
    base_class_name
        .rsplit('/')
        .next()
        .unwrap_or(base_class_name)
}

fn clean_fallback_jvm_sig(jvm_sig: &str) -> String {
    let mut array_dims = 0;
    let mut base = jvm_sig.trim();
    while base.starts_with('[') {
        array_dims += 1;
        base = &base[1..];
    }
    let type_name = match base {
        "B" => "byte",
        "C" => "char",
        "D" => "double",
        "F" => "float",
        "I" => "int",
        "J" => "long",
        "S" => "short",
        "Z" => "boolean",
        "V" => "void",
        _ if base.starts_with('L') && base.ends_with(';') => &base[1..base.len() - 1],
        _ => base,
    };
    let source_type = type_name.replace('/', ".");
    let source_type = source_type.replace('$', ".");
    format!("{}{}", source_type, "[]".repeat(array_dims))
}

fn method_signature_to_use(method: &MethodSummary) -> Arc<str> {
    method
        .generic_signature
        .clone()
        .unwrap_or_else(|| method.desc())
}

fn render_source_param_types(
    method: &MethodSummary,
    provider: &impl SymbolProvider,
) -> Vec<String> {
    let sig_to_use = method_signature_to_use(method);
    let mut param_types = Vec::new();

    if let Some(start) = sig_to_use.find('(')
        && let Some(end) = sig_to_use.find(')')
    {
        let mut params_str = &sig_to_use[start + 1..end];
        while !params_str.is_empty() {
            if let Some((_, rest)) = JvmType::parse(params_str) {
                let param_jvm_str = &params_str[..params_str.len() - rest.len()];
                let rendered = signature_to_source_type(param_jvm_str, provider)
                    .or_else(|| descriptor_to_source_type(param_jvm_str, provider))
                    .unwrap_or_else(|| clean_fallback_jvm_sig(param_jvm_str));
                param_types.push(rendered);
                params_str = rest;
            } else {
                break;
            }
        }
    }

    param_types
        .into_iter()
        .enumerate()
        .map(|(i, type_name)| render_param_type_for_display(method, i, type_name))
        .collect()
}

fn render_substituted_param_types(
    receiver_internal: &str,
    class_meta: &ClassMetadata,
    method: &MethodSummary,
    provider: &impl SymbolProvider,
) -> Vec<String> {
    let sig_to_use = method_signature_to_use(method);
    let mut param_types = Vec::new();

    if let Some(start) = sig_to_use.find('(')
        && let Some(end) = sig_to_use.find(')')
    {
        let mut params_str = &sig_to_use[start + 1..end];
        while !params_str.is_empty() {
            if let Some((_, rest)) = JvmType::parse(params_str) {
                let param_jvm_str = &params_str[..params_str.len() - rest.len()];
                let substituted = substitute_type(
                    receiver_internal,
                    class_meta.generic_signature.as_deref(),
                    param_jvm_str,
                );
                let rendered = substituted
                    .as_ref()
                    .map(|t| type_name_to_source_style(t, provider))
                    .or_else(|| signature_to_source_type(param_jvm_str, provider))
                    .or_else(|| descriptor_to_source_type(param_jvm_str, provider))
                    .unwrap_or_else(|| clean_fallback_jvm_sig(param_jvm_str));
                param_types.push(rendered);
                params_str = rest;
            } else {
                break;
            }
        }
    }

    param_types
        .into_iter()
        .enumerate()
        .map(|(i, type_name)| render_param_type_for_display(method, i, type_name))
        .collect()
}

fn render_source_return_type(method: &MethodSummary, provider: &impl SymbolProvider) -> String {
    let base_return = method.return_type.as_deref().unwrap_or("V");
    let ret_jvm = method
        .generic_signature
        .as_deref()
        .and_then(|sig| sig.find(')').map(|i| &sig[i + 1..]))
        .unwrap_or(base_return);

    signature_to_source_type(ret_jvm, provider)
        .or_else(|| descriptor_to_source_type(ret_jvm, provider))
        .unwrap_or_else(|| clean_fallback_jvm_sig(ret_jvm))
}

#[instrument(skip(class_meta, method, provider))]
pub fn method_label(
    receiver_internal: &str,
    class_meta: &ClassMetadata,
    method: &MethodSummary,
    provider: &impl SymbolProvider,
) -> String {
    let rendered_params =
        render_substituted_param_types(receiver_internal, class_meta, method, provider);
    format!("{}({})", method.name, rendered_params.join(", "))
}

pub fn source_method_label(method: &MethodSummary, provider: &impl SymbolProvider) -> String {
    let rendered_params = render_source_param_types(method, provider);
    format!("{}({})", method.name, rendered_params.join(", "))
}

#[instrument(skip(class_meta, method, provider))]
pub fn method_detail(
    receiver_internal: &str,
    class_meta: &ClassMetadata,
    method: &MethodSummary,
    provider: &impl SymbolProvider,
) -> String {
    let base_return = method.return_type.as_deref().unwrap_or("V");

    let ret_jvm: &str = method
        .generic_signature
        .as_deref()
        .and_then(|sig| sig.find(')').map(|i| &sig[i + 1..]))
        .unwrap_or(base_return);

    let substituted_return = substitute_type(
        receiver_internal,
        class_meta.generic_signature.as_deref(),
        ret_jvm,
    );
    let source_style_return = substituted_return
        .as_ref()
        .map(|t| type_name_to_source_style(t, provider))
        .or_else(|| signature_to_source_type(ret_jvm, provider))
        .or_else(|| descriptor_to_source_type(ret_jvm, provider))
        .unwrap_or_else(|| clean_fallback_jvm_sig(ret_jvm));

    let param_types =
        render_substituted_param_types(receiver_internal, class_meta, method, provider);

    let full_params: Vec<String> = param_types
        .into_iter()
        .enumerate()
        .map(|(i, type_name)| {
            let param_name = method
                .params
                .param_names()
                .get(i)
                .cloned()
                .unwrap_or_else(|| Arc::<str>::from(format!("arg{}", i)));
            format!("{} {}", type_name, param_name)
        })
        .collect();

    let short_class_name = short_class_name(receiver_internal);

    let detail = format!(
        "{} — {} {}({})",
        short_class_name,
        source_style_return,
        method.name,
        full_params.join(", ")
    );

    detail
}

#[instrument(skip(class_meta, field, provider))]
pub fn field_detail(
    receiver_internal: &str,
    class_meta: &ClassMetadata,
    field: &FieldSummary,
    provider: &impl SymbolProvider,
) -> String {
    let sig_to_use = field
        .generic_signature
        .as_deref()
        .unwrap_or(&field.descriptor);

    let display_type = substitute_type(
        receiver_internal,
        class_meta.generic_signature.as_deref(),
        sig_to_use,
    )
    .map(|t| Arc::from(t.to_jvm_signature()))
    .unwrap_or_else(|| Arc::from(sig_to_use));

    tracing::debug!(?class_meta.generic_signature);

    let source_style_type = descriptor_to_source_type(&display_type, provider)
        .unwrap_or_else(|| display_type.to_string());

    let base_class_name = receiver_internal
        .split('<')
        .next()
        .unwrap_or(receiver_internal);
    let short_class_name = base_class_name
        .rsplit('/')
        .next()
        .unwrap_or(base_class_name);

    format!(
        "{} — {} : {}",
        short_class_name, field.name, source_style_type
    )
}

pub fn source_member_detail(
    receiver_internal: &str,
    member: &CurrentClassMember,
    provider: &impl SymbolProvider,
) -> String {
    let short_class_name = short_class_name(receiver_internal);

    if let CurrentClassMember::Method(md) = member {
        let md = md.clone();
        let source_style_return = render_source_return_type(&md, provider);
        let param_types = render_source_param_types(&md, provider);

        let full_params: Vec<String> = param_types
            .into_iter()
            .enumerate()
            .map(|(i, type_name)| {
                let param_name = md // method not found
                    .params
                    .param_names()
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| Arc::<str>::from(format!("arg{}", i)));
                format!("{} {}", type_name, param_name)
            })
            .collect();

        let detail = format!(
            "{} — {} {}({})",
            short_class_name,
            source_style_return,
            member.name(),
            full_params.join(", ")
        );
        detail
    } else {
        let sig_to_use = member.descriptor();
        let display_type: Arc<str> = JvmType::parse(&sig_to_use)
            .map(|(t, _)| Arc::from(t.to_signature_string()))
            .unwrap_or_else(|| sig_to_use.clone());

        let source_style_type = descriptor_to_source_type(&display_type, provider)
            .unwrap_or_else(|| clean_fallback_jvm_sig(&sig_to_use));

        format!(
            "{} — {} : {}",
            short_class_name,
            member.name(),
            source_style_type
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ClassMetadata, ClassOrigin, MethodParam, MethodParams};
    use rust_asm::constants::{ACC_PUBLIC, ACC_VARARGS};

    struct TestProvider;

    impl SymbolProvider for TestProvider {
        fn resolve_source_name(&self, internal_name: &str) -> Option<String> {
            Some(internal_name.replace('/', "."))
        }
    }

    fn empty_meta(internal: &str) -> ClassMetadata {
        ClassMetadata {
            package: Some(Arc::from("org/example")),
            name: Arc::from("Demo"),
            internal_name: Arc::from(internal),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    #[test]
    fn test_method_detail_renders_true_varargs_with_ellipsis() {
        let method = MethodSummary {
            name: Arc::from("printf"),
            params: MethodParams {
                items: vec![
                    MethodParam {
                        descriptor: Arc::from("Ljava/lang/String;"),
                        name: Arc::from("format"),
                        annotations: vec![],
                    },
                    MethodParam {
                        descriptor: Arc::from("[Ljava/lang/Object;"),
                        name: Arc::from("args"),
                        annotations: vec![],
                    },
                ],
            },
            annotations: vec![],
            access_flags: ACC_PUBLIC | ACC_VARARGS,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };

        let detail = method_detail(
            "java/io/PrintStream",
            &empty_meta("java/io/PrintStream"),
            &method,
            &TestProvider,
        );
        assert!(detail.contains("java.lang.Object... args"), "{detail}");
    }

    #[test]
    fn test_method_detail_keeps_plain_array_when_not_varargs() {
        let method = MethodSummary {
            name: Arc::from("takeArray"),
            params: MethodParams {
                items: vec![MethodParam {
                    descriptor: Arc::from("[Ljava/lang/Object;"),
                    name: Arc::from("args"),
                    annotations: vec![],
                }],
            },
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        };

        let detail = method_detail(
            "org/example/Demo",
            &empty_meta("org/example/Demo"),
            &method,
            &TestProvider,
        );
        assert!(detail.contains("java.lang.Object[] args"), "{detail}");
        assert!(!detail.contains("..."), "{detail}");
    }
}
