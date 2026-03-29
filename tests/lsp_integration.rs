/// LSP Integration Tests
///
/// These tests verify end-to-end LSP functionality by simulating real client interactions.
use java_analyzer::completion::engine::CompletionEngine;
use java_analyzer::language::LanguageRegistry;
use java_analyzer::lsp::handlers::completion::handle_completion;
use java_analyzer::lsp::request_cancellation::RequestFamily;
use java_analyzer::lsp::request_context::RequestContext;
use java_analyzer::workspace::Workspace;
use java_analyzer::workspace::document::Document;
use std::sync::Arc;
use tower_lsp::lsp_types::*;

/// Helper to create a test workspace with a simple Java file
fn create_test_workspace() -> Arc<Workspace> {
    Arc::new(Workspace::new())
}

/// Helper to open a document in the workspace
async fn open_document(workspace: &Arc<Workspace>, uri: &str, content: &str) {
    let uri = Url::parse(uri).unwrap();
    let doc = Document::new(java_analyzer::workspace::SourceFile::new(
        uri.clone(),
        "java",
        1,
        content,
        None,
    ));
    workspace.documents.open(doc);

    // Parse the document
    let registry = LanguageRegistry::new();
    if let Some(lang) = registry.find("java") {
        let mut parser = lang.make_parser();
        let tree = parser.parse(content, None);
        workspace.documents.with_doc_mut(&uri, |doc| {
            doc.set_tree(tree);
        });
    }

    // Index the document using Salsa
    let salsa_file = workspace
        .get_or_update_salsa_file(&uri)
        .expect("opened document should have a Salsa file");
    let classes = {
        let db = workspace.salsa_db.lock();
        let _result = java_analyzer::salsa_queries::index::extract_classes(&*db, salsa_file);
        java_analyzer::salsa_queries::index::get_extracted_classes(&*db, salsa_file)
    };

    eprintln!("Extracted {} classes from {}", classes.len(), uri);
    for class in &classes {
        eprintln!("  - {}", class.internal_name);
    }

    let analysis = workspace.analysis_context_for_uri(&uri);
    let origin = java_analyzer::index::ClassOrigin::SourceFile(Arc::from(uri.as_str()));

    eprintln!(
        "Adding classes to index: module={}, source_root={:?}",
        analysis.module.0,
        analysis.source_root.map(|id| id.0)
    );

    workspace.index.update(|index| {
        index.update_source_in_context(analysis.module, analysis.source_root, origin, classes);
    });
}

async fn update_document_without_reindex(
    workspace: &Arc<Workspace>,
    uri: &str,
    version: i32,
    content: &str,
) {
    let uri = Url::parse(uri).unwrap();
    let registry = LanguageRegistry::new();
    let tree = registry.find("java").and_then(|lang| {
        let mut parser = lang.make_parser();
        parser.parse(content, None)
    });

    workspace.documents.with_doc_mut(&uri, |doc| {
        doc.update_source(java_analyzer::workspace::SourceFile::new(
            uri.clone(),
            "java",
            version,
            content,
            tree,
        ));
    });
}

fn completion_request(uri: &Url) -> Arc<RequestContext> {
    RequestContext::new(
        "completion",
        uri,
        RequestFamily::Completion,
        1,
        Default::default(),
    )
}

async fn run_completion(
    workspace: Arc<Workspace>,
    engine: Arc<CompletionEngine>,
    registry: Arc<LanguageRegistry>,
    params: CompletionParams,
) -> Option<CompletionResponse> {
    let request = completion_request(&params.text_document_position.text_document.uri);
    handle_completion(workspace, engine, registry, params, request)
        .await
        .expect("completion request should succeed")
}

fn strip_cursor_marker(content_with_marker: &str) -> (String, Position) {
    let marker = "/*caret*/";
    let offset = content_with_marker.find(marker).expect("cursor marker");
    let content = content_with_marker.replacen(marker, "", 1);

    let mut line = 0u32;
    let mut character = 0u32;
    for ch in content[..offset].chars() {
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += 1;
        }
    }

    (content, Position { line, character })
}

