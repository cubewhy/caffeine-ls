use std::collections::HashSet;
use std::sync::Arc;

use rust_asm::constants::{ACC_FINAL, ACC_INTERFACE, ACC_PRIVATE, ACC_STATIC, ACC_TRANSIENT};

use crate::{
    completion::{
        CandidateKind, CompletionCandidate,
        candidate::ReplacementMode,
        provider::{CompletionProvider, ProviderCompletionResult},
    },
    index::{ClassMetadata, FieldSummary, IndexScope, IndexView, MethodSummary},
    semantic::{
        context::{CurrentClassMember, CursorLocation, SemanticContext},
        types::ContextualResolver,
    },
};

pub struct OverrideProvider;

const JAVA_LANG_OBJECT_INTERNAL: &str = "java/lang/Object";
const THROW_NOT_IMPLEMENTED_BODY: &str = r#"throw new RuntimeException("Not implemented yet");"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverrideBodyStrategy {
    InterfaceThrow,
    SuperCall,
    ObjectEquals,
    ObjectHashCode,
    ObjectToString,
}

#[derive(Debug, Clone)]
struct ParamBinding {
    ty: String,
    name: String,
}

impl CompletionProvider for OverrideProvider {
    fn name(&self) -> &'static str {
        "override"
    }

    fn is_applicable(&self, ctx: &SemanticContext) -> bool {
        ctx.is_class_member_position && matches!(&ctx.location, CursorLocation::Expression { .. })
    }

    fn provide(
        &self,
        scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
        _request: Option<&crate::lsp::request_context::RequestContext>,
        _limit: Option<usize>,
    ) -> crate::lsp::request_cancellation::RequestResult<ProviderCompletionResult> {
        if !ctx.is_class_member_position {
            return Ok(ProviderCompletionResult::default());
        }

        let prefix = match &ctx.location {
            CursorLocation::Expression { prefix } => prefix.as_str(),
            _ => return Ok(ProviderCompletionResult::default()),
        };

        if !is_access_modifier_prefix(prefix) {
            return Ok(ProviderCompletionResult::default());
        }

        let enclosing = match ctx.enclosing_internal_name.as_deref() {
            Some(e) => e,
            None => return Ok(ProviderCompletionResult::default()),
        };

        // A collection of (name, descriptor) methods that have been overridden in the current class
        // current_class_members is a HashMap<name, member>, which only keeps the last overridden method
        // Therefore, it additionally retrieves the current class's own methods from the index for precise deduplication.
        let already_overridden: HashSet<(Arc<str>, Arc<str>)> =
            self.collect_current_class_methods(enclosing, index, scope);

        // (name, descriptor) already appearing in this candidate list, to avoid the same method appearing repeatedly from multiple ancestors.
        let mut emitted: HashSet<(Arc<str>, Arc<str>)> = HashSet::new();

        let mro = index.mro(enclosing);
        let mut candidates = Vec::new();
        let resolver = ContextualResolver::new(index, ctx);
        let current_class_fields = self.collect_current_class_fields(enclosing, ctx, index);
        let current_class_name = self.current_class_name(enclosing, ctx, index);

        for (i, class_meta) in mro.iter().enumerate() {
            if i == 0 {
                continue;
            }
            for method in &class_meta.methods {
                if !is_overridable(method) {
                    continue;
                }
                let key = (Arc::clone(&method.name), method.desc());
                if already_overridden.contains(&key) {
                    continue;
                }

                // Source-level member
                let candidate_param_count = method.params.len();
                let blocked_by_source = ctx.current_class_member_list.iter().any(|m| {
                    if !m.is_method() || m.name() != method.name {
                        return false;
                    }
                    // bad descriptor ast
                    if m.descriptor().is_empty() {
                        return true;
                    }
                    crate::semantic::types::count_params(&m.descriptor()) == candidate_param_count
                });

                if blocked_by_source {
                    continue;
                }
                if !emitted.insert(key) {
                    // It has been generated from a more recent ancestor.
                    continue;
                }

                let Some((params_source, return_type_source)) =
                    crate::semantic::types::parse_strict_method_signature(
                        &method.desc(),
                        &resolver,
                    )
                else {
                    continue;
                };

                let candidate = build_candidate(
                    method,
                    &return_type_source,
                    &params_source,
                    class_meta.as_ref(),
                    &current_class_fields,
                    &current_class_name,
                    ctx,
                    index,
                    self.name(),
                );
                candidates.push(candidate);
            }
        }

        Ok(candidates.into())
    }
}

impl OverrideProvider {
    /// Accurately collect the (name, descriptor) members already present in the current class:
    // Prioritize index (if the current file has been compiled into index),
    // Then overlay current_class_members (members resolved at the source level).
    fn collect_current_class_methods(
        &self,
        enclosing: &str,
        index: &IndexView,
        _scope: IndexScope,
    ) -> HashSet<(Arc<str>, Arc<str>)> {
        let mut set: HashSet<(Arc<str>, Arc<str>)> = HashSet::new();
        if let Some(meta) = index.get_class(enclosing) {
            for m in &meta.methods {
                set.insert((Arc::clone(&m.name), m.desc()));
            }
        }
        set
    }

