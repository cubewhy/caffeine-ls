use std::sync::Arc;

/// Per-file type resolution context built from the file's own package + imports.
/// Converts bare Java simple names → JVM internal names following JLS §7.5 priority.
pub struct SourceTypeCtx {
    package: Option<Arc<str>>,
    /// Normalized import strings, e.g. `"java.util.List"` or `"java.util.*"`.
    imports: Vec<Arc<str>>,
    name_table: Option<Arc<crate::index::NameTable>>,
}

impl SourceTypeCtx {
    pub fn new(
        package: Option<Arc<str>>,
        imports: Vec<Arc<str>>,
        name_table: Option<Arc<crate::index::NameTable>>,
    ) -> Self {
        tracing::debug!(
            package = ?package,
            imports = imports.len(),
            has_table = name_table.is_some(),
            table_size = name_table.as_ref().map(|t| t.len()).unwrap_or(0),
            "SourceTypeCtx created"
        );

        Self {
            package,
            imports,
            name_table,
        }
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
    /// Returns `simple` unchanged if unresolvable — never guesses.
    pub fn resolve_simple(&self, simple: &str) -> String {
        let result = self.resolve_simple_inner(simple);
        if result.contains('/') && result != simple {
            tracing::trace!(simple, resolved = %result, "type resolved");
        } else if result == simple && !simple.contains('/') {
            tracing::warn!(
                simple,
                has_table = self.name_table.is_some(),
                "type UNRESOLVED — descriptor will be bare simple name"
            );
        }
        result
    }

    fn resolve_simple_inner(&self, simple: &str) -> String {
        if simple.contains('/') {
            return simple.to_string(); // already internal
        }
        // Rule 1: Single-type-import — JLS §7.5.1
        // The import text itself IS the full qualified name; no index lookup needed.
        for imp in &self.imports {
            let s = imp.as_ref();
            if !s.ends_with(".*") && (s == simple || s.ends_with(&format!(".{}", simple))) {
                return s.replace('.', "/");
            }
        }
        // Rule 2: java.lang.* — JLS §7.5.3 (always implicit)
        let java_lang = format!("java/lang/{}", simple);
        if let Some(nt) = &self.name_table
            && nt.exists(&java_lang)
        {
            return java_lang;
        }

        // Rule 3: Same package — JLS §6.4.1; verify via index, never assume
        if let Some(pkg) = &self.package {
            let candidate = format!("{}/{}", pkg, simple);
            if self
                .name_table
                .as_ref()
                .is_some_and(|nt| nt.exists(&candidate))
            {
                return candidate;
            }
        }
        // Rule 4: Type-import-on-demand (wildcard) — JLS §7.5.2; requires index
        if let Some(nt) = &self.name_table {
            for imp in &self.imports {
                let s = imp.as_ref();
                if s.ends_with(".*") {
                    let pkg = s.trim_end_matches(".*").replace('.', "/");
                    let candidate = format!("{}/{}", pkg, simple);
                    if nt.exists(&candidate) {
                        return candidate;
                    }
                }
            }
        }
        // Unresolvable
        simple.to_string()
    }
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
