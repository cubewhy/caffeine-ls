use std::sync::Arc;

use tracing::instrument;

use crate::{
    index::{ClassMetadata, FieldSummary, MethodSummary},
    semantic::{
        context::CurrentClassMember,
        types::{
            SymbolProvider, descriptor_to_source_type,
            generics::{JvmType, substitute_type},
        },
    },
};

fn should_trace_method(
    receiver_internal: &str,
    class_meta: &ClassMetadata,
    method: &MethodSummary,
) -> bool {
    method.name.as_ref() == "add"
        && (receiver_internal.contains("ArrayList")
            || class_meta.internal_name.contains("ArrayList"))
}

#[instrument(skip(class_meta, method, provider))]
pub fn method_detail(
    receiver_internal: &str,
    class_meta: &ClassMetadata,
    method: &MethodSummary,
    provider: &impl SymbolProvider,
) -> String {
    let trace_add = should_trace_method(receiver_internal, class_meta, method);
    if trace_add {
        tracing::debug!(
            receiver_internal,
            base_receiver = receiver_internal.split('<').next().unwrap_or(receiver_internal),
            class_internal = %class_meta.internal_name,
            class_origin = ?class_meta.origin,
            class_generic_signature = ?class_meta.generic_signature,
            method_name = %method.name,
            method_desc = %method.desc(),
            method_generic_signature = ?method.generic_signature,
            method_return_type = ?method.return_type,
            param_descriptors = ?method.params.items.iter().map(|p| p.descriptor.as_ref()).collect::<Vec<_>>(),
            param_names = ?method.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
            "method_detail: input metadata"
        );
    }

    let base_return = method.return_type.as_deref().unwrap_or("V");

    let ret_jvm: &str = method
        .generic_signature
        .as_deref()
        .and_then(|sig| sig.find(')').map(|i| &sig[i + 1..]))
        .unwrap_or(base_return);

    if trace_add {
        tracing::debug!(base_return, ret_jvm, "method_detail: return type selection");
    }

    let mut display_return: Arc<str> = substitute_type(
        receiver_internal,
        class_meta.generic_signature.as_deref(),
        ret_jvm,
    )
    .map(|t| Arc::from(t.to_jvm_signature()))
    .unwrap_or_else(|| Arc::from(ret_jvm));

    if display_return.starts_with('T') && display_return.ends_with(';') && display_return.len() >= 3
    {
        display_return = Arc::from(&display_return[1..display_return.len() - 1]);
    }

    let source_style_return = descriptor_to_source_type(&display_return, provider)
        .unwrap_or_else(|| display_return.to_string());

    let sig_to_use = method
        .generic_signature
        .clone()
        .unwrap_or_else(|| method.desc());

    if trace_add {
        tracing::debug!(
            sig_to_use = %sig_to_use,
            display_return = %display_return,
            source_style_return,
            "method_detail: signature chosen"
        );
    }

    let mut param_types = Vec::new();

    if let Some(start) = sig_to_use.find('(')
        && let Some(end) = sig_to_use.find(')')
    {
        let mut params_str = &sig_to_use[start + 1..end];
        while !params_str.is_empty() {
            if let Some((_, rest)) = JvmType::parse(params_str) {
                let param_jvm_str = &params_str[..params_str.len() - rest.len()];
                let subbed = substitute_type(
                    receiver_internal,
                    class_meta.generic_signature.as_deref(),
                    param_jvm_str,
                )
                .map(|t| Arc::from(t.to_jvm_signature()))
                .unwrap_or_else(|| {
                    JvmType::parse(param_jvm_str)
                        .map(|(t, _)| Arc::from(t.to_signature_string()))
                        .unwrap_or_else(|| Arc::from(param_jvm_str))
                });

                let rendered = descriptor_to_source_type(&subbed, provider)
                    .unwrap_or_else(|| subbed.to_string());
                if trace_add {
                    tracing::debug!(
                        param_raw = param_jvm_str,
                        param_substituted = %subbed,
                        param_rendered = rendered,
                        "method_detail: parameter substitution"
                    );
                }
                param_types.push(rendered);
                params_str = rest;
            } else {
                break;
            }
        }
    }

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

    let base_class_name = receiver_internal
        .split('<')
        .next()
        .unwrap_or(receiver_internal);
    let short_class_name = base_class_name
        .rsplit('/')
        .next()
        .unwrap_or(base_class_name);

    let detail = format!(
        "{} — {} {}({})",
        short_class_name,
        source_style_return,
        method.name,
        full_params.join(", ")
    );

    if trace_add {
        tracing::debug!(detail, "method_detail: final detail");
    }

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
    let base_class_name = receiver_internal
        .split('<')
        .next()
        .unwrap_or(receiver_internal);
    let short_class_name = base_class_name
        .rsplit('/')
        .next()
        .unwrap_or(base_class_name);

    let clean_fallback = |jvm_sig: &str| -> String {
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
        let source_type = source_type.replace('$', "."); // 处理内部类 Map$Entry -> Map.Entry
        format!("{}{}", source_type, "[]".repeat(array_dims))
    };

    if let CurrentClassMember::Method(md) = member {
        let md = md.clone();
        let sig = member.descriptor();
        let trace_add = member.name().as_ref() == "add" && receiver_internal.contains("ArrayList");
        if trace_add {
            tracing::debug!(
                receiver_internal,
                method_name = %member.name(),
                method_desc = %sig,
                param_descriptors = ?md.params.items.iter().map(|p| p.descriptor.as_ref()).collect::<Vec<_>>(),
                param_names = ?md.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                "source_member_detail: input metadata"
            );
        }

        let ret_jvm = if let Some(ret_idx) = sig.find(')') {
            &sig[ret_idx + 1..]
        } else {
            "V"
        };

        let display_return: Arc<str> = JvmType::parse(ret_jvm)
            .map(|(t, _)| Arc::from(t.to_signature_string()))
            .unwrap_or_else(|| Arc::from(ret_jvm));

        let source_style_return = descriptor_to_source_type(&display_return, provider)
            .unwrap_or_else(|| clean_fallback(ret_jvm));

        let mut param_types = Vec::new();
        if let Some(start) = sig.find('(')
            && let Some(end) = sig.find(')')
        {
            let mut params_str = &sig[start + 1..end];
            while !params_str.is_empty() {
                if let Some((t, rest)) = JvmType::parse(params_str) {
                    let param_jvm_str = &params_str[..params_str.len() - rest.len()];

                    let subbed: Arc<str> = Arc::from(t.to_signature_string());

                    let rendered = descriptor_to_source_type(&subbed, provider)
                        .unwrap_or_else(|| clean_fallback(param_jvm_str));
                    if trace_add {
                        tracing::debug!(
                            param_raw = param_jvm_str,
                            param_substituted = %subbed,
                            param_rendered = rendered,
                            "source_member_detail: parameter rendering"
                        );
                    }
                    param_types.push(rendered);

                    params_str = rest;
                } else {
                    break;
                }
            }
        }

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
        if trace_add {
            tracing::debug!(detail, "source_member_detail: final detail");
        }
        detail
    } else {
        let sig_to_use = member.descriptor();
        let display_type: Arc<str> = JvmType::parse(&sig_to_use)
            .map(|(t, _)| Arc::from(t.to_signature_string()))
            .unwrap_or_else(|| sig_to_use.clone());

        let source_style_type = descriptor_to_source_type(&display_type, provider)
            .unwrap_or_else(|| clean_fallback(&sig_to_use));

        format!(
            "{} — {} : {}",
            short_class_name,
            member.name(),
            source_style_type
        )
    }
}