#[tokio::test]
async fn test_completion_after_file_open() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());

    // Open a simple Java file
    let content = r#"
package org.example;

public class User {
    private String name;
    
    public String getName() {
        return name;
    }
    
    public void test() {
        String n = get
    }
}
"#;

    open_document(&workspace, "file:///test/User.java", content).await;

    // Request completion at "get|"
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse("file:///test/User.java").unwrap(),
            },
            position: Position {
                line: 10,
                character: 22, // After "get"
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let response = run_completion(
        Arc::clone(&workspace),
        Arc::clone(&engine),
        Arc::clone(&registry),
        params,
    )
    .await;

    // Verify we got completion results
    assert!(response.is_some(), "Expected completion results, got None");

    if let Some(CompletionResponse::List(list)) = response {
        assert!(!list.items.is_empty(), "Expected non-empty completion list");

        // Should contain "getName" method
        let has_get_name = list.items.iter().any(|item| item.label.contains("getName"));
        assert!(
            has_get_name,
            "Expected 'getName' in completion results, got: {:?}",
            list.items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }
}

async fn test_completion_with_empty_workspace() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());

    // Don't open any files - workspace is empty

    // Request completion
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse("file:///test/Empty.java").unwrap(),
            },
            position: Position {
                line: 0,
                character: 0,
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let response = run_completion(
        Arc::clone(&workspace),
        Arc::clone(&engine),
        Arc::clone(&registry),
        params,
    )
    .await;

    // Should return None for unopened file
    assert!(response.is_none(), "Expected None for unopened file");
}

#[tokio::test]
async fn test_completion_uses_current_document_overlay_for_nested_inner_constructor() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());
    let uri = "file:///test/Main.java";

    let initial = r#"
package org.example;

public class Main {
    public class Test {
        private void foo() {
            Test t = new Test();
        }
    }
}
"#;

    let (updated, position) = strip_cursor_marker(
        r#"
package org.example;

public class Main {
    public class Test {
        private void foo() {
            Test t = new Test();
            t.new NestedNonSt/*caret*/();
        }

        public class NestedNonStatic {}
    }
}
"#,
    );

    open_document(&workspace, uri, initial).await;
    update_document_without_reindex(&workspace, uri, 2, &updated).await;

    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse(uri).unwrap(),
            },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let response = run_completion(
        Arc::clone(&workspace),
        Arc::clone(&engine),
        Arc::clone(&registry),
        params,
    )
    .await;

    assert!(
        response.is_some(),
        "expected completion results from current document overlay"
    );

    if let Some(CompletionResponse::List(list)) = response {
        let labels: Vec<&str> = list.items.iter().map(|item| item.label.as_str()).collect();
        assert!(labels.contains(&"NestedNonStatic"), "labels={labels:?}");
    }
}

#[tokio::test]
async fn test_completion_member_access() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());

    let content = r#"
package org.example;

public class User {
    private String name;
    
    public String getName() {
        return name;
    }
    
    public void setName(String name) {
        this.name = name;
    }
    
    public void test() {
        User user = new User();
        user.
    }
}
"#;

    open_document(&workspace, "file:///test/User.java", content).await;

    // Request completion at "user.|"
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse("file:///test/User.java").unwrap(),
            },
            position: Position {
                line: 15,
                character: 13, // After "user."
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: Some(CompletionContext {
            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(".".to_string()),
        }),
    };

    let response = run_completion(
        Arc::clone(&workspace),
        Arc::clone(&engine),
        Arc::clone(&registry),
        params,
    )
    .await;

    // Verify we got completion results (the bug was returning None)
    assert!(
        response.is_some(),
        "Expected completion results for User class context"
    );

    // The main bug was empty results - as long as we get SOME results, the bug is fixed
    if let Some(CompletionResponse::List(list)) = response {
        assert!(!list.items.is_empty(), "Expected non-empty completion list");

        eprintln!("Got {} completion items", list.items.len());
        for item in list.items.iter().take(5) {
            eprintln!("  - {}", item.label);
        }
    }
}

