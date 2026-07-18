use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use dashmap::DashMap;
use ide_db::IndexDatabase;
use lasso::ThreadedRodeo;
use parking_lot::RwLock;
use project_model::{ClasspathEntry, WorkspaceGraph};
use rustc_hash::FxHashMap;
use syntax::SyntaxError;
use vfs::{AbsPathBuf, FileId};

use crate::delta::WorkspaceDelta;

pub mod delta;

pub struct ParsedFile {
    pub revision: u64,
    pub green_node: rowan::GreenNode,
    pub syntax_errors: Vec<SyntaxError>,
}

impl ParsedFile {
    pub fn new(
        revision: u64,
        green_node: rowan::GreenNode,
        syntax_errors: Vec<SyntaxError>,
    ) -> Self {
        Self {
            revision,
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
        let parsed = self.trees.get(&file_id)?.clone();
        let current_revision = *self.file_revisions.get(&file_id)?;
        (parsed.revision == current_revision).then_some(parsed)
    }

    /// Bumps the revision for a file and returns the new revision number.
    pub fn bump_revision(&self, file_id: FileId) -> u64 {
        let mut rev = self.file_revisions.entry(file_id).or_insert(0);
        *rev += 1;
        self.trees.remove(&file_id);
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
    indexes: Arc<RwLock<FxHashMap<AbsPathBuf, Arc<IndexDatabase>>>>,
    parse_cache: Arc<ParseCache>,
}

impl Analysis {
    pub fn parsed_file(&self, file_id: FileId) -> Option<Arc<ParsedFile>> {
        self.parse_cache.get_tree(file_id)
    }

    pub fn resolve_class_for_path(
        &self,
        path: &AbsPathBuf,
        fqn: &str,
    ) -> Option<triomphe::Arc<syntax::ClassStub>> {
        let (root, workspace) = self
            .workspaces
            .iter()
            .filter(|(root, _)| path.starts_with(root))
            .max_by_key(|(root, _)| root.as_str().len())?;
        let (project, source_set_kind) = workspace.resolve_source_set_for_path(path)?;
        let source_set = project.source_sets.get(&source_set_kind)?;
        let index = self.indexes.read().get(root)?.clone();
        let mut allowed = std::collections::HashSet::new();
        for entry in &source_set.compile_classpath {
            match entry {
                ClasspathEntry::External(library_id) => {
                    allowed.insert(*library_id);
                }
                ClasspathEntry::Sdk(sdk_id) => {
                    if let Some(library_id) = index.sdk_libraries.get(sdk_id) {
                        allowed.insert(*library_id);
                    }
                }
                ClasspathEntry::Internal { .. } => {}
            }
        }
        index.symbols.resolve_class_scoped(fqn, &allowed)
    }
}

impl std::panic::UnwindSafe for Analysis {}

pub struct AnalysisHost {
    pub(crate) workspaces: Arc<FxHashMap<AbsPathBuf, WorkspaceGraph>>,
    cache_dir: PathBuf,
    parse_cache: Arc<ParseCache>,
    interner: Arc<ThreadedRodeo>,
    indexes: Arc<RwLock<FxHashMap<AbsPathBuf, Arc<IndexDatabase>>>>,
}

impl AnalysisHost {
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            workspaces: Arc::new(HashMap::default()),
            cache_dir: cache_dir.to_path_buf(),
            parse_cache: Arc::new(ParseCache::default()),
            interner: Arc::new(ThreadedRodeo::new()),
            indexes: Arc::new(RwLock::new(FxHashMap::default())),
        }
    }

