//! The colour wheel behind Settings › Appearance › Background colour: hue
//! around the ring, saturation out from the centre, and a bar for brightness.
//! A picture is cheaper than a thousand wedges — the ring does not depend on
//! the colour picked, so it is drawn once and shown beside the two handles.

use std::sync::{Arc, OnceLock};

use gpui::{Bounds, Pixels, Point, RenderImage, rgba};
use image::{Frame, RgbaImage};

/// Diameter of the wheel on screen, in pixels.
pub(crate) const SIZE: f32 = 176.0;

/// A colour as hue (`0..360`), saturation and value (`0..1` each).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hsv {
    pub h: f32,
    pub s: f32,
    pub v: f32,
}

impl Hsv {
    /// The colour `0xrrggbb` read as hue, saturation and value.
    pub(crate) fn from_rgb(color: u32) -> Self {
        let channel = |shift: u32| ((color >> shift) & 0xFF) as f32 / 255.0;
        let (r, g, b) = (channel(16), channel(8), channel(0));
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let range = max - min;
        let hue = if range == 0.0 {
            0.0
        } else if max == r {
            60.0 * (((g - b) / range) % 6.0)
        } else if max == g {
            60.0 * ((b - r) / range + 2.0)
        } else {
            60.0 * ((r - g) / range + 4.0)
        };
        Self {
            h: hue.rem_euclid(360.0),
            s: if max == 0.0 { 0.0 } else { range / max },
            v: max,
        }
    }

    /// The colour as `0xrrggbb`.
    pub(crate) fn to_rgb(self) -> u32 {
        let hue = self.h.rem_euclid(360.0);
        let chroma = self.v * self.s;
        let second = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
        let base = self.v - chroma;
        let (r, g, b) = match (hue / 60.0) as u32 {
            0 => (chroma, second, 0.0),
            1 => (second, chroma, 0.0),
            2 => (0.0, chroma, second),
            3 => (0.0, second, chroma),
            4 => (second, 0.0, chroma),
            _ => (chroma, 0.0, second),
        };
        let channel = |value: f32| ((value + base).clamp(0.0, 1.0) * 255.0).round() as u32;
        channel(r) << 16 | channel(g) << 8 | channel(b)
    }

    /// The colour as the theme and the picker both understand it.
    pub(crate) fn to_hsla(self) -> gpui::Hsla {
        gpui::Hsla::from(rgba((self.to_rgb() << 8) | 0xFF))
    }

    /// The colour as the `#rrggbb` the config keeps.
    pub(crate) fn hex(self) -> String {
        format!("#{:06x}", self.to_rgb())
    }
}

/// The wheel's picture: hue by angle, saturation by distance from the centre,
/// brightness at full. It never changes with the colour picked, so it is drawn
/// once for the whole run.
pub(crate) fn image() -> Arc<RenderImage> {
    static IMAGE: OnceLock<Arc<RenderImage>> = OnceLock::new();
    IMAGE
        .get_or_init(|| {
            let size = SIZE as u32;
            let mut buffer = RgbaImage::new(size, size);
            let centre = size as f32 / 2.0;
            let radius = centre - 1.0;
            for y in 0..size {
                for x in 0..size {
                    let dx = x as f32 + 0.5 - centre;
                    let dy = y as f32 + 0.5 - centre;
                    let distance = (dx * dx + dy * dy).sqrt() / radius;
                    if distance > 1.0 {
                        // Outside the ring: nothing is drawn there.
                        continue;
                    }
                    let hue = (dy.atan2(dx).to_degrees() + 360.0) % 360.0;
                    let color = Hsv { h: hue, s: distance.min(1.0), v: 1.0 }.to_rgb();
                    // gpui draws BGRA, whatever the buffer type is called.
                    let pixel = image::Rgba([
                        (color & 0xFF) as u8,
                        ((color >> 8) & 0xFF) as u8,
                        ((color >> 16) & 0xFF) as u8,
                        0xFF,
                    ]);
                    buffer.put_pixel(x, y, pixel);
                }
            }
            Arc::new(RenderImage::new(vec![Frame::new(buffer)]))
        })
        .clone()
}

