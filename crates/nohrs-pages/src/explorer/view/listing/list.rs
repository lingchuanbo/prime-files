use super::row;
use crate::explorer::ExplorerPane;
use crate::explorer::grouping::{self, ListRow};
use crate::explorer::types::SortKey;
use gpui::prelude::*;
use gpui::*;
use gpui_component::v_virtual_list;
use gpui_component::{Icon, IconName};
use nohrs_core::t;
use nohrs_ui::theme::theme;
use std::rc::Rc;

/// The left margin of every row before the Name column (the rows' 22px
/// padding and their 2px selection bar).
pub(crate) const ROW_MARGIN: f32 = 24.0;
/// Width of the line on each heading's right edge.
const SEPARATOR_WIDTH: f32 = 1.0;

/// Height of the sticky column-header row. Shared with the preview pane's
/// header so the two line up across the divider, and with the virtual list's
/// size table, which must agree with what the header actually paints.
pub const HEADER_HEIGHT: f32 = 40.0;

/// Renders the file listing as a virtualized multi-column table.
pub fn render(page: &mut ExplorerPane, cx: &mut Context<ExplorerPane>) -> AnyElement {
    let table_width = page.total_table_width();
    let col_name = page.col_name_width;
    let col_type = page.col_type_width;
    let col_size = page.col_size_width;
    let col_modified = page.col_modified_width;
    let col_action = page.col_action_width;

    div()
        .size_full()
        .flex()
        .flex_col()
        .min_h(px(0.0))
        .overflow_hidden()
        .child(render_table_with_header(
            page,
            table_width,
            col_name,
            col_type,
            col_size,
            col_modified,
            col_action,
            cx,
        ))
        .into_any_element()
}

// Column widths are passed individually to mirror the table layout; grouping
// them into a struct would not improve clarity here.
#[allow(clippy::too_many_arguments)]
fn render_table_with_header(
    page: &mut ExplorerPane,
    table_width: f32,
    col_name: f32,
    col_type: f32,
    col_size: f32,
    col_modified: f32,
    col_action: f32,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    let entity = cx.entity().clone();

    // Rebuilt when the rows or the table's width change, not on every frame:
    // each thumbnail or icon that arrives is a frame, and this is two vectors
    // the length of the folder.
    let key = (
        page.rows_generation,
        page.filtered_entries.len(),
        table_width.to_bits(),
    );
    let ListLayout {
        rows, all_sizes, ..
    } = match page.list_layout.as_ref().filter(|layout| layout.key == key) {
        Some(layout) => layout.clone(),
        None => {
            let rows = Rc::new(grouping::list_rows(
                page.groups(),
                page.filtered_entries.len(),
            ));
            let mut all_sizes = vec![gpui::size(px(table_width), px(HEADER_HEIGHT))];
            all_sizes.extend(rows.iter().map(|row| match row {
                ListRow::Group(_) => gpui::size(px(table_width), px(super::GROUP_HEADER_HEIGHT)),
                ListRow::Entry(index) => {
                    page.item_sizes.get(*index).copied().unwrap_or_else(|| {
                        gpui::size(px(table_width), px(nohrs_core::config::BASE_ROW_HEIGHT))
                    })
                }
            }));
            let layout = ListLayout {
                key,
                rows,
                all_sizes: Rc::new(all_sizes),
            };
            page.list_layout = Some(layout.clone());
            layout
        }
    };
    let scroll_handle = page.virtual_scroll_handle.clone();
    page.listing_extent = super::extent(&all_sizes, false);

    div().flex_1().overflow_hidden().child(
        v_virtual_list(
            entity,
            "file-table",
            all_sizes,
            move |view, visible_range, window, cx| {
                // The Shell draws icons at the exact pixel size asked for, so
                // the row's 16px icon is fetched at the screen's scale.
                let icon_px = ((row::ICON_SIZE * window.scale_factor()).round() as u32).max(16);
                visible_range
                    .filter_map(|ix| {
                        if ix == 0 {
                            Some(
                                render_header_row(
                                    view,
                                    table_width,
                                    col_name,
                                    col_type,
                                    col_size,
                                    col_modified,
                                    col_action,
                                    cx,
                                )
                                .into_any_element(),
                            )
                        } else {
                            match rows.get(ix - 1)? {
                                ListRow::Group(group) => {
                                    let group = view.groups().get(*group)?;
                                    Some(super::group_header(group, table_width).into_any_element())
                                }
                                ListRow::Entry(data_ix) => {
                                    let data_ix = *data_ix;
                                    view.filtered_entries.get(data_ix).cloned().map(|item| {
                                        let icon = view.thumbnails.icon(
                                            &item.path,
                                            item.kind == "dir",
                                            icon_px,
                                            cx,
                                        );
                                        row::render(view, &item, data_ix, icon, cx)
                                            .into_any_element()
                                    })
                                }
                            }
                        }
                    })
                    .collect()
            },
        )
        .track_scroll(&scroll_handle),
    )
}

