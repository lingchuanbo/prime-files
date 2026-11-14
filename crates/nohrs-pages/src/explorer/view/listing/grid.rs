use crate::explorer::grouping::{self, GridRow};
use crate::explorer::{ExplorerPane, zoom};
use gpui::prelude::*;
use gpui::*;
use gpui_component::Sizable as _;
use gpui_component::input::Input;
use gpui_component::v_virtual_list;
use gpui_component::{Icon, IconName};
use nohrs_services::fs::listing::FileEntryDto;
use nohrs_ui::theme::theme;
use std::rc::Rc;

/// Room around the thumbnail inside a tile, and between tiles.
const TILE_PADDING: f32 = 10.0;
const TILE_GAP: f32 = 8.0;
/// Line height of the name (`text_xs`). The whole name is shown, on as many
/// lines as it takes, as in Explorer's icon views; each row of tiles is as
/// tall as its longest name. There is no size or date line under it, as
/// there is none in Explorer.
const NAME_LINE_HEIGHT: f32 = 16.0;
/// The `gap_1` the tile puts between thumbnail and name.
const TILE_ROW_GAP: f32 = 4.0;
const GRID_PADDING_X: f32 = 16.0;

/// Renders the file listing as a grid of tiles sized by the pane's zoom.
///
/// Virtualized by row: a directory of thousands of photos lays out, and asks
/// for thumbnails of, only the rows on screen.
pub fn render(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let icon = zoom::icon_size(page.zoom);
    let tile_width = icon + TILE_PADDING * 2.0;
    // The tile's inner width: its padding and the 1px border on each side.
    let name_width = tile_width - TILE_PADDING * 2.0 - 2.0;
    let tile_height = move |lines: usize| icon + TILE_PADDING * 2.0 + TILE_ROW_GAP + NAME_LINE_HEIGHT * lines as f32;
    let usable = (page.grid_width - GRID_PADDING_X * 2.0).max(tile_width);
    let columns = (((usable + TILE_GAP) / (tile_width + TILE_GAP)).floor() as usize).max(1);
    page.grid_columns = columns;
    let thumbnail_size = zoom::thumbnail_bucket(icon, window.scale_factor());

    // Every row's height has to be known before the list lays out, so the
    // names are wrapped for all rows, not just those on screen. That used to
    // happen on every frame, and every thumbnail that arrives is a frame: in a
    // folder of 35,000 that was 35,000 wraps per tile loaded. Now it happens
    // when the rows, the width or the zoom change; while the zoom glides it is
    // still every frame, but each name was shaped once (`name_layouts`) and
    // wrapping it at a new width is arithmetic that allocates nothing.
    let key = GridKey {
        generation: page.rows_generation,
        entries: page.filtered_entries.len(),
        columns,
        name_width: name_width.to_bits(),
        width: page.grid_width.max(tile_width).to_bits(),
        rem: f32::from(window.rem_size()).to_bits(),
    };
    let layout = match page.grid_layout.as_ref().filter(|layout| layout.key == key) {
        Some(layout) => layout.clone(),
        None => {
            let layout = build_layout(page, key, name_width, tile_height, window);
            page.grid_layout = Some(layout.clone());
            layout
        }
    };
    let GridLayout {
        rows,
        row_lines,
        row_sizes,
        ..
    } = layout;
    let entity = cx.entity();
    page.listing_extent = super::extent(&row_sizes, false);
    aim_reveal(page, window);

    div()
        .id("grid-area")
        .relative()
        .flex_1()
        .min_h(px(0.0))
        .overflow_hidden()
        .child(
            v_virtual_list(
                entity.clone(),
                "file-grid",
                row_sizes,
                move |page, visible_rows, window, cx| {
                    visible_rows
                        .filter_map(|row| {
                            let (first, last) = match rows.get(row)? {
                                GridRow::Group(group) => {
                                    let group = page.groups().get(*group)?;
                                    return Some(
                                        div()
                                            .pt(px(TILE_GAP))
                                            .child(super::group_header(group, page.grid_width))
                                            .into_any_element(),
                                    );
                                }
                                GridRow::Tiles { first, end } => (*first, *end),
                            };
                            let mut line = div()
                                .flex()
                                .flex_row()
                                .gap(px(TILE_GAP))
                                .px(px(GRID_PADDING_X))
                                .pt(px(TILE_GAP));
                            for ix in first..last {
                                let Some(item) = page.filtered_entries.get(ix).cloned() else {
                                    continue;
                                };
                                let selected = page.is_selected(ix);
                                let lines = row_lines.get(row).copied().unwrap_or(1);
                                let tile = TileSize {
                                    icon,
                                    width: tile_width,
                                    height: tile_height(lines),
                                    label_lines: lines,
                                    thumbnail: thumbnail_size,
                                };
                                line = line.child(render_grid_item(
                                    page, item, ix, selected, tile, window, cx,
                                ));
                            }
                            Some(line.into_any_element())
                        })
                        .collect()
                },
            )
            .track_scroll(&page.grid_scroll_handle),
        )

        .child(
            // Measures the width rows are laid out against. A change is only
            // known after layout, so it takes effect on the next frame.
            canvas(
                move |bounds, _window, cx| {
                    let width = f32::from(bounds.size.width);
                    entity.update(cx, |page, cx| {
                        if (page.grid_width - width).abs() > 0.5 {
                            page.grid_width = width;
                            cx.notify();
                        }
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .into_any_element()
}

/// Scrolls to the entry `reveal` asked for, now that `grid_columns` is
/// this frame's. Before the canvas below has measured the grid, there is no
/// width to count columns from, so it waits; once the width it aimed with
/// holds for a frame, the reveal is done.
fn aim_reveal(page: &mut ExplorerPane, window: &mut Window) {
    let Some((index, aimed)) = page.reveal_scroll else {
        return;
    };
    if page.grid_width <= 0.0 {
        window.request_animation_frame();
        return;
    }
    if aimed == Some(page.grid_width) {
        page.reveal_scroll = None;
        return;
    }
    page.scroll_entry_into_view(index);
    page.reveal_scroll = Some((index, Some(page.grid_width)));
    window.request_animation_frame();
}

/// What the grid's row layout was built for; any change rebuilds it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct GridKey {
    generation: u64,
    entries: usize,
    columns: usize,
    name_width: u32,
    width: u32,
    rem: u32,
}

/// The grid's rows, the lines of the longest name in each, and their sizes.
#[derive(Clone)]
pub(crate) struct GridLayout {
    key: GridKey,
    rows: Rc<Vec<GridRow>>,
    row_lines: Rc<Vec<usize>>,
    row_sizes: Rc<Vec<Size<Pixels>>>,
}

fn build_layout(
    page: &mut ExplorerPane,
    key: GridKey,
    name_width: f32,
    tile_height: impl Fn(usize) -> f32,
    window: &Window,
) -> GridLayout {
    // The cap follows the folder: a fixed 50,000 cleared the cache on every
    // build in a larger folder, and every name was shaped again each time.
    if page.name_layouts.len() > NAME_LAYOUTS_KEPT.max(page.filtered_entries.len() * 2) {
        page.name_layouts.clear();
    }
    let rows = grouping::grid_rows(page.groups(), page.filtered_entries.len(), key.columns);
    let row_lines: Vec<usize> = rows
        .iter()
        .map(|row| match row {
            GridRow::Group(_) => 0,
            GridRow::Tiles { first, end } => (*first..*end)
                .filter_map(|ix| page.filtered_entries.get(ix))
                .map(|entry| {
                    let layout = name_layout(&mut page.name_layouts, &entry.name, window);
                    let mut lines = 0;
                    wrap_layout(&layout, &entry.name, name_width, |_| lines += 1);
                    limit_lines(lines.max(1), page.name_lines)
                })
                .max()
                .unwrap_or(1),
        })
        .collect();
    let width = px(f32::from_bits(key.width));
    let row_sizes = rows
        .iter()
        .zip(row_lines.iter())
        .map(|(row, &lines)| match row {
            GridRow::Group(_) => size(width, px(super::GROUP_HEADER_HEIGHT + TILE_GAP)),
            GridRow::Tiles { .. } => size(width, px(tile_height(lines) + TILE_GAP)),
        })
        .collect();
    GridLayout {
        key,
        rows: Rc::new(rows),
        row_lines: Rc::new(row_lines),
        row_sizes: Rc::new(row_sizes),
    }
}

#[derive(Clone, Copy)]
struct TileSize {
    icon: f32,
    width: f32,
    height: f32,
    /// Lines the tallest name in this row takes; every tile in it gets them.
    label_lines: usize,
    thumbnail: u32,
}

/// Names kept laid out before the cache starts over, or twice the folder on
/// screen if that is more; a folder of tens of thousands of files would
/// otherwise hold every name it ever showed. Leaving the folder clears it.
const NAME_LAYOUTS_KEPT: usize = 50_000;

fn render_grid_item(
    page: &mut ExplorerPane,
    item: FileEntryDto,
    ix: usize,
    selected: bool,
    tile: TileSize,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let name_width = tile.width - 2.0 * TILE_PADDING - 2.0;
    let name_lines = wrap_name(&mut page.name_layouts, &item.name, name_width, page.name_lines, window);
    let query = page.highlight_query();

    let rename_input = page
        .renaming
        .as_ref()
        .filter(|state| state.index == ix)
        .map(|state| state.input.clone());

    let icon_name = match item.kind.as_str() {
        "dir" => IconName::Folder,
        _ => IconName::File,
    };

    let is_dir = item.kind == "dir";
    let name_color = if selected || item.kind != "file" {
        theme::fg()
    } else {
        page.file_type_color(&item.name).unwrap_or_else(theme::fg)
    };
    // A sound file with waveforms on asks for its waveform and, until it
    // arrives or if it cannot be had, the type's icon: one image shared by
    // every file of the extension, where the thumbnail would have the Shell
    // read each file for an icon that is about to be covered.
    let waveform_wanted =
        page.audio_waveforms && item.kind == "file" && crate::explorer::waveforms::is_audio(&item.path);
    let waveform = waveform_wanted
        .then(|| page.waveforms.get(&item.path, item.size, item.modified, cx))
        .flatten();
    let thumbnail = match (waveform_wanted, &waveform) {
        (_, Some(_)) => None,
        (true, None) => page.thumbnails.icon(&item.path, false, tile.thumbnail, cx),
        (false, None) => page.thumbnails.get(&item.path, tile.thumbnail, cx),
    };
    let waveform_shown = waveform.is_some();
    let activation_item = item.clone();
    let preview_item = item.clone();

    let bg_color = if selected {
        rgb(theme::accent_subtle())
    } else {
        rgb(theme::bg())
    };

    // Only the selection is outlined: a border round every tile turns a page of
    // small icons into a page of boxes.
    let border_color = if selected {
        rgb(theme::accent())
    } else {
        rgb(theme::bg())
    };

    div()
        .id(("grid-tile", ix))
        .map(|this| page.hover_audio_target(this, &item, cx))
        .relative()
        .flex_shrink_0()
        .w(px(tile.width))
        .h(px(tile.height))
        .p(px(TILE_PADDING))
        .rounded(px(8.0))
        .border_1()
        .border_color(border_color)
        .bg(bg_color)
        .when(!selected, |this| {
            this.hover(|this| this.bg(rgb(theme::bg_secondary())))
        })
        .cursor_pointer()
        .flex()
        .flex_col()
        .items_center()
        .text_center()
        .gap_1()
        .on_mouse_down(
            gpui::MouseButton::Right,
            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                // The rows sit inside the listing, whose own right click is
                // the folder's menu.
                cx.stop_propagation();
                this.open_context_menu(Some(ix), event.modifiers.shift, window, cx);
            }),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                this.record_click(ix, event.click_count);
                let modifiers = event.modifiers;
                let plain = !(modifiers.shift || modifiers.platform || modifiers.control);
                let keep_selection = this.press_entry(ix, event.position, plain);
                if modifiers.shift {
                    this.select_range_to(ix);
                } else if modifiers.platform || modifiers.control {
                    this.toggle_select(ix);
                } else if !keep_selection {
                    this.select_single(ix);
                }
                if preview_item.kind == "file" || preview_item.kind == "dir" {
                    this.open_preview(preview_item.path.clone(), window, cx);
                }
                if event.click_count >= 2 {
                    this.activate_entry(activation_item.clone(), window, cx);
                }
                cx.notify();
            }),
        )
        .child(
            div()
                .size(px(tile.icon))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .child(match (waveform, thumbnail) {
                    (Some(bars), _) => crate::explorer::waveforms::canvas(
                        bars,
                        tile.icon,
                        selected,
                        crate::explorer::hover_audio::progress_of(&item.path),
                    ),
                    (None, Some(image)) => img(ImageSource::Render(image))
                        .size_full()
                        .object_fit(ObjectFit::Contain)
                        .into_any_element(),
                    (None, None) => Icon::new(icon_name)
                        .size(px((tile.icon * 0.6).max(24.0)))
                        .text_color(rgb(if is_dir {
                            theme::accent()
                        } else {
                            theme::gray_500()
                        }))
                        .into_any_element(),
                }),
        )
        .child(match rename_input {
            Some(input) => div()
                .w_full()
                .h(px(20.0))
                .flex_shrink_0()
                .child(
                    Input::new(&input)
                        .small()
                        .appearance(false)
                        .px(px(0.0))
                        .text_xs()
                        .text_color(rgb(theme::fg())),
                )
                .into_any_element(),
            // The whole name, centered under the icon as Explorer draws it.
            // Each line is laid out by `wrap_name` rather than by gpui's
            // wrapping, which does not break a run with no spaces
            // (`PineTools.com_2026-10-12_12h16m40s`) and left a line wider
            // than the tile, clipped on both sides by the centering.
            None => div()
                .w_full()
                .h(px(NAME_LINE_HEIGHT * tile.label_lines as f32))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .items_center()
                .text_xs()
                .line_height(px(NAME_LINE_HEIGHT))
                .text_color(rgb(name_color))
                .overflow_hidden()
                .children(name_lines.into_iter().map(|line| {
                    div()
                        .max_w_full()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(crate::explorer::view::highlighted_name(&line, &query))
                }))
                .into_any_element(),
        })
        // Without a waveform to colour, a line along the tile's foot follows
        // the sound playing under the mouse.
        .when(!waveform_shown, |this| this.children(crate::explorer::hover_audio::progress_strip(&item.path)))
        .children(page.reveal_glow(&item.path))
        .map(|this| page.track_hit(this, ix))
        .when(is_dir, |this| crate::explorer::drag::drop_into(this, &item.path, cx))
        .into_any_element()
}

