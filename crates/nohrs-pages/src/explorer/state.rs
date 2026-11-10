use nohrs_core::config;
use nohrs_services::fs::listing::FileEntryDto;
use nohrs_services::fs::trash::Ledger as TrashLedger;
use nohrs_services::search::{SearchScope, SearchService};
use nohrs_services::syntax::SyntaxService;
use nohrs_ui::components::file_list::FileListDelegate;

use gpui::{AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Window, px, size};
use gpui_component::VirtualListScrollHandle;
use gpui_component::input::InputState;
use gpui_component::list::ListState;
use gpui_component::resizable::ResizableState;
use std::{rc::Rc, sync::Arc, time::Instant};

use super::entries;
use super::types::*;
use super::view::preview::editor::PreviewEditor;

/// State for the file explorer page: the current directory listing, navigation
/// history, sorting and filtering, search, preview, and view layout.
pub struct ExplorerPane {
    /// Absolute path of the currently displayed directory.
    pub cwd: String,
    /// Navigation history of visited directories for back/forward.
    pub history: Vec<String>,
    /// Index of the current position within `history`.
    pub history_index: usize,
    /// All entries read from `cwd`, before filtering.
    pub entries: Vec<FileEntryDto>,
    /// Entries currently shown after applying hidden/search filters and sorting.
    pub filtered_entries: Vec<FileEntryDto>,
    // Whether the current directory has been loaded. Tracked explicitly rather
    // than via `entries.is_empty()` so an empty (or failed-to-read) directory is
    // not reloaded on every render.
    /// Whether `cwd` has been loaded into `entries` at least once.
    pub loaded: bool,
    /// Bumped by every read of `cwd`; a read answering for an older one was
    /// overtaken and is dropped.
    pub(crate) listing_generation: u64,
    /// Whether a read of `cwd` is still on its way (the folder is slow).
    pub(crate) listing_pending: bool,
    /// The tasks waiting on that read; replacing them cancels them.
    pub(crate) listing_tasks: Vec<gpui::Task<()>>,
    /// How long a read may hold the UI thread before it goes on in the
    /// background (`navigation::LISTING_WAIT`).
    pub(crate) listing_wait: std::time::Duration,
    /// A path to select once the pending read arrives.
    pub(crate) reveal_after_listing: Option<String>,
    /// The folder the rows on screen were read from. A read of the same folder
    /// is a refresh and keeps what the user had open (`navigation::reload`).
    pub(crate) listed_cwd: Option<String>,
    /// The sort a folder opens with when it was never changed
    /// (`ui.default_sort`, `ui.default_sort_descending`).
    pub(crate) default_sort_key: SortKey,
    pub(crate) default_sort_asc: bool,
    /// The folder whose remembered view is on screen (`folder_views::folder_key`):
    /// entering another one applies its view, refreshing this one does not.
    pub(crate) view_folder: Option<String>,
    /// Column the listing is sorted by.
    pub sort_key: SortKey,
    /// Whether the sort is ascending.
    pub sort_asc: bool,
    /// Where folders go in the sort (the bottom bar's folder-order button).
    pub(crate) folder_order: super::entries::FolderOrder,
    // The `[ui]` section as last applied. A reload only changes what changed
    // in the file: writing the theme from the menu reloads the config, and
    // that must not undo a sort picked from the column headers or the menu.
    applied_ui: Option<config::Ui>,
    /// What the listing is grouped by (the context menu's Group by).
    pub(crate) group_by: super::grouping::GroupBy,
    /// The runs of `filtered_entries` under each group header; empty when
    /// not grouped. See [`Self::groups`].
    group_runs: Vec<super::grouping::Group>,
    // Whether dotfiles are listed; driven by `ui.show_hidden` (config.md §5).
    /// Whether dotfiles are listed; driven by `ui.show_hidden`.
    pub show_hidden: bool,
    // Whether the sidebar starts with the Windows Quick access section;
    // driven by `ui.show_quick_access`.
    /// Whether the sidebar starts with the Windows Quick access section;
    /// driven by `ui.show_quick_access`.
    pub show_quick_access: bool,
    // Identifier of the active icon pack; driven by `ui.icon_pack`.
    /// Identifier of the active icon pack; driven by `ui.icon_pack`.
    pub icon_pack: String,
    /// Most lines of a name under an icon, 0 = all; driven by `ui.name_lines`.
    pub(crate) name_lines: u32,
    /// How long a wheel glide takes, 0 = none; driven by `ui.scroll_smoothness`.
    pub(crate) scroll_smoothness: u32,
    /// Whole size of the listing on screen, for its scrollbar.
    pub(crate) listing_extent: gpui::Size<gpui::Pixels>,
    /// The wheel glide under way in the listing.
    pub(crate) smooth_scroll: Option<super::smooth_scroll::SmoothScroll>,
    /// Current name-filter query for the in-directory listing.
    pub search_query: String,
    /// Whether the search bar is visible.
    pub search_visible: bool,
    /// Input state backing the search field.
    pub search_input: Entity<InputState>,
    /// State of the resizable split between listing and preview.
    pub resizable: Entity<ResizableState>,
    /// The listing's virtualized list entity, once initialized.
    pub list: Option<Entity<ListState<FileListDelegate>>>,
    /// GPUI subscriptions kept alive for the lifetime of the page.
    pub subs: Vec<gpui::Subscription>,
    /// Path of the file currently shown in the preview pane.
    pub preview_path: Option<String>,
    /// Text content of the previewed file, when it is textual.
    pub preview_text: Option<String>,
    /// Paths of all currently selected entries.
    ///
    /// Held as paths rather than row indices: an index-addressed selection stops
    /// meaning anything the moment the visible rows are rebuilt, so sorting or
    /// filtering had to clear it to avoid acting on whichever entries had moved
    /// into those slots. Rows are matched back to it when the listing renders.
    pub selection: std::collections::HashSet<String>,
    /// Path a Shift range selection extends from, or `None` when nothing is
    /// anchored.
    pub selection_anchor: Option<String>,
    /// The active/primary entry that drives the preview and keyboard navigation.
    pub active_path: Option<String>,
    /// Scroll handle for the virtualized listing.
    pub virtual_scroll_handle: VirtualListScrollHandle,
    /// Per-row sizes for the virtualized listing.
    pub item_sizes: Rc<Vec<gpui::Size<gpui::Pixels>>>,
    // Column widths (resizable)
    /// Width of the name column.
    pub col_name_width: f32,
    /// Width of the type column.
    pub col_type_width: f32,
    /// Width of the size column.
    pub col_size_width: f32,
    /// Width of the modified-time column.
    pub col_modified_width: f32,
    /// Width of the action column.
    pub col_action_width: f32,
    // The width each column had before it was hidden from the header's menu
    // (0 while it is shown). A hidden column is one of width 0, so every
    // layout that sums or reads the widths already leaves it out.
    pub(crate) hidden_column_widths: [f32; 4],
    // Resize state
    /// The column currently being resized by a drag, if any.
    pub resizing_column: Option<ResizingColumn>,
    /// Focus handle for the explorer page.
    pub focus_handle: FocusHandle,
    /// Whether focus should be requested on the next render.
    pub focus_requested: bool,
    /// Information about the most recent row click, used for double-click detection.
    pub last_click_info: Option<LastClickInfo>,
    /// Whether the listing is shown as a list or a grid.
    pub view_mode: ViewMode,
    /// Whether the left quick-access sidebar is shown (toggled with `Cmd/Ctrl+B`).
    pub sidebar_visible: bool,
    /// What the sidebar lists and which of its nodes are open.
    pub(crate) sidebar: super::places::SidebarState,