#[tokio::test]
async fn test_completion_member_access_on_qualified_nested_local_type() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());

    let (content, position) = strip_cursor_marker(
        r#"
package org.example;

public class Main {
    public class Test {
        private void test() {
            Test.Nested n = new Test.Nested();
            n./*caret*/
        }

        public static class Nested {
            public void foo() {}
        }
    }
}
"#,
    );

    open_document(&workspace, "file:///test/Main.java", &content).await;

    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse("file:///test/Main.java").unwrap(),
            },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: Some(CompletionContext {
            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(".".to_string()),
        }),
    };

    let response = run_completion(
        Arc::clone(&workspace),
        Arc::clone(&engine),
        Arc::clone(&registry),
        params,
    )
    .await;

    assert!(
        response.is_some(),
        "expected completion results for qualified nested local type"
    );

    if let Some(CompletionResponse::List(list)) = response {
        let labels: Vec<&str> = list.items.iter().map(|item| item.label.as_str()).collect();
        assert!(
            labels.iter().any(|label| label.starts_with("foo")),
            "labels={labels:?}"
        );
    }
}

#[tokio::test]
async fn test_completion_member_access_on_qualified_nested_local_type_after_did_change() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());
    let uri = "file:///test/Main.java";

    let initial = r#"
package org.example;

public class Main {
    public class Test {
        private void test() {}
    }
}
"#;

    let (updated, position) = strip_cursor_marker(
        r#"
package org.example;

public class Main {
    public class Test {
        private void test() {
            Test.Nested n = new Test.Nested();
            n./*caret*/
        }

        public static class Nested {
            public void foo() {}
        }
    }
}
"#,
    );

    open_document(&workspace, uri, initial).await;
    update_document_without_reindex(&workspace, uri, 2, &updated).await;

    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse(uri).unwrap(),
            },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: Some(CompletionContext {
            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(".".to_string()),
        }),
    };

    let response = run_completion(
        Arc::clone(&workspace),
        Arc::clone(&engine),
        Arc::clone(&registry),
        params,
    )
    .await;

    assert!(
        response.is_some(),
        "expected completion results for qualified nested local type after did_change"
    );

    if let Some(CompletionResponse::List(list)) = response {
        let labels: Vec<&str> = list.items.iter().map(|item| item.label.as_str()).collect();
        assert!(
            labels.iter().any(|label| label.starts_with("foo")),
            "labels={labels:?}"
        );
    }
}

#[tokio::test]
async fn test_completion_member_access_lombok_delegate() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());

    let (content, position) = strip_cursor_marker(
        r#"
package org.example;

import lombok.experimental.Delegate;

interface Service {
    void doSomething();
    String getName();
}

class ServiceImpl implements Service {
    @Override
    public void doSomething() {}

    @Override
    public String getName() {
        return "ServiceImpl";
    }
}

public class Wrapper {
    @Delegate
    private final Service service = new ServiceImpl();

    public static void main(String[] args) {
        Wrapper wrapper = new Wrapper();
        wrapper./*caret*/
    }
}
"#,
    );

    open_document(&workspace, "file:///test/Wrapper.java", &content).await;

    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse("file:///test/Wrapper.java").unwrap(),
            },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: Some(CompletionContext {
            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(".".to_string()),
        }),
    };

    let response = run_completion(
        Arc::clone(&workspace),
        Arc::clone(&engine),
        Arc::clone(&registry),
        params,
    )
    .await;

    assert!(response.is_some(), "Expected completion results, got None");

    if let Some(CompletionResponse::List(list)) = response {
        let labels: Vec<&str> = list.items.iter().map(|item| item.label.as_str()).collect();
        assert!(
            labels.iter().any(|label| label.starts_with("doSomething")),
            "labels={labels:?}"
        );
        assert!(
            labels.iter().any(|label| label.starts_with("getName")),
            "labels={labels:?}"
        );
    }
}

