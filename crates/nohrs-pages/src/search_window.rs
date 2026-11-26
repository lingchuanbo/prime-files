//! Standalone Everything-style search window.
//!
//! This view deliberately keeps only the small hit records returned by the
//! indexer. It does not walk the filesystem or duplicate the Explorer index.
//!
//! It is also a small file manager of its own: opening a folder lands in the
//! app's Explorer instead of Explorer.exe, a right-click offers the basic file
//! operations against the Explorer's own clipboard, `Alt+Q` previews the
//! selection, and `Alt+<digit|letter>` activates a result by position.

use std::ffi::OsStr;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, AppContext, Bounds, ClipboardItem, Context, Div, Entity, FontWeight,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, Pixels, Render,
    ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
    WindowBounds, WindowHandle, WindowKind, WindowOptions, div, img, px, rgb, rgba, size,
    uniform_list,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{Icon, IconName, Root};
use nohrs_services::fs::ops;
use nohrs_services::fs::trash::Ledger;
use nohrs_ui::theme::theme;

use crate::explorer::IndexedSearchHit;
use crate::explorer::everything::{self, WholeComputerError, WholeComputerHit};
use crate::explorer::file_ops::{self, ClipMode};

const WIDTH: f32 = 1180.0;
const HEIGHT: f32 = 650.0;
const LIMIT: usize = 20_000;
const DEBOUNCE: Duration = Duration::from_millis(70);
/// How long to wait before asking again while the whole-computer index is
/// still being read or walked.
const INDEX_POLL: Duration = Duration::from_millis(400);
/// How many times a search may ask for the index before giving up on it: the
/// first whole-computer search of a run reads the saved table back from disk,
/// which takes seconds.
const PREPARE_TRIES: usize = 60;
/// Width of the preview pane `Alt+Q` opens on the right.
const PREVIEW_WIDTH: f32 = 360.0;
/// How much of a text file the preview reads. A preview that renders a whole
/// log file is slower than the search that found it.
const TEXT_PREVIEW_BYTES: u64 = 32 * 1024;
/// Lines of a text file shown in the preview.
const TEXT_PREVIEW_LINES: usize = 160;
/// Entries counted for a folder's preview before the count is called "2000+".
const FOLDER_PREVIEW_LIMIT: usize = 2000;
/// Images larger than this are described instead of decoded: the preview pane
/// is a few hundred pixels wide, and decoding a huge photo at full size costs
/// far more memory than the preview is worth.
const MAX_PREVIEW_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Width of the "键" column.
const KEY_WIDTH: f32 = 22.0;
/// Width of the icon column.
const ICON_WIDTH: f32 = 20.0;
/// Width of the draggable space between two columns.
const DIVIDER_WIDTH: f32 = 8.0;
/// Horizontal padding of the header and the rows.
const ROW_INSET: f32 = 12.0;
const MIN_NAME_WIDTH: f32 = 120.0;
const MAX_NAME_WIDTH: f32 = 720.0;
const MIN_SIZE_WIDTH: f32 = 56.0;
const MAX_SIZE_WIDTH: f32 = 220.0;
const MIN_MODIFIED_WIDTH: f32 = 100.0;
const MAX_MODIFIED_WIDTH: f32 = 280.0;
const DEFAULT_NAME_WIDTH: f32 = 230.0;
const DEFAULT_SIZE_WIDTH: f32 = 90.0;
const DEFAULT_MODIFIED_WIDTH: f32 = 160.0;

/// A column whose width a header divider changes.
#[derive(Clone, Copy)]
enum ResizeColumn {
    Name,
    Size,
    Modified,
}

/// The widths the header and the rows share; the path column takes the rest.
#[derive(Clone, Copy)]
struct ColumnWidths {
    name: f32,
    size: f32,
    modified: f32,
}

/// A divider being dragged.
struct ColumnResize {
    column: ResizeColumn,
    start_x: Pixels,
    start_width: f32,
}

/// What the result list is ordered by, once a heading has been clicked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Path,
    Size,
    Modified,
}

/// Accelerators run digits first and then letters with `q` left out: `Alt+Q`
/// toggles the preview, so it cannot also open the 27th result.
const ACCELERATOR_DIGITS: [char; 10] = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
const ACCELERATOR_LETTERS: [char; 25] = [
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'r', 's', 't',
    'u', 'v', 'w', 'x', 'y', 'z',
];

/// The accelerator for the result at `index`, if it is within reach.
fn accelerator(index: usize) -> Option<char> {
    ACCELERATOR_DIGITS.get(index).copied().or_else(|| {
        let letter = index.checked_sub(ACCELERATOR_DIGITS.len())?;
        ACCELERATOR_LETTERS.get(letter).copied()
    })
}

/// The single character a row shows for its accelerator, the way Everything's
/// "键" column does: the modifier is explained once, not repeated on every row.
fn accelerator_label(character: char) -> String {
    character.to_ascii_uppercase().to_string()
}

/// The result position a pressed accelerator names, if the key is one.
fn accelerator_index(key: &str) -> Option<usize> {
    let mut characters = key.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }
    if let Some(index) = ACCELERATOR_DIGITS
        .iter()
        .position(|digit| *digit == character)
    {
        return Some(index);
    }
    ACCELERATOR_LETTERS
        .iter()
        .position(|letter| *letter == character)
        .map(|index| index + ACCELERATOR_DIGITS.len())
}

#[derive(Clone, Debug)]
struct SearchResult {
    hit: IndexedSearchHit,
    name: String,
    directory: String,
}

/// What the preview pane is showing.
#[derive(Clone)]
enum PreviewKind {
    Loading,
    Folder { children: Option<usize> },
    Image,
    Text { text: String, truncated: bool },
    Other { reason: String },
}

struct Preview {
    path: PathBuf,
    kind: PreviewKind,
    size: u64,
    modified: u32,
}

/// An open inline rename: the path it started from and the field holding the
/// typed name.
struct RenameState {
    original_path: PathBuf,
    input: Entity<InputState>,
    _subscription: Subscription,
}