/// The hue and saturation a pointer at `position` means on a ring drawn in
/// `bounds`: the angle around the centre is the hue, the distance from it the
/// saturation. Past the rim the distance is capped, so a drag that leaves the
/// ring keeps the colour it had rather than dropping it.
pub(crate) fn ring_hue_saturation(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<(f32, f32)> {
    let radius = f64::from(bounds.size.width) / 2.0;
    if radius <= 0.0 {
        return None;
    }
    let centre = (
        f64::from(bounds.origin.x) + radius,
        f64::from(bounds.origin.y) + f64::from(bounds.size.height) / 2.0,
    );
    let dx = f64::from(position.x) - centre.0;
    let dy = f64::from(position.y) - centre.1;
    let distance = (dx * dx + dy * dy).sqrt() / radius;
    let hue = (dy.atan2(dx).to_degrees() + 360.0) % 360.0;
    Some((hue as f32, distance.min(1.0) as f32))
}

/// The brightness a pointer at `position` means on a bar drawn in `bounds`:
/// full at its left end, black at its right.
pub(crate) fn bar_brightness(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<f32> {
    let width = f64::from(bounds.size.width);
    if width <= 0.0 {
        return None;
    }
    let travelled = ((f64::from(position.x) - f64::from(bounds.origin.x)) / width).clamp(0.0, 1.0);
    Some((1.0 - travelled) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_round_trip_through_hsv() {
        for color in [0x000000, 0xFFFFFF, 0xFF0000, 0x00FF00, 0x0000FF, 0x1E1F22, 0x8B5CF6] {
            assert_eq!(Hsv::from_rgb(color).to_rgb(), color, "{color:06x} did not round trip");
        }
    }

    #[test]
    fn the_wheel_reads_its_angles_the_way_a_wheel_does() {
        // Right is red, down is green, left is cyan: the ring runs clockwise
        // from the positive x axis, which is how the picture is drawn.
        assert_eq!(Hsv { h: 0.0, s: 1.0, v: 1.0 }.to_rgb(), 0xFF0000);
        assert_eq!(Hsv { h: 120.0, s: 1.0, v: 1.0 }.to_rgb(), 0x00FF00);
        assert_eq!(Hsv { h: 240.0, s: 1.0, v: 1.0 }.to_rgb(), 0x0000FF);
        // The centre of the wheel is white, its edge the pure hue.
        assert_eq!(Hsv { h: 200.0, s: 0.0, v: 1.0 }.to_rgb(), 0xFFFFFF);
        assert_eq!(Hsv { h: 200.0, s: 1.0, v: 0.0 }.to_rgb(), 0x000000);
    }

    #[test]
    fn a_pointer_on_the_ring_means_the_colour_under_it() {
        use gpui::{point, px, size};
        let bounds = Bounds::new(point(px(100.0), px(100.0)), size(px(200.0), px(200.0)));
        // The centre is white: no saturation, whatever the hue.
        let (_, saturation) = ring_hue_saturation(bounds, point(px(200.0), px(200.0))).unwrap();
        assert!(saturation < 0.01);
        // Right is red (hue 0), down is green (120), left is cyan (180).
        let (hue, saturation) = ring_hue_saturation(bounds, point(px(295.0), px(200.0))).unwrap();
        assert!(hue < 1.0 || hue > 359.0, "{hue}");
        assert!((saturation - 0.95).abs() < 0.06, "{saturation}");
        let (hue, _) = ring_hue_saturation(bounds, point(px(200.0), px(295.0))).unwrap();
        assert!((hue - 90.0).abs() < 1.0, "{hue}");
        let (hue, _) = ring_hue_saturation(bounds, point(px(105.0), px(200.0))).unwrap();
        assert!((hue - 180.0).abs() < 1.0, "{hue}");
        // Leaving the ring keeps the hue and caps the saturation.
        let (_, far) = ring_hue_saturation(bounds, point(px(900.0), px(200.0))).unwrap();
        assert_eq!(far, 1.0);

        // The bar is full at its left end and black at its right.
        let bar = Bounds::new(point(px(0.0), px(0.0)), size(px(220.0), px(16.0)));
        assert_eq!(bar_brightness(bar, point(px(0.0), px(8.0))), Some(1.0));
        assert_eq!(bar_brightness(bar, point(px(110.0), px(8.0))), Some(0.5));
        assert_eq!(bar_brightness(bar, point(px(220.0), px(8.0))), Some(0.0));
        assert_eq!(bar_brightness(bar, point(px(-40.0), px(8.0))), Some(1.0));
    }

    #[test]
    fn the_picture_is_a_ring_that_matches_the_colours_it_promises() {        let image = image();
        assert_eq!(image.size(0).width.0 as u32, SIZE as u32);
        // The middle of the picture is white (to within the rounding of a
        // pixel that is not quite on the centre); the outer edge, to the
        // right, is red; the corner is transparent.
        let size = SIZE as usize;
        let at = |x: usize, y: usize| -> [u8; 4] {
            let bytes = image.as_bytes(0).expect("pixels");
            let start = (y * size + x) * 4;
            [bytes[start], bytes[start + 1], bytes[start + 2], bytes[start + 3]]
        };
        let middle = at(size / 2, size / 2);
        assert!(middle[..3].iter().all(|channel| *channel > 250), "{middle:?} is near white");
        let edge = at(size - 3, size / 2);
        // The pixel sits a hair off the axis, so "red" is to within rounding.
        assert_eq!(edge[3], 0xFF);
        assert!(edge[2] > 250, "{edge:?} is red (in BGRA)");
        assert!(edge[0] < 20 && edge[1] < 20, "{edge:?} has no green or blue to speak of");
        assert_eq!(at(1, 1)[3], 0x00, "the square corners are left transparent");
    }
}
