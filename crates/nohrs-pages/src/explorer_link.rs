//! The in-process link from another window to the Explorer.
//!
//! The standalone search window is a window of its own, with no handle on the
//! `RootView` that owns the Explorer. Opening a folder "in the app" therefore
//! goes through a channel the Explorer publishes at startup, the way the IPC
//! server ([`crate::ipc`]) routes requests from other programs — except that
//! this one never leaves the process and never blocks the UI thread.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use gpui::{AsyncWindowContext, Context, WeakEntity, Window};

use crate::root::RootView;

static SENDER: OnceLock<async_channel::Sender<PathBuf>> = OnceLock::new();

/// Asks the running Explorer to show `path`: a folder is opened, a file is
/// revealed and selected inside its folder. Returns `false` when there is no
/// Explorer to ask (a launcher-only run, or the IPC of a previous instance
/// still on disk), so the caller can fall back to the system handler.
pub(crate) fn request(path: PathBuf) -> bool {
    SENDER
        .get()
        .is_some_and(|sender| sender.try_send(path).is_ok())
}

/// Starts answering [`request`]s for the lifetime of the Explorer window.
pub(crate) fn install(window: &mut Window, cx: &mut Context<RootView>) {
    let (sender, receiver) = async_channel::unbounded::<PathBuf>();
    if SENDER.set(sender).is_err() {
        return;
    }
    cx.spawn_in(
        window,
        async move |this: WeakEntity<RootView>, cx: &mut AsyncWindowContext| {
            while let Ok(path) = receiver.recv().await {
                let shown = this.update_in(cx, |root, window, cx| {
                    root.show_path_from_elsewhere(&path, window, cx);
                });
                if shown.is_err() {
                    // The Explorer window is gone; nothing left to show in.
                    return;
                }
            }
        },
    )
    .detach();
}

impl RootView {
    fn show_path_from_elsewhere(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some((folder, select)) = crate::ipc::folder_and_selection(path) else {
            return;
        };
        self.show_explorer(cx);
        self.explorer_entity()
            .update(cx, |page, cx| page.open_path(folder, select, window, cx));
        window.activate_window();
        crate::foreground::bring_to_front(window);
    }
}
