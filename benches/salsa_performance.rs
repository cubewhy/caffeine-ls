use criterion::{Criterion, black_box, criterion_group, criterion_main};
use java_analyzer::index::{ClasspathId, ModuleId};
use java_analyzer::salsa_db::{Database, FileId, SourceFile};
use java_analyzer::salsa_queries;
use std::sync::Arc;
use tower_lsp::lsp_types::Url;

fn bench_name_table_cold(c: &mut Criterion) {
    c.bench_function("name_table_cold", |b| {
        b.iter(|| {
            let db = Database::default();
            let module_id = ModuleId::ROOT;
            let classpath = ClasspathId::Main;
            let source_root = None;
            let workspace_version = 0u64;

            black_box(salsa_queries::cached_name_table(
                &db,
                module_id,
                classpath,
                source_root,
                workspace_version,
            ))
        })
    });
}

fn bench_name_table_cached(c: &mut Criterion) {
    c.bench_function("name_table_cached", |b| {
        let db = Database::default();
        let module_id = ModuleId::ROOT;
        let classpath = ClasspathId::Main;
        let source_root = None;
        let workspace_version = 0u64;

        // Warm up the cache
        salsa_queries::cached_name_table(&db, module_id, classpath, source_root, workspace_version);

        b.iter(|| {
            black_box(salsa_queries::cached_name_table(
                &db,
                module_id,
                classpath,
                source_root,
                workspace_version,
            ))
        })
    });
}

fn bench_get_name_table(c: &mut Criterion) {
    c.bench_function("get_name_table_for_context", |b| {
        let db = Database::default();
        let module_id = ModuleId::ROOT;
        let classpath = ClasspathId::Main;
        let source_root = None;

        b.iter(|| {
            black_box(salsa_queries::get_name_table_for_context(
                &db,
                module_id,
                classpath,
                source_root,
            ))
        })
    });
}

fn bench_class_extraction(c: &mut Criterion) {
    c.bench_function("extract_classes", |b| {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            "package com.example;\npublic class Test { void foo() {} }".to_string(),
            Arc::from("java"),
        );

        b.iter(|| black_box(salsa_queries::extract_classes(&db, file)))
    });
}

fn bench_class_extraction_cached(c: &mut Criterion) {
    c.bench_function("extract_classes_cached", |b| {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            "package com.example;\npublic class Test { void foo() {} }".to_string(),
            Arc::from("java"),
        );

        // Warm up the cache
        salsa_queries::extract_classes(&db, file);

        b.iter(|| black_box(salsa_queries::extract_classes(&db, file)))
    });
}

fn bench_completion_context_metadata_cold(c: &mut Criterion) {
    c.bench_function("completion_context_metadata_cold", |b| {
        b.iter(|| {
            let db = Database::default();
            let file_uri: Arc<str> = Arc::from("file:///test/Test.java");
            let content_hash = 12345u64;
            let line = 10u32;
            let character = 5u32;
            let trigger_char = Some('.');

            black_box(salsa_queries::cached_completion_context_metadata(
                &db,
                file_uri,
                content_hash,
                line,
                character,
                trigger_char,
            ))
        })
    });
}

fn bench_completion_context_metadata_cached(c: &mut Criterion) {
    c.bench_function("completion_context_metadata_cached", |b| {
        let db = Database::default();
        let file_uri: Arc<str> = Arc::from("file:///test/Test.java");
        let content_hash = 12345u64;
        let line = 10u32;
        let character = 5u32;
        let trigger_char = Some('.');

        // Warm up the cache
        salsa_queries::cached_completion_context_metadata(
            &db,
            file_uri.clone(),
            content_hash,
            line,
            character,
            trigger_char,
        );

        b.iter(|| {
            black_box(salsa_queries::cached_completion_context_metadata(
                &db,
                file_uri.clone(),
                content_hash,
                line,
                character,
                trigger_char,
            ))
        })
    });
}

fn bench_content_hash_computation(c: &mut Criterion) {
    c.bench_function("compute_relevant_content_hash", |b| {
        let source = r#"
package com.example;

public class Test {
    private String field;
    
    public void method() {
        String v = field.
    }
}
"#;
        let line = 7u32;
        let character = 28u32;

        b.iter(|| {
            black_box(salsa_queries::compute_relevant_content_hash(
                source, line, character,
            ))
        })
    });
}

criterion_group!(
    benches,
    bench_name_table_cold,
    bench_name_table_cached,
    bench_get_name_table,
    bench_class_extraction,
    bench_class_extraction_cached,
    bench_completion_context_metadata_cold,
    bench_completion_context_metadata_cached,
    bench_content_hash_computation
);
criterion_main!(benches);
