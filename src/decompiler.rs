use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::decompiler::backend::{cfr::CfrDecompiler, vineflower::VineflowerDecompiler};

pub mod backend;
pub mod cache;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecompilerType {
    Vineflower,
    Cfr,
}

impl DecompilerType {
    pub fn get_decompiler(&self) -> Box<dyn Decompiler + 'static> {
        match self {
            Self::Cfr => Box::new(CfrDecompiler),
            Self::Vineflower => Box::new(VineflowerDecompiler),
        }
    }
}

#[async_trait::async_trait]
pub trait Decompiler: Send + Sync {
    /// Perform the decompilation task
    /// class_path: The path to the .class files on disk or a temporary extraction path
    /// output_dir: The output directory for the decompiled results
    async fn decompile(
        &self,
        java_bin: &Path,
        decompiler_jar: &Path,
        class_data: &[u8],
        output_path: &Path,
    ) -> Result<()>;
}