/// A resizable, standalone window for searching the resident filename index.
pub struct SearchWindow {
    input: Entity<InputState>,
    results: Vec<SearchResult>,
    selected: Option<usize>,
    query: String,
    status: String,
    generation: u64,
    _input_subscription: Subscription,
    _search_task: Option<Task<()>>,
    scroll_handle: UniformListScrollHandle,
    /// Whether `Alt+Q` has the preview pane open.
    preview_visible: bool,
    preview: Option<Preview>,
    /// Bumped on every load and on every selection change, so a slow read can
    /// never land on top of a newer selection.
    preview_generation: u64,
    renaming: Option<RenameState>,
    /// The same ledger the Explorer writes, for the right-click Delete.
    trash_ledger: Ledger,
    /// What the list is ordered by once a heading has been clicked.
    sort: Option<(SortKey, bool)>,
    widths: ColumnWidths,
    resizing: Option<ColumnResize>,
    /// The menu a right click opened, if any.
    menu: Option<crate::drawn_menu::Menu>,
    /// The result the open menu acts on.
    menu_index: Option<usize>,
}

impl crate::drawn_menu::MenuHost for SearchWindow {
    fn menu(&self) -> Option<&crate::drawn_menu::Menu> {
        self.menu.as_ref()
    }

    fn set_menu(&mut self, menu: Option<crate::drawn_menu::Menu>) {
        // The result the menu acts on outlives the menu: it is closed before
        // `choose` runs, and that is where the index is read.
        self.menu = menu;
    }

    fn choose(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.menu_index.take() else {
            return;
        };
        match id {
            MENU_OPEN => self.open_result(index, cx),
            MENU_REVEAL => self.reveal_result(index, cx),
            MENU_COPY => self.copy_result(index, false, cx),
            MENU_CUT => self.copy_result(index, true, cx),
            MENU_TRASH => self.trash_result(index, cx),
            MENU_RENAME => self.begin_rename(index, window, cx),
            MENU_COPY_PATH => self.copy_path(index, cx),
            MENU_COPY_NAME => self.copy_name(index, cx),
            _ => {}
        }
        cx.notify();
    }
}

/// The ids of the rows of the result menu.
const MENU_OPEN: u32 = 1;
const MENU_REVEAL: u32 = 2;
const MENU_COPY: u32 = 3;
const MENU_CUT: u32 = 4;
const MENU_TRASH: u32 = 5;
const MENU_RENAME: u32 = 6;
const MENU_COPY_PATH: u32 = 7;
const MENU_COPY_NAME: u32 = 8;

/// The rows of the menu a right click opens on a result.
fn result_menu_rows() -> Vec<crate::drawn_menu::MenuRow> {
    use crate::drawn_menu::MenuRow;
    vec![
        MenuRow::entry("打开", true, MENU_OPEN),
        MenuRow::entry("在资源管理器中显示", true, MENU_REVEAL),
        MenuRow::rule(),
        MenuRow::entry("复制", true, MENU_COPY),
        MenuRow::entry("剪切", true, MENU_CUT),
        MenuRow::entry("删除", true, MENU_TRASH),
        MenuRow::entry("重命名", true, MENU_RENAME),
        MenuRow::rule(),
        MenuRow::entry("复制路径", true, MENU_COPY_PATH),
        MenuRow::entry("复制名字", true, MENU_COPY_NAME),
    ]
}

