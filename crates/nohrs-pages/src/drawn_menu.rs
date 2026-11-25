//! The menu the quick launch window and its search window draw themselves.
//!
//! Those two windows do not use `gpui_component`'s popup menu: in the quick
//! launch window that menu's shadow is composited as a near-black, banded cloud
//! in the light theme (the same menu draws correctly in the main window), and a
//! panel with a border instead of a shadow reads the same in both themes. The
//! menu is drawn inside the window, so it is clamped to the viewport the way the
//! explorer's own menu is.

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, MouseButton, MouseDownEvent, Pixels, Point, Render,
    SharedString, Window, div, px, rgb,
};
use gpui_component::{Icon, IconName};
use nohrs_ui::theme::theme;

/// Height of one row, of the rule between groups of rows, the panel's padding,
/// and the gap kept from the window's edges.
const ROW: f32 = 30.0;
const RULE: f32 = 9.0;
const PADDING: f32 = 4.0;
const MARGIN: f32 = 6.0;
const MIN_WIDTH: f32 = 200.0;
const MAX_WIDTH: f32 = 380.0;

/// One row of a drawn menu.
#[derive(Clone)]
pub(crate) struct MenuRow {
    pub(crate) label: SharedString,
    pub(crate) disabled: bool,
    /// A rule above this row, separating it from the entries before it.
    pub(crate) rule: bool,
    /// The rows of the submenu this row opens; empty for an entry.
    pub(crate) children: Vec<MenuRow>,
    /// What an entry runs when clicked; `None` for a row that opens a submenu.
    pub(crate) id: Option<u32>,
}

impl MenuRow {
    /// An entry that runs `id` when clicked. A disabled entry is drawn greyed
    /// and does nothing.
    pub(crate) fn entry(label: impl Into<SharedString>, enabled: bool, id: u32) -> Self {
        Self {
            label: label.into(),
            disabled: !enabled,
            rule: false,
            children: Vec::new(),
            id: Some(id),
        }
    }

    /// A row that opens `children` beside itself.
    pub(crate) fn submenu(label: impl Into<SharedString>, children: Vec<MenuRow>) -> Self {
        Self {
            label: label.into(),
            disabled: false,
            rule: false,
            children,
            id: None,
        }
    }

    /// The rule between two groups of entries.
    pub(crate) fn rule() -> Self {
        Self {
            label: SharedString::default(),
            disabled: true,
            rule: true,
            children: Vec::new(),
            id: None,
        }
    }
}

/// The menu a window has open: where it opened, what it holds, and which rows
/// have their submenu open (outermost first).
#[derive(Clone)]
pub(crate) struct Menu {
    position: Point<Pixels>,
    rows: Vec<MenuRow>,
    open: Vec<usize>,
}

/// A view that shows a drawn menu.
pub(crate) trait MenuHost: Render + Sized + 'static {
    fn menu(&self) -> Option<&Menu>;

    /// Replaces the menu; `None` closes it.
    fn set_menu(&mut self, menu: Option<Menu>);

    /// Runs the action the row with `id` stands for. The menu has been closed
    /// by the time this is called.
    fn choose(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>);
}

/// Opens `rows` at `position`, over the window.
pub(crate) fn open_at<V: MenuHost>(
    view: &mut V,
    position: Point<Pixels>,
    rows: Vec<MenuRow>,
    cx: &mut Context<V>,
) {
    if rows.is_empty() {
        return;
    }
    view.set_menu(Some(Menu {
        position,
        rows,
        open: Vec::new(),
    }));
    cx.notify();
}

pub(crate) fn close<V: MenuHost>(view: &mut V, cx: &mut Context<V>) {
    if view.menu().is_some() {
        view.set_menu(None);
        cx.notify();
    }
}

pub(crate) fn is_open<V: MenuHost>(view: &V) -> bool {
    view.menu().is_some()
}

/// Closes the menu when the window it belongs to lost the focus.
pub(crate) fn window_deactivated<V: MenuHost>(view: &mut V, window: &Window, cx: &mut Context<V>) {
    if !window.is_window_active() {
        close(view, cx);
    }
}

