// Only the real opening reports a failure; tests record the request instead.
use nohrs_core::tf;
use nohrs_core::config;
use nohrs_core::telemetry::LogErr;
use nohrs_services::fs::listing::{FileEntryDto, ListParams, list_dir_sync};

use gpui::{AppContext as _, Context, WeakEntity, Window};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::ExplorerPane;
use super::types::{PaneEvent, StatusLevel};

/// How long a navigation lets the UI thread wait for the folder. A local
/// folder answers well inside it (System32, 5,218 entries, in about 6 ms) and
/// appears in the same frame as the click, as before. A network share, a
/// cloud mount or a drive waking up can take seconds or never answer; past
/// this the read goes on in its own thread and the window keeps drawing.
#[cfg(not(test))]
pub(crate) const LISTING_WAIT: Duration = Duration::from_millis(30);
// Tests read temp folders on a busy machine and check the listing right after
// navigating, so they wait as long as it takes; `listing_wait` is set to zero
// in the test that exercises the background path.
#[cfg(test)]
pub(crate) const LISTING_WAIT: Duration = Duration::from_secs(10);

/// How long the previous folder stays on screen while a slow one is read. The
/// path in the header changes at once, which shows the click worked; the old
/// rows only give way to "Reading…" if the new ones are not there by then, so a
/// folder that takes 100 ms does not flash an empty list first.
const LISTING_HOLD: Duration = Duration::from_millis(220);

type Listing = std::result::Result<Vec<FileEntryDto>, String>;

/// What a read of the current folder brings back.
struct FolderRead {
    listing: Listing,
    /// On a refresh, the folders open in the Tree view, read again, so a row
    /// deleted inside one goes away with the rest; `None` for one that could
    /// not be read (it was deleted or moved), which closes it.
    open_folders: Vec<(String, Option<Vec<FileEntryDto>>)>,
    /// On a refresh, whether the file in the preview is gone. Asked here, off
    /// the UI thread, because it is a disk access like the listing itself.
    preview_gone: bool,
}

/// What a refresh reads besides the folder itself.
#[derive(Default)]
struct RefreshExtras {
    open_folders: Vec<String>,
    preview: Option<String>,
}

/// Where "up" leads from `path`, as in Explorer: the parent folder, and
/// This PC from a drive's root. `None` at This PC itself and at a root with
/// nothing above it (a network share).
pub(crate) fn parent_location(path: &str) -> Option<String> {
    // Inside an archive: its own folders, then the folder holding the file.
    if let Some(parent) = super::archive::parent(path) {
        return Some(parent);
    }
    if path.eq_ignore_ascii_case(super::places::THIS_PC) || super::tags::tag_of_location(path).is_some() {
        return None;
    }
    if path.eq_ignore_ascii_case(super::network::NETWORK) {
        return None;
    }
    if let Some(parent) = super::network::parent_location(path) {
        return Some(parent);
    }
    let bytes = path.trim_end_matches(['\\', '/']).as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some(super::places::THIS_PC.to_string());
    }
    std::path::Path::new(path).parent().map(|parent| parent.to_string_lossy().into_owned())
}

fn read_folder(path: &str) -> Listing {
    // An archive browsed as a folder: its entries, listed the same way.
    if super::archive::split(path).is_some() {
        return super::archive::read_folder(path);
    }
    // Not a folder on disk: the Shell says what it holds.
    if path.eq_ignore_ascii_case(super::places::THIS_PC) {
        return Ok(super::places::this_pc_entries());
    }
    // A tag's items, wherever each is.
    if let Some(id) = super::tags::tag_of_location(path) {
        return Ok(super::tags::read_items(id));
    }
    // The Network place and a server's shares are no folders either.
    if let Some(rows) = super::network::read_location(path) {
        return rows;
    }
    // A share that wants a login gets the credential window first.
    super::network::log_in_if_asked(path);
    list_dir_sync(ListParams {
        path,
        limit: config::DIR_LISTING_LIMIT,
        cursor: None,
    })
    .map(|result| result.entries)
    .map_err(|error| error.to_string())
}

/// A directory read running on a thread of its own. Its own thread rather
/// than the background executor: a read stuck on a dead share must not take
/// a pool thread with it, and nothing ever waits for an abandoned read — it
/// finishes whenever the drive answers and finds nobody listening.
struct PendingListing {
    slot: Arc<(Mutex<Option<FolderRead>>, Condvar)>,
    done: async_channel::Receiver<()>,
}