impl SearchWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>, trash_ledger: Ledger) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            // The syntax is Everything's: `regex:` for an expression and a
            // typed `D:\...` for a folder's tree.
            state.set_placeholder(
                "搜索文件和文件夹…（regex: 正则，或输入 D:\\ 路径）",
                window,
                cx,
            );
        });
        let subscription = cx.subscribe(&input, |this, state, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let query = state.read(cx).text().to_string();
            this.start_search(query, cx);
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        Self {
            input,
            results: Vec::new(),
            selected: None,
            query: String::new(),
            status: "输入名称即可搜索整个电脑".to_string(),
            generation: 0,
            _input_subscription: subscription,
            _search_task: None,
            scroll_handle: UniformListScrollHandle::new(),
            preview_visible: false,
            preview: None,
            preview_generation: 0,
            renaming: None,
            trash_ledger,
            sort: None,
            widths: ColumnWidths {
                name: DEFAULT_NAME_WIDTH,
                size: DEFAULT_SIZE_WIDTH,
                modified: DEFAULT_MODIFIED_WIDTH,
            },
            resizing: None,
            menu: None,
            menu_index: None,
        }
    }

    fn start_search(&mut self, query: String, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.query = query.clone();
        self.results.clear();
        self.selected = None;
        self.preview = None;
        self.preview_generation = self.preview_generation.wrapping_add(1);
        if query.trim().is_empty() {
            self.status = "输入名称即可搜索整个电脑".to_string();
            cx.notify();
            return;
        }
        self.status = "正在读取电脑索引…".to_string();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            // On a cold start the index is read back from the cache (or walked
            // again) and there is nothing to search for a moment. The window
            // keeps asking until it is ready rather than failing the query.
            for _ in 0..PREPARE_TRIES {
                let prepared = match this.update(cx, |_, cx| everything::prepare(cx)) {
                    Ok(prepared) => prepared,
                    Err(_) => return,
                };
                if !prepared.ready() {
                    cx.background_executor().timer(INDEX_POLL).await;
                    continue;
                }
                let query = query.clone();
                let outcome = cx
                    .background_spawn(async move { prepared.search(&query, LIMIT) })
                    .await;
                match outcome {
                    Ok(hits) => {
                        this.update(cx, |this, cx| {
                            if this.generation == generation {
                                this.apply_hits(hits, cx);
                            }
                        })
                        .ok();
                        return;
                    }
                    Err(WholeComputerError::NotReady) => {
                        cx.background_executor().timer(INDEX_POLL).await;
                    }
                    Err(WholeComputerError::Failed(error)) => {
                        this.update(cx, |this, cx| {
                            if this.generation == generation {
                                this.selected = None;
                                this.status = error;
                                cx.notify();
                            }
                        })
                        .ok();
                        return;
                    }
                }
            }
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.status = "电脑索引还在建立，稍后再试".to_string();
                    cx.notify();
                }
            })
            .ok();
        });
        self._search_task = Some(task);
        cx.notify();
    }

    /// Puts a finished whole-computer search on screen.
    fn apply_hits(&mut self, hits: Vec<WholeComputerHit>, cx: &mut Context<Self>) {
        let truncated = hits.len() >= LIMIT;
        self.results = hits
            .into_iter()
            .map(|hit| {
                SearchResult::from(IndexedSearchHit {
                    path: hit.path,
                    is_dir: hit.is_dir,
                    size: hit.size,
                    modified: hit.modified,
                })
            })
            .collect();
        self.sort_results();
        self.status = if self.results.is_empty() {
            "没有匹配的结果".to_string()
        } else if truncated {
            format!("{} 个结果（已显示前 {} 个）", self.results.len(), LIMIT)
        } else {
            format!("{} 个结果", self.results.len())
        };
        // A result list is keyboard-driven: without a selection there is
        // nothing for Enter, Alt+Q or the accelerators to act on.
        if self.results.is_empty() {
            self.selected = None;
        } else {
            self.selected = Some(0);
            self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Orders the list by the heading the user picked, keeping the selection
    /// on the same path.
    fn sort_results(&mut self) {
        let Some((key, ascending)) = self.sort else {
            return;
        };
        let selected_path = self
            .selected
            .and_then(|index| self.results.get(index))
            .map(|result| result.hit.path.clone());
        self.results.sort_by(|left, right| {
            let ordering = match key {
                SortKey::Name => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
                SortKey::Path => left
                    .hit
                    .path
                    .to_lowercase()
                    .cmp(&right.hit.path.to_lowercase()),
                SortKey::Size => left.hit.size.cmp(&right.hit.size),
                SortKey::Modified => left.hit.modified.cmp(&right.hit.modified),
            };
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        });
        if let Some(path) = selected_path {
            self.selected = self
                .results
                .iter()
                .position(|result| result.hit.path == path);
        }
    }

    /// Sorts by `key`, flipping the direction when it is already the one in use.
    fn toggle_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.sort = match self.sort {
            Some((current, ascending)) if current == key => Some((key, !ascending)),
            _ => Some((key, true)),
        };
        self.sort_results();
        self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
        self.refresh_preview(cx);
        cx.notify();
    }

    fn begin_column_resize(&mut self, column: ResizeColumn, position: Pixels) {
        let start_width = match column {
            ResizeColumn::Name => self.widths.name,
            ResizeColumn::Size => self.widths.size,
            ResizeColumn::Modified => self.widths.modified,
        };
        self.resizing = Some(ColumnResize {
            column,
            start_x: position,
            start_width,
        });
    }

    /// Follows the pointer while a divider is held. Returns whether a divider
    /// was being dragged, so the caller knows to repaint.
    fn drag_column(&mut self, position: Pixels) -> bool {
        let Some(resize) = &self.resizing else {
            return false;
        };
        let column = resize.column;
        let start_x = resize.start_x;
        let start_width = resize.start_width;
        let delta = f32::from(position - start_x);
        match column {
            ResizeColumn::Name => {
                self.widths.name = (start_width + delta).clamp(MIN_NAME_WIDTH, MAX_NAME_WIDTH);
            }
            // The path column is flexible, so a divider on the left edge of a
            // fixed column widens that column as it is dragged left.
            ResizeColumn::Size => {
                self.widths.size = (start_width - delta).clamp(MIN_SIZE_WIDTH, MAX_SIZE_WIDTH);
            }
            ResizeColumn::Modified => {
                self.widths.modified =
                    (start_width - delta).clamp(MIN_MODIFIED_WIDTH, MAX_MODIFIED_WIDTH);
            }
        }
        true
    }

    fn end_column_resize(&mut self) -> bool {
        self.resizing.take().is_some()
    }

    /// Moves the keyboard selection to `index`, scrolling it into view.
    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.results.is_empty() {
            return;
        }
        let index = index.min(self.results.len() - 1);
        self.selected = Some(index);
        // Non-strict: scrolls only when the row is off screen, so an arrow
        // press does not drag the list up by one row every time.
        self.scroll_handle
            .scroll_to_item_with_offset(index, ScrollStrategy::Top, 0);
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Opens the result the way a double-click does: a folder in LumaFiles,
    /// a file with the program Windows associates with it.
    fn open_result(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(index) else {
            return;
        };
        let path = PathBuf::from(&result.hit.path);
        if !result.hit.is_dir {
            cx.open_with_system(&path);
            return;
        }
        if !crate::explorer_link::request(path.clone()) {
            // No Explorer window to show it in (launcher-only run); the system
            // handler is the only thing left that can open a folder.
            self.status = "没有可用的 轻寻文件 窗口，改为在资源管理器中打开".to_string();
            cx.open_with_system(&path);
            cx.notify();
        }
    }

    fn reveal_result(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(index) else {
            return;
        };
        cx.reveal_path(Path::new(&result.hit.path));
    }

    /// Puts the result on the file clipboard, as the Explorer's own Copy and
    /// Cut do, so a Paste here or in Explorer.exe moves/copies it.
    fn copy_result(&mut self, index: usize, cut: bool, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(index) else {
            return;
        };
        let paths = vec![result.hit.path.clone()];
        let mode = if cut { ClipMode::Cut } else { ClipMode::Copy };
        match file_ops::write_paths_to_clipboard(mode, &paths, cx) {
            Ok(()) => {
                self.status = if cut {
                    "已剪切 1 项".to_string()
                } else {
                    "已复制 1 项".to_string()
                }
            }
            Err(error) => self.status = format!("无法写入剪贴板：{error}"),
        }
        cx.notify();
    }

    fn copy_text(&mut self, text: String, label: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status = format!("已复制{label}");
        cx.notify();
    }

    fn copy_path(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(index) else {
            return;
        };
        self.copy_text(result.hit.path.clone(), "路径", cx);
    }

    fn copy_name(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(index) else {
            return;
        };
        self.copy_text(result.name.clone(), "名字", cx);
    }

    /// Moves the result to the OS trash, through the same ledger the Explorer
    /// writes so the item can still be restored.
    fn trash_result(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(index) else {
            return;
        };
        let paths = vec![PathBuf::from(&result.hit.path)];
        let ledger = self.trash_ledger.clone();
        let task = cx.background_spawn(async move {
            let record = match ledger.to_record() {
                Ok(record) => record,
                Err(error) => return Err(format!("无法记录回收站来源：{error}")),
            };
            match ops::trash_paths(&paths, record).into_iter().next() {
                Some((path, error)) => Err(format!("{}：{error}", path.display())),
                None => Ok(()),
            }
        });
        cx.spawn(async move |this, cx| match task.await {
            Ok(()) => this
                .update(cx, |this, cx| {
                    this.status = "已移到回收站".to_string();
                    this.start_search(this.query.clone(), cx);
                })
                .ok(),
            Err(error) => this
                .update(cx, |this, cx| {
                    this.status = error;
                    cx.notify();
                })
                .ok(),
        })
        .detach();
    }

    /// Starts an inline rename of the result at `index`, over the list.
    fn begin_rename(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(index) else {
            return;
        };
        let original_path = PathBuf::from(&result.hit.path);
        let name = result.name.clone();
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
            state.focus(window, cx);
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_rename(window, cx),
                // Clicking anywhere else abandons the rename rather than
                // committing a name the user did not confirm.
                InputEvent::Blur => this.cancel_rename(window, cx),
                _ => {}
            },
        );
        self.renaming = Some(RenameState {
            original_path,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            self.input.update(cx, |state, cx| state.focus(window, cx));
            cx.notify();
        }
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.renaming.take() else {
            return;
        };
        let new_name = state.input.read(cx).value().trim().to_string();
        let original_path = state.original_path;
        let original_name = original_path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        self.input.update(cx, |state, cx| state.focus(window, cx));
        if new_name.is_empty() || new_name == original_name {
            cx.notify();
            return;
        }
        let task = cx.background_spawn(async move {
            // A name already taken by another entry is numbered instead of
            // overwritten, the way the Explorer's own rename does it.
            let final_name = match original_path.parent() {
                Some(parent)
                    if ops::destination_in(parent, OsStr::new(&new_name)).is_ok_and(|target| {
                        ops::would_collide_with_another(&original_path, &target)
                    }) =>
                {
                    ops::unique_name(parent, &new_name)
                }
                _ => new_name.clone(),
            };
            let renamed = ops::rename_in_place(&original_path, &final_name);
            (final_name, renamed)
        });
        cx.spawn(async move |this, cx| {
            let (final_name, renamed) = task.await;
            this.update(cx, |this, cx| match renamed {
                Ok(_) => {
                    this.status = format!("已重命名为 {final_name}");
                    this.start_search(this.query.clone(), cx);
                }
                Err(error) => {
                    this.status = format!("重命名失败：{error}");
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Reloads the preview pane for the current selection (or clears it when
    /// the pane is closed or nothing is selected).
    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_generation = self.preview_generation.wrapping_add(1);
        let generation = self.preview_generation;
        if !self.preview_visible {
            self.preview = None;
            return;
        }
        let Some(result) = self.selected.and_then(|index| self.results.get(index)) else {
            self.preview = None;
            return;
        };
        let path = PathBuf::from(&result.hit.path);
        self.preview = Some(Preview {
            path: path.clone(),
            kind: PreviewKind::Loading,
            size: result.hit.size,
            modified: result.hit.modified,
        });
        cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(async move { load_preview(path) }).await;
            this.update(cx, |this, cx| {
                if this.preview_generation != generation {
                    return;
                }
                this.preview = Some(loaded);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_preview(&self) -> AnyElement {
        let Some(preview) = &self.preview else {
            return div()
                .w(px(PREVIEW_WIDTH))
                .flex_shrink_0()
                .h_full()
                .border_l_1()
                .border_color(rgb(theme::border()))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child("选择一行以预览")
                .into_any_element();
        };
        let name = preview.path.file_name().map_or_else(
            || preview.path.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let body: AnyElement = match &preview.kind {
            PreviewKind::Loading => div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child("正在读取…")
                .into_any_element(),
            PreviewKind::Image => div()
                .flex_1()
                .min_h(px(0.0))
                .p(px(8.0))
                .child(
                    img(preview.path.clone())
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
            PreviewKind::Text { text, truncated } => div()
                .id("search-preview-text")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .p(px(10.0))
                .text_size(px(12.0))
                .line_height(px(16.0))
                .font_family("Mono")
                .text_color(rgb(theme::fg_secondary()))
                .child(text.clone())
                .when(*truncated, |this| {
                    this.child(
                        div()
                            .pt(px(8.0))
                            .text_xs()
                            .text_color(rgb(theme::muted()))
                            .child("（已截断）"),
                    )
                })
                .into_any_element(),
            PreviewKind::Folder { children } => div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(Icon::new(IconName::Folder).size_6())
                .child(div().text_sm().child(name.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::muted()))
                        .child(match children {
                            Some(count) if *count >= FOLDER_PREVIEW_LIMIT => {
                                format!("{FOLDER_PREVIEW_LIMIT}+ 个条目")
                            }
                            Some(count) => format!("{count} 个条目"),
                            None => "无法读取文件夹".to_string(),
                        }),
                )
                .into_any_element(),
            PreviewKind::Other { reason } => div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .p(px(16.0))
                .gap(px(8.0))
                .child(Icon::new(IconName::File).size_6())
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::muted()))
                        .child(reason.clone()),
                )
                .into_any_element(),
        };
        let metadata = if matches!(preview.kind, PreviewKind::Folder { .. }) {
            String::new()
        } else {
            format!(
                "{} · {}",
                format_bytes(preview.size),
                format_modified(preview.modified)
            )
        };
        div()
            .w(px(PREVIEW_WIDTH))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(theme::border()))
            .child(
                div()
                    .flex_shrink_0()
                    .h(px(34.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .border_b_1()
                    .border_color(rgb(theme::border()))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_xs()
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(rgb(theme::muted()))
                            .child("Alt+Q"),
                    ),
            )
            .child(body)
            .when(!metadata.is_empty(), |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .px(px(12.0))
                        .py(px(6.0))
                        .border_t_1()
                        .border_color(rgb(theme::border()))
                        .text_xs()
                        .text_color(rgb(theme::muted()))
                        .child(metadata),
                )
            })
            .into_any_element()
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.to_lowercase();
        let modifiers = event.keystroke.modifiers;
        // The drawn menu takes the keyboard while it is open: Esc closes it and
        // nothing behind it runs.
        if crate::drawn_menu::is_open(self) {
            if key == "escape" {
                crate::drawn_menu::close(self, cx);
            }
            cx.stop_propagation();
            return;
        }
        // While renaming, the field owns typing, Enter and Escape; only Escape
        // is taken here so it can close the dialog.
        if self.renaming.is_some() {
            if key == "escape" {
                self.cancel_rename(window, cx);
                cx.stop_propagation();
            }
            return;
        }
        let handled =
            if modifiers.alt && !modifiers.control && !modifiers.platform && !modifiers.shift {
                if key == "q" {
                    self.preview_visible = !self.preview_visible;
                    self.refresh_preview(cx);
                    cx.notify();
                    true
                } else if let Some(index) = accelerator_index(&key) {
                    if index < self.results.len() {
                        self.select(index, cx);
                        self.open_result(index, cx);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                match key.as_str() {
                    "escape" => {
                        window.remove_window();
                        true
                    }
                    "up" => {
                        if !self.results.is_empty() {
                            let next = self.selected.map_or(0, |index| index.saturating_sub(1));
                            self.select(next, cx);
                        }
                        true
                    }
                    "down" => {
                        if !self.results.is_empty() {
                            let next = match self.selected {
                                Some(index) => (index + 1).min(self.results.len() - 1),
                                None => 0,
                            };
                            self.select(next, cx);
                        }
                        true
                    }
                    "enter" => {
                        let index = self.selected.or(if self.results.is_empty() {
                            None
                        } else {
                            Some(0)
                        });
                        if let Some(index) = index {
                            self.open_result(index, cx);
                        }
                        true
                    }
                    _ => false,
                }
            };
        if handled {
            // The field must not also act on a key the list consumed: an arrow
            // would move the text caret as well as the selection.
            cx.stop_propagation();
        }
    }
}

impl From<IndexedSearchHit> for SearchResult {
    fn from(hit: IndexedSearchHit) -> Self {
        let path = Path::new(&hit.path);
        let name = path.file_name().map_or_else(
            || hit.path.clone(),
            |name| name.to_string_lossy().into_owned(),
        );
        let directory = path.parent().map_or_else(
            || hit.path.clone(),
            |parent| parent.to_string_lossy().into_owned(),
        );
        Self {
            hit,
            name,
            directory,
        }
    }
}

impl Render for SearchWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.results.clone();
        let selected = self.selected;
        let view = cx.entity().downgrade();
        let status = self.status.clone();
        let query = self.query.clone();
        let preview_visible = self.preview_visible;
        let preview = if preview_visible {
            Some(self.render_preview())
        } else {
            None
        };
        let rename = self.renaming.as_ref().map(|state| state.input.clone());

        let widths = self.widths;
        let sort = self.sort;

        let header = div()
            .h(px(30.0))
            .px(px(ROW_INSET))
            .flex()
            .items_center()
            .text_color(rgb(theme::muted()))
            .child(div().w(px(KEY_WIDTH)).child("键"))
            .child(div().w(px(ICON_WIDTH)))
            .child(column_header(
                "名称",
                SortKey::Name,
                Some(widths.name),
                false,
                active_sort(sort, SortKey::Name),
                cx,
            ))
            .child(column_divider(ResizeColumn::Name, cx))
            .child(column_header(
                "路径",
                SortKey::Path,
                None,
                true,
                active_sort(sort, SortKey::Path),
                cx,
            ))
            .child(column_divider(ResizeColumn::Size, cx))
            .child(column_header(
                "大小",
                SortKey::Size,
                Some(widths.size),
                false,
                active_sort(sort, SortKey::Size),
                cx,
            ))
            .child(column_divider(ResizeColumn::Modified, cx))
            .child(column_header(
                "修改时间",
                SortKey::Modified,
                Some(widths.modified),
                false,
                active_sort(sort, SortKey::Modified),
                cx,
            ));

        let mut body = div().flex_1().min_h(px(0.0)).flex().child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .child(header)
                .child(
                    uniform_list(
                        "indexed-search-results",
                        results.len(),
                        move |range: Range<usize>, _window, _cx: &mut App| {
                            range
                                .filter_map(|index| {
                                    results.get(index).map(|result| {
                                        let accelerator = accelerator(index).map(accelerator_label);
                                        render_row(
                                            result,
                                            index,
                                            selected == Some(index),
                                            accelerator,
                                            widths,
                                            &view,
                                        )
                                    })
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .flex_1()
                    .pb(px(12.0))
                    .track_scroll(self.scroll_handle.clone()),
                ),
        );
        if let Some(preview) = preview {
            body = body.child(preview);
        }

        let mut root =
            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .bg(rgb(theme::bg()))
                .text_color(rgb(theme::fg()))
                .capture_key_down(cx.listener(Self::on_key_down))
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                    if event.pressed_button != Some(MouseButton::Left) {
                        if this.end_column_resize() {
                            cx.notify();
                        }
                        return;
                    }
                    if this.drag_column(event.position.x) {
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        if this.end_column_resize() {
                            cx.notify();
                        }
                    }),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .p(px(14.0))
                        .border_b_1()
                        .border_color(rgb(theme::border()))
                        .child(
                            div()
                                .h(px(38.0))
                                .w_full()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .px(px(10.0))
                                .rounded(px(7.0))
                                .bg(rgb(theme::footer_bg()))
                                .border_1()
                                .border_color(rgb(theme::border()))
                                .child(Icon::new(IconName::Search).size_4())
                                .child(div().flex_1().child(
                                    Input::new(&self.input).appearance(false).cleanable(true),
                                )),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .h(px(32.0))
                        .px(px(16.0))
                        .flex()
                        .items_center()
                        .gap(px(20.0))
                        .text_xs()
                        .text_color(rgb(theme::fg_secondary()))
                        .child(if query.is_empty() {
                            "索引搜索".to_string()
                        } else {
                            query
                        })
                        .child(div().flex_1())
                        .child("Alt+键 打开 · Alt+Q 预览")
                        .child(div().text_color(rgb(theme::muted())).child(status)),
                )
                .child(body);

        // The menu goes over the list but under any dialog.
        if let Some(menu) = crate::drawn_menu::render(self, window, cx) {
            root = root.child(menu);
        }
        if let Some(rename_input) = rename {
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(0x00000066))
                    .child(
                        div()
                            .w(px(440.0))
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .p(px(16.0))
                            .rounded(px(10.0))
                            .border_1()
                            .border_color(rgb(theme::border()))
                            .bg(rgb(theme::bg()))
                            .child(div().text_sm().child("重命名"))
                            .child(
                                div()
                                    .h(px(38.0))
                                    .px(px(10.0))
                                    .flex()
                                    .items_center()
                                    .rounded(px(7.0))
                                    .bg(rgb(theme::footer_bg()))
                                    .border_1()
                                    .border_color(rgb(theme::border()))
                                    .child(
                                        div()
                                            .flex_1()
                                            .child(Input::new(&rename_input).appearance(false)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::muted()))
                                    .child("Enter 确认 · Esc 取消"),
                            ),
                    ),
            );
        }

        root
    }
}

/// The direction a heading is sorted in, when it is the active one.
fn active_sort(sort: Option<(SortKey, bool)>, key: SortKey) -> Option<bool> {
    match sort {
        Some((active, ascending)) if active == key => Some(ascending),
        _ => None,
    }
}

/// A heading: clicking it sorts the list by its column, and clicking it again
/// flips the direction.
fn column_header(
    label: &'static str,
    key: SortKey,
    width: Option<f32>,
    flexible: bool,
    sorted: Option<bool>,
    cx: &mut Context<SearchWindow>,
) -> impl IntoElement {
    let mut cell = div()
        .id(("search-heading", key as usize))
        .h_full()
        .flex()
        .items_center()
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_sort(key, cx)));
    if let Some(width) = width {
        cell = cell.w(px(width)).flex_shrink_0();
    }
    if flexible {
        cell = cell.flex_1().min_w(px(0.0));
    }
    cell.child(
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(if sorted.is_some() {
                        rgb(theme::fg())
                    } else {
                        rgb(theme::muted())
                    })
                    .child(label),
            )
            .when_some(sorted, |this, ascending| {
                this.child(
                    Icon::new(if ascending {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    })
                    .size_3()
                    .text_color(rgb(theme::accent())),
                )
            }),
    )
}

/// The space between two columns. Dragging it changes a column's width: the
/// name grows to the right, and the fixed columns on the right grow as their
/// divider is pulled left, the path column taking up the difference.
fn column_divider(column: ResizeColumn, cx: &mut Context<SearchWindow>) -> Div {
    div()
        .w(px(DIVIDER_WIDTH))
        .h_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .cursor_col_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                this.begin_column_resize(column, event.position.x);
                cx.stop_propagation();
            }),
        )
        .child(div().w(px(1.0)).h(px(14.0)).bg(rgb(theme::border())))
}

