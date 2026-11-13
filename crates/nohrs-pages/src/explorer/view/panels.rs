//! The listing with the sidebar and the preview docked around it.
//!
//! Each of the two docks to any side of the listing (`[layout]` in the
//! config, or carried there by its grip). A panel on the left or the right is
//! a column and keeps the width it was dragged to; on the top or the bottom
//! it is a band and keeps its height; the listing takes what is left, as in
//! Explorer: widening the window widens the listing. This replaced
//! gpui-component's resizable group, which on every change of the window or
//! of a panel shared the space out again in proportion and ignored the
//! panels' limits, so the sidebar grew past its maximum and the listing
//! could be squeezed to nothing.
//!
//! The gestures are `crate::dock`'s: the inner edge resizes and, pulled past
//! half the smallest size, hides the panel; a hidden panel's side keeps a
//! strip that drags it open; the grip carries it to another side; an edge
//! no hidden panel claims drags out a second pane.

use std::cell::Cell;

use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::Icon;
use nohrs_core::config::DockSide;
use nohrs_ui::theme::theme;

use crate::dock::{self, Gesture, Resized};
use crate::explorer::ExplorerPane;
use crate::explorer::types::PaneEvent;

/// Room always left to the listing between the panels: across, and down.
const LISTING_MIN: f32 = 220.0;
const LISTING_MIN_HEIGHT: f32 = 120.0;

/// A panel docked around the listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Sidebar,
    Preview,
}

impl Panel {
    const ALL: [Panel; 2] = [Panel::Sidebar, Panel::Preview];

    fn index(self) -> usize {
        match self {
            Panel::Sidebar => 0,
            Panel::Preview => 1,
        }
    }

    /// The smallest and largest size as a column (`true`) or a band.
    fn range(self, column: bool) -> (f32, f32) {
        match (self, column) {
            (Panel::Sidebar, true) => (160.0, 520.0),
            (Panel::Sidebar, false) => (96.0, 600.0),
            (Panel::Preview, true) => (240.0, 4000.0),
            (Panel::Preview, false) => (140.0, 4000.0),
        }
    }

    fn default_size(self, column: bool) -> f32 {
        match (self, column) {
            (Panel::Sidebar, true) => 240.0,
            (Panel::Sidebar, false) => 170.0,
            (Panel::Preview, true) => 340.0,
            (Panel::Preview, false) => 280.0,
        }
    }

    /// The `[layout]` key that says where it docks.
    pub(crate) fn config_key(self) -> &'static str {
        match self {
            Panel::Sidebar => "sidebar",
            Panel::Preview => "preview",
        }
    }
}

/// Where the two panels dock and the sizes they were dragged to, as a column
/// and as a band, so carrying one to the other kind of side and back finds
/// its size again.
#[derive(Clone, Copy)]
struct Arrangement {
    sides: [DockSide; 2],
    // [panel][0 = column width, 1 = band height]
    sizes: [[f32; 2]; 2],
}

thread_local! {
    // `layout.move_grips`: whether the panels' edges carry the grip.
    static MOVE_GRIPS: Cell<bool> = const { Cell::new(false) };
}

/// Shows or hides the grip that carries a panel to another side.
pub(crate) fn set_move_grips(shown: bool) {
    MOVE_GRIPS.set(shown);
}

thread_local! {
    // Shared by every tab, as the panes of Explorer's windows are: a new tab
    // opens with the sides and sizes the last drag left.
    static ARRANGEMENT: Cell<Arrangement> = const {
        Cell::new(Arrangement {
            sides: [DockSide::Left, DockSide::Right],
            sizes: [[240.0, 170.0], [340.0, 280.0]],
        })
    };
}

fn slot(side: DockSide) -> usize {
    if side.is_column() { 0 } else { 1 }
}

/// The side `panel` docks to.
pub(crate) fn side(panel: Panel) -> DockSide {
    ARRANGEMENT.get().sides[panel.index()]
}

/// Docks the panels where `[layout]` says.
pub(crate) fn set_sides(sidebar: DockSide, preview: DockSide) {
    let mut arrangement = ARRANGEMENT.get();
    arrangement.sides = [sidebar, preview];
    ARRANGEMENT.set(arrangement);
}

