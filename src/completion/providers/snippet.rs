use super::super::{
    candidate::{CandidateKind, CompletionCandidate},
    context::{CompletionContext, CursorLocation},
};
use super::CompletionProvider;
use crate::index::GlobalIndex;
use std::sync::Arc;

pub struct SnippetProvider;

/// A snippet rule
struct SnippetRule {
    /// The label displayed to the user (also used for prefix matching)
    label: &'static str,
    aliases: &'static [&'static str],
    /// Brief description
    detail: &'static str,
    /// Sort by score
    score: f32,
    /// Function that generates insert_text
    build: fn(ctx: &CompletionContext) -> String,
    /// Should it be displayed (additional filtering conditions)?
    should_show: fn(ctx: &CompletionContext) -> bool,
}

fn always(_ctx: &CompletionContext) -> bool {
    true
}

/// Only displayed inside the class (with enclosing_class)
fn inside_class(ctx: &CompletionContext) -> bool {
    ctx.enclosing_class.is_some()
}

/// Only displayed inside the class (with enclosing_class)
fn inside_method(ctx: &CompletionContext) -> bool {
    ctx.enclosing_class_member
        .as_ref()
        .map(|it| it.is_method())
        .unwrap_or(false)
}

fn inside_class_but_not_method(ctx: &CompletionContext) -> bool {
    inside_class(ctx) && !inside_method(ctx)
}

/// Only displayed if the file does not yet have a package declaration.
fn no_package_declared(ctx: &CompletionContext) -> bool {
    ctx.enclosing_package.is_none()
}

/// All snippet rules, sorted by priority
fn all_rules() -> &'static [SnippetRule] {
    &[
        SnippetRule {
            label: "package",
            aliases: &[],
            detail: "package declaration",
            score: 30.0,
            build: |ctx| {
                let pkg = ctx
                    .effective_package()
                    .map(|p| p.replace('/', "."))
                    .unwrap_or_default();
                if pkg.is_empty() {
                    "package ${1:com.example};".to_string()
                } else {
                    format!("package {};", pkg)
                }
            },
            should_show: no_package_declared,
        },
        SnippetRule {
            label: "class",
            aliases: &[],
            detail: "public class declaration",
            score: 25.0,
            build: |ctx| {
                let name = ctx.file_stem().unwrap_or("ClassName");
                format!("public class {} {{\n\t${{0}}\n}}", name)
            },
            should_show: always,
        },
        SnippetRule {
            label: "interface",
            aliases: &[],
            detail: "public interface declaration",
            score: 25.0,
            build: |ctx| {
                let name = ctx.file_stem().unwrap_or("MyInterface");
                format!("public interface {} {{\n\t${{0}}\n}}", name)
            },
            should_show: always,
        },
        SnippetRule {
            label: "abstract",
            aliases: &[],
            detail: "public abstract class declaration",
            score: 25.0,
            build: |ctx| {
                let name = ctx.file_stem().unwrap_or("AbstractClass");
                format!("public abstract class {} {{\n\t${{0}}\n}}", name)
            },
            should_show: always,
        },
        SnippetRule {
            label: "enum",
            aliases: &[],
            detail: "public enum declaration",
            score: 25.0,
            build: |ctx| {
                let name = ctx.file_stem().unwrap_or("MyEnum");
                format!("public enum {} {{\n\t${{1}};\n\t${{0}}\n}}", name)
            },
            should_show: always,
        },
        SnippetRule {
            label: "record",
            aliases: &[],
            detail: "public record declaration",
            score: 25.0,
            build: |ctx| {
                let name = ctx.file_stem().unwrap_or("MyRecord");
                format!("public record {}(${{1}}) {{\n\t${{0}}\n}}", name)
            },
            should_show: always,
        },
        SnippetRule {
            label: "annotation",
            aliases: &[],
            detail: "public annotation type declaration",
            score: 25.0,
            build: |ctx| {
                let name = ctx.file_stem().unwrap_or("MyAnnotation");
                format!("public @interface {} {{\n\t${{0}}\n}}", name)
            },
            should_show: always,
        },
        SnippetRule {
            label: "psvm",
            aliases: &["main"],
            detail: "public static void main(String[] args)",
            score: 20.0,
            build: |_| "public static void main(String[] args) {\n\t${0}\n}".to_string(),
            should_show: inside_class_but_not_method,
        },
        SnippetRule {
            label: "sout",
            aliases: &["println"],
            detail: "System.out.println()",
            score: 20.0,
            build: |_| "System.out.println(${0});".to_string(),
            should_show: inside_method,
        },
    ]
}

