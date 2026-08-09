use ide_db::{RootDatabase, base_db::FileChange};

pub use ide_db::line_index::{LineCol, LineIndex};

pub mod delta;

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

    pub fn apply_change(&mut self, change: FileChange) {
        change.apply(&mut self.db);
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
