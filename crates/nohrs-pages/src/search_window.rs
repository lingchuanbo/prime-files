//! Standalone Everything-style search window.
//!
//! This view deliberately keeps only the small hit records returned by the
//! indexer. It does not walk the filesystem or duplicate the Explorer index.

use std::ops::Range;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{App, AppContext, Bounds, Context, Entity, Render, SharedString,
    Subscription, Task, Window, WindowBounds, WindowHandle, WindowKind, WindowOptions, div,
    px, rgb, size, uniform_list};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{Icon, IconName, Root};
use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use nohrs_ui::theme::theme;

use crate::explorer::{self, IndexedSearchHit};

const WIDTH: f32 = 980.0;
const HEIGHT: f32 = 650.0;
const LIMIT: u32 = 20_000;
const DEBOUNCE: Duration = Duration::from_millis(70);

#[derive(Clone, Debug)]
struct SearchResult {
    hit: IndexedSearchHit,
    name: String,
    directory: String,
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
}

impl SearchWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            state.set_placeholder("搜索文件和文件夹…", window, cx);
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
        }
    }

    fn start_search(&mut self, query: String, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.query = query.clone();
        self.results.clear();
        self.selected = None;
        if query.trim().is_empty() {
            self.status = "输入名称即可搜索整个电脑".to_string();
            cx.notify();
            return;
        }
        self.status = "正在搜索索引…".to_string();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let result = cx
                .background_spawn(async move { explorer::search_indexed(&query, LIMIT) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok((hits, truncated)) => {
                        this.results = hits.into_iter().map(SearchResult::from).collect();
                        this.status = if truncated {
                            format!("{} 个结果（已显示前 {} 个）", this.results.len(), LIMIT)
                        } else {
                            format!("{} 个结果", this.results.len())
                        };
                    }
                    Err(error) => {
                        this.status = error;
                    }
                }
                cx.notify();
            });
        });
        self._search_task = Some(task);
        cx.notify();
    }

    fn open_result(&self, index: usize, cx: &mut Context<Self>) {
        if let Some(result) = self.results.get(index) {
            cx.open_with_system(std::path::Path::new(&result.hit.path));
        }
    }

    fn reveal_result(&self, index: usize, cx: &mut Context<Self>) {
        if let Some(result) = self.results.get(index) {
            cx.reveal_path(std::path::Path::new(&result.hit.path));
        }
    }
}

impl From<IndexedSearchHit> for SearchResult {
    fn from(hit: IndexedSearchHit) -> Self {
        let path = std::path::Path::new(&hit.path);
        let name = path.file_name().map_or_else(|| hit.path.clone(), |name| name.to_string_lossy().into_owned());
        let directory = path.parent().map_or_else(|| hit.path.clone(), |parent| parent.to_string_lossy().into_owned());
        Self { hit, name, directory }
    }
}

impl Render for SearchWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.results.clone();
        let selected = self.selected;
        let view = cx.entity().downgrade();
        let status = self.status.clone();
        let query = self.query.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::bg()))
            .text_color(rgb(theme::fg()))
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
                            .child(div().flex_1().child(Input::new(&self.input).appearance(false).cleanable(true))),
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
                    .child(if query.is_empty() { "索引搜索".to_string() } else { query })
                    .child(div().flex_1())
                    .child(status),
            )
            .child(
                div().h(px(30.0)).px(px(22.0)).flex().items_center().gap(px(10.0))
                    .text_xs().text_color(rgb(theme::muted()))
                    .child(div().w(px(16.0)))
                    .child(div().w(px(230.0)).child("名称"))
                    .child(div().flex_1().child("路径"))
                    .child(div().w(px(90.0)).child("大小"))
                    .child(div().w(px(160.0)).child("修改时间")),
            )
            .child(uniform_list("indexed-search-results", results.len(), move |range: Range<usize>, _window, _cx: &mut App| {
                range.filter_map(|index| {
                    results.get(index).map(|result| render_row(result, index, selected == Some(index), &view))
                }).collect::<Vec<_>>()
            }).flex_1().px(px(12.0)).pb(px(12.0)))
    }
}