impl SnippetRule {
    /// Returns the label (main label or an alias) that matches the prefix
    /// Returns None if none match
    fn matched_label(&self, prefix: &str) -> Option<&'static str> {
        if prefix.is_empty() {
            return Some(self.label); // Empty prefix displays the main label
        }
        let p = prefix.to_lowercase();
        if self.label.starts_with(p.as_str()) {
            return Some(self.label);
        }
        self.aliases
            .iter()
            .find(|&alias| alias.starts_with(p.as_str()))
            .map(|v| v as _)
    }
}

impl CompletionProvider for SnippetProvider {
    fn name(&self) -> &'static str {
        "snippet"
    }

    fn provide(
        &self,
        ctx: &CompletionContext,
        _index: &mut GlobalIndex,
    ) -> Vec<CompletionCandidate> {
        let prefix = match &ctx.location {
            CursorLocation::Expression { prefix } => prefix.as_str(),
            CursorLocation::TypeAnnotation { prefix } => prefix.as_str(),
            _ => return vec![],
        };

        tracing::debug!(
            "SnippetProvider: ast_pkg={:?} inferred={:?} effective={:?}",
            ctx.enclosing_package,
            ctx.inferred_package,
            ctx.effective_package()
        );

        all_rules()
            .iter()
            .filter(|rule| (rule.should_show)(ctx))
            .filter_map(|rule| {
                let matched = rule.matched_label(prefix)?;
                let insert = (rule.build)(ctx);
                Some(
                    CompletionCandidate::new(
                        Arc::from(matched),
                        insert,
                        CandidateKind::Snippet,
                        self.name(),
                    )
                    .with_detail(rule.detail)
                    .with_score(rule.score),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use rust_asm::constants::ACC_PUBLIC;

    use super::*;
    use crate::completion::context::{CompletionContext, CurrentClassMember, CursorLocation};
    use crate::completion::type_resolver::parse_return_type_from_descriptor;
    use crate::index::{GlobalIndex, MethodParams, MethodSummary};
    use std::sync::Arc;

    fn ctx_full(
        prefix: &str,
        file_uri: Option<&str>,
        ast_pkg: Option<&str>,
        inferred_pkg: Option<&str>,
        enclosing_class: Option<&str>,
    ) -> CompletionContext {
        let mut c = CompletionContext::new(
            CursorLocation::Expression {
                prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            enclosing_class.map(Arc::from),
            None,
            ast_pkg.map(Arc::from),
            vec![],
        );
        if let Some(uri) = file_uri {
            c = c.with_file_uri(Arc::from(uri));
        }
        if let Some(pkg) = inferred_pkg {
            c = c.with_inferred_package(Arc::from(pkg));
        }
        c
    }

    fn ctx(
        prefix: &str,
        file_uri: Option<&str>,
        ast_pkg: Option<&str>,
        inferred_pkg: Option<&str>,
    ) -> CompletionContext {
        ctx_full(prefix, file_uri, ast_pkg, inferred_pkg, None)
    }

    fn make_method(name: &str, descriptor: &str, flags: u16, is_synthetic: bool) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: flags,
            is_synthetic,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    #[test]
    fn test_package_snippet_not_shown_when_pkg_declared() {
        let mut idx = GlobalIndex::new();
        let c = ctx("pack", None, Some("org/cubewhy"), None);
        let results = SnippetProvider.provide(&c, &mut idx);
        assert!(results.iter().all(|r| r.label.as_ref() != "package"));
    }

    #[test]
    fn test_package_snippet_shown_with_inferred_pkg() {
        let mut idx = GlobalIndex::new();
        let c = ctx("pack", None, None, Some("org/cubewhy"));
        let results = SnippetProvider.provide(&c, &mut idx);
        let pkg = results
            .iter()
            .find(|r| r.label.as_ref() == "package")
            .unwrap();
        assert!(
            pkg.insert_text.contains("org.cubewhy"),
            "{}",
            pkg.insert_text
        );
    }

    #[test]
    fn test_package_snippet_fallback_placeholder() {
        let mut idx = GlobalIndex::new();
        let c = ctx("pack", None, None, None);
        let results = SnippetProvider.provide(&c, &mut idx);
        let pkg = results
            .iter()
            .find(|r| r.label.as_ref() == "package")
            .unwrap();
        assert!(
            pkg.insert_text.contains("${1:com.example}"),
            "{}",
            pkg.insert_text
        );
    }

    #[test]
    fn test_class_snippet_uses_file_stem() {
        let mut idx = GlobalIndex::new();
        let c = ctx("class", Some("file:///path/to/MyService.java"), None, None);
        let results = SnippetProvider.provide(&c, &mut idx);
        let cls = results
            .iter()
            .find(|r| r.label.as_ref() == "class")
            .unwrap();
        assert!(cls.insert_text.contains("MyService"), "{}", cls.insert_text);
    }

    #[test]
    fn test_all_class_like_snippets_on_empty_prefix() {
        let mut idx = GlobalIndex::new();
        let c = ctx("", Some("file:///Foo.java"), None, None);
        let results = SnippetProvider.provide(&c, &mut idx);
        let labels: Vec<&str> = results.iter().map(|r| r.label.as_ref()).collect();
        for expected in &[
            "class",
            "interface",
            "abstract",
            "enum",
            "record",
            "annotation",
        ] {
            assert!(
                labels.contains(expected),
                "missing {}: {:?}",
                expected,
                labels
            );
        }
    }

    #[test]
    fn test_psvm_alias_main_matches() {
        let mut idx = GlobalIndex::new();
        // "main" 应该触发 psvm snippet，label 显示为 "main"
        let c = ctx_full("main", None, None, None, Some("MyClass"));
        let results = SnippetProvider.provide(&c, &mut idx);
        let m = results.iter().find(|r| r.label.as_ref() == "main");
        assert!(
            m.is_some(),
            "alias 'main' should match psvm: {:?}",
            results.iter().map(|r| r.label.as_ref()).collect::<Vec<_>>()
        );
        assert!(m.unwrap().insert_text.contains("public static void main"));
    }

    #[test]
    fn test_psvm_label_also_matches() {
        let mut idx = GlobalIndex::new();
        let c = ctx_full("psvm", None, None, None, Some("MyClass"));
        let results = SnippetProvider.provide(&c, &mut idx);
        assert!(
            results.iter().any(|r| r.label.as_ref() == "psvm"),
            "{:?}",
            results.iter().map(|r| r.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_psvm_not_shown_outside_class() {
        let mut idx = GlobalIndex::new();
        // enclosing_class = None → outside class
        let c = ctx("psvm", None, None, None);
        let results = SnippetProvider.provide(&c, &mut idx);
        assert!(
            results.iter().all(|r| r.label.as_ref() != "psvm"),
            "psvm should not appear outside a class"
        );
    }

    #[test]
    fn test_sout_alias_println() {
        let mut idx = GlobalIndex::new();
        let c = ctx("println", None, None, None).with_enclosing_member(Some(
            CurrentClassMember::Method(Arc::new(make_method(
                "randomMethod",
                "()V",
                ACC_PUBLIC,
                false,
            ))),
        ));
        let results = SnippetProvider.provide(&c, &mut idx);
        let m = results.iter().find(|r| r.label.as_ref() == "println");
        assert!(m.is_some(), "alias 'println' should match sout");
        assert!(m.unwrap().insert_text.contains("System.out.println"));
    }

    #[test]
    fn test_annotation_label() {
        let mut idx = GlobalIndex::new();
        let c = ctx("ann", Some("file:///MyAnno.java"), None, None);
        let results = SnippetProvider.provide(&c, &mut idx);
        assert!(
            results.iter().any(|r| r.label.as_ref() == "annotation"),
            "{:?}",
            results.iter().map(|r| r.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_snippets_in_member_access() {
        let mut idx = GlobalIndex::new();
        let mut c = ctx("class", Some("file:///Foo.java"), None, None);
        c.location = CursorLocation::MemberAccess {
            receiver_type: None,
            member_prefix: "class".to_string(),
            receiver_expr: "obj".to_string(),
            arguments: None,
        };
        assert!(SnippetProvider.provide(&c, &mut idx).is_empty());
    }

    #[test]
    fn test_snippet_has_cursor_placeholder() {
        let mut idx = GlobalIndex::new();
        let c = ctx("class", Some("file:///Foo.java"), None, None);
        let results = SnippetProvider.provide(&c, &mut idx);
        let cls = results
            .iter()
            .find(|r| r.label.as_ref() == "class")
            .unwrap();
        assert!(cls.insert_text.contains("${0}"), "{}", cls.insert_text);
    }

    #[test]
    fn test_prefix_filter() {
        let mut idx = GlobalIndex::new();
        let c = ctx("cl", Some("file:///Foo.java"), None, None);
        let results = SnippetProvider.provide(&c, &mut idx);
        assert!(results.iter().any(|r| r.label.as_ref() == "class"));
        assert!(results.iter().all(|r| r.label.as_ref() != "interface"));
    }

    #[test]
    fn test_empty_prefix_shows_all_valid() {
        let mut idx = GlobalIndex::new();
        // 在 class 内，有 ast_pkg，空前缀
        let c = ctx_full(
            "",
            Some("file:///Foo.java"),
            Some("org/cubewhy"),
            None,
            Some("Foo"),
        );
        let results = SnippetProvider.provide(&c, &mut idx);
        let labels: Vec<&str> = results.iter().map(|r| r.label.as_ref()).collect();
        // psvm 应该出现（在 class 内）
        assert!(
            labels.contains(&"psvm"),
            "psvm should appear inside class: {:?}",
            labels
        );
        // package 不应该出现（已有 pkg 声明）
        assert!(
            !labels.contains(&"package"),
            "package should be hidden: {:?}",
            labels
        );
    }
}
