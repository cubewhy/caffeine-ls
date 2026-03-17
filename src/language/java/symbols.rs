use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};
use tree_sitter::Node;

use crate::lsp::converters::ts_node_to_range;
use tree_sitter_utils::{Handler, HandlerExt, Input, handler_fn};

pub fn collect_java_symbols<'a>(root: Node<'a>, bytes: &'a [u8]) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    collect_type_declarations(root, bytes, &mut out);
    out
}

fn collect_type_declarations<'a>(node: Node<'a>, bytes: &'a [u8], out: &mut Vec<DocumentSymbol>) {
    // For each child: if it's a type declaration build a symbol; if it's a
    // container node ("program" or "ERROR") recurse into it; otherwise skip.
    let dispatch = (|inp: Input<&[u8]>| -> Option<Vec<DocumentSymbol>> {
        if is_type_declaration(inp.node.kind()) {
            Some(build_type_symbol(inp.node, inp.ctx).into_iter().collect())
        } else {
            None
        }
    })
    .or((|inp: Input<&[u8]>| -> Option<Vec<DocumentSymbol>> {
        let mut nested = Vec::new();
        collect_type_declarations(inp.node, inp.ctx, &mut nested);
        Some(nested)
    })
    .for_kinds(&["program", "ERROR"]))
    .for_children();

    let collected = dispatch
        .handle(Input::new(node, bytes, None))
        .unwrap_or_default();
    out.extend(collected.into_iter().flatten());
}

fn is_type_declaration(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}

fn build_type_symbol<'a>(node: Node<'a>, bytes: &'a [u8]) -> Option<DocumentSymbol> {
    let (mut sym, body) = start_type_symbol(node, bytes)?;
    let children = body
        .map(|body_node| collect_type_members(body_node, bytes))
        .unwrap_or_default();
    sym.children = Some(children);
    Some(sym)
}

fn collect_type_members<'a>(body: Node<'a>, bytes: &'a [u8]) -> Vec<DocumentSymbol> {
    // Dispatch each child by kind using a combinator chain.
    // Each arm returns a Vec<DocumentSymbol>; arms that produce a single
    // symbol wrap it in a one-element vec. Arms that recurse also return
    // a vec.  `.for_children()` collects all `Some(vec)` results;
    // we then flatten them into the final output.
    let dispatch = (
        // Type declarations: recurse into nested classes/interfaces/etc.
        |inp: Input<&[u8]>| -> Option<Vec<DocumentSymbol>> {
            if is_type_declaration(inp.node.kind()) {
                Some(build_type_symbol(inp.node, inp.ctx).into_iter().collect())
            } else {
                None
            }
        }
    )
    .or(handler_fn(|inp: Input<&[u8]>| -> Vec<DocumentSymbol> {
        parse_method_symbol(inp.node, inp.ctx).into_iter().collect()
    })
    .for_kinds(&[
        "method_declaration",
        "constructor_declaration",
        "compact_constructor_declaration",
    ]))
    .or(handler_fn(|inp: Input<&[u8]>| -> Vec<DocumentSymbol> {
        parse_enum_constant_symbol(inp.node, inp.ctx)
            .into_iter()
            .collect()
    })
    .for_kinds(&["enum_constant", "enum_constant_declaration"]))
    .or(
        handler_fn(|inp: Input<&[u8]>| collect_type_members(inp.node, inp.ctx))
            .for_kinds(&["enum_body_declarations", "ERROR"]),
    )
    .or(
        handler_fn(|inp: Input<&[u8]>| parse_field_symbols(inp.node, inp.ctx))
            .for_kinds(&["field_declaration"]),
    )
    .for_children();

    dispatch
        .handle(Input::new(body, bytes, None))
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .collect()
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

    let kind = if node.kind() == "constructor_declaration"
        || node.kind() == "compact_constructor_declaration"
    {
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
        .or_else(|| tree_sitter_utils::traversal::any_child_of_kind(node, "identifier"))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::java::make_java_parser;
    use insta::assert_ron_snapshot;

    fn collect(src: &str) -> Vec<DocumentSymbol> {
        let mut parser = make_java_parser();
        let tree = parser.parse(src, None).expect("parse java");
        collect_java_symbols(tree.root_node(), src.as_bytes())
    }

    #[test]
    fn nested_class_symbols_preserve_ownership_boundaries() {
        let src = indoc::indoc! {r#"
            package org.cubewhy;

            class ChainCheck {
                int outerField;
                void outerMethod() {}

                static class Box<T> {
                    int innerField;
                    T get() { return null; }
                    static class BoxV<V> {
                        V getV() { return null; }
                    }
                }
            }
        "#};
        let syms = collect(src);
        assert_ron_snapshot!(syms);
    }

    #[test]
    fn nested_class_members_do_not_absorb_parent_members() {
        let src = indoc::indoc! {r#"
            class Outer {
                int outerField;
                void outerMethod() {}
                static class Inner {
                    int innerField;
                    void innerMethod() {}
                }
            }
        "#};
        let syms = collect(src);
        let outer = syms
            .iter()
            .find(|s| s.name == "Outer")
            .expect("outer symbol");
        let outer_children = outer.children.as_ref().expect("outer children");
        let inner = outer_children
            .iter()
            .find(|s| s.name == "Inner")
            .expect("inner symbol");
        let inner_children = inner.children.as_ref().expect("inner children");

        assert!(
            inner_children.iter().all(|s| s.name != "outerField"),
            "inner must not contain parent field"
        );
        assert!(
            inner_children.iter().all(|s| s.name != "outerMethod"),
            "inner must not contain parent method"
        );
        assert!(
            outer_children.iter().any(|s| s.name == "outerField")
                && outer_children.iter().any(|s| s.name == "outerMethod"),
            "outer must keep its own members"
        );
    }
}