    // Search
    /// The full-text search service, when available.
    pub search_service: Option<Arc<SearchService>>,
    /// The directory scope for full-text search.
    pub search_scope: SearchScope,
    /// The kind of items full-text search targets.
    pub search_type: SearchType,
    /// Whether full-text search is case-sensitive.
    pub match_case: bool,
    /// Whether full-text search matches whole words only.
    pub match_whole_word: bool,
    /// Whether the full-text search query is treated as a regular expression.
    pub use_regex: bool,
    /// Results of the active full-text search, when one is displayed.
    pub search_results: Option<Vec<SearchFileResult>>,
    /// Whether a full-text search is currently running.
    pub is_performing_search: bool,
    // Monotonic counter incremented on every search request; used to discard
    // results from a stale in-flight search that completes after a newer one.
    /// Monotonic counter used to discard results from stale in-flight searches.
    pub search_generation: u64,
    /// Paths of search-result files whose match snippets are expanded.
    pub expanded_search_files: std::collections::HashSet<String>,
    // Syntax
    /// Service providing syntax highlighting for previews.
    pub syntax_service: Arc<SyntaxService>,
    // Preview State
    /// Path of the image currently shown in the preview pane.
    pub preview_image_path: Option<String>,
    /// Markdown source shown rendered in the preview pane.
    pub preview_markdown: Option<gpui::SharedString>,
    /// Message shown in the preview pane when a file cannot be previewed.
    pub preview_message: Option<String>,
    /// A picture the preview decoded itself (a Photoshop file, an Affinity
    /// or Illustrator thumbnail), drawn like an image.
    pub(crate) preview_picture: Option<std::sync::Arc<gpui::RenderImage>>,
    /// A line about what the preview shows (a picture's size), in its header.
    pub(crate) preview_caption: Option<String>,
    /// Whether a Markdown file is shown as its source, to be edited.
    pub(crate) preview_editing: bool,
    /// Whether the text in the preview's editor differs from the file's.
    pub(crate) preview_dirty: bool,
    /// Whether the editor's text is the file as written, so saving it
    /// writes only what was typed (a re-indented JSON is not).
    pub(crate) preview_saveable: bool,
    /// Whether the file's lines end in CRLF, which saving keeps.
    pub(crate) preview_crlf: bool,
    /// Watches the editor's changes, to know when there is something to save.
    pub(crate) preview_editor_changes: Option<gpui::Subscription>,
    /// A folder's contents or a zip's entries, shown as a list in the preview.
    pub(crate) preview_listing: Option<std::rc::Rc<super::preview_listing::PreviewListing>>,
    /// The PDF in the preview, its pages arriving as they render.
    pub(crate) preview_pdf: Option<super::pdf::PdfPreview>,
    /// The video or audio file playing in the preview.
    pub(crate) preview_media: Option<super::media::MediaPreview>,
    /// Player volume, 0..=1, and whether it is muted: kept by the pane, not
    /// the player, so the next file plays as loud as the last one did.
    pub(crate) media_volume: f64,
    pub(crate) media_muted: bool,
    /// The selection was set without the preview following it (`reveal`);
    /// the next render opens it.
    pub(crate) preview_selection_pending: bool,
    /// The preview waiting for the row the arrow keys left active to stay put
    /// (`preview::PREVIEW_FOLLOW_DELAY`); replacing it cancels it.
    pub(crate) preview_follow: Option<gpui::Task<()>>,
    /// That wait; a field so tests can drive it with the test clock.
    pub(crate) preview_follow_delay: std::time::Duration,
    /// The read of the file being previewed and the step that shows it.
    /// Replacing it drops the read of a file nobody is looking at any more.
    pub(crate) preview_load: Option<gpui::Task<()>>,
    /// A drag of a docked panel's edge or grip, or of an edge that splits.
    pub(crate) dock_drag: Option<super::view::panels::DockDrag>,
    /// The area the listing and its docked panels share, measured at paint.
    pub(crate) columns_bounds: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
    /// Whether the window has room for another pane, so a free edge of the
    /// listing drags one out. Set by the pane group on every render.
    pub(crate) split_available: bool,
    /// Whether the page's terminal is open; while it is not, an edge of the
    /// listing offers it. Set by the page on every render.
    pub(crate) terminal_shown: bool,
    /// The column the PDF preview's pages fill, measured at paint.
    pub(crate) preview_pdf_bounds: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
    /// The image in the preview decoded from its file at the panel's size
    /// (`picture.rs`), replacing the Shell's lower-quality thumbnail.
    pub(crate) preview_sharp: Option<super::view::preview::SharpPicture>,
    /// The 3D model in the preview, turned by dragging.
    pub(crate) preview_model: Option<super::model3d::ModelPreview>,
    /// What the preview showed before an item of a folder listing was
    /// clicked in it, for its back button. Cleared when the selection in the
    /// listing opens something new.
    pub(crate) preview_back: Vec<String>,
    /// Magnification of the image, video, PDF, waveform or model in the
    /// preview, 1 = fit. Back to 1 for every file opened.
    pub(crate) preview_zoom: f32,
    /// Magnification of a folder or archive listing in the preview. Kept
    /// across folders: it is how large the user wants the items, not a
    /// look at one file.
    pub(crate) preview_listing_zoom: f32,
    /// Scroll of the zoomed content, so a drag can pan it.
    pub(crate) preview_scroll: gpui::ScrollHandle,
    /// Width of the preview's content, measured at paint, which the
    /// listing's grid lays its columns out by.
    pub(crate) preview_width: std::rc::Rc<std::cell::Cell<f32>>,
    /// Last point of a drag panning the zoomed preview.
    pub(crate) preview_pan: Option<gpui::Point<gpui::Pixels>>,
    /// The editor entity backing a text preview.
    pub preview_editor: Option<Entity<PreviewEditor>>,
    // Transient message shown in the footer status bar.
    /// Transient message shown in the footer status bar.
    pub status_message: Option<StatusMessage>,

