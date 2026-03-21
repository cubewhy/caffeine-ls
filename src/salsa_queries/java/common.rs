use crate::index::{IndexScope, ModuleId};
use crate::salsa_queries::Db;
use std::sync::Arc;
use tree_sitter::Node;

pub(super) fn is_in_comment(content: &str, offset: usize) -> bool {
    crate::language::java::utils::is_cursor_in_comment(content, offset.min(content.len()))
}

pub(super) fn build_internal_name(
    package: &Option<Arc<str>>,
    class: &Option<Arc<str>>,
) -> Option<Arc<str>> {
    match (package, class) {
        (Some(pkg), Some(cls)) => Some(Arc::from(format!("{}/{}", pkg, cls))),
        (None, Some(cls)) => Some(Arc::clone(cls)),
        _ => None,
    }
}

pub(super) fn find_ancestor_of_kind<'a>(mut node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    loop {
        if node.kind() == kind {
            return Some(node);
        }
        node = node.parent()?;
    }
}

pub(super) fn root_index_view(db: &dyn Db) -> crate::index::IndexView {
    let workspace_index = db.workspace_index();
    let index = workspace_index.read();
    index.view(IndexScope {
        module: ModuleId::ROOT,
    })
}
