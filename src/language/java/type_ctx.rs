use std::sync::Arc;

use crate::index::{IndexView, MethodSummary};

/// Per-file type resolution context built from the file's own package + imports.
/// Converts bare Java simple names → JVM internal names following JLS §7.5 priority.
#[derive(Clone)]
pub struct SourceTypeCtx {
    package: Option<Arc<str>>,
    /// Normalized import strings, e.g. `"java.util.List"` or `"java.util.*"`.
    imports: Vec<Arc<str>>,
    name_table: Option<Arc<crate::index::NameTable>>,
    view: Option<IndexView>,
    current_class_methods: std::collections::HashMap<Arc<str>, Arc<MethodSummary>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeResolveQuality {
    Exact,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaxedTypeResolution {
    pub ty: crate::semantic::types::type_name::TypeName,
    pub quality: TypeResolveQuality,
}

impl SourceTypeCtx {
    pub fn new(
        package: Option<Arc<str>>,
        imports: Vec<Arc<str>>,
        name_table: Option<Arc<crate::index::NameTable>>,
    ) -> Self {
        Self {
            package,
            imports,
            name_table,
            view: None,
            current_class_methods: std::collections::HashMap::new(),
        }
    }

    pub fn with_view(mut self, view: IndexView) -> Self {
        self.view = Some(view);
        self
    }

    pub fn with_current_class_methods(
        mut self,
        methods: std::collections::HashMap<Arc<str>, Arc<MethodSummary>>,
    ) -> Self {
        self.current_class_methods = methods;
        self
    }

    pub fn current_class_method(&self, name: &str) -> Option<&Arc<MethodSummary>> {
        self.current_class_methods.get(name)
    }

    /// Convert a Java source-level type expression to a JVM descriptor fragment.
    /// Handles arrays, generics (erasure), varargs, primitives.
    pub fn to_descriptor(&self, ty: &str) -> String {
        let ty = ty.trim();
        // Vararg → treated as one extra array dimension
        let (ty, extra_dim) = if let Some(stripped) = ty.strip_suffix("...") {
            (stripped, 1usize)
        } else {
            (ty, 0)
        };
        let mut dims = extra_dim;
        let mut base = ty.trim();
        while base.ends_with("[]") {
            dims += 1;
            base = base[..base.len() - 2].trim();
        }
        // Erase generics
        let base = base.split('<').next().unwrap_or(base).trim();

        let mut desc = String::new();
        for _ in 0..dims {
            desc.push('[');
        }
        match base {
            "void" => desc.push('V'),
            "boolean" => desc.push('Z'),
            "byte" => desc.push('B'),
            "char" => desc.push('C'),
            "short" => desc.push('S'),
            "int" => desc.push('I'),
            "long" => desc.push('J'),
            "float" => desc.push('F'),
            "double" => desc.push('D'),
            other => {
                let resolved = self.resolve_simple(other);
                desc.push('L');
                desc.push_str(&resolved);
                desc.push(';');
            }
        }
        desc
    }

    /// Resolve a bare simple name to its JVM internal name.
    /// Returns `simple` unchanged if unresolvable.
    pub fn resolve_simple(&self, simple: &str) -> String {
        match self.resolve_simple_strict(simple) {
            Some(result) => {
                tracing::trace!(simple, resolved = %result, "type resolved");
                result
            }
            None => {
                tracing::trace!(
                    simple,
                    has_table = self.name_table.is_some(),
                    "type unresolved"
                );
                simple.to_string()
            }
        }
    }

    /// Resolve a bare simple name to its JVM internal name.
    /// Returns None if resolution cannot be proven (no guessing).
    pub fn resolve_simple_strict(&self, simple: &str) -> Option<String> {
        self.resolve_simple_inner_strict(simple)
    }

    fn resolve_simple_inner_strict(&self, simple: &str) -> Option<String> {
        if simple.contains('/') {
            return self.class_exists(simple).then(|| simple.to_string());
        }
        // Rule 1: Single-type-import — JLS §7.5.1
        // The import text itself IS the full qualified name; must exist in NameTable.
        let mut exact_candidates = Vec::new();
        for imp in &self.imports {
            let s = imp.as_ref();
            if !s.ends_with(".*") && (s == simple || s.ends_with(&format!(".{}", simple))) {
                let internal = s.replace('.', "/");
                if self.class_exists(&internal) {
                    exact_candidates.push(internal);
                }
            }
        }
        if exact_candidates.len() > 1 {
            tracing::debug!(
                simple,
                candidates = ?exact_candidates,
                "resolve_simple_strict: ambiguous single-type imports"
            );
            return None;
        }
        if let Some(hit) = exact_candidates.pop() {
            self.log_resolution_hit(simple, &hit, "single_import");
            return Some(hit);
        }

        // Rule 2: Same package — JLS §6.4.1; verify via index, never assume
        if let Some(pkg) = &self.package {
            let candidate = format!("{}/{}", pkg, simple);
            if self.class_exists(&candidate) {
                self.log_resolution_hit(simple, &candidate, "same_package");
                return Some(candidate);
            }
        } else if self.class_exists(simple) {
            self.log_resolution_hit(simple, simple, "default_package");
            return Some(simple.to_string());
        }

        // Rule 3: java.lang.* — JLS §7.5.3 (always implicit)
        let java_lang = format!("java/lang/{}", simple);
        if self.class_exists(&java_lang) {
            self.log_resolution_hit(simple, &java_lang, "java_lang");
            return Some(java_lang);
        }

        // Rule 4: Type-import-on-demand (wildcard) — JLS §7.5.2; requires index
        let mut wildcard_candidates = Vec::new();
        for imp in &self.imports {
            let s = imp.as_ref();
            if s.ends_with(".*") {
                let pkg = s.trim_end_matches(".*").replace('.', "/");
                let candidate = format!("{}/{}", pkg, simple);
                if self.class_exists(&candidate) {
                    wildcard_candidates.push(candidate);
                }
            }
        }
        if wildcard_candidates.len() > 1 {
            tracing::debug!(
                simple,
                candidates = ?wildcard_candidates,
                "resolve_simple_strict: ambiguous wildcard imports"
            );
            return None;
        }
        let resolved = wildcard_candidates.pop();
        if let Some(hit) = &resolved {
            self.log_resolution_hit(simple, hit, "wildcard_import");
        }
        resolved
    }

    /// Resolve a source-level type name (with optional generics/arrays) to an internal TypeName.
    /// Returns None if any part cannot be proven.
    pub fn resolve_type_name_strict(
        &self,
        ty: &str,
    ) -> Option<crate::semantic::types::type_name::TypeName> {
        resolve_type_name_strict_inner(self, ty)
    }

    /// Resolve a source-level type name in a best-effort mode.
    /// Preserves the outer/base type whenever possible, and marks the
    /// result as Partial when generic arguments cannot be fully resolved.
    pub fn resolve_type_name_relaxed(&self, ty: &str) -> Option<RelaxedTypeResolution> {
        let (ty, quality) = resolve_type_name_relaxed_inner(self, ty)?;
        Some(RelaxedTypeResolution { ty, quality })
    }
}

impl SourceTypeCtx {
    fn log_resolution_hit(&self, simple: &str, internal: &str, rule: &str) {
        let origin = self
            .view
            .as_ref()
            .and_then(|view| view.get_class(internal))
            .map(|class| match class.origin {
                crate::index::ClassOrigin::SourceFile(_) => "source",
                crate::index::ClassOrigin::Jar(_) => "jar_or_jdk",
                crate::index::ClassOrigin::ZipSource { .. } => "zip_source",
                crate::index::ClassOrigin::Unknown => "unknown",
            });
        tracing::debug!(
            simple,
            internal,
            rule,
            lookup = if self.view.is_some() { "index_view" } else { "name_table" },
            origin = ?origin,
            "SourceTypeCtx resolved simple name"
        );
    }

    fn class_exists(&self, internal: &str) -> bool {
        if let Some(view) = &self.view {
            let exists = view.get_class(internal).is_some();
            tracing::trace!(
                lookup = internal,
                path = "index_view",
                found = exists,
                "SourceTypeCtx class existence lookup"
            );
            return exists;
        }
        let exists = self
            .name_table
            .as_ref()
            .is_some_and(|nt| nt.exists(internal));
        tracing::trace!(
            lookup = internal,
            path = "name_table",
            found = exists,
            "SourceTypeCtx class existence lookup"
        );
        exists
    }
}

fn resolve_type_name_strict_inner(
    ctx: &SourceTypeCtx,
    ty: &str,
) -> Option<crate::semantic::types::type_name::TypeName> {
    let ty = ty.trim();
    if ty.is_empty() {
        return None;
    }

    let mut base = ty;
    let mut dims = 0usize;
    while let Some(stripped) = base.strip_suffix("[]") {
        dims += 1;
        base = stripped.trim();
    }

    while base.starts_with('@') {
        if let Some(idx) = base.find(' ') {
            base = base[idx..].trim_start();
        } else {
            return None;
        }
    }

    let (base_name, args_str) = split_generic_base(base)?;

    let base_internal = if base_name.contains('/') {
        ctx.class_exists(base_name).then(|| base_name.to_string())
    } else if base_name.contains('.') {
        let internal = base_name.replace('.', "/");
        ctx.class_exists(&internal).then_some(internal)
    } else {
        ctx.resolve_simple_strict(base_name)
    }?;

    let mut ty = if let Some(args) = args_str {
        let arg_types = split_generic_args(args)
            .into_iter()
            .map(|arg| resolve_type_arg_type(ctx, arg))
            .collect::<Option<Vec<crate::semantic::types::type_name::TypeName>>>()?;
        if arg_types.is_empty() {
            crate::semantic::types::type_name::TypeName::new(base_internal)
        } else {
            crate::semantic::types::type_name::TypeName::with_args(base_internal, arg_types)
        }
    } else {
        crate::semantic::types::type_name::TypeName::new(base_internal)
    };

    if dims > 0 {
        ty = ty.with_array_dims(dims);
    }

    Some(ty)
}

fn resolve_type_name_relaxed_inner(
    ctx: &SourceTypeCtx,
    ty: &str,
) -> Option<(
    crate::semantic::types::type_name::TypeName,
    TypeResolveQuality,
)> {
    let ty = ty.trim();
    if ty.is_empty() {
        return None;
    }

    let mut base = ty;
    let mut dims = 0usize;
    while let Some(stripped) = base.strip_suffix("[]") {
        dims += 1;
        base = stripped.trim();
    }

    while base.starts_with('@') {
        if let Some(idx) = base.find(' ') {
            base = base[idx..].trim_start();
        } else {
            return None;
        }
    }

    let (base_name, args_str) = split_generic_base(base)?;
    let base_internal = resolve_base_internal(ctx, base_name)?;

    let mut quality = TypeResolveQuality::Exact;
    let mut out = if let Some(args) = args_str {
        let mut resolved_args = Vec::new();
        for arg in split_generic_args(args) {
            match resolve_type_arg_type_relaxed(ctx, arg) {
                Some((arg_ty, arg_quality)) => {
                    if arg_quality == TypeResolveQuality::Partial {
                        quality = TypeResolveQuality::Partial;
                    }
                    resolved_args.push(arg_ty);
                }
                None => {
                    // Preserve outer/base even if this inner argument fails.
                    quality = TypeResolveQuality::Partial;
                }
            }
        }
        if resolved_args.is_empty() {
            crate::semantic::types::type_name::TypeName::new(base_internal)
        } else {
            crate::semantic::types::type_name::TypeName::with_args(base_internal, resolved_args)
        }
    } else {
        crate::semantic::types::type_name::TypeName::new(base_internal)
    };

    if dims > 0 {
        out = out.with_array_dims(dims);
    }

    Some((out, quality))
}

fn resolve_base_internal(ctx: &SourceTypeCtx, base_name: &str) -> Option<String> {
    if base_name.contains('/') {
        return ctx.class_exists(base_name).then(|| base_name.to_string());
    }
    if base_name.contains('.') {
        let internal = base_name.replace('.', "/");
        return ctx.class_exists(&internal).then_some(internal);
    }
    ctx.resolve_simple_strict(base_name)
}

fn split_generic_base(ty: &str) -> Option<(&str, Option<&str>)> {
    if let Some(start) = ty.find('<') {
        let mut depth = 0i32;
        for (i, c) in ty.char_indices().skip(start) {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        let base = ty[..start].trim();
                        let args = ty[start + 1..i].trim();
                        return Some((base, Some(args)));
                    }
                }
                _ => {}
            }
        }
        None
    } else {
        Some((ty.trim(), None))
    }
}

