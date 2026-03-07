use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::decompiler::{
    Decompiler,
    backend::{cfr::CfrDecompiler, vineflower::VineflowerDecompiler},
};

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

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaAnalyzerConfig {
    pub jdk_path: Option<std::path::PathBuf>,
    pub decompiler_path: Option<std::path::PathBuf>,
    pub decompiler_backend: DecompilerType,
}

impl Default for JavaAnalyzerConfig {
    fn default() -> Self {
        Self {
            jdk_path: None,
            decompiler_path: None,
            decompiler_backend: DecompilerType::Vineflower,
        }
    }
}

impl JavaAnalyzerConfig {
    pub fn get_java_bin(&self) -> PathBuf {
        self.jdk_path
            .as_ref()
            .map(|p| p.join("bin").join("java"))
            .unwrap_or_else(|| PathBuf::from("java"))
    }
}