    // File operations (`docs/explorer-essentials.md` §1)
    /// Where a trashed item's origin is recorded, on platforms with no OS trash
    /// index of their own — or why it cannot be, which has to stop a delete
    /// rather than let one through unrecorded.
    pub(crate) trash_ledger: TrashLedger,
    /// In-progress inline rename of a listing row, if any.
    pub(crate) renaming: Option<super::file_ops::RenameState>,
    /// The path bar while it is a text field (double click on it), if open.
    pub(crate) path_edit: Option<super::path_edit::PathEditState>,
    /// A left press on an entry that may become a drag.
    pub(crate) drag_candidate: Option<super::drag::DragCandidate>,
    /// A left press on an item the preview lists (a folder's contents) that
    /// may become a drag out.
    pub(crate) preview_drag: Option<super::drag::PreviewDragCandidate>,
    /// A selection rectangle being dragged over the listing (`marquee.rs`).
    pub(crate) marquee: Option<super::marquee::Marquee>,
    /// Where each entry was painted, for the rectangle's hit test.
    pub(crate) hit_boxes: super::marquee::HitBoxes,
    /// In-progress paste whose name conflicts are being resolved via the dialog.
    pub(crate) paste_plan: Option<super::file_ops::PastePlan>,

    // Zoom and the grid it drives
    /// Listing zoom in 0..=1: the list table at the bottom, ever larger tiles
    /// above (see `zoom.rs`). `view_mode` always follows it.
    pub(crate) zoom: f32,
    /// Where `zoom` is heading. The wheel, the keys and the menu move the
    /// target and `tick_zoom` glides `zoom` after it, frame by frame, as File
    /// Pilot does; dragging the slider sets both at once.
    pub(crate) zoom_target: f32,
    /// When the glide last advanced, to step it by elapsed time rather than
    /// by frame count.
    pub(crate) zoom_tick: Option<std::time::Instant>,
    /// Each entry name laid out on one line, kept across frames: a tile
    /// label is wrapped from it at any width without shaping the text again,
    /// which is what lets every row's height follow the zoom as it glides.
    pub(crate) name_layouts: std::collections::HashMap<String, std::sync::Arc<gpui::LineLayout>>,
    /// Bumped by `update_item_sizes`, which every change to the rows on
    /// screen goes through. The row layouts below are kept while it, and the
    /// sizes they were built for, stay the same.
    pub(crate) rows_generation: u64,
    /// The grid's rows and their heights, as last built.
    pub(crate) grid_layout: Option<super::view::listing::grid::GridLayout>,
    /// The list's rows and their sizes, as last built.
    pub(crate) list_layout: Option<super::view::listing::list::ListLayout>,
    /// The grid zoom to return to when the grid is chosen again after the list.
    pub(crate) last_grid_zoom: f32,
    /// Tiles per grid row at the last render, for Up/Down to move by a row.
    pub(crate) grid_columns: usize,
    /// Width the grid had at the last paint; rows are laid out against it.
    pub(crate) grid_width: f32,
    /// Scroll position of the virtualized grid.
    pub(crate) grid_scroll_handle: VirtualListScrollHandle,
    /// Scroll position of the folder or archive listing in the preview. Held
    /// here because `v_virtual_list` without one makes a new handle on every
    /// render, and the listing jumped back to the top on the next frame.
    pub(crate) preview_listing_scroll: VirtualListScrollHandle,
    /// Scroll of the List view's columns.
    pub(crate) compact_scroll_handle: VirtualListScrollHandle,
    /// Height the List view's columns are filled to, measured after layout.
    pub(crate) compact_height: f32,
    /// Names per column in the List view, from that height.
    pub(crate) compact_rows: usize,
    /// Open folders of the Tree view.
    pub(crate) tree: super::tree::TreeState,
    // Filter bar (`filter.rs`)
    /// Whether the filter bar under the header is shown.
    pub(crate) filter_bar_visible: bool,
    /// Kind of entry the listing is narrowed to; kept across folders.
    pub(crate) kind_filter: super::filter::KindFilter,
    /// Name pattern the listing is narrowed to; cleared on leaving a folder.
    pub(crate) name_filter: String,
    /// The filter bar's text field, made the first time the bar is shown
    /// (it needs a `Window`, which the pane's constructor is not handed).
    pub(crate) name_filter_input: Option<Entity<InputState>>,
    /// Thumbnails of the directory on screen.
    pub(crate) thumbnails: super::thumbnails::Thumbnails,
    /// Waveforms of the sound files in the icons view (`ui.audio_waveforms`).
    pub(crate) waveforms: super::waveforms::Waveforms,
    pub(crate) audio_waveforms: bool,
    /// Whether media opened in the preview starts playing (`ui.preview_autoplay`).
    pub(crate) preview_autoplay: bool,
    /// The sound file the mouse rests on, playing or about to (`ui.hover_audio`).
    pub(crate) hover_audio: Option<super::hover_audio::HoverAudio>,
    pub(crate) hover_audio_enabled: bool,
    pub(crate) hover_audio_delay_ms: u32,
    /// The context menu on screen, if any.
    pub(crate) context_menu: Option<super::context_menu::OpenMenu>,
    /// Whether the preview pane is shown (the bottom bar's panel button).
    pub(crate) preview_visible: bool,
    /// Space's quick look is open: the preview drawn large over the pane
    /// (`view::preview::render_quick_look`), whatever the side panel shows.
    pub(crate) quick_look: bool,
    /// The bottom bar's zoom menu is open.
    pub(crate) zoom_menu_open: bool,
    /// Closes the zoom menu shown by a wheel or key zoom (`flash_zoom_menu`);
    /// `None` while the menu was opened by a click, or is shut.
    pub(crate) zoom_menu_flash: Option<gpui::Task<()>>,
    /// The mouse is on the zoom menu.
    pub(crate) zoom_menu_hovered: bool,
    /// The header's "open in another program" menu is open.
    pub(crate) open_in_menu: bool,
    /// Whether the recent-folders menu under the header's button is open.
    pub(crate) recent_menu: bool,
    /// The file `reveal` brought forward and when: it glows for a moment so
    /// the eye finds it among its neighbours (`reveal_glow`).
    pub(crate) reveal_highlight: Option<(String, std::time::Instant)>,
    /// The revealed entry still to be scrolled to, and the grid width the
    /// last scroll aimed with (`None` before any). A tab opened for it has
    /// not measured its grid yet: with no width there is one column, and the
    /// row worked out from it is far past the file. The grid scrolls only
    /// once it has a width, and again whenever the width changes, until a
    /// frame finds the width it aimed with.
    pub(crate) reveal_scroll: Option<(usize, Option<f32>)>,
    /// What changes on disk refresh the listing (`folder_watch`).
    pub(crate) folder_watch: Option<super::folder_watch::FolderWatch>,
    /// The zoom menu's slider knob is being dragged.
    pub(crate) zoom_dragging: bool,
    /// Whether the bar under the listing searches subfolders (Ctrl+F) rather
    /// than filtering the folder on screen.
    pub(crate) quick_recursive: bool,
    /// Each search hit's folder, relative to where the search started.
    pub(crate) hit_folders: std::collections::HashMap<String, String>,
    /// The subfolder search in progress or last finished.
    pub(crate) deep_search: Option<super::quick_search::DeepSearch>,
    /// The bar searches the whole computer ("Everything" mode) when set.
    pub(crate) everything: Option<super::everything::EverythingMode>,
    /// Decoded frames of the GIF playing in the preview, and which one it is.
    pub(crate) gif_cache: Option<(String, Entity<gpui::RetainAllImageCache>)>,
    /// The `[compact]` settings, handed over by the pane group each render.
    pub(crate) compact: nohrs_core::config::Compact,
    /// The top edge's slide, shared with the tab row; the group ticks it.
    pub(crate) top_reveal: Option<std::rc::Rc<crate::compact::Reveal>>,
    /// The other edges' slides, ticked here: sidebar, preview, bottom bar.
    pub(crate) left_reveal: std::rc::Rc<crate::compact::Reveal>,
    pub(crate) right_reveal: std::rc::Rc<crate::compact::Reveal>,
    pub(crate) bottom_reveal: std::rc::Rc<crate::compact::Reveal>,
    /// The address bar's own height, measured as it lays out, for its slide.
    pub(crate) header_height: std::rc::Rc<std::cell::Cell<f32>>,
    /// Which bars come out of compact mode's frame on this frame.
    pub(crate) framed: FramedBars,
}

