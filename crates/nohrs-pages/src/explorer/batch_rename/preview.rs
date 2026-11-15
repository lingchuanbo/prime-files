use super::{chrome, plan::Preview, window::RenameWindow};
use gpui::{prelude::*, *};
use nohrs_ui::theme::theme;

pub(super) fn render(view: &RenameWindow, cx: &Context<RenameWindow>) -> impl IntoElement + use<> {
    let rows = view.rows.clone();
    let applied = view.applied;
    let pending = view.pending;
    div()
        .debug_selector(|| "rename-preview".into())
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .child(chrome::preview_toolbar(view, cx))
        .child(headings())
        .when(view.rows.is_empty(), |body| {
            body.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .p_6()
                    .child(div().text_sm().child(if pending {
                        "正在生成预览…"
                    } else {
                        "暂时无法生成预览"
                    }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::fg_secondary()))
                            .child(if pending {
                                "文件较多时，请稍候。"
                            } else {
                                "请检查左侧规则，详细原因见下方提示。"
                            }),
                    ),
            )
        })
        .when(!view.rows.is_empty(), |body| {
            body.child(
                uniform_list("batch-rename-preview", rows.len(), move |range, _, _| {
                    range
                        .filter_map(|index| {
                            rows.get(index)
                                .map(|item| row(item, index, (applied, pending)))
                        })
                        .collect::<Vec<_>>()
                })
                .flex_1()
                .min_h_0(),
            )
        })
}

fn headings() -> Div {
    div()
        .h_8()
        .flex_shrink_0()
        .px_4()
        .flex()
        .items_center()
        .gap_3()
        .text_xs()
        .text_color(rgb(theme::fg_secondary()))
        .bg(rgb(theme::footer_bg()))
        .border_b_1()
        .border_color(rgb(theme::border()))
        .child(div().w_6().flex_shrink_0().child("#"))
        .child(div().flex_1().min_w_0().child("原名称 / 所在目录"))
        .child(div().w_4().flex_shrink_0())
        .child(div().flex_1().min_w_0().child("新名称"))
        .child(div().w(px(96.0)).flex_shrink_0().child("状态"))
}

fn row(item: &Preview, index: usize, state: (bool, bool)) -> impl IntoElement + use<> {
    let path = std::path::Path::new(&item.entry.path)
        .parent()
        .unwrap_or(std::path::Path::new(""))
        .to_string_lossy()
        .into_owned();
    let (applied, pending) = state;
    let (status, color) = row_status(item, (applied, pending));
    let details = format!(
        "原名称：{}\n新名称：{}\n所在目录：{}\n{}",
        item.entry.name, item.name, path, status
    );
    div()
        .id(("batch-rename-row", index))
        .h(px(60.0))
        .px_4()
        .flex()
        .items_center()
        .gap_3()
        .text_sm()
        .border_b_1()
        .border_color(rgb(theme::border()))
        .when(index % 2 == 1, |row| row.bg(rgb(theme::bg_secondary())))
        .hover(|row| row.bg(rgb(theme::bg_hover())))
        .child(
            div()
                .w_6()
                .flex_shrink_0()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child((index + 1).to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(ellipsis(item.entry.name.clone()))
                .child(
                    ellipsis(path)
                        .text_xs()
                        .text_color(rgb(theme::fg_secondary())),
                ),
        )
        .child(
            div()
                .w_4()
                .flex_shrink_0()
                .text_color(rgb(theme::muted()))
                .child("→"),
        )
        .child(
            ellipsis(item.name.clone())
                .flex_1()
                .when(item.changed(), |cell| cell.text_color(rgb(color))),
        )
        .child(
            div()
                .w(px(96.0))
                .flex_shrink_0()
                .text_xs()
                .text_color(rgb(color))
                .child(ellipsis(status)),
        )
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(details.clone()).build(window, cx)
        })
}

fn row_status(item: &Preview, state: (bool, bool)) -> (String, u32) {
    match state {
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
