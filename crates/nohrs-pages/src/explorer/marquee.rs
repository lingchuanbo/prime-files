//! Selecting by dragging a rectangle over the listing, as Explorer does: press
//! on empty space (in Details, anywhere right of the Name column) and drag;
//! every entry the rectangle touches is selected. Ctrl inverts what it touches,
//! Shift adds to the selection, and holding the pointer near (or past) the
//! listing's edge scrolls it.
//!
//! The hit test reads where each entry was actually painted rather than
//! recomputing the list, grid and List-view layouts here: those already
//! differ in three ways and a fourth copy would drift. Positions are kept in
//! content space (window position minus the scroll offset), so entries that
//! scrolled out of view during the drag still count.

use gpui::{
    Bounds, Context, IntoElement, Modifiers, ParentElement, Pixels, Point, Styled, Window, canvas,
    div, point, px, rgba,
};
use nohrs_ui::theme::theme;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use super::ExplorerPane;
use super::types::ViewMode;

/// How far the pointer moves before a press becomes a rectangle.
const START_THRESHOLD: f32 = 4.0;
/// Band along the listing's edge where the pointer starts scrolling it.
const EDGE: f32 = 28.0;
/// Scroll speed, in pixels per second per pixel into or past that band.
const SPEED: f32 = 18.0;
/// Width of the scrollbar strip, where a press is the scrollbar's.
const SCROLLBAR: f32 = 16.0;

/// Where each entry was painted, shared with the elements that paint them.
#[derive(Clone, Default)]
pub(crate) struct HitBoxes(Rc<RefCell<HitInner>>);

#[derive(Default)]
struct HitInner {
    /// What the boxes were painted for; any change starts them over.
    key: Option<HitKey>,
    /// Entry index -> bounds in content space.
    boxes: HashMap<usize, Bounds<Pixels>>,
    /// The listing panel in window space, at the last paint.
    listing: Option<Bounds<Pixels>>,
}

type HitKey = (u8, u64, usize, u32, u32, u32);

impl HitBoxes {
    /// Forgets every box once the layout they belong to is gone.
    fn sync(&self, key: HitKey) {
        let mut inner = self.0.borrow_mut();
        if inner.key != Some(key) {
            inner.key = Some(key);
            inner.boxes.clear();
        }
    }

    fn listing(&self) -> Option<Bounds<Pixels>> {
        self.0.borrow().listing
    }

    pub(crate) fn set_listing(&self, bounds: Bounds<Pixels>) {
        self.0.borrow_mut().listing = Some(bounds);
    }

    fn get(&self, ix: usize) -> Option<Bounds<Pixels>> {
        self.0.borrow().boxes.get(&ix).copied()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A plain drag: the rectangle is the selection.
    Replace,
    /// Shift: the rectangle adds to what was selected.
    Add,
    /// Ctrl: the rectangle inverts what it touches.
    Toggle,
}

/// A rectangle being dragged.
pub(crate) struct Marquee {
    /// Where the press was, in content space.
    origin: Point<Pixels>,
    /// Where the press was, in window space (for the start threshold).
    press: Point<Pixels>,
    /// The pointer, in window space.
    pointer: Point<Pixels>,
    /// The selection before the drag, kept for Shift and Ctrl.
    base: HashSet<String>,
    mode: Mode,
    /// Past the threshold: a rectangle, not a click.
    moved: bool,
    last_tick: Option<Instant>,
}

impl ExplorerPane {
    /// Called at the start of every listing render: drops the boxes of a
    /// layout that changed (folder, view, zoom, width).
    pub(crate) fn sync_hit_boxes(&self) {
        let mode = match self.view_mode {
            ViewMode::List => 0,
            ViewMode::Tree => 1,
            ViewMode::Grid => 2,
            ViewMode::Compact => 3,
        };
        self.hit_boxes.sync((
            mode,
            self.rows_generation,
            self.filtered_entries.len(),
            self.zoom.to_bits(),
            f32::from(self.listing_extent.width).to_bits(),
            self.grid_columns as u32,
        ));
    }