fn render_row(
    result: &SearchResult,
    index: usize,
    selected: bool,
    accelerator: Option<String>,
    widths: ColumnWidths,
    view: &WeakEntity<SearchWindow>,
) -> impl IntoElement + use<> {
    let path = result.directory.clone();
    let name = result.name.clone();
    let size = if result.hit.is_dir {
        "—".to_string()
    } else {
        format_bytes(result.hit.size)
    };
    let modified = format_modified(result.hit.modified);
    div()
        .id(("indexed-search-row", index))
        .h(px(42.0))
        .w_full()
        .flex()
        .items_center()
        .px(px(ROW_INSET))
        .rounded(px(6.0))
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme::bg_hover())))
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .on_click({
            let view = view.clone();
            move |event: &gpui::ClickEvent, _window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.select(index, cx);
                    if event.click_count() >= 2 {
                        this.open_result(index, cx);
                    }
                });
            }
        })
        .child(
            div()
                .w(px(KEY_WIDTH))
                .flex_shrink_0()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(accelerator.unwrap_or_default()),
        )
        .child(
            div()
                .w(px(ICON_WIDTH))
                .flex_shrink_0()
                .flex()
                .items_center()
                .child(
                    Icon::new(if result.hit.is_dir {
                        IconName::Folder
                    } else {
                        IconName::File
                    })
                    .size_4(),
                ),
        )
        .child(
            div()
                .w(px(widths.name))
                .flex_shrink_0()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(name),
        )
        .child(div().w(px(DIVIDER_WIDTH)).flex_shrink_0())
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(rgb(theme::fg_secondary()))
                .child(path),
        )
        .child(div().w(px(DIVIDER_WIDTH)).flex_shrink_0())
        .child(
            div()
                .w(px(widths.size))
                .flex_shrink_0()
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child(size),
        )
        .child(div().w(px(DIVIDER_WIDTH)).flex_shrink_0())
        .child(
            div()
                .w(px(widths.modified))
                .flex_shrink_0()
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child(modified),
        )
        .on_mouse_down(MouseButton::Right, {
            // Right-clicking a row is also selecting it, so the menu items and
            // the preview always describe the row the menu was opened on.
            let view = view.clone();
            move |event: &MouseDownEvent, _window, cx| {
                view.update(cx, |this, cx| {
                    this.menu_index = Some(index);
                    if this.selected != Some(index) {
                        this.selected = Some(index);
                        this.refresh_preview(cx);
                    }
                    crate::drawn_menu::open_at(this, event.position, result_menu_rows(), cx);
                })
                .ok();
            }
        })
}

