use std::sync::Arc;
use tower_lsp::lsp_types::*;

use crate::completion::candidate::{CandidateKind, CompletionCandidate};
use crate::completion::fuzzy::fuzzy_match;
use crate::index::{IndexScope, IndexView};

/// Generates all candidate classes (classes + packages) based on the import prefix.
/// This is the unified entry point for ImportProvider and PackageProvider in import scenarios.
pub fn candidates_for_import(
    prefix: &str,
    _scope: IndexScope,
    index: &IndexView,
) -> Vec<CompletionCandidate> {
    if prefix.is_empty() {
        return vec![];
    }

    match prefix.rfind('.') {
        Some(dot_pos) => {
            let pkg_path = &prefix[..dot_pos];
            let name_prefix = &prefix[dot_pos + 1..];
            let internal_pkg = pkg_path.replace('.', "/");

            let mut results = Vec::new();

            // 当前包下的类，用 fuzzy 匹配
            for meta in index.classes_in_package(&internal_pkg) {
                if !name_prefix.is_empty() && fuzzy_match(name_prefix, &meta.name).is_none() {
                    continue;
                }
                let fqn = format!("{}.{}", pkg_path, meta.name);
                let score = if name_prefix.is_empty() {
                    70.0
                } else {
                    70.0 + fuzzy_match(name_prefix, &meta.name).unwrap_or(0) as f32 * 0.01
                };
                results.push(
                    CompletionCandidate::new(
                        Arc::from(fqn.as_str()),
                        fqn.clone(),
                        CandidateKind::ClassName,
                        "import",
                    )
                    .with_detail(meta.name.to_string())
                    .with_score(score),
                );
            }

            // 子包，用 fuzzy 匹配
            let pkg_prefix_slash = format!("{}/", internal_pkg);
            let mut sub_packages: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            for meta in index.iter_all_classes() {
                if let Some(pkg) = &meta.package
                    && pkg.starts_with(&pkg_prefix_slash)
                {
                    let rest = &pkg[pkg_prefix_slash.len()..];
                    let sub = rest.split('/').next().unwrap_or("");
                    if !sub.is_empty()
                        && (name_prefix.is_empty() || fuzzy_match(name_prefix, sub).is_some())
                    {
                        sub_packages.insert(sub.to_string());
                    }
                }
            }
            for sub in sub_packages {
                let insert = format!("{}.{}.", pkg_path, sub);
                results.push(
                    CompletionCandidate::new(
                        Arc::from(insert.as_str()),
                        insert,
                        CandidateKind::Package,
                        "import",
                    )
                    .with_detail(format!("{}.{}", pkg_path, sub))
                    .with_score(65.0),
                );
            }

            results
        }
        None => {
            // 无点：顶层包 + 类名，都用 fuzzy 匹配
            let mut results = Vec::new();

            // 顶层包
            let mut tops: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for meta in index.iter_all_classes() {
                if let Some(pkg) = &meta.package {
                    let top = pkg.split('/').next().unwrap_or("");
                    if !top.is_empty() && fuzzy_match(prefix, top).is_some() {
                        tops.insert(top.to_string());
                    }
                }
            }
            for top in tops {
                let insert = format!("{}.", top);
                results.push(
                    CompletionCandidate::new(
                        Arc::from(insert.as_str()),
                        insert,
                        CandidateKind::Package,
                        "import",
                    )
                    .with_detail(format!("package {}", top))
                    .with_score(60.0),
                );
            }

            // 类名，fuzzy 匹配
            for meta in index.iter_all_classes().into_iter().take(200) {
                if fuzzy_match(prefix, &meta.name).is_some() {
                    let fqn = fqn_of_meta(&meta);
                    let score = 55.0 + fuzzy_match(prefix, &meta.name).unwrap_or(0) as f32 * 0.01;
                    results.push(
                        CompletionCandidate::new(
                            Arc::from(fqn.as_str()),
                            fqn.clone(),
                            CandidateKind::ClassName,
                            "import",
                        )
                        .with_detail(meta.name.to_string())
                        .with_score(score),
                    );
                }
            }

            results
        }
    }
}

