use std::sync::Arc;

use rust_asm::constants::ACC_PRIVATE;

use crate::{
    completion::{
        context::CurrentClassMember,
        type_resolver::{
            SymbolProvider, descriptor_to_source_type,
            generics::{JvmType, substitute_type},
        },
    },
    index::{ClassMetadata, FieldSummary, MethodSummary},
};

#[derive(Debug, Clone, Copy)]
pub struct AccessFilter {
    pub hide_private: bool,
    pub hide_synthetic: bool,
}

impl AccessFilter {
    /// Member completion (external access): hidden private + synthetic
    pub fn member_completion() -> Self {
        Self {
            hide_private: true,
            hide_synthetic: true,
        }
    }

    /// Similar access: Only hide synthetic; private/protected are visible.
    pub fn same_class() -> Self {
        Self {
            hide_private: false,
            hide_synthetic: true,
        }
    }

    /// Loose Mode
    pub fn permissive() -> Self {
        Self {
            hide_private: true,
            hide_synthetic: false,
        }
    }

    /// Determines if a method is accessible
    // - `access_flags`: Access flags from the bytecode
    // - `is_synthetic`: From the Synthetic property (MethodSummary.is_synthetic)
    pub fn is_method_accessible(&self, access_flags: u16, is_synthetic: bool) -> bool {
        // private exclude directly
        if self.hide_private && (access_flags & ACC_PRIVATE != 0) {
            return false;
        }

        // Only trust the Synthetic attribute, not the ACC_SYNTHETIC flag
        // ACC_SYNTHETIC (0x1000) is added to normal methods in the Kotlin compilation artifacts
        // The Synthetic attribute is only present in the compiler's actual internal artifacts
        if self.hide_synthetic && is_synthetic {
            return false;
        }
        true
    }

    /// Determines if a field is accessible
    // - `access_flags`: Access flags from the bytecode
    // - `is_synthetic`: From the Synthetic property (MethodSummary.is_synthetic)
    pub fn is_field_accessible(&self, access_flags: u16, is_synthetic: bool) -> bool {
        if self.hide_private && (access_flags & ACC_PRIVATE != 0) {
            return false;
        }
        if self.hide_synthetic && is_synthetic {
            return false;
        }
        true
    }
}

pub struct Scorer {
    query: String,
}

impl Scorer {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
        }
    }

    pub fn score(&self, candidate: &super::candidate::CompletionCandidate) -> f32 {
        let mut score = 0.0f32;
        score += self.prefix_score(candidate.label.as_ref());
        score += self.kind_base_score(&candidate.kind);
        score
    }

    fn prefix_score(&self, label: &str) -> f32 {
        if self.query.is_empty() {
            return 20.0;
        }
        let q = self.query.to_lowercase();
        let l = label.to_lowercase();

        if l == q {
            return 100.0;
        }
        if l.starts_with(&q) {
            return 80.0;
        }

        // CamelCase First Letter Matching
        let initials = camel_initials(label);
        if initials.starts_with(&q) {
            return 60.0;
        }

        if l.contains(&q) {
            return 40.0;
        }
        if is_subsequence(&q, &l) {
            return 20.0;
        }
        0.0
    }

    fn kind_base_score(&self, kind: &super::candidate::CandidateKind) -> f32 {
        use super::candidate::CandidateKind::*;
        match kind {
            LocalVariable { .. } => 30.0,
            Method { .. } => 20.0,
            Field { .. } => 18.0,
            Constructor { .. } => 15.0,
            StaticMethod { .. } => 12.0,
            StaticField { .. } => 10.0,
            ClassName => 8.0,
            Package => 6.0,
            Keyword => 5.0,
            Annotation => 3.0,
            Snippet => 2.0,
            NameSuggestion => 1.0,
        }
    }
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut it = haystack.chars();
    'outer: for nc in needle.chars() {
        loop {
            match it.next() {
                Some(hc) if hc == nc => continue 'outer,
                Some(_) => {}
                None => return false,
            }
        }
    }
    true
}

