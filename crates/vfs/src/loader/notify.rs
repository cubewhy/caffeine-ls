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
    time::Duration,
};

pub struct NotifyHandle {
    debouncer: Debouncer<RecommendedWatcher>,
    currently_watched: FxHashSet<VfsPath>,
}

impl Handle for NotifyHandle {
    fn spawn(sender: Sender) -> Self {
        let debouncer = new_debouncer(
            Duration::from_millis(100),
            move |res: Result<Vec<DebouncedEvent>, _>| match res {
                Ok(events) => {
                    let files: Vec<(VfsPath, Option<Vec<u8>>)> = events
                        .into_iter()
                        .map(|e| {
                            let vfs_path = VfsPath::Physical(e.path.try_into().unwrap());

                            (vfs_path, None)
                        })
                        .collect();

                    if !files.is_empty() {
                        let _ = sender.send(Message::Changed { files });
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
        }
    }

    fn set_config(&mut self, config: Config) {
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
