//! The folders visited last, for the header's "recent" button: the place one
//! goes back to without walking the breadcrumbs again. Kept for the whole run
//! and shared by every pane; the tab history is per pane and per tab, which is
//! a different question ("where was I in this tab"), asked by the back and
//! forward buttons.

use gpui::{App, BorrowAppContext as _, Global};

/// How many folders are kept: a menu long enough to be useful and short
/// enough to read without scrolling.
pub(crate) const MAX: usize = 20;

/// The folders visited last, newest first.
#[derive(Default)]
pub(crate) struct RecentFolders {
    folders: Vec<String>,
}

impl Global for RecentFolders {}

impl RecentFolders {
    /// The folders, newest first.
    pub(crate) fn all(cx: &App) -> Vec<String> {
        cx.try_global::<Self>()
            .map(|recent| recent.folders.clone())
            .unwrap_or_default()
    }

    /// Records `path` as just visited, moving it to the front if it was
    /// already there. Only folders on disk are kept: an archive's own address
    /// or a tag's is not a place the button goes back to.
    pub(crate) fn push(path: &str, cx: &mut App) {
        if path.is_empty() || !std::path::Path::new(path).is_dir() {
            return;
        }
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        let changed = cx.update_global(|recent: &mut Self, _| {
            let before = recent.folders.clone();
            recent.folders.retain(|kept| !super::same_path(kept, path));
            recent.folders.insert(0, path.to_string());
            recent.folders.truncate(MAX);
            recent.folders != before
        });
        if changed {
            cx.refresh_windows();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn folders_are_kept_newest_first_once_each_and_capped(cx: &mut gpui::TestAppContext) {
        let folder = tempfile::tempdir().expect("temp folder");
        let mut folders = Vec::new();
        for index in 0..(MAX + 5) {
            let path = folder.path().join(format!("pasta{index}"));
            std::fs::create_dir(&path).expect("folder");
            folders.push(path.to_string_lossy().into_owned());
        }
        let file = folder.path().join("nota.txt");
        std::fs::write(&file, "x").expect("write");
        cx.update(|cx| {
            for path in &folders {
                RecentFolders::push(path, cx);
            }
            let kept = RecentFolders::all(cx);
            assert_eq!(kept.len(), MAX, "only the newest are kept");
            assert_eq!(kept[0], folders[folders.len() - 1], "newest first");
            assert!(!kept.contains(&folders[0]), "the oldest fell off");

            // Visiting one again moves it to the front instead of repeating it.
            RecentFolders::push(&folders[2], cx);
            let kept = RecentFolders::all(cx);
            assert_eq!(kept[0], folders[2]);
            assert_eq!(kept.iter().filter(|path| **path == folders[2]).count(), 1);

            // The same folder under another spelling is one entry.
            let before = RecentFolders::all(cx).len();
            RecentFolders::push(&format!("{}\\", folders[2].to_uppercase()), cx);
            assert_eq!(RecentFolders::all(cx).len(), before);

            // A file, or an address that is not a folder on disk, is not kept.
            RecentFolders::push(&file.to_string_lossy(), cx);
            RecentFolders::push("arc:C:\\a.zip|docs", cx);
            assert_eq!(RecentFolders::all(cx).len(), before);
        });
    }
}
