use std::panic::AssertUnwindSafe;

use ide_db::{
    RootDatabase,
    base_db::{FileChange, salsa::Cancelled},
};

pub use ide_db::line_index::{LineCol, LineIndex};

pub mod delta;

pub type Cancellable<T> = Result<T, Cancelled>;

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

/// Snapshot of [AnalysisHost]
pub struct Analysis {
    db: RootDatabase,
}

impl std::panic::UnwindSafe for Analysis {}

impl Analysis {
    pub fn raw_database(&self) -> &RootDatabase {
        &self.db
    }

    /// Performs an operation on the database that may be canceled.
    ///
    /// rust-analyzer needs to be able to answer semantic questions about the
    /// code while the code is being modified. A common problem is that a
    /// long-running query is being calculated when a new change arrives.
    ///
    /// We can't just apply the change immediately: this will cause the pending
    /// query to see inconsistent state (it will observe an absence of
    /// repeatable read). So what we do is we **cancel** all pending queries
    /// before applying the change.
    ///
    /// Salsa implements cancellation by unwinding with a special value and
    /// catching it on the API boundary.
    fn with_db<F, T>(&self, f: F) -> Cancellable<T>
    where
        F: FnOnce(&RootDatabase) -> T + std::panic::UnwindSafe,
    {
        Cancelled::catch(AssertUnwindSafe(|| f(&self.db)))
    }
}