#[allow(clippy::too_many_arguments)]
fn render_header_row(
    page: &ExplorerPane,
    table_width: f32,
    col_name: f32,
    col_type: f32,
    col_size: f32,
    col_modified: f32,
    col_action: f32,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    div()
        .w(px(table_width))
        .h(px(HEADER_HEIGHT))
        .pr(px(24.0))
        .bg(rgb(theme::bg()))
        .border_b_1()
        .border_color(rgb(theme::border()))
        // A press on the headings sorts or resizes; it is not empty space
        // for a selection rectangle.
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        // The header's own menu, not the folder's.
        .on_mouse_down(
            gpui::MouseButton::Right,
            cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.open_column_menu(None, window, cx);
            }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .w_full()
                .h_full()
                .child(render_resizable_column_header(
                    page,
                    t!("Nome"),
                    SortKey::Name,
                    0,
                    col_name,
                    cx,
                ))
                .child(render_resizable_column_header(
                    page,
                    t!("Tipo"),
                    SortKey::Type,
                    1,
                    col_type,
                    cx,
                ))
                .child(render_resizable_column_header(
                    page,
                    t!("Tamanho"),
                    SortKey::Size,
                    2,
                    col_size,
                    cx,
                ))
                .child(render_resizable_column_header(
                    page,
                    t!("Modificado"),
                    SortKey::Modified,
                    3,
                    col_modified,
                    cx,
                ))
                .child(div().w(px(col_action)).flex_shrink_0()),
        )
}