/// Reads what the preview pane shows. Runs on the background executor: the
/// window is open and searchable while a large file is being read.
fn load_preview(path: PathBuf) -> Preview {
    let metadata = std::fs::metadata(&path);
    let (size, modified) = match &metadata {
        Ok(metadata) => (
            metadata.len(),
            metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |elapsed| {
                    u32::try_from(elapsed.as_secs()).unwrap_or(u32::MAX)
                }),
        ),
        Err(_) => (0, 0),
    };
    let kind = match &metadata {
        Ok(metadata) if metadata.is_dir() => PreviewKind::Folder {
            children: folder_children(&path),
        },
        Ok(metadata) if metadata.is_file() && is_image(&path) => {
            if metadata.len() > MAX_PREVIEW_IMAGE_BYTES {
                PreviewKind::Other {
                    reason: "图片过大，未预览".to_string(),
                }
            } else {
                PreviewKind::Image
            }
        }
        Ok(_) if path.is_file() => read_text_preview(&path),
        Ok(_) => PreviewKind::Other {
            reason: "无法预览".to_string(),
        },
        Err(error) => PreviewKind::Other {
            reason: format!("无法读取：{error}"),
        },
    };
    Preview {
        path,
        kind,
        size,
        modified,
    }
}

fn folder_children(path: &Path) -> Option<usize> {
    let entries = std::fs::read_dir(path).ok()?;
    let mut count = 0;
    for _ in entries.flatten() {
        count += 1;
        if count >= FOLDER_PREVIEW_LIMIT {
            break;
        }
    }
    Some(count)
}