impl PendingListing {
    fn start(path: String, extras: RefreshExtras) -> Self {
        let slot: Arc<(Mutex<Option<FolderRead>>, Condvar)> = Arc::default();
        let (signal, done) = async_channel::bounded(1);
        let read = {
            let slot = slot.clone();
            move || {
                let listing = read_folder(&path);
                let open_folders = extras
                    .open_folders
                    .into_iter()
                    .map(|folder| {
                        let entries = read_folder(&folder).ok();
                        (folder, entries)
                    })
                    .collect();
                #[allow(clippy::disallowed_methods)]
                let preview_gone = extras
                    .preview
                    .is_some_and(|preview| std::fs::symlink_metadata(preview).is_err());
                let (lock, ready) = &*slot;
                match lock.lock() {
                    Ok(mut held) => {
                        *held = Some(FolderRead {
                            listing,
                            open_folders,
                            preview_gone,
                        })
                    }
                    Err(error) => tracing::error!("listing slot poisoned: {error}"),
                }
                ready.notify_all();
                signal.try_send(()).ok();
            }
        };
        if let Err(error) = std::thread::Builder::new()
            .name("nohrs-listing".into())
            .spawn(read)
        {
            // Without a thread the read cannot happen at all; the slot says
            // so, and the pane shows an error instead of waiting forever.
            match slot.0.lock() {
                Ok(mut held) => {
                    *held = Some(FolderRead {
                        listing: Err(format!("could not start reading: {error}")),
                        open_folders: Vec::new(),
                        preview_gone: false,
                    })
                }
                Err(poisoned) => tracing::error!("listing slot poisoned: {poisoned}"),
            }
        }
        Self { slot, done }
    }

    /// The read, if it is ready within `wait`.
    fn wait(&self, wait: Duration) -> Option<FolderRead> {
        let (lock, ready) = &*self.slot;
        let held = lock.lock().ok()?;
        let (mut held, _) = ready
            .wait_timeout_while(held, wait, |held| held.is_none())
            .ok()?;
        held.take()
    }

    fn take(&self) -> Option<FolderRead> {
        self.slot.0.lock().ok()?.take()
    }
}

