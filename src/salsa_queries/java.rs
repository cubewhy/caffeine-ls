/// Java-specific Salsa queries
///
/// These queries handle Java-specific parsing and analysis.
mod common;
mod completion;
mod hints;
mod indexing;
mod resolve;
mod scope;

pub use completion::{
    build_java_semantic_context, extract_java_completion_context,
    extract_java_completion_context_at_offset, extract_java_semantic_context_at_offset,
    extract_java_semantic_context_from_source_at_offset,
};
pub use hints::{compute_java_inlay_hints, infer_java_variable_type};
pub use indexing::{
    extract_java_imports, extract_java_module_descriptor, extract_java_package,
    extract_java_static_imports, extract_java_static_imports_from_source, parse_java_classes,
    parse_java_classes_with_index_view,
};
pub use resolve::{is_java_local_variable, resolve_java_symbol};
pub use scope::{
    count_java_locals_in_scope, extract_java_enclosing_method, find_java_enclosing_class_name,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ClassOrigin, WorkspaceIndex};
    use crate::salsa_db::{Database, FileId, SourceFile};
    use crate::salsa_queries::context::{
        CursorLocationData, JavaModuleContextKindData, line_col_to_offset,
    };
    use crate::semantic::context::CursorLocation;
    use ropey::Rope;
    use std::sync::Arc;
    use tower_lsp::lsp_types::Url;

    #[test]
    fn test_parse_java_classes() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            "package com.example;\npublic class Test { void foo() {} }".to_string(),
            Arc::from("java"),
        );

        let classes = parse_java_classes(&db, file);
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name.as_ref(), "Test");
        assert_eq!(classes[0].package.as_deref(), Some("com/example"));
        assert_eq!(classes[0].methods.len(), 1);
        assert_eq!(classes[0].methods[0].name.as_ref(), "foo");
    }

    #[test]
    fn test_extract_java_package() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            "package org.example.test;\npublic class Test {}".to_string(),
            Arc::from("java"),
        );

        let package = extract_java_package(&db, file);
        assert_eq!(package.as_deref(), Some("org/example/test"));
    }

    #[test]
    fn test_extract_java_imports() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            "import java.util.*;\nimport java.io.File;\npublic class Test {}".to_string(),
            Arc::from("java"),
        );

        let imports = extract_java_imports(&db, file);
        assert_eq!(imports.len(), 2);
    }

    #[test]
    fn test_extract_java_context_keeps_system_out_as_member_access() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let content = "class Test { void f() { System.out.println } }";
        let rope = Rope::from_str(content);
        let byte_offset = content.find("println").unwrap() + "println".len();
        let char_idx = rope.byte_to_char(byte_offset);
        let line = rope.char_to_line(char_idx) as u32;
        let character = (char_idx - rope.line_to_char(line as usize)) as u32;
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            content.to_string(),
            Arc::from("java"),
        );

        let ctx = extract_java_completion_context(&db, file, line, character, None);

        match &ctx.location {
            CursorLocationData::MemberAccess {
                receiver_expr,
                member_prefix,
                ..
            } => {
                assert_eq!(receiver_expr.as_ref(), "System.out");
                assert_eq!(member_prefix.as_ref(), "println");
            }
            other => panic!("expected MemberAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_java_context_keeps_user_dot_as_member_access() {
        let db = Database::default();
        let uri = Url::parse("file:///test/User.java").unwrap();
        let content = r#"
class User {
    void test() {
        User user = new User();
        user.
    }
}
"#;
        let rope = Rope::from_str(content);
        let byte_offset = content.find("user.").unwrap() + "user.".len();
        let char_idx = rope.byte_to_char(byte_offset);
        let line = rope.char_to_line(char_idx) as u32;
        let character = (char_idx - rope.line_to_char(line as usize)) as u32;
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            content.to_string(),
            Arc::from("java"),
        );

        let ctx = extract_java_completion_context(&db, file, line, character, Some('.'));

        match &ctx.location {
            CursorLocationData::MemberAccess {
                receiver_expr,
                member_prefix,
                ..
            } => {
                assert_eq!(receiver_expr.as_ref(), "user");
                assert!(member_prefix.is_empty());
            }
            other => panic!("expected MemberAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_java_context_at_offset_matches_line_character_query() {
        let db = Database::default();
        let uri = Url::parse("file:///test/User.java").unwrap();
        let content = r#"
class User {
    void test() {
        User user = new User();
        user.
    }
}
"#;
        let rope = Rope::from_str(content);
        let byte_offset = content.find("user.").unwrap() + "user.".len();
        let char_idx = rope.byte_to_char(byte_offset);
        let line = rope.char_to_line(char_idx) as u32;
        let character = (char_idx - rope.line_to_char(line as usize)) as u32;
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            content.to_string(),
            Arc::from("java"),
        );

        let by_line_col = extract_java_completion_context(&db, file, line, character, Some('.'));
        let by_offset =
            extract_java_completion_context_at_offset(&db, file, byte_offset, Some('.'));

        assert_eq!(by_line_col.as_ref(), by_offset.as_ref());
    }

    #[test]
    fn test_extract_java_context_marks_module_target_module_position() {
        let db = Database::default();
        let uri = Url::parse("file:///test/module-info.java").unwrap();
        let content = "module demo { exports com.example.api to com.example.te; }";
        let rope = Rope::from_str(content);
        let byte_offset = content.find("com.example.te").unwrap() + "com.example.te".len();
        let char_idx = rope.byte_to_char(byte_offset);
        let line = rope.char_to_line(char_idx) as u32;
        let character = (char_idx - rope.line_to_char(line as usize)) as u32;
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            content.to_string(),
            Arc::from("java"),
        );

        let ctx = extract_java_completion_context(&db, file, line, character, None);

        assert_eq!(
            ctx.java_module_context,
            Some(JavaModuleContextKindData::TargetModule)
        );
        assert_eq!(ctx.query.as_ref(), "com.example.te");
        assert!(matches!(
            ctx.location,
            CursorLocationData::Expression { .. }
        ));
    }

    #[test]
    fn test_extract_java_context_marks_requires_module_after_trailing_dot() {
        let db = Database::default();
        let uri = Url::parse("file:///test/module-info.java").unwrap();
        let content = "module demo { requires java. }";
        let rope = Rope::from_str(content);
        let byte_offset = content.find("java.").unwrap() + "java.".len();
        let char_idx = rope.byte_to_char(byte_offset);
        let line = rope.char_to_line(char_idx) as u32;
        let character = (char_idx - rope.line_to_char(line as usize)) as u32;
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            content.to_string(),
            Arc::from("java"),
        );

        let ctx = extract_java_completion_context(&db, file, line, character, Some('.'));

        assert_eq!(
            ctx.java_module_context,
            Some(JavaModuleContextKindData::RequiresModule)
        );
        assert_eq!(ctx.query.as_ref(), "java.");
        assert!(matches!(
            ctx.location,
            CursorLocationData::Expression { .. }
        ));
    }

    #[test]
    fn test_extract_java_enclosing_method_preserves_static_main() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let content = indoc::indoc! {r#"
            class Test {
                static void main(String[] args) {

                }
            }
        "#};
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            content.to_string(),
            Arc::from("java"),
        );
        let blank_line = content
            .lines()
            .enumerate()
            .find_map(|(line, text)| text.trim().is_empty().then_some(line as u32))
            .expect("blank line");
        let offset = line_col_to_offset(content, blank_line, 0).expect("blank-line offset");

        let method = extract_java_enclosing_method(&db, file, offset).expect("enclosing method");

        assert_eq!(method.name.as_ref(), "main");
        assert!(
            method.descriptor.as_ref().starts_with("([L"),
            "expected source method descriptor to preserve the array parameter shape, got {}",
            method.descriptor
        );
        assert_ne!(
            method.access_flags & rust_asm::constants::ACC_STATIC,
            0,
            "expected enclosing method to preserve static access"
        );
    }

    #[test]
    fn test_extract_java_semantic_context_at_offset_materializes_var_receiver() {
        let workspace_index = crate::index::WorkspaceIndexHandle::new(WorkspaceIndex::new());
        let source = indoc::indoc! {r#"
            package org.example;

            class Main {
                void foo(String name, int age) {
                    var a = new User(name, age);
                    a.
                }
            }

            class User {
                User(String name, int age) {}

                void greet() {}
            }
        "#}
        .to_string();
        let parsed = crate::language::java::class_parser::parse_java_source_via_tree_for_test(
            &source,
            ClassOrigin::Unknown,
            None,
        );
        workspace_index.update(|index| index.add_classes(parsed));

        let db = Database::with_workspace_index(workspace_index.clone());
        let uri = Url::parse("file:///test/Main.java").unwrap();
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));
        let offset = source.find("a.").expect("member access") + 2;
        let view = workspace_index.load().view(crate::index::IndexScope {
            module: crate::index::ModuleId::ROOT,
        });

        let ctx =
            extract_java_semantic_context_at_offset(&db, file, offset, view, None).expect("ctx");

        let local = ctx
            .local_variables
            .iter()
            .find(|local| local.name.as_ref() == "a")
            .expect("local a");
        assert_eq!(local.type_internal.erased_internal(), "org/example/User");

        match &ctx.location {
            CursorLocation::MemberAccess {
                receiver_type,
                receiver_semantic_type,
                ..
            } => {
                assert_eq!(receiver_type.as_deref(), Some("org/example/User"));
                assert_eq!(
                    receiver_semantic_type
                        .as_ref()
                        .map(|ty| ty.erased_internal()),
                    Some("org/example/User")
                );
            }
            other => panic!("expected MemberAccess, got {other:?}"),
        }
    }
}