/// The size `panel` was left at on its side, before fitting.
fn size(panel: Panel) -> f32 {
    let arrangement = ARRANGEMENT.get();
    arrangement.sizes[panel.index()][slot(arrangement.sides[panel.index()])]
}

fn set_size(panel: Panel, value: f32) {
    let mut arrangement = ARRANGEMENT.get();
    let slot = slot(arrangement.sides[panel.index()]);
    arrangement.sizes[panel.index()][slot] = value;
    ARRANGEMENT.set(arrangement);
}

fn range(panel: Panel) -> (f32, f32) {
    panel.range(side(panel).is_column())
}

/// The preview's width as a column, in logical pixels: what a fitted image
/// is asked of the Shell for. As a band the preview is about as wide as the
/// listing, which the window's width bounds the same way.
pub(crate) fn preview_width() -> f32 {
    let arrangement = ARRANGEMENT.get();
    if arrangement.sides[Panel::Preview.index()].is_column() {
        arrangement.sizes[Panel::Preview.index()][0]
    } else {
        arrangement.sizes[Panel::Preview.index()][1] * 2.0
    }
}

/// The width the sidebar has as a column, for compact mode, where it floats
/// over the listing at that width.
pub(crate) fn sidebar_width() -> f32 {
    let (min, max) = Panel::Sidebar.range(true);
    ARRANGEMENT.get().sizes[Panel::Sidebar.index()][0].clamp(min, max)
}

/// What a drag out of an edge can open there, each offered as a bubble.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Opening {
    /// A second pane, on the same folder.
    Split,
    /// A hidden sidebar or preview, docked to that side.
    Panel(Panel),
    /// The terminal, while it is closed, docked to that side.
    Terminal,
}

impl Opening {
    fn icon(self) -> &'static str {
        match self {
            Opening::Split => "icons/columns.svg",
            Opening::Panel(Panel::Sidebar) => "icons/list-tree.svg",
            Opening::Panel(Panel::Preview) => "icons/eye.svg",
            Opening::Terminal => "icons/square-terminal.svg",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Opening::Split => nohrs_core::t!("Nova divisão"),
            Opening::Panel(Panel::Sidebar) => nohrs_core::t!("Barra lateral"),
            Opening::Panel(Panel::Preview) => nohrs_core::t!("Visualizador"),
            Opening::Terminal => nohrs_core::t!("Terminal"),
        }
    }

    /// The smallest size it opens at, as a column (`true`) or a band.
    fn min(self, column: bool) -> f32 {
        match self {
            Opening::Split => 160.0,
            Opening::Panel(panel) => panel.range(column).0,
            Opening::Terminal => {
                if column {
                    TERMINAL_MIN_WIDTH
                } else {
                    TERMINAL_MIN_HEIGHT
                }
            }
        }
    }
}

/// The terminal's smallest width and height, which the page keeps too.
pub(crate) const TERMINAL_MIN_WIDTH: f32 = 240.0;
pub(crate) const TERMINAL_MIN_HEIGHT: f32 = 80.0;

/// A drag of a panel's edge or grip, or of an edge of the listing that opens
/// something there (`Gesture::Open`, `panel` is `None` then).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DockDrag {
    pub panel: Option<Panel>,
    pub side: DockSide,
    pub gesture: Gesture,
    pub pointer: Point<Pixels>,
    /// Where the press was.
    pub origin: Point<Pixels>,
    /// Pulled under half its smallest size: hidden when the button comes up.
    pub collapsing: bool,
    /// An edge pulled past [`dock::OPEN_PULL`]: the bubbles are out.
    pub committed: bool,
    /// The bubble chosen, by its place in [`openings`].
    pub choice: usize,
}

impl DockDrag {
    fn new(panel: Option<Panel>, side: DockSide, gesture: Gesture, pointer: Point<Pixels>) -> Self {
        Self {
            panel,
            side,
            gesture,
            pointer,
            origin: pointer,
            collapsing: false,
            committed: false,
            choice: 0,
        }
    }
}

