use super::common::find_ancestor_of_kind;
use super::indexing::get_name_table_for_java_file;
use crate::salsa_db::SourceFile;
use crate::salsa_queries::Db;
use crate::salsa_queries::MethodSummaryData;
use std::sync::Arc;
use tree_sitter_utils::traversal::find_node_by_offset;

#[salsa::tracked]
pub fn find_java_enclosing_class_name(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
) -> Option<Arc<str>> {
    if let Some((name, _, _)) =
        crate::salsa_queries::semantic::find_enclosing_class_bounds(db, file, offset)
    {
        Some(name)
    } else {
        None
    }
}

#[salsa::tracked]
pub fn extract_java_enclosing_method(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
) -> Option<Arc<MethodSummaryData>> {
    let content = file.content(db);
    let tree = crate::salsa_queries::parse::parse_tree(db, file)?;
    let root = tree.root_node();
    let name_table = get_name_table_for_java_file(db, file);
    let extractor = crate::language::java::JavaContextExtractor::new_with_overview(
        content.to_string(),
        offset.min(content.len()),
        name_table.clone(),
    );
    let cursor_node = extractor.find_cursor_node(root);
    let package = crate::salsa_queries::parse::extract_package(db, file);
    let imports = crate::salsa_queries::parse::extract_imports(db, file);
    let type_ctx = crate::language::java::type_ctx::SourceTypeCtx::from_overview(
        package,
        imports.as_ref().clone(),
        name_table,
    );

    cursor_node
        .and_then(|node| find_ancestor_of_kind(node, "method_declaration"))
        .or_else(|| find_node_by_offset(root, "method_declaration", offset))
        .or_else(|| crate::language::java::utils::find_enclosing_method_in_error(root, offset))
        .and_then(|node| {
            crate::language::java::members::parse_method_node(&extractor, &type_ctx, node)
        })
        .and_then(convert_current_member_to_method_data)
        .map(Arc::new)
}

#[salsa::tracked]
pub fn count_java_locals_in_scope(db: &dyn Db, file: SourceFile, offset: usize) -> usize {
    let Some((method_start, method_end)) =
        crate::salsa_queries::semantic::find_enclosing_method_bounds(db, file, offset)
    else {
        return 0;
    };

    let metadata = crate::salsa_queries::semantic::extract_method_locals_metadata(
        db,
        file,
        method_start,
        method_end,
    );
    metadata.local_count
}

fn convert_current_member_to_method_data(
    member: crate::semantic::context::CurrentClassMember,
) -> Option<MethodSummaryData> {
    match member {
        crate::semantic::context::CurrentClassMember::Method(method) => Some(MethodSummaryData {
            name: Arc::clone(&method.name),
            descriptor: method.desc(),
            param_names: method.params.param_names(),
            access_flags: method.access_flags,
            is_synthetic: method.is_synthetic,
            generic_signature: method.generic_signature.clone(),
            return_type: method.return_type.clone(),
        }),
        crate::semantic::context::CurrentClassMember::Field(_) => None,
    }
}
