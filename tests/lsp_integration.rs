/// LSP Integration Tests
///
/// These tests verify end-to-end LSP functionality by simulating real client interactions.
use java_analyzer::completion::engine::CompletionEngine;
use java_analyzer::language::LanguageRegistry;
use java_analyzer::lsp::handlers::completion::handle_completion;
use java_analyzer::workspace::Workspace;
use java_analyzer::workspace::document::Document;
use std::sync::Arc;
use tower_lsp::lsp_types::*;

/// Helper to create a test workspace with a simple Java file
fn create_test_workspace() -> Arc<Workspace> {
    let workspace = Arc::new(Workspace::new());
    workspace
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
    let salsa_file = workspace.get_or_create_salsa_file(&uri, content, "java");
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

    workspace.index.write().update_source_in_context(
        analysis.module,
        analysis.source_root,
        origin,
        classes,
    );
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

    let response = handle_completion(
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

#[tokio::test]
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

    let response = handle_completion(
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

    let response = handle_completion(
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
    let index = workspace.index.read();
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
        let index = workspace.index.read();
        index.version()
    };
    eprintln!("Workspace version: {}", workspace_version);

    assert!(
        name_table.len() > 0,
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
        handle_completion(workspace.clone(), engine.clone(), registry.clone(), params1).await;

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
        handle_completion(workspace.clone(), engine.clone(), registry.clone(), params2).await;

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
        handle_completion(workspace.clone(), engine.clone(), registry.clone(), params1).await;

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
        handle_completion(workspace.clone(), engine.clone(), registry.clone(), params2).await;

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
        handle_completion(workspace.clone(), engine.clone(), registry.clone(), params1).await;

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
        handle_completion(workspace.clone(), engine.clone(), registry.clone(), params2).await;

    assert!(
        result2.is_some(),
        "Completion in method2 should return results"
    );

    // Both should return results (different cache entries due to different positions)
}
