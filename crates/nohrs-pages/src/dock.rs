//! Panels docked to a side of something: the sidebar and the preview to a
//! side of the listing, the terminal to a side of the panes.
//!
//! The same gestures for all of them, as in VS Code and Zen: the strip on a
//! panel's inner edge resizes it, and dragged past half its smallest size the
//! panel closes; a closed panel leaves a strip on its side's edge that drags
//! it open again; the grip in the middle of the strip carries the panel to
//! another side. Where no closed panel claims an edge, dragging that edge
//! splits the listing instead.
//!
//! This module holds the arithmetic and the drawing the panels share; the
//! views keep their own drag state and route the mouse to it.

use gpui::prelude::FluentBuilder;
use gpui::*;
use nohrs_ui::theme::theme;

pub use nohrs_core::config::DockSide;

/// Depth of the strip a drag grabs, along a panel's inner edge or along an
/// edge where a closed panel waits.
pub(crate) const GRIP: f32 = 7.0;

/// A resize past this fraction of a panel's smallest size closes it. Above
/// it and under the smallest size, the panel holds at the smallest size, so
/// closing is a deliberate pull and not a slip.
pub(crate) const COLLAPSE_FRACTION: f32 = 0.5;

/// Depth of the strip along an edge where a closed panel waits, or that
/// splits: thinner than a panel's own, since it lies over the listing's
/// scroll bar on the right and its column headings at the top, which keep
/// all but these few pixels.
pub(crate) const EDGE: f32 = 4.0;

/// How far along a hidden panel's edge the pointer must pull before the
/// panel opens: any less is a click on the edge, not a drag.
pub(crate) const OPEN_PULL: f32 = 24.0;

/// What a drag of a docked panel is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Gesture {
    /// Its inner edge: the pointer's position along the axis when the drag
    /// started, and the panel's size then.
    Resize { start: f32, start_size: f32 },
    /// The edge a closed panel waits at, pulled inward.
    Open,
    /// Its grip, toward another side.
    Move,
}

/// The pointer's coordinate along the axis a panel on `side` is sized on.
pub(crate) fn along(side: DockSide, point: Point<Pixels>) -> f32 {
    if side.is_column() {
        f32::from(point.x)
    } else {
        f32::from(point.y)
    }
}

/// +1 where moving the pointer toward larger coordinates grows the panel
/// (its inner edge is at its right or bottom), -1 otherwise.
pub(crate) fn growth(side: DockSide) -> f32 {
    match side {
        DockSide::Left | DockSide::Top => 1.0,
        DockSide::Right | DockSide::Bottom => -1.0,
    }
}

/// How far `point` is inside `bounds` from its `side` edge.
pub(crate) fn depth(side: DockSide, bounds: Bounds<Pixels>, point: Point<Pixels>) -> f32 {
    match side {
        DockSide::Left => f32::from(point.x - bounds.left()),
        DockSide::Right => f32::from(bounds.right() - point.x),
        DockSide::Top => f32::from(point.y - bounds.top()),
        DockSide::Bottom => f32::from(bounds.bottom() - point.y),
    }
}

/// The result of dragging a panel's edge to `raw`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Resized {
    /// The panel stays, at this size.
    Size(f32),
    /// Pulled under [`COLLAPSE_FRACTION`] of its smallest size: it closes
    /// when the button comes up.
    Collapse,
}

/// What dragging an edge to `raw` makes of a panel whose sizes run from
/// `min` to `max`.
pub(crate) fn resized(raw: f32, min: f32, max: f32) -> Resized {
    if raw < min * COLLAPSE_FRACTION {
        Resized::Collapse
    } else {
        Resized::Size(raw.clamp(min, max.max(min)))
    }
}

/// The side of `bounds` the pointer is nearest to, in proportion to the
/// area's width and height, so a tall window does not favour its sides.
pub(crate) fn nearest_side(bounds: Bounds<Pixels>, point: Point<Pixels>) -> DockSide {
    let width = f32::from(bounds.size.width).max(1.0);
    let height = f32::from(bounds.size.height).max(1.0);
    let distances = [
        (DockSide::Left, depth(DockSide::Left, bounds, point) / width),
        (
            DockSide::Right,
            depth(DockSide::Right, bounds, point) / width,
        ),
        (DockSide::Top, depth(DockSide::Top, bounds, point) / height),
        (
            DockSide::Bottom,
            depth(DockSide::Bottom, bounds, point) / height,
        ),
    ];
    distances
        .into_iter()
        .min_by(|one, other| one.1.total_cmp(&other.1))
        .map(|(side, _)| side)
        .unwrap_or(DockSide::Right)
}

