//! Defines a unit of change that can applied to the database to get the next
//! state. Changes are transactional.

use std::fmt;

use rustc_hash::FxHashSet;
use salsa::{Durability, Setter as _};
use triomphe::Arc;
use vfs::FileId;

use crate::{LocalRoots, SourceDatabase, SourceRoot, SourceRootId};

/// Encapsulate a bunch of raw `.set` calls on the database.
#[derive(Default)]
pub struct FileChange {
    pub roots: Option<Vec<SourceRoot>>,
    pub files_changed: Vec<(FileId, Option<String>)>,
}

impl fmt::Debug for FileChange {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = fmt.debug_struct("Change");
        if let Some(roots) = &self.roots {
            d.field("roots", roots);
        }
        if !self.files_changed.is_empty() {
            d.field("files_changed", &self.files_changed.len());
        }
        d.finish()
    }
}

impl FileChange {
    pub fn set_roots(&mut self, roots: Vec<SourceRoot>) {
        self.roots = Some(roots);
    }

    pub fn change_file(&mut self, file_id: FileId, new_text: Option<String>) {
        self.files_changed.push((file_id, new_text))
    }

    pub fn apply(self, db: &mut dyn SourceDatabase) {
        let _p = tracing::info_span!("FileChange::apply").entered();
        if let Some(roots) = self.roots {
            let mut local_roots = FxHashSet::default();
            for (idx, root) in roots.into_iter().enumerate() {
                let root_id = SourceRootId(idx as u32);
                local_roots.insert(root_id);
                for file_id in root.iter() {
                    db.set_file_source_root_with_durability(file_id, root_id, Durability::LOW);
                }

                db.set_source_root_with_durability(root_id, Arc::new(root), Durability::LOW);
            }
            match LocalRoots::try_get(db) {
                Some(singleton) => {
                    // Re-setting an unchanged root set is a no-op: salsa 0.28
                    // records a write (and bumps the revision counter) on every
                    // `set`, so skipping equal writes keeps a `didChange` that
                    // only touched file text from invalidating root-keyed
                    // queries like `source_root_symbols_query`.
                    if *singleton.roots(db) != local_roots {
                        singleton.set_roots(db).to(local_roots);
                    }
                }
                None => {
                    LocalRoots::new(db, local_roots);
                }
            }
        }

        for (file_id, text) in self.files_changed {
            // XXX: can't actually remove the file, just reset the text
            let text = text.unwrap_or_default();
            db.set_file_text_with_durability(file_id, &text, Durability::LOW)
        }
    }
}
