use crate::index::{ClassMetadata, NameTable};
use crate::salsa_db::SourceFile;
use crate::salsa_queries::Db;
use std::{sync::Arc, time::Instant};

/// Parse Java source and extract class metadata with full incremental support
///
/// This is the main entry point for Java file indexing.
/// Note: We return the classes directly, not wrapped in Arc, because Salsa
/// will handle the memoization.
pub fn parse_java_classes(db: &dyn Db, file: SourceFile) -> Vec<ClassMetadata> {
    let total_started = Instant::now();
    let content = file.content(db);
    let file_id = file.file_id(db);
    let name_table_started = Instant::now();
    let name_table = get_name_table_for_java_file(db, file);
    let name_table_elapsed = name_table_started.elapsed();
    let origin = crate::index::ClassOrigin::SourceFile(Arc::from(file_id.as_str()));
    let parse_tree_started = Instant::now();
    let Some(tree) = crate::salsa_queries::parse::parse_tree(db, file) else {
        tracing::debug!(
            file = %file_id.as_str(),
            source_len = content.len(),
            name_table_ms = name_table_elapsed.as_secs_f64() * 1000.0,
            parse_tree_ms = parse_tree_started.elapsed().as_secs_f64() * 1000.0,
            total_ms = total_started.elapsed().as_secs_f64() * 1000.0,
            "tracked java extraction profile"
        );
        return vec![];
    };
    let parse_tree_elapsed = parse_tree_started.elapsed();
    let extract_started = Instant::now();

    let classes = crate::language::java::class_parser::extract_java_classes_from_tree(
        content, &tree, &origin, name_table, None,
    );
    tracing::debug!(
        file = %file_id.as_str(),
        source_len = content.len(),
        class_count = classes.len(),
        name_table_ms = name_table_elapsed.as_secs_f64() * 1000.0,
        parse_tree_ms = parse_tree_elapsed.as_secs_f64() * 1000.0,
        extract_from_tree_ms = extract_started.elapsed().as_secs_f64() * 1000.0,
        total_ms = total_started.elapsed().as_secs_f64() * 1000.0,
        "tracked java extraction profile"
    );
    classes
}

pub(super) fn get_name_table_for_java_file(
    db: &dyn Db,
    file: SourceFile,
) -> Option<Arc<NameTable>> {
    let index = db.workspace_index();
    let _ = file;
    tracing::debug!(
        phase = "indexing",
        file = %file.file_id(db).as_str(),
        purpose = "java source indexing parse",
        "constructing NameTable for Java file"
    );
    Some(index.build_name_table(crate::index::IndexScope {
        module: crate::index::ModuleId::ROOT,
    }))
}

pub fn extract_java_package(db: &dyn Db, file: SourceFile) -> Option<Arc<str>> {
    crate::salsa_queries::parse::extract_package(db, file)
}

pub fn extract_java_imports(db: &dyn Db, file: SourceFile) -> Vec<Arc<str>> {
    crate::salsa_queries::parse::extract_imports(db, file)
        .as_ref()
        .clone()
}

pub fn extract_java_static_imports(db: &dyn Db, file: SourceFile) -> Vec<Arc<str>> {
    let content = file.content(db);
    let Some(tree) = crate::salsa_queries::parse::parse_tree(db, file) else {
        return vec![];
    };
    crate::language::java::class_parser::extract_static_imports_from_root(content, tree.root_node())
}

pub fn extract_java_static_imports_from_source(source: &str) -> Vec<Arc<str>> {
    let mut parser = crate::language::java::make_java_parser();
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    crate::language::java::class_parser::extract_static_imports_from_root(source, tree.root_node())
}
