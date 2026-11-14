use super::truncate_middle;

use crate::explorer::ExplorerPane;
use gpui::prelude::*;
use gpui::*;
use gpui_component::Sizable as _;
use gpui_component::input::Input;
use gpui_component::list::ListItem;
use gpui_component::{Icon, IconName};
use nohrs_services::fs::listing::FileEntryDto;
use nohrs_ui::theme::theme;

/// Horizontal space a row's name column spends before the name itself: the
/// 20px chevron gutter, the 16px type icon, and the two 4px gaps around them.
/// The table header indents its "Name" label by the same amount so the column
/// heading lines up with the file names beneath it.
pub const NAME_INDENT: f32 = 44.0;
/// Space between a column's left line and its text, in the cells and the
/// headings alike.
pub const CELL_INSET: f32 = 10.0;
/// Edge of the type icon, in logical pixels.
pub const ICON_SIZE: f32 = 16.0;
/// How far each level of an open folder is indented in the Tree view.
const TREE_INDENT: f32 = 16.0;

/// Renders a single listing row for the given entry at row index `ix`.
pub fn render(
    page: &ExplorerPane,
    item: &FileEntryDto,
    ix: usize,
    shell_icon: Option<std::sync::Arc<gpui::RenderImage>>,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    use nohrs_ui::components::file_list::{format_date, get_file_type, human_bytes};

    let icon_name = match item.kind.as_str() {
        "dir" => IconName::Folder,
        _ => IconName::File,
    };
    let icon_color = match item.kind.as_str() {
        "dir" => rgb(theme::accent()),
        _ => rgb(theme::gray_500()),
    };

    let is_selected = page.is_selected(ix);
    let name_color = if is_selected || item.kind != "file" {
        theme::fg()
    } else {
        page.file_type_color(&item.name).unwrap_or_else(theme::fg)
    };
    let tree_view = page.view_mode == crate::explorer::types::ViewMode::Tree;
    let depth = if tree_view { page.tree_depth(&item.path) } else { 0 };
    let tree_folder = tree_view && item.kind == "dir";
    let tree_open = tree_folder && page.tree_open(&item.path);

    let file_type = get_file_type(&item.name, &item.kind);

    // Budget only the space the name actually gets: the column minus the
    // chevron gutter, icon and gaps that precede it (see `NAME_INDENT`).
    let max_chars = ((page.col_name_width - NAME_INDENT) / 7.0) as usize;
    let display_name = truncate_middle(&item.name, max_chars.max(20));

    let total_width = page.total_table_width();
    let item_for_preview = item.clone();
    let item_for_activate = item.clone();

    // Check if query matches filename (for highlighting)
    let query_lower = page.search_query.to_lowercase();
    let has_filename_match =
        !page.search_query.is_empty() && item.name.to_lowercase().contains(&query_lower);

    // Check if there are content matches (for expand arrow)
    let has_content_matches = page
        .search_results
        .as_ref()
        .map(|results| {
            results
                .iter()
                .any(|r| r.path == item.path && !r.matches.is_empty())
        })
        .unwrap_or(false);

    let is_expanded = page.expanded_search_files.contains(&item.path);

    let match_snippets: Vec<(usize, String)> = if is_expanded && has_content_matches {
        page.search_results
            .as_ref()
            .and_then(|results| {
                results.iter().find(|r| r.path == item.path).map(|r| {
                    r.matches
                        .iter()
                        .take(10) // Limit to 10 snippets
                        .map(|m| (m.line_number, m.line_content.clone()))
                        .collect()
                })
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let rename_input = page
        .renaming
        .as_ref()
        .filter(|state| state.index == ix)
        .map(|state| state.input.clone());

    let query = page.search_query.clone();
    let path_for_toggle = item.path.clone();
    let expand_icon = if is_expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    };

    // What a filter or search matched, in the accent colour.
    let styled_name = if has_filename_match && !page.search_query.is_empty() {
        let highlights = crate::explorer::view::find_query_highlights(&display_name, &query);
        StyledText::new(display_name.clone()).with_highlights(highlights)
    } else {
        crate::explorer::view::highlighted_name(&display_name, &page.highlight_query())
    };
    let hit_folder = page.hit_folder(&item.path).map(str::to_string);
    // A dot per tag the item carries, after its name, as the Files app shows.
    let tag_colors = crate::explorer::tags::Tags::colors_of(&item.path, cx);

    // ... rendering ...
    // Note: page.total_table_width() method is needed.
    // I need to make `total_table_width` pub on ExplorerPane. I did make fields pub, but method?
    // I should check if `total_table_width` is a method. Yes (line 347 in original).
    // I need to ensure that method is `pub` or copy logic.
    // Copying logic is safer: `page.col_name_width + ...`.
    // I'll copy the logic.

    let folder = (item.kind == "dir").then(|| item.path.clone());
    let row = div()
        .id(("list-row", ix))
        .map(|this| page.hover_audio_target(this, item, cx))
        .relative()
        .flex()
        .flex_col()
        .w(px(total_width))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseDownEvent, _window, cx| {
                // Right of the Name column a press draws a selection
                // rectangle, as in Explorer; on the name it picks up the file.
                if !(this.press_on_row_blank(ix, event.position)
                    && this.start_marquee(event.position, event.modifiers, Some(ix), cx))
                {
                    this.press_entry(ix, event.position, false);
                }
            }),
        )
        .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    // The rows sit inside the listing, whose own right click is
                    // the folder's menu.
                    cx.stop_propagation();
                    this.open_context_menu(Some(ix), event.modifiers.shift, window, cx);
                }),
            )
        .child(
            ListItem::new(("file-row", ix))
                .w(px(total_width))
                .h(px(32.0))
                .pr(px(24.0))
                // A flush-left accent bar carries the selection instead of the
                // former zebra striping, which fought with the selected color.
                .border_l_2()
                .border_color(rgb(if is_selected {
                    theme::accent()
                } else {
                    theme::bg()
                }))
                .pl(px(22.0))
                // `ListItem` paints its own hover fill, which also covers a
                // selected row's tint; the accent bar above is what keeps the
                // selection legible while the pointer is over it.
                .bg(rgb(if is_selected {
                    theme::accent_subtle()
                } else {
                    theme::bg()
                }))
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if let gpui::ClickEvent::Mouse(mouse) = event
                            && mouse.up.button == gpui::MouseButton::Left
                            && !this.marquee_dragging()
                        {
                            this.record_click(ix, mouse.up.click_count);
                            let modifiers = mouse.up.modifiers;
                            if modifiers.shift {
                                this.select_range_to(ix);
                            } else if modifiers.platform || modifiers.control {
                                this.toggle_select(ix);
                            } else {
                                this.select_single(ix);
                            }
                            if item_for_preview.kind == "file" || item_for_preview.kind == "dir" {
                                this.open_preview(item_for_preview.path.clone(), window, cx);
                            }
                            if mouse.up.click_count >= 2 {
                                this.activate_entry(item_for_activate.clone(), window, cx);
                            }
                            cx.notify();
                        }
                    }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .w_full()
                        .h_full()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .w(px(page.col_name_width))
                                .flex_shrink_0()
                                // The gutter before the Type column has to come
                                // off this container: padding on the label itself
                                // sits inside its `overflow_hidden` clip, so the
                                // text would still run to the column edge.
                                .pr(px(12.0))
                                .when(has_content_matches, |this| {
                                    this.child(
                                        div()
                                            // Same width as the empty gutter
                                            // below, so a row with matches keeps
                                            // its name aligned with the rest.
                                            .w(px(20.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .cursor_pointer()
                                            .hover(|s| {
                                                s.bg(rgb(theme::bg_hover())).rounded(px(4.0))
                                            })
                                            .on_mouse_down(
                                                gpui::MouseButton::Left,
                                                cx.listener({
                                                    let path = path_for_toggle.clone();
                                                    move |this, _, _, cx| {
                                                        if this
                                                            .expanded_search_files
                                                            .contains(&path)
                                                        {
                                                            this.expanded_search_files
                                                                .remove(&path);
                                                        } else {
                                                            this.expanded_search_files
                                                                .insert(path.clone());
                                                        }
                                                        this.update_item_sizes();
                                                        cx.notify();
                                                    }
                                                }),
                                            )
                                            .child(
                                                Icon::new(expand_icon)
                                                    .size_3()
                                                    .text_color(rgb(theme::gray_600())),
                                            ),
                                    )
                                })
                                .when(depth > 0, |this| {
                                    this.child(div().w(px(depth as f32 * TREE_INDENT)).flex_shrink_0())
                                })
                                .when(!has_content_matches && !tree_folder, |this| {
                                    this.child(div().w(px(20.0)).flex_shrink_0())
                                })
                                .when(tree_folder, |this| {
                                    let path = item.path.clone();
                                    this.child(
                                        div()
                                            .id(("tree-toggle", ix))
                                            .w(px(20.0))
                                            .h_full()
                                            .flex()
                                            .flex_shrink_0()
                                            .items_center()
                                            .justify_center()
                                            .cursor_pointer()
                                            .on_mouse_down(
                                                gpui::MouseButton::Left,
                                                cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.toggle_tree_folder(path.clone(), cx);
                                                }),
                                            )
                                            .child(
                                                Icon::new(if tree_open {
                                                    IconName::ChevronDown
                                                } else {
                                                    IconName::ChevronRight
                                                })
                                                .size_3()
                                                .text_color(rgb(theme::gray_600())),
                                            ),
                                    )
                                })
                                // The Windows icon once extracted; until then,
                                // or when the Shell has none, the generic one.
                                .child(match shell_icon {
                                    Some(image) => img(ImageSource::Render(image))
                                        .size(px(ICON_SIZE))
                                        .flex_shrink_0()
                                        .into_any_element(),
                                    None => Icon::new(icon_name)
                                        .size_4()
                                        .text_color(icon_color)
                                        .into_any_element(),
                                })
                                .child(match rename_input {
                                    // Borderless and flush with the label it
                                    // stands in for, so the row keeps its shape
                                    // and only the caret marks the edit.
                                    Some(input) => div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .child(
                                            // Matches the resting label's
                                            // typography exactly, so committing
                                            // the rename is not a visible jump.
                                            Input::new(&input)
                                                .small()
                                                .appearance(false)
                                                .px(px(0.0))
                                                .text_sm()
                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                .text_color(rgb(theme::fg())),
                                        )
                                        .into_any_element(),
                                    // `flex_1` + `min_w(0)` give the ellipsis a
                                    // width to clamp against; without them a long
                                    // name overflows into the next column.
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
                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                .text_color(rgb(name_color))
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                .whitespace_nowrap()
                                                .child(styled_name),
                                        )
                                        .when(!tag_colors.is_empty(), |this| {
                                            this.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(3.0))
                                                    .pl(px(6.0))
                                                    .children(tag_colors.iter().map(|color| {
                                                        div().size(px(8.0)).rounded_full().bg(rgb(*color))
                                                    })),
                                            )
                                        })
                                        .when_some(hit_folder, |this, folder| {
                                            this.child(crate::explorer::view::hit_folder_label(
                                                &folder,
                                            ))
                                        })
                                        .into_any_element(),
                                }),
                        )
                        .child(
                            div()
                                .w(px(page.col_type_width))
                                .pl(px(CELL_INSET))
                                .flex_shrink_0()
                                .text_sm()
                                .text_color(rgb(theme::fg_secondary()))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(file_type),
                        )
                        .child(
                            div()
                                .w(px(page.col_size_width))
                                .pl(px(CELL_INSET))
                                .flex_shrink_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_sm()
                                .text_color(rgb(theme::fg_secondary()))
                                .child(match item.kind.as_str() {
                                    "file" => human_bytes(item.size),
                                    // A folder found by the whole-computer
                                    // search knows what it holds; one in a
                                    // folder listing would need a walk of its
                                    // own, and shows nothing, as Explorer does.
                                    "dir" if item.size > 0 => human_bytes(item.size),
                                    "dir" => "-".to_string(),
                                    other => other.to_string(),
                                }),
                        )
                        .child(
                            div()
                                .w(px(page.col_modified_width))
                                .pl(px(CELL_INSET))
                                .flex_shrink_0()
                                .text_sm()
                                .text_color(rgb(theme::fg_secondary()))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(format_date(&item.modified)),
                        ),
                ),
        )
        .children(
            match_snippets
                .into_iter()
                .map(|(line_num, content)| {
                    let highlights = crate::explorer::view::find_query_highlights(&content, &query);
                    let styled = StyledText::new(content.clone()).with_highlights(highlights);
                    let path = item.path.clone();
                    div()
                        .id(SharedString::from(format!(
                            "snippet-{}-{}",
                            path.clone(),
                            line_num
                        )))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.scroll_to_line(line_num, window, cx);
                            cx.notify();
                        }))
                        .h(px(24.0))
                        .pl(px(48.0))
                        .pr(px(24.0))
                        .bg(rgb(theme::gray_50()))
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::muted()))
                                .w(px(32.0))
                                .child(format!("{}", line_num)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::fg_secondary()))
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(styled),
                        )
                })
                .collect::<Vec<_>>(),
        );
    let row = row
        .children(crate::explorer::hover_audio::progress_strip(&item.path))
        .children(page.reveal_glow(&item.path));
    let row = page.track_hit(row, ix);
    match folder {
        Some(folder) => crate::explorer::drag::drop_into(row, &folder, cx),
        None => row,
    }
}