impl ExplorerPane {
    pub(crate) fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if !self.loaded {
            self.reload(cx);
        }
    }

    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        // Every folder the pane lands in goes into the recent list, restoring
        // the last session's tabs included; a refresh of the same folder is
        // not a visit, and the list says so by leaving it where it is.
        super::recent::RecentFolders::push(&self.cwd, cx);
        self.end_hover_audio();
        // Mark as loaded regardless of outcome so an empty or unreadable
        // directory is not re-read on every subsequent render.
        self.loaded = true;
        // Reading the folder already on screen again — F5, or after a delete,
        // paste, rename or drop — is a refresh, not a visit (OTIMIZACAO.md
        // §3.9): the folders open in the Tree view stay open and are read
        // again with it, the shaped names are kept, the preview stays unless
        // its file is gone, and the old rows stay until the new ones arrive.
        // Until 04/11/2026 every one of those cleared it all, so deleting one
        // file closed every open folder in the Tree view and the text preview.
        let refresh = self.listed_cwd.as_deref() == Some(self.cwd.as_str());
        let extras = if refresh {
            RefreshExtras {
                open_folders: self.tree.expanded.iter().cloned().collect(),
                preview: self.preview_path.clone(),
            }
        } else {
            // Open folders are read again when reopened.
            self.tree.children.clear();
            self.tree.expanded.clear();
            RefreshExtras::default()
        };
        self.enter_folder_view(cx);
        // A logical clock: an answer stamped with an older generation was
        // overtaken by a later navigation and is dropped, however late it
        // arrives (OTIMIZACAO.md §5.4).
        self.listing_generation += 1;
        let generation = self.listing_generation;
        // Replacing the tasks cancels those of the previous read.
        self.listing_tasks.clear();

        let pending = PendingListing::start(self.cwd.clone(), extras);
        if let Some(read) = pending.wait(self.listing_wait) {
            self.listing_pending = false;
            self.apply_listing(read, refresh, cx);
            self.watch_listed_folders(cx);
            return;
        }

        self.listing_pending = true;
        let hold = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LISTING_HOLD).await;
            this.update(cx, |pane, cx| {
                if pane.listing_pending && pane.listing_generation == generation {
                    // The rows of the same folder are still the best answer
                    // there is; only another folder's give way to "Reading…".
                    if !refresh {
                        pane.entries.clear();
                        pane.apply_filter();
                    }
                    pane.set_status(StatusLevel::Info, format!("Reading {}…", pane.cwd));
                    cx.notify();
                }
            })
            .ok();
        });
        let arrive = cx.spawn(async move |this, cx| {
            pending.done.recv().await.ok();
            let Some(read) = pending.take() else {
                return;
            };
            this.update(cx, |pane, cx| {
                if pane.listing_generation == generation {
                    pane.listing_pending = false;
                    pane.listing_tasks.clear();
                    pane.apply_listing(read, refresh, cx);
                    pane.watch_listed_folders(cx);
                    cx.notify();
                }
            })
            .ok();
        });
        self.listing_tasks = vec![hold, arrive];
    }

    fn apply_listing(&mut self, read: FolderRead, refresh: bool, cx: &mut Context<Self>) {
        match read.listing {
            Ok(entries) => {
                if refresh {
                    // Before `apply_filter`, which splices these under their
                    // folders. A folder that could not be read is gone, and
                    // closes; cached contents of closed folders are dropped, as
                    // they may be as old as the change that prompted this.
                    self.tree.children.clear();
                    for (folder, contents) in read.open_folders {
                        match contents {
                            Some(contents) => {
                                self.tree.children.insert(folder, contents);
                            }
                            None => {
                                self.tree.expanded.remove(&folder);
                            }
                        }
                    }
                    if read.preview_gone {
                        self.clear_preview(cx);
                    }
                    // A file whose size or date changed since the last read
                    // (overwritten by a paste, saved by another program) gets
                    // its picture made again; the rest keep theirs.
                    let before: std::collections::HashMap<&str, (u64, u64)> = self
                        .entries
                        .iter()
                        .map(|entry| (entry.path.as_str(), (entry.size, entry.modified)))
                        .collect();
                    let changed: Vec<String> = entries
                        .iter()
                        .filter(|entry| {
                            before
                                .get(entry.path.as_str())
                                .is_some_and(|&was| was != (entry.size, entry.modified))
                        })
                        .map(|entry| entry.path.clone())
                        .collect();
                    for path in changed {
                        self.thumbnails.forget(&path, cx);
                    }
                } else {
                    // The names the grid shaped for the previous folder are of
                    // no use here; without this a visit to a folder of 50,000
                    // kept their layouts until the cap cleared them
                    // (OTIMIZACAO.md §6.8). A refresh keeps them: the names
                    // that are still there are most of them.
                    self.name_layouts.clear();
                    self.preview_text = None;
                    self.preview_path = None;
                    self.preview_editor = None;
                    self.preview_image_path = None;
                    self.preview_sharp = None;
                    self.preview_markdown = None;
                    self.preview_message = None;
                    self.preview_picture = None;
                    self.preview_caption = None;
                    self.preview_editing = false;
                    self.preview_dirty = false;
                    self.preview_editor_changes = None;
                    // Its file belonged to the folder left behind.
                    self.quick_look = false;
                }
                self.entries = entries;
                self.apply_filter();
                self.update_item_sizes();
                self.listed_cwd = Some(self.cwd.clone());
                self.clear_status();
                if let Some(path) = self.reveal_after_listing.take() {
                    self.reveal(&path);
                }
            }
            Err(e) => {
                tracing::error!("Failed to list directory '{}': {}", self.cwd, e);
                // The rows are not this folder's, so the next read is a visit.
                self.listed_cwd = None;
                self.reveal_after_listing = None;
                self.entries = Vec::new();
                self.filtered_entries = Vec::new();
                // The rows are gone, so the selection addressing them has to go
                // too. Without this it survives the empty listing and comes back
                // the moment a later reload puts those paths on screen again,
                // selected by nobody.
                self.prune_selection();
                self.update_item_sizes();
                self.set_status(
                    StatusLevel::Error,
                    format!("Cannot open '{}': {}", self.cwd, e),
                );
            }
        }
    }

    pub(crate) fn change_dir(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if path == self.cwd {
            return;
        }
        self.cancel_rename(window, cx);
        self.close_search(window, cx);
        self.clear_name_filter();
        // A folder opened from a search of the whole computer is where the
        // person wanted to go: the folder shows, not the search.
        self.set_everything(false, cx);
        self.tree.reset();
        self.push_history(path.clone());
        self.cwd = path;
        // What the header's recent button offers back.
        super::recent::RecentFolders::push(&self.cwd, cx);
        self.reload(cx);
        cx.emit(PaneEvent::Navigated(self.cwd.clone()));
        cx.notify();
    }

    // Records a forward navigation in the back/forward history, seeding the
    // starting directory on first use and dropping any forward entries.
    fn push_history(&mut self, path: String) {
        if self.history.is_empty() {
            self.history.push(self.cwd.clone());
            self.history_index = 0;
        }
        if self.history_index + 1 < self.history.len() {
            self.history.truncate(self.history_index + 1);
        }
        self.history.push(path);
        self.history_index += 1;
    }

    /// Mirrors a path requested by a sibling pane while `synced_panes` is on
    /// (§3.2). Unlike [`change_dir`], it does not re-emit a navigation event, so
    /// the originating pane is not driven back into an update loop.
    pub(crate) fn navigate_to_synced(&mut self, path: String, cx: &mut Context<Self>) {
        if path == self.cwd {
            return;
        }
        // The rename field is positioned by row index, so it must not outlive the
        // listing it points into — same reason `change_dir` cancels it.
        self.discard_rename(cx);
        // Clear search state so mirrored navigation doesn't leave a stale filter
        // or full-text results from the previous directory visible. This mirrors
        // the `close_search` reset on `change_dir`, minus the window-bound editor
        // sync (the subscription that drives sync has no `Window`).
        self.search_visible = false;
        self.search_results = None;
        self.search_query.clear();
        self.clear_name_filter();
        self.tree.reset();
        self.push_history(path.clone());
        self.cwd = path;
        self.reload(cx);
        cx.notify();
    }

    pub(crate) fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history_index > 0 {
            self.history_index -= 1;
            if let Some(p) = self.history.get(self.history_index).cloned() {
                self.cwd = p;
                super::recent::RecentFolders::push(&self.cwd, cx);
                self.cancel_rename(window, cx);
                self.close_search(window, cx);
                self.clear_name_filter();
                self.reload(cx);
                cx.emit(PaneEvent::Navigated(self.cwd.clone()));
                cx.notify();
            }
        }
    }

    /// Goes to the folder that holds this one (Alt+Up, the arrow beside
    /// back and forward).
    pub(crate) fn go_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent) = parent_location(&self.cwd) else {
            return;
        };
        self.change_dir(parent, window, cx);
    }

    pub(crate) fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history_index + 1 < self.history.len() {
            self.history_index += 1;
            if let Some(p) = self.history.get(self.history_index).cloned() {
                self.cwd = p;
                super::recent::RecentFolders::push(&self.cwd, cx);
                self.cancel_rename(window, cx);
                self.close_search(window, cx);
                self.clear_name_filter();
                self.reload(cx);
                cx.emit(PaneEvent::Navigated(self.cwd.clone()));
                cx.notify();
            }
        }
    }

    pub(crate) fn activate_entry(
        &mut self,
        item: FileEntryDto,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if item.kind == "dir" {
            self.change_dir(item.path, window, cx);
        } else if super::archive::is_archive_file(&item.path) {
            // An archive opens like a folder: its entries are listed here.
            self.change_dir(super::archive::location(&item.path, ""), window, cx);
        } else if item.path.starts_with(super::archive::PREFIX) {
            // A file inside an archive: unpack it to the temporary folder
            // first, then hand that over — Windows has no path inside an
            // archive to give a program.
            let address = item.path.clone();
            let unpacked = cx.background_spawn(async move { super::archive::extract(&address) });
            cx.spawn(async move |this: WeakEntity<Self>, cx| {
                let outcome = unpacked.await;
                this.update(cx, |pane, cx| match outcome {
                    Ok(path) => pane.open_with_default_app(&path.to_string_lossy()),
                    Err(error) => {
                        pane.set_status(
                            StatusLevel::Error,
                            tf!("Não foi possível abrir {path}: {error}", path = item.path, error = error),
                        );
                        cx.notify();
                    }
                })
                .log_err();
            })
            .detach();
        } else if crate::file_dialog::picking().is_some() {
            // A browser is waiting on its Open dialog: the file goes there. The
            // whole selection goes when the activated file is part of it, so
            // several files can be sent at once; folders in it are skipped.
            let selected = self.selected_paths();
            let paths: Vec<String> = if selected.contains(&item.path) {
                self.filtered_entries
                    .iter()
                    .filter(|entry| entry.kind != "dir" && selected.contains(&entry.path))
                    .map(|entry| entry.path.clone())
                    .collect()
            } else {
                vec![item.path]
            };
            crate::file_dialog::deliver(&paths);
            cx.notify();
        } else {
            self.open_with_default_app(&item.path);
        }
    }

    /// Opens a file in the program Windows associates with it, as a double
    /// click in Explorer does.
    fn open_with_default_app(&mut self, path: &str) {
        // Tests record the request instead: launching the real program would
        // open windows on the machine running them.
        #[cfg(test)]
        OPENED_WITH_DEFAULT_APP.with(|opened| opened.borrow_mut().push(path.to_owned()));
        // `explorer.exe <file>` hands the file to its associated program
        // without a shell in between to misread `&` or `^` in the name.
        #[cfg(not(test))]
        if let Err(error) = std::process::Command::new("explorer.exe").arg(path).spawn() {
            self.set_status(StatusLevel::Error, tf!("Não foi possível abrir {path}: {error}", path = path, error = error));
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Paths `open_with_default_app` was asked to open, in order.
    pub(crate) static OPENED_WITH_DEFAULT_APP: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}
