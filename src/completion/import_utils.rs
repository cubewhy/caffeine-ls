use crate::index::ClassMetadata;
use std::sync::Arc;

/// Check if an FQN has been overridden by existing imports (exact match + wildcard + same package)
///
/// - `fqn`: Dotted FQN, e.g., "org.cubewhy.RandomClass"
/// - `existing_imports`: Existing import statements in the file
/// - `enclosing_package`: The package containing the current file (internal format, e.g., "org/cubewhy/a")
///
/// Check if an FQN needs an import statement
///
/// Cases where imports are not needed:
/// 1. Exact match with an existing import
/// 2. Wildcard import already overridden (same package level, not across sub-packages)
/// 3. Same package (enclosing_package is the same as the package of fqn)
/// 4. java.lang package (auto-import)
/// 5. Default package (classes without a package name)
pub fn is_import_needed(
    fqn: &str,
    existing_imports: &[Arc<str>],
    enclosing_package: Option<&str>,
) -> bool {
    // default package
    if !fqn.contains('.') {
        return false;
    }

    if is_java_lang(fqn) {
        return false;
    }

    // exact match
    if existing_imports.iter().any(|imp| imp.as_ref() == fqn) {
        return false;
    }

    // wildcard
    if existing_imports.iter().any(|imp| wildcard_covers(imp, fqn)) {
        return false;
    }

    // same package
    if let Some(enc_pkg) = enclosing_package
        && same_package(fqn, enc_pkg)
    {
        return false;
    }

    true
}

/// Determinate a class is in java.lang
fn is_java_lang(fqn: &str) -> bool {
    // Only classes directly under the java.lang package are automatically imported; sub-packages are not included.
    // e.g. "java.lang.String" -> true
    //      "java.lang.reflect.Method" -> false
    if let Some(rest) = fqn.strip_prefix("java.lang.") {
        !rest.contains('.')
    } else {
        false
    }
}

/// Check if fqn is in the same package as enclosing_package
/// enclosing_package is in internal format ("org/cubewhy/a"), fqn is in dotted-part format ("org.cubewhy.a.Main")
fn same_package(fqn: &str, enclosing_package: &str) -> bool {
    if enclosing_package.is_empty() {
        // Default package: fqn must also be the default package (without '.')
        return !fqn.contains('.');
    }
    let enc_dot = enclosing_package.replace('/', ".");
    match fqn.rfind('.') {
        Some(pos) => fqn[..pos] == enc_dot,
        None => false, // fqn no package, enclosing has package -> different packages
    }
}

/// Check if wildcard imports override fqn
/// "org.cubewhy.*" covers "org.cubewhy.Foo" but NOT "org.cubewhy.sub.Bar"
fn wildcard_covers(wildcard_import: &str, fqn: &str) -> bool {
    let pkg = match wildcard_import.strip_suffix(".*") {
        Some(p) => p,
        None => return false,
    };
    match fqn.strip_prefix(pkg) {
        Some(rest) => rest.starts_with('.') && !rest[1..].contains('.'),
        None => false,
    }
}

/// Given a ClassMetadata, compute its point score FQN
pub fn fqn_of_meta(meta: &ClassMetadata) -> String {
    match &meta.package {
        Some(pkg) => format!("{}.{}", pkg.replace('/', "."), meta.name),
        None => meta.name.to_string(),
    }
}

/// Extract the list of existing imports from the source text
pub fn extract_imports_from_source(source: &str) -> Vec<Arc<str>> {
    source
        .lines()
        .filter_map(|line| {
            let t = line.trim();
            t.strip_prefix("import ")
                .map(|rest| rest.trim_end_matches(';').trim().into())
        })
        .filter(|s: &Arc<str>| !s.is_empty())
        .collect()
}

