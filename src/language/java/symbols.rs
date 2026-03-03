use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};
use tree_sitter::Node;

use crate::lsp::converters::ts_node_to_range;

pub fn collect_java_symbols<'a>(root: Node<'a>, bytes: &'a [u8]) -> Vec<DocumentSymbol> {
    enum WorkItem<'a> {
        Visit(Node<'a>),
        FinishType(DocumentSymbol),
    }

    let mut scopes: Vec<Vec<DocumentSymbol>> = vec![Vec::new()];
    let mut stack: Vec<WorkItem<'a>> = vec![WorkItem::Visit(root)];

    while let Some(item) = stack.pop() {
        match item {
            WorkItem::Visit(node) => {
                let mut cursor = node.walk();

                for child in node.children(&mut cursor) {
                    match child.kind() {
                        "class_declaration"
                        | "interface_declaration"
                        | "enum_declaration"
                        | "record_declaration"
                        | "annotation_type_declaration" => {
                            if let Some((sym, body)) = start_type_symbol(child, bytes) {
                                scopes.push(Vec::new());

                                stack.push(WorkItem::FinishType(sym));

                                if let Some(body) = body {
                                    stack.push(WorkItem::Visit(body));
                                }
                            }
                        }

                        "method_declaration" | "constructor_declaration" => {
                            if let Some(sym) = parse_method_symbol(child, bytes) {
                                scopes.last_mut().unwrap().push(sym);
                            }
                        }

                        "enum_constant" | "enum_constant_declaration" => {
                            if let Some(sym) = parse_enum_constant_symbol(child, bytes) {
                                scopes.last_mut().unwrap().push(sym);
                            }
                        }

                        "field_declaration" => {
                            let fields = parse_field_symbols(child, bytes);
                            scopes.last_mut().unwrap().extend(fields);
                        }

                        "class_body" | "interface_body" | "enum_body" | "program" | "ERROR" => {
                            stack.push(WorkItem::Visit(child));
                        }

                        _ => {}
                    }
                }
            }

            WorkItem::FinishType(mut sym) => {
                let mut children = scopes.pop().expect("missing child scope for type");
                children.reverse();

                sym.children = Some(children);

                scopes.last_mut().unwrap().push(sym);
            }
        }
    }

    let mut top = scopes.pop().unwrap();
    top.reverse();
    top
}

/// Generate a "type symbol (children empty for now) + body node (for continued traversal)"
fn start_type_symbol<'a>(
    node: Node<'a>,
    bytes: &'a [u8],
) -> Option<(DocumentSymbol, Option<Node<'a>>)> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(bytes).ok()?.to_string();

    let kind = match node.kind() {
        "interface_declaration" | "annotation_type_declaration" => SymbolKind::INTERFACE,
        "enum_declaration" => SymbolKind::ENUM,
        _ => SymbolKind::CLASS, // Classes and records are both CLASS
    };

    let range = ts_node_to_range(&node);
    let selection_range = ts_node_to_range(&name_node);
    let body = node.child_by_field_name("body");

    #[allow(deprecated)]
    let sym = DocumentSymbol {
        name,
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children: None,
    };

    Some((sym, body))
}

fn parse_method_symbol<'a>(node: Node<'a>, bytes: &'a [u8]) -> Option<DocumentSymbol> {
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("identifier"))?; // constructor 用 identifier
    let name = name_node.utf8_text(bytes).ok()?.to_string();

    let kind = if node.kind() == "constructor_declaration" {
        SymbolKind::CONSTRUCTOR
    } else {
        SymbolKind::METHOD
    };

    #[allow(deprecated)]
    Some(DocumentSymbol {
        name,
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range: ts_node_to_range(&node),
        selection_range: ts_node_to_range(&name_node),
        children: None,
    })
}

fn parse_field_symbols<'a>(node: Node<'a>, bytes: &'a [u8]) -> Vec<DocumentSymbol> {
    let mut results = Vec::new();

    // Find type: used for detail display
    let type_text = {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|c| c.kind().ends_with("_type") || c.kind() == "type_identifier")
            .and_then(|c| c.utf8_text(bytes).ok())
            .map(|t| t.to_string())
    };

    // parse variable_declarator
    let mut cursor = node.walk();
    for declarator in node
        .children(&mut cursor)
        .filter(|c| c.kind() == "variable_declarator")
    {
        let Some(name_node) = declarator.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(bytes) else {
            continue;
        };

        #[allow(deprecated)]
        results.push(DocumentSymbol {
            name: name.to_string(),
            detail: type_text.clone(),
            kind: SymbolKind::FIELD,
            tags: None,
            deprecated: None,
            range: ts_node_to_range(&node),
            selection_range: ts_node_to_range(&name_node),
            children: None,
        });
    }

    results
}

fn parse_enum_constant_symbol<'a>(node: Node<'a>, bytes: &'a [u8]) -> Option<DocumentSymbol> {
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("identifier"))
        .or_else(|| {
            let mut c = node.walk();
            node.children(&mut c).find(|n| n.kind() == "identifier")
        })?;

    let name = name_node.utf8_text(bytes).ok()?.to_string();

    #[allow(deprecated)]
    Some(DocumentSymbol {
        name,
        detail: None,
        kind: SymbolKind::ENUM_MEMBER,
        tags: None,
        deprecated: None,
        range: ts_node_to_range(&node),
        selection_range: ts_node_to_range(&name_node),
        children: None,
    })
}
