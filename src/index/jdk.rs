use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::index::{ClassOrigin, cache, merge_source_into_bytecode, parse_class_data_bytes};

use super::{ClassMetadata, index_jar};

const JDK_ORIGIN: &str = "jdk://builtin";

pub struct JdkIndexer {
    java_home: PathBuf,
}

impl JdkIndexer {
    pub fn new(java_home: PathBuf) -> Self {
        Self { java_home }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let java_home = match std::env::var("JAVA_HOME") {
            Ok(h) => PathBuf::from(h),
            Err(_) => {
                warn!("JAVA_HOME not set, JDK classes will not be indexed");
                return Err(anyhow::anyhow!("No JAVA_HOME environment variable defined"));
            }
        };

        Ok(Self { java_home })
    }

    /// Try to find and index JDK classes.
    /// Returns empty vec if JAVA_HOME is not set or JDK cannot be parsed.
    pub fn index(&self) -> Vec<ClassMetadata> {
        let java_home = &self.java_home;

        info!(java_home = %java_home.display(), "indexing JDK");

        // Try to locate JDK sources
        let src_zip = java_home.join("lib").join("src.zip");
        let src_zip = if src_zip.exists() {
            src_zip
        } else {
            java_home.join("src.zip")
        };

        // JDK 9+: lib/modules (jimage)
        let modules = java_home.join("lib").join("modules");
        if modules.exists() {
            info!("detected JDK 9+ (jimage)");
            let mut bytecode_classes = Self::index_jimage(&modules);
            if src_zip.exists() {
                let name_table = crate::index::NameTable::from_classes(&bytecode_classes);
                let source_classes = Self::parse_jdk_source_zip(&src_zip, Some(name_table));
                merge_source_into_bytecode(&mut bytecode_classes, source_classes);
            }
            return bytecode_classes;
        }

        // JDK 8: jre/lib/rt.jar
        let rt_jar = java_home.join("jre").join("lib").join("rt.jar");
        if rt_jar.exists() {
            info!("detected JDK 8 (rt.jar)");
            let mut bytecode_classes = index_jar(&rt_jar).unwrap_or_else(|e| {
                warn!(error = %e, "failed to index rt.jar");
                vec![]
            });
            if src_zip.exists() {
                let name_table = crate::index::NameTable::from_classes(&bytecode_classes);
                let source_classes = Self::parse_jdk_source_zip(&src_zip, Some(name_table));
                merge_source_into_bytecode(&mut bytecode_classes, source_classes);
            }
            return bytecode_classes;
        }

        // JDK 8 alternative layout (some distros)
        // TODO: merge source
        let rt_jar_alt = java_home.join("lib").join("rt.jar");
        if rt_jar_alt.exists() {
            info!("detected JDK 8 alt layout (lib/rt.jar)");
            return index_jar(&rt_jar_alt).unwrap_or_else(|e| {
                warn!(error = %e, "failed to index rt.jar");
                vec![]
            });
        }

        warn!(
            java_home = %java_home.display(),
            "could not find JDK class library (neither lib/modules nor jre/lib/rt.jar)"
        );
        vec![]
    }

