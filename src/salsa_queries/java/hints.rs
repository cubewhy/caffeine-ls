use super::common::{find_ancestor_of_kind, root_index_view};
use crate::language::java::inlay_hints::{JavaInlayHintKind, collect_java_inlay_hints};
use crate::salsa_db::SourceFile;
use crate::salsa_queries::Db;
use crate::salsa_queries::context::line_col_to_offset;
use crate::salsa_queries::hints::{InlayHintData, InlayHintKindData};
use ropey::Rope;
use std::sync::Arc;

#[salsa::tracked]
pub fn compute_java_inlay_hints(
    db: &dyn Db,
    file: SourceFile,
    start_line: u32,
    start_char: u32,
    end_line: u32,
    end_char: u32,
) -> Arc<Vec<InlayHintData>> {
    let content = file.content(db);
    let Some(start_offset) = line_col_to_offset(content, start_line, start_char) else {
        return Arc::new(Vec::new());
    };
    let Some(end_offset) = line_col_to_offset(content, end_line, end_char) else {
        return Arc::new(Vec::new());
    };

    let Some(tree) = crate::salsa_queries::parse::parse_tree(db, file) else {
        return Arc::new(Vec::new());
    };

    let view = root_index_view(db);
    let rope = Rope::from_str(content);

    Arc::new(
        collect_java_inlay_hints(
            content,
            &rope,
            tree.root_node(),
            &view,
            start_offset..end_offset,
            None,
            None,
            None,
        )
        .into_iter()
        .map(|hint| InlayHintData {
            offset: hint.offset,
            label: Arc::from(hint.label),
            kind: match hint.kind {
                JavaInlayHintKind::Type => InlayHintKindData::Type,
                JavaInlayHintKind::Parameter => InlayHintKindData::Parameter,
            },
        })
        .collect(),
    )
}

#[salsa::tracked]
pub fn infer_java_variable_type(
    db: &dyn Db,
    file: SourceFile,
    decl_offset: usize,
) -> Option<Arc<str>> {
    let content = file.content(db);
    let tree = crate::salsa_queries::parse::parse_tree(db, file)?;
    let root = tree.root_node();
    let node = find_node_at_offset(root, decl_offset)?;
    let var_decl = find_ancestor_of_kind(node, "variable_declarator")?;
    let init = var_decl.child_by_field_name("value")?;

    infer_type_from_expression(init, content.as_bytes())
}

fn find_node_at_offset<'a>(
    root: tree_sitter::Node<'a>,
    offset: usize,
) -> Option<tree_sitter::Node<'a>> {
    root.named_descendant_for_byte_range(offset, offset + 1)
}

fn infer_type_from_expression(expr: tree_sitter::Node, source: &[u8]) -> Option<Arc<str>> {
    match expr.kind() {
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => Some(Arc::from("int")),
        "decimal_floating_point_literal" | "hex_floating_point_literal" => {
            Some(Arc::from("double"))
        }
        "string_literal" => Some(Arc::from("String")),
        "true" | "false" => Some(Arc::from("boolean")),
        "null_literal" => Some(Arc::from("Object")),
        "object_creation_expression" => expr
            .child_by_field_name("type")
            .and_then(|type_node| type_node.utf8_text(source).ok().map(Arc::from)),
        _ => None,
    }
}
