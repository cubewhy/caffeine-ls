use std::{collections::HashMap, path::Path, sync::Arc};

use dashmap::DashMap;
use project_model::WorkspaceGraph;
use rustc_hash::FxHashMap;
use syntax::SyntaxError;
use vfs::{AbsPathBuf, FileId};

use crate::delta::WorkspaceDelta;

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

    /// Apply new workspace graph and track changes
    pub fn apply_workspace_change(
        &mut self,
        root: AbsPathBuf,
        new_workspace: WorkspaceGraph,
    ) -> WorkspaceDelta {
        let workspaces = std::sync::Arc::make_mut(&mut self.workspaces);
        let mut delta = WorkspaceDelta::default();

        if let Some(old_workspace) = workspaces.get(&root) {
            // find sdk diff
            for (id, sdk) in &new_workspace.sdks {
                if !old_workspace.sdks.contains_key(id) {
                    delta.sdks.added.insert(*id, sdk.clone());
                }
            }
            for (id, sdk) in &old_workspace.sdks {
                if !new_workspace.sdks.contains_key(id) {
                    delta.sdks.removed.insert(*id, sdk.clone());
                }
            }

            // find library diff
            for (id, path) in &new_workspace.library_paths {
                if !old_workspace.library_paths.contains_key(id) {
                    delta.libs.added.insert(*id, path.clone());
                }
            }
            for (id, path) in &old_workspace.library_paths {
                if !old_workspace.library_paths.contains_key(id) {
                    delta.libs.removed.insert(*id, path.clone());
                }
            }

            // find projects diff
            for (id, new_project) in &new_workspace.projects {
                if let Some(old_project) = old_workspace.projects.get(id) {
                    // changed project
                    if new_project != old_project {
                        delta
                            .projects
                            .changed
                            .insert(*id, (old_project.clone(), new_project.clone()));
                    }
                } else {
                    delta.projects.added.insert(*id, new_project.clone());
                }
            }
            for (id, data) in &old_workspace.projects {
                if !new_workspace.projects.contains_key(id) {
                    delta.projects.removed.insert(*id, data.clone());
                }
            }
        } else {
            delta.sdks.added = new_workspace.sdks.clone();
            delta.libs.added = new_workspace.library_paths.clone();
            delta.projects.added = new_workspace.projects.clone();
        }

        workspaces.insert(root, new_workspace);

        delta
    }

    pub fn remove_workspace(&mut self, root: &AbsPathBuf) {
        let workspaces = Arc::make_mut(&mut self.workspaces);
        workspaces.remove(root);
    }
}
