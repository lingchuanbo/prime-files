//! Compact mode, as in Zen Browser: bars that stay hidden and slide in from
//! their edge of the window while the pointer rests there.
//!
//! The tab row (in `pane_group.rs`) and the explorer's address bar, sidebar,
//! preview and bottom bar (in `explorer/view.rs`) all slide the same way: a
//! box clipped to `extent * amount` along its axis, with the bar anchored to
//! the far side so it comes out of the edge rather than unrolling. A strip
//! along the edge watches the pointer and grows with the bar, so moving from
//! the edge onto the bar keeps it out.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::*;
use nohrs_core::config::Compact;

/// Longest step one frame may advance the slide. Frames stop while nothing
/// moves, so the first one after a pause would otherwise jump to the end.
const MAX_FRAME_STEP: Duration = Duration::from_millis(50);

/// Hover sources of the top edge: the tab row and the address bar slide as
/// one, and either holding the pointer keeps both out.
pub const SOURCE_TABS: u8 = 1;
/// The address bar's hover source on the top edge.
pub const SOURCE_HEADER: u8 = 2;
/// The only source of an edge with one bar.
pub const SOURCE_EDGE: u8 = 1;

/// Where one edge's bars are in their slide. Cells, behind an `Rc`, because
/// the hover listeners run outside the render that owns the state.
#[derive(Default)]
pub struct Reveal {
    /// One bit per [`SOURCE_TABS`]-style source the pointer is over.
    hovered: Cell<u8>,
    /// Out regardless of the pointer: a path or a filter being typed.
    forced: Cell<bool>,
    /// Left where it is regardless of the pointer: a menu is open, and the
    /// pointer going into it must not take away the bar it came from.
    held: Cell<bool>,
    /// When the pointer last arrived or left; the delays count from here.
    hover_changed: Cell<Option<Instant>>,
    /// Where the bars are heading once the delay has passed.
    shown: Cell<bool>,
    /// Position in the slide, 0 = hidden and 1 = shown, linear in time.
    position: Cell<f32>,
    last_tick: Cell<Option<Instant>>,
    /// How far down the top edge's floating tab row reaches, so the address
    /// bar floats under it rather than behind it. Set by the tab row's owner
    /// each frame, before the address bar's owner reads it.
    inset: Cell<f32>,
}

impl Reveal {
    /// Returns whether anything changed, so pointer moves over a bar that is
    /// already out do not redraw the window.
    pub fn set_hovered(&self, source: u8, hovered: bool, now: Instant) -> bool {
        let before = self.hovered.get();
        let after = if hovered {
            before | source
        } else {
            before & !source
        };
        self.hovered.set(after);
        if (before != 0) != (after != 0) {
            self.hover_changed.set(Some(now));
        }
        before != after
    }

    /// Shows the bar while `forced`. Returns whether that changed, so the
    /// caller can ask for a frame.
    pub fn set_forced(&self, forced: bool) -> bool {
        self.forced.replace(forced) != forced
    }

    /// Keeps the bar where it is while `held`. Returns whether that changed.
    pub fn set_held(&self, held: bool) -> bool {
        self.held.replace(held) != held
    }

    /// Back to hidden at once, as when compact mode is switched on.
    pub fn hide_now(&self) {
        self.shown.set(false);
        self.position.set(0.0);
        self.last_tick.set(None);
    }

    /// How far down the floating tab row reaches, in px.
    pub fn inset(&self) -> f32 {
        self.inset.get()
    }

    /// Set by the tab row's owner each frame; 0 when the row is not floating.
    pub fn set_inset(&self, inset: f32) {
        self.inset.set(inset);
    }

    /// Advances the slide to `now`. Returns whether another frame is needed,
    /// either to keep sliding or to notice a delay running out.
    pub fn tick(&self, settings: &Compact, now: Instant) -> bool {
        let mut waiting = false;
        if self.forced.get() {
            // Forced shows at once: the bar is wanted now, not after a pause.
            self.shown.set(true);
        } else if !self.held.get() {
            let hovered = self.hovered.get() != 0;
            if hovered != self.shown.get() {
                let delay = if hovered {
                    settings.reveal_delay_ms
                } else {
                    settings.hide_delay_ms
                };
                let since = self
                    .hover_changed
                    .get()
                    .map_or(Duration::MAX, |at| now.saturating_duration_since(at));
                if since >= Duration::from_millis(u64::from(delay)) {
                    self.shown.set(hovered);
                } else {
                    waiting = true;
                }
            }
        }

        let goal = if self.shown.get() { 1.0 } else { 0.0 };
        let step = self
            .last_tick
            .replace(Some(now))
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last))
            .min(MAX_FRAME_STEP);
        let position = self.position.get();
        let position = if settings.animation_ms == 0 {
            goal
        } else {
            let advance = step.as_secs_f32() * 1000.0 / settings.animation_ms as f32;
            if goal > position {
                (position + advance).min(goal)
            } else {
                (position - advance).max(goal)
            }
        };
        self.position.set(position);
        let moving = position != goal;
        if !moving && !waiting {
            // Idle: the next movement starts its step from its own frame.
            self.last_tick.set(None);
        }
        moving || waiting
    }

    /// How far the bars are shown, eased: smoothstep is symmetric, so turning
    /// around halfway does not jump.
    pub fn amount(&self) -> f32 {
        let t = self.position.get().clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }
}

