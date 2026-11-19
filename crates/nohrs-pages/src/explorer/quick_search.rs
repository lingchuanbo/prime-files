//! The bar under the listing, as File Pilot's: one field that filters the
//! folder on screen as you type, or — in its subfolder mode (Ctrl+F) —
//! searches the folder and everything under it by name, with results arriving
//! while the walk is still going. No index: it reads the disk, so it finds
//! what is there now, wherever "here" is.

use std::time::{Duration, Instant};

use gpui::{Context, Task, WeakEntity};
use nohrs_core::telemetry::LogErr;
use nohrs_services::fs::listing::FileEntryDto;

use super::ExplorerPane;
use super::types::SearchFileResult;

// Enough for any name search a person reads through; a pattern that matches
// more ("*") stops here instead of filling memory.
const MAX_RESULTS: usize = 20_000;
// Results are handed to the UI in batches, so a fast walk does not re-render
// for every hit and a slow one still shows its first hits at once.
const BATCH: usize = 256;
const FLUSH_EVERY: Duration = Duration::from_millis(80);

/// A running subfolder search.
pub struct DeepSearch {
    pub query: String,
    pub found: usize,
    pub running: bool,
    /// After the count: what the whole-computer search understood and how
    /// old its index is. Empty for other searches.
    pub detail: String,
    // Dropping it drops the receiver, which stops the walk at its next send.
    _task: Task<()>,
}

impl DeepSearch {
    /// A search for `query` whose hits `task` is delivering.
    pub(crate) fn new(query: String, task: Task<()>) -> Self {
        Self {
            query,
            found: 0,
            running: true,
            detail: String::new(),
            _task: task,
        }
    }
}

impl ExplorerPane {
    /// What names are highlighted by: the subfolder search's query or the
    /// filter's, reduced to its literal part when it is a wildcard (`*.png`
    /// highlights `.png`). Empty when nothing is being looked for.
    pub(crate) fn highlight_query(&self) -> String {
        let query = match &self.deep_search {
            Some(search) if self.everything_on() => {
                return super::everything::highlight_term(&search.query);
            }
            Some(search) => search.query.as_str(),
            None if !self.name_filter.trim().is_empty() => self.name_filter.as_str(),
            None => self.search_query.as_str(),
        };
        query
            .split(['*', '?', ' ', '\\'])
            .max_by_key(|part| part.len())
            .unwrap_or("")
            .trim()
            .to_string()
    }

    /// The folder a search hit is in, relative to where the search started.
    pub(crate) fn hit_folder(&self, path: &str) -> Option<&str> {
        self.hit_folders.get(path).map(String::as_str)
    }

    /// Starts (or restarts) the subfolder search for `query` from the folder
    /// on screen. An empty query ends it and brings the folder back.
    pub(crate) fn search_subfolders(&mut self, query: String, cx: &mut Context<Self>) {
        if self
            .deep_search
            .as_ref()
            .is_some_and(|search| search.query == query)
        {
            return;
        }
        if query.trim().is_empty() {
            self.end_subfolder_search();
            cx.notify();
            return;
        }
        let (sender, receiver) = async_channel::unbounded::<Vec<FileEntryDto>>();
        let root = self.cwd.clone();
        let pattern = query.clone();
        let spawned = std::thread::Builder::new()
            .name("nohrs-subfolder-search".into())
            .spawn(move || walk(&root, &pattern, &sender));
        if let Err(error) = spawned {
            tracing::error!("could not start the subfolder search: {error}");
            return;
        }
        self.clear_selection();
        self.hit_folders.clear();
        self.filtered_entries.clear();
        self.search_results = Some(Vec::new());
        self.update_item_sizes();
        let task = cx.spawn(async move |this: WeakEntity<Self>, cx| {
            while let Ok(batch) = receiver.recv().await {
                let applied = this.update(cx, |pane, cx| {
                    pane.add_search_hits(batch);
                    cx.notify();
                });
                if applied.is_err() {
                    return;
                }
            }
            this.update(cx, |pane, cx| {
                if let Some(search) = pane.deep_search.as_mut() {
                    search.running = false;
                }
                cx.notify();
            })
            .log_err();
        });
        self.deep_search = Some(DeepSearch {
            query,
            found: 0,
            running: true,
            detail: String::new(),
            _task: task,
        });
        cx.notify();
    }

