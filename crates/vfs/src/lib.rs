use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashMap;
use url::Url;

use crate::virtual_path::VirtualPathHandler;

pub mod loader;
pub mod virtual_path;

#[derive(Debug, Clone, Copy, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct FileId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AbsPathBuf(Utf8PathBuf);

impl AbsPathBuf {
    pub fn assert_utf8(path: std::path::PathBuf) -> Self {
        let utf8_path = Utf8PathBuf::try_from(path).expect("Path is not valid UTF-8");

        assert!(
            utf8_path.is_absolute(),
            "Path is not absolute: {}",
            utf8_path
        );

        Self(utf8_path)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn assert_from(base: &AbsPath, path: impl AsRef<Utf8Path>) -> Self {
        let joined = base.join(path);
        Self(joined)
    }
}

impl TryFrom<Utf8PathBuf> for AbsPathBuf {
    type Error = Utf8PathBuf;

    fn try_from(path: Utf8PathBuf) -> Result<Self, Self::Error> {
        if path.is_absolute() {
            Ok(Self(path))
        } else {
            Err(path)
        }
    }
}

impl TryFrom<PathBuf> for AbsPathBuf {
    type Error = PathBuf;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        if !path.is_absolute() {
            return Err(path);
        }

        match Utf8PathBuf::try_from(path) {
            Ok(utf8_path) => Ok(Self(utf8_path)),
            Err(err) => Err(err.into_path_buf()),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct AbsPath(Utf8Path);

impl AbsPath {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn assert(path: &Utf8Path) -> &Self {
        assert!(path.is_absolute(), "path is not absolute: {}", path);
        unsafe { Self::new_unchecked(path) }
    }

    /// # Safety
    /// AbsPath is a wrapper type of Utf8Path, so the conversion is safe
    pub unsafe fn new_unchecked(path: &Utf8Path) -> &Self {
        unsafe { &*(path as *const Utf8Path as *const AbsPath) }
    }
}

impl std::ops::Deref for AbsPathBuf {
    type Target = AbsPath;

    fn deref(&self) -> &AbsPath {
        unsafe { AbsPath::new_unchecked(&self.0) }
    }
}

impl std::ops::Deref for AbsPath {
    type Target = Utf8Path;

    fn deref(&self) -> &Utf8Path {
        &self.0
    }
}

impl std::borrow::Borrow<AbsPath> for AbsPathBuf {
    fn borrow(&self) -> &AbsPath {
        self
    }
}

impl AsRef<Utf8Path> for AbsPathBuf {
    fn as_ref(&self) -> &Utf8Path {
        &self.0
    }
}

impl AsRef<Utf8Path> for AbsPath {
    fn as_ref(&self) -> &Utf8Path {
        &self.0
    }
}

impl AsRef<Path> for AbsPathBuf {
    fn as_ref(&self) -> &Path {
        self.0.as_std_path()
    }
}

impl AsRef<Path> for AbsPath {
    fn as_ref(&self) -> &Path {
        self.0.as_std_path()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VfsPath {
    Physical(AbsPathBuf),
    Virtual(Url),
}

impl VfsPath {
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        match self {
            VfsPath::Physical(path) => path.as_str().into(),
            VfsPath::Virtual(url) => std::borrow::Cow::Borrowed(url.as_str()),
        }
    }

    pub fn into_physical(self) -> Option<AbsPathBuf> {
        match self {
            VfsPath::Physical(path_buf) => Some(path_buf),
            VfsPath::Virtual(_) => None,
        }
    }

    pub fn exists(&self) -> bool {
        match self {
            VfsPath::Physical(path_buf) => path_buf.exists(),
            VfsPath::Virtual(_) => true,
        }
    }

    pub fn extension(&self) -> Option<&str> {
        match self {
            VfsPath::Physical(path) => path.extension(),
            VfsPath::Virtual(url) => {
                let file_name = url.path_segments()?.next_back()?;

                let dot_idx = file_name.rfind('.')?;

                if dot_idx == 0 {
                    return None;
                }

                Some(&file_name[dot_idx + 1..])
            }
        }
    }

    pub fn to_url(&self) -> Url {
        match self {
            VfsPath::Physical(abs_path) => Url::from_file_path(abs_path).expect(
                "AbsPathBuf is guaranteed to be absolute, please report this to developers!",
            ),
            VfsPath::Virtual(url) => url.clone(),
        }
    }
}

impl From<&Url> for VfsPath {
    fn from(url: &Url) -> Self {
        if url.scheme() == "file"
            && let Ok(path) = url.to_file_path()
            && let Ok(abs_path) = AbsPathBuf::try_from(path)
        {
            return VfsPath::Physical(abs_path);
        }

        VfsPath::Virtual(url.clone())
    }
}

#[derive(Default)]
pub struct Vfs {
    next_id: u32,

    path_to_id: FxHashMap<VfsPath, FileId>,
    id_to_path: FxHashMap<FileId, VfsPath>,

    overlays: FxHashMap<FileId, Arc<[u8]>>,

    handlers: Vec<Box<dyn VirtualPathHandler>>,
    pending_events: Vec<VfsEvent>,
}

impl Vfs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_handler(&mut self, handler: impl VirtualPathHandler + 'static) {
        self.handlers.push(Box::new(handler));
    }

    pub fn alloc_file_id(&mut self, path: VfsPath) -> FileId {
        if let Some(&id) = self.path_to_id.get(&path) {
            return id;
        }

        let id = FileId(self.next_id);
        self.next_id += 1;

        self.pending_events.push(VfsEvent::Created {
            id,
            path: path.clone(),
        });

        self.path_to_id.insert(path.clone(), id);
        self.id_to_path.insert(id, path);

        id
    }

    pub fn file_path(&self, id: FileId) -> Option<&VfsPath> {
        self.id_to_path.get(&id)
    }

    pub fn file_id(&self, path: &VfsPath) -> Option<FileId> {
        self.path_to_id.get(path).copied()
    }

    pub fn set_overlay(&mut self, id: FileId, content: Vec<u8>) {
        self.overlays.insert(id, Arc::from(content));
        self.pending_events.push(VfsEvent::Modified { id });
    }

    pub fn clear_overlay(&mut self, id: FileId) {
        if self.overlays.remove(&id).is_some() {
            self.pending_events.push(VfsEvent::Modified { id });
        }
    }

    pub fn take_events(&mut self) -> Vec<VfsEvent> {
        std::mem::take(&mut self.pending_events)
    }

    pub fn set_file_contents(&mut self, path: VfsPath, content: Option<Vec<u8>>) -> FileId {
        let file_id = self.alloc_file_id(path);
        if let Some(content) = content {
            self.set_overlay(file_id, content);
        } else {
            self.clear_overlay(file_id);
        }
        file_id
    }

    pub fn fetch_content(&self, id: FileId) -> std::io::Result<Arc<[u8]>> {
        if let Some(content) = self.overlays.get(&id) {
            return Ok(content.clone());
        }

        let path = self
            .id_to_path
            .get(&id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "FileId not found"))?;

        match path {
            VfsPath::Physical(p) => {
                let bytes = std::fs::read(p)?;
                Ok(Arc::from(bytes))
            }
            VfsPath::Virtual(url) => {
                let scheme = url.scheme();

                for handler in &self.handlers {
                    if handler.can_handle(scheme) {
                        let bytes = handler.fetch_bytes(url)?;
                        return Ok(Arc::from(bytes));
                    }
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!("No handler registered for scheme: {}", scheme),
                ))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsEvent {
    Created { id: FileId, path: VfsPath },
    Modified { id: FileId },
    Deleted { id: FileId },
}
