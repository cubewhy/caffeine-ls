use triomphe::Arc;

pub struct PathResolver {
    handlers: Vec<Arc<dyn VirtualPathHandler>>,
}

impl PathResolver {
    pub fn new(handlers: Vec<Arc<dyn VirtualPathHandler>>) -> Self {
        Self { handlers }
    }

    pub fn resolve(&self, path: &vfs::VfsPath) -> std::io::Result<Vec<u8>> {
        // resolve from filesystem
        if let Some(abs_path) = path.as_path() {
            return std::fs::read(abs_path);
        }

        let path_str = path.to_string();

        if let Some((protocol, remainder)) = path_str.split_once("://") {
            for handler in &self.handlers {
                if handler.can_handle(protocol) {
                    return handler.fetch_bytes(remainder);
                }
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("No handler found for path: {}", path),
        ))
    }
}

pub trait VirtualPathHandler: Send + Sync {
    /// Determine if this handler can parse a certain protocol.
    fn can_handle(&self, protocol: &str) -> bool;

    /// Get bytes.
    ///
    /// The input is the path after the protocol has been stripped.
    /// For example:
    ///   Raw uri: `protocol:///a.txt`
    ///   Path without protocol: `/a.txt`
    fn fetch_bytes(&self, path: &str) -> std::io::Result<Vec<u8>>;
}
