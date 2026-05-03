use std::fs::File;
use std::io;

use base_db::path_resolver::VirtualPathHandler;
use dashmap::DashMap;
use parking_lot::Mutex;
use triomphe::Arc;
use zip::ZipArchive;

pub struct JarHandler {
    cache: DashMap<String, Arc<Mutex<ZipArchive<File>>>>,
}

impl JarHandler {
    pub fn new() -> Self {
        Self {
            cache: Default::default(),
        }
    }
}

impl Default for JarHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualPathHandler for JarHandler {
    fn can_handle(&self, protocol: &str) -> bool {
        protocol == "jar"
    }

    fn fetch_bytes(&self, path: &str) -> io::Result<Vec<u8>> {
        // C:/libs/rt.jar!/java/lang/Object.class
        let (jar_path, entry_path) = path
            .split_once('!')
            .ok_or_else(|| io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid Jar Path"))?;

        let archive_arc = self
            .cache
            .entry(jar_path.to_string())
            .or_try_insert_with(|| {
                let file = File::open(jar_path)?;
                let archive = ZipArchive::new(file).map_err(io::Error::other)?;
                Ok::<_, io::Error>(Arc::new(Mutex::new(archive)))
            })?
            .clone();

        let mut archive = archive_arc.lock();

        let mut file = archive
            .by_name(entry_path.strip_prefix('/').unwrap_or(entry_path))
            .map_err(|e| io::Error::new(std::io::ErrorKind::NotFound, e))?;

        let mut buf = Vec::with_capacity(file.size() as usize);
        io::copy(&mut file, &mut buf)?;
        Ok(buf)
    }
}