    pub fn snapshot(&self) -> Analysis {
        Analysis {
            workspaces: self.workspaces.clone(),
            indexes: Arc::clone(&self.indexes),
            parse_cache: Arc::clone(&self.parse_cache),
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
                match old_workspace.sdks.get(id) {
                    None => {
                        delta.sdks.added.insert(*id, sdk.clone());
                    }
                    Some(old_sdk) if old_sdk != sdk => {
                        delta.sdks.removed.insert(*id, old_sdk.clone());
                        delta.sdks.added.insert(*id, sdk.clone());
                    }
                    Some(_) => {}
                }
            }
            for (id, sdk) in &old_workspace.sdks {
                if !new_workspace.sdks.contains_key(id) {
                    delta.sdks.removed.insert(*id, sdk.clone());
                }
            }

            // find library diff
            for (id, library) in &new_workspace.library_paths {
                match old_workspace.library_paths.get(id) {
                    None => {
                        delta.libs.added.insert(*id, library.clone());
                    }
                    Some(old_library) if old_library != library => {
                        delta.libs.removed.insert(*id, old_library.clone());
                        delta.libs.added.insert(*id, library.clone());
                    }
                    Some(_) => {}
                }
            }
            for (id, library) in &old_workspace.library_paths {
                if !new_workspace.library_paths.contains_key(id) {
                    delta.libs.removed.insert(*id, library.clone());
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

        let root = workspaces
            .keys()
            .find(|candidate| !self.indexes.read().contains_key(*candidate))
            .cloned();
        if let Some(root) = root {
            let index = Arc::new(IndexDatabase::new(
                &self.cache_dir.join("symbols"),
                root.as_std_path(),
            ));
            self.indexes.write().insert(root, index);
        }

        delta
    }

    pub fn remove_workspace(&mut self, root: &AbsPathBuf) {
        let workspaces = Arc::make_mut(&mut self.workspaces);
        workspaces.remove(root);
        self.indexes.write().remove(root);
    }

    pub fn workspaces(&self) -> &FxHashMap<AbsPathBuf, WorkspaceGraph> {
        &self.workspaces
    }

    pub fn parse_cache(&self) -> Arc<ParseCache> {
        Arc::clone(&self.parse_cache)
    }

    pub fn interner(&self) -> Arc<ThreadedRodeo> {
        Arc::clone(&self.interner)
    }

    pub fn index_for_path(&self, path: &AbsPathBuf) -> Option<(AbsPathBuf, Arc<IndexDatabase>)> {
        let root = self
            .workspaces
            .keys()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.as_str().len())?
            .clone();
        let index = self.indexes.read().get(&root)?.clone();
        Some((root, index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use project_model::{Library, LibraryId};

    #[test]
    fn workspace_delta_reports_removed_libraries() {
        let temp = tempfile::tempdir().unwrap();
        let root = AbsPathBuf::try_from(temp.path().to_path_buf()).unwrap();
        let jar_path = root.join("dependency.jar");
        std::fs::write(jar_path.as_std_path(), b"jar").unwrap();
        let library_id = LibraryId::from_jar_path(jar_path.as_std_path()).unwrap();

        let mut initial = WorkspaceGraph::default();
        initial.library_paths.insert(
            library_id,
            Library::readonly(
                library_id,
                AbsPathBuf::try_from(jar_path.into_std_path_buf()).unwrap(),
            ),
        );

        let mut host = AnalysisHost::new(&temp.path().join("cache"));
        host.apply_workspace_change(root.clone(), initial);
        let delta = host.apply_workspace_change(root, WorkspaceGraph::default());
        assert!(delta.libs.removed.contains_key(&library_id));
    }

    #[test]
    fn bumping_revision_hides_stale_parse_results() {
        let cache = ParseCache::default();
        let file_id = FileId(7);
        let revision = cache.bump_revision(file_id);
        let parsed = syntax::parse_file(
            syntax::LanguageId::Java,
            "class Old {}",
            &ThreadedRodeo::new(),
        );
        cache.update(
            file_id,
            ParsedFile::new(revision, parsed.tree, parsed.errors),
        );
        assert!(cache.get_tree(file_id).is_some());

        cache.bump_revision(file_id);
        assert!(cache.get_tree(file_id).is_none());
    }
}