/// What an edge of this pane's listing can open, in the order the bubbles
/// show: a second pane, the hidden panels, the closed terminal.
pub(crate) fn openings(page: &ExplorerPane) -> Vec<Opening> {
    let mut options = Vec::with_capacity(4);
    if page.split_available {
        options.push(Opening::Split);
    }
    for panel in Panel::ALL {
        if !page.panel_visible(panel) {
            options.push(Opening::Panel(panel));
        }
    }
    if !page.terminal_shown {
        options.push(Opening::Terminal);
    }
    options
}

/// Which of `count` bubbles the pointer chooses: each owns a band across the
/// edge, as in PrimeSFX, so sliding along the edge goes from one to the next.
fn band(side: DockSide, bounds: Bounds<Pixels>, pointer: Point<Pixels>, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let fraction = if side.is_column() {
        f32::from(pointer.y - bounds.top()) / f32::from(bounds.size.height).max(1.0)
    } else {
        f32::from(pointer.x - bounds.left()) / f32::from(bounds.size.width).max(1.0)
    };
    ((fraction * count as f32).floor().max(0.0) as usize).min(count - 1)
}

/// How deep an edge drag reaches into the area, leaving the listing a strip.
fn pull_depth(side: DockSide, bounds: Bounds<Pixels>, pointer: Point<Pixels>) -> f32 {
    let span = f32::from(if side.is_column() { bounds.size.width } else { bounds.size.height });
    dock::depth(side, bounds, pointer).clamp(0.0, (span - 60.0).max(0.0))
}

impl ExplorerPane {
    fn panel_visible(&self, panel: Panel) -> bool {
        match panel {
            Panel::Sidebar => self.sidebar_visible,
            Panel::Preview => self.preview_visible,
        }
    }

    fn set_panel_visible(&mut self, panel: Panel, visible: bool, cx: &mut App) {
        if panel == Panel::Preview && self.preview_visible != visible {
            // Remembered in the config: a window closed with the preview
            // hidden opens that way, as the person left it.
            save_layout("preview_visible", visible.to_string(), cx);
        }
        match panel {
            Panel::Sidebar => self.sidebar_visible = visible,
            Panel::Preview => self.preview_visible = visible,
        }
    }

    /// Starts a drag of `panel`'s inner edge from `pointer`.
    pub(crate) fn start_panel_resize(&mut self, panel: Panel, pointer: Point<Pixels>) {
        let side = side(panel);
        let gesture = Gesture::Resize {
            start: dock::along(side, pointer),
            start_size: size(panel),
        };
        self.dock_drag = Some(DockDrag::new(Some(panel), side, gesture, pointer));
    }

    /// Starts pulling the listing's `side` edge, to open something there.
    pub(crate) fn start_edge_pull(&mut self, side: DockSide, pointer: Point<Pixels>) {
        self.dock_drag = Some(DockDrag::new(None, side, Gesture::Open, pointer));
    }

    /// Starts carrying `panel` by its grip.
    pub(crate) fn start_panel_move(&mut self, panel: Panel, pointer: Point<Pixels>) {
        self.dock_drag = Some(DockDrag::new(Some(panel), side(panel), Gesture::Move, pointer));
    }

    /// Esc during a drag: nothing happens.
    pub(crate) fn cancel_dock_drag(&mut self) -> bool {
        self.dock_drag.take().is_some()
    }

    /// Follows a dock drag to `pointer`. Returns whether one is under way.
    pub(crate) fn update_dock_drag(&mut self, pointer: Point<Pixels>) -> bool {
        let Some(mut drag) = self.dock_drag else {
            return false;
        };
        drag.pointer = pointer;
        match (drag.gesture, drag.panel) {
            (Gesture::Resize { start, start_size }, Some(panel)) => {
                let (min, max) = range(panel);
                let raw = start_size + (dock::along(drag.side, pointer) - start) * dock::growth(drag.side);
                match dock::resized(raw, min, max) {
                    Resized::Size(value) => {
                        drag.collapsing = false;
                        set_size(panel, value);
                    }
                    Resized::Collapse => drag.collapsing = true,
                }
            }
            (Gesture::Open, _) => {
                let moved = (dock::along(drag.side, pointer) - dock::along(drag.side, drag.origin)).abs();
                drag.committed |= moved >= dock::OPEN_PULL;
                if drag.committed {
                    drag.choice = band(drag.side, self.columns_bounds.get(), pointer, openings(self).len());
                }
            }
            _ => {}
        }
        self.dock_drag = Some(drag);
        true
    }

