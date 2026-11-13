use super::super::types::ViewMode;
use crate::explorer::{ExplorerPane, zoom};
use gpui::*;

/// The List view: names in columns, scrolling sideways.
pub mod compact;
/// Grid-mode rendering of the listing.
pub mod grid;
/// List-mode rendering of the listing.
pub mod list;
/// Rendering of a single listing row.
pub mod row;
/// The full-text search bar shown above the listing.
pub mod search_bar;
/// This PC overview and recent folders.
mod home;

/// Renders the file listing in the active view mode, with the search bar when
/// search is visible.
pub fn render(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    page.ensure_list_initialized(window, cx);
    // In both modes: the preview pane draws from the same cache, and the list
    // has to release a folder's thumbnails on leaving it just as the grid does.
    page.thumbnails.show_directory(&page.cwd, cx);
    page.thumbnails.begin_frame(cx);
    page.waveforms.show_directory(&page.cwd);
    page.waveforms.begin_frame();
    page.sync_hit_boxes();

    let file_list = match page.view_mode {
        ViewMode::List | ViewMode::Tree => list::render(page, cx),
        ViewMode::Compact => compact::render(page, window, cx),
        ViewMode::Grid => grid::render(page, window, cx),
    };
    let file_list = with_wheel_zoom(page, file_list, listing_scrollbar(page), cx);

    // 首页以卡片呈现磁盘；筛选和搜索时仍使用完整列表。
    let overview = page.cwd.eq_ignore_ascii_case(crate::explorer::places::THIS_PC)
        && !page.is_filtering() && page.search_results.is_none() && !page.search_visible
        && !page.entries.is_empty()
        && page.entries.iter().all(|entry| crate::explorer::drives::drive_root(&entry.path).is_some());
    let file_list = if overview {
        home::render(page, cx).unwrap_or(file_list)
    } else { file_list };

    if page.search_visible {
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(search_bar::render(page, cx))
            .child(file_list)
            .into_any_element()
    } else {
        file_list
    }
}

/// A bar of a drive's used space: the accent colour, red when nearly full,
/// empty while the size is not known yet.
pub(crate) fn usage_bar(space: Option<crate::explorer::drives::Space>, height: f32) -> Div {
    use nohrs_ui::theme::theme;
    let (fraction, color) = match space {
        Some(space) => (
            space.used_fraction().clamp(0.0, 1.0),
            if space.nearly_full() { theme::danger() } else { theme::accent() },
        ),
        None => (0.0, theme::accent()),
    };
    div()
        .w_full()
        .h(px(height))
        .rounded(px(height / 2.0))
        .bg(rgb(theme::border()))
        .overflow_hidden()
        .child(div().h_full().w(relative(fraction)).bg(rgb(color)))
}

/// Height of a group's header row in the list.
pub const GROUP_HEADER_HEIGHT: f32 = 34.0;

/// The header above a group of entries ("Imagens  12"), in the list and the
/// grid alike.
pub fn group_header(group: &crate::explorer::grouping::Group, width: f32) -> Div {
    use nohrs_ui::theme::theme;
    div()
        .w(px(width))
        .h(px(GROUP_HEADER_HEIGHT))
        .px(px(24.0))
        .flex()
        .items_end()
        .pb(px(6.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .w_full()
                .border_b_1()
                .border_color(rgb(theme::border()))
                .pb(px(4.0))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::fg()))
                        .child(group.label),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::fg_secondary()))
                        .child(group.len.to_string()),
                ),
        )
}

