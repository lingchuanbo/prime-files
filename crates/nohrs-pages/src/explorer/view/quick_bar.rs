use nohrs_core::{t, tf};
use nohrs_core::i18n;
use crate::explorer::entries::FolderOrder;
use crate::explorer::filter::KindFilter;
use gpui_component::tooltip::Tooltip;
use crate::explorer::types::StatusLevel;
use crate::explorer::{ExplorerPane, zoom};
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::InputState;
use gpui_component::{Icon, IconName};
use nohrs_ui::theme::theme;

/// 底栏左侧显示搜索框，右侧显示状态、计数与视图缩放控件。
pub fn render(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    let files = page.entries.iter().filter(|entry| entry.kind != "dir").count();
    let search = super::quick_search::render(page, window, cx);
    // A file operation's report, a search's progress, or nothing.
    let everything_status = page.everything_status(cx);
    let (status, status_color) = match (page.status_for_footer(), &page.deep_search) {
        (None, _) if everything_status.is_some() => {
            (everything_status.unwrap_or_default(), theme::fg_secondary())
        }
        (Some((text, level)), _) => (
            text,
            match level {
                StatusLevel::Info => theme::fg_secondary(),
                StatusLevel::Warning => theme::warning(),
                StatusLevel::Error => theme::danger(),
            },
        ),
        (None, Some(search)) if search.running => {
            (tf!("Buscando… {n}", n = search.found), theme::fg_secondary())
        }
        (None, Some(search)) if search.detail.is_empty() => {
            (tf!("{n} encontrados", n = search.found), theme::fg_secondary())
        }
        (None, Some(search)) => (
            format!("{} · {}", tf!("{n} encontrados", n = search.found), search.detail),
            theme::fg_secondary(),
        ),
        (None, None) => (String::new(), theme::fg_secondary()),
    };

    let is_file = |entry: &&nohrs_services::fs::listing::FileEntryDto| entry.kind != "dir";
    let folders_shown = page.filtered_entries.iter().filter(|entry| entry.kind == "dir").count();
    let folders_total = page.entries.iter().filter(|entry| entry.kind == "dir").count();
    let files_shown = page.filtered_entries.iter().filter(is_file).count();
    // Search results come from other folders: against this folder's totals
    // they would read as "1121 of 272".
    let searching = page.search_results.is_some();
    let ratio = |shown: usize, total: usize| {
        if searching || shown == total {
            short_count(shown)
        } else {
            format!("{}/{}", short_count(shown), short_count(total))
        }
    };
    let folders = ratio(folders_shown, folders_total);
    let files = ratio(files_shown, files);
    let selected = page.selection.len();
    let kind = page.kind_filter;
    let framed = page.framed.bottom;

    div()
        .id("quick-bar")
        .flex_shrink_0()
        .w_full()
        // Out of compact mode's frame it takes in the frame's band below it,
        // and centres its contents in the whole.
        .h(px(32.0 + if framed { page.compact.frame_px as f32 } else { 0.0 }))
        // Out of compact mode's frame, the same room on both ends, clear of
        // the frame's sides.
        .pl(px(if framed { 8.0 } else { 6.0 }))
        .pr(px(8.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .when(!framed, |this| this.bg(rgb(theme::footer_bg())).border_t_1())
        .border_color(rgb(theme::border()))
        .child(search)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_xs()
                .text_color(rgb(status_color))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(status),
        )
        .when(selected > 0, |bar| {
            bar.child(
                bar_button("bar-selected", Icon::new(IconName::Check), tf!("{n} selecionados", n = selected), false)
                    // Clicking the count clears the selection.
                    .on_click(cx.listener(|page, _, _, cx| {
                        page.clear_selection();
                        cx.notify();
                    })),
            )
            .child(divider())
        })
        .child(
            // Opens or closes the terminal under the explorer, in this tab's
            // folder: the same action as Ctrl+', which the page above every
            // tab answers.
            bar_button(
                "bar-terminal",
                Icon::new(Icon::empty()).path(SharedString::from("icons/square-terminal.svg")),
                String::new(),
                false,
            )
            .tooltip(|window, cx| {
                Tooltip::new(nohrs_core::shortcuts::with_keys("Terminal", nohrs_core::shortcuts::Action::ToggleTerminal))
                    .build(window, cx)
            })
            .on_click(cx.listener(|_page, _, window, cx| {
                window.dispatch_action(Box::new(crate::explorer::page::ToggleTerminal), cx);
            })),
        )
        .child(divider())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .child(
                    bar_button("bar-folders", Icon::new(IconName::Folder), folders, kind == KindFilter::Folders)
                        // As in File Pilot: the folder count shows folders
                        // only, and a second click shows everything again.
                        .on_click(cx.listener(|page, _, _, cx| page.toggle_kind_only(KindFilter::Folders, cx))),
                )
                .child(
                    bar_button("bar-files", Icon::new(IconName::File), files, kind == KindFilter::Files)
                        .on_click(cx.listener(|page, _, _, cx| page.toggle_kind_only(KindFilter::Files, cx))),
                ),
        )
        .child(divider())
        .child({
            let order = page.folder_order;
            let (icon, tip) = match order {
                FolderOrder::First => ("icons/folder-up.svg", t!("Pastas primeiro, depois arquivos")),
                FolderOrder::Last => ("icons/folder-down.svg", t!("Arquivos primeiro, depois pastas")),
                FolderOrder::Mixed => ("icons/folder.svg", t!("Pastas e arquivos misturados")),
            };
            bar_button(
                "bar-folder-order",
                Icon::new(Icon::empty()).path(SharedString::from(icon)),
                String::new(),
                false,
            )
            .tooltip(move |window, cx| {
                Tooltip::new(tf!("{tip}. Clique para alternar.", tip = tip)).build(window, cx)
            })
            .on_click(cx.listener(|page, _, _, cx| {
                page.folder_order = page.folder_order.next();
                page.resort();
                cx.notify();
            }))
        })
        .child(
            // A menu of the kinds, File Pilot's filter switch. Lit whenever
            // something is hidden by a filter, so a short listing never looks
            // like a folder that is simply short.
            bar_button(
                "bar-filter",
                Icon::new(Icon::empty()).path(SharedString::from("icons/funnel.svg")),
                String::new(),
                page.is_filtering() || page.filter_bar_visible,
            )
            .on_click(cx.listener(|page, _, window, cx| page.open_filter_menu(window, cx))),
        )
        .child(divider())
        .child(render_zoom_switch(page, cx))
}