fn is_image(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
    )
}

fn read_text_preview(path: &Path) -> PreviewKind {
    use std::io::Read as _;

    let Ok(file) = std::fs::File::open(path) else {
        return PreviewKind::Other {
            reason: "无法打开文件".to_string(),
        };
    };
    let mut buffer = Vec::new();
    if let Err(error) = file.take(TEXT_PREVIEW_BYTES).read_to_end(&mut buffer) {
        return PreviewKind::Other {
            reason: format!("读取失败：{error}"),
        };
    }
    if buffer.contains(&0) {
        return PreviewKind::Other {
            reason: "二进制文件，无法预览".to_string(),
        };
    }
    if buffer.is_empty() {
        return PreviewKind::Text {
            text: "（空文件）".to_string(),
            truncated: false,
        };
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut truncated = buffer.len() as u64 >= TEXT_PREVIEW_BYTES;
    let mut shown = String::new();
    for line in text.lines().take(TEXT_PREVIEW_LINES) {
        shown.push_str(line);
        shown.push('\n');
    }
    if text.lines().count() > TEXT_PREVIEW_LINES {
        truncated = true;
    }
    if shown.is_empty() {
        shown.push_str(&text);
    }
    PreviewKind::Text {
        text: shown,
        truncated,
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes)
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_modified(seconds: u32) -> String {
    // Same clock as the explorer's 修改时间 column: the index stores seconds
    // since the Unix epoch, which is UTC.
    nohrs_ui::components::file_list::format_date(&u64::from(seconds))
}

#[derive(Default)]
struct OpenSearchWindow(Option<WindowHandle<Root>>);
impl gpui::Global for OpenSearchWindow {}

/// Opens the search window, or closes it when it is already open.
pub fn toggle(cx: &mut App) -> anyhow::Result<()> {
    let existing = cx
        .try_global::<OpenSearchWindow>()
        .and_then(|state| state.0);
    if let Some(handle) = existing.filter(|handle| {
        cx.windows()
            .iter()
            .any(|window| window.window_id() == handle.window_id())
    }) {
        cx.set_global(OpenSearchWindow(None));
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        });
        return Ok(());
    }
    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(SharedString::from("轻寻文件 搜索")),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        // Its own class, so a script can drive the search window without
        // matching the explorer's (`nohrs_core::window_class` names them all).
        window_class: Some(nohrs_core::window_class::SEARCH),
        is_movable: true,
        is_resizable: true,
        is_minimizable: true,
        display_id: None,
        window_background: gpui::WindowBackgroundAppearance::Opaque,
        app_id: None,
        window_min_size: Some(size(px(720.0), px(400.0))),
        window_decorations: None,
        tabbing_identifier: None,
    };
    let ledger = Ledger::open();
    let handle = cx.open_window(options, |window, cx| {
        let search = cx.new(|cx| SearchWindow::new(window, cx, ledger));
        cx.new(|cx| Root::new(search, window, cx))
    })?;
    cx.set_global(OpenSearchWindow(Some(handle)));
    Ok(())
}