/// Bars drawn as part of compact mode's frame: with no background or
/// separator line of their own, so the frame's colour, and its rounded
/// corners, show through. Set by the pane's render before the bars render.
#[derive(Clone, Copy, Default)]
pub(crate) struct FramedBars {
    pub header: bool,
    pub sidebar: bool,
    pub preview: bool,
    pub bottom: bool,
}

impl Focusable for ExplorerPane {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PaneEvent> for ExplorerPane {}

impl crate::pane_group::PaneItem for ExplorerPane {
    fn tab_title(&self, cx: &gpui::App) -> String {
        if let Some(name) = self.location_name(cx) {
            return name;
        }
        std::path::Path::new(&self.cwd)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.cwd.clone())
    }

    fn set_split_available(&mut self, available: bool) {
        self.split_available = available;
    }

    fn attach_compact(
        &mut self,
        settings: &nohrs_core::config::Compact,
        top: &std::rc::Rc<crate::compact::Reveal>,
    ) {
        if settings.enabled && !self.compact.enabled {
            for reveal in [&self.left_reveal, &self.right_reveal, &self.bottom_reveal] {
                reveal.hide_now();
            }
        }
        if self.compact != *settings {
            self.compact = settings.clone();
        }
        if !self.top_reveal.as_ref().is_some_and(|mine| std::rc::Rc::ptr_eq(mine, top)) {
            self.top_reveal = Some(top.clone());
        }
    }
}

impl ExplorerPane {
    /// Builds a self-contained pane, creating the window-bound sub-entities
    /// (listing/preview resizable, search input) and focus handle it owns. Used
    /// by the split-view container, which may hold several independent panes.
    pub fn build(
        search_service: Option<Arc<SearchService>>,
        trash_ledger: TrashLedger,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let resizable = cx.new(|_| ResizableState::default());
        let search_input = cx.new(|cx| InputState::new(window, cx));
        Self::new(
            resizable,
            search_input,
            search_service,
            trash_ledger,
            cx.focus_handle(),
        )
    }

    /// Creates a new explorer pane rooted at the current working directory,
    /// wired to the given resizable, search input, and optional search service.
    pub fn new(
        resizable: Entity<ResizableState>,
        search_input: Entity<InputState>,
        search_service: Option<Arc<SearchService>>,
        trash_ledger: TrashLedger,
        focus_handle: FocusHandle,
    ) -> Self {
        Self {
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".into()),
            history: Vec::new(),
            history_index: 0,
            entries: Vec::new(),
            filtered_entries: Vec::new(),
            loaded: false,
            listing_generation: 0,
            listing_pending: false,
            listing_tasks: Vec::new(),
            listing_wait: super::navigation::LISTING_WAIT,
            reveal_after_listing: None,
            listed_cwd: None,
            default_sort_key: SortKey::Name,
            default_sort_asc: true,
            view_folder: None,
            sort_key: SortKey::Name,
            sort_asc: true,
            folder_order: Default::default(),
            applied_ui: None,
            group_by: Default::default(),
            group_runs: Vec::new(),
            show_hidden: false,
            show_quick_access: true,
            icon_pack: "default".to_string(),
            name_lines: 0,
            scroll_smoothness: nohrs_core::config::Ui::default().scroll_smoothness,
            smooth_scroll: None,
            listing_extent: Default::default(),
            search_query: String::new(),
            search_visible: false,
            search_input,
            resizable,
            list: None,
            subs: Vec::new(),
            preview_path: None,
            preview_text: None,
            selection: std::collections::HashSet::new(),
            selection_anchor: None,
            active_path: None,
            virtual_scroll_handle: VirtualListScrollHandle::new(),
            item_sizes: Rc::new(Vec::new()),
            col_name_width: config::COL_NAME_WIDTH,
            col_type_width: config::COL_TYPE_WIDTH,
            col_size_width: config::COL_SIZE_WIDTH,
            col_modified_width: config::COL_MODIFIED_WIDTH,
            col_action_width: config::COL_ACTION_WIDTH,
            hidden_column_widths: [0.0; 4],
            resizing_column: None,
            focus_handle,
            focus_requested: false,
            last_click_info: None,
            view_mode: ViewMode::List,
            // Hidden by default; the root pane is revealed by `ExplorerPage::new`,
            // split-created panes stay collapsed (issue #164, §2).
            sidebar_visible: false,
            sidebar: Default::default(),

            // Search
            search_service,
            search_scope: SearchScope::Home,
            search_type: SearchType::All,
            match_case: false,
            match_whole_word: false,
            use_regex: false,
            search_results: None,
            is_performing_search: false,
            search_generation: 0,
            expanded_search_files: std::collections::HashSet::new(),
            syntax_service: Arc::new(SyntaxService::new()),
            preview_editor: None,
            preview_image_path: None,
            preview_markdown: None,
            preview_message: None,
            preview_picture: None,
            preview_caption: None,
            preview_editing: false,
            preview_dirty: false,
            preview_saveable: false,
            preview_crlf: false,
            preview_editor_changes: None,
            preview_listing: None,
            preview_pdf: None,
            preview_media: None,
            media_volume: 1.0,
            preview_selection_pending: false,
            preview_follow: None,
            preview_follow_delay: super::preview::PREVIEW_FOLLOW_DELAY,
            preview_load: None,
            dock_drag: None,
            columns_bounds: Default::default(),
            split_available: false,
            terminal_shown: false,
            preview_pdf_bounds: Default::default(),
            preview_sharp: None,
            preview_model: None,
            preview_back: Vec::new(),
            preview_zoom: 1.0,
            preview_listing_zoom: 1.0,
            preview_scroll: gpui::ScrollHandle::new(),
            preview_width: Default::default(),
            preview_pan: None,
            media_muted: false,
            status_message: None,
            trash_ledger,
            renaming: None,
            path_edit: None,
            drag_candidate: None,
            preview_drag: None,
            marquee: None,
            hit_boxes: Default::default(),
            paste_plan: None,
            zoom: 0.0,
            zoom_target: 0.0,
            zoom_tick: None,
            name_layouts: std::collections::HashMap::new(),
            rows_generation: 0,
            grid_layout: None,
            list_layout: None,
            last_grid_zoom: super::zoom::DEFAULT_GRID,
            grid_columns: 1,
            grid_width: 0.0,
            grid_scroll_handle: VirtualListScrollHandle::new(),
            preview_listing_scroll: VirtualListScrollHandle::new(),
            compact_scroll_handle: VirtualListScrollHandle::new(),
            compact_height: 400.0,
            compact_rows: 1,
            tree: Default::default(),
            filter_bar_visible: false,
            kind_filter: Default::default(),
            name_filter: String::new(),
            name_filter_input: None,
            thumbnails: Default::default(),
            waveforms: Default::default(),
            audio_waveforms: false,
            preview_autoplay: false,
            hover_audio: None,
            hover_audio_enabled: true,
            hover_audio_delay_ms: 400,
            context_menu: None,
            preview_visible: true,
            quick_look: false,
            zoom_menu_open: false,
            zoom_menu_flash: None,
            zoom_menu_hovered: false,
            open_in_menu: false,
            recent_menu: false,
            reveal_highlight: None,
            reveal_scroll: None,
            folder_watch: None,
            zoom_dragging: false,
            quick_recursive: false,
            deep_search: None,
            everything: None,
            hit_folders: Default::default(),
            gif_cache: None,
            compact: Default::default(),
            top_reveal: None,
            left_reveal: Default::default(),
            right_reveal: Default::default(),
            bottom_reveal: Default::default(),
            header_height: std::rc::Rc::new(std::cell::Cell::new(52.0)),
            framed: FramedBars::default(),
        }
    }