/// Makes Ctrl+wheel over the listing zoom it instead of scrolling it.
///
/// Caught in the capture phase: the virtual lists inside handle the wheel as it
/// bubbles, so by the time a bubbling handler here ran they would already have
/// scrolled.
fn with_wheel_zoom(
    page: &ExplorerPane,
    listing: AnyElement,
    scrollbar: gpui_component::scroll::Scrollbar,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let entity = cx.entity();
    let hit_boxes = page.hit_boxes.clone();
    div()
        .relative()
        .flex_1()
        .flex()
        .flex_col()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.open_context_menu(None, event.modifiers.shift, window, cx);
            }),
        )
        // Bubbles up after the entries' own handlers: a press that no entry
        // took (and no blank part of a row already turned into a rectangle)
        // was on empty space.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                if this.drag_candidate.is_none() && this.marquee.is_none() {
                    this.start_marquee(event.position, event.modifiers, None, cx);
                }
            }),
        )
        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
            this.drag_moved(event, window, cx);
        }))
        // After a row's own click, which has to see that the press became a
        // rectangle and leave the selection alone.
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                this.release_entry(cx);
                this.end_marquee(cx);
            }),
        )
        .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
            let folder = std::path::PathBuf::from(&this.cwd);
            this.drop_paths(paths.paths(), folder, window, cx);
        }))
        .child(listing)
        .child(scrollbar)
        .children(page.marquee_overlay())
        .child(
            canvas(
                move |bounds, window, _| {
                    hit_boxes.set_listing(bounds);
                    // The wheel belongs to the listing only where nothing is
                    // drawn over it: compact mode's sliding bars (sidebar,
                    // preview) float over the listing, and with the bounds
                    // alone the listing took their wheel, so they could not
                    // scroll and the files behind them did.
                    window.insert_hitbox(bounds, HitboxBehavior::Normal)
                },
                move |bounds, wheel_hitbox, window, _cx| {
                    let entity = entity.clone();
                    // The rectangle follows the pointer outside the listing
                    // too (that is how it scrolls), so these listen to the
                    // whole window.
                    window.on_mouse_event({
                        let entity = entity.clone();
                        move |event: &MouseMoveEvent, phase, _window, cx| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let pressed = event.pressed_button == Some(MouseButton::Left);
                            entity.update(cx, |page, cx| {
                                if page.marquee.is_some() {
                                    page.marquee_moved(event.position, pressed, cx);
                                }
                            });
                        }
                    });
                    window.on_mouse_event({
                        let entity = entity.clone();
                        move |event: &MouseUpEvent, phase, _window, cx| {
                            // Inside, the listing's own mouse-up ends it.
                            if phase != DispatchPhase::Capture
                                || event.button != MouseButton::Left
                                || bounds.contains(&event.position)
                            {
                                return;
                            }
                            entity.update(cx, |page, cx| page.end_marquee(cx));
                        }
                    });
                    window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture
                            || !bounds.contains(&event.position)
                            || !wheel_hitbox.is_hovered(window)
                        {
                            return;
                        }
                        if !event.modifiers.secondary() {
                            // A mouse wheel's notches glide (`smooth_scroll`);
                            // a touchpad's pixels and Shift+wheel go on to gpui.
                            if let ScrollDelta::Lines(_) = event.delta
                                && !event.modifiers.shift
                            {
                                let delta = event.delta.pixel_delta(window.line_height());
                                if entity.update(cx, |page, cx| page.smooth_wheel(delta, cx)) {
                                    cx.stop_propagation();
                                }
                            }
                            return;
                        }
                        let notches = match event.delta {
                            // One event per wheel notch, already multiplied by
                            // the system's lines-per-notch (3 by default), which
                            // made each notch jump three steps.
                            ScrollDelta::Lines(lines) => lines.y.signum(),
                            // Touchpads report pixels in many small events;
                            // about a notch's worth of travel per step keeps a
                            // swipe from racing through the whole range.
                            ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 60.0,
                        };
                        if notches != 0.0 {
                            entity.update(cx, |page, cx| {
                                let next = zoom::step(page.zoom_target, notches * zoom::STEP);
                                page.zoom_to(next, cx);
                                page.flash_zoom_menu(cx);
                            });
                        }
                        cx.stop_propagation();
                    });
                },
            )
            .absolute()
            .size_full(),
        )
        .into_any_element()
}

/// The whole size of a virtual list whose rows (or columns, when
/// `horizontal`) are `sizes`.
pub(crate) fn extent(sizes: &[Size<Pixels>], horizontal: bool) -> Size<Pixels> {
    let along: f32 = sizes
        .iter()
        .map(|item| f32::from(if horizontal { item.width } else { item.height }))
        .sum();
    let across = sizes
        .iter()
        .map(|item| f32::from(if horizontal { item.height } else { item.width }))
        .fold(0.0, f32::max);
    if horizontal { size(px(along), px(across)) } else { size(px(across), px(along)) }
}