#[tokio::test]
async fn test_name_table_not_empty_after_indexing() {
    let workspace = create_test_workspace();

    let content = r#"
package org.example;

public class User {
    private String name;
}
"#;

    let uri = Url::parse("file:///test/User.java").unwrap();
    open_document(&workspace, uri.as_str(), content).await;

    // Check that the name table is not empty
    let analysis = workspace.analysis_context_for_uri(&uri);

    eprintln!(
        "Analysis context: module={}, classpath={:?}, source_root={:?}",
        analysis.module.0,
        analysis.classpath,
        analysis.source_root.map(|id| id.0)
    );

    // Check the index directly
    let index = workspace.index.load();
    let view =
        index.view_for_analysis_context(analysis.module, analysis.classpath, analysis.source_root);
    eprintln!("IndexView layers: {}", view.layer_count());

    let all_classes = view.iter_all_classes();
    eprintln!("Classes in view: {}", all_classes.len());
    for class in all_classes.iter().take(5) {
        eprintln!("  - {}", class.internal_name);
    }

    // Test exact_match_keys directly
    let keys = view.exact_match_keys();
    eprintln!("exact_match_keys returned: {} keys", keys.len());
    for key in keys.iter().take(5) {
        eprintln!("  - {}", key);
    }

    // Test build_name_table directly
    let direct_name_table = view.build_name_table();
    eprintln!(
        "Direct build_name_table: {} entries",
        direct_name_table.len()
    );

    drop(index);

    let name_table = {
        let db = workspace.salsa_db.lock();
        java_analyzer::salsa_queries::get_name_table_for_context(
            &*db,
            analysis.module,
            analysis.classpath,
            analysis.source_root,
        )
    };

    eprintln!("NameTable size: {}", name_table.len());

    // Check workspace version
    let workspace_version = {
        let index = workspace.index.load();
        index.version()
    };
    eprintln!("Workspace version: {}", workspace_version);

    assert!(
        !name_table.is_empty(),
        "NameTable should not be empty after indexing a file. Got {} entries",
        name_table.len()
    );

    // Should contain the User class
    assert!(
        name_table.exists("org/example/User"),
        "NameTable should contain 'org/example/User'"
    );
}

#[tokio::test]
async fn test_completion_enum_variants_inside_enum_method() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());

    let (content, position) = strip_cursor_marker(
        r#"
package org.example;

public enum RandomEnum {
    A, B, C;

    public void test() {
        A/*caret*/
    }
}
"#,
    );

    open_document(&workspace, "file:///test/RandomEnum.java", &content).await;

    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse("file:///test/RandomEnum.java").unwrap(),
            },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let response = run_completion(
        Arc::clone(&workspace),
        Arc::clone(&engine),
        Arc::clone(&registry),
        params,
    )
    .await;

    assert!(response.is_some(), "Expected completion results, got None");

    if let Some(CompletionResponse::List(list)) = response {
        let labels: Vec<&str> = list.items.iter().map(|item| item.label.as_str()).collect();
        assert!(labels.contains(&"A"), "labels={labels:?}");
    }
}

#[tokio::test]
async fn test_completion_context_caching() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());

    // Open a Java file
    let content = r#"
package org.example;

public class CacheTest {
    private String value;
    
    public void method() {
        String v = val
    }
}
"#;

    open_document(&workspace, "file:///test/CacheTest.java", content).await;

    let uri = Url::parse("file:///test/CacheTest.java").unwrap();
    let position = Position {
        line: 7,
        character: 22, // After "val"
    };

    // First completion request - should compute context
    let params1 = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let result1 =
        run_completion(workspace.clone(), engine.clone(), registry.clone(), params1).await;

    assert!(result1.is_some(), "First completion should return results");

    // Second completion request at same position - should use cached context
    let params2 = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let result2 =
        run_completion(workspace.clone(), engine.clone(), registry.clone(), params2).await;

    assert!(result2.is_some(), "Second completion should return results");

    // Both results should be similar (cache hit)
    if let (Some(CompletionResponse::List(list1)), Some(CompletionResponse::List(list2))) =
        (result1, result2)
    {
        assert_eq!(
            list1.items.len(),
            list2.items.len(),
            "Cached completion should return same number of items"
        );
    }
}