    /// Ends a dock drag: hides a panel pulled shut, docks a carried one where
    /// it was let go, or opens what the chosen bubble offers. Returns whether
    /// one ended.
    pub(crate) fn finish_dock_drag(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.dock_drag.take() else {
            return false;
        };
        let bounds = self.columns_bounds.get();
        match (drag.gesture, drag.panel) {
            (Gesture::Resize { start_size, .. }, Some(panel)) if drag.collapsing => {
                // Hidden at the size it had, which it opens at again.
                set_size(panel, start_size);
                self.set_panel_visible(panel, false, cx);
            }
            (Gesture::Move, Some(panel)) => {
                let target = dock::nearest_side(bounds, drag.pointer);
                if target != drag.side {
                    dock_panel(panel, target, cx);
                    cx.refresh_windows();
                }
            }
            (Gesture::Open, _) if drag.committed => {
                let Some(&opening) = openings(self).get(drag.choice) else {
                    return true;
                };
                let column = drag.side.is_column();
                let depth = pull_depth(drag.side, bounds, drag.pointer);
                // Let go too near the edge, nothing opens: the zone and its
                // line turned amber to say so.
                if depth < opening.min(column) * dock::COLLAPSE_FRACTION {
                    return true;
                }
                match opening {
                    Opening::Split => {
                        let across = f32::from(if column { bounds.size.width } else { bounds.size.height });
                        if across > 0.0 {
                            cx.emit(PaneEvent::SplitRequested {
                                side: drag.side,
                                fraction: (depth / across).clamp(0.15, 0.85),
                            });
                        }
                    }
                    Opening::Panel(panel) => {
                        if side(panel) != drag.side {
                            dock_panel(panel, drag.side, cx);
                        }
                        let (min, max) = range(panel);
                        set_size(panel, depth.clamp(min, max));
                        self.set_panel_visible(panel, true, cx);
                        cx.refresh_windows();
                    }
                    Opening::Terminal => cx.emit(PaneEvent::TerminalRequested {
                        side: drag.side,
                        size: depth.max(opening.min(column)),
                    }),
                }
            }
            _ => {}
        }
        true
    }

    /// Back to the default size, from a double click on the panel's edge.
    pub(crate) fn reset_panel_size(&mut self, panel: Panel) {
        set_size(panel, panel.default_size(side(panel).is_column()));
    }
}

/// Docks `panel` on `side` now, in every tab, and remembers it in the config
/// so the next start opens that way. The write goes off the UI thread; the
/// config watcher then applies the same sides again, which changes nothing.
pub(crate) fn dock_panel(panel: Panel, side: DockSide, cx: &mut App) {
    let mut arrangement = ARRANGEMENT.get();
    arrangement.sides[panel.index()] = side;
    ARRANGEMENT.set(arrangement);
    save_layout(panel.config_key(), format!("\"{}\"", side.as_str()), cx);
}

/// Writes `layout.<key> = value` to the config file in the background.
pub(crate) fn save_layout(key: &'static str, value: String, cx: &mut App) {
    // A test's window is not the person's window: it writes no config.
    if cfg!(test) {
        let _ = (key, value, cx);
        return;
    }
    cx.background_spawn(async move {
        if let Err(error) =
            nohrs_core::config::set_value(&nohrs_core::config::paths::config_file(), "layout", key, &value)
        {
            tracing::warn!("could not save layout.{key}: {error}");
        }
    })
    .detach();
}

/// A panel to lay out around the listing, and how far out compact mode has
/// slid it (1.0 otherwise); one part way out has no edge to drag.
pub(crate) struct Docked {
    pub panel: Panel,
    pub element: AnyElement,
    pub amount: f32,
}