#[cfg(test)]
// Tests build fixture files; there is no UI thread here.
#[allow(clippy::disallowed_methods)]
mod tests {
    use gpui::{
        AppContext as _, Entity, KeyDownEvent, Keystroke, Modifiers, TestAppContext, WindowHandle,
        px,
    };

    use super::{
        ACCELERATOR_DIGITS, ACCELERATOR_LETTERS, DEFAULT_NAME_WIDTH, DEFAULT_SIZE_WIDTH,
        MIN_NAME_WIDTH, ResizeColumn, SearchResult, SortKey, accelerator, accelerator_index,
        accelerator_label, format_bytes, is_image,
    };
    use crate::explorer::IndexedSearchHit;

    /// A search window in a test window, wrapped in the `gpui_component::Root`
    /// it renders inside in the real app, with its result list filled directly:
    /// the tests are about the window's own behaviour, not about the resident
    /// index behind the search.
    struct Harness {
        window: WindowHandle<gpui_component::Root>,
        search: Entity<super::SearchWindow>,
    }

    fn harness(cx: &mut TestAppContext, paths: &[(&str, bool)]) -> Harness {
        cx.update(gpui_component::init);
        let mut built = None;
        let window = cx.add_window(|window, cx| {
            let search = cx.new(|cx| super::SearchWindow::new(window, cx, super::Ledger::open()));
            built = Some(search.clone());
            gpui_component::Root::new(search, window, cx)
        });
        let search = built.expect("the search view should be built with the window");
        search.update(cx, |view, cx| {
            view.results = paths
                .iter()
                .map(|(path, is_dir)| {
                    SearchResult::from(IndexedSearchHit {
                        path: (*path).to_string(),
                        is_dir: *is_dir,
                        size: 0,
                        modified: 0,
                    })
                })
                .collect();
            view.selected = view.results.first().map(|_| 0);
            cx.notify();
        });
        Harness { window, search }
    }

    fn press(harness: &Harness, cx: &mut TestAppContext, key: &str, modifiers: Modifiers) {
        let event = KeyDownEvent {
            keystroke: Keystroke {
                key: key.to_string(),
                modifiers,
                key_char: None,
            },
            is_held: false,
        };
        harness
            .window
            .update(cx, |_root, window, cx| {
                harness
                    .search
                    .update(cx, |view, cx| view.on_key_down(&event, window, cx))
            })
            .expect("the search window should be open");
    }

    fn result_names(harness: &Harness, cx: &mut TestAppContext) -> Vec<String> {
        harness.search.read_with(cx, |view, _| {
            view.results
                .iter()
                .map(|result| result.name.clone())
                .collect()
        })
    }

    #[test]
    fn formats_sizes_for_result_rows() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    #[test]
    fn result_rows_separate_the_name_from_its_parent_path() {
        let path = std::path::Path::new("parent").join("report.txt");
        let result = super::SearchResult::from(IndexedSearchHit {
            path: path.to_string_lossy().into_owned(),
            is_dir: false,
            size: 12,
            modified: 0,
        });

        assert_eq!(result.name, "report.txt");
        assert_eq!(result.directory, "parent");
    }