/// Splits a tile's `name` into the lines it shows, each no wider than
/// `width`, as Explorer wraps icon labels: after a space, `-`, `_` or `.` when
/// one falls in the second half of the line, mid-word otherwise. Nothing is
/// cut, unless `ui.name_lines` caps the lines (`max_lines`, 0 = no cap):
/// then the last one shown ends in "…".
fn wrap_name(
    layouts: &mut std::collections::HashMap<String, std::sync::Arc<LineLayout>>,
    name: &str,
    width: f32,
    max_lines: u32,
    window: &Window,
) -> Vec<String> {
    let layout = name_layout(layouts, name, window);
    let mut lines = Vec::new();
    wrap_layout(&layout, name, width, |line| lines.push(line.to_string()));
    if lines.is_empty() {
        lines.push(String::new());
    }
    cap_lines(&mut lines, max_lines);
    lines
}

/// Lines a name of `lines` lines takes under a cap of `max_lines` (0 = none).
fn limit_lines(lines: usize, max_lines: u32) -> usize {
    match max_lines {
        0 => lines,
        cap => lines.min(cap as usize),
    }
}

/// Drops the lines past `max_lines` and ends the last one kept with "…".
/// Two characters make room for it: the line already filled the width, and
/// "…" is about as wide as two letters of the label's font.
fn cap_lines(lines: &mut Vec<String>, max_lines: u32) {
    let keep = limit_lines(lines.len(), max_lines);
    if keep == lines.len() {
        return;
    }
    lines.truncate(keep);
    if let Some(last) = lines.last_mut() {
        let chars = last.chars().count();
        let kept: String = last.chars().take(chars.saturating_sub(2)).collect();
        *last = format!("{}…", kept.trim_end());
    }
}

