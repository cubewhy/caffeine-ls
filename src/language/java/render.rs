use std::sync::Arc;

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

pub fn method_detail(
    receiver_internal: &str,
    class_meta: &ClassMetadata,
    method: &MethodSummary,
    provider: &impl SymbolProvider,
) -> String {
    let base_return = method.return_type.as_deref().unwrap_or("V");
    let display_return: Arc<str> = if let Some(sig) = method.generic_signature.as_deref() {
        if let Some(ret_idx) = sig.find(')') {
            let ret_jvm = &sig[ret_idx + 1..];
            substitute_type(
                receiver_internal,
                class_meta.generic_signature.as_deref(),
                ret_jvm,
            )
            .map(|t| t.0.clone())
            .unwrap_or_else(|| {
                JvmType::parse(ret_jvm)
                    .map(|(t, _)| Arc::from(t.to_signature_string()))
                    .unwrap_or_else(|| Arc::from(base_return))
            })
        } else {
            Arc::from(base_return)
        }
    } else {
        Arc::from(base_return)
    };

    let source_style_return = descriptor_to_source_type(&display_return, provider)
        .unwrap_or_else(|| display_return.to_string());

    let sig_to_use = method
        .generic_signature
        .clone()
        .unwrap_or_else(|| method.desc());

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
                .map(|t| t.0.clone())
                .unwrap_or_else(|| {
                    JvmType::parse(param_jvm_str)
                        .map(|(t, _)| Arc::from(t.to_signature_string()))
                        .unwrap_or_else(|| Arc::from("void"))
                });

                param_types.push(
                    descriptor_to_source_type(&subbed, provider)
                        .unwrap_or_else(|| subbed.to_string()),
                );
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
                .unwrap_or_else(|| Arc::from(format!("arg{}", i).as_str()));
            format!("{} {}", type_name, param_name)
        })
        .collect();

    // 3. 提取类名
    let base_class_name = receiver_internal
        .split('<')
        .next()
        .unwrap_or(receiver_internal);
    let short_class_name = base_class_name
        .rsplit('/')
        .next()
        .unwrap_or(base_class_name);

    format!(
        "{} — {} {}({})",
        short_class_name,
        source_style_return,
        method.name,
        full_params.join(", ")
    )
}

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
    .map(|t| t.0.clone())
    .unwrap_or_else(|| Arc::from(sig_to_use));

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

    // 安全网：专门处理 provider 无法解析 (返回 None) 的情况
    // 确保即使 provider 匹配失败，也不泄漏 L...; 或 / 等 JVM 内部特征
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

        // 1. 解析返回类型
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

        // 解析参数列表
        let mut param_types = Vec::new();
        if let Some(start) = sig.find('(')
            && let Some(end) = sig.find(')')
        {
            let mut params_str = &sig[start + 1..end];
            while !params_str.is_empty() {
                if let Some((t, rest)) = JvmType::parse(params_str) {
                    let param_jvm_str = &params_str[..params_str.len() - rest.len()];

                    let subbed: Arc<str> = Arc::from(t.to_signature_string());

                    param_types.push(
                        descriptor_to_source_type(&subbed, provider)
                            .unwrap_or_else(|| clean_fallback(param_jvm_str)),
                    );

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
                    .unwrap_or_else(|| Arc::from(format!("arg{}", i).as_str()));
                format!("{} {}", type_name, param_name)
            })
            .collect();

        format!(
            "{} — {} {}({})",
            short_class_name,
            source_style_return,
            member.name(),
            full_params.join(", ")
        )
    } else {
        // == 处理字段 ==
        let sig_to_use = member.descriptor();
        let display_type: Arc<str> = JvmType::parse(&sig_to_use)
            .map(|(t, _)| Arc::from(t.to_signature_string()))
            .unwrap_or_else(|| sig_to_use.clone());

        // 核心：优先信任 provider！
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
