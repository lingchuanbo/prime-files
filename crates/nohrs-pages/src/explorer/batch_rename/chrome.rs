use super::window::RenameWindow;
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
};
use nohrs_ui::theme::theme;

pub(super) fn footer(view: &RenameWindow, cx: &Context<RenameWindow>) -> Div {
    let has_errors =
        !view.pending && (view.rows.is_empty() || view.rows.iter().any(|row| row.error.is_some()));
    div()
        .debug_selector(|| "rename-footer".into())
        .flex_shrink_0()
        .px_5()
        .py_3()
        .flex()
        .items_center()
        .gap_4()
        .border_t_1()
        .border_color(rgb(theme::border()))
        .child(
            div()
                .id("rename-status")
                .flex_1()
                .min_w_0()
                .max_h(px(82.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .when(has_errors, |label| label.text_color(rgb(theme::danger())))
                        .child(view.status.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::fg_secondary()))
                        .child("Ctrl+Enter 开始 · Esc 关闭 · 关闭窗口后无法撤销本次操作"),
                ),
        )
        .child(
            Button::new("rename-undo")
                .label("撤销")
                .tooltip("撤销最近一次重命名；关闭窗口后失效")
                .disabled(view.undo.is_none() || view.busy || view.pending)
                .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
        )
        .child(
            Button::new("rename-close")
                .label("关闭")
                .disabled(view.busy)
                .on_click(|_, window, _| window.remove_window()),
        )
        .child(
            Button::new("rename-start")
                .primary()
                .label(if view.busy {
                    "正在处理…"
                } else {
                    "开始重命名"
                })
                .tooltip("确认右侧预览后执行（Ctrl+Enter）")
                .disabled(!view.can_apply())
                .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
        )
}

pub(super) fn preview_toolbar(view: &RenameWindow, cx: &Context<RenameWindow>) -> Div {
    let changed = view
        .rows
        .iter()
        .filter(|row| row.changed() && row.error.is_none())
        .count();
    let errors = view.rows.iter().filter(|row| row.error.is_some()).count();
    div()
        .h(px(58.0))
        .flex_shrink_0()
        .px_4()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child("结果预览"),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child(if view.pending {
                    "正在更新…".into()
                } else if view.applied {
                    format!("已完成 {changed} 项")
                } else {
                    format!("待更改 {changed} 项")
                }),
        )
        .when(errors > 0, |bar| {
            bar.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::danger()))
                    .child(format!("{errors} 项需处理")),
            )
        })
        .child(div().flex_1())
        .child(
            Button::new("rename-refresh")
                .small()
                .ghost()
                .label("刷新预览")
                .tooltip("重新检查文件与名称冲突；修改规则时自动更新预览")
                .disabled(view.busy || view.pending)
                .on_click(cx.listener(|this, _, _, cx| this.schedule_preview(cx))),
        )
}