// The listing's scrollbar, always shown, as Explorer's. Drawn over the
// panel, whose height is the view's, with the length of the whole folder
// (`listing_extent`): the virtual list lays out only the rows on screen, so
// a bar fed by its scroll handle alone never appeared.
fn listing_scrollbar(page: &ExplorerPane) -> gpui_component::scroll::Scrollbar {
    let (handle, horizontal) = match page.view_mode {
        ViewMode::List | ViewMode::Tree => (&page.virtual_scroll_handle, false),
        ViewMode::Grid => (&page.grid_scroll_handle, false),
        ViewMode::Compact => (&page.compact_scroll_handle, true),
    };
    let bar = if horizontal {
        gpui_component::scroll::Scrollbar::horizontal(handle)
    } else {
        gpui_component::scroll::Scrollbar::vertical(handle)
    };
    bar.id("listing-scrollbar")
        .scroll_size(page.listing_extent)
        .scrollbar_show(gpui_component::scroll::ScrollbarShow::Always)
}

/// Truncates `text` to at most `max_len` characters by eliding the middle,
/// preserving the file extension where possible.
pub fn truncate_middle(text: &str, max_len: usize) -> String {
    // Width reserved for the "..." elision marker.
    const ELLIPSIS: usize = 3;

    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_len {
        return text.to_string();
    }

    // Too little room for the "..." marker plus any content: just take the
    // leading characters so the result never exceeds `max_len` (the contract
    // above). Callers pass `max_len >= 8`, so this only guards pathological
    // inputs, but keeping the function self-consistent avoids surprises.
    if max_len <= ELLIPSIS {
        return chars[..max_len].iter().collect();
    }

    // Prefer eliding the middle of the *name* so the extension stays visible.
    // `checked_sub` guards the case where the extension alone (plus the marker)
    // already exceeds `max_len`: in debug builds the previous `max_len - ext - 3`
    // panicked on overflow, in release it would have wrapped to a huge budget.
    if let Some(dot_pos) = text.rfind('.') {
        let name_chars: Vec<char> = text[..dot_pos].chars().collect();
        let ext_part = &text[dot_pos..];
        let ext_chars = ext_part.chars().count();

        if let Some(budget) = max_len.checked_sub(ext_chars + ELLIPSIS) {
            if name_chars.len() > budget {
                let keep_start = budget / 2;
                let keep_end = budget - keep_start;

                let start_part: String = name_chars[..keep_start].iter().collect();
                let end_part: String = name_chars[name_chars.len() - keep_end..].iter().collect();

                return format!("{}...{}{}", start_part, end_part, ext_part);
            }
            return text.to_string();
        }
    }

    // No usable extension, or one too long to preserve: hard-truncate the whole
    // string, keeping the head and tail around the marker.
    let budget = max_len.saturating_sub(ELLIPSIS);
    let keep_start = budget / 2;
    let keep_end = budget - keep_start;

    let start_part: String = chars[..keep_start].iter().collect();
    let end_part: String = chars[chars.len() - keep_end..].iter().collect();

    format!("{}...{}", start_part, end_part)
}

#[cfg(test)]
mod truncate_middle_tests {
    use super::truncate_middle;

    #[test]
    fn keeps_short_names_unchanged() {
        assert_eq!(truncate_middle("Cargo.toml", 20), "Cargo.toml");
    }

    #[test]
    fn elides_middle_preserving_extension() {
        let out = truncate_middle("a-very-long-file-name.rs", 16);
        assert!(out.contains("..."));
        assert!(out.ends_with(".rs"));
        assert!(out.chars().count() <= 16);
    }

    #[test]
    fn does_not_panic_when_extension_exceeds_budget() {
        // Regression: an extension longer than `max_len - 3` previously
        // underflowed `max_len - ext - 3` and panicked in debug builds. Such
        // names reach this code path via search results.
        let _ = truncate_middle("x.0123456789012345678901234567890", 20);
        let _ = truncate_middle("no-dot-but-extremely-long-name-here", 8);
        let _ = truncate_middle(".gitignore", 5);
    }

    #[test]
    fn never_exceeds_max_len_even_below_ellipsis_width() {
        // The result must honor the "at most `max_len` characters" contract for
        // every budget, including ones too small to fit the "..." marker.
        for max_len in 0..=6 {
            let out = truncate_middle("really-long-filename.rs", max_len);
            assert!(
                out.chars().count() <= max_len,
                "max_len={max_len} produced {out:?}"
            );
        }
    }
}
