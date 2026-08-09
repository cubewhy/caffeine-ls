use std::sync::Arc;

use dashmap::DashMap;
use ide_db::RootDatabase;
use syntax::SyntaxError;
use vfs::FileId;

pub use ide_db::line_index::{LineCol, LineIndex};

pub mod delta;

pub struct ParsedFile {
    pub green_node: rowan::GreenNode,
    pub syntax_errors: Vec<SyntaxError>,
}

impl ParsedFile {
    pub fn new(green_node: rowan::GreenNode, syntax_errors: Vec<SyntaxError>) -> Self {
        Self {
            green_node,
            syntax_errors,
        }
    }
}

#[derive(Default)]
pub struct ParseCache {
    trees: DashMap<FileId, Arc<ParsedFile>>,
    file_revisions: DashMap<FileId, u64>,
}

impl ParseCache {
    pub fn get_tree(&self, file_id: FileId) -> Option<Arc<ParsedFile>> {
        self.trees
            .get(&file_id)
            .map(|parsed_file| parsed_file.clone())
    }

    /// Bumps the revision for a file and returns the new revision number.
    pub fn bump_revision(&self, file_id: FileId) -> u64 {
        let mut rev = self.file_revisions.entry(file_id).or_insert(0);
        *rev += 1;
        *rev
    }

    /// Checks if a given task revision is still the latest.
    pub fn is_cancelled(&self, file_id: FileId, task_revision: u64) -> bool {
        if let Some(current_rev) = self.file_revisions.get(&file_id) {
            *current_rev != task_revision
        } else {
            // File was removed
            true
        }
    }

    pub fn update(&self, file_id: FileId, parsed: ParsedFile) {
        self.trees.insert(file_id, Arc::new(parsed));
    }

    pub fn remove(&self, file_id: FileId) {
        self.trees.remove(&file_id);
        self.file_revisions.remove(&file_id);
    }
}

/// Snapshot of [AnalysisHost]
pub struct Analysis {
    db: RootDatabase,
}

impl Analysis {
    pub fn raw_database(&self) -> &RootDatabase {
        &self.db
    }
}

impl std::panic::UnwindSafe for Analysis {}

pub struct AnalysisHost {
    db: RootDatabase,
}

impl AnalysisHost {
    pub fn new() -> Self {
        Self {
            db: RootDatabase::new(),
        }
    }

    pub fn snapshot(&self) -> Analysis {
        Analysis {
            db: self.db.clone(),
        }
    }

    pub fn raw_database(&self) -> &RootDatabase {
        &self.db
    }

    pub fn raw_database_mut(&mut self) -> &mut RootDatabase {
        &mut self.db
    }
}

impl Default for AnalysisHost {
    fn default() -> Self {
        Self::new()
    }
}
