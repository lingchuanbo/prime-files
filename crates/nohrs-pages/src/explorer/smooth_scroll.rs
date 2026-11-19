//! Smooth wheel scrolling in the listing.
//!
//! A mouse wheel moves the content in steps of three lines, which on a list
//! reads as jumps. Each notch here moves a target instead, and the listing
//! glides to it with an exponential ease, so a burst of notches is one
//! movement. How long the glide takes is `ui.scroll_smoothness`; at 0 the
//! wheel is left to gpui and scrolls in steps as before. A touchpad already
//! reports small pixel steps and is never smoothed.

use gpui::{Context, Pixels, Point, ScrollHandle, Window, point, px};
use std::time::Instant;

use super::ExplorerPane;
use super::types::ViewMode;

/// The glide under way.
pub(crate) struct SmoothScroll {
    target: Point<Pixels>,
    /// The offset the last frame set: anything else moving the list (the
    /// scrollbar, a jump to an item) ends the glide rather than fight it.
    last_set: Point<Pixels>,
    last_tick: Option<Instant>,
    mode: ViewMode,
}

/// Time constant of the glide, in seconds, for a smoothness of 0..=100:
/// up to 0.16 s, past which the list feels like it lags the wheel.
fn time_constant(smoothness: u32) -> f32 {
    smoothness.min(100) as f32 * 0.0016
}

/// `offset` moved by `delta` and kept within the content.
fn clamp_offset(offset: Point<Pixels>, max: gpui::Size<Pixels>) -> Point<Pixels> {
    point(
        offset.x.clamp(-max.width, px(0.0)),
        offset.y.clamp(-max.height, px(0.0)),
    )
}

impl ExplorerPane {
    // The scroll handle of the listing on screen.
    pub(crate) fn listing_scroll(&self) -> &ScrollHandle {
        match self.view_mode {
            ViewMode::List | ViewMode::Tree => self.virtual_scroll_handle.base_handle(),
            ViewMode::Grid => self.grid_scroll_handle.base_handle(),
            ViewMode::Compact => self.compact_scroll_handle.base_handle(),
        }
    }

    /// Takes a wheel movement of `delta` pixels over the listing into the
    /// glide. Returns false when smoothing is off, so gpui scrolls as usual.
    pub(crate) fn smooth_wheel(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        if self.scroll_smoothness == 0 {
            return false;
        }
        // The Lista view runs sideways: the wheel turns it as it would turn
        // a vertical list.
        let delta = if self.view_mode == ViewMode::Compact {
            point(if delta.x != px(0.0) { delta.x } else { delta.y }, px(0.0))
        } else {
            point(px(0.0), delta.y)
        };
        let handle = self.listing_scroll().clone();
        let current = handle.offset();
        let from = match &self.smooth_scroll {
            Some(glide) if glide.mode == self.view_mode && glide.last_set == current => {
                glide.target
            }
            _ => current,
        };
        let target = clamp_offset(from + delta, handle.max_offset());
        self.smooth_scroll = Some(SmoothScroll {
            target,
            last_set: current,
            last_tick: self
                .smooth_scroll
                .as_ref()
                .and_then(|glide| glide.last_tick),
            mode: self.view_mode,
        });
        cx.notify();
        true
    }

    /// Moves the listing one frame along the glide; called on every render.
    pub(crate) fn tick_scroll(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        let Some(glide) = self.smooth_scroll.as_mut() else {
            return;
        };
        let handle = match glide.mode {
            ViewMode::List | ViewMode::Tree => self.virtual_scroll_handle.base_handle().clone(),
            ViewMode::Grid => self.grid_scroll_handle.base_handle().clone(),
            ViewMode::Compact => self.compact_scroll_handle.base_handle().clone(),
        };
        let current = handle.offset();
        if glide.mode != self.view_mode || current != glide.last_set {
            self.smooth_scroll = None;
            return;
        }
        let remaining = glide.target - current;
        if f32::from(remaining.x).abs() < 0.5 && f32::from(remaining.y).abs() < 0.5 {
            handle.set_offset(glide.target);
            self.smooth_scroll = None;
            window.refresh();
            return;
        }
        let now = Instant::now();
        let elapsed = glide
            .last_tick
            .map_or(1.0 / 60.0, |last| now.duration_since(last).as_secs_f32())
            .min(0.1);
        glide.last_tick = Some(now);
        let advanced = 1.0 - (-elapsed / time_constant(self.scroll_smoothness).max(0.001)).exp();
        let next = current + point(remaining.x * advanced, remaining.y * advanced);
        handle.set_offset(next);
        glide.last_set = next;
        window.request_animation_frame();
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_offset, time_constant};
    use gpui::{point, px, size};

    #[test]
    fn the_target_stays_inside_the_content() {
        let max = size(px(0.0), px(500.0));
        assert_eq!(
            clamp_offset(point(px(0.0), px(60.0)), max),
            point(px(0.0), px(0.0))
        );
        assert_eq!(
            clamp_offset(point(px(0.0), px(-900.0)), max),
            point(px(0.0), px(-500.0))
        );
    }

    #[test]
    fn smoothness_sets_how_long_the_glide_takes() {
        assert_eq!(time_constant(0), 0.0);
        assert!((time_constant(50) - 0.08).abs() < 1e-6);
        assert!((time_constant(500) - 0.16).abs() < 1e-6);
    }
}