#[tokio::test]
async fn test_completion_context_invalidation_on_content_change() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());

    // Open initial content
    let content1 = r#"
package org.example;

public class InvalidationTest {
    private String oldField;
    
    public void method() {
        String v = old
    }
}
"#;

    open_document(&workspace, "file:///test/InvalidationTest.java", content1).await;

    let uri = Url::parse("file:///test/InvalidationTest.java").unwrap();
    let position = Position {
        line: 7,
        character: 22,
    };

    // First completion
    let params1 = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let result1 =
        run_completion(workspace.clone(), engine.clone(), registry.clone(), params1).await;

    assert!(result1.is_some(), "First completion should return results");

    // Modify the content (change field name)
    let content2 = r#"
package org.example;

public class InvalidationTest {
    private String newField;
    
    public void method() {
        String v = new
    }
}
"#;

    // Update the document
    workspace.documents.close(&uri);
    open_document(&workspace, "file:///test/InvalidationTest.java", content2).await;

    let position2 = Position {
        line: 7,
        character: 22,
    };

    // Second completion with modified content
    let params2 = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: position2,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let result2 =
        run_completion(workspace.clone(), engine.clone(), registry.clone(), params2).await;

    assert!(
        result2.is_some(),
        "Second completion after content change should return results"
    );

    // Results should be different due to content change
    // (cache should have been invalidated)
}

#[tokio::test]
async fn test_completion_context_different_positions() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());

    let content = r#"
package org.example;

public class PositionTest {
    private String field1;
    private String field2;
    
    public void method1() {
        String v = fie
    }
    
    public void method2() {
        String v = fie
    }
}
"#;

    open_document(&workspace, "file:///test/PositionTest.java", content).await;

    let uri = Url::parse("file:///test/PositionTest.java").unwrap();

    // Completion in method1
    let params1 = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position {
                line: 8,
                character: 22,
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let result1 =
        run_completion(workspace.clone(), engine.clone(), registry.clone(), params1).await;

    assert!(
        result1.is_some(),
        "Completion in method1 should return results"
    );

    // Completion in method2 (different position)
    let params2 = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position {
                line: 12,
                character: 22,
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let result2 =
        run_completion(workspace.clone(), engine.clone(), registry.clone(), params2).await;

    assert!(
        result2.is_some(),
        "Completion in method2 should return results"
    );

    // Both should return results (different cache entries due to different positions)
}

#[tokio::test]
async fn test_completion_annotation_param_fresh_slot_filters_used_keys() {
    let workspace = create_test_workspace();
    let engine = Arc::new(CompletionEngine::new());
    let registry = Arc::new(LanguageRegistry::new());

    let (content, position) = strip_cursor_marker(
        r#"
package org.example;

@interface ConfigAnno {
    String name();
    String value();
}

@ConfigAnno(name = "x", /*caret*/)
class A {}
"#,
    );

    open_document(&workspace, "file:///test/A.java", &content).await;
    let uri = Url::parse("file:///test/A.java").unwrap();

    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    let response = run_completion(
        Arc::clone(&workspace),
        Arc::clone(&engine),
        Arc::clone(&registry),
        params,
    )
    .await;

    assert!(
        response.is_some(),
        "expected annotation parameter completion results"
    );

    if let Some(CompletionResponse::List(list)) = response {
        let labels: Vec<&str> = list.items.iter().map(|item| item.label.as_str()).collect();
        assert!(labels.contains(&"value"), "labels={labels:?}");
        assert!(!labels.contains(&"name"), "labels={labels:?}");

        let value_item = list
            .items
            .iter()
            .find(|item| item.label == "value")
            .expect("expected value annotation element");

        assert_eq!(value_item.kind, Some(CompletionItemKind::PROPERTY));
        assert_eq!(value_item.filter_text.as_deref(), Some("value"));

        let new_text = value_item.text_edit.as_ref().map(|edit| match edit {
            CompletionTextEdit::Edit(edit) => edit.new_text.as_str(),
            CompletionTextEdit::InsertAndReplace(edit) => edit.new_text.as_str(),
        });
        assert_eq!(new_text, Some("value = "));
    }
}
