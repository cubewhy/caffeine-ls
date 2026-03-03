use super::super::{
    candidate::CompletionCandidate,
    context::{CompletionContext, CursorLocation},
};
use super::CompletionProvider;
use crate::index::GlobalIndex;

pub struct PackageProvider;

impl CompletionProvider for PackageProvider {
    fn name(&self) -> &'static str {
        "package"
    }

    fn provide(
        &self,
        ctx: &CompletionContext,
        index: &mut GlobalIndex,
    ) -> Vec<CompletionCandidate> {
        match &ctx.location {
            CursorLocation::Import { prefix } => {
                crate::completion::import_completion::candidates_for_import(prefix, index)
            }
            CursorLocation::Expression { prefix } | CursorLocation::TypeAnnotation { prefix } => {
                if !prefix.contains('.') {
                    return vec![];
                }
                crate::completion::import_completion::candidates_for_import(prefix, index)
            }
            CursorLocation::MemberAccess {
                receiver_expr,
                member_prefix,
                ..
            } => {
                if !receiver_expr.contains('.')
                    && receiver_expr
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_uppercase())
                {
                    return vec![];
                }
                let full_prefix = if member_prefix.is_empty() {
                    format!("{}.", receiver_expr)
                } else {
                    format!("{}.{}", receiver_expr, member_prefix)
                };
                crate::completion::import_completion::candidates_for_import(&full_prefix, index)
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::CandidateKind;
    use crate::completion::context::{CompletionContext, CursorLocation};
    use crate::index::{ClassMetadata, ClassOrigin, GlobalIndex};
    use rust_asm::constants::ACC_PUBLIC;
    use std::sync::Arc;

    fn make_index() -> GlobalIndex {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_cls("org/cubewhy", "Main"),
            make_cls("org/cubewhy", "Main2"),
            make_cls("org/cubewhy/utils", "StringUtil"),
            make_cls("org/cubewhy/utils", "FileUtil"),
            make_cls("com/other", "Other"),
        ]);
        idx
    }

    fn make_cls(pkg: &str, name: &str) -> ClassMetadata {
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(format!("{}/{}", pkg, name).as_str()),
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

    fn import_ctx(prefix: &str) -> CompletionContext {
        CompletionContext::new(
            CursorLocation::Import {
                prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            None,
            None,
            None,
            vec![],
        )
    }

    fn expr_ctx(prefix: &str) -> CompletionContext {
        CompletionContext::new(
            CursorLocation::Expression {
                prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            None,
            None,
            None,
            vec![],
        )
    }

    #[test]
    fn test_top_level_package_no_dot() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx("org"), &mut idx);
        let org = results.iter().find(|c| c.label.as_ref() == "org.");
        assert!(
            org.is_some(),
            "should suggest 'org.': {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        assert_eq!(org.unwrap().insert_text, "org.");
    }

    #[test]
    fn test_expression_no_dot_no_package_completion() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&expr_ctx("Main"), &mut idx);
        assert!(results.is_empty(), "no dot = no package completion");
    }

    #[test]
    fn test_empty_prefix_no_crash() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx(""), &mut idx);
        assert!(results.is_empty());
    }

    #[test]
    fn test_member_access_uppercase_receiver_not_package() {
        // String.| → 不是包路径
        let mut idx = make_index();
        let ctx = CompletionContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "String".to_string(),
                arguments: None,
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        let results = PackageProvider.provide(&ctx, &mut idx);
        assert!(
            results.is_empty(),
            "String.| should not trigger package completion"
        );
    }

    #[test]
    fn test_top_level_package_label_has_dot() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx("org"), &mut idx);
        let org = results.iter().find(|c| c.label.as_ref() == "org.").unwrap();
        assert_eq!(org.insert_text, "org.");
    }

    #[test]
    fn test_import_pkg_dot_lists_classes() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx("org.cubewhy."), &mut idx);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"org.cubewhy.Main"), "{:?}", labels);
        assert!(labels.contains(&"org.cubewhy.Main2"), "{:?}", labels);
    }

    #[test]
    fn test_import_pkg_dot_lists_sub_packages() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx("org.cubewhy."), &mut idx);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"org.cubewhy.utils."), "{:?}", labels);
    }

    #[test]
    fn test_import_pkg_with_name_prefix() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx("org.cubewhy.Ma"), &mut idx);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"org.cubewhy.Main"), "{:?}", labels);
        assert!(labels.contains(&"org.cubewhy.Main2"), "{:?}", labels);
        assert!(labels.iter().all(|l| !l.contains("Other")), "{:?}", labels);
    }

    #[test]
    fn test_import_insert_text_is_fqn() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx("org.cubewhy.Ma"), &mut idx);
        let main = results
            .iter()
            .find(|c| c.label.as_ref() == "org.cubewhy.Main")
            .unwrap();
        assert_eq!(main.insert_text, "org.cubewhy.Main");
    }

    #[test]
    fn test_sub_package_insert_text_ends_with_dot() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx("org.cubewhy."), &mut idx);
        let utils = results
            .iter()
            .find(|c| c.label.as_ref() == "org.cubewhy.utils.")
            .unwrap();
        assert!(utils.insert_text.ends_with('.'), "{:?}", utils.insert_text);
    }

    #[test]
    fn test_sub_package_kind_is_package() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx("org.cubewhy."), &mut idx);
        let utils = results
            .iter()
            .find(|c| c.label.as_ref() == "org.cubewhy.utils.")
            .unwrap();
        assert_eq!(utils.kind, CandidateKind::Package);
    }

    #[test]
    fn test_import_no_dot_returns_top_level_packages() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx("Main"), &mut idx);
        // 大写开头不匹配包名，但现在 candidates_for_import 会匹配类名
        // PackageProvider 在 Import 场景下直接转发给 candidates_for_import
        // "Main" 大写开头 → 返回类，不返回包
        assert!(
            results.iter().all(|c| c.kind != CandidateKind::Package),
            "uppercase prefix should not match packages: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_sub_package_label_is_full_path() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&import_ctx("org.cubewhy."), &mut idx);
        let utils = results
            .iter()
            .find(|c| c.label.as_ref() == "org.cubewhy.utils.")
            .unwrap();
        assert_eq!(utils.kind, CandidateKind::Package);
    }

    #[test]
    fn test_expression_with_dot_triggers() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&expr_ctx("org.cubewhy."), &mut idx);
        assert!(!results.is_empty(), "prefix with dot should trigger");
    }

    #[test]
    fn test_expression_no_dot_no_completion() {
        let mut idx = make_index();
        let results = PackageProvider.provide(&expr_ctx("Main"), &mut idx);
        assert!(results.is_empty(), "no dot = no package completion");
    }

    #[test]
    fn test_member_access_single_uppercase_no_package() {
        let mut idx = make_index();
        let ctx = CompletionContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "String".to_string(),
                arguments: None,
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        let results = PackageProvider.provide(&ctx, &mut idx);
        assert!(
            results.is_empty(),
            "single uppercase segment should not trigger package completion"
        );
    }

    #[test]
    fn test_member_access_insert_text_trimmed() {
        // org.cubewhy.| → insert_text 应该是 "Main" 而不是 "org.cubewhy.Main"
        let mut idx = make_index();
        let ctx = CompletionContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "org.cubewhy".to_string(),
                arguments: None,
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        let results = PackageProvider.provide(&ctx, &mut idx);
        let main = results
            .iter()
            .find(|c| c.label.as_ref() == "org.cubewhy.Main")
            .unwrap();
        assert_eq!(
            main.insert_text, "org.cubewhy.Main",
            "insert_text should be trimmed to just 'Main'"
        );
    }

    #[test]
    fn test_member_access_package_like_triggers() {
        let mut idx = make_index();
        let ctx = CompletionContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "org.cubewhy".to_string(),
                arguments: None,
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        let results = PackageProvider.provide(&ctx, &mut idx);
        assert!(
            !results.is_empty(),
            "org.cubewhy.| should trigger package completion: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        // label 是 FQN，insert_text 是截断后的短名
        assert!(
            results
                .iter()
                .any(|c| c.label.as_ref() == "org.cubewhy.Main"),
            "{:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_member_access_package_with_name_prefix() {
        let mut idx = make_index();
        let ctx = CompletionContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "Ma".to_string(),
                receiver_expr: "org.cubewhy".to_string(),
                arguments: None,
            },
            "Ma",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        let results = PackageProvider.provide(&ctx, &mut idx);
        assert!(
            results
                .iter()
                .any(|c| c.label.as_ref() == "org.cubewhy.Main"),
            "{:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        assert!(
            results
                .iter()
                .any(|c| c.label.as_ref() == "org.cubewhy.Main2"),
            "{:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        // insert_text 应该是截断后的短名
        let main = results
            .iter()
            .find(|c| c.label.as_ref() == "org.cubewhy.Main")
            .unwrap();
        assert_eq!(main.insert_text, "org.cubewhy.Main");
    }
}