/// Fits the sizes of the panels on two opposite sides into `total`, leaving
/// `keep` to what they surround: each panel within its range, and the inner
/// ones (later in each list) giving way first, so the sidebar outlasts a
/// preview stacked inside it.
pub(crate) fn fit(sizes: &mut [(f32, f32)], ranges: &[(f32, f32)], total: f32, keep: f32) {
    for (size, (min, max)) in sizes.iter_mut().zip(ranges) {
        size.0 = size.0.clamp(*min, max.max(*min));
    }
    if total <= 0.0 {
        return;
    }
    let mut excess = sizes.iter().map(|size| size.0).sum::<f32>() - (total - keep).max(0.0);
    for (size, (min, _)) in sizes.iter_mut().zip(ranges).rev() {
        if excess <= 0.0 {
            break;
        }
        let give = (size.0 - min).min(excess).max(0.0);
        size.0 -= give;
        excess -= give;
    }
}

/// A panel's box on `side`, `size` deep (scaled by `amount` while compact
/// mode slides it), with the separator line on its inner side.
pub(crate) fn frame(side: DockSide, size: f32, amount: f32) -> Div {
    let shown = size * amount;
    let base = div()
        .relative()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .overflow_hidden();
    let base = if side.is_column() {
        base.w(px(shown)).h_full()
    } else {
        base.h(px(shown)).w_full()
    };
    let base = if amount > 0.0 {
        match side {
            DockSide::Left => base.border_r_1(),
            DockSide::Right => base.border_l_1(),
            DockSide::Top => base.border_b_1(),
            DockSide::Bottom => base.border_t_1(),
        }
    } else {
        base
    };
    base.border_color(rgb(theme::border()))
}

/// Where the strip over a panel's inner edge goes, inside the panel's box.
pub(crate) fn place_on_inner_edge(strip: Stateful<Div>, side: DockSide) -> Stateful<Div> {
    let strip = strip.absolute();
    match side {
        DockSide::Left => strip.top_0().bottom_0().right_0().w(px(GRIP)),
        DockSide::Right => strip.top_0().bottom_0().left_0().w(px(GRIP)),
        DockSide::Top => strip.left_0().right_0().bottom_0().h(px(GRIP)),
        DockSide::Bottom => strip.left_0().right_0().top_0().h(px(GRIP)),
    }
}

/// Where a strip along the `side` edge of an area goes, inside it.
pub(crate) fn place_on_edge(strip: Stateful<Div>, side: DockSide, depth: f32) -> Stateful<Div> {
    let strip = strip.absolute();
    match side {
        DockSide::Left => strip.top_0().bottom_0().left_0().w(px(depth)),
        DockSide::Right => strip.top_0().bottom_0().right_0().w(px(depth)),
        DockSide::Top => strip.left_0().right_0().top_0().h(px(depth)),
        DockSide::Bottom => strip.left_0().right_0().bottom_0().h(px(depth)),
    }
}

/// The look of a resize strip on a panel on `side`: the resize cursor, and
/// a line that lights up while it is held or under the mouse.
pub(crate) fn strip(id: impl Into<ElementId>, side: DockSide, held: bool) -> Stateful<Div> {
    let line = div().bg(rgb(theme::accent()));
    let line = if side.is_column() {
        line.w(px(1.0)).h_full()
    } else {
        line.h(px(1.0)).w_full()
    };
    // `occlude`: in gpui every hitbox under the pointer sees a press, not
    // only the top one, and the text editor in the preview took the drag of
    // its own edge as a selection (05/11/2026).
    let base = div()
        .id(id)
        .occlude()
        .flex()
        .items_center()
        .justify_center();
    let base = if side.is_column() {
        base.cursor_col_resize()
    } else {
        base.cursor_row_resize()
    };
    base.hover(|style| style.bg(rgba((theme::accent() << 8) | 0x30)))
        .when(held, |this| this.child(line))
}

