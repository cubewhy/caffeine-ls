use crate::index::NameTable;
use crate::salsa_db::SourceFile;
use crate::salsa_queries::Db;
/// Salsa-aware context for language operations
///
/// This module provides SalsaContext which replaces ParseEnv and gives
/// access to cached Salsa queries.
use std::sync::Arc;

/// Salsa-aware context for language operations
///
/// This replaces ParseEnv and provides access to cached Salsa queries.
/// All operations go through Salsa for automatic memoization.
pub struct SalsaContext<'db> {
    pub db: &'db dyn Db,
    pub file: SourceFile,
    workspace: Option<Arc<crate::workspace::Workspace>>,
}

impl<'db> SalsaContext<'db> {
    /// Create a new SalsaContext
    pub fn new(db: &'db dyn Db, file: SourceFile) -> Self {
        Self {
            db,
            file,
            workspace: None,
        }
    }

    /// Create a SalsaContext with workspace reference
    pub fn with_workspace(
        db: &'db dyn Db,
        file: SourceFile,
        workspace: Arc<crate::workspace::Workspace>,
    ) -> Self {
        Self {
            db,
            file,
            workspace: Some(workspace),
        }
    }

    /// Get package (cached by Salsa)
    pub fn package(&self) -> Option<Arc<str>> {
        crate::salsa_queries::extract_package(self.db, self.file)
    }

    /// Get imports (cached by Salsa)
    pub fn imports(&self) -> Arc<Vec<Arc<str>>> {
        crate::salsa_queries::extract_imports(self.db, self.file)
    }

    /// Get name table (cached by Salsa)
    pub fn name_table(&self) -> Option<Arc<NameTable>> {
        // For now, return None - this will be implemented when we integrate
        // with the workspace model to determine the file's context
        None
    }

    /// Get workspace reference
    pub fn workspace(&self) -> Option<&Arc<crate::workspace::Workspace>> {
        self.workspace.as_ref()
    }

    /// Get file content
    pub fn content(&self) -> &str {
        self.file.content(self.db)
    }

    /// Get language ID
    pub fn language_id(&self) -> Arc<str> {
        self.file.language_id(self.db)
    }

    /// Get file URI
    pub fn file_uri(&self) -> Arc<str> {
        Arc::from(self.file.file_id(self.db).as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::salsa_db::{Database, FileId};
    use tower_lsp::lsp_types::Url;

    #[test]
    fn test_salsa_context_basic() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            "package com.example;\npublic class Test {}".to_string(),
            Arc::from("java"),
        );

        let ctx = SalsaContext::new(&db, file);

        assert_eq!(ctx.language_id().as_ref(), "java");
        assert_eq!(ctx.package().as_deref(), Some("com/example"));
    }

    #[test]
    fn test_salsa_context_imports() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            "import java.util.List;\nimport java.util.Map;\npublic class Test {}".to_string(),
            Arc::from("java"),
        );

        let ctx = SalsaContext::new(&db, file);
        let imports = ctx.imports();

        assert_eq!(imports.len(), 2);
        assert!(imports.iter().any(|i| i.as_ref() == "java.util.List"));
        assert!(imports.iter().any(|i| i.as_ref() == "java.util.Map"));
    }
}