fn render_resizable_column_header(
    page: &ExplorerPane,
    label: &str,
    key: SortKey,
    column_index: usize,
    width: f32,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    // We need to call page methods like start_column_resize which take &mut self.
    // But `v_virtual_list` closure gives `view` (&mut ExplorerPane), so `page` here could be &mut?
    // In `render_table_with_header` closure, `view` is passed to `render_header_row`.
    // So `page` is &mut ExplorerPane.
    // I should change signature to &mut ExplorerPane if needed.
    // But `start_column_resize` is called in event handler which takes `this` (mut).
    // The `page` argument is used to bind listener.
    // So `page` doesn't need to be mut here, the lambda captures things?
    // Wait, `cx.listener` callback receives `this: &mut V` (ExplorerPane).
    // So `page` is just used for rendering state (width etc).
    // `render_resizable_column_header` logic:
    // ... .child(render_column_header(...))
    // ... .child(div()... on_mouse_down(... this.start_column_resize ...))

    // The name column's rows lead with a chevron gutter and a type icon, so its
    // heading is indented to match; the other columns start flush. The column
    // resizes down to `MIN_COLUMN_WIDTH` (80px), where the full indent would
    // push the label and sort icon over the Type column, so give those a
    // reserved slice and let the indent absorb the rest.
    const HEADING_RESERVE: f32 = 52.0;
    let leading_indent = if column_index == 0 {
        (width - HEADING_RESERVE).clamp(0.0, super::row::NAME_INDENT)
    } else {
        // Clear of the previous column's line; the cells below are inset by
        // the same amount, so headings and values stay in line.
        super::row::CELL_INSET
    };

    // The Name heading also takes the rows' left margin, so its hover runs to
    // the edge of the table instead of stopping short of it.
    let margin = if column_index == 0 { ROW_MARGIN } else { 0.0 };
    let group = SharedString::from(format!("column-heading-{column_index}"));

    div()
        .id(("column-heading", column_index))
        .group(group.clone())
        .w(px(width + margin))
        .h(px(HEADER_HEIGHT))
        .flex_shrink_0()
        .relative()
        // A hidden column is width 0; its heading must not spill out.
        .overflow_hidden()
        .when(width == 0.0, |this| this.invisible())
        .pl(px(margin + leading_indent))
        // The heading under the pointer lightens, cell by cell, as
        // Explorer's do. Drawn as a layer that ends where the separator on
        // the cell's right edge begins, so the line stays visible.
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .right(px(SEPARATOR_WIDTH))
                .group_hover(group, |style| style.bg(rgb(theme::bg_hover()))),
        )
        .on_mouse_down(
            gpui::MouseButton::Right,
            cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.open_column_menu(Some(column_index), window, cx);
            }),
        )
        .child(render_column_header(
            page,
            label,
            key,
            column_index,
            false,
            cx,
        ))
        .child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .w(px(8.0))
                .h_full()
                .cursor_col_resize()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _window, cx| {
                        this.start_column_resize(column_index, event.position);
                        cx.stop_propagation();
                    }),
                )
                // The line on the cell's very edge: the hover layer then
                // runs from the previous column's line to this one with no
                // gap on either side.
                .child(
                    div()
                        .w(px(1.0))
                        .h_full()
                        .ml(px(7.0))
                        .bg(rgb(theme::border())),
                ),
        )
}

fn render_column_header(
    page: &ExplorerPane,
    label: &str,
    key: SortKey,
    key_idx: usize,
    flex: bool,
    cx: &mut Context<ExplorerPane>,
) -> gpui::Div {
    let is_active = page.sort_key == key;
    let label_str = label.to_string();
    let sort_icon = if page.sort_asc {
        IconName::ChevronUp
    } else {
        IconName::ChevronDown
    };

    let mut wrapper = div();

    if flex {
        wrapper = wrapper.flex_1();
    }

    wrapper.child(
        // `ListItem` pads itself horizontally; zeroed here so each heading sits
        // flush with the column data below it.
        // Plain, not a `ListItem`: its hover fill covered the whole column
        // heading in a lighter block. Hovering lights the label instead, as
        // Explorer's headings do.
        div()
            .id(("sort-header", key_idx))
            .h(px(HEADER_HEIGHT))
            .flex()
            .items_center()
            .cursor_pointer()
            .group(SharedString::from(format!("sort-header-{key_idx}")))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_sort_key(key, cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(if is_active {
                                rgb(theme::fg())
                            } else {
                                rgb(theme::fg_secondary())
                            })
                            .group_hover(
                                SharedString::from(format!("sort-header-{key_idx}")),
                                |style| style.text_color(rgb(theme::fg())),
                            )
                            .child(label_str),
                    )
                    .when(is_active, |this| {
                        this.child(
                            Icon::new(sort_icon)
                                .size_3()
                                .text_color(rgb(theme::accent())),
                        )
                    }),
            ),
    )
}

/// The list's rows and sizes, with what they were built for: the rows'
/// generation, how many there are, and the table's width.
#[derive(Clone)]
pub(crate) struct ListLayout {
    key: (u64, usize, u32),
    rows: Rc<Vec<ListRow>>,
    all_sizes: Rc<Vec<gpui::Size<Pixels>>>,
}