    pub(crate) fn set_status(&mut self, level: StatusLevel, text: impl Into<String>) {
        self.status_message = Some(StatusMessage {
            text: text.into(),
            level,
        });
    }

    pub(crate) fn clear_status(&mut self) {
        self.status_message = None;
    }

    /// Toggles the left quick-access sidebar (issue #164, §2). Mirrors the
    /// `toggle_search` open/close pattern.
    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        cx.notify();
    }

    /// Returns the current status text and what it is reporting, for rendering
    /// in the footer status bar.
    pub fn status_for_footer(&self) -> Option<(String, StatusLevel)> {
        self.status_message
            .as_ref()
            .map(|status| (status.text.clone(), status.level))
    }

    pub(crate) fn set_search_scope(&mut self, scope: SearchScope, cx: &mut Context<Self>) {
        if self.search_scope != scope {
            self.search_scope = scope;
            cx.notify();
        }
    }

    pub(crate) fn toggle_match_case(&mut self, cx: &mut Context<Self>) {
        self.match_case = !self.match_case;
        cx.notify();
    }

    pub(crate) fn toggle_match_whole_word(&mut self, cx: &mut Context<Self>) {
        self.match_whole_word = !self.match_whole_word;
        cx.notify();
    }

    pub(crate) fn toggle_use_regex(&mut self, cx: &mut Context<Self>) {
        self.use_regex = !self.use_regex;
        cx.notify();
    }

    pub(crate) fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        // Against where the glide is headed, not the view on screen: choosing
        // List again while a glide to Grid is still in List would otherwise
        // be ignored, and the glide would carry on to Grid.
        if super::zoom::view_mode(self.zoom_target) != mode {
            let zoom = match mode {
                ViewMode::List => 0.0,
                ViewMode::Tree => super::zoom::TREE,
                ViewMode::Compact => super::zoom::LIST_MAX,
                ViewMode::Grid => self.last_grid_zoom,
            };
            self.zoom_to(zoom, cx);
        }
    }

    /// The image cache the preview draws `path` from.
    ///
    /// Only the previewed GIF plays; the grid shows stills. GPUI decodes every
    /// frame at full size and keeps them, so the cache is replaced when the
    /// preview moves to another file, which releases the last one's frames.
    pub(crate) fn preview_image_cache(
        &mut self,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Entity<gpui::RetainAllImageCache> {
        match &self.gif_cache {
            Some((held, cache)) if held == path => cache.clone(),
            _ => {
                let cache = gpui::RetainAllImageCache::new(cx);
                self.gif_cache = Some((path.to_string(), cache.clone()));
                cache
            }
        }
    }

    /// Shows or hides the filter bar. Hiding it also drops its filters, so a
    /// listing never stays narrowed by something no longer on screen.
    pub(crate) fn toggle_filter_bar(&mut self, cx: &mut Context<Self>) {
        self.filter_bar_visible = !self.filter_bar_visible;
        if !self.filter_bar_visible {
            self.kind_filter = Default::default();
            self.name_filter.clear();
            self.name_filter_input = None;
        }
        self.apply_filter();
        cx.notify();
    }

    /// Narrows the listing to one kind of entry.
    pub(crate) fn set_kind_filter(
        &mut self,
        kind: super::filter::KindFilter,
        cx: &mut Context<Self>,
    ) {
        if self.kind_filter != kind {
            self.kind_filter = kind;
            self.apply_filter();
            cx.notify();
        }
    }

    /// Drops the name pattern on leaving a folder: it was typed for that
    /// folder's names. The kind filter is a way of browsing and carries over.
    pub(crate) fn clear_name_filter(&mut self) {
        self.name_filter.clear();
        self.name_filter_input = None;
    }

    /// Whether the filter bar is narrowing the listing at all.
    pub(crate) fn is_filtering(&self) -> bool {
        self.kind_filter != Default::default() || !self.name_filter.trim().is_empty()
    }

    /// Selects the entry at `path` and scrolls it into view. A filter hiding
    /// it is dropped first: the entry was asked for by name.
    pub(crate) fn reveal(&mut self, path: &str) {
        if self.listing_pending {
            self.reveal_after_listing = Some(path.to_owned());
            return;
        }
        let find = |pane: &Self| {
            pane.filtered_entries
                .iter()
                .position(|entry| super::same_path(&entry.path, path))
        };
        let index = match find(self) {
            Some(index) => Some(index),
            None if self.is_filtering() => {
                self.kind_filter = Default::default();
                self.clear_name_filter();
                self.apply_filter();
                find(self)
            }
            None => None,
        };
        let Some(index) = index else {
            return;
        };
        self.select_single(index);
        self.reveal_highlight = Some((path.to_owned(), std::time::Instant::now()));
        // Previewed on the next frame, which has the window that opening a
        // preview needs: a file another program sent was selected with the
        // preview still on whatever it showed before.
        self.preview_selection_pending = true;
        self.reveal_scroll = Some((index, None));
        self.scroll_entry_into_view(index);
    }

    /// Scrolls entry `index` to the middle of the listing, in any view.
    pub(crate) fn scroll_entry_into_view(&mut self, index: usize) {
        let (list_row, grid_row) = self.visual_rows_of(index);
        match self.view_mode {
            ViewMode::List | ViewMode::Tree => self
                .virtual_scroll_handle
                .scroll_to_item(list_row, gpui::ScrollStrategy::Center),
            ViewMode::Compact => self.compact_scroll_handle.scroll_to_item(
                index / self.compact_rows.max(1),
                gpui::ScrollStrategy::Center,
            ),
            ViewMode::Grid => self
                .grid_scroll_handle
                .scroll_to_item(grid_row, gpui::ScrollStrategy::Center),
        }
    }

    /// How many entries Up/Down move by: a whole row of tiles in the grid.
    pub(crate) fn row_stride(&self) -> isize {
        match self.view_mode {
            ViewMode::List | ViewMode::Tree | ViewMode::Compact => 1,
            ViewMode::Grid => self.grid_columns.max(1) as isize,
        }
    }

    /// Sets the listing zoom at once, with no glide: for the slider, which
    /// the hand is already moving smoothly.
    pub(crate) fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(0.0, 1.0);
        self.zoom_target = zoom;
        self.zoom_tick = None;
        self.apply_zoom(zoom, cx);
        self.remember_folder_view(cx);
    }

    /// Glides the listing zoom to `target` over the next frames.
    pub(crate) fn zoom_to(&mut self, target: f32, cx: &mut Context<Self>) {
        let target = target.clamp(0.0, 1.0);
        if target != self.zoom_target {
            self.zoom_target = target;
            self.remember_folder_view(cx);
            cx.notify();
        }
    }

    /// Advances the glide toward `zoom_target`, asking for another frame
    /// while it is not there. Called on every render of the pane.
    pub(crate) fn tick_zoom(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        // How long the glide takes to cover about two thirds of the way: the
        // rest eases out, so a burst of wheel notches reads as one movement.
        const TIME_CONSTANT: f32 = 0.07;
        let remaining = self.zoom_target - self.zoom;
        if remaining.abs() < 0.0005 {
            self.zoom_tick = None;
            if remaining != 0.0 {
                self.apply_zoom(self.zoom_target, cx);
            }
            return;
        }
        let now = std::time::Instant::now();
        // The first frame of a glide has no elapsed time to go by; a 60 Hz
        // frame's worth starts it moving at once.
        let elapsed = self
            .zoom_tick
            .map_or(1.0 / 60.0, |last| now.duration_since(last).as_secs_f32())
            .min(0.1);
        self.zoom_tick = Some(now);
        let advanced = 1.0 - (-elapsed / TIME_CONSTANT).exp();
        self.apply_zoom(self.zoom + remaining * advanced, cx);
        window.request_animation_frame();
    }

    // Shows `zoom`, switching between the list and the grid when it crosses
    // the threshold between them.
    fn apply_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        if zoom == self.zoom {
            return;
        }
        self.zoom = zoom;
        let previous = self.view_mode;
        self.view_mode = super::zoom::view_mode(zoom);
        if self.view_mode == ViewMode::Grid {
            self.last_grid_zoom = zoom;
        }
        // The Tree view's rows are the listing with open folders spliced in,
        // so entering or leaving it rebuilds them.
        if (previous == ViewMode::Tree) != (self.view_mode == ViewMode::Tree) {
            self.apply_filter();
        }
        cx.notify();
    }

    pub(crate) fn record_click(&mut self, row: usize, click_count: usize) {
        self.last_click_info = Some(LastClickInfo {
            row,
            timestamp: Instant::now(),
            click_count,
        });
    }

    /// Path of the entry shown at row `ix`, if there is one.
    fn path_at(&self, ix: usize) -> Option<&str> {
        self.filtered_entries
            .get(ix)
            .map(|entry| entry.path.as_str())
    }

    /// Row currently showing `path`, if it is visible.
    fn row_of(&self, path: &str) -> Option<usize> {
        self.filtered_entries
            .iter()
            .position(|entry| entry.path == path)
    }

    /// Returns whether the row at `ix` (an index into `filtered_entries`) is
    /// part of the current selection.
    pub fn is_selected(&self, ix: usize) -> bool {
        self.path_at(ix)
            .is_some_and(|path| self.selection.contains(path))
    }

    /// Row of the active/primary entry, if it is visible.
    pub fn active_row(&self) -> Option<usize> {
        self.active_path
            .as_deref()
            .and_then(|path| self.row_of(path))
    }

    /// Replaces the selection with the single row `ix`, making it both the
    /// anchor and the active row (a plain click or arrow-key move).
    pub(crate) fn select_single(&mut self, ix: usize) {
        let Some(path) = self.path_at(ix).map(str::to_string) else {
            return;
        };
        self.selection.clear();
        self.selection.insert(path.clone());
        self.selection_anchor = Some(path.clone());
        self.active_path = Some(path);
    }

    /// Toggles row `ix` in the selection (Cmd/Ctrl+click) and re-anchors there.
    pub(crate) fn toggle_select(&mut self, ix: usize) {
        let Some(path) = self.path_at(ix).map(str::to_string) else {
            return;
        };
        if !self.selection.remove(&path) {
            self.selection.insert(path.clone());
        }
        self.selection_anchor = Some(path.clone());
        self.active_path = Some(path);
    }

    /// Selects the contiguous range between the current anchor and `ix`
    /// (Shift+click / Shift+arrow). Falls back to a single selection when there
    /// is no anchor yet, or when the anchored entry is no longer visible.
    pub(crate) fn select_range_to(&mut self, ix: usize) {
        let anchor = match self
            .selection_anchor
            .as_deref()
            .and_then(|a| self.row_of(a))
        {
            Some(anchor) => anchor,
            None => {
                self.select_single(ix);
                return;
            }
        };
        let (low, high) = if anchor <= ix {
            (anchor, ix)
        } else {
            (ix, anchor)
        };
        let range = (low..=high)
            .filter_map(|row| self.path_at(row).map(str::to_string))
            .collect();
        self.selection = range;
        self.active_path = self.path_at(ix).map(str::to_string);
    }

    /// Selects every visible row.
    pub(crate) fn select_all(&mut self) {
        self.selection = self
            .filtered_entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        self.selection_anchor = self.path_at(0).map(str::to_string);
        self.active_path = self
            .filtered_entries
            .len()
            .checked_sub(1)
            .and_then(|last| self.path_at(last))
            .map(str::to_string);
    }

    /// Clears the selection, anchor, and active row.
    pub(crate) fn clear_selection(&mut self) {
        self.selection.clear();
        self.selection_anchor = None;
        self.active_path = None;
    }

    // Drops selected, anchored, and active paths that the visible rows no longer
    // hold, so a hidden or deleted entry cannot be acted on and cannot come back
    // selected if something later takes its path.
    pub(crate) fn prune_selection(&mut self) {
        let visible: std::collections::HashSet<&str> = self
            .filtered_entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        self.selection
            .retain(|path| visible.contains(path.as_str()));
        let still_visible =
            |path: &Option<String>| path.as_deref().is_some_and(|path| visible.contains(path));
        if !still_visible(&self.selection_anchor) {
            self.selection_anchor = None;
        }
        if !still_visible(&self.active_path) {
            self.active_path = None;
        }
    }

    /// Moves the active row by `delta` rows, clamped to the visible range. With
    /// `extend` (Shift held) the selection grows from the anchor; otherwise the
    /// moved-to row becomes the sole selection. Selecting from an empty state
    /// lands on the first row.
    pub(crate) fn move_active(&mut self, delta: isize, extend: bool) {
        let len = self.filtered_entries.len();
        if len == 0 {
            return;
        }
        let next = match self.active_row() {
            Some(current) => (current as isize + delta).clamp(0, len as isize - 1) as usize,
            None => 0,
        };
        if extend {
            self.select_range_to(next);
        } else {
            self.select_single(next);
        }
        if self.view_mode == ViewMode::Grid {
            let (_, row) = self.visual_rows_of(next);
            self.grid_scroll_handle
                .scroll_to_item(row, gpui::ScrollStrategy::Center);
        }
        if self.view_mode == ViewMode::Compact {
            self.compact_scroll_handle.scroll_to_item(
                next / self.compact_rows.max(1),
                gpui::ScrollStrategy::Center,
            );
        }
    }

    /// Paths of the currently selected rows, in row order. Used by file
    /// operations and drag-and-drop.
    pub fn selected_paths(&self) -> Vec<String> {
        self.filtered_entries
            .iter()
            .filter(|entry| self.selection.contains(&entry.path))
            .map(|entry| entry.path.clone())
            .collect()
    }

    pub(crate) fn update_item_sizes(&mut self) {
        self.rows_generation += 1;
        let total_width = self.total_table_width();

        let sizes = self
            .filtered_entries
            .iter()
            .map(|entry| {
                // Check if there are match snippets for this file AND it's expanded
                let is_expanded = self.expanded_search_files.contains(&entry.path);
                let snippet_count = if is_expanded {
                    self.search_results
                        .as_ref()
                        .and_then(|results| {
                            results
                                .iter()
                                .find(|r| r.path == entry.path)
                                .map(|r| r.matches.len().min(config::MAX_SNIPPETS))
                        })
                        .unwrap_or(0)
                } else {
                    0
                };
                let total_height =
                    config::BASE_ROW_HEIGHT + (snippet_count as f32 * config::SNIPPET_ROW_HEIGHT);
                size(px(total_width), px(total_height))
            })
            .collect();
        self.item_sizes = Rc::new(sizes);
    }

    pub(crate) fn total_table_width(&self) -> f32 {
        self.col_name_width
            + self.col_type_width
            + self.col_size_width
            + self.col_modified_width
            + self.col_action_width
            + config::TABLE_HORIZONTAL_PADDING
    }

    /// Rebuilds the rows on screen from `entries`, sorted. `entries` itself
    /// is never sorted: only counts are read from it, and sorting it first
    /// sorted every folder twice.
    pub(crate) fn apply_filter(&mut self) {
        // The selection is addressed by path, so rebuilding the visible rows
        // keeps it: sorting or filtering no longer throws away what the user
        // picked. `prune_selection` below drops whatever is no longer visible.

        // When explicit search results are displayed, `filtered_entries` is owned
        // by the search path; only refresh row sizes here.
        if self.search_results.is_some() {
            self.prune_selection();
            self.update_item_sizes();
            return;
        }

        let show_hidden = self.show_hidden;
        let kind_filter = self.kind_filter;
        let name_filter = super::filter::NameMatcher::new(&self.name_filter);
        let visible = self
            .entries
            .iter()
            .filter(move |entry| show_hidden || !is_hidden(&entry.name))
            .filter(move |entry| kind_filter.matches(entry))
            .filter(move |entry| name_filter.matches(&entry.name));

        if self.search_query.is_empty() {
            self.filtered_entries = visible.cloned().collect();
        } else {
            // The quick search reads names the way the filter bar does, so a
            // Chinese name can be typed in pinyin initials (`wjj` finds
            // 文件夹).
            let query = super::filter::NameMatcher::new(&self.search_query);
            self.filtered_entries = visible.filter(|entry| query.matches(&entry.name)).cloned().collect();
        }

        entries::sort_entries_with(&mut self.filtered_entries, self.sort_key, self.sort_asc, self.folder_order);
        if self.view_mode == ViewMode::Tree {
            // Groups would pull a folder's children away from it.
            self.group_runs.clear();
            self.splice_tree();
        } else {
            self.group_runs = super::grouping::group_entries(
                &mut self.filtered_entries,
                self.group_by,
                &super::grouping::Clock::now(),
            );
        }
        self.prune_selection();
        self.update_item_sizes();
    }

    /// The group runs, when they describe the entries on screen. Search
    /// results and a folder being read replace `filtered_entries` without
    /// grouping it, so runs that no longer add up to it are ignored rather
    /// than drawn over the wrong rows.
    pub(crate) fn groups(&self) -> &[super::grouping::Group] {
        let covered: usize = self.group_runs.iter().map(|group| group.len).sum();
        if self.search_results.is_none() && covered == self.filtered_entries.len() {
            &self.group_runs
        } else {
            &[]
        }
    }

    /// Groups the listing by `by`, keeping the sort inside each group.
    pub(crate) fn set_group_by(&mut self, by: super::grouping::GroupBy) {
        self.group_by = by;
        self.apply_filter();
    }

    /// The list row (counting the column header as row 0) and the grid row
    /// that show entry `index`, once group headers are taken into account.
    pub(crate) fn visual_rows_of(&self, index: usize) -> (usize, usize) {
        let groups = self.groups();
        let list = super::grouping::list_rows(groups, self.filtered_entries.len())
            .iter()
            .position(|row| *row == super::grouping::ListRow::Entry(index))
            .map_or(index + 1, |row| row + 1);
        let grid = super::grouping::grid_rows(groups, self.filtered_entries.len(), self.grid_columns)
            .iter()
            .position(|row| matches!(row, super::grouping::GridRow::Tiles { first, end } if (*first..*end).contains(&index)))
            .unwrap_or(index / self.grid_columns.max(1));
        (list, grid)
    }

    /// Apply the `[ui]` config section to this open view, re-sorting and
    /// re-filtering when anything changed. Used both at startup and on hot
    /// reload (config.md §5). Note: `icon_pack` is stored for the row renderer to
    /// consult; there is no icon cache to invalidate yet.
    pub fn apply_config_ui(&mut self, ui: &config::Ui, cx: &mut Context<Self>) {
        let previous = self.applied_ui.replace(ui.clone());
        self.default_sort_key = sort_key_from_config(ui.default_sort);
        self.default_sort_asc = !ui.default_sort_descending;
        let mut changed = false;
        let sort_changed_in_file = previous.as_ref().is_none_or(|previous| {
            (previous.default_sort, previous.default_sort_descending)
                != (ui.default_sort, ui.default_sort_descending)
        });
        // A folder whose sort was chosen keeps it, as in Explorer; the new
        // default reaches every folder still on the default, this one too.
        let remembered = self
            .view_folder
            .is_some()
            .then(|| super::folder_views::FolderViews::get(&self.cwd, cx))
            .flatten()
            .is_some();
        if sort_changed_in_file
            && !remembered
            && (self.sort_key, self.sort_asc) != (self.default_sort_key, self.default_sort_asc)
        {
            self.sort_key = self.default_sort_key;
            self.sort_asc = self.default_sort_asc;
            changed = true;
        }
        if self.show_hidden != ui.show_hidden {
            self.show_hidden = ui.show_hidden;
            changed = true;
        }
        if self.show_quick_access != ui.show_quick_access {
            self.show_quick_access = ui.show_quick_access;
            changed = true;
        }
        if self.icon_pack != ui.icon_pack {
            self.icon_pack = ui.icon_pack.clone();
            changed = true;
        }
        self.scroll_smoothness = ui.scroll_smoothness;
        self.preview_autoplay = ui.preview_autoplay;
        self.hover_audio_delay_ms = ui.hover_audio_delay_ms;
        if self.hover_audio_enabled != ui.hover_audio {
            self.hover_audio_enabled = ui.hover_audio;
            self.end_hover_audio();
            cx.notify();
        }
        if self.audio_waveforms != ui.audio_waveforms {
            self.audio_waveforms = ui.audio_waveforms;
            cx.notify();
        }
        if self.name_lines != ui.name_lines {
            self.name_lines = ui.name_lines;
            // Every tile row's height follows its longest name.
            self.rows_generation += 1;
            cx.notify();
        }
        if changed {
            self.apply_filter();
            cx.notify();
        }
    }

    /// Sorts by `key` in the given direction, as the context menu chooses it.
    pub(crate) fn set_sort(&mut self, key: SortKey, ascending: bool, cx: &mut Context<Self>) {
        self.sort_key = key;
        self.sort_asc = ascending;
        self.apply_filter();
        self.remember_folder_view(cx);
    }

    pub(crate) fn set_sort_key(&mut self, key: SortKey, cx: &mut Context<Self>) {
        if self.sort_key == key {
            self.sort_asc = !self.sort_asc;
        } else {
            self.sort_key = key;
            self.sort_asc = true;
        }
        self.apply_filter();
        self.remember_folder_view(cx);
    }

    /// Puts on screen the view `cwd` was left with, or the defaults for a
    /// folder never changed — whose zoom is the one the tab already had, so
    /// browsing in icons stays in icons. Does nothing while the folder stays
    /// the same: a refresh keeps whatever is on screen. The listing is sorted
    /// by the read that follows.
    pub(crate) fn enter_folder_view(&mut self, cx: &mut Context<Self>) {
        let key = super::folder_views::folder_key(&self.cwd);
        if self.view_folder.as_deref() == Some(key.as_str()) {
            return;
        }
        self.view_folder = Some(key);
        match super::folder_views::FolderViews::get(&self.cwd, cx) {
            Some(view) => {
                self.sort_key = view.sort.into();
                self.sort_asc = view.ascending;
                let zoom = view.zoom.clamp(0.0, 1.0);
                // At once, not gliding: the folder opens the way it was left.
                self.zoom_target = zoom;
                self.zoom_tick = None;
                self.apply_zoom(zoom, cx);
            }
            None => {
                self.sort_key = self.default_sort_key;
                self.sort_asc = self.default_sort_asc;
            }
        }
    }

    /// Remembers how `cwd` looks now, after the user changed it.
    fn remember_folder_view(&self, cx: &mut Context<Self>) {
        // Before a folder was entered there is nothing to attach it to.
        if self.view_folder.is_none() {
            return;
        }
        super::folder_views::FolderViews::remember(
            &self.cwd,
            super::folder_views::FolderView {
                zoom: self.zoom_target,
                sort: self.sort_key.into(),
                ascending: self.sort_asc,
            },
            cx,
        );
    }
}

fn sort_key_from_config(order: config::SortOrder) -> SortKey {
    match order {
        config::SortOrder::Name => SortKey::Name,
        config::SortOrder::Modified => SortKey::Modified,
        config::SortOrder::Size => SortKey::Size,
        config::SortOrder::Kind => SortKey::Type,
    }
}

pub(crate) fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_hidden_matches_dotfiles_only() {
        assert!(is_hidden(".gitignore"));
        assert!(is_hidden("..git"));
        assert!(!is_hidden("visible.txt"));
        assert!(!is_hidden(""));
    }

    #[test]
    fn sort_key_from_config_maps_every_order() {
        assert!(sort_key_from_config(config::SortOrder::Name) == SortKey::Name);
        assert!(sort_key_from_config(config::SortOrder::Modified) == SortKey::Modified);
        assert!(sort_key_from_config(config::SortOrder::Size) == SortKey::Size);
        assert!(sort_key_from_config(config::SortOrder::Kind) == SortKey::Type);
    }
}