/// `name` shaped in the label's font, once per name.
fn name_layout(
    layouts: &mut std::collections::HashMap<String, std::sync::Arc<LineLayout>>,
    name: &str,
    window: &Window,
) -> std::sync::Arc<LineLayout> {
    if let Some(layout) = layouts.get(name) {
        return layout.clone();
    }
    let style = window.text_style();
    // `text_xs`, which the label is drawn in.
    let font_size = rems(0.75).to_pixels(window.rem_size());
    let layout = window
        .text_system()
        .layout_line(name, font_size, &[style.to_run(name.len())], None);
    layouts.insert(name.to_string(), layout.clone());
    layout
}

/// Calls `line` with each line of `name` wrapped at `width`. The grid counts
/// the lines of every name and draws those of a few, so the wrap itself
/// allocates nothing.
fn wrap_layout(layout: &LineLayout, name: &str, width: f32, mut line: impl FnMut(&str)) {
    let width = px(width);
    let mut start = 0;
    while start < name.len() {
        // The first character that does not fit after `start`; none when the
        // rest of the name does.
        let Some(fits) = layout.index_for_x(layout.x_for_index(start) + width) else {
            line(&name[start..]);
            break;
        };
        let at_least_one = start + name[start..].chars().next().map_or(1, char::len_utf8);
        let end = name[start..fits]
            .rfind([' ', '-', '_', '.'])
            .map(|at| start + at + 1)
            .filter(|&at| at > start + (fits - start) / 2)
            .unwrap_or(fits)
            .max(at_least_one);
        line(name[start..end].trim_end());
        start = end + name[end..].len() - name[end..].trim_start_matches(' ').len();
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings in gpui's `test` attribute in place of std's.
    use super::{cap_lines, limit_lines};

    #[test]
    fn a_cap_keeps_the_first_lines_and_ends_with_an_ellipsis() {
        let mut lines = vec!["um nome ".to_string(), "bem longo".to_string(), "demais".to_string()];
        cap_lines(&mut lines, 2);
        assert_eq!(lines, ["um nome ", "bem lon…"]);
        assert_eq!(limit_lines(3, 2), 2);
    }

    #[test]
    fn no_cap_leaves_every_line() {
        let mut lines = vec!["a".to_string(), "b".to_string()];
        cap_lines(&mut lines, 0);
        assert_eq!(lines, ["a", "b"]);
        cap_lines(&mut lines, 5);
        assert_eq!(lines, ["a", "b"]);
    }
}