    fn index_jimage(path: &Path) -> Vec<ClassMetadata> {
        // Try cache
        if let Some(cached) = crate::index::cache::load_cached(path) {
            tracing::info!(count = cached.len(), "JDK index loaded from cache");
            return cached;
        }

        let origin = Arc::from(JDK_ORIGIN);

        let jimage = match jimage_rs::JImage::open(path) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, path = %path.display(), "failed to open jimage");
                return vec![];
            }
        };

        let mut results = Vec::new();
        let resource_names = jimage.resource_names_iter();

        for entry in resource_names {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    debug!(error = %e, "skipping jimage entry");
                    continue;
                }
            };

            let (module, path_in_module) = entry.get_full_name();

            // Only process .class files, skip module-info
            if !path_in_module.ends_with(".class") {
                continue;
            }
            if path_in_module.ends_with("module-info.class") {
                continue;
            }

            let resource_path = format!("/{}/{}", module, path_in_module);
            let bytes = match jimage.find_resource(&resource_path) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    debug!(path = resource_path, "resource not found in jimage");
                    continue;
                }
                Err(e) => {
                    debug!(error = %e, path = resource_path, "failed to read jimage resource");
                    continue;
                }
            };

            // path_in_module: "java/lang/String.class"
            // file_name for parse_class_data is the path within the module
            match parse_class_data_bytes(&path_in_module, &bytes, Arc::clone(&origin)) {
                Some(meta) => results.push(meta),
                None => debug!(path = resource_path, "failed to parse class"),
            }
        }

        // cache index
        let results_clone = results.clone();
        let path_buf = path.to_path_buf();
        std::thread::spawn(move || {
            cache::save_cache(&path_buf, &results_clone);
        });

        info!(count = results.len(), "JDK jimage indexed");
        results
    }

    fn parse_jdk_source_zip(
        path: &Path,
        name_table: Option<Arc<crate::index::NameTable>>,
    ) -> Vec<ClassMetadata> {
        tracing::info!(
            path = %path.display(),
            table_size = name_table.as_ref().map(|t| t.len()).unwrap_or(0),
            "parse_jdk_source_zip: starting with name_table"
        );

        if let Some(cached) = crate::index::cache::load_cached(path) {
            tracing::info!(count = cached.len(), "JDK source index loaded from cache");
            return cached;
        }

        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return vec![],
        };
        let mut archive = match zip::ZipArchive::new(std::io::BufReader::new(file)) {
            Ok(a) => a,
            Err(_) => return vec![],
        };

        let mut source_files = Vec::new();
        for i in 0..archive.len() {
            let mut entry = match archive.by_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.name().to_string();
            if name.ends_with(".java") {
                let mut buf = String::new();
                if std::io::Read::read_to_string(&mut entry, &mut buf).is_ok() {
                    source_files.push((name, buf));
                }
            }
        }

        let zip_path = Arc::from(path.to_string_lossy().as_ref());
        info!(
            count = source_files.len(),
            "found java files in JDK src.zip"
        );

        let results: Vec<_> = source_files
            .into_par_iter()
            .flat_map(|(name, content)| {
                let origin = ClassOrigin::ZipSource {
                    zip_path: Arc::clone(&zip_path),
                    entry_name: Arc::from(name.as_str()),
                };
                crate::index::source::parse_source_str(&content, "java", origin, name_table.clone())
            })
            .collect();

        let results_clone = results.clone();
        let path_buf = path.to_path_buf();
        std::thread::spawn(move || {
            crate::index::cache::save_cache(&path_buf, &results_clone);
        });

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only runs if JAVA_HOME is set and valid.
    /// Use: JAVA_HOME=/path/to/jdk cargo test test_jdk_index -- --nocapture
    #[test]
    #[ignore]
    fn test_jdk_index_finds_string() {
        let classes = JdkIndexer::from_env()
            .expect("Failed to create JDKIndexer")
            .index();
        if classes.is_empty() {
            eprintln!("JAVA_HOME not set or JDK not found, skipping");
            return;
        }
        eprintln!("JDK classes indexed: {}", classes.len());

        let string_class = classes
            .iter()
            .find(|c| c.internal_name.as_ref() == "java/lang/String");
        assert!(
            string_class.is_some(),
            "java/lang/String should be in JDK index. \
             First 10 classes: {:?}",
            classes
                .iter()
                .take(10)
                .map(|c| c.internal_name.as_ref())
                .collect::<Vec<_>>()
        );

        let string = string_class.unwrap();
        assert_eq!(string.package.as_deref(), Some("java/lang"));
        assert!(
            string.methods.iter().any(|m| m.name.as_ref() == "length"),
            "String should have length() method"
        );
        assert!(
            string
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "substring"),
            "String should have substring() method"
        );
    }

    #[test]
    #[ignore]
    fn test_jdk_index_object_methods() {
        let classes = JdkIndexer::from_env()
            .expect("Failed to create JDKIndexer")
            .index();
        if classes.is_empty() {
            return;
        }
        let object = classes
            .iter()
            .find(|c| c.internal_name.as_ref() == "java/lang/Object");
        assert!(object.is_some(), "java/lang/Object should be indexed");
        let object = object.unwrap();
        for method in &["equals", "hashCode", "toString"] {
            assert!(
                object.methods.iter().any(|m| m.name.as_ref() == *method),
                "Object should have {}()",
                method
            );
        }
    }

    #[test]
    #[ignore]
    fn test_jdk_skips_module_info() {
        let classes = JdkIndexer::from_env()
            .expect("Failed to create JDKIndexer")
            .index();
        assert!(
            classes.iter().all(|c| c.name.as_ref() != "module-info"),
            "module-info should be filtered out"
        );
    }

    #[test]
    #[ignore]
    fn test_jdk_classes_have_valid_internal_names() {
        let classes = JdkIndexer::from_env()
            .expect("Failed to create JDKIndexer")
            .index();
        for cls in classes.iter().take(100) {
            assert!(
                !cls.internal_name.contains('.'),
                "internal_name should use '/' not '.': {}",
                cls.internal_name
            );
        }
    }
}
