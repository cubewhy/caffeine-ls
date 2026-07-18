use crate::{lexical::LexicalIndex, symbol::GlobalSymbolIndex};
use dashmap::DashMap;
use project_model::{LibraryId, SdkId};
use std::path::Path;

pub mod lexical;
pub mod symbol;

pub struct IndexDatabase {
    pub lexical: LexicalIndex,
    pub symbols: GlobalSymbolIndex,
    pub sdk_libraries: DashMap<SdkId, LibraryId>,
}

impl IndexDatabase {
    pub fn new(global_cache_dir: &Path, project_root: &Path) -> Self {
        Self {
            lexical: LexicalIndex::new(),
            symbols: GlobalSymbolIndex::new(global_cache_dir, project_root),
            sdk_libraries: DashMap::new(),
        }
    }
}
