/// Modern theme tokens inspired by macOS and modern file explorers
// Consumers import this as `theme::theme` (e.g. `use nohrs_ui::theme::theme`);
// the inner module name is part of the established public path.
#[allow(clippy::module_inception)]
pub mod theme {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    // The tokens are read during render, on every frame, by code that has no
    // `cx` to hand in some places (free helper functions building elements). A
    // process-wide flag keeps them plain calls; the appearance is chosen once at
    // startup, before the first window opens, so no frame ever reads it torn.
    static DARK: AtomicBool = AtomicBool::new(false);

    /// Selects the palette every token below reads from.
    pub fn set_dark(dark: bool) {
        DARK.store(dark, Ordering::Relaxed);
    }

    /// Whether the dark palette is the one in use.
    pub fn is_dark() -> bool {
        DARK.load(Ordering::Relaxed)
    }

    // The accent the user picked, as `0x01rrggbb`; zero means the palette's
    // own orange. The marker byte keeps black (`#000000`) distinguishable from
    // "nothing picked".
    static ACCENT: AtomicU32 = AtomicU32::new(0);
    const ACCENT_SET: u32 = 0x0100_0000;

    /// The accents the settings page offers, by their `config.toml` name.
    /// Orange is the palette's own and carries no colour here: the two
    /// palettes tune it separately, which a single base colour cannot.
    pub const NAMED_ACCENTS: &[(&str, Option<u32>)] = &[
        ("orange", None),
        ("blue", Some(0x3B82F6)),
        ("teal", Some(0x14B8A6)),
        ("green", Some(0x22C55E)),
        ("purple", Some(0x8B5CF6)),
        ("pink", Some(0xEC4899)),
        ("red", Some(0xEF4444)),
    ];