fn render_row(result: &SearchResult, index: usize, selected: bool, view: &gpui::WeakEntity<SearchWindow>) -> impl IntoElement + use<> {
    let path = result.directory.clone();
    let name = result.name.clone();
    let size = if result.hit.is_dir { "—".to_string() } else { format_bytes(result.hit.size) };
    let modified = format_modified(result.hit.modified);
    let open_view = view.clone();
    let reveal_view = view.clone();
    div()
        .id(("indexed-search-row", index))
        .h(px(42.0))
        .w_full()
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(10.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme::bg_hover())))
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .on_click({ let view = view.clone(); move |event: &gpui::ClickEvent, _window, cx| { let _ = view.update(cx, |this, cx| { this.selected = Some(index); if event.click_count() >= 2 { this.open_result(index, cx); } cx.notify(); }); } })
        .child(Icon::new(if result.hit.is_dir { IconName::Folder } else { IconName::File }).size_4())
        .child(div().w(px(230.0)).min_w(px(0.0)).overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
        .child(div().flex_1().min_w(px(0.0)).overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(rgb(theme::fg_secondary())).child(path))
        .child(div().w(px(90.0)).text_xs().text_color(rgb(theme::fg_secondary())).child(size))
        .child(div().w(px(160.0)).text_xs().text_color(rgb(theme::fg_secondary())).child(modified))
        .context_menu(move |menu: PopupMenu, _window, _cx| {
            menu.item(PopupMenuItem::new("打开").on_click({ let view = open_view.clone(); move |_, _, cx| { let _ = view.update(cx, |this, cx| this.open_result(index, cx)); } }))
                .item(PopupMenuItem::new("在资源管理器中显示").on_click({ let view = reveal_view.clone(); move |_, _, cx| { let _ = view.update(cx, |this, cx| this.reveal_result(index, cx)); } }))
        })
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{} B", bytes) } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn format_modified(seconds: u32) -> String {
    time::OffsetDateTime::from_unix_timestamp(i64::from(seconds))
        .ok()
        .and_then(|value| value.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_else(|| "—".to_string())
}

#[derive(Default)]
struct OpenSearchWindow(Option<WindowHandle<Root>>);
impl gpui::Global for OpenSearchWindow {}

/// Opens the search window, or closes it when it is already open.
pub fn toggle(cx: &mut App) -> anyhow::Result<()> {
    let existing = cx.try_global::<OpenSearchWindow>().and_then(|state| state.0.clone());
    if let Some(handle) = existing.filter(|handle| cx.windows().iter().any(|window| window.window_id() == handle.window_id())) {
        cx.set_global(OpenSearchWindow(None));
        cx.defer(move |cx| { let _ = handle.update(cx, |_, window, _| window.remove_window()); });
        return Ok(());
    }
    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions { title: Some(SharedString::from("Prime Files 搜索")), appears_transparent: false, traffic_light_position: None }),
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        is_movable: true,
        is_resizable: true,
        is_minimizable: true,
        display_id: None,
        window_background: gpui::WindowBackgroundAppearance::Opaque,
        app_id: None,
        window_min_size: Some(size(px(640.0), px(400.0))),
        window_decorations: None,
        tabbing_identifier: None,
    };
    let handle = cx.open_window(options, |window, cx| {
        let search = cx.new(|cx| SearchWindow::new(window, cx));
        cx.new(|cx| Root::new(search, window, cx))
    })?;
    cx.set_global(OpenSearchWindow(Some(handle)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SearchResult, format_bytes};
    use crate::explorer::IndexedSearchHit;

    #[test]
    fn formats_sizes_for_result_rows() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    #[test]
    fn result_rows_separate_the_name_from_its_parent_path() {
        let path = std::path::Path::new("parent").join("report.txt");
        let result = SearchResult::from(IndexedSearchHit {
            path: path.to_string_lossy().into_owned(),
            is_dir: false,
            size: 12,
            modified: 0,
        });

        assert_eq!(result.name, "report.txt");
        assert_eq!(result.directory, "parent");
    }
}
