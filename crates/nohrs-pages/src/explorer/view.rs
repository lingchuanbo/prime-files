use std::time::Instant;

use crate::compact::{self, Edge};
use crate::explorer::ExplorerPane;
use crate::explorer::state::FramedBars;
use gpui::prelude::FluentBuilder;
use nohrs_core::telemetry::LogErr;
use gpui::*;
use nohrs_ui::theme::theme;

/// Height of the bar under the listing (`quick_bar`'s own `h`).
const QUICK_BAR_HEIGHT: f32 = 32.0;

/// Modal dialogs for file operations (permanent-delete confirm, paste conflict).
mod dialogs;
/// The filter bar: narrowing the listing by name pattern and kind.
mod filter_bar;
/// The explorer header with navigation controls and the path bar.
pub mod header;
/// The main file listing, in list or grid mode, with the search bar.
pub mod listing;
/// The file preview pane.
pub mod preview;
/// The explorer sidebar with quick-access locations.
pub mod sidebar;
mod navigation_icons;

/// The page a workspace (a tag, renamed) opens to: its folders as cards.
pub mod workspace;
/// The filter and subfolder-search field under the listing.
pub mod quick_bar;
/// Shared search field: in the header, or the compact footer.
mod quick_search;
/// The drawn context menus.
pub mod menu;
/// Sidebar, listing and preview side by side, with draggable dividers.
pub(crate) mod panels;