/// The listing with its panels around it, each panel's edge with a strip to
/// drag it by, and the strips of hidden panels and of the split along the
/// area's edges.
pub(crate) fn render(
    page: &mut ExplorerPane,
    docked: Vec<Docked>,
    listing: AnyElement,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let bounds = page.columns_bounds.get();
    let collapsing = page.dock_drag.filter(|drag| drag.collapsing).and_then(|drag| drag.panel);
    let resizing = page
        .dock_drag
        .filter(|drag| matches!(drag.gesture, Gesture::Resize { .. }))
        .and_then(|drag| drag.panel);

    // Fit the columns into the width and the bands into the height, the
    // sidebar (listed first) keeping its size longest.
    let mut fitted: Vec<(Panel, DockSide, f32, f32, AnyElement)> = Vec::new();
    for column in [true, false] {
        let mine: Vec<&Docked> = docked
            .iter()
            .filter(|item| side(item.panel).is_column() == column && collapsing != Some(item.panel))
            .collect();
        let mut sizes: Vec<(f32, f32)> = mine.iter().map(|item| (size(item.panel) * item.amount, 0.0)).collect();
        let ranges: Vec<(f32, f32)> = mine
            .iter()
            .map(|item| {
                let (min, max) = range(item.panel);
                (min * item.amount, max)
            })
            .collect();
        let (total, keep) = if column {
            (f32::from(bounds.size.width), LISTING_MIN)
        } else {
            (f32::from(bounds.size.height), LISTING_MIN_HEIGHT)
        };
        dock::fit(&mut sizes, &ranges, total, keep);
        for (item, (fitted_size, _)) in mine.iter().zip(sizes) {
            fitted.push((item.panel, side(item.panel), fitted_size, item.amount, div().into_any_element()));
        }
    }
    // The elements themselves, moved into their fitted places.
    for item in docked {
        if let Some(entry) = fitted.iter_mut().find(|entry| entry.0 == item.panel) {
            entry.4 = item.element;
        }
    }

    let dragging = page.dock_drag;
    // The columns beside the listing, which a band above or below it does
    // not reach: the landing of a panel carried there is drawn between them.
    let carried = dragging.filter(|drag| drag.gesture == Gesture::Move).and_then(|drag| drag.panel);
    let beside = |wanted: DockSide| -> f32 {
        fitted
            .iter()
            .filter(|entry| entry.1 == wanted && Some(entry.0) != carried)
            .map(|entry| entry.2)
            .sum()
    };
    let (left_room, right_room) = (beside(DockSide::Left), beside(DockSide::Right));
    let mut boxes: [Vec<AnyElement>; 4] = Default::default();
    for (panel, panel_side, fitted_size, amount, element) in fitted {
        // `amount` already shrank the size compact mode is sliding.
        let frame = dock::frame(panel_side, fitted_size, 1.0).when(amount <= 0.0, |frame| frame.border_0());
        let frame = frame.child(element);
        let frame = if amount >= 1.0 {
            frame.child(edge_strip(page, panel, panel_side, resizing == Some(panel), cx))
        } else {
            frame
        };
        boxes[side_index(panel_side)].push(frame.into_any_element());
    }
    let [left, right, top, bottom] = boxes;

    let center = div()
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .flex()
        .flex_col()
        .min_h(px(0.0))
        .overflow_hidden()
        .children(top)
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(listing),
        )
        // The panel listed first is the outermost on its side.
        .children(bottom.into_iter().rev());

    let mut edges: Vec<AnyElement> = Vec::new();
    // Every edge of the listing opens whatever is closed (a second pane, the
    // hidden panels, the terminal), chosen from bubbles as it is pulled.
    let offered = !openings(page).is_empty();
    for edge in [DockSide::Left, DockSide::Right, DockSide::Top, DockSide::Bottom] {
        if !offered {
            break;
        }
        // A hidden panel docked here opens on a double click, at its size.
        let claimant = Panel::ALL
            .into_iter()
            .find(|&panel| side(panel) == edge && !page.panel_visible(panel) && collapsing != Some(panel));
        let id = SharedString::from(format!("dock-edge-{}", edge.as_str()));
        let pulling = dragging.is_some_and(|drag| drag.side == edge && drag.gesture == Gesture::Open);
        let strip = dock::place_on_edge(dock::strip(id, edge, pulling), edge, dock::EDGE).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
                match claimant {
                    Some(panel) if event.click_count >= 2 => {
                        this.set_panel_visible(panel, true, cx);
                        this.dock_drag = None;
                    }
                    _ => this.start_edge_pull(edge, event.position),
                }
                cx.notify();
            }),
        );
        edges.push(strip.into_any_element());
    }

    // What the drag under way would do, drawn over everything.
    let guide = dragging.and_then(|drag| match (drag.gesture, drag.panel) {
        (Gesture::Move, Some(panel)) => {
            let target = dock::nearest_side(bounds, drag.pointer);
            let (min, _) = panel.range(target.is_column());
            let room = if target.is_column() { bounds.size.width } else { bounds.size.height };
            let depth = panel_size_on(panel, target).min(f32::from(room) * 0.45).max(min.min(f32::from(room)));
            let landing = dock::landing(target, depth);
            let landing = if target.is_column() { landing } else { landing.left(px(left_room)).right(px(right_room)) };
            Some(landing.into_any_element())
        }
        (Gesture::Open, _) if drag.committed => Some(chooser(page, &drag, bounds)),
        _ => None,
    });

    div()
        .relative()
        .flex()
        .flex_row()
        .flex_grow()
        .min_h(px(0.0))
        .min_w(px(0.0))
        .children(left)
        .child(center)
        .children(right.into_iter().rev())
        .child(dock::measure(page.columns_bounds.clone()))
        .children(edges)
        .children(guide)
        .into_any_element()
}