    /// Wraps an entry's element so its painted bounds are known to the
    /// rectangle. The element must be `relative()`.
    pub(crate) fn track_hit<E: ParentElement>(&self, element: E, ix: usize) -> E {
        let boxes = self.hit_boxes.clone();
        let scroll = self.listing_scroll().clone();
        element.child(
            canvas(
                move |bounds, _, _| {
                    let offset = scroll.offset();
                    let content = Bounds::new(bounds.origin - offset, bounds.size);
                    boxes.0.borrow_mut().boxes.insert(ix, content);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }

    /// Whether a press at `position` on row `ix` of the Details view is on
    /// the row's blank part (right of the Name column), where Explorer starts
    /// a rectangle instead of picking up the file.
    pub(crate) fn press_on_row_blank(&self, ix: usize, position: Point<Pixels>) -> bool {
        let Some(content) = self.hit_boxes.get(ix) else {
            return false;
        };
        let left = content.origin.x + self.listing_scroll().offset().x;
        position.x > left + px(super::view::listing::list::ROW_MARGIN + self.col_name_width)
    }

    /// Starts a rectangle at `position` (window space). `on_row` is the row
    /// whose blank part was pressed, if any. Returns whether one started.
    pub(crate) fn start_marquee(
        &mut self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        on_row: Option<usize>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.renaming.is_some() {
            return false;
        }
        // The scrollbar sits over the listing's far edge and takes its own
        // presses; a rectangle started there would fight it.
        if let Some(listing) = self.hit_boxes.listing() {
            let along_bar = if self.view_mode == ViewMode::Compact {
                position.y > listing.bottom() - px(SCROLLBAR)
            } else {
                position.x > listing.right() - px(SCROLLBAR)
            };
            if !listing.contains(&position) || along_bar {
                return false;
            }
        }
        let mode = if modifiers.shift {
            Mode::Add
        } else if modifiers.control || modifiers.platform {
            Mode::Toggle
        } else {
            Mode::Replace
        };
        let base = if mode == Mode::Replace {
            HashSet::new()
        } else {
            self.selection.clone()
        };
        // A click on empty space clears the selection at once, as in
        // Explorer; on a row's blank part it stays that row's click until the
        // pointer moves.
        if mode == Mode::Replace && on_row.is_none() && !self.selection.is_empty() {
            self.clear_selection();
            cx.notify();
        }
        self.drag_candidate = None;
        self.marquee = Some(Marquee {
            origin: position - self.listing_scroll().offset(),
            press: position,
            pointer: position,
            base,
            mode,
            moved: false,
            last_tick: None,
        });
        true
    }

    /// Whether a rectangle is being dragged (so a row's click is not a click).
    pub(crate) fn marquee_dragging(&self) -> bool {
        self.marquee.as_ref().is_some_and(|marquee| marquee.moved)
    }

    /// Follows the pointer. `pressed` is whether the left button is still
    /// down: a release that never reached us ends the rectangle here.
    pub(crate) fn marquee_moved(
        &mut self,
        position: Point<Pixels>,
        pressed: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(marquee) = self.marquee.as_mut() else {
            return;
        };
        if !pressed {
            self.end_marquee(cx);
            return;
        }
        marquee.pointer = position;
        if !marquee.moved {
            let moved = position - marquee.press;
            if f32::from(moved.x).abs() < START_THRESHOLD
                && f32::from(moved.y).abs() < START_THRESHOLD
            {
                return;
            }
            marquee.moved = true;
        }
        self.apply_marquee();
        cx.notify();
    }

    /// Ends the rectangle; the selection it made stays.
    pub(crate) fn end_marquee(&mut self, cx: &mut Context<Self>) {
        if let Some(marquee) = self.marquee.take()
            && marquee.moved
        {
            cx.notify();
        }
    }

    // The rectangle in content space.
    fn marquee_rect(&self, marquee: &Marquee) -> Bounds<Pixels> {
        let current = marquee.pointer - self.listing_scroll().offset();
        Bounds::from_corners(
            point(
                marquee.origin.x.min(current.x),
                marquee.origin.y.min(current.y),
            ),
            point(
                marquee.origin.x.max(current.x),
                marquee.origin.y.max(current.y),
            ),
        )
    }

    // Sets the selection from what the rectangle touches.
    fn apply_marquee(&mut self) {
        let Some(marquee) = self.marquee.as_ref() else {
            return;
        };
        let rect = self.marquee_rect(marquee);
        let mut hits: Vec<usize> = self
            .hit_boxes
            .0
            .borrow()
            .boxes
            .iter()
            .filter(|(_, bounds)| bounds.intersects(&rect))
            .map(|(&ix, _)| ix)
            .collect();
        hits.sort_unstable();
        let hit_paths: Vec<String> = hits
            .iter()
            .filter_map(|&ix| {
                self.filtered_entries
                    .get(ix)
                    .map(|entry| entry.path.clone())
            })
            .collect();
        let mut selection = marquee.base.clone();
        for path in &hit_paths {
            match marquee.mode {
                Mode::Replace | Mode::Add => {
                    selection.insert(path.clone());
                }
                Mode::Toggle => {
                    if !selection.remove(path) {
                        selection.insert(path.clone());
                    }
                }
            }
        }
        self.selection = selection;
        if let (Some(first), Some(last)) = (hit_paths.first(), hit_paths.last()) {
            self.selection_anchor = Some(first.clone());
            self.active_path = Some(last.clone());
        }
    }

    /// Scrolls the listing while the pointer is held at or past its edge;
    /// called on every render, since the pointer may sit still meanwhile.
    pub(crate) fn tick_marquee(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(listing) = self.hit_boxes.listing() else {
            return;
        };
        let horizontal = self.view_mode == ViewMode::Compact;
        let Some(marquee) = self.marquee.as_mut() else {
            return;
        };
        if !marquee.moved {
            return;
        }
        let pointer = window.mouse_position();
        marquee.pointer = pointer;
        let (pos, low, high) = if horizontal {
            (pointer.x, listing.left(), listing.right())
        } else {
            (pointer.y, listing.top(), listing.bottom())
        };
        let push = if pos < low + px(EDGE) {
            f32::from(pos - (low + px(EDGE)))
        } else if pos > high - px(EDGE) {
            f32::from(pos - (high - px(EDGE)))
        } else {
            marquee.last_tick = None;
            return;
        };
        let now = Instant::now();
        let elapsed = marquee
            .last_tick
            .map_or(1.0 / 60.0, |last| now.duration_since(last).as_secs_f32())
            .min(0.1);
        marquee.last_tick = Some(now);
        let step = push * SPEED * elapsed;
        let handle = self.listing_scroll().clone();
        let max = handle.max_offset();
        let mut offset = handle.offset();
        if horizontal {
            offset.x = (offset.x - px(step)).clamp(-max.width, px(0.0));
        } else {
            offset.y = (offset.y - px(step)).clamp(-max.height, px(0.0));
        }
        if offset != handle.offset() {
            self.smooth_scroll = None;
            handle.set_offset(offset);
            self.apply_marquee();
            cx.notify();
        }
        window.request_animation_frame();
    }

    /// The rectangle as drawn over the listing, if one is being dragged.
    pub(crate) fn marquee_overlay(&self) -> Option<gpui::AnyElement> {
        let marquee = self.marquee.as_ref().filter(|marquee| marquee.moved)?;
        let listing = self.hit_boxes.listing()?;
        let offset = self.listing_scroll().offset();
        let rect = self.marquee_rect(marquee);
        let a = rect.origin + offset;
        let b = rect.bottom_right() + offset;
        let left = a.x.max(listing.left());
        let top = a.y.max(listing.top());
        let right = b.x.min(listing.right());
        let bottom = b.y.min(listing.bottom());
        if right <= left || bottom <= top {
            return None;
        }
        Some(
            div()
                .absolute()
                .left(left - listing.left())
                .top(top - listing.top())
                .w(right - left)
                .h(bottom - top)
                .bg(rgba((theme::accent() << 8) | 0x33))
                .border_1()
                .border_color(rgba((theme::accent() << 8) | 0xcc))
                .into_any_element(),
        )
    }
}
