use rust_asm::constants::ACC_PRIVATE;

use crate::index::IndexView;
use crate::semantic::context::SemanticContext;
use crate::semantic::types::type_name::TypeName;
use crate::semantic::types::{
    ConversionKind, TypeResolver, parse_single_type_to_internal, singleton_descriptor_to_type,
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

pub struct Scorer<'idx> {
    query: String,
    expected_type: Option<TypeName>,
    resolver: Option<TypeResolver<'idx>>,
}

impl<'idx> Scorer<'idx> {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            expected_type: None,
            resolver: None,
        }
    }

    pub fn with_expected_type(
        query: impl Into<String>,
        ctx: &SemanticContext,
        index: &'idx IndexView,
    ) -> Self {
        Self {
            query: query.into(),
            expected_type: ctx
                .typed_expr_ctx
                .as_ref()
                .and_then(|typed| typed.expected_type.as_ref().map(|e| e.ty.clone())),
            resolver: Some(TypeResolver::new(index)),
        }
    }

    pub fn score(&self, candidate: &super::candidate::CompletionCandidate) -> f32 {
        let mut score = 0.0f32;
        let match_text = candidate
            .insertion
            .filter_text
            .as_deref()
            .unwrap_or(candidate.label.as_ref());
        score += self.prefix_score(match_text);
        score += self.kind_base_score(&candidate.kind);
        score += self.compatibility_score(candidate);
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
            StatementLabel => 5.5,
            Keyword => 5.0,
            Annotation => 3.0,
            Snippet => 2.0,
            NameSuggestion => 1.0,
        }
    }

    fn compatibility_score(&self, candidate: &super::candidate::CompletionCandidate) -> f32 {
        let (Some(expected), Some(resolver)) = (&self.expected_type, &self.resolver) else {
            return 0.0;
        };
        let Some(source_ty) = candidate_value_type(candidate) else {
            return 0.0;
        };
        let compatibility = resolver.type_compatibility(&source_ty, expected);
        match compatibility.kind {
            ConversionKind::Identity => 180.0,
            ConversionKind::WideningPrimitive => 140.0,
            ConversionKind::UnboxingWideningPrimitive => 120.0,
            ConversionKind::Unboxing | ConversionKind::Boxing => 110.0,
            ConversionKind::BoxingWideningPrimitive => 90.0,
            ConversionKind::ReferenceWidening => 70.0,
            ConversionKind::NullToReference => 50.0,
            ConversionKind::Unknown => 0.0,
            ConversionKind::NarrowingPrimitive => -80.0,
            ConversionKind::Incompatible => -120.0,
        }
    }
}

fn candidate_value_type(candidate: &super::candidate::CompletionCandidate) -> Option<TypeName> {
    use super::candidate::CandidateKind;
    match &candidate.kind {
        CandidateKind::LocalVariable { type_descriptor } => descriptor_or_type(type_descriptor),
        CandidateKind::Field { descriptor, .. } | CandidateKind::StaticField { descriptor, .. } => {
            descriptor_or_type(descriptor)
        }
        CandidateKind::Method { descriptor, .. }
        | CandidateKind::StaticMethod { descriptor, .. } => method_return_type(descriptor),
        CandidateKind::Constructor { defining_class, .. } => {
            Some(TypeName::new(defining_class.clone()))
        }
        CandidateKind::ClassName
        | CandidateKind::Package
        | CandidateKind::StatementLabel
        | CandidateKind::Snippet
        | CandidateKind::Keyword
        | CandidateKind::Annotation
        | CandidateKind::NameSuggestion => None,
    }
}

fn method_return_type(descriptor: &str) -> Option<TypeName> {
    let idx = descriptor.find(')')?;
    descriptor_or_type(&descriptor[idx + 1..])
}

fn descriptor_or_type(raw: &str) -> Option<TypeName> {
    parse_single_type_to_internal(raw)
        .or_else(|| singleton_descriptor_to_type(raw).map(TypeName::new))
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
    use crate::index::{IndexScope, ModuleId, WorkspaceIndex};
    use crate::semantic::SemanticContext;
    use crate::semantic::context::{
        CursorLocation, ExpectedType, ExpectedTypeConfidence, ExpectedTypeSource,
        TypedExpressionContext,
    };
    use crate::semantic::types::type_name::TypeName;

    fn make_candidate(label: &str, kind: CandidateKind) -> CompletionCandidate {
        CompletionCandidate::new(Arc::from(label), label.to_string(), kind, "test")
    }

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
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

    #[test]
    fn test_expected_type_semantic_ranking_prefers_conversion_compatibility() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "fo".to_string(),
            },
            "fo",
            vec![],
            None,
            None,
            None,
            vec![],
        )
        .with_typed_expression_context(Some(TypedExpressionContext {
            expected_type: Some(ExpectedType {
                ty: TypeName::new("double"),
                source: ExpectedTypeSource::VariableInitializer,
                confidence: ExpectedTypeConfidence::Exact,
            }),
            receiver_type: None,
            functional_compat: None,
        }));

        let scorer = Scorer::with_expected_type("fo", &ctx, &view);
        let c_double = make_candidate(
            "foo",
            CandidateKind::Method {
                descriptor: Arc::from("()D"),
                defining_class: Arc::from("A"),
            },
        );
        let c_int = make_candidate(
            "fooInt",
            CandidateKind::Method {
                descriptor: Arc::from("()I"),
                defining_class: Arc::from("A"),
            },
        );
        let c_integer = make_candidate(
            "fooInteger",
            CandidateKind::Method {
                descriptor: Arc::from("()Ljava/lang/Integer;"),
                defining_class: Arc::from("A"),
            },
        );
        let c_string = make_candidate(
            "fooString",
            CandidateKind::Method {
                descriptor: Arc::from("()Ljava/lang/String;"),
                defining_class: Arc::from("A"),
            },
        );

        let s_double = scorer.score(&c_double);
        let s_int = scorer.score(&c_int);
        let s_integer = scorer.score(&c_integer);
        let s_string = scorer.score(&c_string);

        assert!(s_double > s_int, "exact should rank above widening");
        assert!(
            s_int > s_integer,
            "widening primitive should rank above unboxing+widening"
        );
        assert!(
            s_integer > s_string,
            "compatible primitive-wrapper should rank above incompatible reference"
        );
    }
}