/// Renders the explorer page: header, sidebar, listing, and preview panes.
pub fn render(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    page.ensure_loaded(cx);
    page.tick_zoom(window, cx);
    page.tick_scroll(window, cx);
    page.tick_marquee(window, cx);
    // The played part of a sound under the mouse moves while it plays.
    if page.hover_audio.is_some() && super::hover_audio::playing() {
        window.request_animation_frame();
    }
    // The grid aims its own reveal once it knows its columns
    // (`grid::render`); the other views count rows, which need no measure.
    if page.view_mode != super::types::ViewMode::Grid
        && let Some((index, _)) = page.reveal_scroll.take()
    {
        page.scroll_entry_into_view(index);
    }
    // A file revealed from another program glows for a moment.
    if page.tick_reveal_glow() {
        window.request_animation_frame();
    }
    if std::mem::take(&mut page.preview_selection_pending)
        && let Some(path) = page.active_path.clone()
    {
        page.open_preview(path, window, cx);
    }
    page.update_editor_search(window, cx);
    if !page.focus_requested {
        page.focus_requested = true;
        cx.focus_self(window);
    }

    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::bg()))
        .relative()
        .track_focus(&page.focus_handle)
        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
            let key_lc = event.keystroke.key.to_lowercase();
            let modifiers = event.keystroke.modifiers;
            let with_modifier = modifiers.platform || modifiers.control;
            // Escape lets go of a drag of a panel or an edge: nothing opens.
            if key_lc == "escape" && this.cancel_dock_drag() {
                cx.notify();
                cx.stop_propagation();
                return;
            }
            // Escape closes an open menu before anything else hears it.
            if key_lc == "escape" && this.context_menu.is_some() {
                this.close_menu(cx);
                cx.stop_propagation();
                return;
            }
            // Ctrl+S saves what was typed in the preview's editor.
            if with_modifier && key_lc == "s" && this.preview_dirty {
                this.save_preview(cx);
                cx.stop_propagation();
                return;
            }
            if key_lc == "escape" && this.quick_look {
                this.close_quick_look(cx);
                cx.stop_propagation();
                return;
            }
            // Escape while a browser waits on Nohrs hands its dialog back.
            if key_lc == "escape" && crate::file_dialog::picking().is_some() {
                crate::file_dialog::cancel();
                cx.stop_propagation();
                cx.notify();
                return;
            }
            // Escape abandons an open rename before anything else can claim it —
            // the field has focus, so this is the only Escape the user means.
            // The path field is a text field like any other: Escape closes
            // it, and every other key is its own (Ctrl+C copies the path, not
            // the selected files).
            if this.path_edit.is_some() {
                if key_lc == "escape" {
                    this.cancel_path_edit(window, cx);
                    cx.stop_propagation();
                }
                return;
            }
            if key_lc == "escape" && this.renaming.is_some() {
                this.cancel_rename(window, cx);
                cx.stop_propagation();
                return;
            }
            if key_lc == "escape" && this.search_visible {
                this.toggle_search(window, cx);
                cx.stop_propagation();
                return;
            }
            // The configurable shortcuts (Settings › Shortcuts). A bare letter
            // never resolves to one here: typed on the listing it filters.
            if let Some(action) = nohrs_core::shortcuts::resolve(with_modifier, modifiers.alt, modifiers.shift, &key_lc)
                && this.run_shortcut(action, window, cx)
            {
                cx.stop_propagation();
                return;
            }
            // Esc with nothing typed leaves the whole-computer search.
            if key_lc == "escape" && this.everything_on() && this.deep_search.is_none() {
                this.set_everything(false, cx);
                cx.stop_propagation();
                return;
            }
            let quick_bar_active = this.deep_search.is_some() || !this.name_filter.is_empty();
            if key_lc == "escape" && quick_bar_active {
                this.clear_quick_input(window, cx);
                cx.stop_propagation();
                return;
            }
            // A letter typed on the listing goes to the filter, as in File
            // Pilot; with a modifier it is a shortcut, and space is left to
            // the listing.
            if this.focus_handle.is_focused(window)
                && !with_modifier
                && !modifiers.alt
                && let Some(text) = event.keystroke.key_char.as_deref()
                && text.chars().count() == 1
                && text.chars().all(|character| !character.is_control() && !character.is_whitespace())
            {
                this.focus_quick_input(Some(text), window, cx);
                cx.stop_propagation();
                return;
            }
            // File operations (§1, canonical keys in §6). Each arm consumes the
            // event; anything unmatched falls through to the selection model below.
            let shift = modifiers.shift;
            match key_lc.as_str() {
                // Space shows the active entry large over the listing and
                // hides it again, as Quick Look does; the arrows move on
                // under it. In the filter field a space is a space: the
                // field has the focus then, not the listing.
                "space"
                    if !with_modifier
                        && !modifiers.alt
                        && (this.quick_look || this.focus_handle.is_focused(window)) =>
                {
                    this.toggle_quick_look(window, cx);
                    cx.stop_propagation();
                    return;
                }
                // macOS also trashes on Backspace (§6); on Linux only Delete does.
                "backspace" if cfg!(target_os = "macos") && !shift => {
                    this.trash_selection(cx);
                    cx.stop_propagation();
                    return;
                }
                // Enter opens the active entry on Windows, as in Explorer: a
                // folder is entered, a file opened (or sent to a waiting
                // browser dialog). macOS keeps Enter for rename, below.
                // Only with the listing itself focused: Enter in the search or
                // filter field bubbles up here too, and means something else.
                "enter"
                    if !cfg!(target_os = "macos")
                        && !with_modifier
                        && this.focus_handle.is_focused(window) =>
                {
                    if let Some(item) = this
                        .active_row()
                        .and_then(|index| this.filtered_entries.get(index).cloned())
                    {
                        this.close_quick_look(cx);
                        this.activate_entry(item, window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                "enter" if cfg!(target_os = "macos") && !with_modifier => {
                    if let Some(index) = this.active_row() {
                        this.begin_rename(index, window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
            // Selection model (§5). Escape while searching is handled above, so
            // here it only clears the selection.
            match key_lc.as_str() {
                // Search-close Escape is handled by the branch above (which
                // returns early), so here Escape only clears a selection — and
                // only consumes the event when there was one to clear, leaving
                // an empty-selection Escape free to bubble.
                "escape" if !this.selection.is_empty() => {
                    this.clear_selection();
                    cx.stop_propagation();
                    cx.notify();
                }
                // In the grid Up/Down cross a row and Left/Right step along
                // it; in the list a row is a single entry.
                "up" => {
                    this.move_active_following(-this.row_stride(), modifiers.shift, window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
                "down" => {
                    this.move_active_following(this.row_stride(), modifiers.shift, window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
                // In the List view a column is a run of entries; sideways
                // crosses a whole column.
                "left" if this.view_mode == super::types::ViewMode::Compact => {
                    this.move_active_following(-(this.compact_rows.max(1) as isize), modifiers.shift, window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
                "right" if this.view_mode == super::types::ViewMode::Compact => {
                    this.move_active_following(this.compact_rows.max(1) as isize, modifiers.shift, window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
                // Tree: Right opens the folder, Left closes it, as in
                // Explorer's navigation pane.
                "right" | "left" if this.view_mode == super::types::ViewMode::Tree => {
                    let open = key_lc == "right";
                    if let Some(folder) = this
                        .active_row()
                        .and_then(|row| this.filtered_entries.get(row))
                        .filter(|entry| entry.kind == "dir")
                        .map(|entry| entry.path.clone())
                        && this.tree_open(&folder) != open
                    {
                        this.toggle_tree_folder(folder, cx);
                    }
                    cx.stop_propagation();
                    cx.notify();
                }
                "left" if this.view_mode == super::types::ViewMode::Grid => {
                    this.move_active_following(-1, modifiers.shift, window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
                "right" if this.view_mode == super::types::ViewMode::Grid => {
                    this.move_active_following(1, modifiers.shift, window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
                _ => {}
            }
        }))
        .on_mouse_move(
            cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                // The button came up outside the pane: the drag ends there.
                if this.dock_drag.is_some() && event.pressed_button != Some(gpui::MouseButton::Left) {
                    if this.finish_dock_drag(cx) {
                        cx.notify();
                    }
                    return;
                }
                if event.pressed_button == Some(gpui::MouseButton::Left)
                    && this.update_dock_drag(event.position)
                {
                    cx.notify();
                    return;
                }
                if this.resizing_column.is_some() {
                    this.update_column_resize(event.position);
                    cx.notify();
                }
            }),
        )
        // Let go outside the pane (over the other pane, the title bar): the
        // drag ends there too.
        .on_mouse_up_out(
            gpui::MouseButton::Left,
            cx.listener(|this, _event, _window, cx| {
                if this.finish_dock_drag(cx) {
                    cx.notify();
                }
            }),
        )
        .on_mouse_up(
            gpui::MouseButton::Left,
            cx.listener(|this, _event, _window, cx| {
                if this.finish_dock_drag(cx) {
                    cx.notify();
                }
                if this.resizing_column.is_some() {
                    this.stop_column_resize();
                    cx.notify();
                }
            }),
        )
        // The mouse's side buttons go back and forward, as in Explorer and
        // every browser. gpui reports them as `Navigate`; nothing listened.
        // Anywhere over the pane: each pane of a split has its own history.
        .on_mouse_down(
            gpui::MouseButton::Navigate(gpui::NavigationDirection::Back),
            cx.listener(|this, _event, window, cx| {
                this.close_quick_look(cx);
                this.go_back(window, cx);
                cx.stop_propagation();
            }),
        )
        .on_mouse_down(
            gpui::MouseButton::Navigate(gpui::NavigationDirection::Forward),
            cx.listener(|this, _event, window, cx| {
                this.close_quick_look(cx);
                this.go_forward(window, cx);
                cx.stop_propagation();
            }),
        )
        .map(|this| render_body(this, page, window, cx))
        .children(preview::render_quick_look(page, window, cx))
        .children(menu::render(page, window, cx))
}

// Address bar, filter bar, the three columns and the bar under them, in that
// order (the bottom bar applies the filter as it renders, after the listing,
// as before compact mode). In compact mode each chosen bar slides out of its
// edge instead (see `crate::compact`): the address bar under the floating
// tab row, the sidebar and the preview over the listing's sides, the bottom
// bar over its foot.
fn render_body(
    root: Div,
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> Div {
    let chrome = compact_chrome(page, window, cx);
    let view = cx.entity_id();
    let overlay = page.compact.overlay;
    let edge_depth = page.compact.edge_px as f32;
    // Zen's frame: the pane's content in a card with rounded corners, set in
    // from the window by this much. The strips stay on the outside and reach
    // across the frame, so the window's edge still calls the bars.
    let frame = if page.compact.enabled { page.compact.frame_px as f32 } else { 0.0 };
    // With a frame, a bar comes out of it, as in Zen: the frame's colour, no
    // shadow, square corners, so the frame looks to grow in on that side.
    // Floating, it lies over the card (see `frame_bar`); pushing, it takes
    // its room in the frame beside the card, which shrinks and keeps its own
    // rounded outline. Without a frame, a floating bar lies over the listing
    // with a shadow, and a pushing one is part of the pane's column.
    let framed = frame > 0.0;
    page.framed = FramedBars {
        header: framed && chrome.header,
        sidebar: framed && chrome.sidebar,
        preview: framed && chrome.preview,
        bottom: framed && chrome.bottom,
    };
    let mut out_of_frame: Vec<AnyElement> = Vec::new();
    // Pushing bars in the frame, above and below the card.
    let mut above_card: Option<AnyElement> = None;
    let mut below_card: Option<AnyElement> = None;
    let body = div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .min_w(px(0.0));
    // What floats over the pane, then the strips that watch the edges, last
    // so nothing blocks the pointer from them.
    let mut floating: Vec<AnyElement> = Vec::new();
    let mut strips: Vec<AnyElement> = Vec::new();

    let header = header::render(page, window, cx).into_any_element();
    let body = match page.top_reveal.clone().filter(|_| chrome.header) {
        Some(top) => {
            let amount = top.amount();
            let height = page.header_height.get();
            let inset = top.inset();
            strips.push(
                compact::edge_strip(
                    "compact-header-edge",
                    Edge::Top,
                    frame + edge_depth.max(height * amount),
                    top,
                    compact::SOURCE_HEADER,
                    view,
                )
                .top(px(inset))
                .into_any_element(),
            );
            let measure = Some(page.header_height.clone());
            if overlay && framed {
                // Under the tab row when that is out, which covers the
                // frame's top band; out of the band itself otherwise.
                let lead = (frame - inset).max(0.0);
                out_of_frame.push(
                    frame_bar(compact::slider(Edge::Top, lead, height, amount, measure, header), Edge::Top, amount)
                        .top(px(inset))
                        .left(px(frame))
                        .right(px(frame))
                        .into_any_element(),
                );
                body
            } else if overlay {
                floating.push(
                    compact::slider(Edge::Top, 0.0, height, amount, measure, header)
                        .absolute()
                        .occlude()
                        .top(px((inset - frame).max(0.0)))
                        .left_0()
                        .right_0()
                        .when(amount > 0.0, |this| this.shadow_md())
                        .into_any_element(),
                );
                body
            } else if framed {
                above_card = Some(compact::slider(Edge::Top, 0.0, height, amount, measure, header).into_any_element());
                body
            } else {
                body.child(compact::slider(Edge::Top, 0.0, height, amount, measure, header))
            }
        }
        None => body.child(header),
    };

    let body = if page.filter_bar_visible {
        body.child(filter_bar::render(page, window, cx))
    } else {
        body
    };

    let sidebar = if page.sidebar_visible {
        Some(sidebar::render(page, window, cx).into_any_element())
    } else {
        None
    };
    // A workspace opens as a page of folder cards, not as a listing of the
    // same paths: what it holds is the folders, and what a listing would show
    // is what is inside each of them, row after row.
    let listing = match super::tags::tag_of_location(&page.cwd) {
        Some(tag) => page.workspace_page(tag, cx),
        None => listing::render(page, window, cx),
    };
    // While Space's quick look shows the preview over everything, the side
    // panel keeps its room, empty, so the listing behind does not reflow
    // twice; in compact mode it stays in its edge.
    let preview = if page.quick_look {
        (page.preview_visible && !chrome.preview)
            .then(|| div().size_full().bg(rgb(theme::bg())).into_any_element())
    } else if page.preview_visible {
        Some(preview::render(page, window, cx).into_any_element())
    } else {
        None
    };
    // Floating, the sides lie over the listing. Pushing, they take their
    // room from it: in the frame beside the card, or, with no frame, as the
    // listing's own columns, which narrow with the slide. Compact mode slides
    // a panel docked to the left or the right out of that edge; one docked
    // above or below the listing stays put (`compact_chrome`).
    let mut sides: Vec<AnyElement> = Vec::new();
    let mut left_of_card: Vec<AnyElement> = Vec::new();
    let mut right_of_card: Vec<AnyElement> = Vec::new();
    let mut docked: Vec<panels::Docked> = Vec::new();
    for (panel, element, sliding) in [
        (panels::Panel::Sidebar, sidebar, chrome.sidebar),
        (panels::Panel::Preview, preview, chrome.preview),
    ] {
        let Some(element) = element else {
            continue;
        };
        if !sliding {
            docked.push(panels::Docked { panel, element, amount: 1.0 });
            continue;
        }
        let on_right = panels::side(panel) == nohrs_core::config::DockSide::Right;
        let edge = if on_right { Edge::Right } else { Edge::Left };
        let reveal = match panel {
            panels::Panel::Sidebar => page.left_reveal.clone(),
            panels::Panel::Preview => page.right_reveal.clone(),
        };
        let amount = reveal.amount();
        let width = match panel {
            panels::Panel::Sidebar => panels::sidebar_width(),
            panels::Panel::Preview => {
                let width = panels::preview_width().max(240.0);
                // Floating, kept inside the listing it covers. Not when
                // pushing: the listing narrows as the preview widens, and
                // capping by it would chase its own tail.
                let room = f32::from(page.columns_bounds.get().size.width);
                if overlay && room > 0.0 { width.min(room * 0.9) } else { width }
            }
        };
        if !overlay && framed {
            // The frame's width again between the bar and the card, as on
            // the window's side of it.
            let padded = div().size_full().flex().flex_col();
            let padded = if on_right { padded.pl(px(frame)) } else { padded.pr(px(frame)) }.child(element);
            let slid = compact::slider(edge, 0.0, width + frame, amount, None, padded).into_any_element();
            if on_right { right_of_card.push(slid) } else { left_of_card.push(slid) }
        } else if !overlay {
            docked.push(panels::Docked {
                panel,
                element: compact::slider(edge, 0.0, width, amount, None, element).into_any_element(),
                amount,
            });
        } else if framed {
            let bar = frame_bar(compact::slider(edge, frame, width, amount, None, element), edge, amount)
                .top(px(frame))
                .bottom(px(frame));
            let bar = if on_right { bar.right_0() } else { bar.left_0() };
            out_of_frame.push(bar.into_any_element());
        } else {
            let floating_side = side_panel(compact::slider(edge, 0.0, width, amount, None, element), amount);
            let floating_side = if on_right {
                floating_side.right_0().when(amount > 0.0, |this| this.border_l_1())
            } else {
                floating_side.left_0().when(amount > 0.0, |this| this.border_r_1())
            };
            sides.push(floating_side.into_any_element());
        }
        strips.push(
            compact::edge_strip(
                SharedString::from(format!("compact-{}-edge", panel.config_key())),
                edge,
                frame + edge_depth.max(width * amount),
                reveal,
                compact::SOURCE_EDGE,
                view,
            )
            .into_any_element(),
        );
    }
    let columns = panels::render(page, docked, listing, cx);
    let body = body.child(
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_grow()
            .min_h(px(0.0))
            .child(columns)
            .children(sides),
    );

    let quick = quick_bar::render(page, window, cx).into_any_element();
    let body = if chrome.bottom {
        let amount = page.bottom_reveal.amount();
        strips.push(
            compact::edge_strip(
                "compact-bottom-edge",
                Edge::Bottom,
                frame + edge_depth.max(QUICK_BAR_HEIGHT * amount),
                page.bottom_reveal.clone(),
                compact::SOURCE_EDGE,
                view,
            )
            .into_any_element(),
        );
        if overlay && framed {
            out_of_frame.push(
                // The frame's band is part of the bar here, not a gap under
                // it, so the bar's contents sit centred between the outline
                // above and the window's edge (see `quick_bar`'s height).
                frame_bar(
                    compact::slider(Edge::Bottom, 0.0, QUICK_BAR_HEIGHT + frame, amount, None, quick),
                    Edge::Bottom,
                    amount,
                )
                    .bottom_0()
                    .left(px(frame))
                    .right(px(frame))
                    .into_any_element(),
            );
            body
        } else if overlay {
            floating.push(
                compact::slider(Edge::Bottom, 0.0, QUICK_BAR_HEIGHT, amount, None, quick)
                    .absolute()
                    .occlude()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .when(amount > 0.0, |this| this.shadow_md())
                    .into_any_element(),
            );
            body
        } else if framed {
            // The bar is 32 px plus the frame tall (`quick_bar`), its contents
            // centred; shown 32 px of it over the frame's own band below, the
            // gaps above and below its contents come out even.
            below_card = Some(compact::slider(Edge::Bottom, 0.0, QUICK_BAR_HEIGHT, amount, None, quick).into_any_element());
            body
        } else {
            body.child(compact::slider(Edge::Bottom, 0.0, QUICK_BAR_HEIGHT, amount, None, quick))
        }
    } else {
        body.child(quick)
    };

    let body = body.children(floating);
    if frame <= 0.0 {
        return root.child(body).children(strips);
    }
    // The corners go with the card, which pushing bars move.
    let card = div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .min_w(px(0.0))
        .child(body.bg(rgb(theme::bg())).overflow_hidden())
        .children(frame_corners());
    let middle = div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h(px(0.0))
        .children(left_of_card)
        .child(card)
        .children(right_of_card.into_iter().rev());
    root.p(px(frame))
        .bg(rgb(theme::toolbar_bg()))
        .children(above_card)
        .child(middle)
        .children(below_card)
        .children(out_of_frame)
        .children(strips)
}

/// Radius of the card's corners in compact mode's frame.
const FRAME_RADIUS: f32 = 8.0;

// The card's rounded corners and its outline. gpui clips children to
// rectangles, so the content's own square corners would show through a
// rounded card; instead a ring in the frame's colour is drawn over them: a
// box larger than the card by the radius on every side, with a border that
// wide, whose inner edge is then the card's rounded outline. Its own outer
// corners round off over the frame, which is the same colour, and the band
// it lays outside the card is frame-coloured too: over the frame, or under a
// pushing bar, which is drawn after it and paints nothing of its own.
// Neither box takes the mouse (no listeners, so no hitbox).
fn frame_corners() -> [AnyElement; 2] {
    let reach = FRAME_RADIUS;
    let ring = div()
        .absolute()
        .top(px(-reach))
        .bottom(px(-reach))
        .left(px(-reach))
        .right(px(-reach))
        .border(px(reach))
        .rounded(px(FRAME_RADIUS + reach))
        .border_color(rgb(theme::toolbar_bg()));
    let outline = div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .border_1()
        .rounded(px(FRAME_RADIUS))
        .border_color(rgb(theme::border()));
    [ring.into_any_element(), outline.into_any_element()]
}

// A bar out of compact mode's frame: the frame's colour (the bar itself
// paints none, see `FramedBars`), square corners, no shadow, so it reads as
// the frame grown in on that side. Along its side toward the listing runs
// the card's outline, which the bar covers while it is out, so the line
// carries on round the card. `occlude`: what is behind a bar must not get
// its clicks.
fn frame_bar(slid: Div, edge: Edge, amount: f32) -> Div {
    let line = div().absolute().bg(rgb(theme::border()));
    let line = match edge {
        Edge::Top => line.bottom_0().left_0().right_0().h(px(1.0)),
        Edge::Bottom => line.top_0().left_0().right_0().h(px(1.0)),
        Edge::Left => line.right_0().top_0().bottom_0().w(px(1.0)),
        Edge::Right => line.left_0().top_0().bottom_0().w(px(1.0)),
    };
    // The slide makes the bar full width (or height) along its edge; here
    // the caller's offsets set that instead, clear of the frame's sides,
    // and a fixed 100% on top of them would run the bar past the far one.
    let slid = match edge {
        Edge::Top | Edge::Bottom => slid.w_auto(),
        Edge::Left | Edge::Right => slid.h_auto(),
    };
    slid.absolute()
        .occlude()
        .bg(rgb(theme::toolbar_bg()))
        .when(amount > 0.0, |this| this.child(line))
}

// A side column floating over the listing, full height, on the window's
// background with its divider line.
fn side_panel(slid: Div, amount: f32) -> Div {
    slid.absolute()
        .occlude()
        .top_0()
        .bottom_0()
        .bg(rgb(theme::bg()))
        .border_color(rgb(theme::border()))
        .when(amount > 0.0, |this| this.shadow_lg())
}

/// Which bars compact mode hides on this pane, this frame.
#[derive(Default)]
struct CompactChrome {
    header: bool,
    sidebar: bool,
    preview: bool,
    bottom: bool,
}

// Decides which bars hide, keeps them out or still while they are in use, and
// advances the slides this pane owns (the top one belongs to the pane group,
// which ticks it before this renders).
fn compact_chrome(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> CompactChrome {
    let settings = page.compact.clone();
    if !settings.enabled {
        return CompactChrome::default();
    }
    let chrome = CompactChrome {
        header: settings.hide_header && page.top_reveal.is_some(),
        // Only a panel docked to the left or the right slides: the top and
        // the bottom edges already call the address bar and the bottom bar.
        sidebar: settings.hide_sidebar && page.sidebar_visible && panels::side(panels::Panel::Sidebar).is_column(),
        preview: settings.hide_preview && page.preview_visible && panels::side(panels::Panel::Preview).is_column(),
        bottom: settings.hide_bottom_bar,
    };
    // An open menu freezes every bar: the pointer going into the menu must
    // not take away the bar it was opened from.
    let menu_open = page.context_menu.is_some();
    let filter_focused = page
        .name_filter_input
        .as_ref()
        .is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(window));
    let filtering = filter_focused || page.quick_recursive || page.everything_on() || !page.name_filter.is_empty();

    let mut frame = false;
    if let Some(top) = &page.top_reveal {
        // The group reads these on its next frame.
        frame |= top.set_forced(chrome.header && page.path_edit.is_some());
        frame |= top.set_held(menu_open);
    }
    let now = Instant::now();
    for (reveal, used, forced) in [
        (&page.left_reveal, chrome.sidebar, false),
        (&page.right_reveal, chrome.preview, false),
        (&page.bottom_reveal, chrome.bottom, filtering),
    ] {
        if used {
            frame |= reveal.set_forced(forced);
            frame |= reveal.set_held(menu_open);
            frame |= reveal.tick(&settings, now);
        }
    }
    if frame {
        window.request_animation_frame();
    }
    chrome
}

/// `text` with every occurrence of `query` in the accent colour, as File
/// Pilot marks what a filter or search matched.
pub fn highlighted_name(text: &str, query: &str) -> gpui::StyledText {
    let accent = gpui::HighlightStyle {
        color: Some(rgb(theme::accent()).into()),
        ..Default::default()
    };
    let ranges = find_query_highlights(text, query)
        .into_iter()
        .map(|(range, _)| (range, accent))
        .collect::<Vec<_>>();
    gpui::StyledText::new(text.to_string()).with_highlights(ranges)
}

/// The "› folder" after a search hit's name, muted, as File Pilot shows it.
pub fn hit_folder_label(folder: &str) -> gpui::Div {
    div()
        .flex_shrink()
        .min_w(px(0.0))
        .pl(px(6.0))
        .text_xs()
        .text_color(rgb(theme::muted()))
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .child(format!("›  {folder}"))
}

/// Returns highlight ranges for every case-insensitive occurrence of `query`
/// within `text`, for emphasizing search matches.
pub fn find_query_highlights(
    text: &str,
    query: &str,
) -> Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> {
    let mut highlights = Vec::new();
    if query.is_empty() {
        return highlights;
    }

    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let text_chars: Vec<(usize, char)> = text.char_indices().collect();

    let mut i = 0;
    while i < text_chars.len() {
        let mut match_found = true;
        let mut q_idx = 0;

        let mut current_t_offset = 0;

        while q_idx < query_lower.len() {
            if i + current_t_offset >= text_chars.len() {
                match_found = false;
                break;
            }

            let (_, t_char) = text_chars[i + current_t_offset];
            let t_lower = t_char.to_lowercase();

            for tc in t_lower {
                if q_idx >= query_lower.len() || query_lower[q_idx] != tc {
                    match_found = false;
                    break;
                }
                q_idx += 1;
            }

            if !match_found {
                break;
            }
            current_t_offset += 1;
        }

        if match_found && q_idx == query_lower.len() {
            let start_byte = text_chars[i].0;
            let end_byte = if i + current_t_offset < text_chars.len() {
                text_chars[i + current_t_offset].0
            } else {
                text.len()
            };

            highlights.push((
                start_byte..end_byte,
                gpui::HighlightStyle {
                    background_color: Some(gpui::Hsla::from(gpui::Rgba {
                        r: 1.0,
                        g: 0.9,
                        b: 0.0,
                        a: 0.5,
                    })),
                    color: Some(gpui::Hsla::from(gpui::Rgba {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    })),
                    ..Default::default()
                },
            ));

            i += current_t_offset;
        } else {
            i += 1;
        }
    }

    highlights.retain(|(range, _)| {
        let start_ok = text.is_char_boundary(range.start);
        let end_ok = text.is_char_boundary(range.end);
        if !start_ok || !end_ok {
            if std::env::var("NOHR_DEBUG").is_ok() {
                tracing::error!("[CRITICAL] find_query_highlights: Removing invalid highlight: {:?} (start_ok={}, end_ok={}) in text len {}", range, start_ok, end_ok, text.len());
            }
            return false;
        }
        true
    });

    highlights
}

impl ExplorerPane {
    /// The page of workspace `tag`: its folders as cards, counted the first
    /// time it opens (and again after a folder is added or removed).
    pub(crate) fn workspace_page(&mut self, tag: u32, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_workspace(tag, cx);
        let cards = self
            .workspace
            .as_ref()
            .and_then(|view| view.cards.clone())
            .unwrap_or_default();
        self::workspace::page(self, tag, &cards, cx)
    }

    fn ensure_workspace(&mut self, tag: u32, cx: &mut Context<Self>) {
        let ready = self.workspace.as_ref().is_some_and(|view| {
            view.tag == tag && (view.cards.is_some() || view.task.is_some())
        });
        if ready {
            return;
        }
        // The folders the workspace holds are the rows its location listed.
        let paths: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| entry.kind == "dir")
            .map(|entry| entry.path.clone())
            .collect();
        self.workspace = Some(super::state::WorkspaceView {
            tag,
            cards: None,
            task: None,
        });
        let read = cx.background_spawn(async move { self::workspace::read_cards(paths) });
        let task = cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let cards = read.await;
            this.update(cx, |pane, cx| {
                if let Some(view) = pane.workspace.as_mut() {
                    view.cards = Some(std::rc::Rc::new(cards));
                    view.task = None;
                }
                cx.notify();
            })
            .log_err();
        });
        if let Some(view) = self.workspace.as_mut() {
            view.task = Some(task);
        }
    }

    /// Throws the cards away so the next render counts the folders again,
    /// after one was added to the workspace or taken out of it.
    pub(crate) fn refresh_workspace(&mut self, cx: &mut Context<Self>) {
        self.workspace = None;
        cx.notify();
    }
}