fn split_generic_args(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                result.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        result.push(s[start..].trim());
    }
    result.into_iter().filter(|s| !s.is_empty()).collect()
}

fn resolve_type_arg_type(
    ctx: &SourceTypeCtx,
    arg: &str,
) -> Option<crate::semantic::types::type_name::TypeName> {
    let arg = arg.trim();
    if arg.is_empty() {
        return None;
    }
    if arg == "?" {
        return Some(crate::semantic::types::type_name::TypeName::new("*"));
    }
    if let Some(bound) = arg.strip_prefix("? extends ") {
        let inner = resolve_type_arg_type(ctx, bound)?;
        return Some(crate::semantic::types::type_name::TypeName::with_args(
            "+",
            vec![inner],
        ));
    }
    if let Some(bound) = arg.strip_prefix("? super ") {
        let inner = resolve_type_arg_type(ctx, bound)?;
        return Some(crate::semantic::types::type_name::TypeName::with_args(
            "-",
            vec![inner],
        ));
    }

    let resolved = resolve_type_name_strict_inner(ctx, arg)?;
    if resolved.is_array() {
        return None;
    }
    let base = resolved.erased_internal();
    if matches!(
        base,
        "void" | "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double"
    ) {
        return None;
    }
    if base.contains('/') {
        return Some(resolved);
    }
    None
}