/// The overlay with the open menu, to append to the window's root, if any.
pub(crate) fn render<V: MenuHost>(
    view: &V,
    window: &Window,
    cx: &mut Context<V>,
) -> Option<AnyElement> {
    let menu = view.menu()?.clone();
    let viewport = window.viewport_size();
    let (viewport_width, viewport_height) = (f32::from(viewport.width), f32::from(viewport.height));
    let mut layers = vec![backdrop(cx, viewport_width, viewport_height)];
    let mut rows = menu.rows.as_slice();
    let mut x = f32::from(menu.position.x).max(MARGIN);
    let mut y = f32::from(menu.position.y).max(MARGIN);
    let mut level = 0;
    loop {
        let width = width_for(rows);
        let height = height_for(rows);
        let panel_x = x.min((viewport_width - width - MARGIN).max(MARGIN));
        let panel_y = y.min((viewport_height - height - MARGIN).max(MARGIN));
        let open = menu.open.get(level).copied();
        layers.push(panel(
            level, rows, open, panel_x, panel_y, width, height, cx,
        ));
        let Some(open) = open else {
            break;
        };
        let Some(children) = rows.get(open).map(|row| row.children.as_slice()) else {
            break;
        };
        if children.is_empty() {
            break;
        }
        // The submenu opens beside the row that holds it, kept on screen.
        x = panel_x + width - 2.0;
        y = panel_y + PADDING + offset_of(rows, open);
        rows = children;
        level += 1;
    }
    Some(
        div()
            .absolute()
            .top_0()
            .left_0()
            .w(px(viewport_width))
            .h(px(viewport_height))
            .children(layers)
            .into_any_element(),
    )
}

/// The layer that closes the menu on a click anywhere else.
fn backdrop<V: MenuHost>(cx: &mut Context<V>, width: f32, height: f32) -> AnyElement {
    div()
        .id("drawn-menu-backdrop")
        .absolute()
        .top_0()
        .left_0()
        .w(px(width))
        .h(px(height))
        .occlude()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, _: &MouseDownEvent, _window, cx| close(view, cx)),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|view, _: &MouseDownEvent, _window, cx| close(view, cx)),
        )
        .on_mouse_down(
            MouseButton::Middle,
            cx.listener(|view, _: &MouseDownEvent, _window, cx| close(view, cx)),
        )
        .into_any_element()
}

/// One level of the menu.
#[allow(clippy::too_many_arguments)]
fn panel<V: MenuHost>(
    level: usize,
    rows: &[MenuRow],
    open: Option<usize>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    cx: &mut Context<V>,
) -> AnyElement {
    div()
        .id(("drawn-menu", level))
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(width))
        .h(px(height))
        .p(px(PADDING))
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded(px(8.0))
        .border_1()
        .border_color(rgb(theme::border()))
        .bg(rgb(theme::bg_secondary()))
        .occlude()
        .children(
            rows.iter()
                .enumerate()
                .map(|(index, row)| row_element(row, level, index, open == Some(index), cx)),
        )
        .into_any_element()
}

fn row_element<V: MenuHost>(
    row: &MenuRow,
    level: usize,
    index: usize,
    open: bool,
    cx: &mut Context<V>,
) -> AnyElement {
    if row.rule {
        return div()
            .h(px(RULE))
            .flex_shrink_0()
            .flex()
            .items_center()
            .px(px(6.0))
            .child(div().w_full().h(px(1.0)).bg(rgb(theme::border())))
            .into_any_element();
    }
    let text_color = if row.disabled {
        theme::muted()
    } else {
        theme::fg()
    };
    let mut element = div()
        .id(("drawn-menu-row", level * 1000 + index))
        .h(px(ROW))
        .flex_shrink_0()
        .px(px(8.0))
        .flex()
        .items_center()
        .gap_2()
        .rounded(px(5.0))
        .text_sm()
        .text_color(rgb(text_color));
    if row.children.is_empty() {
        if let Some(id) = row.id.filter(|_| !row.disabled) {
            element = element
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .on_hover(cx.listener(move |view, hovered, _window, cx| {
                    if *hovered {
                        close_submenus(view, level, cx);
                    }
                }))
                .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                    close(view, cx);
                    view.choose(id, window, cx);
                    cx.notify();
                }));
        }
        element.child(row.label.clone()).into_any_element()
    } else {
        element
            .when(open, |this| this.bg(rgb(theme::bg_hover())))
            .cursor_pointer()
            .on_hover(cx.listener(move |view, hovered, _window, cx| {
                if *hovered {
                    open_submenu(view, level, index, cx);
                }
            }))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(row.label.clone()),
            )
            .child(Icon::new(IconName::ChevronRight).size_3())
            .into_any_element()
    }
}