    /// Empties the hits for a search whose first ones are arriving.
    pub(crate) fn begin_search_hits(&mut self) {
        self.clear_selection();
        self.hit_folders.clear();
        self.filtered_entries.clear();
        self.search_results = Some(Vec::new());
        if let Some(search) = self.deep_search.as_mut() {
            search.found = 0;
        }
        self.update_item_sizes();
    }

    pub(crate) fn add_search_hits(&mut self, batch: Vec<FileEntryDto>) {
        let results = self.search_results.get_or_insert_with(Vec::new);
        results.extend(batch.iter().map(|entry| SearchFileResult {
            path: entry.path.clone(),
            folder: String::new(),
            filename: entry.name.clone(),
            matches: Vec::new(),
        }));
        // Shown beside each hit as "name › folder", as File Pilot does: from
        // where the search started, or the whole folder for a search of the
        // whole computer.
        let root = (!self.everything_on()).then(|| std::path::Path::new(&self.cwd));
        for entry in &batch {
            let folder = std::path::Path::new(&entry.path)
                .parent()
                .and_then(|parent| match root {
                    Some(root) => parent.strip_prefix(root).ok(),
                    None => Some(parent),
                })
                .map(|relative| relative.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !folder.is_empty() {
                self.hit_folders.insert(entry.path.clone(), folder);
            }
        }
        if let Some(search) = self.deep_search.as_mut() {
            search.found += batch.len();
        }
        self.filtered_entries.extend(batch);
        self.update_item_sizes();
    }

    /// Stops the subfolder search, if any, and shows the folder again.
    pub(crate) fn end_subfolder_search(&mut self) {
        self.hit_folders.clear();
        if self.deep_search.take().is_some() {
            self.search_results = None;
            self.apply_filter();
        }
    }
}

// Walks `root` depth-first, sending every entry whose name matches `pattern`
// (as the filter reads it: part of the name, or a wildcard).
fn walk(root: &str, pattern: &str, sender: &async_channel::Sender<Vec<FileEntryDto>>) {
    let root_path = std::path::Path::new(root);
    let matcher = super::filter::NameMatcher::new(pattern);
    let mut pending = vec![root_path.to_path_buf()];
    let mut batch = Vec::new();
    let mut sent = 0usize;
    let mut last_flush = Instant::now();
    while let Some(folder) = pending.pop() {
        let Ok(read) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in read.filter_map(Result::ok) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            // Links and junctions are listed but not followed: a junction to
            // a parent folder would otherwise walk forever.
            if kind.is_dir() && !kind.is_symlink() {
                pending.push(entry.path());
            }
            if !matcher.matches(&name) {
                continue;
            }
            let path = entry.path();
            let metadata = entry.metadata().ok();
            batch.push(FileEntryDto {
                name,
                path: path.to_string_lossy().into_owned(),
                kind: if kind.is_dir() { "dir" } else { "file" }.to_string(),
                size: metadata.as_ref().map_or(0, |metadata| metadata.len()),
                modified: metadata
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_secs()),
            });
            if batch.len() >= BATCH || last_flush.elapsed() >= FLUSH_EVERY {
                sent += batch.len();
                if sender.send_blocking(std::mem::take(&mut batch)).is_err() {
                    return;
                }
                last_flush = Instant::now();
                if sent >= MAX_RESULTS {
                    return;
                }
            }
        }
        // A folder with no hits still lets a pending batch out on time.
        if !batch.is_empty() && last_flush.elapsed() >= FLUSH_EVERY {
            sent += batch.len();
            if sender.send_blocking(std::mem::take(&mut batch)).is_err() {
                return;
            }
            last_flush = Instant::now();
        }
    }
    if !batch.is_empty() {
        sender.send_blocking(batch).log_err();
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn the_walk_finds_names_in_subfolders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a").join("b")).unwrap();
        std::fs::write(dir.path().join("a").join("b").join("photo.png"), "x").unwrap();
        std::fs::write(dir.path().join("a").join("notes.txt"), "x").unwrap();
        let (sender, receiver) = async_channel::unbounded();
        walk(&dir.path().to_string_lossy(), "*.png", &sender);
        drop(sender);
        let mut hits: Vec<FileEntryDto> = Vec::new();
        while let Ok(batch) = receiver.try_recv() {
            hits.extend(batch);
        }
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "photo.png");
        assert!(hits[0].path.ends_with("photo.png"));
    }
}