    fn collect_current_class_fields(
        &self,
        enclosing: &str,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<FieldSummary> {
        let source_fields: Vec<FieldSummary> = ctx
            .current_class_member_list
            .iter()
            .filter_map(|member| match member {
                CurrentClassMember::Field(field) => Some((**field).clone()),
                CurrentClassMember::Method(_) => None,
            })
            .collect();
        if !source_fields.is_empty() {
            return source_fields;
        }

        index
            .get_class(enclosing)
            .map(|meta| meta.fields.to_vec())
            .unwrap_or_default()
    }

    fn current_class_name(
        &self,
        enclosing: &str,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> String {
        if let Some(name) = ctx.enclosing_class.as_deref() {
            return name.to_string();
        }
        if let Some(meta) = index.get_class(enclosing) {
            return meta.direct_name().to_string();
        }
        // TODO: return None instead using heuristics
        enclosing
            .rsplit(['/', '$'])
            .next()
            .unwrap_or(enclosing)
            .to_string()
    }
}

fn is_overridable(method: &MethodSummary) -> bool {
    // Constructor / Static Initialization Block
    if matches!(method.name.as_ref(), "<init>" | "<clinit>") {
        return false;
    }
    // Compiler-generated synthesis method
    if method.is_synthetic {
        return false;
    }
    // private, cannot be overridden
    if method.access_flags & ACC_PRIVATE != 0 {
        return false;
    }
    // Static methods can only be hidden, not overridden.
    if method.access_flags & ACC_STATIC != 0 {
        return false;
    }
    // final cannot be overridden
    if method.access_flags & ACC_FINAL != 0 {
        return false;
    }
    true
}

/// The current input prefix is ​​a valid start (at least 2 characters) of "public" or "protected".
fn is_access_modifier_prefix(prefix: &str) -> bool {
    let p = prefix.trim();
    if p.len() < 2 {
        return false;
    }
    "public".starts_with(p) || "protected".starts_with(p)
}

#[allow(clippy::too_many_arguments)]
fn build_candidate(
    method: &MethodSummary,
    return_type_source: &str,
    params_source: &[String],
    defining_class: &ClassMetadata,
    current_class_fields: &[FieldSummary],
    current_class_name: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    source: &'static str,
) -> CompletionCandidate {
    use rust_asm::constants::{ACC_PROTECTED, ACC_PUBLIC};

    let visibility = if method.access_flags & ACC_PUBLIC != 0 {
        "public"
    } else if method.access_flags & ACC_PROTECTED != 0 {
        "protected"
    } else {
        // package-private
        ""
    };

    let body_strategy = classify_body_strategy(method, defining_class);
    let param_bindings = build_param_bindings(method, params_source, body_strategy);
    let params_str = render_param_declarations(&param_bindings);

    let label_text = format!(
        "{} {} {}({})",
        visibility, return_type_source, method.name, params_str
    )
    .trim()
    .to_string();

    let body = build_method_body(
        body_strategy,
        method,
        &param_bindings,
        current_class_fields,
        current_class_name,
    );

    let vis_prefix = if visibility.is_empty() {
        String::new()
    } else {
        format!("{} ", visibility)
    };

    let insert_text = if ctx.is_followed_by_opener() {
        format!(
            "@Override\n{}{}  {}(",
            vis_prefix, return_type_source, method.name
        )
    } else {
        format!(
            "@Override\n{}{} {}({}) {{\n{}\n}}",
            vis_prefix,
            return_type_source,
            method.name,
            params_str,
            indent_block(&body, 4)
        )
    };

    // 查找展示名称，如果因为某些极端的并发原因没查到，做个兜底展示
    let defining_class_display = index
        .get_source_type_name(&defining_class.internal_name)
        .unwrap_or_else(|| defining_class.internal_name.replace(['/', '$'], "."));

    let detail = format!("@Override — {}", defining_class_display);

    CompletionCandidate::new(
        Arc::from(label_text.as_str()),
        insert_text,
        CandidateKind::Method {
            descriptor: method.desc(),
            defining_class: Arc::clone(&defining_class.internal_name),
        },
        source,
    )
    .with_replacement_mode(ReplacementMode::AccessModifierPrefix)
    .with_filter_text(label_text)
    .with_detail(detail)
    .with_score(65.0)
}

fn classify_body_strategy(
    method: &MethodSummary,
    defining_class: &ClassMetadata,
) -> OverrideBodyStrategy {
    let descriptor = method.desc();
    if defining_class.internal_name.as_ref() == JAVA_LANG_OBJECT_INTERNAL {
        return match (method.name.as_ref(), descriptor.as_ref()) {
            ("equals", "(Ljava/lang/Object;)Z") => OverrideBodyStrategy::ObjectEquals,
            ("hashCode", "()I") => OverrideBodyStrategy::ObjectHashCode,
            ("toString", "()Ljava/lang/String;") => OverrideBodyStrategy::ObjectToString,
            _ => OverrideBodyStrategy::SuperCall,
        };
    }

    if defining_class.access_flags & ACC_INTERFACE != 0 {
        OverrideBodyStrategy::InterfaceThrow
    } else {
        OverrideBodyStrategy::SuperCall
    }
}

fn build_param_bindings(
    method: &MethodSummary,
    params_source: &[String],
    body_strategy: OverrideBodyStrategy,
) -> Vec<ParamBinding> {
    params_source
        .iter()
        .enumerate()
        .map(|(i, ty)| ParamBinding {
            ty: ty.clone(),
            name: match body_strategy {
                OverrideBodyStrategy::ObjectEquals if i == 0 => "o".to_string(),
                _ => format!("arg{i}"),
            },
        })
        .take(method.params.len())
        .collect()
}

fn render_param_declarations(param_bindings: &[ParamBinding]) -> String {
    param_bindings
        .iter()
        .map(|param| format!("{} {}", param.ty, param.name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn build_method_body(
    body_strategy: OverrideBodyStrategy,
    method: &MethodSummary,
    param_bindings: &[ParamBinding],
    current_class_fields: &[FieldSummary],
    current_class_name: &str,
) -> String {
    match body_strategy {
        OverrideBodyStrategy::InterfaceThrow => THROW_NOT_IMPLEMENTED_BODY.to_string(),
        OverrideBodyStrategy::SuperCall => build_super_call_body(method, param_bindings),
        OverrideBodyStrategy::ObjectEquals => {
            build_equals_body(current_class_name, current_class_fields)
        }
        OverrideBodyStrategy::ObjectHashCode => build_hash_code_body(current_class_fields),
        OverrideBodyStrategy::ObjectToString => {
            build_to_string_body(current_class_name, current_class_fields)
        }
    }
}

fn build_super_call_body(method: &MethodSummary, param_bindings: &[ParamBinding]) -> String {
    let args = param_bindings
        .iter()
        .map(|param| param.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let call = format!("super.{}({args})", method.name);
    if method.return_type.is_some() {
        format!("return {call};")
    } else {
        format!("{call};")
    }
}

fn build_equals_body(current_class_name: &str, fields: &[FieldSummary]) -> String {
    let comparable_fields = select_equals_hash_code_fields(fields);
    let mut lines = vec![
        "if (this == o) return true;".to_string(),
        "if (o == null || getClass() != o.getClass()) return false;".to_string(),
    ];

    if comparable_fields.is_empty() {
        lines.push("return true;".to_string());
        return lines.join("\n");
    }

    lines.push(format!(
        "{current_class_name} other = ({current_class_name}) o;"
    ));

    let comparisons = comparable_fields
        .iter()
        .map(|field| field_equality_expr(field))
        .collect::<Vec<_>>();
    if comparisons.len() == 1 {
        lines.push(format!("return {};", comparisons[0]));
    } else {
        lines.push(format!("return {};", comparisons.join("\n        && ")));
    }

    lines.join("\n")
}

fn build_hash_code_body(fields: &[FieldSummary]) -> String {
    let comparable_fields = select_equals_hash_code_fields(fields);
    if comparable_fields.is_empty() {
        return "return 0;".to_string();
    }

    let mut lines = vec![format!(
        "int result = {};",
        field_hash_expr(comparable_fields[0])
    )];
    for field in comparable_fields.iter().skip(1) {
        lines.push(format!(
            "result = 31 * result + {};",
            field_hash_expr(field)
        ));
    }
    lines.push("return result;".to_string());
    lines.join("\n")
}

fn build_to_string_body(current_class_name: &str, fields: &[FieldSummary]) -> String {
    let printable_fields = select_to_string_fields(fields);
    if printable_fields.is_empty() {
        return format!("return \"{current_class_name}{{}}\";");
    }

    let mut lines = vec![format!("return \"{current_class_name}{{\" +")];
    for (i, field) in printable_fields.iter().enumerate() {
        let prefix = if i == 0 {
            format!("\"{}=\" + {}", field.name, field_to_string_expr(field))
        } else {
            format!("\", {}=\" + {}", field.name, field_to_string_expr(field))
        };
        lines.push(format!("        {prefix} +"));
    }
    lines.push("        '}';".to_string());
    lines.join("\n")
}

fn select_equals_hash_code_fields(fields: &[FieldSummary]) -> Vec<&FieldSummary> {
    fields
        .iter()
        .filter(|field| {
            !field.is_synthetic
                && field.access_flags & ACC_STATIC == 0
                && field.access_flags & ACC_TRANSIENT == 0
                && !field.name.as_ref().starts_with('$')
        })
        .collect()
}

fn select_to_string_fields(fields: &[FieldSummary]) -> Vec<&FieldSummary> {
    fields
        .iter()
        .filter(|field| {
            !field.is_synthetic
                && field.access_flags & ACC_STATIC == 0
                && !field.name.as_ref().starts_with('$')
        })
        .collect()
}

fn field_equality_expr(field: &FieldSummary) -> String {
    let left = format!("this.{}", field.name);
    let right = format!("other.{}", field.name);
    match descriptor_array_depth(&field.descriptor) {
        0 => match descriptor_base(&field.descriptor) {
            "F" => format!("Float.compare({left}, {right}) == 0"),
            "D" => format!("Double.compare({left}, {right}) == 0"),
            "B" | "C" | "I" | "J" | "S" | "Z" => format!("{left} == {right}"),
            _ => format!("java.util.Objects.equals({left}, {right})"),
        },
        1 => format!("java.util.Arrays.equals({left}, {right})"),
        _ => format!("java.util.Arrays.deepEquals({left}, {right})"),
    }
}

fn field_hash_expr(field: &FieldSummary) -> String {
    let access = format!("this.{}", field.name);
    match descriptor_array_depth(&field.descriptor) {
        0 => match descriptor_base(&field.descriptor) {
            "Z" => format!("({access} ? 1 : 0)"),
            "B" | "C" | "I" | "S" => access,
            "J" => format!("Long.hashCode({access})"),
            "F" => format!("Float.hashCode({access})"),
            "D" => format!("Double.hashCode({access})"),
            _ => format!("java.util.Objects.hashCode({access})"),
        },
        1 => format!("java.util.Arrays.hashCode({access})"),
        _ => format!("java.util.Arrays.deepHashCode({access})"),
    }
}

fn field_to_string_expr(field: &FieldSummary) -> String {
    let access = format!("this.{}", field.name);
    match descriptor_array_depth(&field.descriptor) {
        0 => access,
        1 => format!("java.util.Arrays.toString({access})"),
        _ => format!("java.util.Arrays.deepToString({access})"),
    }
}

fn descriptor_array_depth(descriptor: &str) -> usize {
    descriptor.bytes().take_while(|b| *b == b'[').count()
}

fn descriptor_base(descriptor: &str) -> &str {
    &descriptor[descriptor_array_depth(descriptor)..]
}

fn indent_block(block: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    block
        .lines()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::WorkspaceIndex;
    use crate::index::{
        ClassMetadata, ClassOrigin, FieldSummary, IndexScope, MethodParams, MethodSummary, ModuleId,
    };
    use crate::language::test_helpers::completion_context_from_marked_source;
    use crate::semantic::context::{CurrentClassMember, CursorLocation, SemanticContext};
    use crate::semantic::types::parse_return_type_from_descriptor;
    use rust_asm::constants::{ACC_PROTECTED, ACC_PUBLIC, ACC_STATIC, ACC_TRANSIENT};
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn method(name: &str, descriptor: &str, flags: u16) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            params: MethodParams::from_method_descriptor(descriptor),
            annotations: vec![],
            access_flags: flags,
            is_synthetic: false,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    fn synthetic_method(name: &str, descriptor: &str) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            params: MethodParams::from_method_descriptor(descriptor),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: true,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    fn field(name: &str, descriptor: &str, flags: u16) -> FieldSummary {
        FieldSummary {
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            access_flags: flags,
            annotations: vec![],
            is_synthetic: false,
            generic_signature: None,
        }
    }

    fn make_class(
        pkg: &str,
        name: &str,
        super_name: Option<&str>,
        methods: Vec<MethodSummary>,
    ) -> ClassMetadata {
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(format!("{}/{}", pkg, name).as_str()),
            super_name: super_name.map(Arc::from),
            interfaces: vec![],
            annotations: vec![],
            methods,
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    fn make_nested_class(
        pkg: &str,
        internal_name: &str,
        simple_name: &str,
        owner_internal: &str,
        super_name: Option<&str>,
        methods: Vec<MethodSummary>,
    ) -> ClassMetadata {
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(simple_name),
            internal_name: Arc::from(internal_name),
            super_name: super_name.map(Arc::from),
            interfaces: vec![],
            annotations: vec![],
            methods,
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: Some(Arc::from(owner_internal)),
            origin: ClassOrigin::Unknown,
        }
    }

    fn ctx_with_prefix(prefix: &str, enclosing_internal: &str) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::Expression {
                prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            Some(Arc::from(
                enclosing_internal.rsplit('/').next().unwrap_or(""),
            )),
            Some(Arc::from(enclosing_internal)),
            None,
            vec![],
        )
        .with_class_member_position(true)
    }

    fn ctx_from_marked_source(src_with_cursor: &str) -> SemanticContext {
        completion_context_from_marked_source("java", src_with_cursor, None)
    }

    #[test]
    fn test_prefix_public_triggers() {
        assert!(is_access_modifier_prefix("pu"));
        assert!(is_access_modifier_prefix("pub"));
        assert!(is_access_modifier_prefix("publ"));
        assert!(is_access_modifier_prefix("publi"));
        assert!(is_access_modifier_prefix("public"));
    }

    #[test]
    fn test_prefix_protected_triggers() {
        assert!(is_access_modifier_prefix("pr"));
        assert!(is_access_modifier_prefix("pro"));
        assert!(is_access_modifier_prefix("prot"));
        assert!(is_access_modifier_prefix("protected"));
    }

    #[test]
    fn test_prefix_too_short_no_trigger() {
        assert!(!is_access_modifier_prefix(""));
        assert!(!is_access_modifier_prefix("p")); // 单字符太模糊
    }

    #[test]
    fn test_unrelated_prefix_no_trigger() {
        assert!(!is_access_modifier_prefix("vo")); // void
        assert!(!is_access_modifier_prefix("pri")); // private
        assert!(!is_access_modifier_prefix("abc"));
    }

    #[test]
    fn test_full_word_with_space_no_trigger() {
        // "public void" cannot match "public".starts_with("public void")
        assert!(!is_access_modifier_prefix("public void"));
    }

    #[test]
    fn test_basic_override_from_superclass() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();

        assert!(
            labels.iter().any(|l| l.contains("doWork")),
            "doWork should appear as overridable: {:?}",
            labels
        );
    }

    #[test]
    fn test_insert_text_contains_override_annotation() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let candidate = results.iter().find(|c| c.label.contains("doWork")).unwrap();

        assert!(
            candidate.insert_text.contains("@Override"),
            "insert_text must contain @Override: {:?}",
            candidate.insert_text
        );
    }

    #[test]
    fn test_insert_text_contains_method_body_stub() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let c = results.iter().find(|c| c.label.contains("doWork")).unwrap();

        assert!(
            c.insert_text.contains("super.doWork();"),
            "superclass override should delegate to super: {}",
            c.insert_text
        );
    }

    #[test]
    fn test_already_overridden_excluded_via_index() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class(
                "com/example",
                "Child",
                Some("com/example/Parent"),
                // Child 已经 override doWork
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();

        assert!(
            labels.iter().all(|l| !l.contains("doWork")),
            "doWork already overridden, must not appear: {:?}",
            labels
        );
    }

    #[test]
    fn test_already_overridden_excluded_via_source_members() {
        // Child is not compiled into the index, but current_class_members has doWork.
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_class(
            "com/example",
            "Parent",
            None,
            vec![method("doWork", "()V", ACC_PUBLIC)],
        )]);
        // Child only exists at the source level (without add_classes)
        let child_meta = make_class("com/example", "Child", Some("com/example/Parent"), vec![]);
        // Manually let the index know the superclass of Child (otherwise the MRO cannot find the Parent).
        idx.add_classes(vec![child_meta.clone()]);

        let source_member = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("doWork"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let ctx = ctx_with_prefix("pub", "com/example/Child")
            .with_class_members(std::iter::once(source_member));

        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(
            labels.iter().all(|l| !l.contains("doWork")),
            "doWork in source members must be excluded: {:?}",
            labels
        );
    }

    #[test]
    fn test_overloads_both_shown_when_none_overridden() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            make_class(
                "com/example",
                "Parent",
                None,
                vec![
                    method("compute", "(I)I", ACC_PUBLIC),
                    method("compute", "(Ljava/lang/String;)I", ACC_PUBLIC),
                ],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let compute_count = results
            .iter()
            .filter(|c| c.label.contains("compute"))
            .count();
        assert_eq!(
            compute_count,
            2,
            "both overloads should appear: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_overloads_only_unoverridden_shown() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            make_class(
                "com/example",
                "Parent",
                None,
                vec![
                    method("compute", "(I)I", ACC_PUBLIC),
                    method("compute", "(Ljava/lang/String;)I", ACC_PUBLIC),
                ],
            ),
            make_class(
                "com/example",
                "Child",
                Some("com/example/Parent"),
                // 只 override 了 int 版本
                vec![method("compute", "(I)I", ACC_PUBLIC)],
            ),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let compute: Vec<_> = results
            .iter()
            .filter(|c| c.label.contains("compute"))
            .collect();

        assert_eq!(
            compute.len(),
            1,
            "only unoverridden overload should remain: {:?}",
            compute.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        // 剩下的应该是 String 参数版本
        assert!(
            compute[0].insert_text.contains("String"),
            "remaining overload should be String variant: {:?}",
            compute[0].insert_text
        );
    }

    #[test]
    fn test_private_method_not_overridable() {
        use rust_asm::constants::ACC_PRIVATE;
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("secret", "()V", ACC_PRIVATE)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().all(|c| !c.label.contains("secret")),
            "private method must not appear"
        );
    }

    #[test]
    fn test_static_method_not_overridable() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("staticFn", "()V", ACC_PUBLIC | ACC_STATIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().all(|c| !c.label.contains("staticFn")),
            "static method must not appear"
        );
    }