fn resolve_type_arg_type_relaxed(
    ctx: &SourceTypeCtx,
    arg: &str,
) -> Option<(
    crate::semantic::types::type_name::TypeName,
    TypeResolveQuality,
)> {
    let arg = arg.trim();
    if arg.is_empty() {
        return None;
    }
    if arg == "?" {
        return Some((
            crate::semantic::types::type_name::TypeName::new("*"),
            TypeResolveQuality::Exact,
        ));
    }
    if let Some(bound) = arg.strip_prefix("? extends ") {
        let inner = resolve_type_arg_type_relaxed(ctx, bound)
            .or_else(|| resolve_type_name_relaxed_inner(ctx, bound))?;
        return Some((
            crate::semantic::types::type_name::TypeName::with_args("+", vec![inner.0]),
            inner.1,
        ));
    }
    if let Some(bound) = arg.strip_prefix("? super ") {
        let inner = resolve_type_arg_type_relaxed(ctx, bound)
            .or_else(|| resolve_type_name_relaxed_inner(ctx, bound))?;
        return Some((
            crate::semantic::types::type_name::TypeName::with_args("-", vec![inner.0]),
            inner.1,
        ));
    }

    resolve_type_name_relaxed_inner(ctx, arg)
}

pub fn build_java_descriptor(
    params_text: &str,
    ret_type: &str,
    type_ctx: &SourceTypeCtx,
) -> String {
    let inner = params_text
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');

    let mut desc = String::from("(");

    if !inner.trim().is_empty() {
        for param in split_params(inner) {
            desc.push_str(&type_ctx.to_descriptor(extract_param_type(param.trim())));
        }
    }

    desc.push(')');
    desc.push_str(&type_ctx.to_descriptor(ret_type.trim()));
    desc
}

