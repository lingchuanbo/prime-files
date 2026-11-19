//! Each folder's own zoom and sort, remembered the way Explorer remembers a
//! folder's view: change them in a folder and that folder opens that way
//! again, in any tab and after a restart. A folder never changed opens with
//! the configured sort (`ui.default_sort`) and the zoom the tab already had.
//!
//! One map for the whole app, held as a GPUI global, read when a tab enters a
//! folder and written, debounced, to the same redb store as the tabs.

use gpui::{App, AppContext as _, BorrowAppContext, Global, Task};
use nohrs_core::telemetry::LogErr;
use nohrs_store::{KvKey, KvStore, kv_key};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::types::SortKey;

const FOLDER_VIEWS_KEY: KvKey = kv_key!("explorer.folder_views");

/// Folders remembered before the one used longest ago is forgotten.
/// Explorer keeps 5,000 by default (`BagMRU Size`); each entry here is about
/// a hundred bytes of JSON, so the whole map stays under half a megabyte.
const KEPT: usize = 5_000;

/// Quiet time before the map is written: a zoom gliding with the wheel
/// changes it on every notch, and that is one write, not dozens.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(1);

/// What a folder looks like when it is opened.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct FolderView {
    /// The listing zoom, 0..=1 (`zoom.rs`).
    pub zoom: f32,
    pub sort: SavedSort,
    pub ascending: bool,
}

/// [`SortKey`] as stored, so renaming the enum's variants cannot silently
/// forget everyone's folders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SavedSort {
    Name,
    Size,
    Modified,
    Type,
}

impl From<SortKey> for SavedSort {
    fn from(key: SortKey) -> Self {
        match key {
            SortKey::Name => Self::Name,
            SortKey::Size => Self::Size,
            SortKey::Modified => Self::Modified,
            SortKey::Type => Self::Type,
        }
    }
}

impl From<SavedSort> for SortKey {
    fn from(sort: SavedSort) -> Self {
        match sort {
            SavedSort::Name => Self::Name,
            SavedSort::Size => Self::Size,
            SavedSort::Modified => Self::Modified,
            SavedSort::Type => Self::Type,
        }
    }
}

/// The remembered folders, by [`folder_key`], each with the tick it was last
/// used at so the oldest can go once there are [`KEPT`].
#[derive(Default)]
pub(crate) struct FolderViews {
    views: HashMap<String, (FolderView, u64)>,
    clock: u64,
    store: Option<Arc<dyn KvStore>>,
    save: Option<Task<()>>,
}

impl Global for FolderViews {}

/// The stored form: oldest first, so the order of use survives a restart.
#[derive(Serialize, Deserialize)]
struct Stored {
    views: Vec<(String, FolderView)>,
}

