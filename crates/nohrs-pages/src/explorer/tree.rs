//! The Tree view: the details table with folders that open in place, as File
//! Pilot's. An open folder's entries are spliced into `filtered_entries` right
//! under it, one level deeper, so selection, rename, the context menu and file
//! operations — all addressed by row or by path — work on them unchanged.

use std::collections::{HashMap, HashSet};

use gpui::{AppContext as _, Context, WeakEntity};
use nohrs_core::config;
use nohrs_core::telemetry::LogErr;
use nohrs_services::fs::listing::{FileEntryDto, ListParams, list_dir_sync};

use super::ExplorerPane;
use super::types::ViewMode;

/// Which folders are open in the Tree view and what was read from them.
#[derive(Default)]
pub struct TreeState {
    /// Open folders, by path.
    pub expanded: HashSet<String>,
    /// Entries read from open folders, by folder path, unsorted.
    pub children: HashMap<String, Vec<FileEntryDto>>,
    /// How deep each spliced-in entry sits (top-level entries are absent).
    pub depth: HashMap<String, usize>,
}

impl TreeState {
    /// Forgets everything: a new folder starts with every node closed.
    pub fn reset(&mut self) {
        self.expanded.clear();
        self.children.clear();
        self.depth.clear();
    }
}

impl ExplorerPane {
    /// The indent level of the entry at `path` in the Tree view.
    pub(crate) fn tree_depth(&self, path: &str) -> usize {
        self.tree.depth.get(path).copied().unwrap_or(0)
    }

    /// Whether `path` is an open folder in the Tree view.
    pub(crate) fn tree_open(&self, path: &str) -> bool {
        self.tree.expanded.contains(path)
    }

    /// Opens or closes the folder at `path` in place, reading it the first
    /// time on a background thread.
    pub(crate) fn toggle_tree_folder(&mut self, path: String, cx: &mut Context<Self>) {
        if self.tree.expanded.remove(&path) {
            self.watch_listed_folders(cx);
            self.apply_filter();
            cx.notify();
            return;
        }
        self.tree.expanded.insert(path.clone());
        // An open folder shows its files, so it follows the disk too.
        self.watch_listed_folders(cx);
        if self.tree.children.contains_key(&path) {
            self.apply_filter();
            cx.notify();
            return;
        }
        let read = cx.background_spawn({
            let path = path.clone();
            async move {
                list_dir_sync(ListParams {
                    path: &path,
                    limit: config::DIR_LISTING_LIMIT,
                    cursor: None,
                })
                .map(|listing| listing.entries)
                .unwrap_or_default()
            }
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let entries = read.await;
            this.update(cx, |pane, cx| {
                pane.tree.children.insert(path, entries);
                pane.apply_filter();
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// Splices the entries of open folders under them, recursively, with the
    /// same sort and hidden-file rule as the top level. Called at the end of
    /// `apply_filter` in the Tree view.
    pub(crate) fn splice_tree(&mut self) {
        self.tree.depth.clear();
        if self.view_mode != ViewMode::Tree || self.tree.expanded.is_empty() {
            return;
        }
        let top = std::mem::take(&mut self.filtered_entries);
        let mut out = Vec::with_capacity(top.len());
        for entry in top {
            self.push_with_children(entry, 0, &mut out);
        }
        self.filtered_entries = out;
    }

    fn push_with_children(
        &mut self,
        entry: FileEntryDto,
        depth: usize,
        out: &mut Vec<FileEntryDto>,
    ) {
        let open = entry.kind == "dir" && self.tree.expanded.contains(&entry.path);
        let path = entry.path.clone();
        if depth > 0 {
            self.tree.depth.insert(path.clone(), depth);
        }
        out.push(entry);
        if !open {
            return;
        }
        let Some(children) = self.tree.children.get(&path) else {
            return;
        };
        let show_hidden = self.show_hidden;
        let mut children: Vec<FileEntryDto> = children
            .iter()
            .filter(|child| show_hidden || !super::state::is_hidden(&child.name))
            .cloned()
            .collect();
        super::entries::sort_entries_with(
            &mut children,
            self.sort_key,
            self.sort_asc,
            self.folder_order,
        );
        for child in children {
            self.push_with_children(child, depth + 1, out);
        }
    }
}
