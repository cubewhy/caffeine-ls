use std::sync::Arc;
use tower_lsp::lsp_types::*;

use crate::language::rope_utils::rope_line_col_to_offset;
use crate::language::{LanguageRegistry, TokenCollector};
use crate::workspace::{SourceFile, Workspace};

/// Generate a unique but stable result_id for this document version.
fn make_result_id(version: i32) -> String {
    version.to_string()
}

/// Ensure the document has a parsed tree, returning an `Arc<SourceFile>` with
/// a tree attached.  If the file already has a tree this is a no-op clone.
fn ensure_tree(
    workspace: &Workspace,
    uri: &Url,
    lang: &dyn crate::language::Language,
) -> Option<Arc<SourceFile>> {
    // First pass: check if tree already exists (read-only).
    let has_tree = workspace
        .documents
        .with_doc(uri, |doc| doc.source().tree.is_some())
        .unwrap_or(false);

    if !has_tree {
        workspace.documents.with_doc_mut(uri, |doc| {
            if doc.source().tree.is_some() {
                return; // already set by a concurrent call
            }
            let tree = lang.parse_tree(doc.source().text(), None);
            doc.set_tree(tree);
        });
    }

    workspace
        .documents
        .with_doc(uri, |doc| Arc::clone(doc.source()))
}

pub async fn handle_semantic_tokens_full(
    registry: Arc<LanguageRegistry>,
    workspace: Arc<Workspace>,
    params: SemanticTokensParams,
) -> Option<SemanticTokensResult> {
    let uri = params.text_document.uri;

    let lang_id = workspace
        .documents
        .with_doc(&uri, |doc| doc.language_id().to_owned())?;

    let lang = registry.find(&lang_id)?;
    let file = ensure_tree(&workspace, &uri, lang)?;

    let root = file.root_node()?;
    let mut collector = TokenCollector::new(&file, lang);
    collector.collect(root);
    let data = collector.finish();

    let result_id = make_result_id(file.version);
    let data_clone = data.clone();
    let id_clone = result_id.clone();
    workspace.documents.with_doc_mut(&uri, |doc| {
        doc.semantic_token_cache = Some((id_clone, data_clone));
    });

    Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: Some(result_id),
        data,
    }))
}

/// Backward-compat alias used by the old server call site.
pub async fn handle_semantic_tokens(
    registry: Arc<LanguageRegistry>,
    workspace: Arc<Workspace>,
    params: SemanticTokensParams,
) -> Option<SemanticTokensResult> {
    handle_semantic_tokens_full(registry, workspace, params).await
}

pub async fn handle_semantic_tokens_range(
    registry: Arc<LanguageRegistry>,
    workspace: Arc<Workspace>,
    params: SemanticTokensRangeParams,
) -> Option<SemanticTokensRangeResult> {
    let uri = params.text_document.uri;
    let range = params.range;

    let lang_id = workspace
        .documents
        .with_doc(&uri, |doc| doc.language_id().to_owned())?;

    let lang = registry.find(&lang_id)?;
    let file = ensure_tree(&workspace, &uri, lang)?;

    let root = file.root_node()?;
    let start_byte =
        rope_line_col_to_offset(&file.rope, range.start.line, range.start.character).unwrap_or(0);
    let end_byte = rope_line_col_to_offset(&file.rope, range.end.line, range.end.character)
        .unwrap_or_else(|| file.text().len());

    let mut collector = TokenCollector::new(&file, lang);
    collector.collect_range(root, start_byte, end_byte);
    let data = collector.finish();

    Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
        result_id: None,
        data,
    }))
}

pub async fn handle_semantic_tokens_full_delta(
    registry: Arc<LanguageRegistry>,
    workspace: Arc<Workspace>,
    params: SemanticTokensDeltaParams,
) -> Option<SemanticTokensFullDeltaResult> {
    let uri = params.text_document.uri;
    let previous_result_id = params.previous_result_id;

    let lang_id = workspace
        .documents
        .with_doc(&uri, |doc| doc.language_id().to_owned())?;

    let lang = registry.find(&lang_id)?;
    let file = ensure_tree(&workspace, &uri, lang)?;

    let root = file.root_node()?;
    let mut collector = TokenCollector::new(&file, lang);
    collector.collect(root);
    let new_data = collector.finish();

    let new_result_id = make_result_id(file.version);

    let maybe_old_data = workspace.documents.with_doc(&uri, |doc| {
        doc.semantic_token_cache
            .as_ref()
            .filter(|(id, _)| id == &previous_result_id)
            .map(|(_, data)| data.clone())
    })?;

    // Update cache.
    {
        let data_clone = new_data.clone();
        let id_clone = new_result_id.clone();
        workspace.documents.with_doc_mut(&uri, |doc| {
            doc.semantic_token_cache = Some((id_clone, data_clone));
        });
    }

    match maybe_old_data {
        Some(old_data) => {
            let edits = diff_semantic_tokens(&old_data, &new_data);
            Some(SemanticTokensFullDeltaResult::TokensDelta(
                SemanticTokensDelta {
                    result_id: Some(new_result_id),
                    edits,
                },
            ))
        }
        None => Some(SemanticTokensFullDeltaResult::Tokens(SemanticTokens {
            result_id: Some(new_result_id),
            data: new_data,
        })),
    }
}

