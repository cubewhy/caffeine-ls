use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info};
use walkdir::WalkDir;

use super::source::parse_source_file;
use super::{ClassMetadata, ClassOrigin};

/// Scan result
pub struct CodebaseIndex {
    pub classes: Vec<ClassMetadata>,
    /// Number of files actually scanned
    pub file_count: usize,
}

/// Index the entire codebase directory
///
/// - Recursively scan all `.java` / `.kt` files under `root`
/// - Parallel parsing
/// - Skip directories such as `target/`, `build/`, `.git/`, etc.
pub fn index_codebase<P: AsRef<Path>>(
    root: P,
    name_table: Option<Arc<crate::index::NameTable>>,
) -> CodebaseIndex {
    // TODO: respect .gitignore
    let root = root.as_ref();
    info!(root = %root.display(), "Two-pass indexing started");

    let source_files: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_excluded(e.path()))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            let ext = e.path().extension().and_then(|s| s.to_str());
            matches!(ext, Some("java") | Some("kt"))
        })
        .map(|e| e.into_path())
        .collect();

    let file_count = source_files.len();

    tracing::debug!("discovering stubs...");
    let discovered_names: Vec<Arc<str>> = source_files
        .par_iter()
        .flat_map(|path| {
            let content = std::fs::read_to_string(path).ok()?;
            let lang = if path.extension().and_then(|s| s.to_str()) == Some("kt") {
                "kotlin"
            } else {
                "java"
            };
            Some(super::source::discover_internal_names_str(&content, lang))
        })
        .flatten()
        .collect();

    let enriched_name_table = match name_table {
        Some(existing) => existing.extend_with(discovered_names),
        None => crate::index::NameTable::from_names(discovered_names),
    };

    debug!("full structural analysis...");
    let classes: Vec<ClassMetadata> = source_files
        .into_par_iter()
        .flat_map(|path| {
            let uri = path_to_uri_str(&path);
            let origin = ClassOrigin::SourceFile(Arc::from(uri.as_str()));
            parse_source_file(&path, origin, Some(enriched_name_table.clone()))
        })
        .collect();

    info!(
        classes = classes.len(),
        files = file_count,
        "Codebase indexing complete"
    );

    CodebaseIndex {
        classes,
        file_count,
    }
}

/// Parse source text from memory (for LSP textDocument/didChange)
pub fn index_source_text(
    uri: &str,
    content: &str,
    lang: &str,
    name_table: Option<Arc<crate::index::NameTable>>,
) -> Vec<ClassMetadata> {
    let origin = ClassOrigin::SourceFile(Arc::from(uri));
    super::source::parse_source_str(content, lang, origin, name_table)
}

fn is_excluded(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str().unwrap_or(""),
            "target"
                | "build"
                | ".git"
                | ".gradle"
                | "node_modules"
                | ".idea"
                | "out"
                | "dist"
                | ".kotlin"
                | "generated"
                | "__pycache__"
        )
    })
}

fn path_to_uri_str(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", abs.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_test_dir() -> TempDir {
        let dir = tempfile::tempdir().unwrap();

        fs::write(
            dir.path().join("Foo.java"),
            r#"
package com.example;
public class Foo {
    private String name;
    public String getName() { return name; }
    public void setName(String name) { this.name = name; }
    public static class Inner {}
}
"#,
        )
        .unwrap();

        fs::write(
            dir.path().join("Bar.kt"),
            r#"
package com.example
class Bar(val value: String) {
    fun process(input: Int): String = ""
    companion object {
        fun create(): Bar = Bar("")
    }
}
"#,
        )
        .unwrap();

        // target/ dir: should be skipped
        fs::create_dir_all(dir.path().join("target")).unwrap();
        fs::write(
            dir.path().join("target/Ignored.java"),
            "package x; class Ignored {}",
        )
        .unwrap();

        dir
    }

    #[test]
    fn test_index_codebase_finds_files() {
        let dir = make_test_dir();
        let result = index_codebase(dir.path(), None);

        assert_eq!(
            result.file_count, 2,
            "should find 2 source files (not target/)"
        );
        assert!(
            result.classes.iter().any(|c| c.name.as_ref() == "Foo"),
            "classes: {:?}",
            result
                .classes
                .iter()
                .map(|c| c.name.as_ref())
                .collect::<Vec<_>>()
        );
        assert!(result.classes.iter().any(|c| c.name.as_ref() == "Bar"));
    }

    #[test]
    fn test_index_codebase_skips_excluded() {
        let dir = make_test_dir();
        let result = index_codebase(dir.path(), None);
        assert!(result.classes.iter().all(|c| c.name.as_ref() != "Ignored"));
    }

    #[test]
    fn test_index_codebase_package() {
        let dir = make_test_dir();
        let result = index_codebase(dir.path(), None);
        let foo = result
            .classes
            .iter()
            .find(|c| c.name.as_ref() == "Foo")
            .unwrap();
        assert_eq!(foo.package.as_deref(), Some("com/example"));
        assert_eq!(foo.internal_name.as_ref(), "com/example/Foo");
    }

    #[test]
    fn test_index_codebase_inner_class() {
        let dir = make_test_dir();
        let result = index_codebase(dir.path(), None);
        let inner = result.classes.iter().find(|c| c.name.as_ref() == "Inner");
        assert!(inner.is_some(), "Inner class should be indexed");
        assert_eq!(inner.unwrap().inner_class_of.as_deref(), Some("Foo"));
    }

    #[test]
    fn test_index_source_text_java() {
        let src = r#"
package org.test;
public class MyService {
    private int count;
    public void run() {}
    public int getCount() { return count; }
}
"#;
        let classes = index_source_text("file:///MyService.java", src, "java", None);
        assert_eq!(classes.len(), 1);
        let cls = &classes[0];
        assert_eq!(cls.name.as_ref(), "MyService");
        assert_eq!(cls.package.as_deref(), Some("org/test"));
        assert!(cls.methods.iter().any(|m| m.name.as_ref() == "run"));
        assert!(cls.methods.iter().any(|m| m.name.as_ref() == "getCount"));
        assert!(cls.fields.iter().any(|f| f.name.as_ref() == "count"));
    }

    #[test]
    fn test_index_source_text_kotlin() {
        let src = r#"
package org.test
class UserRepo(val db: String) {
    fun findById(id: Int): String = ""
    fun save(entity: String) {}
}
"#;
        let classes = index_source_text("file:///UserRepo.kt", src, "kotlin", None);
        assert!(
            classes.iter().any(|c| c.name.as_ref() == "UserRepo"),
            "classes: {:?}",
            classes.iter().map(|c| c.name.as_ref()).collect::<Vec<_>>()
        );
        let cls = classes
            .iter()
            .find(|c| c.name.as_ref() == "UserRepo")
            .unwrap();
        assert!(cls.methods.iter().any(|m| m.name.as_ref() == "findById"));
        assert!(cls.methods.iter().any(|m| m.name.as_ref() == "save"));
    }

    #[test]
    fn test_index_source_text_origin() {
        let src = "package x;\npublic class A {}";
        let uri = "file:///workspace/A.java";
        let classes = index_source_text(uri, src, "java", None);
        assert!(
            classes
                .iter()
                .all(|c| { matches!(&c.origin, ClassOrigin::SourceFile(u) if u.as_ref() == uri) })
        );
    }
}