fn fqn_of_meta(meta: &Arc<crate::index::ClassMetadata>) -> String {
    match &meta.package {
        Some(pkg) => format!("{}.{}", pkg.replace('/', "."), meta.name),
        None => meta.name.to_string(),
    }
}

/// import 场景下的 textEdit：替换整个 import 路径部分（支持多行 import）。
///
/// 策略：
/// - 同行 import：替换 "import " 后到行尾（去掉分号/注释）
/// - 换行 import：只替换包名所在行的缩进后内容
pub fn make_import_text_edit(
    insert_text: &str,
    source: &str,
    position: Position,
) -> Option<CompletionTextEdit> {
    let lines: Vec<&str> = source.lines().collect();
    let current_line = lines.get(position.line as usize)?;

    // 找 import 关键字所在行
    let import_line_idx = if current_line.trim_start().starts_with("import") {
        position.line as usize
    } else {
        (0..position.line as usize)
            .rev()
            .find(|&i| lines[i].trim_start().starts_with("import"))?
    };

    let (start_line, start_char) = if import_line_idx == position.line as usize {
        // 同行：从 "import " 后开始
        let line_str = lines[import_line_idx];
        let kw_pos = line_str.find("import")?;
        (position.line, (kw_pos + "import ".len()) as u32)
    } else {
        // 换行：只替换当前行，从缩进后开始
        let indent = current_line.len() - current_line.trim_start().len();
        (position.line, indent as u32)
    };

    // 结束：当前行去掉分号和注释
    let end_char = current_line
        .find(';')
        .or_else(|| current_line.find("//"))
        .map(|p| p as u32)
        .unwrap_or(current_line.len() as u32);

    Some(CompletionTextEdit::Edit(TextEdit {
        range: Range {
            start: Position {
                line: start_line,
                character: start_char,
            },
            end: Position {
                line: position.line,
                character: end_char,
            },
        },
        new_text: insert_text.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, IndexView, ModuleId, WorkspaceIndex,
    };
    use rust_asm::constants::ACC_PUBLIC;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            root_scope(),
            vec![
                make_cls("org/cubewhy", "Main"),
                make_cls("org/cubewhy", "RealMain"),
                make_cls("org/cubewhy/utils", "StringUtil"),
                make_cls("java/util", "ArrayList"),
                make_cls("java/util", "HashMap"),
                make_cls("java/lang", "String"),
            ],
        );
        idx
    }

    fn make_view() -> IndexView {
        let idx = make_index();
        idx.view(root_scope())
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

    // ── candidates_for_import ─────────────────────────────────────────────

    #[test]
    fn test_candidates_pkg_dot_lists_classes_and_subpkgs() {
        let view = make_view();
        let results = candidates_for_import("org.cubewhy.", root_scope(), &view);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"org.cubewhy.Main"), "{:?}", labels);
        assert!(labels.contains(&"org.cubewhy.RealMain"), "{:?}", labels);
        assert!(labels.contains(&"org.cubewhy.utils."), "{:?}", labels);
    }

    #[test]
    fn test_candidates_top_level_pkg() {
        let view = make_view();
        let results = candidates_for_import("org", root_scope(), &view);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"org."), "{:?}", labels);
    }

    #[test]
    fn test_candidates_uppercase_matches_class() {
        let view = make_view();
        let results = candidates_for_import("Array", root_scope(), &view);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"java.util.ArrayList"), "{:?}", labels);
    }

    #[test]
    fn test_candidates_subpkg_prefix() {
        let view = make_view();
        let results = candidates_for_import("java.u", root_scope(), &view);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"java.util."), "{:?}", labels);
        assert!(!labels.iter().any(|l| l.contains("lang")), "{:?}", labels);
    }

    #[test]
    fn test_candidates_label_is_fqn_for_classes() {
        let view = make_view();
        let results = candidates_for_import("org.cubewhy.Ma", root_scope(), &view);
        let main = results
            .iter()
            .find(|c| c.label.as_ref() == "org.cubewhy.Main")
            .unwrap();
        assert_eq!(main.insert_text, "org.cubewhy.Main");
        assert_eq!(main.kind, CandidateKind::ClassName);
    }

    #[test]
    fn test_candidates_pkg_kind_is_package() {
        let view = make_view();
        let results = candidates_for_import("org.cubewhy.", root_scope(), &view);
        let utils = results
            .iter()
            .find(|c| c.label.as_ref() == "org.cubewhy.utils.")
            .unwrap();
        assert_eq!(utils.kind, CandidateKind::Package);
        assert!(utils.insert_text.ends_with('.'));
    }

    // ── make_import_text_edit ─────────────────────────────────────────────

    #[test]
    fn test_text_edit_same_line() {
        let source = "import org.cubewhy.\nclass A {}";
        let pos = Position {
            line: 0,
            character: 19,
        };
        let edit = make_import_text_edit("org.cubewhy.Main", source, pos).unwrap();
        if let CompletionTextEdit::Edit(e) = edit {
            assert_eq!(e.range.start.line, 0);
            assert_eq!(e.range.start.character, 7); // after "import "
            assert_eq!(e.range.end.line, 0);
            assert_eq!(e.new_text, "org.cubewhy.Main");
        }
    }

    #[test]
    fn test_text_edit_multiline() {
        let source = "import\n    org.cubewhy.\nclass A {}";
        let pos = Position {
            line: 1,
            character: 15,
        };
        let edit = make_import_text_edit("org.cubewhy.Main", source, pos).unwrap();
        if let CompletionTextEdit::Edit(e) = edit {
            // 换行场景：只替换当前行（包名行），从缩进后开始
            assert_eq!(e.range.start.line, 1);
            assert_eq!(e.range.start.character, 4); // 4 spaces indent
            assert_eq!(e.range.end.line, 1);
            assert_eq!(e.new_text, "org.cubewhy.Main");
        }
    }

    #[test]
    fn test_text_edit_with_semicolon() {
        let source = "import org.cubewhy.Main;\nclass A {}";
        let pos = Position {
            line: 0,
            character: 23,
        };
        let edit = make_import_text_edit("org.cubewhy.Main", source, pos).unwrap();
        if let CompletionTextEdit::Edit(e) = edit {
            // end_char 应该在分号前
            assert_eq!(e.range.end.character, 23); // "import org.cubewhy.Main" = 23 chars
        }
    }

    #[test]
    fn test_text_edit_with_inline_comment() {
        let source = "import org.; // comment\nclass A {}";
        let pos = Position {
            line: 0,
            character: 11,
        };
        let edit = make_import_text_edit("org.cubewhy.Main", source, pos).unwrap();
        if let CompletionTextEdit::Edit(e) = edit {
            // end_char 应该在 "//" 前（分号位置）
            assert_eq!(e.range.end.character, 11); // "import org." = 11, then ";"
        }
    }

    #[test]
    fn test_candidates_pkg_with_name_prefix() {
        let view = make_view();
        let results = candidates_for_import("org.cubewhy.Ma", root_scope(), &view);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"org.cubewhy.Main"), "{:?}", labels);
        // RealMain fuzzy 匹配 "Ma" 也应该出现（Ma 是 RealMain 的子序列）
        assert!(labels.contains(&"org.cubewhy.RealMain"), "{:?}", labels);
    }

    #[test]
    fn test_candidates_subpkg_fuzzy() {
        let view = make_view();
        // "utl" fuzzy 匹配 "utils"
        let results = candidates_for_import("org.cubewhy.utl", root_scope(), &view);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"org.cubewhy.utils."), "{:?}", labels);
    }

    #[test]
    fn test_candidates_no_dot_fuzzy_class() {
        let view = make_view();
        // "real" fuzzy 匹配 "RealMain"
        let results = candidates_for_import("real", root_scope(), &view);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"org.cubewhy.RealMain"), "{:?}", labels);
    }
}
