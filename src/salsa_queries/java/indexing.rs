use crate::index::{ClassMetadata, NameTable};
use crate::salsa_db::SourceFile;
use crate::salsa_queries::Db;
use std::sync::Arc;

/// Parse Java source and extract class metadata with full incremental support
///
/// This is the main entry point for Java file indexing.
/// Note: We return the classes directly, not wrapped in Arc, because Salsa
/// will handle the memoization.
pub fn parse_java_classes(db: &dyn Db, file: SourceFile) -> Vec<ClassMetadata> {
    let content = file.content(db);
    let file_id = file.file_id(db);
    let name_table = get_name_table_for_java_file(db, file);
    let origin = crate::index::ClassOrigin::SourceFile(Arc::from(file_id.as_str()));

    crate::language::java::class_parser::parse_java_source(content, origin, name_table)
}

pub(super) fn get_name_table_for_java_file(
    db: &dyn Db,
    file: SourceFile,
) -> Option<Arc<NameTable>> {
    let workspace_index = db.workspace_index();
    let index = workspace_index.read();
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
    let content = file.content(db);
    crate::language::java::class_parser::extract_package_from_source(content)
}

pub fn extract_java_imports(db: &dyn Db, file: SourceFile) -> Vec<Arc<str>> {
    let content = file.content(db);
    crate::language::java::class_parser::extract_imports_from_source(content)
}

pub fn extract_java_static_imports(db: &dyn Db, file: SourceFile) -> Vec<Arc<str>> {
    let content = file.content(db);
    extract_java_static_imports_from_source(content)
}

pub fn extract_java_static_imports_from_source(source: &str) -> Vec<Arc<str>> {
    let ctx = crate::language::java::JavaContextExtractor::for_indexing(source, None);
    let mut parser = crate::language::java::make_java_parser();
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let root = tree.root_node();
    crate::language::java::scope::extract_static_imports(&ctx, root)
}