/// Amber, for a pull that would open nothing yet.
const TOO_NEAR: u32 = 0xF59E0B;
/// Half a chosen bubble's size: its place is its centre.
const GROWN_HALF: f32 = 29.0;

/// The bubbles of an edge pulled out, after PrimeSFX's: the room the new
/// thing would take, tinted, with a glowing line under the pointer, and in the
/// middle of that room one bubble per thing that can open there, each owning
/// a band across the edge, so sliding along it picks. The chosen one swells
/// into the accent with a halo and its name; the size it would open at rides
/// on the line. Let go too near the edge and it all turns amber: nothing
/// opens.
fn chooser(page: &ExplorerPane, drag: &DockDrag, bounds: Bounds<Pixels>) -> AnyElement {
    let options = openings(page);
    let side = drag.side;
    let column = side.is_column();
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let depth = pull_depth(side, bounds, drag.pointer);
    let chosen = options.get(drag.choice).copied();
    let too_near = chosen.is_none_or(|opening| depth < opening.min(column) * dock::COLLAPSE_FRACTION);
    let tone = if too_near { TOO_NEAR } else { theme::accent() };
    let glow = |alpha: f32| -> Hsla {
        let mut color: Hsla = rgb(tone).into();
        color.a = alpha;
        color
    };

    let zone = div()
        .absolute()
        .bg(rgba((tone << 8) | 0x1A))
        .border_1()
        .border_dashed()
        .border_color(rgba((tone << 8) | 0xA0))
        .rounded(px(10.0))
        .shadow(vec![BoxShadow {
            color: glow(0.10),
            offset: point(px(0.0), px(0.0)),
            blur_radius: px(32.0),
            spread_radius: px(0.0),
        }]);
    let zone = match side {
        DockSide::Left => zone.top_0().bottom_0().left_0().w(px(depth)),
        DockSide::Right => zone.top_0().bottom_0().right_0().w(px(depth)),
        DockSide::Top => zone.left_0().right_0().top_0().h(px(depth)),
        DockSide::Bottom => zone.left_0().right_0().bottom_0().h(px(depth)),
    };
    let line = div()
        .absolute()
        .bg(rgb(tone))
        .rounded(px(2.0))
        .shadow(vec![BoxShadow {
            color: glow(0.65),
            offset: point(px(0.0), px(0.0)),
            blur_radius: px(12.0),
            spread_radius: px(0.0),
        }]);
    let line = match side {
        DockSide::Left => line.top_0().bottom_0().left(px(depth - 1.5)).w(px(3.0)),
        DockSide::Right => line.top_0().bottom_0().right(px(depth - 1.5)).w(px(3.0)),
        DockSide::Top => line.left_0().right_0().top(px(depth - 1.5)).h(px(3.0)),
        DockSide::Bottom => line.left_0().right_0().bottom(px(depth - 1.5)).h(px(3.0)),
    };

    // A point `inward` from the edge and `across` along it, in the area.
    let at = |inward: f32, across: f32| -> (f32, f32) {
        match side {
            DockSide::Left => (inward, across),
            DockSide::Right => (width - inward, across),
            DockSide::Top => (across, inward),
            DockSide::Bottom => (across, height - inward),
        }
    };
    let length = if column { height } else { width };
    // In the middle of the room, or against the line while the room is
    // still too narrow to hold a bubble.
    let inward = (depth / 2.0).min(depth - 40.0).max(40.0);
    let count = options.len().max(1);
    let mut layers: Vec<AnyElement> = vec![zone.into_any_element(), line.into_any_element()];
    for (index, opening) in options.iter().enumerate() {
        let picked = Some(*opening) == chosen && !too_near;
        let (x, y) = at(inward, (index as f32 + 0.5) / count as f32 * length);
        layers.push(bubble(*opening, picked, index, drag.choice, too_near).left(px(x - 60.0)).top(px(y - GROWN_HALF)).into_any_element());
    }

    // The size it would open at, on the line, facing the room; never closer
    // to the edge than its own half width, where the window would cut it.
    let (x, y) = at((depth + 18.0).max(if column { 78.0 } else { 18.0 }), length / 2.0);
    let readout = if too_near {
        nohrs_core::t!("Solte mais para dentro").to_string()
    } else {
        format!("{} px", depth.round() as i32)
    };
    layers.push(
        div()
            .absolute()
            .left(px(x - 70.0))
            .top(px(y - 11.0))
            .w(px(140.0))
            .flex()
            .justify_center()
            .child(
                div()
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded_full()
                    .bg(rgb(tone))
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(0xFFFFFF))
                    .whitespace_nowrap()
                    .child(readout),
            )
            .into_any_element(),
    );
    // How to pick, once there is more than one bubble and room to say it.
    if options.len() > 1 && depth >= 150.0 && !too_near {
        let hint = if column {
            nohrs_core::t!("Suba ou desça para escolher")
        } else {
            nohrs_core::t!("Vá para os lados para escolher")
        };
        let hint = div()
            .absolute()
            .flex()
            .justify_center()
            .child(
                div()
                    .px(px(10.0))
                    .py(px(3.0))
                    .rounded_full()
                    .bg(rgba((theme::bg() << 8) | 0xD0))
                    .border_1()
                    .border_color(rgb(theme::border()))
                    .text_xs()
                    .text_color(rgb(theme::muted()))
                    .whitespace_nowrap()
                    .child(hint),
            );
        let hint = match side {
            DockSide::Left => hint.left_0().w(px(depth)).bottom(px(12.0)),
            DockSide::Right => hint.right_0().w(px(depth)).bottom(px(12.0)),
            DockSide::Top => hint.left_0().right_0().top(px(depth - 34.0)),
            DockSide::Bottom => hint.left_0().right_0().bottom(px(depth - 34.0)),
        };
        layers.push(hint.into_any_element());
    }
    div().absolute().inset_0().children(layers).into_any_element()
}