    /// The colour a `theme.accent` value names: one of [`NAMED_ACCENTS`] or a
    /// `#rrggbb` hex. `None` for orange and for anything unrecognized, so a
    /// typo leaves the window looking as it always did.
    pub fn parse_accent(value: &str) -> Option<u32> {
        let value = value.trim();
        if let Some(hex) = value.strip_prefix('#') {
            return (hex.len() == 6).then(|| u32::from_str_radix(hex, 16).ok()).flatten();
        }
        NAMED_ACCENTS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(value))
            .and_then(|(_, color)| *color)
    }

    /// Replaces the palette's accent with `color`, or restores it with `None`.
    pub fn set_accent(color: Option<u32>) {
        ACCENT.store(color.map_or(0, |color| ACCENT_SET | (color & 0xFF_FFFF)), Ordering::Relaxed);
    }

    fn picked_accent() -> Option<u32> {
        let value = ACCENT.load(Ordering::Relaxed);
        (value & ACCENT_SET != 0).then_some(value & 0xFF_FFFF)
    }

    // The background the user picked, `0x01rrggbb`; zero means the palette's.
    static BACKGROUND: AtomicU32 = AtomicU32::new(0);

    /// Replaces the palettes' content background with `color` (`0xrrggbb`), or
    /// restores it with `None`. The surfaces around it — hover, borders, the
    /// chrome — and the text are shaded from it by [`custom`], so any colour
    /// stays usable rather than leaving white text on a white window.
    pub fn set_background(color: Option<u32>) {
        BACKGROUND.store(color.map_or(0, |color| ACCENT_SET | (color & 0xFF_FFFF)), Ordering::Relaxed);
    }

    /// The colour a `theme.background` value names: `#rrggbb` only. `None` for
    /// an empty or unrecognized value, so a typo leaves the palette alone.
    pub fn parse_color(value: &str) -> Option<u32> {
        let hex = value.trim().strip_prefix('#')?;
        if hex.len() != 6 || !hex.chars().all(|digit| digit.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(hex, 16).ok()
    }

    // A picked background and everything shaded from it: the surfaces around
    // it step toward the text colour in small amounts, and the chrome steps
    // away from the background so the frame still reads as a frame.
    struct Custom {
        bg: u32,
        fg: u32,
        bg_secondary: u32,
        bg_hover: u32,
        fg_secondary: u32,
        muted: u32,
        border: u32,
        border_hover: u32,
        toolbar_bg: u32,
        toolbar_hover: u32,
        toolbar_text: u32,
        toolbar_active_bg: u32,
        toolbar_border: u32,
    }

    impl Custom {
        fn new(bg: u32) -> Self {
            // Which side of the background reads: the same 0.4 the accent uses
            // to decide its own text colour.
            let dark = luminance(bg) < 0.4;
            let fg = if dark { 0xDCDDE0 } else { 0x111827 };
            // Further from the background than nothing, but not as far as the
            // text: what a hover fill, a border and the chrome are.
            let away = |amount: f32| mix(bg, fg, amount);
            // The chrome goes the other way: darker under a dark background,
            // lighter under a light one, as the palettes do.
            let chrome = |amount: f32| mix(bg, if dark { 0x000000 } else { 0xFFFFFF }, amount);
            Self {
                bg,
                fg,
                bg_secondary: away(0.04),
                bg_hover: away(0.08),
                fg_secondary: away(0.45),
                muted: away(0.62),
                border: away(0.14),
                border_hover: away(0.26),
                toolbar_bg: chrome(0.05),
                toolbar_hover: away(0.08),
                toolbar_text: away(0.42),
                toolbar_active_bg: away(0.06),
                toolbar_border: away(0.14),
            }
        }
    }

    fn custom() -> Option<Custom> {
        let value = BACKGROUND.load(Ordering::Relaxed);
        (value & ACCENT_SET != 0).then(|| Custom::new(value & 0xFF_FFFF))
    }

    // Linear blend of two `0xrrggbb` colours, `amount` of the way to `to`.
    fn mix(from: u32, to: u32, amount: f32) -> u32 {
        let channel = |shift: u32| {
            let a = ((from >> shift) & 0xFF) as f32;
            let b = ((to >> shift) & 0xFF) as f32;
            ((a + (b - a) * amount).round() as u32).min(0xFF) << shift
        };
        channel(16) | channel(8) | channel(0)
    }

    // Relative luminance, enough to choose black or white text on a fill.
    fn luminance(color: u32) -> f32 {
        let linear = |shift: u32| {
            let c = ((color >> shift) & 0xFF) as f32 / 255.0;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0)
    }

    // A picked accent, shaded for the palette in use the way the orange is:
    // darker on white so text on it still reads, lighter on the dark grays.
    fn shaded_accent(base: u32) -> u32 {
        if is_dark() { mix(base, 0xFFFFFF, 0.12) } else { mix(base, 0x000000, 0.18) }
    }

    struct Palette {
        gray_50: u32,
        gray_100: u32,
        gray_200: u32,
        gray_300: u32,
        gray_400: u32,
        gray_500: u32,
        gray_600: u32,
        gray_700: u32,
        gray_800: u32,
        gray_900: u32,
        bg: u32,
        bg_secondary: u32,
        bg_hover: u32,
        fg: u32,
        fg_secondary: u32,
        muted: u32,
        border: u32,
        border_hover: u32,
        toolbar_bg: u32,
        toolbar_hover: u32,
        toolbar_text: u32,
        toolbar_active_bg: u32,
        toolbar_border: u32,
        accent: u32,
        accent_hover: u32,
        accent_subtle: u32,
        accent_fg: u32,
        danger: u32,
        warning: u32,
        preview_backdrop: u32,
    }

    const LIGHT: Palette = Palette {
        gray_50: 0xF5F6F8,
        gray_100: 0xF0F2F5,
        gray_200: 0xE5E7EC,
        gray_300: 0xD1D5DB,
        gray_400: 0x9CA3AF,
        gray_500: 0x647084,
        gray_600: 0x4B5563,
        gray_700: 0x374151,
        gray_800: 0x1F2937,
        gray_900: 0x202733,
        bg: 0xFFFFFF,
        bg_secondary: 0xF5F6F8,
        bg_hover: 0xF0F2F5,
        fg: 0x202733,
        fg_secondary: 0x647084,
        muted: 0x9CA3AF,
        border: 0xE5E7EC,
        border_hover: 0xD1D5DB,
        // Chrome surfaces (nav rail, tab bar, status bar) share one tone so the
        // window reads as a frame around the white content areas.
        toolbar_bg: 0xF0F2F5,
        toolbar_hover: 0xE5E7EC,
        toolbar_text: 0x4B5563,
        toolbar_active_bg: 0xFFFFFF,
        toolbar_border: 0xE5E7EC,
        // Warm amber keeps filled controls legible on the light surface.
        accent: 0xA36319,
        accent_hover: 0x895017,
        accent_subtle: 0xF5E9D9,
        accent_fg: 0xFFFFFF,
        danger: 0xDC2626,
        warning: 0xB45309,
        preview_backdrop: 0x1F2937,
    };

    // Graphite rather than black: pure black behind near-white text is the
    // harshest contrast a screen can show and smears on OLED. The text stops
    // short of white for the same reason, and every step keeps the chrome a
    // shade darker than the content so the frame reads the same way it does in
    // the light palette.
    const DARK_PALETTE: Palette = Palette {
        // The gray ramp is inverted, since its callers mean "faint" by the low
        // end and "strong" by the high end, not "light" and "dark".
        gray_50: 0x1C2028,
        gray_100: 0x242A35,
        gray_200: 0x303746,
        gray_300: 0x3B4454,
        gray_400: 0x778196,
        gray_500: 0x9BA5B7,
        gray_600: 0xBCC4D1,
        gray_700: 0xCDD3DE,
        gray_800: 0xE8EBF0,
        gray_900: 0xF2F4F8,
        bg: 0x171A21,
        bg_secondary: 0x1C2028,
        bg_hover: 0x242A35,
        fg: 0xE8EBF0,
        fg_secondary: 0x9BA5B7,
        muted: 0x778196,
        border: 0x2A2E38,
        border_hover: 0x3B4454,
        toolbar_bg: 0x12151B,
        toolbar_hover: 0x242A35,
        toolbar_text: 0xBCC4D1,
        toolbar_active_bg: 0x242A35,
        toolbar_border: 0x242A35,
        // Warm amber on a cool canvas; filled controls use dark text.
        accent: 0xE6B56E,
        accent_hover: 0xF0C98E,
        accent_subtle: 0x352C21,
        accent_fg: 0x171A21,
        danger: 0xF07171,
        warning: 0xE0A33A,
        preview_backdrop: 0x101218,
    };

    fn palette() -> &'static Palette {
        if is_dark() { &DARK_PALETTE } else { &LIGHT }
    }

    macro_rules! tokens {
        ($($(#[$doc:meta])* $name:ident;)*) => {
            $(
                $(#[$doc])*
                pub fn $name() -> u32 {
                    palette().$name
                }
            )*
        };
    }

    /// Primary accent color.
    pub fn accent() -> u32 {
        picked_accent().map_or(palette().accent, shaded_accent)
    }

    /// The palette's own accent (the orange), whatever the user picked; for
    /// the swatch that offers it back.
    pub fn palette_accent() -> u32 {
        palette().accent
    }

    /// Accent color on hover / pressed.
    pub fn accent_hover() -> u32 {
        match picked_accent() {
            Some(base) if is_dark() => mix(shaded_accent(base), 0xFFFFFF, 0.15),
            Some(base) => mix(shaded_accent(base), 0x000000, 0.2),
            None => palette().accent_hover,
        }
    }

    /// Faint accent tint, for selected rows and active chip backgrounds.
    pub fn accent_subtle() -> u32 {
        match picked_accent() {
            Some(base) if is_dark() => mix(bg(), base, 0.2),
            Some(base) => mix(bg(), base, 0.1),
            None => palette().accent_subtle,
        }
    }

    /// Text and icon color on top of an `accent`-filled surface.
    pub fn accent_fg() -> u32 {
        match picked_accent() {
            Some(_) if luminance(accent()) > 0.4 => 0x1B1C1F,
            Some(_) => 0xFFFFFF,
            None => palette().accent_fg,
        }
    }

    tokens! {
        /// Faintest step of the gray ramp.
        gray_50;
        /// Faint gray, used for subtle backgrounds.
        gray_100;
        /// Gray used for default borders.
        gray_200;
        /// Gray used for borders on hover.
        gray_300;
        /// Gray used for muted text.
        gray_400;
        /// Gray used for secondary text.
        gray_500;
        /// Gray used for primary text.
        gray_600;
        /// Gray used for high-emphasis text.
        gray_700;
        /// Second-strongest step of the gray ramp.
        gray_800;
        /// Strongest step of the gray ramp.
        gray_900;
        /// Danger color, used for error messages and destructive actions.
        danger;
        /// Warning color, for an operation that did what was asked and left
        /// something unfinished. Amber rather than a lighter red: the distance
        /// from `danger` is the whole point, since the two say opposite things
        /// about whether the thing the user asked for happened.
        warning;
        /// Backdrop behind an image preview; deliberately dark so transparent
        /// and light-colored images stay legible.
        preview_backdrop;
    }

    // The surface tokens read a picked background first: everything drawn on
    // it (text, hover fills, borders, the chrome around it) is shaded from it,
    // and the palettes answer when no background was picked.
    macro_rules! surface {
        ($($(#[$doc:meta])* $name:ident => $palette:ident;)*) => {
            $(
                $(#[$doc])*
                pub fn $name() -> u32 {
                    custom().map_or_else(|| palette().$palette, |custom| custom.$name)
                }
            )*
        };
    }

    surface! {
        /// Primary surface background color.
        bg => bg;
        /// Secondary surface background color.
        bg_secondary => bg_secondary;
        /// Background color for hovered surfaces.
        bg_hover => bg_hover;
        /// Primary foreground (text) color.
        fg => fg;
        /// Secondary foreground (text) color for less prominent content.
        fg_secondary => fg_secondary;
        /// Muted foreground color for de-emphasized content.
        muted => muted;
        /// Default border color.
        border => border;
        /// Border color on hover.
        border_hover => border_hover;
        /// Toolbar background color.
        toolbar_bg => toolbar_bg;
        /// Toolbar item background color on hover.
        toolbar_hover => toolbar_hover;
        /// Toolbar text color.
        toolbar_text => toolbar_text;
        /// Background color of the active toolbar item.
        toolbar_active_bg => toolbar_active_bg;
        /// Toolbar border color.
        toolbar_border => toolbar_border;
    }

    /// Text color of the active toolbar item.
    pub fn toolbar_active_text() -> u32 {
        accent()
    }

    /// Status bar background; matches the other chrome surfaces.
    pub fn footer_bg() -> u32 {
        toolbar_bg()
    }

    /// Opacity of the scrim painted behind a modal dialog. `gpui_component`
    /// ships 5%, which is invisible against this theme's white surfaces and
    /// leaves confirmations looking like floating panels rather than modals.
    pub const MODAL_SCRIM_ALPHA: f32 = 0.40;

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn only_rrggbb_names_a_background_colour() {
            assert_eq!(parse_color("#1e1f22"), Some(0x1E1F22));
            assert_eq!(parse_color(" #101828 "), Some(0x101828));
            assert_eq!(parse_color("#FFFFFF"), Some(0xFFFFFF));
            // Anything else leaves the palette alone rather than turning the
            // window an unexpected colour.
            assert_eq!(parse_color(""), None);
            assert_eq!(parse_color("black"), None);
            assert_eq!(parse_color("#12345"), None);
            assert_eq!(parse_color("#1234567"), None);
            assert_eq!(parse_color("#gggggg"), None);
            assert_eq!(parse_color("1e1f22"), None);
        }

        #[test]
        fn a_custom_background_puts_the_text_on_the_side_that_reads() {
            // Dark backgrounds carry light text, light ones dark text.
            let dark = Custom::new(0x101820);
            assert!(luminance(dark.fg) > luminance(dark.bg));
            let light = Custom::new(0xF5F1E8);
            assert!(luminance(light.fg) < luminance(light.bg));
            // The surfaces step away from the background, and the chrome
            // further still, so the frame reads as a frame.
            assert_ne!(light.bg_hover, light.bg);
            assert_ne!(light.border, light.bg);
            assert_ne!(light.toolbar_bg, light.bg);
        }
    }
}
