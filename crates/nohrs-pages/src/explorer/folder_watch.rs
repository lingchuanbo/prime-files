//! The folder on screen follows the disk: a file another program saves,
//! downloads, deletes or renames there shows up (or goes) without F5.
//!
//! Until 05/11/2026 the listing was read on entering a folder, on F5 and
//! after Prime Files' own operations only, so a file saved by an editor or a
//! browser did not appear — a person who tried it said that alone made them
//! leave the explorer they had used before. Explorer itself watches the
//! folders it shows; so does this, with the system's change notifications
//! (`ReadDirectoryChangesW`, through `notify`), not by polling.
//!
//! Only the folder on screen and the Tree view's open folders are watched,
//! not their subfolders: what is not shown needs no refresh. A save is a
//! burst of events (create, write, rename into place), and a download
//! writes for minutes, so events are gathered for [`SETTLE`] and refreshes
//! kept [`SPACING`] apart; each one is the same refresh as F5, which keeps
//! selection, scroll, open folders and preview.

use std::path::Path;
use std::time::{Duration, Instant};

use gpui::{Context, Task, WeakEntity};
use notify::{EventKind, RecursiveMode, Watcher as _};

use super::ExplorerPane;

/// How long a burst of events is gathered before the folder is read.
const SETTLE: Duration = Duration::from_millis(200);
/// The least time between two refreshes of a folder that keeps changing.
const SPACING: Duration = Duration::from_millis(800);
/// How often a refresh put off (an open rename field, a search on screen) is
/// tried again.
const RETRY: Duration = Duration::from_secs(1);

/// The watch on a pane's folders; dropping it stops it.
pub(crate) struct FolderWatch {
    folders: Vec<String>,
    recursive: bool,
    _watcher: notify::RecommendedWatcher,
    _task: Task<()>,
}

impl ExplorerPane {
    /// Watches what is on screen now: the folder, and the folders open in
    /// the Tree view. Called after each listing; a no-op when that set did
    /// not change.
    pub(crate) fn watch_listed_folders(&mut self, cx: &mut Context<Self>) {
        let mut folders: Vec<String> = std::iter::once(self.cwd.clone())
            .chain(self.tree.expanded.iter().cloned())
            // "Este Computador" and other shell places are no folder on disk.
            .filter(|folder| Path::new(folder).is_dir())
            .collect();
        folders.sort();
        folders.dedup();
        let recursive = self.directory_expanded;
        if self
            .folder_watch
            .as_ref()
            .is_some_and(|watch| watch.folders == folders && watch.recursive == recursive)
        {
            return;
        }
        self.folder_watch = None;
        if folders.is_empty() {
            return;
        }
        let (sender, receiver) = async_channel::unbounded::<()>();
        let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            let Ok(event) = result else {
                return;
            };
            // Reading a file (a thumbnail, the preview, a sound under the
            // mouse) changes nothing in the listing.
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            if sender.try_send(()).is_err() {
                // The pane is gone; its watcher goes with it.
            }
        });
        let mut watcher = match watcher {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!("cannot watch {}: {error}", self.cwd);
                return;
            }
        };
        for folder in &folders {
            // A drive that sends no notifications (some network and cloud
            // drives) is refreshed by F5 as before.
            let mode = if recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            if let Err(error) = watcher.watch(Path::new(folder), mode) {
                tracing::debug!("cannot watch {folder}: {error}");
            }
        }
        let task = cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let mut last: Option<Instant> = None;
            while receiver.recv().await.is_ok() {
                cx.background_executor().timer(SETTLE).await;
                if let Some(since) = last.map(|last| last.elapsed())
                    && since < SPACING
                {
                    cx.background_executor().timer(SPACING - since).await;
                }
                while receiver.try_recv().is_ok() {}
                loop {
                    let Ok(done) = this.update(cx, |pane, cx| pane.refresh_from_disk(cx)) else {
                        return;
                    };
                    if done {
                        break;
                    }
                    cx.background_executor().timer(RETRY).await;
                }
                last = Some(Instant::now());
            }
        });
        self.folder_watch = Some(FolderWatch {
            folders,
            recursive,
            _watcher: watcher,
            _task: task,
        });
    }

    /// Reads the folder again because the disk changed. Put off (false)
    /// while it would get in the way: a name being typed, search hits on
    /// screen, a listing already under way.
    fn refresh_from_disk(&mut self, cx: &mut Context<Self>) -> bool {
        if self.renaming.is_some() || self.search_results.is_some() || self.listing_pending {
            return false;
        }
        self.reload(cx);
        cx.notify();
        true
    }
}