// One bubble: a disc with the thing's icon and its name under it. The chosen
// one pops (a short ease-out swell, started again whenever the choice moves)
// into the accent, with a halo; the others sit back, muted.
fn bubble(opening: Opening, picked: bool, index: usize, choice: usize, too_near: bool) -> Div {
    const REST: f32 = 46.0;
    const GROWN: f32 = 58.0;
    let icon = Icon::new(Icon::empty())
        .path(opening.icon())
        .size(px(if picked { 24.0 } else { 20.0 }))
        .text_color(rgb(if picked { 0xFFFFFF } else { theme::fg_secondary() }));
    let disc = div()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .child(icon);
    let disc: AnyElement = if picked {
        disc.bg(rgb(theme::accent()))
            .border_2()
            .border_color(rgb(theme::accent_hover()))
            .shadow(vec![
                BoxShadow {
                    color: {
                        let mut halo: Hsla = rgb(theme::accent()).into();
                        halo.a = 0.28;
                        halo
                    },
                    offset: point(px(0.0), px(0.0)),
                    blur_radius: px(0.0),
                    spread_radius: px(6.0),
                },
                BoxShadow {
                    color: hsla(0.0, 0.0, 0.0, 0.45),
                    offset: point(px(0.0), px(6.0)),
                    blur_radius: px(18.0),
                    spread_radius: px(0.0),
                },
            ])
            .with_animation(
                ("bubble-pop", index * 16 + choice),
                Animation::new(Duration::from_millis(170)).with_easing(ease_out_quint()),
                |disc, delta| disc.size(px(REST + (GROWN - REST) * delta)),
            )
            .into_any_element()
    } else {
        disc.size(px(REST))
            .bg(rgb(theme::toolbar_bg()))
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow(vec![BoxShadow {
                color: hsla(0.0, 0.0, 0.0, 0.40),
                offset: point(px(0.0), px(4.0)),
                blur_radius: px(14.0),
                spread_radius: px(0.0),
            }])
            .into_any_element()
    };
    let label = div()
        .px(px(9.0))
        .py(px(2.0))
        .rounded_full()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
        .when(picked, |label| label.bg(rgba((theme::accent() << 8) | 0xE0)).text_color(rgb(0xFFFFFF)))
        .when(!picked, |label| label.bg(rgba((theme::bg() << 8) | 0xC8)).text_color(rgb(theme::muted())))
        .child(opening.label());
    // A fixed box centred on the bubble's place, so the swell grows about
    // its centre and the name keeps its line.
    div()
        .absolute()
        .w(px(120.0))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(6.0))
        .when(too_near, |this| this.opacity(0.45))
        .child(div().size(px(GROWN)).flex().items_center().justify_center().child(disc))
        .child(label)
}

