use crate::{
    VfsPath,
    loader::{Config, Entry, Handle, Message, Sender},
};
use notify_debouncer_mini::{
    DebouncedEvent, Debouncer, new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
};
use rustc_hash::FxHashSet;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

pub struct NotifyHandle {
    debouncer: Debouncer<RecommendedWatcher>,
    currently_watched: FxHashSet<VfsPath>,
    sender: Sender,
    config_version: Arc<AtomicU32>,
}

impl Handle for NotifyHandle {
    fn spawn(sender: Sender) -> Self {
        let watcher_sender = sender.clone();
        let config_version = Arc::new(AtomicU32::new(0));
        let watcher_config_version = Arc::clone(&config_version);
        let debouncer = new_debouncer(
            Duration::from_millis(100),
            move |res: Result<Vec<DebouncedEvent>, _>| match res {
                Ok(events) => {
                    let files: Vec<(VfsPath, Option<Vec<u8>>)> = events
                        .into_iter()
                        .map(|e| {
                            let contents = std::fs::read(&e.path).ok();
                            let vfs_path = VfsPath::Physical(e.path.try_into().unwrap());
                            (vfs_path, contents)
                        })
                        .collect();

                    if !files.is_empty() {
                        let _ = watcher_sender.send(Message::Changed {
                            files,
                            config_version: watcher_config_version.load(Ordering::Acquire),
                        });
                    }
                }
                Err(err) => {
                    tracing::error!("VFS Watcher error: {err:?}");
                }
            },
        )
        .expect("Failed to create notify-debouncer");

        Self {
            debouncer,
            currently_watched: Default::default(),
            sender,
            config_version,
        }
    }

    fn set_config(&mut self, config: Config) {
        self.config_version.store(config.version, Ordering::Release);
        let mut new_paths = FxHashSet::default();

        for &idx in &config.watch {
            if let Some(entry) = config.load.get(idx) {
                match entry {
                    Entry::Files(files) => {
                        new_paths.extend(files.iter().cloned());
                    }
                    Entry::Directories(dirs) => {
                        new_paths.extend(dirs.include.iter().cloned());
                    }
                }
            }
        }

        let watcher = self.debouncer.watcher();

        for old_path in &self.currently_watched {
            if !new_paths.contains(old_path)
                && let VfsPath::Physical(path) = old_path
                && let Err(e) = watcher.unwatch(path.as_std_path())
            {
                tracing::error!("VFS Watcher failed to unwatch {:?}: {:?}", old_path, e);
            }
        }

        let mut successfully_watched = FxHashSet::default();

        for new_path in new_paths {
            if let VfsPath::Physical(path) = &new_path
                && path.exists()
            {
                if !self.currently_watched.contains(&new_path) {
                    match watcher.watch(path.as_std_path(), RecursiveMode::Recursive) {
                        Ok(_) => {
                            successfully_watched.insert(new_path);
                        }
                        Err(e) => {
                            tracing::error!("VFS Watcher failed to watch {:?}: {:?}", new_path, e);
                        }
                    }
                } else {
                    successfully_watched.insert(new_path);
                }
            }
        }

        self.currently_watched = successfully_watched;

        let sender = self.sender.clone();
        std::thread::spawn(move || load_initial(config, sender));
    }

    fn invalidate(&mut self, path: PathBuf) {
        let is_watched = self.currently_watched.iter().any(|root| match root {
            VfsPath::Physical(phys_root) => path.starts_with(phys_root),
            VfsPath::Virtual(_) => false,
        });

        if is_watched {
            let watcher = self.debouncer.watcher();
            let _ = watcher.unwatch(&path);
            let _ = watcher.watch(&path, RecursiveMode::Recursive);
        }
    }

    fn load_sync(&mut self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }
}

fn load_initial(config: Config, sender: Sender) {
    let mut paths = FxHashSet::default();

    for entry in &config.load {
        match entry {
            Entry::Files(files) => paths.extend(files.iter().cloned()),
            Entry::Directories(dirs) => {
                for include in &dirs.include {
                    let VfsPath::Physical(include) = include else {
                        continue;
                    };
                    if !include.exists() {
                        continue;
                    }
                    for item in walkdir::WalkDir::new(include.as_std_path())
                        .follow_links(false)
                        .into_iter()
                        .filter_map(Result::ok)
                        .filter(|item| item.file_type().is_file())
                    {
                        let path = item.path();
                        let excluded = dirs.exclude.iter().any(|root| match root {
                            VfsPath::Physical(root) => path.starts_with(root.as_std_path()),
                            VfsPath::Virtual(_) => false,
                        });
                        if excluded {
                            continue;
                        }
                        let extension_matches = path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .is_some_and(|ext| dirs.extensions.iter().any(|item| item == ext));
                        if extension_matches
                            && let Ok(path) = crate::AbsPathBuf::try_from(path.to_path_buf())
                        {
                            paths.insert(VfsPath::Physical(path));
                        }
                    }
                }
            }
        }
    }

    let _ = sender.send(Message::Progress {
        n_total: paths.len(),
        n_done: crate::loader::LoadingProgress::Started,
        dir: None,
        config_version: config.version,
    });

    let mut loaded = Vec::with_capacity(paths.len());
    for path in paths {
        let contents = match &path {
            VfsPath::Physical(path) => std::fs::read(path.as_std_path()).ok(),
            VfsPath::Virtual(_) => None,
        };
        loaded.push((path, contents));
    }
    if !loaded.is_empty() {
        let _ = sender.send(Message::Loaded {
            files: loaded,
            config_version: config.version,
        });
    }
    let _ = sender.send(Message::Progress {
        n_total: 0,
        n_done: crate::loader::LoadingProgress::Finished,
        dir: None,
        config_version: config.version,
    });
}