impl FolderViews {
    /// Loads the remembered folders from `store` and makes them the app's.
    /// Without this nothing is remembered, which is what tests that build a
    /// bare pane get.
    pub(crate) fn install(store: Option<Arc<dyn KvStore>>, cx: &mut App) {
        if cx.has_global::<Self>() {
            return;
        }
        let mut views = Self {
            store,
            ..Default::default()
        };
        let stored = views
            .store
            .as_ref()
            .and_then(|store| store.get(&FOLDER_VIEWS_KEY).log_err().flatten())
            .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).log_err());
        for (folder, view) in stored.map(|stored| stored.views).unwrap_or_default() {
            views.insert(folder, view);
        }
        cx.set_global(views);
    }

    /// How `folder` was left, if it was ever changed.
    pub(crate) fn get(folder: &str, cx: &mut App) -> Option<FolderView> {
        if !cx.has_global::<Self>() {
            return None;
        }
        cx.update_global(|views: &mut Self, _cx| {
            views.clock += 1;
            let clock = views.clock;
            let (view, used) = views.views.get_mut(&folder_key(folder))?;
            *used = clock;
            Some(*view)
        })
    }

    /// Remembers `view` for `folder` and saves the map shortly after.
    pub(crate) fn remember(folder: &str, view: FolderView, cx: &mut App) {
        if !cx.has_global::<Self>() {
            return;
        }
        cx.update_global(|views: &mut Self, cx| {
            views.insert(folder_key(folder), view);
            if views.store.is_some() {
                views.save = Some(cx.spawn(async move |cx| {
                    cx.background_executor().timer(SAVE_DEBOUNCE).await;
                    let prepared = cx
                        .update(|cx| {
                            let views = cx.global::<Self>();
                            let bytes = serde_json::to_vec(&views.stored()).log_err()?;
                            Some((views.store.clone()?, bytes))
                        })
                        .ok()
                        .flatten();
                    let Some((store, bytes)) = prepared else {
                        return;
                    };
                    cx.background_spawn(async move {
                        store.put(&FOLDER_VIEWS_KEY, &bytes).log_err();
                    })
                    .await;
                }));
            }
        });
    }

    fn insert(&mut self, key: String, view: FolderView) {
        self.clock += 1;
        self.views.insert(key, (view, self.clock));
        if self.views.len() > KEPT
            && let Some(oldest) = self
                .views
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone())
        {
            self.views.remove(&oldest);
        }
    }

    fn stored(&self) -> Stored {
        let mut views: Vec<(&String, &(FolderView, u64))> = self.views.iter().collect();
        views.sort_by_key(|(_, (_, used))| *used);
        Stored {
            views: views
                .into_iter()
                .map(|(key, (view, _))| (key.clone(), *view))
                .collect(),
        }
    }
}

/// The same folder however it was typed: Windows paths ignore case, and
/// `C:\Fotos\` is `C:\Fotos`. A drive root keeps its separator.
pub(crate) fn folder_key(folder: &str) -> String {
    let unified = folder.replace('/', "\\").to_lowercase();
    let trimmed = unified.trim_end_matches('\\');
    if trimmed.ends_with(':') || trimmed.is_empty() {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(zoom: f32) -> FolderView {
        FolderView {
            zoom,
            sort: SavedSort::Modified,
            ascending: false,
        }
    }

    #[test]
    fn folder_key_ignores_case_slashes_and_the_last_separator() {
        assert_eq!(folder_key(r"C:\Fotos\"), folder_key("c:/fotos"));
        assert_eq!(folder_key(r"C:\"), r"c:\");
        assert_eq!(folder_key("C:"), r"c:\");
        assert_ne!(folder_key(r"C:\Fotos"), folder_key(r"C:\Fotos 2"));
    }

    #[gpui::test]
    fn a_folder_comes_back_as_it_was_left_and_the_oldest_goes_past_the_cap(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            FolderViews::install(None, cx);
            assert_eq!(FolderViews::get(r"C:\Fotos", cx), None);
            FolderViews::remember(r"C:\Fotos", view(0.5), cx);
            assert_eq!(FolderViews::get(r"c:\fotos\", cx), Some(view(0.5)));

            for index in 0..KEPT {
                FolderViews::remember(&format!(r"C:\pasta {index}"), view(0.1), cx);
                if index == 10 {
                    // Used recently, so it outlives folders remembered after it.
                    FolderViews::get(r"C:\Fotos", cx);
                }
            }
            let views = cx.global::<FolderViews>();
            assert_eq!(views.views.len(), KEPT);
            assert!(views.views.contains_key(&folder_key(r"C:\Fotos")));
            assert!(!views.views.contains_key(&folder_key(r"C:\pasta 0")));
        });
    }

    #[test]
    fn the_stored_form_round_trips_oldest_first() {
        let mut views = FolderViews::default();
        views.insert("b".into(), view(0.2));
        views.insert("a".into(), view(0.7));
        let bytes = serde_json::to_vec(&views.stored()).unwrap();
        let stored: Stored = serde_json::from_slice(&bytes).unwrap();
        let keys: Vec<&str> = stored.views.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["b", "a"]);
        assert_eq!(stored.views[1].1, view(0.7));
    }
}
