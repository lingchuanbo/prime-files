//! The listing's zoom: one continuous value that runs from the list table up
//! through ever larger icon tiles, the way File Pilot and Explorer's
//! Ctrl+wheel do, instead of two fixed modes.

use super::types::ViewMode;
use nohrs_core::k;

/// Zoom at or below which the listing is the list table rather than tiles.
pub const LIST_MAX: f32 = 0.1;
/// Zoom a fresh switch to the grid lands on: medium icons.
pub const DEFAULT_GRID: f32 = 0.35;
/// How far one notch of Ctrl+wheel, or one Ctrl+= / Ctrl+-, moves the zoom.
pub const STEP: f32 = 0.05;

const ICON_MIN: f32 = 48.0;
const ICON_MAX: f32 = 256.0;

/// Zoom of the Tree view, between the details table and the List view, as
/// File Pilot orders them; the List view sits at `LIST_MAX`.
pub const TREE: f32 = 0.05;

/// The view a zoom value shows: the details table, then Tree, then List up to
/// `LIST_MAX`, and tiles above it.
pub fn view_mode(zoom: f32) -> ViewMode {
    if zoom < TREE / 2.0 {
        ViewMode::List
    } else if zoom < (TREE + LIST_MAX) / 2.0 {
        ViewMode::Tree
    } else if zoom <= LIST_MAX {
        ViewMode::Compact
    } else {
        ViewMode::Grid
    }
}

/// Side of a grid tile's thumbnail, in logical pixels.
pub fn icon_size(zoom: f32) -> f32 {
    let along = ((zoom - LIST_MAX) / (1.0 - LIST_MAX)).clamp(0.0, 1.0);
    (ICON_MIN + (ICON_MAX - ICON_MIN) * along).round()
}

/// Moves `zoom` by `delta`, kept inside 0..=1.
pub fn step(zoom: f32, delta: f32) -> f32 {
    (zoom + delta).clamp(0.0, 1.0)
}

/// The name the footer shows for a zoom value.
pub fn label(zoom: f32) -> &'static str {
    match view_mode(zoom) {
        ViewMode::List => return k!("Detalhes"),
        ViewMode::Tree => return k!("Árvore"),
        ViewMode::Compact => return k!("Lista"),
        ViewMode::Grid => {}
    }
    match icon_size(zoom) {
        size if size < 80.0 => k!("Ícones P"),
        size if size < 136.0 => k!("Ícones M"),
        size if size < 200.0 => k!("Ícones G"),
        _ => k!("Ícones XL"),
    }
}

/// The footer's zoom menu, largest first as File Pilot lists them: each
/// view's name and the zoom it jumps to, inside the band `label` gives it.
pub const PRESETS: [(&str, f32); 7] = [
    (k!("Ícones XL"), 0.85),
    (k!("Ícones G"), 0.6),
    (k!("Ícones M"), DEFAULT_GRID),
    (k!("Ícones P"), 0.15),
    (k!("Lista"), LIST_MAX),
    (k!("Árvore"), TREE),
    (k!("Detalhes"), 0.0),
];

/// The views the zoom menu lists, smallest zoom first: one evenly spaced row
/// each, which is also where its slider puts them. Small icons are left out,
/// as File Pilot leaves them out; the slider still passes through them.
pub const MENU_STOPS: [(&str, f32); 6] = [
    (k!("Detalhes"), 0.0),
    (k!("Árvore"), TREE),
    (k!("Lista"), LIST_MAX),
    (k!("Ícones M"), DEFAULT_GRID),
    (k!("Ícones G"), 0.6),
    (k!("Ícones XL"), 0.85),
];

/// The zoom menu's slider position (0..=100) for a zoom value. The menu's
/// rows are evenly spaced, so each step between two rows is the same length
/// on the slider, and the zoom between them is spread along it.
pub fn slider_position(zoom: f32) -> f32 {
    let steps = (MENU_STOPS.len() - 1) as f32;
    for (index, pair) in MENU_STOPS.windows(2).enumerate() {
        let (low, high) = (pair[0].1, pair[1].1);
        if zoom <= high {
            let along = ((zoom - low) / (high - low)).clamp(0.0, 1.0);
            return (index as f32 + along) / steps * 100.0;
        }
    }
    100.0
}

/// The zoom for a position of the zoom menu's slider.
pub fn zoom_at(position: f32) -> f32 {
    let steps = (MENU_STOPS.len() - 1) as f32;
    let at = position.clamp(0.0, 100.0) / 100.0 * steps;
    let index = (at.floor() as usize).min(MENU_STOPS.len() - 2);
    let (low, high) = (MENU_STOPS[index].1, MENU_STOPS[index + 1].1);
    low + (high - low) * (at - index as f32)
}

/// Edge of the square thumbnail to ask for, in device pixels.
///
/// Rounded up to a few fixed sizes so dragging the zoom does not re-extract
/// every thumbnail at each pixel; the image is scaled down to the tile anyway.
pub fn thumbnail_bucket(icon_size: f32, scale_factor: f32) -> u32 {
    let wanted = (icon_size * scale_factor).ceil() as u32;
    [64, 128, 256, 512]
        .into_iter()
        .find(|&bucket| bucket >= wanted)
        .unwrap_or(512)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_below_the_threshold_tiles_above() {
        assert!(view_mode(0.0) == ViewMode::List);
        assert!(view_mode(TREE) == ViewMode::Tree);
        assert!(view_mode(LIST_MAX) == ViewMode::Compact);
        assert!(view_mode(LIST_MAX + 0.01) == ViewMode::Grid);
    }

    #[test]
    fn icon_size_spans_the_range_and_grows() {
        assert_eq!(icon_size(LIST_MAX), ICON_MIN);
        assert_eq!(icon_size(1.0), ICON_MAX);
        assert!(icon_size(0.5) > icon_size(0.3));
    }

    #[test]
    fn every_preset_is_named_by_the_band_it_lands_in() {
        for (name, zoom) in PRESETS {
            assert_eq!(label(zoom), name);
        }
    }

    #[test]
    fn the_slider_puts_each_menu_view_one_even_step_apart() {
        for (index, (_, zoom)) in MENU_STOPS.into_iter().enumerate() {
            assert!((slider_position(zoom) - index as f32 * 20.0).abs() < 1e-4);
            assert!((zoom_at(slider_position(zoom)) - zoom).abs() < 1e-5);
        }
        // Between rows the zoom is spread, so small icons have a place.
        assert!(slider_position(0.15) > 40.0 && slider_position(0.15) < 60.0);
        // Beyond the largest row the knob stays at the top.
        assert_eq!(slider_position(1.0), 100.0);
    }

    #[test]
    fn step_stays_in_range() {
        assert_eq!(step(0.98, STEP), 1.0);
        assert_eq!(step(0.02, -STEP), 0.0);
    }

    #[test]
    fn buckets_round_up_and_cap() {
        assert_eq!(thumbnail_bucket(48.0, 1.0), 64);
        assert_eq!(thumbnail_bucket(100.0, 1.0), 128);
        assert_eq!(thumbnail_bucket(100.0, 1.5), 256);
        assert_eq!(thumbnail_bucket(256.0, 3.0), 512);
    }
}
