use dashmap::DashMap;
use std::collections::HashSet;
use vfs::FileId;

#[derive(Default)]
pub struct LexicalIndex {
    forward: DashMap<FileId, HashSet<String>>,
    inverted: DashMap<String, HashSet<FileId>>,
}

impl LexicalIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Diff changed identifiers
    pub fn update_file_tokens(
        &self,
        file_id: FileId,
        new_tokens: HashSet<String>,
    ) -> HashSet<String> {
        let old_tokens = self
            .forward
            .remove(&file_id)
            .map(|(_, set)| set)
            .unwrap_or_default();

        let deleted_words: HashSet<String> = old_tokens.difference(&new_tokens).cloned().collect();

        for old_word in &old_tokens {
            if let Some(mut files) = self.inverted.get_mut(old_word) {
                files.remove(&file_id);
                if files.is_empty() {
                    drop(files);
                    self.inverted.remove(old_word);
                }
            }
        }

        for new_word in &new_tokens {
            self.inverted
                .entry(new_word.clone())
                .or_default()
                .insert(file_id);
        }

        self.forward.insert(file_id, new_tokens);

        deleted_words
    }

    pub fn get_files_containing(&self, word: &str) -> Option<HashSet<FileId>> {
        self.inverted.get(word).map(|r| r.value().clone())
    }

    pub fn remove_file(&self, file_id: FileId) {
        if let Some((_, old_tokens)) = self.forward.remove(&file_id) {
            for word in old_tokens {
                if let Some(mut files) = self.inverted.get_mut(&word) {
                    files.remove(&file_id);
                }
            }
        }
    }
}