// A count or switch in the bar, as File Pilot's: an icon and its number,
// lit while what it switches is on.
fn bar_button(
    id: &'static str,
    icon: Icon,
    text: String,
    on: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .h(px(22.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .rounded(px(4.0))
        .text_xs()
        .cursor_pointer()
        .hover(|button| button.bg(rgb(theme::bg_hover())))
        .text_color(rgb(if on { theme::accent() } else { theme::fg_secondary() }))
        .child(icon.size_3p5().text_color(rgb(if on { theme::accent() } else { theme::muted() })))
        .when(!text.is_empty(), |button| button.child(text))
}

// The hairline between the bar's groups.
fn divider() -> impl IntoElement {
    div().w(px(1.0)).h(px(14.0)).flex_shrink_0().bg(rgb(theme::border()))
}

/// A count shortened the way File Pilot writes it: 7 600 is "7.6K".
fn short_count(count: usize) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{:.1}K", count as f32 / 1_000.0).replace(".0K", "K"),
        _ => format!("{:.1}M", count as f32 / 1_000_000.0).replace(".0M", "M"),
    }
}

// The current view and its zoom, opening the menu of views with a vertical
// slider above it.
fn render_zoom_switch(
    page: &mut ExplorerPane,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    let percent = (page.zoom * 100.0).round();
    let position = zoom::slider_position(page.zoom);
    let open = page.zoom_menu_open;
    let current = zoom::label(page.zoom);
    div()
        .relative()
        .flex_shrink_0()
        .child(
            div()
                .id("zoom-switch")
                .h(px(22.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .pl(px(8.0))
                .rounded(px(4.0))
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .when(open, |style| style.bg(rgb(theme::bg_hover())))
                .on_click(cx.listener(|page, _, _, cx| {
                    // A shown-for-a-moment menu clicked stays, as a clicked one.
                    page.zoom_menu_open = page.zoom_menu_flash.take().is_some() || !page.zoom_menu_open;
                    cx.notify();
                }))
                .child(view_icon(current).size_3p5().text_color(rgb(theme::muted())))
                .child(i18n::tr(current))
                .child(
                    div()
                        .h_full()
                        .px(px(7.0))
                        .flex()
                        .items_center()
                        .border_l_2()
                        .border_color(rgb(theme::accent()))
                        .rounded_r(px(4.0))
                        .bg(rgb(theme::accent_subtle()))
                        .text_color(rgb(theme::fg()))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{percent}%")),
                ),
        )
        .when(open, |this| this.child(deferred(render_zoom_menu(current, position, cx))))
}

// Height of the slider's travel in the menu, and of one view's row.
const ROW: f32 = 26.0;
const ROW_GAP: f32 = 4.0;
const TRACK: f32 = ROW * zoom::MENU_STOPS.len() as f32
    + ROW_GAP * (zoom::MENU_STOPS.len() - 1) as f32;
// The slider runs from the middle of the top row to the middle of the bottom
// one, so the knob is level with the row it stands for.
const INSET: f32 = ROW / 2.0;
// Width of the slider's column; the track and the knob are centered in it.
const TRACK_WIDTH: f32 = KNOB * 2.0 + 2.0;
// The knob's radius: the slider's travel starts and ends this far inside it.
const KNOB: f32 = 8.0;

// The menu of views, File Pilot's: each view's row sits level with the
// slider position that picks it, so the knob points at the view it shows.
fn render_zoom_menu(
    current: &'static str,
    position: f32,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    let rows: Vec<AnyElement> = zoom::MENU_STOPS
        .into_iter()
        .rev()
        .map(|(name, preset)| preset_row(name, preset, name == current, cx).into_any_element())
        .collect();
    div()
        .id("zoom-menu")
        .absolute()
        .bottom(px(30.0))
        .right_0()
        .w(px(168.0))
        .p(px(6.0))
        .flex()
        .gap(px(8.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(rgb(theme::border()))
        .bg(rgb(theme::bg_secondary()))
        .shadow_xl()
        .on_mouse_down_out(cx.listener(|page, _, _, cx| {
            page.zoom_menu_open = false;
            page.zoom_menu_flash = None;
            page.zoom_menu_hovered = false;
            cx.notify();
        }))
        .on_hover(cx.listener(|page, hovered: &bool, _, _cx| {
            page.zoom_menu_hovered = *hovered;
        }))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(ROW_GAP))
                .children(rows),
        )
        .child(zoom_track(position, cx))
}

// The menu's slider: a thin track, filled in the accent colour up to the
// knob. Pressing anywhere on it jumps there and dragging follows the pointer;
// near a view's row it snaps to that view, so picking one by dragging lands
// on it exactly.
fn zoom_track(position: f32, cx: &mut Context<ExplorerPane>) -> impl IntoElement + use<> {
    let travel = TRACK - INSET * 2.0;
    let knob_center = INSET + (1.0 - position / 100.0) * travel;
    let entity = cx.entity();
    div()
        .relative()
        .w(px(TRACK_WIDTH))
        .h(px(TRACK))
        .cursor_pointer()
        .child(
            div()
                .absolute()
                .left(px(TRACK_WIDTH / 2.0 - 2.0))
                .top(px(INSET))
                .w(px(4.0))
                .h(px(travel))
                .rounded(px(2.0))
                .bg(rgb(theme::border())),
        )
        .child(
            div()
                .absolute()
                .left(px(TRACK_WIDTH / 2.0 - 2.0))
                .top(px(knob_center))
                .w(px(4.0))
                .h(px(TRACK - INSET - knob_center))
                .rounded(px(2.0))
                .bg(rgb(theme::accent())),
        )
        .child(
            div()
                .absolute()
                // Centered on the track's axis: both are placed from the
                // column's middle, so neither drifts off the other.
                .left(px(TRACK_WIDTH / 2.0 - KNOB + 1.0))
                .top(px(knob_center - KNOB + 1.0))
                .size(px(KNOB * 2.0 - 2.0))
                .rounded_full()
                .bg(rgb(theme::accent()))
                .border_2()
                .border_color(rgb(theme::bg_secondary()))
                .shadow_sm(),
        )
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let press = entity.clone();
                    window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble
                            && event.button == MouseButton::Left
                            && bounds.contains(&event.position)
                        {
                            press.update(cx, |page, cx| {
                                page.zoom_dragging = true;
                                page.set_zoom_from_track(bounds, event.position.y, cx);
                            });
                        }
                    });
                    let drag = entity.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble
                            && event.pressed_button == Some(MouseButton::Left)
                            && drag.read(cx).zoom_dragging
                        {
                            drag.update(cx, |page, cx| {
                                page.set_zoom_from_track(bounds, event.position.y, cx);
                            });
                        }
                    });
                    let release = entity.clone();
                    window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                        if release.read(cx).zoom_dragging {
                            release.update(cx, |page, _| page.zoom_dragging = false);
                        }
                    });
                },
            )
            .absolute()
            .size_full(),
        )
}