/// Parameters are separated by commas, ignoring commas within generic angle brackets.
pub fn split_params(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                result.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        result.push(&s[start..]);
    }
    result
}

/// Extract the type portion from a formal parameter string.
/// Handles generics, arrays, varargs, annotations, and `final`.
/// Walks backward to find the last whitespace outside angle brackets:
/// everything to the left is the type, the rightmost token is the name.
pub(crate) fn extract_param_type(param: &str) -> &str {
    let mut depth = 0i32;
    let mut last_sep = None;
    for (i, b) in param.bytes().enumerate().rev() {
        match b {
            b'>' => depth += 1,
            b'<' => depth -= 1,
            b' ' | b'\t' if depth == 0 => {
                last_sep = Some(i);
                break;
            }
            _ => {}
        }
    }
    match last_sep {
        Some(pos) => param[..pos].trim(),
        None => param,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::NameTable;
    use std::sync::Arc;

    fn name_table(names: &[&str]) -> Arc<NameTable> {
        NameTable::from_names(names.iter().map(|s| Arc::from(*s)).collect())
    }

    #[test]
    fn test_resolve_simple_strict_exact_import() {
        let nt = name_table(&["org/example/Foo"]);
        let ctx = SourceTypeCtx::new(None, vec!["org.example.Foo".into()], Some(nt));
        assert_eq!(
            ctx.resolve_simple_strict("Foo"),
            Some("org/example/Foo".to_string())
        );
    }

    #[test]
    fn test_resolve_simple_strict_wildcard_import() {
        let nt = name_table(&["org/example/Foo"]);
        let ctx = SourceTypeCtx::new(None, vec!["org.example.*".into()], Some(nt));
        assert_eq!(
            ctx.resolve_simple_strict("Foo"),
            Some("org/example/Foo".to_string())
        );
    }

    #[test]
    fn test_resolve_simple_strict_java_lang() {
        let nt = name_table(&["java/lang/String"]);
        let ctx = SourceTypeCtx::new(None, vec![], Some(nt));
        assert_eq!(
            ctx.resolve_simple_strict("String"),
            Some("java/lang/String".to_string())
        );
    }

    #[test]
    fn test_resolve_simple_strict_same_package_wins() {
        let nt = name_table(&["org/example/Foo", "java/lang/Foo"]);
        let ctx = SourceTypeCtx::new(Some(Arc::from("org/example")), vec![], Some(nt));
        assert_eq!(
            ctx.resolve_simple_strict("Foo"),
            Some("org/example/Foo".to_string())
        );
    }

    #[test]
    fn test_resolve_simple_strict_default_package() {
        let nt = name_table(&["Foo", "java/lang/Foo"]);
        let ctx = SourceTypeCtx::new(None, vec![], Some(nt));
        assert_eq!(ctx.resolve_simple_strict("Foo"), Some("Foo".to_string()));
    }

    #[test]
    fn test_resolve_simple_strict_ambiguous_wildcard() {
        let nt = name_table(&["a/Foo", "b/Foo"]);
        let ctx = SourceTypeCtx::new(None, vec!["a.*".into(), "b.*".into()], Some(nt));
        assert_eq!(ctx.resolve_simple_strict("Foo"), None);
    }

    #[test]
    fn test_resolve_simple_strict_ambiguous_exact_imports() {
        let nt = name_table(&["a/Foo", "b/Foo"]);
        let ctx = SourceTypeCtx::new(None, vec!["a.Foo".into(), "b.Foo".into()], Some(nt));
        assert_eq!(ctx.resolve_simple_strict("Foo"), None);
    }

    #[test]
    fn test_resolve_type_name_relaxed_preserves_wildcard_bounds() {
        let nt = name_table(&["java/util/List", "java/lang/Number"]);
        let ctx = SourceTypeCtx::new(None, vec!["java.util.*".into()], Some(nt));

        let resolved = ctx
            .resolve_type_name_relaxed("List<? extends Number>")
            .expect("should resolve");
        assert_eq!(resolved.ty.erased_internal(), "java/util/List");
        assert_eq!(resolved.quality, TypeResolveQuality::Exact);
        assert_eq!(resolved.ty.args.len(), 1);
        assert_eq!(resolved.ty.args[0].erased_internal(), "+");
        assert_eq!(
            resolved.ty.args[0].args[0].erased_internal(),
            "java/lang/Number"
        );
    }

    #[test]
    fn test_resolve_type_name_relaxed_preserves_base_on_partial_args() {
        let nt = name_table(&["java/util/function/Function"]);
        let ctx = SourceTypeCtx::new(None, vec!["java.util.function.*".into()], Some(nt));

        let resolved = ctx
            .resolve_type_name_relaxed("Function<? super T, ? extends K>")
            .expect("base should be preserved");
        assert_eq!(resolved.ty.erased_internal(), "java/util/function/Function");
        assert_eq!(resolved.quality, TypeResolveQuality::Partial);
    }
}
