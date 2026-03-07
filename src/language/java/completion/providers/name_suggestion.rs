use crate::{
    completion::{CandidateKind, CompletionCandidate, provider::CompletionProvider},
    index::{IndexScope, IndexView},
    language::java::completion::providers::name_suggestion::rules::{
        BASE_RULES, ParsedType, pluralize,
    },
    semantic::context::{CursorLocation, SemanticContext},
};
use std::sync::Arc;

pub mod rules;

pub struct NameSuggestionProvider;

impl CompletionProvider for NameSuggestionProvider {
    fn name(&self) -> &'static str {
        "name_suggestion"
    }

    fn provide(
        &self,
        _scope: IndexScope,
        ctx: &SemanticContext,
        _index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        let type_name = match &ctx.location {
            CursorLocation::VariableName { type_name } => type_name.as_str(),
            _ => return vec![],
        };

        if type_name.is_empty() {
            return vec![];
        }

        let suggestions = generate_name_suggestions(type_name);
        suggestions
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                // Higher index = lower score so first suggestion ranks highest
                let score = 100.0 - i as f32;
                CompletionCandidate::new(
                    Arc::from(name.as_str()),
                    name,
                    CandidateKind::NameSuggestion,
                    self.name(),
                )
                .with_detail(format!("{} variable name", type_name))
                .with_score(score)
            })
            .collect()
    }
}

/// Generate variable name suggestions from a type name.
/// e.g. "StringBuilder" → ["sb", "builder", "stringBuilder", "StringBuilder"]  
pub fn generate_name_suggestions(type_name: &str) -> Vec<String> {
    let parsed = ParsedType::parse(type_name);

    if parsed.simple_name.is_empty() {
        return vec![];
    }

    let mut base_suggestions = vec![];
    for rule in BASE_RULES {
        if let Some(suggestions) = rule(&parsed) {
            base_suggestions = suggestions;
            break;
        }
    }

    if parsed.is_array {
        base_suggestions = base_suggestions
            .into_iter()
            .flat_map(|s| {
                let mut variants = vec![format!("{}Arr", s)];

                if s.len() <= 1 {
                    variants.push(s);
                } else {
                    variants.push(pluralize(&s));
                }

                variants
            })
            .collect();
    }

    let mut results: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for s in base_suggestions {
        if !s.is_empty() && is_valid_identifier(&s) && seen.insert(s.clone()) {
            results.push(s);
        }
    }

    results
}

fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        None => false,
        Some(c) => {
            (c.is_alphabetic() || c == '_') && chars.all(|c| c.is_alphanumeric() || c == '_')
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::WorkspaceIndex;
    use crate::index::{IndexScope, ModuleId};
    use crate::semantic::context::{CursorLocation, SemanticContext};

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn ctx(type_name: &str) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::VariableName {
                type_name: type_name.to_string(),
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        )
    }

    #[test]
    fn test_string_builder_suggestions() {
        let idx = WorkspaceIndex::new();
        let results = NameSuggestionProvider.provide(
            root_scope(),
            &ctx("StringBuilder"),
            &idx.view(root_scope()),
        );
        let names: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(
            names.contains(&"sb"),
            "should suggest acronym 'sb': {:?}",
            names
        );
        assert!(
            names.contains(&"builder"),
            "should suggest last word 'builder': {:?}",
            names
        );
        assert!(
            names.contains(&"stringBuilder"),
            "should suggest lowerCamel: {:?}",
            names
        );
    }

    #[test]
    fn test_http_servlet_request() {
        let suggestions = generate_name_suggestions("HttpServletRequest");
        assert!(
            suggestions.contains(&"hsr".to_string()),
            "{:?}",
            suggestions
        );
        assert!(
            suggestions.contains(&"request".to_string()),
            "{:?}",
            suggestions
        );
        assert!(
            suggestions.contains(&"httpServletRequest".to_string()),
            "{:?}",
            suggestions
        );
    }

    #[test]
    fn test_simple_type_string() {
        let suggestions = generate_name_suggestions("String");
        // "s" as acronym, "string" as lower camel, short name lowercased
        assert!(!suggestions.is_empty(), "{:?}", suggestions);
        assert!(
            suggestions.contains(&"string".to_string()) || suggestions.contains(&"s".to_string()),
            "{:?}",
            suggestions
        );
    }

    #[test]
    fn test_array_type_stripped() {
        let suggestions = generate_name_suggestions("String[]");
        // Should treat as "String"
        assert!(!suggestions.is_empty());
        assert!(suggestions.iter().all(|s| !s.contains('[')));
    }

    #[test]
    fn test_internal_name_uses_simple() {
        // "java/util/ArrayList" → suggestions based on "ArrayList"
        let suggestions = generate_name_suggestions("java/util/ArrayList");
        assert!(
            suggestions.contains(&"al".to_string())
                || suggestions.contains(&"arrayList".to_string()),
            "{:?}",
            suggestions
        );
    }

    #[test]
    fn test_no_duplicates() {
        // "List" → acronym "l", lower camel "list", short "list" — "list" should appear once
        let suggestions = generate_name_suggestions("List");
        let unique: std::collections::HashSet<_> = suggestions.iter().collect();
        assert_eq!(
            suggestions.len(),
            unique.len(),
            "no duplicates: {:?}",
            suggestions
        );
    }

    #[test]
    fn test_empty_type_returns_empty() {
        let idx = WorkspaceIndex::new();
        let results =
            NameSuggestionProvider.provide(root_scope(), &ctx(""), &idx.view(root_scope()));
        assert!(results.is_empty());
    }

    #[test]
    fn test_generics_list() {
        let suggestions = generate_name_suggestions("List<User>");
        let names: Vec<&str> = suggestions.iter().map(|s| s.as_str()).collect();
        assert!(names.contains(&"users"), "Should suggest 'users'");
        assert!(names.contains(&"userList"), "Should suggest 'userList'");
    }

    #[test]
    fn test_generics_optional() {
        let suggestions = generate_name_suggestions("Optional<User>");
        let names: Vec<&str> = suggestions.iter().map(|s| s.as_str()).collect();
        assert!(names.contains(&"user"), "Should suggest 'user'");
        assert!(names.contains(&"userOpt"), "Should suggest 'userOpt'");
    }

    #[test]
    fn test_generics_with_packages() {
        let suggestions = generate_name_suggestions("java.util.List<com.example.Entity>");
        let names: Vec<&str> = suggestions.iter().map(|s| s.as_str()).collect();
        assert!(names.contains(&"entities"));
        assert!(names.contains(&"entityList"));
    }

    #[test]
    fn test_keyword_rule_base_and_array() {
        // Base Keyword
        let base_sugg = generate_name_suggestions("Class");
        assert!(base_sugg.contains(&"clazz".to_string()));
        assert!(!base_sugg.contains(&"class".to_string()));

        // Array Keyword (clazz -> clazzes, clazzArr)
        let arr_sugg = generate_name_suggestions("Class[]");
        assert!(arr_sugg.contains(&"clazzes".to_string()));
        assert!(arr_sugg.contains(&"clazzArr".to_string()));
    }

    #[test]
    fn test_array_transform_rules() {
        let suggestions = generate_name_suggestions("String[]");
        let names: Vec<&str> = suggestions.iter().map(|s| s.as_str()).collect();
        assert!(names.contains(&"strings"));
        assert!(names.contains(&"stringArr"));

        // acronym_of("String") -> "s"
        // 长度为1，不再变形为 "ses" 或 "ss"，直接保留 "s" 和 "sArr"
        assert!(names.contains(&"s"));
        assert!(names.contains(&"sArr"));
        assert!(
            !names.contains(&"ses"),
            "Should not pluralize single letter acronyms"
        );
    }
}
