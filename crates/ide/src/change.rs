//! The unit of change applied to the analysis database.
//!
//! A workspace load and a batch of edits are the same thing to the database: a
//! new set of source roots, new file texts and, on a load, the project graph the
//! resolver reads. Bundling them here — instead of poking the salsa inputs one
//! by one — keeps the order the writes must happen in (project graph first: it
//! maps the `SourceRootId`s the root write assigns) inside the analysis layer.

use hir::ProjectGraphData;
use ide_db::{
    RootDatabase,
    base_db::{FileChange, SourceRoot},
};
use vfs::FileId;

/// A unit of change to apply to the database in one write.
#[derive(Default)]
pub struct Change {
    source_change: FileChange,
    /// The workspace project graph, present exactly on a workspace (re)load.
    project_graph: Option<ProjectGraphData>,
}

impl Change {
    /// Replaces the database source roots: index `i` of `roots` becomes
    /// `SourceRootId(i)`, the mapping the project graph carries.
    pub fn set_roots(&mut self, roots: Vec<SourceRoot>) {
        self.source_change.set_roots(roots);
    }

    /// Sets a file's text; `None` means the file was deleted.
    pub fn change_file(&mut self, file_id: FileId, new_text: Option<String>) {
        self.source_change.change_file(file_id, new_text);
    }

    /// Registers `data` as the workspace project graph, replacing the previous
    /// one. Libraries no longer reachable are dropped from the per-library
    /// index cache.
    pub fn set_project_graph(&mut self, data: ProjectGraphData) {
        self.project_graph = Some(data);
    }

    /// Applies the change: the project graph first, then the roots and file
    /// texts, so the `SourceRootId`s the graph maps are the ones this same
    /// change assigns (see [`Change::set_roots`]).
    pub(crate) fn apply(self, db: &mut RootDatabase) {
        if let Some(project_graph) = self.project_graph {
            hir::set_project_graph(db, project_graph);
        }
        self.source_change.apply(db);
    }
}