pub fn method_detail(
    receiver_internal: &str,
    class_meta: &ClassMetadata,
    method: &MethodSummary,
    provider: &impl SymbolProvider,
) -> String {
    // 1. 处理返回类型
    let base_return = method.return_type.as_deref().unwrap_or("void");
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

    // 2. 处理参数列表
    let sig_to_use = method
        .generic_signature
        .as_deref()
        .unwrap_or(&method.descriptor);

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

/// Extract the first letter (lowercase) of each word in camelCase naming
/// "getValue" → "gv"
/// "getValueName" → "gvn"
/// "HTMLParser" → "hp" (consecutive uppercase letters are treated as one word)
fn camel_initials(label: &str) -> String {
    let mut result = String::new();
    let mut prev_was_upper = false;
    for (i, ch) in label.char_indices() {
        if i == 0 {
            result.push(ch.to_ascii_lowercase());
            prev_was_upper = ch.is_uppercase();
        } else if ch.is_uppercase() && !prev_was_upper {
            // New words begin
            result.push(ch.to_ascii_lowercase());
            prev_was_upper = true;
        } else {
            prev_was_upper = ch.is_uppercase();
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::completion::candidate::{CandidateKind, CompletionCandidate};

    fn make_candidate(label: &str, kind: CandidateKind) -> CompletionCandidate {
        CompletionCandidate::new(Arc::from(label), label.to_string(), kind, "test")
    }

    #[test]
    fn test_exact_match_highest() {
        let scorer = Scorer::new("get");
        let exact = make_candidate(
            "get",
            CandidateKind::Method {
                descriptor: Arc::from("()V"),
                defining_class: Arc::from("Foo"),
            },
        );
        let partial = make_candidate(
            "getValue",
            CandidateKind::Method {
                descriptor: Arc::from("()V"),
                defining_class: Arc::from("Foo"),
            },
        );
        assert!(scorer.score(&exact) > scorer.score(&partial));
    }

    #[test]
    fn test_prefix_beats_contains() {
        let scorer = Scorer::new("val");
        let prefix = make_candidate(
            "value",
            CandidateKind::Field {
                descriptor: Arc::from("I"),
                defining_class: Arc::from("Foo"),
            },
        );
        let contains = make_candidate(
            "getVal",
            CandidateKind::Field {
                descriptor: Arc::from("I"),
                defining_class: Arc::from("Foo"),
            },
        );
        assert!(scorer.score(&prefix) > scorer.score(&contains));
    }

    #[test]
    fn test_local_var_beats_method() {
        let scorer = Scorer::new("list");
        let local = make_candidate(
            "list",
            CandidateKind::LocalVariable {
                type_descriptor: Arc::from("Ljava/util/List;"),
            },
        );
        let method = make_candidate(
            "list",
            CandidateKind::Method {
                descriptor: Arc::from("()V"),
                defining_class: Arc::from("Foo"),
            },
        );
        assert!(scorer.score(&local) > scorer.score(&method));
    }

    #[test]
    fn test_no_match_zero_prefix_score() {
        let scorer = Scorer::new("xyz");
        let c = make_candidate(
            "getValue",
            CandidateKind::Method {
                descriptor: Arc::from("()V"),
                defining_class: Arc::from("Foo"),
            },
        );
        // prefix_score should be 0, only kind_base_score remains.
        let score = scorer.score(&c);
        assert!(score <= 20.0 + 1e-3);
    }

    #[test]
    fn test_access_filter_synthetic_attr_hides() {
        let filter = AccessFilter::member_completion();
        // is_synthetic=true (from the Synthetic attribute) -> Hide
        assert!(!filter.is_method_accessible(0x0001, true));
    }

    #[test]
    fn test_access_filter_acc_synthetic_flag_kept() {
        let filter = AccessFilter::member_completion();
        // The ACC_SYNTHETIC flag is present, but is_synthetic=false (no Synthetic attribute) → Reserved
        assert!(filter.is_method_accessible(0x1001, false));
    }

    #[test]
    fn test_access_filter_private_hides() {
        let filter = AccessFilter::member_completion();
        assert!(!filter.is_method_accessible(0x0002, false)); // ACC_PRIVATE
    }

    #[test]
    fn test_access_filter_field_synthetic() {
        let filter = AccessFilter::member_completion();
        assert!(!filter.is_field_accessible(0x1010, true));
        assert!(filter.is_field_accessible(0x0001, false));
    }

    #[test]
    fn test_camel_case_initials() {
        let scorer = Scorer::new("gv");
        let c = make_candidate(
            "getValue",
            CandidateKind::Method {
                descriptor: Arc::from("()V"),
                defining_class: Arc::from("Foo"),
            },
        );
        // "gV" matches the first letter of the camel case, deserves 60 points.
        assert!(scorer.score(&c) >= 60.0);
    }
}
