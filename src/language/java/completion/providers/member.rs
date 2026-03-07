use rust_asm::constants::ACC_STATIC;

use crate::completion::provider::CompletionProvider;
use crate::completion::scorer::AccessFilter;
use crate::completion::{CandidateKind, CompletionCandidate};
use crate::language::java::render;
use crate::semantic::context::{CursorLocation, SemanticContext};
use crate::{
    completion::fuzzy,
    index::{IndexScope, IndexView},
    semantic::types::{ContextualResolver, TypeResolver, type_name::TypeName},
};
use std::sync::Arc;

pub struct MemberProvider;

impl CompletionProvider for MemberProvider {
    fn name(&self) -> &'static str {
        "member"
    }

    fn provide(
        &self,
        scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        if !matches!(&ctx.location, CursorLocation::MemberAccess { .. }) {
            return vec![];
        }

        let receiver_semantic_type = ctx.location.member_access_receiver_semantic_type();
        let receiver_owner_internal = ctx.location.member_access_receiver_owner_internal();
        let member_prefix = ctx.location.member_access_prefix().unwrap_or("");
        let receiver_expr = ctx.location.member_access_expr().unwrap_or("");

        tracing::debug!(
            receiver_expr,
            member_prefix,
            locals = ?ctx.local_variables.iter().map(|lv| format!("{}:{}", lv.name, lv.type_internal)).collect::<Vec<_>>(),
            imports = ?ctx.existing_imports,
            "MemberProvider.provide"
        );

        let resolver = ContextualResolver::new(index, ctx);

        if receiver_expr == "this" {
            if ctx.is_in_static_context() {
                return vec![];
            }

            // source members (including private members, directly parsed from the AST)
            let mut results = if !ctx.current_class_members.is_empty() {
                self.provide_from_source_members(ctx, member_prefix)
            } else {
                vec![]
            };

            if let Some(enclosing) = ctx.enclosing_internal_name.as_deref() {
                let source_names: std::collections::HashSet<Arc<str>> =
                    ctx.current_class_members.keys().map(Arc::clone).collect();
                let prefix_lower = member_prefix.to_lowercase();

                // index MRO: skip(0) means starting from the current class, using the same_class filter
                //
                // When source members have already covered the current class, the index entries of the current class will be skipped due to
                // source_names deduplication, and will not be repeated.
                let mro = index.mro(enclosing);

                for (i, class_meta) in mro.iter().enumerate() {
                    let filter = if i == 0 {
                        AccessFilter::same_class()
                    } else {
                        AccessFilter::member_completion()
                    };

                    for method in &class_meta.methods {
                        if method.name.as_ref() == "<init>" || method.name.as_ref() == "<clinit>" {
                            continue;
                        }
                        if !filter.is_method_accessible(method.access_flags, method.is_synthetic) {
                            continue;
                        }
                        // The current class itself: source members are already included, skipping to avoid duplication.
                        if i == 0 && source_names.contains(&method.name) {
                            continue;
                        }
                        if !prefix_lower.is_empty()
                            && !method.name.to_lowercase().contains(&prefix_lower)
                        {
                            continue;
                        }
                        use rust_asm::constants::ACC_STATIC;
                        let is_static = method.access_flags & ACC_STATIC != 0;
                        let kind = if is_static {
                            CandidateKind::StaticMethod {
                                descriptor: method.desc(),
                                defining_class: Arc::clone(&class_meta.internal_name),
                            }
                        } else {
                            CandidateKind::Method {
                                descriptor: method.desc(),
                                defining_class: Arc::clone(&class_meta.internal_name),
                            }
                        };
                        let insert_text = if ctx.has_paren_after_cursor() {
                            method.name.to_string()
                        } else {
                            format!("{}(", method.name)
                        };
                        let detail = if i == 0 {
                            render::method_detail(
                                ctx.enclosing_internal_name.as_deref().unwrap_or(""),
                                class_meta,
                                method,
                                &resolver,
                            )
                        } else {
                            format!("inherited from {}", class_meta.name)
                        };
                        results.push(
                            CompletionCandidate::new(
                                Arc::clone(&method.name),
                                insert_text,
                                kind,
                                self.name(),
                            )
                            .with_detail(detail)
                            .with_score(if i == 0 {
                                60.0
                            } else {
                                55.0
                            }),
                        );
                    }

                    for field in &class_meta.fields {
                        let filter = if i == 0 {
                            AccessFilter::same_class()
                        } else {
                            AccessFilter::member_completion()
                        };
                        if !filter.is_field_accessible(field.access_flags, field.is_synthetic) {
                            continue;
                        }
                        if i == 0 && source_names.contains(&field.name) {
                            continue;
                        }
                        if !prefix_lower.is_empty()
                            && !field.name.to_lowercase().contains(&prefix_lower)
                        {
                            continue;
                        }

                        let is_static = field.access_flags & ACC_STATIC != 0;
                        let kind = if is_static {
                            CandidateKind::StaticField {
                                descriptor: Arc::clone(&field.descriptor),
                                defining_class: Arc::clone(&class_meta.internal_name),
                            }
                        } else {
                            CandidateKind::Field {
                                descriptor: Arc::clone(&field.descriptor),
                                defining_class: Arc::clone(&class_meta.internal_name),
                            }
                        };
                        let detail = if i == 0 {
                            render::field_detail(
                                ctx.enclosing_internal_name.as_deref().unwrap_or(""),
                                class_meta,
                                field,
                                &resolver,
                            )
                        } else {
                            format!("inherited from {}", class_meta.name)
                        };
                        results.push(
                            CompletionCandidate::new(
                                Arc::clone(&field.name),
                                field.name.to_string(),
                                kind,
                                self.name(),
                            )
                            .with_detail(detail)
                            .with_score(if i == 0 {
                                60.0
                            } else {
                                55.0
                            }),
                        );
                    }
                }
            }
            return results;
        }

        let resolved_semantic = receiver_semantic_type
            .cloned()
            .or_else(|| receiver_owner_internal.map(TypeName::new))
            .or_else(|| resolve_receiver_type(receiver_expr, ctx, index, scope).map(TypeName::from));

        let resolved = match resolved_semantic {
            Some(t) => t,
            None => {
                tracing::debug!(
                    receiver_expr,
                    "resolve_receiver_type returned None, returning empty"
                );
                return vec![];
            }
        };

        let class_internal = resolved.to_internal_with_generics();
        let base_class_internal = resolved.erased_internal();

        tracing::debug!(
            base_class_internal,
            class_internal,
            "looking up class in index"
        );

        // Check if it's a similar access (allow private/protected access)
        let is_same_class = ctx.enclosing_internal_name.as_deref() == Some(base_class_internal);

        let filter = if is_same_class {
            AccessFilter::same_class()
        } else {
            AccessFilter::member_completion()
        };

        let prefix_lower = member_prefix.to_lowercase();
        let mut results = Vec::new();

        let mro = index.mro(base_class_internal);
        let mut seen_methods = std::collections::HashSet::new();
        let mut seen_fields = std::collections::HashSet::new();

        let resolver = ContextualResolver::new(index, ctx);

        for class_meta in &mro {
            for method in &class_meta.methods {
                // skip <init>, <clinit>
                if method.name.as_ref() == "<init>" || method.name.as_ref() == "<clinit>" {
                    continue;
                }
                // shadowing: subclass method hides superclass method with same name+descriptor
                let key = (Arc::clone(&method.name), Arc::clone(&method.desc()));
                if !seen_methods.insert(key) {
                    continue;
                }
                if !filter.is_method_accessible(method.access_flags, method.is_synthetic) {
                    continue;
                }
                if !prefix_lower.is_empty() && !method.name.to_lowercase().contains(&prefix_lower) {
                    continue;
                }
                let trace_add = method.name.as_ref() == "add"
                    && (class_internal.contains("ArrayList")
                        || base_class_internal.contains("ArrayList"));
                if trace_add {
                    tracing::debug!(
                        receiver_expr,
                        member_prefix,
                        class_internal,
                        base_class_internal,
                        class_meta_internal = %class_meta.internal_name,
                        class_meta_origin = ?class_meta.origin,
                        class_meta_generic_signature = ?class_meta.generic_signature,
                        method_name = %method.name,
                        method_desc = %method.desc(),
                        method_generic_signature = ?method.generic_signature,
                        method_return_type = ?method.return_type,
                        param_descriptors = ?method.params.items.iter().map(|p| p.descriptor.as_ref()).collect::<Vec<_>>(),
                        param_names = ?method.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                        "member_provider: add overload candidate before detail rendering"
                    );
                }
                let is_static = method.access_flags & ACC_STATIC != 0;
                let kind = if is_static {
                    CandidateKind::StaticMethod {
                        descriptor: method.desc(),
                        defining_class: Arc::from(class_internal.as_str()),
                    }
                } else {
                    CandidateKind::Method {
                        descriptor: method.desc(),
                        defining_class: Arc::from(class_internal.as_str()),
                    }
                };
                results.push(
                    CompletionCandidate::new(
                        Arc::clone(&method.name),
                        if ctx.has_paren_after_cursor() {
                            method.name.to_string()
                        } else {
                            format!("{}(", method.name)
                        },
                        kind,
                        self.name(),
                    )
                    .with_detail({
                        let detail = render::method_detail(
                            &class_internal,
                            class_meta,
                            method,
                            &resolver,
                        );
                        if trace_add {
                            tracing::debug!(
                                method_name = %method.name,
                                method_desc = %method.desc(),
                                detail,
                                "member_provider: add overload candidate after detail rendering"
                            );
                        }
                        detail
                    }),
                );
            }

            for field in &class_meta.fields {
                if !seen_fields.insert(Arc::clone(&field.name)) {
                    continue;
                }
                if !filter.is_field_accessible(field.access_flags, field.is_synthetic) {
                    continue;
                }
                if !prefix_lower.is_empty() && !field.name.to_lowercase().contains(&prefix_lower) {
                    continue;
                }
                let is_static = field.access_flags & ACC_STATIC != 0;
                let kind = if is_static {
                    CandidateKind::StaticField {
                        descriptor: Arc::clone(&field.descriptor),
                        defining_class: Arc::from(class_internal.as_str()),
                    }
                } else {
                    CandidateKind::Field {
                        descriptor: Arc::clone(&field.descriptor),
                        defining_class: Arc::from(class_internal.as_str()),
                    }
                };
                results.push(
                    CompletionCandidate::new(
                        Arc::clone(&field.name),
                        field.name.to_string(),
                        kind,
                        self.name(),
                    )
                    .with_detail(render::field_detail(
                        &class_internal,
                        class_meta,
                        field,
                        &resolver,
                    )),
                );
            }
        }
        results
    }
}

