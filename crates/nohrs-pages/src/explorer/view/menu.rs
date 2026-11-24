//! The drawn context menu: the Windows menu's entries (read by
//! `shell_menu::prepare`) in Nohrs's own style, with Windows 11's row of
//! quick actions on top.
//!
//! Every level is placed by hand rather than by `anchored`'s snapping: a
//! submenu opens beside its row, so the parent's final position has to be
//! known — and kept on screen — before the child can be put next to it.

use crate::explorer::ExplorerPane;
use crate::explorer::context_menu::OpenMenu;
use crate::shell_menu::{ItemKind, MenuIcon, MenuItem};
use gpui::prelude::*;
use gpui::*;
use gpui_component::{Icon, IconName};
use nohrs_ui::theme::theme;

const ROW: f32 = 30.0;
const SEPARATOR: f32 = 9.0;
const PADDING: f32 = 4.0;
const QUICK_ROW: f32 = 46.0;
const MARGIN: f32 = 8.0;
const MIN_WIDTH: f32 = 220.0;
const MAX_WIDTH: f32 = 380.0;

/// Draws the open menu, if any, over the whole window.
pub fn render(
    page: &ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> Option<AnyElement> {
    let menu = page.context_menu.as_ref()?;
    let viewport = window.viewport_size();
    let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));

    let mut layers: Vec<AnyElement> = Vec::new();
    // Clicks outside every level close the menu, as Windows' do.
    layers.push(
        div()
            .id("menu-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .w(px(vw))
            .h(px(vh))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|page, _, _, cx| page.close_menu(cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|page, event: &MouseDownEvent, window, cx| {
                    page.close_menu(cx);
                    // A right double click goes up one level, wherever it
                    // lands: the first press opened this menu, and the second
                    // closes it and backs out of the folder.
                    if event.click_count >= 2 {
                        page.go_up(window, cx);
                    }
                }),
            )
            .into_any_element(),
    );

    // The top level, then each open submenu beside the row that opened it.
    let mut items: &[MenuItem] = &menu.items;
    let quick = !menu.quick.is_empty();
    let width = width_for(items);
    let height = height_for(items, quick);
    let mut x = f32::from(menu.position.x)
        .min(vw - width - MARGIN)
        .max(MARGIN);
    let mut y = f32::from(menu.position.y);
    if y + height > vh - MARGIN {
        y = (y - height).max(MARGIN);
    }
    let mut level_width = width;
    layers.push(panel(
        menu,
        0,
        items,
        x,
        y,
        width,
        height.min(vh - MARGIN * 2.0),
        quick,
        cx,
    ));

    for (level, &open) in menu.open_path.iter().enumerate() {
        let Some(ItemKind::Submenu(children)) = items.get(open).map(|item| &item.kind) else {
            break;
        };
        let offset =
            PADDING + if level == 0 && quick { QUICK_ROW } else { 0.0 } + offset_of(items, open);
        let child_width = width_for(children);
        let child_height = height_for(children, false);
        let right = x + level_width - 2.0;
        let child_x = if right + child_width <= vw - MARGIN {
            right
        } else {
            (x - child_width + 2.0).max(MARGIN)
        };
        let mut child_y = y + offset - PADDING;
        if child_y + child_height > vh - MARGIN {
            child_y = (vh - MARGIN - child_height).max(MARGIN);
        }
        layers.push(panel(
            menu,
            level + 1,
            children,
            child_x,
            child_y,
            child_width,
            child_height.min(vh - MARGIN * 2.0),
            false,
            cx,
        ));
        items = children;
        x = child_x;
        y = child_y;
        level_width = child_width;
    }

    Some(
        deferred(
            anchored()
                .position_mode(AnchoredPositionMode::Window)
                .position(point(px(0.0), px(0.0)))
                .child(div().w(px(vw)).h(px(vh)).children(layers)),
        )
        .with_priority(10)
        .into_any_element(),
    )
}

// Wide enough for the longest label and its shortcut, within bounds.
fn width_for(items: &[MenuItem]) -> f32 {
    let longest = items
        .iter()
        .map(|item| {
            item.label.chars().count() as f32 * 7.4
                + item
                    .shortcut
                    .as_ref()
                    .map_or(0.0, |keys| keys.chars().count() as f32 * 6.5 + 24.0)
        })
        .fold(0.0, f32::max);
    (longest + 84.0).clamp(MIN_WIDTH, MAX_WIDTH)
}

fn height_for(items: &[MenuItem], quick: bool) -> f32 {
    PADDING * 2.0 + if quick { QUICK_ROW } else { 0.0 } + offset_of(items, items.len())
}

