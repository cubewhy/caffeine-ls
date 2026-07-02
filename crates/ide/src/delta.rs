use ide_db::symbol::LibraryId;
use project_model::{ProjectData, ProjectId, SdkData, SdkId};
use rustc_hash::FxHashMap;
use triomphe::Arc;
use vfs::AbsPathBuf;

#[derive(Debug, Clone, Default)]
pub struct SdkDelta {
    /// Added SDKs
    pub added: FxHashMap<SdkId, Arc<SdkData>>,
    /// Removed SDKs
    pub removed: FxHashMap<SdkId, Arc<SdkData>>,
}

impl SdkDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct LibraryDelta {
    /// Added dependencies
    pub added: FxHashMap<LibraryId, AbsPathBuf>,
    /// Removed dependencies
    pub removed: FxHashMap<LibraryId, AbsPathBuf>,
}

impl LibraryDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProjectsDelta {
    /// Added projects
    pub added: FxHashMap<ProjectId, Arc<ProjectData>>,
    /// Removed projects
    pub removed: FxHashMap<ProjectId, Arc<ProjectData>>,
    /// Changed projects
    pub changed: FxHashMap<ProjectId, (Arc<ProjectData>, Arc<ProjectData>)>, // (old, new)
}

impl ProjectsDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceDelta {
    pub sdks: SdkDelta,
    pub libs: LibraryDelta,
    pub projects: ProjectsDelta,
}

impl WorkspaceDelta {
    pub fn is_empty(&self) -> bool {
        self.sdks.is_empty() && self.libs.is_empty() && self.projects.is_empty()
    }
}
