use crate::explorer::ExplorerPane;
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::Input;
use gpui_component::{Icon, IconName, Sizable as _, h_virtual_list};
use nohrs_services::fs::listing::FileEntryDto;
use nohrs_ui::theme::theme;
use std::rc::Rc;

/// Width of one column of names.
pub const COLUMN_WIDTH: f32 = 260.0;
/// Height of one name.
pub const ITEM_HEIGHT: f32 = 26.0;
const PADDING: f32 = 8.0;
/// How far one wheel notch (already counted in lines) moves the columns.
const WHEEL_STEP: f32 = 40.0;

/// Renders the List view, as File Pilot's and Explorer's: names only, running
/// down a column and on to the next, with the listing scrolling sideways.
///
/// Virtualized by column, so a folder of thousands lays out only the columns
/// on screen.
pub fn render(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let rows = (((page.compact_height - PADDING * 2.0) / ITEM_HEIGHT).floor() as usize).max(1);
    page.compact_rows = rows;
    let columns = page.filtered_entries.len().div_ceil(rows);
    let column_size = size(px(COLUMN_WIDTH), px(page.compact_height.max(ITEM_HEIGHT)));
    let sizes = Rc::new(vec![column_size; columns]);
    page.listing_extent = super::extent(&sizes, true);
    let icon_px = ((16.0 * window.scale_factor()).round() as u32).max(16);
    let entity = cx.entity();
    let scroll = page.compact_scroll_handle.clone();

    div()
        .id("compact-area")
        .relative()
        .flex_1()
        .w_full()
        // Without these the width of every column together became the
        // area's minimum and the columns ran over the preview pane.
        .min_w(px(0.0))
        .min_h(px(0.0))
        .overflow_hidden()
        .child(
            h_virtual_list(
                entity.clone(),
                "file-compact",
                sizes,
                move |page, visible_columns, _window, cx| {
                    visible_columns
                        .map(|column| {
                            let first = column * rows;
                            let last = (first + rows).min(page.filtered_entries.len());
                            let mut items = div()
                                .flex()
                                .flex_col()
                                .w(px(COLUMN_WIDTH))
                                .py(px(PADDING))
                                .pl(px(PADDING));
                            for ix in first..last {
                                let Some(item) = page.filtered_entries.get(ix).cloned() else {
                                    continue;
                                };
                                let icon = page.thumbnails.icon(&item.path, item.kind == "dir", icon_px, cx);
                                items = items.child(render_item(page, item, ix, icon, cx));
                            }
                            items
                        })
                        .collect()
                },
            )
            .track_scroll(&page.compact_scroll_handle),
        )

        .child(
            // Measures the height columns are filled to. A change is only
            // known after layout, so it takes effect on the next frame.
            canvas(
                move |bounds, window, cx| {
                    let height = f32::from(bounds.size.height);
                    entity.update(cx, |page, cx| {
                        if (page.compact_height - height).abs() > 0.5 {
                            page.compact_height = height;
                            cx.notify();
                        }
                    });
                    // As the listing's: no wheel through a bar over it.
                    window.insert_hitbox(bounds, HitboxBehavior::Normal)
                },
                move |bounds, wheel_hitbox, window, _| {
                    let scroll = scroll.clone();
                    // The wheel scrolls the columns sideways, as in Explorer's
                    // List view: there is nothing below to scroll to.
                    window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture
                            || event.modifiers.secondary()
                            || !bounds.contains(&event.position)
                            || !wheel_hitbox.is_hovered(window)
                        {
                            return;
                        }
                        let delta = match event.delta {
                            ScrollDelta::Lines(lines) if lines.x == 0.0 => lines.y * WHEEL_STEP,
                            ScrollDelta::Pixels(pixels) if pixels.x == px(0.0) => f32::from(pixels.y),
                            // A sideways gesture already scrolls the list.
                            _ => return,
                        };
                        let base = scroll.base_handle();
                        let mut offset = base.offset();
                        let max = f32::from(base.max_offset().width);
                        offset.x = px((f32::from(offset.x) + delta).clamp(-max, 0.0));
                        base.set_offset(offset);
                        window.refresh();
                        cx.stop_propagation();
                    });
                },
            )
            .absolute()
            .size_full(),
        )
        .into_any_element()
}

fn render_item(
    page: &ExplorerPane,
    item: FileEntryDto,
    ix: usize,
    icon: Option<std::sync::Arc<RenderImage>>,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let selected = page.is_selected(ix);
    let rename_input = page
        .renaming
        .as_ref()
        .filter(|state| state.index == ix)
        .map(|state| state.input.clone());
    let preview_item = item.clone();
    let activation_item = item.clone();
    let is_dir = item.kind == "dir";
    let name_color = if selected || !matches!(item.kind.as_str(), "file") {
        theme::fg()
    } else {
        page.file_type_color(&item.name).unwrap_or_else(theme::fg)
    };
    let highlight = page.highlight_query();

    div()
        .id(("compact-item", ix))
        .map(|this| page.hover_audio_target(this, &item, cx))
        .relative()
        .w(px(COLUMN_WIDTH - PADDING * 2.0))
        .h(px(ITEM_HEIGHT))
        .px(px(6.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme::accent_subtle())))
        .when(!selected, |this| this.hover(|this| this.bg(rgb(theme::bg_hover()))))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.open_context_menu(Some(ix), event.modifiers.shift, window, cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
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
        .child(match icon {
            Some(image) => img(ImageSource::Render(image))
                .size(px(16.0))
                .flex_shrink_0()
                .into_any_element(),
            None => Icon::new(if is_dir { IconName::Folder } else { IconName::File })
                .size_4()
                .text_color(rgb(theme::gray_500()))
                .into_any_element(),
        })
        .child(match rename_input {
            Some(input) => div()
                .flex_1()
                .min_w(px(0.0))
                .child(Input::new(&input).small().appearance(false).px(px(0.0)).text_sm())
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .flex_shrink()
                        .min_w(px(0.0))
                        .text_sm()
                        .text_color(rgb(name_color))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(crate::explorer::view::highlighted_name(&item.name, &highlight)),
                )
                .when_some(page.hit_folder(&item.path), |this, folder| {
                    this.child(crate::explorer::view::hit_folder_label(folder))
                })
                .into_any_element(),
        })
        .children(crate::explorer::hover_audio::progress_strip(&item.path))
        .children(page.reveal_glow(&item.path))
        .map(|this| page.track_hit(this, ix))
        .when(is_dir, |this| crate::explorer::drag::drop_into(this, &item.path, cx))
        .into_any_element()
}
