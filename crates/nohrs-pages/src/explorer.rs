mod archive;
mod archive_ops;
mod batch_rename;
pub(crate) mod commands;
pub(crate) mod context_menu;
mod creative;
mod documents;
mod drag;
pub(crate) mod drives;
mod entries;
pub(crate) mod everything;
pub(crate) mod favorites;
#[cfg(windows)]
#[allow(unsafe_code)]
mod file_clipboard;
pub(crate) mod file_ops;
mod filter;
mod folder_views;
pub(crate) mod folder_watch;
mod grouping;
pub(crate) mod hover_audio;
mod list_setup;
mod marquee;
/// The split-view container that owns one or more panes (`docs/explorer-essentials.md` §3).
mod media;
mod model3d;
mod navigation;
pub(crate) mod network;
mod page;
mod path_edit;
mod pdf;
mod picture;
mod pinyin;
mod places;
mod preview;
pub(crate) mod preview_listing;
mod quick_search;
mod recent;
mod search;
mod smooth_scroll;
pub(crate) mod space_map;
mod state;
/// Git/SVN and cloud-sync state shown beside the file names.
pub(crate) mod status;
pub(crate) mod tags;
pub(crate) mod thumbnails;
mod tree;
mod type_style;
mod types;
/// Rendering of a single explorer pane: header, sidebar, listing, and preview.
pub mod view;
pub(crate) mod waveforms;
mod workspace_icons;
mod zoom;

#[cfg(test)]
mod tests;

pub(crate) use context_menu::note_theme_mode;
pub use page::{ExplorerPage, rebind_shortcuts};
pub use state::ExplorerPane;
// Part of `status_for_footer`'s signature, so it has to travel with it. The rest
// of `types` stays internal.
pub use types::StatusLevel;
/// The footer zoom menu's views.
pub use zoom::PRESETS as ZOOM_PRESETS;
/// The name the footer shows for a listing zoom value.
pub use zoom::label as zoom_label;

/// One result returned by the resident filename index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedSearchHit {
    /// Full path of the matching file or folder.
    pub path: String,
    /// Whether the match is a folder.
    pub is_dir: bool,
    /// File size in bytes, or the indexed aggregate size for a folder.
    pub size: u64,
    /// Modification time as Unix seconds.
    pub modified: u32,
}

/// Search the resident index used by Explorer's whole-computer search.
///
/// The service returns at most `limit` hits. `truncated` is conservative: a
/// result set exactly at the limit may have more matches beyond it.
pub fn search_indexed(query: &str, limit: u32) -> Result<(Vec<IndexedSearchHit>, bool), String> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Ok((Vec::new(), false));
    }
    // Do not gate on `searchable()`: it caches a negative result for the whole
    // process, so an app started before the service becomes ready would keep
    // reporting it unavailable even after the service starts. The query itself
    // is the authoritative availability check.
    let hits = everything::mft::search(&terms, limit)
        .map_err(|error| format!("轻寻文件 索引服务暂不可用：{error}"))?;
    let truncated = hits.len() >= limit as usize;
    Ok((
        hits.into_iter()
            .map(|hit| IndexedSearchHit {
                path: hit.path,
                is_dir: hit.dir,
                size: hit.size,
                modified: hit.modified,
            })
            .collect(),
        truncated,
    ))
}

fn query_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

#[cfg(test)]
mod indexed_search_tests {
    use super::query_terms;

    #[test]
    fn query_terms_ignore_extra_spaces_and_fold_case() {
        assert_eq!(query_terms("  Project   README "), ["project", "readme"]);
    }

    #[test]
    fn whitespace_only_query_has_no_terms() {
        assert!(query_terms(" \t\n ").is_empty());
    }
}

use gpui::{Context, IntoElement, Render, Window};

/// Whether two paths name the same place. Windows paths are compared without
/// case and with either separator, since a path handed over by another program
/// rarely matches the listing's spelling byte for byte.
pub(crate) fn same_path(left: &str, right: &str) -> bool {
    let normalize = |path: &str| {
        path.trim_end_matches(['\\', '/'])
            .replace('/', "\\")
            .to_lowercase()
    };
    normalize(left) == normalize(right)
}

impl Render for ExplorerPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        view::render(self, window, cx)
    }
}