fn preset_row(
    name: &'static str,
    preset: f32,
    active: bool,
    cx: &mut Context<ExplorerPane>,
) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("zoom-preset-{name}")))
        .h(px(ROW))
        .px(px(6.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(4.0))
        .text_xs()
        .cursor_pointer()
        .when(active, |row| row.bg(rgb(theme::accent())).text_color(rgb(theme::bg())))
        .when(!active, |row| {
            row.text_color(rgb(theme::fg()))
                .hover(|row| row.bg(rgb(theme::bg_hover())))
        })
        .on_click(cx.listener(move |page, _, _, cx| {
            page.zoom_to(preset, cx);
        }))
        .child(view_icon(name).size_3p5().text_color(rgb(if active {
            theme::bg()
        } else {
            theme::fg_secondary()
        })))
        .child(i18n::tr(name))
}

// Each view's glyph: a picture for tiles, the shape of the listing otherwise.
fn view_icon(name: &str) -> Icon {
    let path = match name {
        "Lista" => "icons/list.svg",
        "Árvore" => "icons/list-tree.svg",
        "Detalhes" => "icons/table.svg",
        _ => "icons/image.svg",
    };
    Icon::new(Icon::empty()).path(SharedString::from(path))
}

impl ExplorerPane {
    /// The bar's text field, made on first use (it needs a `Window`).
    pub(crate) fn quick_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = &self.name_filter_input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        self.name_filter_input = Some(input.clone());
        input
    }

    /// Sets the zoom from a point on the menu's slider track, snapping to a
    /// view's position when it is within a few pixels of it.
    fn set_zoom_from_track(&mut self, track: Bounds<Pixels>, y: Pixels, cx: &mut Context<Self>) {
        const SNAP: f32 = 6.0;
        let travel = TRACK - INSET * 2.0;
        let from_top = (f32::from(y - track.origin.y) - INSET).clamp(0.0, travel);
        let mut position = (1.0 - from_top / travel) * 100.0;
        for (_, preset) in zoom::MENU_STOPS {
            let at = zoom::slider_position(preset);
            if ((at - position) / 100.0 * travel).abs() <= SNAP {
                position = at;
                break;
            }
        }
        self.set_zoom(zoom::zoom_at(position), cx);
    }

    /// Switches between filtering this folder and searching its subfolders,
    /// keeping what was typed.
    pub(crate) fn set_quick_recursive(
        &mut self,
        recursive: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if recursive {
            self.set_everything(false, cx);
        }
        if self.quick_recursive != recursive {
            self.quick_recursive = recursive;
            if recursive {
                self.name_filter.clear();
                self.apply_filter();
            } else {
                self.end_subfolder_search();
            }
        }
        self.focus_quick_input(None, window, cx);
        cx.notify();
    }

    /// Puts the caret in the bar's field, optionally typing `text` into it
    /// first: a letter typed on the listing starts a filter, as in File Pilot.
    pub(crate) fn focus_quick_input(
        &mut self,
        text: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self.quick_input(window, cx);
        input.update(cx, |state, cx| {
            if let Some(text) = text {
                let value = format!("{}{text}", state.value());
                state.set_value(value, window, cx);
            }
            state.focus(window, cx);
        });
    }

    /// Empties the bar's field, ending its filter or search.
    pub(crate) fn clear_quick_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = &self.name_filter_input {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.end_subfolder_search();
        self.name_filter.clear();
        self.apply_filter();
        window.focus(&self.focus_handle);
        cx.notify();
    }
}

impl ExplorerPane {
    /// Sorts the folder again, after the folder order changed.
    pub(crate) fn resort(&mut self) {
        self.apply_filter();
    }

    /// Shows only `kind`, or everything again when it already is.
    pub(crate) fn toggle_kind_only(&mut self, kind: KindFilter, cx: &mut Context<Self>) {
        let next = if self.kind_filter == kind { KindFilter::All } else { kind };
        self.set_kind_filter(next, cx);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_shorten_like_file_pilot() {
        assert_eq!(super::short_count(999), "999");
        assert_eq!(super::short_count(7_600), "7.6K");
        assert_eq!(super::short_count(2_000), "2K");
        assert_eq!(super::short_count(1_250_000), "1.2M");
    }
}