    #[test]
    fn test_final_method_not_overridable() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("locked", "()V", ACC_PUBLIC | ACC_FINAL)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().all(|c| !c.label.contains("locked")),
            "final method must not appear"
        );
    }

    #[test]
    fn test_synthetic_method_excluded() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![synthetic_method("access$000", "(Lcom/example/Parent;)V")],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().all(|c| !c.label.contains("access$")),
            "synthetic method must not appear"
        );
    }

    #[test]
    fn test_constructor_excluded() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("<init>", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().all(|c| !c.label.contains("<init>")),
            "<init> must not appear"
        );
    }

    #[test]
    fn test_no_enclosing_class_returns_empty() {
        let idx = WorkspaceIndex::new();
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "pub".to_string(),
            },
            "pub",
            vec![],
            None,
            None, // enclosing_internal_name = None
            None,
            vec![],
        );
        assert!(
            OverrideProvider
                .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
                .candidates
                .is_empty()
        );
    }

    #[test]
    fn test_protected_method_visibility_preserved() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("hook", "()V", ACC_PROTECTED)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pro", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let c = results.iter().find(|c| c.label.contains("hook")).unwrap();
        assert!(
            c.insert_text.contains("protected"),
            "protected visibility should be preserved: {:?}",
            c.insert_text
        );
    }

    #[test]
    fn test_grandparent_method_appears() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "GrandParent",
                None,
                vec![method("ancientMethod", "()V", ACC_PUBLIC)],
            ),
            make_class(
                "com/example",
                "Parent",
                Some("com/example/GrandParent"),
                vec![],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.contains("ancientMethod")),
            "grandparent method should be overridable: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_duplicate_from_multiple_ancestors() {
        // GrandParent 和 Parent 都声明了同一方法（Parent 没有 override，走继承）
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "GrandParent",
                None,
                vec![method("shared", "()V", ACC_PUBLIC)],
            ),
            make_class(
                "com/example",
                "Parent",
                Some("com/example/GrandParent"),
                vec![method("shared", "()V", ACC_PUBLIC)], // 重复声明（模拟 index 含两份）
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let count = results
            .iter()
            .filter(|c| c.label.contains("shared"))
            .count();
        assert_eq!(
            count,
            1,
            "same method must not appear twice: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_wrong_location_returns_empty() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "pub".to_string(),
                receiver_expr: "obj".to_string(),
                arguments: None,
            },
            "pub",
            vec![],
            Some(Arc::from("Child")),
            Some(Arc::from("com/example/Child")),
            None,
            vec![],
        );
        assert!(
            OverrideProvider
                .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
                .candidates
                .is_empty()
        );
    }

    #[test]
    fn test_object_methods_appear_when_no_explicit_superclass() {
        // parser normalizes implicit superclass to java/lang/Object
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            // Object 本身
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                annotations: vec![],
                super_name: None,
                interfaces: vec![],
                methods: vec![
                    method("toString", "()Ljava/lang/String;", ACC_PUBLIC),
                    method("equals", "(Ljava/lang/Object;)Z", ACC_PUBLIC),
                    method("hashCode", "()I", ACC_PUBLIC),
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            make_class("com/example", "Plain", Some("java/lang/Object"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Plain");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();

        assert!(
            labels.iter().any(|l| l.contains("toString")),
            "toString should appear: {:?}",
            labels
        );
        assert!(
            labels.iter().any(|l| l.contains("equals")),
            "equals should appear: {:?}",
            labels
        );
        assert!(
            labels.iter().any(|l| l.contains("hashCode")),
            "hashCode should appear: {:?}",
            labels
        );
    }

    #[test]
    fn test_object_methods_not_duplicated_when_already_in_mro() {
        // 如果 mro 里已经有 Object（通过显式继承链走到），不应重复
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                annotations: vec![],
                super_name: None,
                interfaces: vec![],
                methods: vec![method("toString", "()Ljava/lang/String;", ACC_PUBLIC)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            make_class("com/example", "Parent", Some("java/lang/Object"), vec![]),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let count = results
            .iter()
            .filter(|c| c.label.contains("toString"))
            .count();
        assert_eq!(
            count,
            1,
            "toString must appear exactly once: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    fn make_interface(pkg: &str, name: &str, methods: Vec<MethodSummary>) -> ClassMetadata {
        use rust_asm::constants::ACC_INTERFACE;
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(format!("{}/{}", pkg, name).as_str()),
            annotations: vec![],
            super_name: None,
            interfaces: vec![],
            methods,
            fields: vec![],
            // interface class 自身的 access_flags 带 ACC_INTERFACE，但不影响方法遍历
            access_flags: ACC_PUBLIC | ACC_INTERFACE,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    fn abstract_method(name: &str, descriptor: &str) -> MethodSummary {
        use rust_asm::constants::ACC_ABSTRACT;
        MethodSummary {
            name: Arc::from(name),
            annotations: vec![],
            params: MethodParams::from_method_descriptor(descriptor),
            access_flags: ACC_PUBLIC | ACC_ABSTRACT,
            is_synthetic: false,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    fn default_method(name: &str, descriptor: &str) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            params: MethodParams::from_method_descriptor(descriptor),
            annotations: vec![],
            access_flags: ACC_PUBLIC, // default method: public, non-abstract, non-static
            is_synthetic: false,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    #[test]
    fn test_interface_abstract_method_shown() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_interface(
            "com/example",
            "Runnable",
            vec![abstract_method("run", "()V")],
        )]);
        // 一个实现了 Runnable 但尚未实现 run() 的类
        let mut cls = make_class("com/example", "MyTask", None, vec![]);
        cls.interfaces = vec![Arc::from("com/example/Runnable")];
        idx.add_classes(vec![cls]);

        let ctx = ctx_with_prefix("pub", "com/example/MyTask");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(
            labels.iter().any(|l| l.contains("run")),
            "abstract interface method run() should appear: {:?}",
            labels
        );
    }

    #[test]
    fn test_interface_default_method_shown() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            make_interface(
                "com/example",
                "Greeter",
                vec![default_method("greet", "()Ljava/lang/String;")],
            ),
        ]);
        let mut cls = make_class("com/example", "HelloGreeter", None, vec![]);
        cls.interfaces = vec![Arc::from("com/example/Greeter")];
        idx.add_classes(vec![cls]);

        let ctx = ctx_with_prefix("pub", "com/example/HelloGreeter");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.contains("greet")),
            "default interface method should be overridable: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_non_void_superclass_method_returns_super_call() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                Some("java/lang/Object"),
                vec![method("answer", "()I", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let candidate = results
            .iter()
            .find(|c| c.label.contains("answer"))
            .expect("answer override candidate");

        assert!(
            candidate.insert_text.contains("return super.answer();"),
            "non-void superclass override should return super call: {}",
            candidate.insert_text
        );
    }

    #[test]
    fn test_interface_method_keeps_throw_stub() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_interface(
            "com/example",
            "Runnable",
            vec![abstract_method("run", "()V")],
        )]);
        let mut cls = make_class("com/example", "MyTask", Some("java/lang/Object"), vec![]);
        cls.interfaces = vec![Arc::from("com/example/Runnable")];
        idx.add_classes(vec![cls]);

        let ctx = ctx_with_prefix("pub", "com/example/MyTask");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let candidate = results
            .iter()
            .find(|c| c.label.contains("run"))
            .expect("run override candidate");

        assert!(
            candidate.insert_text.contains(THROW_NOT_IMPLEMENTED_BODY),
            "interface override should keep throw stub: {}",
            candidate.insert_text
        );
    }

    #[test]
    fn test_object_equals_uses_field_comparisons() {
        let idx = WorkspaceIndex::new();
        let mut object = make_class("java/lang", "Object", None, vec![]);
        object.methods = vec![method("equals", "(Ljava/lang/Object;)Z", ACC_PUBLIC)];
        let mut child = make_class("com/example", "Person", Some("java/lang/Object"), vec![]);
        child.fields = vec![
            field("id", "I", ACC_PUBLIC),
            field("name", "Ljava/lang/String;", ACC_PUBLIC),
            field("scores", "[I", ACC_PUBLIC),
            field("matrix", "[[I", ACC_PUBLIC),
            field("weight", "D", ACC_PUBLIC),
            field("ignored", "Ljava/lang/String;", ACC_STATIC),
            field("ephemeral", "Ljava/lang/String;", ACC_TRANSIENT),
        ];
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            object,
            child,
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Person");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let candidate = results
            .iter()
            .find(|c| c.label.contains("equals"))
            .expect("equals override candidate");

        assert!(
            candidate
                .insert_text
                .contains("if (this == o) return true;"),
            "equals should have identity guard: {}",
            candidate.insert_text
        );
        assert!(
            candidate.insert_text.contains("Person other = (Person) o;"),
            "equals should cast to current class: {}",
            candidate.insert_text
        );
        assert!(
            candidate.insert_text.contains("this.id == other.id"),
            "primitive field should use direct comparison: {}",
            candidate.insert_text
        );
        assert!(
            candidate
                .insert_text
                .contains("java.util.Objects.equals(this.name, other.name)"),
            "reference field should use Objects.equals: {}",
            candidate.insert_text
        );
        assert!(
            candidate
                .insert_text
                .contains("java.util.Arrays.equals(this.scores, other.scores)"),
            "1d arrays should use Arrays.equals: {}",
            candidate.insert_text
        );
        assert!(
            candidate
                .insert_text
                .contains("java.util.Arrays.deepEquals(this.matrix, other.matrix)"),
            "nested arrays should use Arrays.deepEquals: {}",
            candidate.insert_text
        );
        assert!(
            candidate
                .insert_text
                .contains("Double.compare(this.weight, other.weight) == 0"),
            "double field should use Double.compare: {}",
            candidate.insert_text
        );
        assert!(
            !candidate.insert_text.contains("ignored"),
            "static fields must be skipped: {}",
            candidate.insert_text
        );
        assert!(
            !candidate.insert_text.contains("ephemeral"),
            "transient fields must be skipped: {}",
            candidate.insert_text
        );
    }

    #[test]
    fn test_object_hash_code_uses_field_hashes() {
        let idx = WorkspaceIndex::new();
        let mut object = make_class("java/lang", "Object", None, vec![]);
        object.methods = vec![method("hashCode", "()I", ACC_PUBLIC)];
        let mut child = make_class("com/example", "Person", Some("java/lang/Object"), vec![]);
        child.fields = vec![
            field("id", "I", ACC_PUBLIC),
            field("name", "Ljava/lang/String;", ACC_PUBLIC),
            field("scores", "[I", ACC_PUBLIC),
            field("matrix", "[[I", ACC_PUBLIC),
            field("weight", "D", ACC_PUBLIC),
        ];
        idx.add_classes(vec![object, child]);

        let ctx = ctx_with_prefix("pub", "com/example/Person");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let candidate = results
            .iter()
            .find(|c| c.label.contains("hashCode"))
            .expect("hashCode override candidate");

        assert!(
            candidate.insert_text.contains("int result = this.id;"),
            "hashCode should seed from first field: {}",
            candidate.insert_text
        );
        assert!(
            candidate
                .insert_text
                .contains("result = 31 * result + java.util.Objects.hashCode(this.name);"),
            "reference fields should use Objects.hashCode: {}",
            candidate.insert_text
        );
        assert!(
            candidate
                .insert_text
                .contains("result = 31 * result + java.util.Arrays.hashCode(this.scores);"),
            "1d arrays should use Arrays.hashCode: {}",
            candidate.insert_text
        );
        assert!(
            candidate
                .insert_text
                .contains("result = 31 * result + java.util.Arrays.deepHashCode(this.matrix);"),
            "nested arrays should use Arrays.deepHashCode: {}",
            candidate.insert_text
        );
        assert!(
            candidate
                .insert_text
                .contains("result = 31 * result + Double.hashCode(this.weight);"),
            "double fields should use Double.hashCode: {}",
            candidate.insert_text
        );
    }

    #[test]
    fn test_object_to_string_uses_array_rendering() {
        let idx = WorkspaceIndex::new();
        let mut object = make_class("java/lang", "Object", None, vec![]);
        object.methods = vec![method("toString", "()Ljava/lang/String;", ACC_PUBLIC)];
        let mut child = make_class("com/example", "Person", Some("java/lang/Object"), vec![]);
        child.fields = vec![
            field("name", "Ljava/lang/String;", ACC_PUBLIC),
            field("scores", "[I", ACC_PUBLIC),
            field("matrix", "[[I", ACC_PUBLIC),
            field("ignored", "Ljava/lang/String;", ACC_STATIC),
        ];
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            object,
            child,
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Person");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let candidate = results
            .iter()
            .find(|c| c.label.contains("toString"))
            .expect("toString override candidate");

        assert!(
            candidate.insert_text.contains("return \"Person{\" +"),
            "toString should start with class name: {}",
            candidate.insert_text
        );
        assert!(
            candidate.insert_text.contains("\"name=\" + this.name +"),
            "reference field should print directly: {}",
            candidate.insert_text
        );
        assert!(
            candidate
                .insert_text
                .contains("\", scores=\" + java.util.Arrays.toString(this.scores) +"),
            "1d arrays should use Arrays.toString: {}",
            candidate.insert_text
        );
        assert!(
            candidate
                .insert_text
                .contains("\", matrix=\" + java.util.Arrays.deepToString(this.matrix) +"),
            "nested arrays should use Arrays.deepToString: {}",
            candidate.insert_text
        );
        assert!(
            !candidate.insert_text.contains("ignored"),
            "static fields must be skipped: {}",
            candidate.insert_text
        );
    }

    #[test]
    fn test_object_methods_prefer_source_fields_over_index_fields() {
        let idx = WorkspaceIndex::new();
        let mut object = make_class("java/lang", "Object", None, vec![]);
        object.methods = vec![method("toString", "()Ljava/lang/String;", ACC_PUBLIC)];
        let mut child = make_class("com/example", "Person", Some("java/lang/Object"), vec![]);
        child.fields = vec![field("indexed", "Ljava/lang/String;", ACC_PUBLIC)];
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            object,
            child,
        ]);

        let source_fields = [CurrentClassMember::Field(Arc::new(field(
            "sourceOnly",
            "Ljava/lang/String;",
            ACC_PUBLIC,
        )))];
        let ctx = ctx_with_prefix("pub", "com/example/Person").with_class_members(source_fields);
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let candidate = results
            .iter()
            .find(|c| c.label.contains("toString"))
            .expect("toString override candidate");

        assert!(
            candidate.insert_text.contains("sourceOnly"),
            "source fields should be used when available: {}",
            candidate.insert_text
        );
        assert!(
            !candidate.insert_text.contains("indexed"),
            "indexed fields should be ignored when source fields exist: {}",
            candidate.insert_text
        );
    }

    #[test]
    fn test_interface_method_excluded_when_already_implemented_in_index() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_interface(
            "com/example",
            "Runnable",
            vec![abstract_method("run", "()V")],
        )]);
        let mut cls = make_class(
            "com/example",
            "MyTask",
            None,
            vec![
                method("run", "()V", ACC_PUBLIC), // 已实现
            ],
        );
        cls.interfaces = vec![Arc::from("com/example/Runnable")];
        idx.add_classes(vec![cls]);

        let ctx = ctx_with_prefix("pub", "com/example/MyTask");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().all(|c| !c.label.contains("run")),
            "already implemented run() must not appear: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_interface_method_excluded_via_source_members() {
        // 未编译，只有 source members
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_interface(
            "com/example",
            "Runnable",
            vec![abstract_method("run", "()V")],
        )]);
        let mut cls = make_class("com/example", "MyTask", None, vec![]);
        cls.interfaces = vec![Arc::from("com/example/Runnable")];
        idx.add_classes(vec![cls]);

        let source_member = CurrentClassMember::Method(Arc::new(method("run", "()V", ACC_PUBLIC)));
        let ctx = ctx_with_prefix("pub", "com/example/MyTask")
            .with_class_members(std::iter::once(source_member));

        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().all(|c| !c.label.contains("run")),
            "run() in source members must be excluded: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_multiple_interfaces_all_shown() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_interface(
                "com/example",
                "Runnable",
                vec![abstract_method("run", "()V")],
            ),
            make_interface(
                "com/example",
                "Closeable",
                vec![abstract_method("close", "()V")],
            ),
        ]);
        let mut cls = make_class("com/example", "Resource", None, vec![]);
        cls.interfaces = vec![
            Arc::from("com/example/Runnable"),
            Arc::from("com/example/Closeable"),
        ];
        idx.add_classes(vec![cls]);

        let ctx = ctx_with_prefix("pub", "com/example/Resource");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(
            labels.iter().any(|l| l.contains("run")),
            "run should appear: {:?}",
            labels
        );
        assert!(
            labels.iter().any(|l| l.contains("close")),
            "close should appear: {:?}",
            labels
        );
    }

    #[test]
    fn test_interface_method_not_duplicated_via_superclass_and_interface() {
        // Parent 实现了 Runnable，Child extends Parent —— run() 只应出现一次
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_interface(
            "com/example",
            "Runnable",
            vec![abstract_method("run", "()V")],
        )]);
        let mut parent = make_class("com/example", "Parent", None, vec![]);
        parent.interfaces = vec![Arc::from("com/example/Runnable")];
        let child = make_class("com/example", "Child", Some("com/example/Parent"), vec![]);
        idx.add_classes(vec![parent, child]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let count = results.iter().filter(|c| c.label.contains("run")).count();
        assert_eq!(
            count,
            1,
            "run() must not be duplicated: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_override_available_in_class_body_member_position() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_from_marked_source(
            r#"
            package com.example;
            class Child extends Parent {
                pub|
            }
            "#,
        );
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;

        assert!(ctx.is_class_member_position, "class body position expected");
        assert!(
            results.iter().any(|c| c.label.contains("doWork")),
            "override candidate should be available at class level"
        );
    }

    #[test]
    fn test_override_available_in_nested_class_body_member_position() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "RunnableParent",
                None,
                vec![method("run", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Outer", None, vec![]),
            make_nested_class(
                "com/example",
                "com/example/Outer$Nested",
                "Nested",
                "com/example/Outer",
                Some("com/example/RunnableParent"),
                vec![],
            ),
        ]);

        let ctx = ctx_from_marked_source(
            r#"
            package com.example;
            class Outer {
                static class Nested extends RunnableParent {
                    pub|
                }
            }
            "#,
        );
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;

        assert!(
            ctx.is_class_member_position,
            "nested class body is a valid member position"
        );
        assert_eq!(
            ctx.enclosing_internal_name.as_deref(),
            Some("com/example/Outer$Nested")
        );
        assert!(
            results.iter().any(|c| c.label.contains("run")),
            "override candidate should be available in nested class body"
        );
    }

    #[test]
    fn test_override_available_in_inner_class_body_member_position() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "RunnableParent",
                None,
                vec![method("run", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Outer", None, vec![]),
            make_nested_class(
                "com/example",
                "com/example/Outer$Inner",
                "Inner",
                "com/example/Outer",
                Some("com/example/RunnableParent"),
                vec![],
            ),
        ]);

        let ctx = ctx_from_marked_source(
            r#"
            package com.example;
            class Outer {
                class Inner extends RunnableParent {
                    pub|
                }
            }
            "#,
        );
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;

        assert!(
            ctx.is_class_member_position,
            "inner class body is a valid member position"
        );
        assert_eq!(
            ctx.enclosing_internal_name.as_deref(),
            Some("com/example/Outer$Inner")
        );
        assert!(
            results.iter().any(|c| c.label.contains("run")),
            "override candidate should be available in inner class body"
        );
    }

    #[test]
    fn test_override_skipped_inside_method_body() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_from_marked_source(
            r#"
            package com.example;
            class Child extends Parent {
                void run() {
                    pub|
                }
            }
            "#,
        );
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;

        assert!(
            !ctx.is_class_member_position,
            "method body must not be class member position"
        );
        assert!(
            results.is_empty(),
            "override must be skipped in method body"
        );
    }

    #[test]
    fn test_override_skipped_inside_constructor_body() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_from_marked_source(
            r#"
            package com.example;
            class Child extends Parent {
                Child() {
                    pub|
                }
            }
            "#,
        );
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;

        assert!(
            !ctx.is_class_member_position,
            "constructor body must not be class member position"
        );
        assert!(
            results.is_empty(),
            "override must be skipped in constructor body"
        );
    }

    #[test]
    fn test_override_skipped_inside_initializer_block() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_from_marked_source(
            r#"
            package com.example;
            class Child extends Parent {
                {
                    pub|
                }
            }
            "#,
        );
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;

        assert!(
            !ctx.is_class_member_position,
            "initializer block must not be class member position"
        );
        assert!(
            results.is_empty(),
            "override must be skipped in initializer block"
        );
    }

    #[test]
    fn test_override_skipped_inside_method_body_of_nested_class() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "RunnableParent",
                None,
                vec![method("run", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Outer", None, vec![]),
            make_nested_class(
                "com/example",
                "com/example/Outer$Nested",
                "Nested",
                "com/example/Outer",
                Some("com/example/RunnableParent"),
                vec![],
            ),
        ]);

        let ctx = ctx_from_marked_source(
            r#"
            package com.example;
            class Outer {
                static class Nested extends RunnableParent {
                    void f() {
                        pub|
                    }
                }
            }
            "#,
        );
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;

        assert!(
            !ctx.is_class_member_position,
            "method body in nested class is executable context"
        );
        assert!(
            results.is_empty(),
            "override must be skipped in method body"
        );
    }

    #[test]
    fn test_override_clone_display_keeps_object_return_type() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "java/lang",
                "Object",
                None,
                vec![method("clone", "()Ljava/lang/Object;", ACC_PROTECTED)],
            ),
            make_class("com/example", "Child", Some("java/lang/Object"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pro", "com/example/Child");
        let results = OverrideProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let clone = results
            .iter()
            .find(|c| c.label.contains("clone"))
            .expect("clone override candidate should exist");

        assert!(
            clone.label.contains("java.lang.Object clone(")
                || clone.label.contains("Object clone("),
            "clone label should keep Object return type, got: {}",
            clone.label
        );
        assert!(
            !clone.label.contains("void clone("),
            "clone label must not collapse Object to void: {}",
            clone.label
        );
        assert!(
            clone.insert_text.contains("java.lang.Object clone(")
                || clone.insert_text.contains("Object clone("),
            "insert text should keep Object return type: {}",
            clone.insert_text
        );
    }
}