/// Opens the submenu of the row at `level`/`index`, closing any level that was
/// open below it.
fn open_submenu<V: MenuHost>(view: &mut V, level: usize, index: usize, cx: &mut Context<V>) {
    let Some(mut menu) = view.menu().cloned() else {
        return;
    };
    if menu.open.get(level).copied() == Some(index) {
        return;
    }
    menu.open.truncate(level);
    menu.open.push(index);
    view.set_menu(Some(menu));
    cx.notify();
}

/// Closes the levels of `level` and below, if any are open.
fn close_submenus<V: MenuHost>(view: &mut V, level: usize, cx: &mut Context<V>) {
    let Some(mut menu) = view.menu().cloned() else {
        return;
    };
    if menu.open.len() <= level {
        return;
    }
    menu.open.truncate(level);
    view.set_menu(Some(menu));
    cx.notify();
}

/// The width the widest row of `rows` needs. CJK characters are counted wider
/// than Latin ones, so a Chinese entry is not clipped.
fn width_for(rows: &[MenuRow]) -> f32 {
    let mut widest = 0.0_f32;
    for row in rows.iter().filter(|row| !row.rule) {
        let mut width = 0.0_f32;
        for character in row.label.chars() {
            width += if character.is_ascii() { 7.5 } else { 13.5 };
        }
        if !row.children.is_empty() {
            width += 20.0;
        }
        widest = widest.max(width);
    }
    (widest + 28.0).clamp(MIN_WIDTH, MAX_WIDTH)
}

fn height_for(rows: &[MenuRow]) -> f32 {
    PADDING * 2.0
        + rows
            .iter()
            .map(|row| if row.rule { RULE } else { ROW })
            .sum::<f32>()
}

/// How far down a level the row at `index` starts.
fn offset_of(rows: &[MenuRow], index: usize) -> f32 {
    rows.iter()
        .take(index)
        .map(|row| if row.rule { RULE } else { ROW })
        .sum()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// The panel grows with its widest row, Chinese included, and its height
    /// adds a row or a rule per entry.
    #[test]
    fn the_menu_is_sized_by_its_widest_row() {
        let short = vec![MenuRow::entry("Open", true, 1)];
        let long = vec![MenuRow::entry("以管理员身份运行并打开所在位置", true, 1)];
        assert!(width_for(&long) > width_for(&short));
        assert!(width_for(&short) >= MIN_WIDTH);
        let rows = vec![
            MenuRow::entry("A", true, 1),
            MenuRow::rule(),
            MenuRow::entry("B", true, 2),
        ];
        assert_eq!(height_for(&rows), PADDING * 2.0 + ROW * 2.0 + RULE);
        assert_eq!(offset_of(&rows, 2), ROW + RULE);
    }

    /// A disabled entry keeps its id but is never handed to the view, and a
    /// submenu row carries no id of its own.
    #[test]
    fn the_rows_keep_what_they_run() {
        let entry = MenuRow::entry("A", false, 7);
        assert!(entry.disabled);
        assert_eq!(entry.id, Some(7));
        let submenu = MenuRow::submenu("Move", vec![MenuRow::entry("B", true, 8)]);
        assert!(submenu.id.is_none());
        assert_eq!(submenu.children.len(), 1);
        assert!(MenuRow::rule().rule);
    }
}