/// The size `panel` would have docked on `side`.
fn panel_size_on(panel: Panel, side: DockSide) -> f32 {
    ARRANGEMENT.get().sizes[panel.index()][slot(side)]
}

fn side_index(side: DockSide) -> usize {
    match side {
        DockSide::Left => 0,
        DockSide::Right => 1,
        DockSide::Top => 2,
        DockSide::Bottom => 3,
    }
}

// The strip over a panel's inner edge: drag to resize (past half its
// smallest size it hides), double click for the default size, and in its
// middle the grip that carries the panel to another side.
fn edge_strip(
    page: &ExplorerPane,
    panel: Panel,
    side: DockSide,
    held: bool,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement {
    let id = SharedString::from(format!("dock-strip-{}", panel.config_key()));
    let carrying = page
        .dock_drag
        .is_some_and(|drag| drag.panel == Some(panel) && drag.gesture == Gesture::Move);
    let grip = dock::grip(SharedString::from(format!("dock-grip-{}", panel.config_key())), side)
        .when(carrying, |grip| grip.bg(gpui::rgb(nohrs_ui::theme::theme::accent())))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
                this.start_panel_move(panel, event.position);
                cx.notify();
            }),
        );
    dock::place_on_inner_edge(dock::strip(id, side, held), side)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
                if event.click_count >= 2 {
                    this.reset_panel_size(panel);
                    this.dock_drag = None;
                } else {
                    this.start_panel_resize(panel, event.position);
                }
                cx.notify();
            }),
        )
        .when(MOVE_GRIPS.get(), |strip| strip.child(grip))
}

#[cfg(test)]
mod tests {
    use super::{Panel, set_sides, set_size, size};
    use nohrs_core::config::DockSide;

    #[test]
    fn a_panel_keeps_a_size_per_kind_of_side() {
        set_sides(DockSide::Left, DockSide::Right);
        set_size(Panel::Preview, 400.0);
        set_sides(DockSide::Left, DockSide::Bottom);
        set_size(Panel::Preview, 200.0);
        set_sides(DockSide::Left, DockSide::Right);
        assert_eq!(size(Panel::Preview), 400.0);
        set_sides(DockSide::Left, DockSide::Top);
        assert_eq!(size(Panel::Preview), 200.0);
        set_sides(DockSide::Left, DockSide::Right);
    }
}