/// Extract package names from Java/Kotlin source text (internal format, such as "org/cubewhy/a")
/// Used for fallback checks in the converter layer
pub fn extract_package_from_source(source: &str) -> Option<String> {
    source.lines().find_map(|line| {
        let t = line.trim();
        t.strip_prefix("package ")
            .map(|rest| rest.trim_end_matches(';').trim().replace('.', "/"))
            .filter(|s| !s.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_java_lang_string_not_needed() {
        assert!(!is_import_needed("java.lang.String", &[], None));
    }

    #[test]
    fn test_java_lang_object_not_needed() {
        assert!(!is_import_needed("java.lang.Object", &[], None));
    }

    #[test]
    fn test_java_lang_subpackage_needed() {
        // java.lang.reflect.Method 不是直接在 java.lang 下
        assert!(is_import_needed("java.lang.reflect.Method", &[], None));
    }

    #[test]
    fn test_java_lang_runnable_not_needed() {
        assert!(!is_import_needed("java.lang.Runnable", &[], None));
    }

    #[test]
    fn test_default_package_not_needed() {
        // 无包名的类（默认包）不需要 import
        assert!(!is_import_needed("MyClass", &[], None));
        assert!(!is_import_needed("MyClass", &[], Some("org/cubewhy")));
    }

    // ── 精确 import ───────────────────────────────────────────────────────

    #[test]
    fn test_exact_import_not_needed() {
        assert!(!is_import_needed(
            "org.cubewhy.RandomClass",
            &["org.cubewhy.RandomClass".into()],
            None,
        ));
    }

    #[test]
    fn test_exact_import_different_class_needed() {
        assert!(is_import_needed(
            "org.cubewhy.OtherClass",
            &["org.cubewhy.RandomClass".into()],
            None,
        ));
    }

    // ── 通配符 import ─────────────────────────────────────────────────────

    #[test]
    fn test_wildcard_covers_direct_child() {
        assert!(!is_import_needed(
            "org.cubewhy.RandomClass",
            &["org.cubewhy.*".into()],
            None,
        ));
    }

    #[test]
    fn test_wildcard_does_not_cover_subpackage() {
        assert!(is_import_needed(
            "org.cubewhy.sub.Foo",
            &["org.cubewhy.*".into()],
            None,
        ));
    }

    #[test]
    fn test_wildcard_does_not_cover_sibling_package() {
        // "org.cubewhy2.Foo" is NOT covered by "org.cubewhy.*"
        assert!(is_import_needed(
            "org.cubewhy2.Foo",
            &["org.cubewhy.*".into()],
            None,
        ));
    }

    #[test]
    fn test_multiple_wildcards_one_covers() {
        assert!(!is_import_needed(
            "java.util.List",
            &["org.cubewhy.*".into(), "java.util.*".into()],
            None,
        ));
    }

    // ── 同包 ──────────────────────────────────────────────────────────────

    #[test]
    fn test_same_package_not_needed() {
        assert!(!is_import_needed(
            "org.cubewhy.a.Main",
            &[],
            Some("org/cubewhy/a"),
        ));
    }

    #[test]
    fn test_same_package_slash_notation() {
        assert!(!is_import_needed(
            "org.cubewhy.a.Helper",
            &[],
            Some("org/cubewhy/a"),
        ));
    }

    #[test]
    fn test_different_package_needed() {
        assert!(is_import_needed(
            "org.cubewhy.RandomClass",
            &[],
            Some("org/cubewhy/a"),
        ));
    }

    #[test]
    fn test_subpackage_is_different_package() {
        // org.cubewhy.a.sub.Foo 和 enclosing org/cubewhy/a 不同包
        assert!(is_import_needed(
            "org.cubewhy.a.sub.Foo",
            &[],
            Some("org/cubewhy/a"),
        ));
    }

    // ── 组合场景 ──────────────────────────────────────────────────────────

    #[test]
    fn test_already_imported_via_wildcard_and_same_pkg() {
        // 通配符覆盖，同时也在同包 → 不需要
        assert!(!is_import_needed(
            "org.cubewhy.a.Foo",
            &["org.cubewhy.a.*".into()],
            Some("org/cubewhy/a"),
        ));
    }

    #[test]
    fn test_no_imports_no_package_foreign_class_needed() {
        assert!(is_import_needed("org.cubewhy.Foo", &[], None));
    }

    // ── wildcard_covers 单元测试 ──────────────────────────────────────────

    #[test]
    fn test_wildcard_covers_basic() {
        assert!(wildcard_covers("org.cubewhy.*", "org.cubewhy.Foo"));
    }

    #[test]
    fn test_wildcard_not_covers_subpkg() {
        assert!(!wildcard_covers("org.cubewhy.*", "org.cubewhy.sub.Foo"));
    }

    #[test]
    fn test_wildcard_not_covers_unrelated() {
        assert!(!wildcard_covers("org.cubewhy.*", "org.other.Foo"));
    }

    #[test]
    fn test_non_wildcard_import_not_covers() {
        assert!(!wildcard_covers("org.cubewhy.Foo", "org.cubewhy.Foo"));
    }

    // ── same_package 单元测试 ─────────────────────────────────────────────

    #[test]
    fn test_same_package_basic() {
        assert!(same_package("org.cubewhy.a.Foo", "org/cubewhy/a"));
    }

    #[test]
    fn test_same_package_different() {
        assert!(!same_package("org.cubewhy.Foo", "org/cubewhy/a"));
    }

    #[test]
    fn test_same_package_default_pkg_fqn_no_dot() {
        // enclosing 是默认包（空串），fqn 也是默认包
        assert!(same_package("Foo", ""));
    }

    #[test]
    fn test_same_package_default_pkg_fqn_has_dot() {
        // enclosing 是默认包，fqn 有包 → 不同
        assert!(!same_package("org.Foo", ""));
    }

    // ── is_java_lang 单元测试 ─────────────────────────────────────────────

    #[test]
    fn test_is_java_lang_direct() {
        assert!(is_java_lang("java.lang.String"));
        assert!(is_java_lang("java.lang.Integer"));
        assert!(is_java_lang("java.lang.Thread"));
    }

    #[test]
    fn test_is_java_lang_subpackage() {
        assert!(!is_java_lang("java.lang.reflect.Method"));
        assert!(!is_java_lang("java.lang.annotation.Retention"));
    }

    #[test]
    fn test_is_not_java_lang() {
        assert!(!is_java_lang("java.util.List"));
        assert!(!is_java_lang("org.cubewhy.Foo"));
    }

    #[test]
    fn test_extract_package_basic() {
        let src = "package org.cubewhy.a;\nclass A {}";
        assert_eq!(
            extract_package_from_source(src).as_deref(),
            Some("org/cubewhy/a")
        );
    }

    #[test]
    fn test_extract_package_none() {
        let src = "class A {}";
        assert!(extract_package_from_source(src).is_none());
    }

    #[test]
    fn test_extract_package_default_pkg() {
        // "package ;" 这种畸形情况返回 None
        let src = "class A {}";
        assert!(extract_package_from_source(src).is_none());
    }
}
