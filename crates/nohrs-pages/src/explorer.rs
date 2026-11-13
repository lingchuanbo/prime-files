pub(crate) mod commands;
pub(crate) mod context_menu;
mod archive;
mod creative;
mod documents;
mod drag;
pub(crate) mod drives;
mod entries;
pub(crate) mod everything;
pub(crate) mod favorites;
pub(crate) mod folder_watch;
pub(crate) mod file_ops;
mod filter;
mod folder_views;
mod grouping;
pub(crate) mod hover_audio;
mod list_setup;
mod marquee;
mod navigation;
pub(crate) mod network;
/// The split-view container that owns one or more panes (`docs/explorer-essentials.md` §3).
mod media;
mod model3d;
mod smooth_scroll;
mod page;
mod pdf;
mod picture;
mod pinyin;
mod path_edit;
mod places;
mod preview;
pub(crate) mod preview_listing;
mod quick_search;
mod recent;
mod search;
mod state;
pub(crate) mod thumbnails;
pub(crate) mod waveforms;
pub(crate) mod tags;
mod workspace_icons;
mod tree;
mod types;
/// Rendering of a single explorer pane: header, sidebar, listing, and preview.
pub mod view;
mod zoom;

#[cfg(test)]
mod tests;

pub(crate) use context_menu::note_theme_mode;
pub use page::{ExplorerPage, rebind_shortcuts};
pub use state::ExplorerPane;
// Part of `status_for_footer`'s signature, so it has to travel with it. The rest
// of `types` stays internal.
pub use types::StatusLevel;
/// The name the footer shows for a listing zoom value.
pub use zoom::label as zoom_label;
/// The footer zoom menu's views.
pub use zoom::PRESETS as ZOOM_PRESETS;

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