// How far down the level the entry at `index` starts.
fn offset_of(items: &[MenuItem], index: usize) -> f32 {
    items
        .iter()
        .take(index)
        .map(|item| {
            if matches!(item.kind, ItemKind::Separator) {
                SEPARATOR
            } else {
                ROW
            }
        })
        .sum()
}

#[allow(clippy::too_many_arguments)]
fn panel(
    menu: &OpenMenu,
    level: usize,
    items: &[MenuItem],
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    quick: bool,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let open = menu.open_path.get(level).copied();
    div()
        .id(("menu-panel", level))
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(width))
        .h(px(height))
        .p(px(PADDING))
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .rounded(px(8.0))
        .border_1()
        .border_color(rgb(theme::border()))
        .bg(rgb(theme::bg_secondary()))
        .shadow_xl()
        .occlude()
        .when(quick, |panel| panel.child(quick_row(menu, cx)))
        .children(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| entry(item, level, index, open == Some(index), cx)),
        )
        .into_any_element()
}

// Windows 11's row of icons above the entries: Cut, Copy, Rename, Delete.
fn quick_row(menu: &OpenMenu, cx: &mut Context<ExplorerPane>) -> impl IntoElement {
    div()
        .h(px(QUICK_ROW))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(2.0))
        .mb(px(0.0))
        .border_b_1()
        .border_color(rgb(theme::border()))
        .children(menu.quick.iter().map(|action| {
            let id = action.id;
            let tip = action.label;
            div()
                .id(SharedString::from(format!("menu-quick-{id}")))
                .size(px(36.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .text_color(rgb(theme::fg()))
                .hover(|button| button.bg(rgb(theme::bg_hover())))
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(tip).build(window, cx)
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(
                    cx.listener(move |page, _, window, cx| page.choose_menu_item(id, window, cx)),
                )
                .child(
                    Icon::new(Icon::empty())
                        .path(SharedString::from(action.icon))
                        .size_4(),
                )
        }))
}

fn entry(
    item: &MenuItem,
    level: usize,
    index: usize,
    open: bool,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    if matches!(item.kind, ItemKind::Separator) {
        return div()
            .h(px(SEPARATOR))
            .flex_shrink_0()
            .flex()
            .items_center()
            .px(px(8.0))
            .child(div().w_full().h(px(1.0)).bg(rgb(theme::border())))
            .into_any_element();
    }
    let submenu = matches!(item.kind, ItemKind::Submenu(_));
    let enabled = item.enabled;
    let id = item.id;
    let icon: AnyElement = match &item.icon {
        MenuIcon::Image(image) => img(ImageSource::Render(image.clone()))
            .size(px(16.0))
            .into_any_element(),
        MenuIcon::Asset(path) => Icon::new(Icon::empty())
            .path(SharedString::from(*path))
            .size_4()
            .text_color(rgb(theme::fg_secondary()))
            .into_any_element(),
        MenuIcon::None if item.checked && item.radio => div()
            .size(px(6.0))
            .rounded_full()
            .bg(rgb(theme::accent()))
            .into_any_element(),
        MenuIcon::None if item.checked => Icon::new(IconName::Check)
            .size_4()
            .text_color(rgb(theme::accent()))
            .into_any_element(),
        MenuIcon::None => div().into_any_element(),
    };
    div()
        .id(("menu-entry", level * 10_000 + index))
        .h(px(ROW))
        .flex_shrink_0()
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .rounded(px(5.0))
        .text_sm()
        .when(!enabled, |row| row.text_color(rgb(theme::muted())))
        .when(enabled, |row| {
            row.text_color(rgb(theme::fg()))
                .cursor_pointer()
                .hover(|row| row.bg(rgb(theme::bg_hover())))
        })
        .when(open, |row| row.bg(rgb(theme::bg_hover())))
        .on_hover(cx.listener(move |page, hovered: &bool, _, cx| {
            if *hovered {
                page.hover_menu_entry(level, index, submenu, cx);
            }
        }))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .when(enabled && !submenu, |row| {
            row.on_click(
                cx.listener(move |page, _, window, cx| page.choose_menu_item(id, window, cx)),
            )
        })
        .child(
            div()
                .w(px(16.0))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .child(icon),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .when(item.bold, |label| label.font_weight(FontWeight::SEMIBOLD))
                // A checked entry with its own icon still shows it is on.
                .when(
                    item.checked && !matches!(item.icon, MenuIcon::None),
                    |label| label.text_color(rgb(theme::accent())),
                )
                .child(item.label.clone()),
        )
        .when_some(item.shortcut.clone(), |row, keys| {
            row.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(rgb(theme::muted()))
                    .child(keys),
            )
        })
        .when(submenu, |row| {
            row.child(
                Icon::new(IconName::ChevronRight)
                    .size_3()
                    .text_color(rgb(theme::muted())),
            )
        })
        .into_any_element()
}