/// The grip in the middle of a resize strip that carries the panel to
/// another side: a short pill across the strip.
pub(crate) fn grip(id: impl Into<ElementId>, side: DockSide) -> Stateful<Div> {
    let pill = div()
        .id(id)
        .flex_shrink_0()
        .rounded(px(3.0))
        .bg(rgb(theme::border()))
        .cursor(CursorStyle::OpenHand)
        .hover(|style| style.bg(rgb(theme::accent())));
    if side.is_column() {
        pill.w(px(5.0)).h(px(36.0))
    } else {
        pill.h(px(5.0)).w(px(36.0))
    }
}

/// While a panel is carried, the part of the area it would take on the
/// side it would land on, tinted in the accent colour.
pub(crate) fn landing(side: DockSide, depth: f32) -> Div {
    let zone = div()
        .absolute()
        .bg(rgba((theme::accent() << 8) | 0x38))
        .border_2()
        .border_color(rgb(theme::accent()))
        .rounded(px(6.0));
    match side {
        DockSide::Left => zone.top_0().bottom_0().left_0().w(px(depth)),
        DockSide::Right => zone.top_0().bottom_0().right_0().w(px(depth)),
        DockSide::Top => zone.left_0().right_0().top_0().h(px(depth)),
        DockSide::Bottom => zone.left_0().right_0().bottom_0().h(px(depth)),
    }
}

/// Measures the area it is put in, absolutely over it, into `bounds`, and
/// asks for another frame when the size changed so the panels refit.
pub(crate) fn measure(bounds: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>) -> impl IntoElement {
    canvas(
        move |area, window, _| {
            let before = bounds.replace(area);
            if (f32::from(before.size.width) - f32::from(area.size.width)).abs() > 0.5
                || (f32::from(before.size.height) - f32::from(area.size.height)).abs() > 0.5
            {
                window.refresh();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::{DockSide, Resized, depth, fit, nearest_side, resized};
    use gpui::{Bounds, Pixels, point, px, size};

    fn area() -> Bounds<Pixels> {
        Bounds::new(point(px(100.0), px(50.0)), size(px(1000.0), px(500.0)))
    }

    #[test]
    fn a_pull_under_half_the_smallest_size_closes() {
        assert_eq!(resized(70.0, 160.0, 520.0), Resized::Collapse);
        assert_eq!(resized(100.0, 160.0, 520.0), Resized::Size(160.0));
        assert_eq!(resized(900.0, 160.0, 520.0), Resized::Size(520.0));
    }

    #[test]
    fn depth_is_measured_from_each_edge() {
        let pointer = point(px(150.0), px(500.0));
        assert_eq!(depth(DockSide::Left, area(), pointer), 50.0);
        assert_eq!(depth(DockSide::Right, area(), pointer), 950.0);
        assert_eq!(depth(DockSide::Top, area(), pointer), 450.0);
        assert_eq!(depth(DockSide::Bottom, area(), pointer), 50.0);
    }

    #[test]
    fn the_nearest_side_weighs_width_and_height_alike() {
        // 150 px from the left of a 1000 px wide area is 15%; 100 px from
        // the bottom of a 500 px tall one is 20%, so the left wins.
        assert_eq!(
            nearest_side(area(), point(px(250.0), px(450.0))),
            DockSide::Left
        );
        assert_eq!(
            nearest_side(area(), point(px(600.0), px(530.0))),
            DockSide::Bottom
        );
        assert_eq!(
            nearest_side(area(), point(px(1080.0), px(300.0))),
            DockSide::Right
        );
    }

    #[test]
    fn the_inner_panel_gives_way_first() {
        let mut sizes = [(400.0, 0.0), (500.0, 0.0)];
        fit(
            &mut sizes,
            &[(160.0, 520.0), (240.0, 2000.0)],
            1000.0,
            220.0,
        );
        assert_eq!(sizes[0].0, 400.0);
        assert_eq!(sizes[1].0, 380.0);
        let mut sizes = [(400.0, 0.0), (500.0, 0.0)];
        fit(&mut sizes, &[(160.0, 520.0), (240.0, 2000.0)], 500.0, 220.0);
        assert_eq!((sizes[0].0, sizes[1].0), (160.0, 240.0));
    }
}
