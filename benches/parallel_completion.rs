use criterion::{Criterion, criterion_group, criterion_main};
use java_analyzer::completion::engine::{CompletionEngine, CompletionPolicy};
use java_analyzer::index::{
    ClassMetadata, ClassOrigin, IndexScope, MethodParams, MethodSummary, ModuleId, WorkspaceIndex,
};
use java_analyzer::language::JavaLanguage;
use java_analyzer::semantic::{AccessReceiverKind, CursorLocation, SemanticContext};
use rust_asm::constants::ACC_PUBLIC;
use std::hint::black_box;
use std::sync::Arc;

fn create_test_index() -> WorkspaceIndex {
    let idx = WorkspaceIndex::new();

    // Add multiple classes to simulate realistic completion scenario
    let mut classes = Vec::new();
    for i in 0..50 {
        classes.push(ClassMetadata {
            package: Some(Arc::from("com/example")),
            name: Arc::from(format!("TestClass{}", i)),
            internal_name: Arc::from(format!("com/example/TestClass{}", i)),
            super_name: Some(Arc::from("java/lang/Object")),
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                MethodSummary {
                    name: Arc::from("method1"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("Ljava/lang/String;")),
                },
                MethodSummary {
                    name: Arc::from("method2"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("I")),
                },
            ],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            inner_class_of: None,
            generic_signature: None,
            origin: ClassOrigin::Unknown,
        });
    }

    idx.add_classes(classes);
    idx
}

fn bench_parallel_completion(c: &mut Criterion) {
    c.bench_function("completion_parallel_providers", |b| {
        let idx = create_test_index();
        let engine = CompletionEngine::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);

        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "Test".to_string(),
            },
            "Test",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("com/example/Main")),
            Some(Arc::from("com/example")),
            vec![],
        );

        b.iter(|| {
            black_box(engine.complete_with_policy(
                scope,
                ctx.clone(),
                &JavaLanguage,
                &view,
                CompletionPolicy::default(),
            ))
        })
    });
}

fn bench_member_access_completion(c: &mut Criterion) {
    c.bench_function("completion_member_access_parallel", |b| {
        let idx = create_test_index();
        let engine = CompletionEngine::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_kind: AccessReceiverKind::Unknown,
                receiver_type: Some(Arc::from("com/example/TestClass0")),
                member_prefix: "meth".to_string(),
                receiver_expr: "obj".to_string(),
                arguments: None,
            },
            "meth",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("com/example/Main")),
            Some(Arc::from("com/example")),
            vec![],
        );

        b.iter(|| {
            black_box(engine.complete_with_policy(
                scope,
                ctx.clone(),
                &JavaLanguage,
                &view,
                CompletionPolicy::default(),
            ))
        })
    });
}

criterion_group!(
    benches,
    bench_parallel_completion,
    bench_member_access_completion
);
criterion_main!(benches);
