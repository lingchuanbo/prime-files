use super::{chrome, plan::Preview, window::RenameWindow};
use gpui::{prelude::*, *};
use gpui_component::{Disableable, checkbox::Checkbox};
use nohrs_core::telemetry::LogErr;
use nohrs_ui::theme::theme;

pub(super) fn render(view: &RenameWindow, cx: &Context<RenameWindow>) -> impl IntoElement + use<> {
    let rows = view.rows.clone();
    let entity = cx.entity().downgrade();
    div()
        .debug_selector(|| "rename-preview".into())
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .child(chrome::preview_toolbar(view, cx))
        .child(headings())
        .when(view.rows.is_empty(), |body| body.child(empty_state(view)))
        .when(!view.rows.is_empty(), |body| {
            body.child(
                uniform_list("batch-rename-preview", rows.len(), move |range, _, cx| {
                    entity
                        .read_with(cx, |view, _| {
                            range
                                .filter_map(|index| {
                                    rows.get(index).map(|item| {
                                        row(
                                            item,
                                            index,
                                            RowContext {
                                                view: entity.clone(),
                                                directory: view
                                                    .scope
                                                    .directory_label(&item.entry.path),
                                                selected: !view
                                                    .scope
                                                    .excluded
                                                    .contains(&item.entry.path),
                                                disabled: view.busy
                                                    || view.scope.scanning
                                                    || view.applied,
                                                state: (view.applied, view.pending),
                                            },
                                        )
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .flex_1()
                .min_h_0(),
            )
        })
}

fn empty_state(view: &RenameWindow) -> Div {
    let (title, description) = if view.scope.scanning {
        ("正在扫描目录…", "可在左侧取消扫描，完成后显示匹配文件。")
    } else if view.pending {
        ("正在生成预览…", "文件较多时，请稍候。")
    } else if view.scope.ready && view.entries.is_empty() {
        (
            "没有匹配的文件",
            "请调整文件格式或子目录选项；如需更换目录，请回到主界面重新选择。",
        )
    } else {
        ("暂时无法生成预览", "请检查左侧输入，详细原因见下方提示。")
    };
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .p_6()
        .child(div().text_sm().child(title))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child(description),
        )
}

fn headings() -> Div {
    div()
        .h_8()
        .flex_shrink_0()
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .text_color(rgb(theme::fg_secondary()))
        .bg(rgb(theme::footer_bg()))
        .border_b_1()
        .border_color(rgb(theme::border()))
        .child(div().w_5().flex_shrink_0())
        .child(div().flex_1().min_w_0().child("原名称"))
        .child(div().flex_1().min_w_0().child("新名称"))
        .child(div().w(px(144.0)).flex_shrink_0().child("所在目录"))
        .child(div().w(px(76.0)).flex_shrink_0().child("状态"))
}

struct RowContext {
    view: WeakEntity<RenameWindow>,
    directory: String,
    selected: bool,
    disabled: bool,
    state: (bool, bool),
}

fn row(item: &Preview, index: usize, context: RowContext) -> impl IntoElement + use<> {
    let (status, color) = row_status(item, context.state);
    let details = format!(
        "原名称：{}\n新名称：{}\n完整路径：{}\n{}",
        item.entry.name, item.name, item.entry.path, status
    );
    let path = item.entry.path.clone();
    div()
        .id(("batch-rename-row", index))
        .debug_selector(move || format!("rename-row-{index}"))
        // A list row starts at its content width; without this the columns
        // stop lining up with the headings above them (each cell the width of
        // its text) and the directory lands under the new-name heading.
        .w_full()
        .h(px(44.0))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .border_b_1()
        .border_color(rgb(theme::border()))
        .when(index % 2 == 1, |row| row.bg(rgb(theme::bg_secondary())))
        .hover(|row| row.bg(rgb(theme::bg_hover())))
        .child(
            div().w_5().flex_shrink_0().child(
                Checkbox::new(("rename-file-check", index))
                    .checked(context.selected)
                    .disabled(context.disabled)
                    .on_click(move |selected, _, cx| {
                        context
                            .view
                            .update(cx, |view, cx| view.select_file(&path, *selected, cx))
                            .log_err();
                    }),
            ),
        )
        .child(ellipsis(item.entry.name.clone()).flex_1())
        .child(
            // The name a row keeps is grey, as the list tools people come from
            // show it: nothing will happen to that file.
            ellipsis(item.name.clone())
                .flex_1()
                .text_color(rgb(if item.changed() {
                    color
                } else {
                    theme::muted()
                })),
        )
        .child(
            ellipsis(context.directory)
                .w(px(144.0))
                .flex_shrink_0()
                .text_xs()
                .text_color(rgb(theme::fg_secondary())),
        )
        .child(
            ellipsis(status)
                .w(px(76.0))
                .flex_shrink_0()
                .text_xs()
                .text_color(rgb(color)),
        )
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(details.clone()).build(window, cx)
        })
}

fn row_status(item: &Preview, state: (bool, bool)) -> (String, u32) {
    match state {
        _ if !item.selected => ("已排除".into(), theme::muted()),
        (_, true) => ("等待更新".into(), theme::muted()),
        _ if item.error.is_some() => (
            format!("需处理：{}", item.error.as_deref().unwrap_or_default()),
            theme::danger(),
        ),
        _ if !item.changed() => ("保持不变".into(), theme::fg_secondary()),
        (true, _) => ("已完成".into(), theme::accent()),
        _ => ("待重命名".into(), theme::accent()),
    }
}

fn ellipsis(text: String) -> Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .child(text)
}
