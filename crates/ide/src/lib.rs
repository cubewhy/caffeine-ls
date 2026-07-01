use std::{collections::HashMap, path::Path, sync::Arc};

use dashmap::DashMap;
use project_model::WorkspaceGraph;
use rustc_hash::FxHashMap;
use syntax::SyntaxError;
use vfs::{AbsPathBuf, FileId};

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
    pub(crate) workspaces: Arc<FxHashMap<AbsPathBuf, WorkspaceGraph>>,
}

impl Analysis {}

impl std::panic::UnwindSafe for Analysis {}

pub struct AnalysisHost {
    pub(crate) workspaces: Arc<FxHashMap<AbsPathBuf, WorkspaceGraph>>,
}

impl AnalysisHost {
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            workspaces: Arc::new(HashMap::default()),
        }
    }

    pub fn snapshot(&self) -> Analysis {
        Analysis {
            workspaces: self.workspaces.clone(),
        }
    }

    pub fn add_workspace(&mut self, root: AbsPathBuf, workspace: WorkspaceGraph) {
        let workspaces = Arc::make_mut(&mut self.workspaces);

        workspaces.insert(root, workspace);
    }

    pub fn remove_workspace(&mut self, root: &AbsPathBuf) {
        let workspaces = Arc::make_mut(&mut self.workspaces);
        workspaces.remove(root);
    }
}