/// The side of the window a bar comes out of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    /// Tabs and address bar.
    Top,
    /// The bar under the listing.
    Bottom,
    /// The sidebar.
    Left,
    /// The preview.
    Right,
}

/// `content` clipped to `amount` of itself along `edge`, anchored to the far
/// side so it slides out of the edge. Across the edge, top and bottom bars
/// keep their own height (`extent` is that height, measured into `measure`
/// as it lays out); left and right ones get `extent` as their width. `lead`
/// is a band always there between the edge and the bar (the frame, when the
/// bar comes out of it), 0 otherwise. The box is in the flow; the caller
/// makes it float when it should.
pub fn slider(
    edge: Edge,
    lead: f32,
    extent: f32,
    amount: f32,
    measure: Option<Rc<Cell<f32>>>,
    content: impl IntoElement,
) -> Div {
    let shown = lead + extent * amount;
    // A flex column: in a plain block a `size_full` bar (the preview) came
    // out as wide as its contents.
    let inner = div()
        .absolute()
        .flex()
        .flex_col()
        .opacity(0.4 + 0.6 * amount)
        .child(content)
        .when_some(measure, |this, measure| {
            this.child(
                canvas(
                    move |bounds, window, _| {
                        let height = f32::from(bounds.size.height);
                        if (measure.replace(height) - height).abs() > 0.5 {
                            window.refresh();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
        });
    let inner = match edge {
        Edge::Top => inner.left_0().right_0().bottom_0(),
        Edge::Bottom => inner.left_0().right_0().top_0(),
        Edge::Left => inner.top_0().bottom_0().right_0().w(px(extent)),
        Edge::Right => inner.top_0().bottom_0().left_0().w(px(extent)),
    };
    // The bar is clipped on its own past the lead band, so the band shows
    // nothing of it: with the bar hidden, the band is just frame.
    let bar = extent * amount;
    let window = div().absolute().overflow_hidden().child(inner);
    let window = match edge {
        Edge::Top => window.left_0().right_0().top(px(lead)).h(px(bar)),
        Edge::Bottom => window.left_0().right_0().bottom(px(lead)).h(px(bar)),
        Edge::Left => window.top_0().bottom_0().left(px(lead)).w(px(bar)),
        Edge::Right => window.top_0().bottom_0().right(px(lead)).w(px(bar)),
    };
    let clip = div().relative().overflow_hidden().flex_shrink_0();
    match edge {
        Edge::Top | Edge::Bottom => clip.w_full().h(px(shown)),
        Edge::Left | Edge::Right => clip.h_full().w(px(shown)),
    }
    .child(window)
}

/// The strip along `edge` that calls the bars out: `thickness` deep, over
/// what is beneath without blocking it (no `occlude`), so clicks, scrolling
/// and the window's own drag area and buttons still get through. Placed by
/// the caller (`absolute` plus the offsets along the edge).
///
/// A move with a button held does not count: the listing's scrollbar runs
/// along the right edge, and dragging it must not pull the preview over it.
pub fn edge_strip(
    id: impl Into<ElementId>,
    edge: Edge,
    thickness: f32,
    reveal: Rc<Reveal>,
    source: u8,
    view: EntityId,
) -> Stateful<Div> {
    let strip = div().id(id).absolute();
    let strip = match edge {
        Edge::Top => strip.top_0().left_0().right_0().h(px(thickness)),
        Edge::Bottom => strip.bottom_0().left_0().right_0().h(px(thickness)),
        Edge::Left => strip.left_0().top_0().bottom_0().w(px(thickness)),
        Edge::Right => strip.right_0().top_0().bottom_0().w(px(thickness)),
    };
    let on_move = reveal.clone();
    strip
        .on_mouse_move(move |event: &MouseMoveEvent, _window, cx| {
            if on_move.set_hovered(source, event.pressed_button.is_none(), Instant::now()) {
                cx.notify(view);
            }
        })
        .on_hover(move |hovered, _window, cx| {
            if !*hovered && reveal.set_hovered(source, false, Instant::now()) {
                cx.notify(view);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::{Reveal, SOURCE_HEADER, SOURCE_TABS};
    use nohrs_core::config::Compact;
    use std::time::{Duration, Instant};

    fn settings() -> Compact {
        Compact {
            reveal_delay_ms: 100,
            hide_delay_ms: 400,
            animation_ms: 200,
            ..Compact::default()
        }
    }

    // Ticks every 10 ms from `start` for `ms`, returning the time reached.
    fn run(reveal: &Reveal, settings: &Compact, start: Instant, ms: u64) -> Instant {
        let mut now = start;
        for _ in 0..ms / 10 {
            now += Duration::from_millis(10);
            reveal.tick(settings, now);
        }
        now
    }

    #[test]
    fn waits_for_the_reveal_delay_then_slides_in() {
        let settings = settings();
        let reveal = Reveal::default();
        let start = Instant::now();
        reveal.set_hovered(SOURCE_TABS, true, start);
        let now = run(&reveal, &settings, start, 90);
        assert_eq!(reveal.amount(), 0.0);
        let now = run(&reveal, &settings, now, 110);
        assert!(reveal.amount() > 0.0 && reveal.amount() < 1.0);
        run(&reveal, &settings, now, 200);
        assert_eq!(reveal.amount(), 1.0);
    }

    #[test]
    fn a_short_trip_away_does_not_hide_the_bar() {
        let settings = settings();
        let reveal = Reveal::default();
        let start = Instant::now();
        reveal.set_hovered(SOURCE_TABS, true, start);
        let now = run(&reveal, &settings, start, 400);
        reveal.set_hovered(SOURCE_TABS, false, now);
        let now = run(&reveal, &settings, now, 300);
        reveal.set_hovered(SOURCE_TABS, true, now);
        run(&reveal, &settings, now, 500);
        assert_eq!(reveal.amount(), 1.0);
    }

    #[test]
    fn moving_from_the_tabs_onto_the_address_bar_keeps_both_out() {
        let settings = Compact {
            hide_delay_ms: 0,
            ..settings()
        };
        let reveal = Reveal::default();
        let start = Instant::now();
        reveal.set_hovered(SOURCE_TABS, true, start);
        let now = run(&reveal, &settings, start, 400);
        // One move: the address bar's strip hears it before the tabs' one.
        reveal.set_hovered(SOURCE_HEADER, true, now);
        reveal.set_hovered(SOURCE_TABS, false, now);
        run(&reveal, &settings, now, 400);
        assert_eq!(reveal.amount(), 1.0);
    }

    #[test]
    fn turning_around_mid_slide_does_not_jump() {
        let settings = Compact {
            hide_delay_ms: 0,
            ..settings()
        };
        let reveal = Reveal::default();
        let start = Instant::now();
        reveal.set_hovered(SOURCE_TABS, true, start);
        let mut now = run(&reveal, &settings, start, 200);
        assert!(reveal.amount() > 0.2 && reveal.amount() < 0.8);
        reveal.set_hovered(SOURCE_TABS, false, now);
        let mut last = reveal.amount();
        for _ in 0..30 {
            now += Duration::from_millis(10);
            reveal.tick(&settings, now);
            assert!(reveal.amount() <= last && last - reveal.amount() < 0.1);
            last = reveal.amount();
        }
        assert_eq!(reveal.amount(), 0.0);
    }

    #[test]
    fn forced_comes_out_without_the_delay_and_hides_after_it() {
        let settings = settings();
        let reveal = Reveal::default();
        let start = Instant::now();
        assert!(reveal.set_forced(true));
        let now = run(&reveal, &settings, start, 300);
        assert_eq!(reveal.amount(), 1.0);
        reveal.set_forced(false);
        run(&reveal, &settings, now, 600);
        assert_eq!(reveal.amount(), 0.0);
    }

    #[test]
    fn held_stays_put_while_the_pointer_is_away() {
        let settings = Compact {
            hide_delay_ms: 0,
            ..settings()
        };
        let reveal = Reveal::default();
        let start = Instant::now();
        reveal.set_hovered(SOURCE_TABS, true, start);
        let now = run(&reveal, &settings, start, 400);
        reveal.set_held(true);
        reveal.set_hovered(SOURCE_TABS, false, now);
        let now = run(&reveal, &settings, now, 400);
        assert_eq!(reveal.amount(), 1.0);
        reveal.set_held(false);
        run(&reveal, &settings, now, 400);
        assert_eq!(reveal.amount(), 0.0);
    }

    #[test]
    fn no_animation_shows_at_once_and_idles() {
        let settings = Compact {
            reveal_delay_ms: 0,
            animation_ms: 0,
            ..settings()
        };
        let reveal = Reveal::default();
        let now = Instant::now();
        reveal.set_hovered(SOURCE_TABS, true, now);
        assert!(!reveal.tick(&settings, now));
        assert_eq!(reveal.amount(), 1.0);
    }
}