impl MemberProvider {
    fn provide_from_source_members(
        &self,
        ctx: &SemanticContext,
        member_prefix: &str,
    ) -> Vec<CompletionCandidate> {
        let enclosing = ctx.enclosing_internal_name.as_deref().unwrap_or("");

        let scored =
            fuzzy::fuzzy_filter_sort(member_prefix, ctx.current_class_members.values(), |m| {
                m.name()
            });

        scored
            .into_iter()
            .map(|(m, score)| {
                let kind = match (m.is_method(), m.is_static()) {
                    (true, true) => CandidateKind::StaticMethod {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                    (true, false) => CandidateKind::Method {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                    (false, true) => CandidateKind::StaticField {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                    (false, false) => CandidateKind::Field {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                };
                let insert_text = if m.is_method() {
                    if ctx.has_paren_after_cursor() {
                        m.name().to_string()
                    } else {
                        format!("{}(", m.name())
                    }
                } else {
                    m.name().to_string()
                };
                let detail = format!(
                    "{} {} {}",
                    if m.is_private() { "private" } else { "public" },
                    if m.is_static() { "static" } else { "" },
                    m.name()
                );
                CompletionCandidate::new(m.name(), insert_text, kind, self.name())
                    .with_detail(detail)
                    .with_score(70.0 + score as f32 * 0.1)
            })
            .collect()
    }
}

/// Parses the receiver expression into an inner class name
fn resolve_receiver_type(
    expr: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    scope: IndexScope,
) -> Option<Arc<str>> {
    tracing::debug!(
        expr,
        locals_count = ctx.local_variables.len(),
        "resolve_receiver_type"
    );

    if expr == "this" {
        let r = ctx.enclosing_internal_name.clone();
        tracing::debug!(?r, "this -> enclosing");
        return r;
    }

    // new Foo() / new Foo(args) -> return Foo's internal name
    if let Some(class_name) = extract_constructor_class(expr) {
        return resolve_simple_name_to_internal(class_name, ctx, index, scope);
    }

    // function call: "getMain2()" / "getMain2(arg1, arg2)"
    if let Some(internal) = resolve_method_call_receiver(expr, ctx, index, scope) {
        return Some(internal);
    }

    // local variable
    if let Some(lv) = ctx
        .local_variables
        .iter()
        .find(|lv| lv.name.as_ref() == expr)
    {
        tracing::debug!(
            expr,
            type_internal = %lv.type_internal,
            "found in locals"
        );

        if lv.type_internal.contains_slash() {
            let internal = lv.type_internal.to_internal_with_generics();
            tracing::debug!(internal, "type contains '/', returning directly");
            return Some(Arc::from(internal));
        }

        let ty = lv.type_internal.erased_internal_with_arrays();
        let result = resolve_complex_type_to_internal(&ty, ctx, index, scope);
        tracing::debug!(?result, ty, "resolve_complex_name_to_internal result");
        return result.map(|t| Arc::from(t.to_internal_with_generics()));
    }

    tracing::debug!(expr, "local var not found");

    if let Some(internal_class) = resolve_strict_class_name(expr, ctx, index, scope) {
        return Some(internal_class);
    }

    None
}

fn resolve_strict_class_name(
    simple_name: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    _scope: IndexScope,
) -> Option<Arc<str>> {
    // Enclosing class
    if let Some(enclosing) = &ctx.enclosing_internal_name {
        let enclosing_simple = enclosing
            .rsplit('/')
            .next()
            .unwrap_or(enclosing)
            .rsplit('$')
            .next()
            .unwrap_or(enclosing);

        if simple_name == enclosing_simple {
            return Some(Arc::clone(enclosing));
        }
    }

    // Explicit Imports
    for imp in &ctx.existing_imports {
        let explicit_suffix = format!("/{}", simple_name);
        if imp.ends_with(&explicit_suffix) {
            return Some(Arc::clone(imp));
        }

        // wildcard imports (*)
        if let Some(pkg) = imp.strip_suffix("/*") {
            let candidate = format!("{}/{}", pkg, simple_name);

            if index.get_class(&candidate).is_some() {
                return Some(Arc::from(candidate));
            }
        }
    }

    // Same Package
    if let Some(enclosing) = &ctx.enclosing_internal_name
        && let Some(last_slash) = enclosing.rfind('/')
    {
        let pkg = &enclosing[..last_slash];
        let candidate = format!("{}/{}", pkg, simple_name);

        if index.get_class(&candidate).is_some() {
            return Some(Arc::from(candidate));
        }
    }

    // java.lang.*
    let java_lang_candidate = format!("java/lang/{}", simple_name);
    if index.get_class(&java_lang_candidate).is_some() {
        return Some(Arc::from(java_lang_candidate));
    }

    None
}

/// Dynamically recursively resolves types, using the current context (ctx/index) to convert source code types into JVM signatures.
fn resolve_complex_type_to_internal(
    ty: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    scope: IndexScope,
) -> Option<TypeName> {
    let ty = ty.trim();

    // 1. Array
    if let Some(stripped) = ty.strip_suffix("[]") {
        let inner = resolve_complex_type_to_internal(stripped, ctx, index, scope)?;
        return Some(inner.wrap_array());
    }

    // 2. Generics
    if let Some(pos) = ty.find('<')
        && ty.ends_with('>')
    {
        let base = &ty[..pos];
        let args_str = &ty[pos + 1..ty.len() - 1];

        // 动态解析 Base 类 (例如 "List" -> "java/util/List")
        let base_internal = resolve_complex_type_to_internal(base, ctx, index, scope)?;

        // 应对 diamond operator: new ArrayList<>()
        if args_str.trim().is_empty() {
            return Some(base_internal);
        }

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

        let mut resolved_args = Vec::new();
        for a in args {
            let arg = a.trim();
            if arg == "?" {
                resolved_args.push(TypeName::new("*"));
                continue;
            }

            // 动态解析泛型实参 (例如 "String" -> "java/lang/String")
            let inner = resolve_complex_type_to_internal(arg, ctx, index, scope)?;
            resolved_args.push(inner);
        }
        let mut base_ty = base_internal;
        base_ty.args = resolved_args;
        return Some(base_ty);
    }

    // 3. Primitive & Special
    match ty {
        "byte" | "short" | "int" | "long" | "float" | "double" | "boolean" | "char" | "void"
        | "var" => {
            return Some(TypeName::new(ty));
        }
        _ => {}
    }

    // 4. Base class name
    if ty.contains('/') {
        Some(TypeName::new(ty))
    } else {
        // 交给原有的 import / global index 推导机制去查
        resolve_simple_name_to_internal(ty, ctx, index, scope).map(TypeName::from)
    }
}

/// Extract "Foo" from "new Foo()" / "new Foo(a, b)".
fn extract_constructor_class(expr: &str) -> Option<&str> {
    let rest = expr.trim().strip_prefix("new ")?;
    // The part before the '(' is the class name.
    let class_part = rest.split('(').next()?.trim();
    if class_part.is_empty() {
        None
    } else {
        Some(class_part)
    }
}

/// Parse receiver expressions of the form "someMethod()" / "someMethod(args)"
/// Find the method in the MRO of the enclosing class and get its return type
fn resolve_method_call_receiver(
    expr: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    _scope: IndexScope,
) -> Option<Arc<str>> {
    // Must contain '(' and end with ')'
    let paren = expr.find('(')?;
    if !expr.ends_with(')') {
        return None;
    }
    let method_name = expr[..paren].trim();
    if method_name.is_empty() || method_name.contains('.') || method_name.contains(' ') {
        return None;
    }
    // arg count (simple estimate, no need for precision)
    let args_text = &expr[paren + 1..expr.len() - 1];
    let arg_count = if args_text.trim().is_empty() {
        0i32
    } else {
        args_text.split(',').count() as i32
    };

    let enclosing = ctx.enclosing_internal_name.as_deref()?;
    let resolver = TypeResolver::new(index);
    resolver
        .resolve_method_return(enclosing, method_name, arg_count, &[])
        .map(|i| Arc::from(i.to_internal_with_generics()))
}

/// Resolves simple class names to internal names
/// Search order: imports → same package → global
fn resolve_simple_name_to_internal(
    simple: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    _scope: IndexScope,
) -> Option<Arc<str>> {
    tracing::debug!(simple, "resolve_simple_name_to_internal called");

    let imported = index.resolve_imports(&ctx.existing_imports);
    tracing::debug!(
        simple,
        imported = ?imported.iter().map(|m| format!("name={} internal={}", m.name, m.internal_name)).collect::<Vec<_>>(),
        "resolve_simple_name: imports"
    );

    if let Some(m) = imported.iter().find(|m| m.name.as_ref() == simple) {
        let exists = index.get_class(m.internal_name.as_ref()).is_some();
        tracing::debug!(
            internal = m.internal_name.as_ref(),
            exists,
            "found in imports"
        );
        return Some(Arc::clone(&m.internal_name));
    }

    if let Some(pkg) = ctx.enclosing_package.as_deref() {
        let classes = index.classes_in_package(pkg);
        tracing::debug!(pkg, count = classes.len(), "same package classes");
        if let Some(m) = classes.iter().find(|m| m.name.as_ref() == simple) {
            return Some(Arc::clone(&m.internal_name));
        }
    }

    let candidates = index.get_classes_by_simple_name(simple);
    tracing::debug!(simple, count = candidates.len(), internals = ?candidates.iter().map(|c| c.internal_name.as_ref()).collect::<Vec<_>>(), "global lookup");

    if !candidates.is_empty() {
        return Some(Arc::clone(&candidates[0].internal_name));
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::index::WorkspaceIndex;
    use rust_asm::constants::{ACC_PRIVATE, ACC_PUBLIC, ACC_STATIC};
    use std::sync::Arc;
    use tracing_subscriber::{EnvFilter, fmt};

    use crate::completion::provider::CompletionProvider;
    use crate::index::{
        ClassMetadata, ClassOrigin, FieldSummary, IndexScope, MethodParams, MethodSummary, ModuleId,
    };
    use crate::language::java::completion::providers::member::MemberProvider;
    use crate::semantic::context::{CurrentClassMember, CursorLocation, SemanticContext};
    use crate::semantic::LocalVar;
    use crate::semantic::types::{parse_return_type_from_descriptor, type_name::TypeName};

    fn init_test_tracing() {
        let _ = fmt()
            .with_test_writer()
            .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug")))
            .try_init();
    }

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_method(name: &str, descriptor: &str, flags: u16, is_synthetic: bool) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            params: MethodParams::from_method_descriptor(descriptor),
            annotations: vec![],
            access_flags: flags,
            is_synthetic,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    fn make_field(name: &str, descriptor: &str, flags: u16, is_synthetic: bool) -> FieldSummary {
        FieldSummary {
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            annotations: vec![],
            access_flags: flags,
            is_synthetic,
            generic_signature: None,
        }
    }

    /// 快速包装方法枚举
    fn m(name: &str, flags: u16, is_private: bool) -> CurrentClassMember {
        let mut f = flags;
        if is_private {
            f |= ACC_PRIVATE;
        }
        CurrentClassMember::Method(Arc::new(make_method(name, "()V", f, false)))
    }

    /// 快速包装字段枚举
    fn f(name: &str, flags: u16, is_private: bool) -> CurrentClassMember {
        let mut f = flags;
        if is_private {
            f |= ACC_PRIVATE;
        }
        CurrentClassMember::Field(Arc::new(make_field(name, "I", f, false)))
    }

    fn make_index(methods: Vec<MethodSummary>, fields: Vec<FieldSummary>) -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("com/example")),
            name: Arc::from("Foo"),
            internal_name: Arc::from("com/example/Foo"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods,
            fields,
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        idx
    }

    fn ctx_with_type(receiver_internal: &str, prefix: &str) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: Some(Arc::from(receiver_internal)),
                member_prefix: prefix.to_string(),
                receiver_expr: "someObj".to_string(),
                arguments: None,
            },
            prefix,
            vec![],
            None,
            None,
            None,
            vec![],
        )
    }

    fn ctx_with_semantic_and_erased(
        semantic_receiver: TypeName,
        erased_receiver: &str,
        prefix: &str,
    ) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(semantic_receiver),
                receiver_type: Some(Arc::from(erased_receiver)),
                member_prefix: prefix.to_string(),
                receiver_expr: "someObj".to_string(),
                arguments: None,
            },
            prefix,
            vec![],
            None,
            None,
            None,
            vec![],
        )
    }

    fn ctx_this(
        enclosing_simple: &str,
        enclosing_internal: &str,
        enclosing_pkg: &str,
        prefix: &str,
    ) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: prefix.to_string(),
                receiver_expr: "this".to_string(),
                arguments: None,
            },
            prefix,
            vec![],
            Some(Arc::from(enclosing_simple)),
            Some(Arc::from(enclosing_internal)),
            Some(Arc::from(enclosing_pkg)),
            vec![],
        )
    }

    #[test]
    fn test_instance_method_found() {
        let idx = make_index(
            vec![make_method(
                "getValue",
                "()Ljava/lang/String;",
                ACC_PUBLIC,
                false,
            )],
            vec![],
        );
        let ctx = ctx_with_type("com/example/Foo", "get");
        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "getValue"));
    }

    #[test]
    fn test_empty_prefix_returns_all_public() {
        let idx = make_index(
            vec![
                make_method("getName", "()Ljava/lang/String;", ACC_PUBLIC, false),
                make_method("setName", "(Ljava/lang/String;)V", ACC_PUBLIC, false),
                make_method("secret", "()V", ACC_PRIVATE, false),
            ],
            vec![],
        );
        let ctx = ctx_with_type("com/example/Foo", "");
        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_same_class_private_visible_via_this() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy/a")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/a/Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                make_method("pri", "()V", ACC_PRIVATE | ACC_STATIC, false),
                make_method("func", "()V", ACC_PUBLIC, false),
            ],
            fields: vec![make_field("secret", "I", ACC_PRIVATE, false)],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = ctx_this("Main", "org/cubewhy/a/Main", "org/cubewhy/a", "pr");
        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "pri"));
    }

    #[test]
    fn test_this_dot_uses_source_members_including_private() {
        let idx = WorkspaceIndex::new();
        let members = vec![
            m("priFunc", ACC_PUBLIC, true), // is_private = true
            m("fun", ACC_PUBLIC, false),
            m("func", ACC_PUBLIC, false),
        ];

        let ctx =
            ctx_this("Main", "org/cubewhy/a/Main", "org/cubewhy/a", "").with_class_members(members);

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "priFunc"));
        assert!(results.iter().any(|c| c.label.as_ref() == "fun"));
    }

    #[test]
    fn test_no_this_completion_in_static_method() {
        let idx = WorkspaceIndex::new();
        let members = vec![
            f("staticField", ACC_STATIC, false),
            f("instanceField", ACC_PUBLIC, false),
        ];

        let enclosing_method = Arc::new(make_method(
            "main",
            "([Ljava/lang/String;)V",
            ACC_STATIC,
            false,
        ));

        let ctx = ctx_this("Main", "org/cubewhy/Main", "org/cubewhy", "")
            .with_class_members(members)
            .with_enclosing_member(Some(CurrentClassMember::Method(enclosing_method)));

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.is_empty());
    }

    #[test]
    fn test_bare_method_call_receiver_resolved() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Main"),
                internal_name: Arc::from("Main"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("getMain2", "()LMain2;", ACC_PUBLIC, false)],
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
                methods: vec![make_method("func", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "getMain2()".to_string(),
                arguments: None,
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("Main")),
            None,
            vec![],
        );

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "func"));
    }

    #[test]
    fn test_member_provider_prefers_semantic_receiver_type() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("com/example")),
                name: Arc::from("Legacy"),
                internal_name: Arc::from("com/example/Legacy"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("legacyOnly", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("com/example")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("listOnly", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let ctx = ctx_with_semantic_and_erased(
            TypeName::with_args("java/util/List", vec![TypeName::new("java/lang/String")]),
            "com/example/Legacy",
            "",
        );

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "listOnly"));
        assert!(!results.iter().any(|c| c.label.as_ref() == "legacyOnly"));
    }

    #[test]
    fn test_member_provider_falls_back_to_legacy_receiver_type() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("com/example")),
            name: Arc::from("Legacy"),
            internal_name: Arc::from("com/example/Legacy"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![make_method("legacyOnly", "()V", ACC_PUBLIC, false)],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = ctx_with_type("com/example/Legacy", "");
        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "legacyOnly"));
    }

    #[test]
    fn test_member_provider_arraylist_add_overloads_for_trace() {
        init_test_tracing();

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/util")),
            name: Arc::from("ArrayList"),
            internal_name: Arc::from("java/util/ArrayList"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                MethodSummary {
                    name: Arc::from("add"),
                    params: MethodParams::from([("TE;", "e")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TE;)Z")),
                    return_type: parse_return_type_from_descriptor("(TE;)Z"),
                },
                MethodSummary {
                    name: Arc::from("add"),
                    params: MethodParams::from([("I", "index"), ("TE;", "element")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(ITE;)V")),
                    return_type: parse_return_type_from_descriptor("(ITE;)V"),
                },
            ],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::with_args(
                    "java/util/ArrayList",
                    vec![TypeName::new("java/lang/String")],
                )),
                receiver_type: Some(Arc::from("java/util/ArrayList")),
                member_prefix: "add".to_string(),
                receiver_expr: "list".to_string(),
                arguments: None,
            },
            "add",
            vec![LocalVar {
                name: Arc::from("list"),
                type_internal: TypeName::with_args(
                    "java/util/ArrayList",
                    vec![TypeName::new("java/lang/String")],
                ),
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("Main")),
            None,
            vec![],
        );

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results
                .iter()
                .filter(|c| c.label.as_ref() == "add")
                .count()
                >= 2,
            "expected at least 2 add overloads, got {:?}",
            results
                .iter()
                .filter(|c| c.label.as_ref() == "add")
                .map(|c| c.detail.clone())
                .collect::<Vec<_>>()
        );
    }
}
