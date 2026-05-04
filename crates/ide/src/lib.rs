use ide_db::RootDatabase;
use salsa::{Database, Durability};

/// Snapshot of [AnalysisHost]
pub struct Analysis {
    db: RootDatabase,
}

impl Analysis {
    pub fn trigger_cancellation(&mut self) {
        // We need to do a synthetic write right now due to how fixpoint cycles handle cancellation
        // the revision bump there is a reset marker for clearing fixpoint poisoning.
        // That is `trigger_cancellation` is currently bugged wrt to cancellation.
        // self.db.trigger_cancellation();
        self.db.synthetic_write(Durability::LOW);
    }

    pub fn raw_database(&self) -> &RootDatabase {
        &self.db
    }
}

impl std::panic::UnwindSafe for Analysis {}

pub struct AnalysisHost {
    db: RootDatabase,
}

impl AnalysisHost {
    pub fn new() -> AnalysisHost {
        AnalysisHost {
            db: RootDatabase::new(),
        }
    }

    pub fn with_database(db: RootDatabase) -> AnalysisHost {
        AnalysisHost { db }
    }

    /// Returns a snapshot of the current state, which you can query for
    /// semantic information.
    pub fn analysis(&self) -> Analysis {
        Analysis {
            db: self.db.clone(),
        }
    }

    pub fn trigger_cancellation(&mut self) {
        // We need to do a synthetic write right now due to how fixpoint cycles handle cancellation
        // the revision bump there is a reset marker for clearing fixpoint poisoning.
        // That is `trigger_cancellation` is currently bugged wrt to cancellation.
        // self.db.trigger_cancellation();
        self.db.synthetic_write(Durability::LOW);
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