/// Compute a minimal sequence of [`SemanticTokensEdit`] to transform `old`
/// into `new` using a prefix/suffix trim then a single contiguous edit.
fn diff_semantic_tokens(old: &[SemanticToken], new: &[SemanticToken]) -> Vec<SemanticTokensEdit> {
    if old.is_empty() && new.is_empty() {
        return vec![];
    }

    let prefix_len = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();

    if prefix_len == old.len() && prefix_len == new.len() {
        return vec![];
    }

    let suffix_len = old[prefix_len..]
        .iter()
        .rev()
        .zip(new[prefix_len..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let old_end = old.len().saturating_sub(suffix_len);
    let new_end = new.len().saturating_sub(suffix_len);
    let replacement = &new[prefix_len..new_end];

    vec![SemanticTokensEdit {
        start: prefix_len as u32,
        delete_count: (old_end - prefix_len) as u32,
        data: if replacement.is_empty() {
            None
        } else {
            Some(replacement.to_vec())
        },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::SemanticToken;

    fn tok(dl: u32, ds: u32, len: u32, ty: u32, mods: u32) -> SemanticToken {
        SemanticToken {
            delta_line: dl,
            delta_start: ds,
            length: len,
            token_type: ty,
            token_modifiers_bitset: mods,
        }
    }

    #[test]
    fn diff_empty_to_empty() {
        assert!(diff_semantic_tokens(&[], &[]).is_empty());
    }

    #[test]
    fn diff_no_change() {
        let tokens = vec![tok(0, 0, 5, 0, 0), tok(1, 2, 3, 1, 0)];
        assert!(diff_semantic_tokens(&tokens, &tokens).is_empty());
    }

    #[test]
    fn diff_append_one() {
        let old = vec![tok(0, 0, 5, 0, 0)];
        let new = vec![tok(0, 0, 5, 0, 0), tok(1, 0, 3, 1, 0)];
        let edits = diff_semantic_tokens(&old, &new);
        assert_eq!(edits.len(), 1);
        let e = &edits[0];
        assert_eq!(e.start, 1);
        assert_eq!(e.delete_count, 0);
        assert_eq!(e.data.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn diff_delete_one() {
        let old = vec![tok(0, 0, 5, 0, 0), tok(1, 0, 3, 1, 0)];
        let new = vec![tok(0, 0, 5, 0, 0)];
        let edits = diff_semantic_tokens(&old, &new);
        assert_eq!(edits.len(), 1);
        let e = &edits[0];
        assert_eq!(e.start, 1);
        assert_eq!(e.delete_count, 1);
        assert!(e.data.is_none());
    }

    #[test]
    fn diff_replace_middle() {
        let a = tok(0, 0, 5, 0, 0);
        let b_old = tok(1, 0, 3, 1, 0);
        let b_new = tok(1, 0, 3, 2, 0);
        let c = tok(2, 0, 4, 0, 0);
        let old = vec![a, b_old, c];
        let new = vec![a, b_new, c];
        let edits = diff_semantic_tokens(&old, &new);
        assert_eq!(edits.len(), 1);
        let e = &edits[0];
        assert_eq!(e.start, 1);
        assert_eq!(e.delete_count, 1);
        assert_eq!(e.data.as_ref().unwrap(), &vec![b_new]);
    }

    #[test]
    fn diff_full_replacement() {
        let old = vec![tok(0, 0, 5, 0, 0), tok(1, 0, 3, 1, 0)];
        let new = vec![tok(0, 5, 5, 2, 0)];
        let edits = diff_semantic_tokens(&old, &new);
        assert_eq!(edits.len(), 1);
        let e = &edits[0];
        assert_eq!(e.start, 0);
        assert_eq!(e.delete_count, 2);
        assert_eq!(e.data.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn diff_empty_to_some() {
        let new = vec![tok(0, 0, 5, 0, 0), tok(1, 0, 3, 1, 0)];
        let edits = diff_semantic_tokens(&[], &new);
        assert_eq!(edits.len(), 1);
        let e = &edits[0];
        assert_eq!(e.start, 0);
        assert_eq!(e.delete_count, 0);
        assert_eq!(e.data.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn diff_some_to_empty() {
        let old = vec![tok(0, 0, 5, 0, 0), tok(1, 0, 3, 1, 0)];
        let edits = diff_semantic_tokens(&old, &[]);
        assert_eq!(edits.len(), 1);
        let e = &edits[0];
        assert_eq!(e.start, 0);
        assert_eq!(e.delete_count, 2);
        assert!(e.data.is_none());
    }
}