    #[gpui::test]
    fn arrows_move_the_selection_and_stop_at_the_ends(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let files: Vec<String> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|name| {
                let path = directory.path().join(name);
                std::fs::write(&path, "x").unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        let paths: Vec<(&str, bool)> = files.iter().map(|path| (path.as_str(), false)).collect();
        let search = harness(cx, &paths);

        press(&search, cx, "down", Modifiers::default());
        search
            .search
            .read_with(cx, |view, _| assert_eq!(view.selected, Some(1)));
        press(&search, cx, "down", Modifiers::default());
        press(&search, cx, "down", Modifiers::default());
        search
            .search
            .read_with(cx, |view, _| assert_eq!(view.selected, Some(2)));
        press(&search, cx, "up", Modifiers::default());
        search
            .search
            .read_with(cx, |view, _| assert_eq!(view.selected, Some(1)));
    }

    #[gpui::test]
    fn alt_q_previews_the_selection_and_alt_q_closes_it(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("notes.txt");
        std::fs::write(&file, "alpha\nbeta\n").unwrap();
        let path = file.to_string_lossy().into_owned();
        let search = harness(cx, &[(&path, false)]);
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };

        press(&search, cx, "q", alt);
        // The preview's read runs on the background executor: let the
        // foreground task schedule it, the read finish, and the answer land.
        cx.run_until_parked();
        cx.background_executor.run_until_parked();
        cx.run_until_parked();
        search.search.read_with(cx, |view, _| {
            assert!(view.preview_visible);
            assert!(matches!(
                view.preview.as_ref().map(|preview| &preview.kind),
                Some(super::PreviewKind::Text { text, .. }) if text.contains("alpha")
            ));
        });

        press(&search, cx, "q", alt);
        search.search.read_with(cx, |view, _| {
            assert!(!view.preview_visible);
            assert!(view.preview.is_none());
        });
    }

    #[gpui::test]
    fn clicking_a_heading_sorts_the_list_both_ways(cx: &mut TestAppContext) {
        let harness = harness(
            cx,
            &[
                ("C:/b.txt", false),
                ("C:/a.txt", false),
                ("C:/c.txt", false),
            ],
        );
        harness
            .search
            .update(cx, |view, cx| view.toggle_sort(SortKey::Name, cx));
        assert_eq!(result_names(&harness, cx), ["a.txt", "b.txt", "c.txt"]);
        harness
            .search
            .update(cx, |view, cx| view.toggle_sort(SortKey::Name, cx));
        assert_eq!(result_names(&harness, cx), ["c.txt", "b.txt", "a.txt"]);
    }

    #[gpui::test]
    fn dragging_a_divider_resizes_a_column_within_limits(cx: &mut TestAppContext) {
        let harness = harness(cx, &[]);
        harness.search.update(cx, |view, _| {
            view.begin_column_resize(ResizeColumn::Name, px(100.0));
            assert!(view.drag_column(px(150.0)));
            assert_eq!(view.widths.name, DEFAULT_NAME_WIDTH + 50.0);
            assert!(view.end_column_resize());
            // A divider on a fixed column's left edge widens that column as it
            // is dragged left, the flexible path column taking the difference.
            view.begin_column_resize(ResizeColumn::Size, px(200.0));
            assert!(view.drag_column(px(180.0)));
            assert_eq!(view.widths.size, DEFAULT_SIZE_WIDTH + 20.0);
            view.end_column_resize();
            view.begin_column_resize(ResizeColumn::Name, px(0.0));
            view.drag_column(px(-10_000.0));
            assert_eq!(view.widths.name, MIN_NAME_WIDTH);
            assert!(view.end_column_resize());
        });
    }

    #[gpui::test]
    fn escape_closes_the_window(cx: &mut TestAppContext) {
        let search = harness(cx, &[]);
        press(&search, cx, "escape", Modifiers::default());
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| cx.windows().len()), 0);
    }

    #[test]
    fn accelerators_cover_digits_then_letters_without_q() {
        assert_eq!(accelerator(0), Some('1'));
        assert_eq!(accelerator(9), Some('0'));
        assert_eq!(accelerator(10), Some('a'));
        // `q` is the preview toggle, not the 27th result.
        assert_eq!(accelerator(10 + 16), Some('r'));
        assert_eq!(
            accelerator(ACCELERATOR_DIGITS.len() + ACCELERATOR_LETTERS.len() - 1),
            Some('z')
        );
        assert_eq!(
            accelerator(ACCELERATOR_DIGITS.len() + ACCELERATOR_LETTERS.len()),
            None
        );
    }

    #[test]
    fn accelerators_map_back_to_their_position() {
        assert_eq!(accelerator_index("1"), Some(0));
        assert_eq!(accelerator_index("0"), Some(9));
        assert_eq!(accelerator_index("a"), Some(10));
        assert_eq!(accelerator_index("z"), Some(34));
        assert_eq!(accelerator_index("q"), None);
        assert_eq!(accelerator_index("ab"), None);
        assert_eq!(accelerator_label('a'), "A");
        assert_eq!(accelerator_label('1'), "1");
    }

    #[test]
    fn preview_treats_image_extensions_case_insensitively() {
        assert!(is_image(std::path::Path::new("a.PNG")));
        assert!(is_image(std::path::Path::new("a.jpeg")));
        assert!(!is_image(std::path::Path::new("a.txt")));
        assert!(!is_image(std::path::Path::new("no-extension")));
    }

    #[test]
    fn preview_reads_the_start_of_a_text_file() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("notes.txt");
        std::fs::write(&file, "alpha\nbeta\n").unwrap();
        match super::load_preview(file).kind {
            super::PreviewKind::Text { text, truncated } => {
                assert!(text.contains("alpha"));
                assert!(!truncated);
            }
            _ => panic!("a text file should preview as text"),
        }
    }

    #[test]
    fn preview_counts_a_folder_and_refuses_binary_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("one.txt"), "1").unwrap();
        std::fs::write(directory.path().join("two.txt"), "2").unwrap();
        match super::load_preview(directory.path().to_path_buf()).kind {
            super::PreviewKind::Folder { children } => assert_eq!(children, Some(2)),
            _ => panic!("a folder should preview as a folder"),
        }
        let binary = directory.path().join("blob.bin");
        std::fs::write(&binary, [0u8, 1, 2, 3]).unwrap();
        assert!(matches!(
            super::load_preview(binary).kind,
            super::PreviewKind::Other { .. }
        ));
    }
}
