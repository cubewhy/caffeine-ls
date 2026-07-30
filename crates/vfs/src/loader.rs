use std::path::{Path, PathBuf};

mod notify;

pub use notify::NotifyHandle;

use crate::VfsPath;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LoadingProgress {
    Started,
    Progress(usize),
    Finished,
}

/// Message about an action taken by a [`Handle`].
pub enum Message {
    /// Indicate a gradual progress.
    ///
    /// This is supposed to be the number of loaded files.
    Progress {
        /// The total files to be loaded.
        n_total: usize,
        /// The files that have been loaded successfully.
        n_done: LoadingProgress,
        /// The dir being loaded, `None` if its for a file.
        dir: Option<VfsPath>,
        /// The [`Config`] version.
        config_version: u32,
    },
    /// The handle loaded the following files' content for the first time.
    Loaded {
        files: Vec<(VfsPath, Option<Vec<u8>>)>,
    },
    /// The handle loaded the following files' content.
    Changed {
        files: Vec<(VfsPath, Option<Vec<u8>>)>,
    },
}

/// Type that will receive [`Messages`](Message) from a [`Handle`].
pub type Sender = crossbeam_channel::Sender<Message>;

#[derive(Debug, Clone)]
pub enum Entry {
    /// The `Entry` is represented by a raw set of files.
    Files(Vec<VfsPath>),
    /// The `Entry` is represented by `Directories`.
    Directories(Directories),
}

/// Specifies a set of files on the file system.
///
/// A file is included if:
///   * it has included extension
///   * it is under an `include` path
///   * it is not under `exclude` path
///
/// If many include/exclude paths match, the longest one wins.
///
/// If a path is in both `include` and `exclude`, the `exclude` one wins.
#[derive(Debug, Clone, Default)]
pub struct Directories {
    pub extensions: Vec<String>,
    pub include: Vec<VfsPath>,
    pub exclude: Vec<VfsPath>,
}

/// [`Handle`]'s configuration.
#[derive(Debug)]
pub struct Config {
    /// Version number to associate progress updates to the right config
    /// version.
    pub version: u32,
    /// Set of initially loaded files.
    pub load: Vec<Entry>,
    /// Index of watched entries in `load`.
    ///
    /// If a path in a watched entry is modified,the [`Handle`] should notify it.
    pub watch: Vec<usize>,
}

pub trait Handle: Send + Sync + 'static {
    /// Spawn a new handle with the given `sender`.
    fn spawn(sender: Sender) -> Self
    where
        Self: Sized;

    /// Set this handle's configuration.
    fn set_config(&mut self, config: Config);

    /// The file's content at `path` has been modified, and should be reloaded.
    fn invalidate(&mut self, path: PathBuf);

    /// Load the content of the given file, returning [`None`] if it does not
    /// exists.
    fn load_sync(&mut self, path: &Path) -> Option<Vec<u8>>;
}
