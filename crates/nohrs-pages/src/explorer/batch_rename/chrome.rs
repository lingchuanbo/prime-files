use super::window::RenameWindow;
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    popover::Popover,
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
                .disabled(view.undo.is_none() || view.busy || view.pending || view.scope.scanning)
                .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
        )
        .child(
            Button::new("rename-close")
                .label("关闭")
                .disabled(view.busy)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.remember(cx);
                    window.remove_window();
                })),
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
    let selected = view.rows.iter().filter(|row| row.selected).count();
    let changed = view
        .rows
        .iter()
        .filter(|row| row.changed() && row.error.is_none())
        .count();
    let errors = view.rows.iter().filter(|row| row.error.is_some()).count();
    div()
        .min_h(px(58.0))
        .py_2()
        .flex_wrap()
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
                    format!("已选 {selected}/{} · 待改 {changed}", view.rows.len())
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
        .child(selection_button(view, (true, "全选"), cx))
        .child(selection_button(view, (false, "全不选"), cx))
        .when(!view.scope.issues.is_empty(), |bar| {
            bar.child(scan_issues(view.scope.issues.clone()))
        })
        .child(
            Button::new("rename-refresh")
                .small()
                .ghost()
                .label("刷新预览")
                .tooltip("重新检查文件与名称冲突；修改规则时自动更新预览")
                .disabled(view.busy || view.pending || view.scope.scanning || !view.scope.ready)
                .on_click(cx.listener(|this, _, _, cx| this.schedule_preview(cx))),
        )
}

fn selection_button(
    view: &RenameWindow,
    option: (bool, &'static str),
    cx: &Context<RenameWindow>,
) -> Button {
    Button::new(if option.0 {
        "rename-select-all"
    } else {
        "rename-select-none"
    })
    .small()
    .ghost()
    .label(option.1)
    .disabled(view.busy || view.scope.scanning || view.rows.is_empty() || view.applied)
    .on_click(cx.listener(move |view, _, _, cx| view.select_all(option.0, cx)))
}

fn scan_issues(issues: Vec<String>) -> Popover {
    let count = issues.len();
    Popover::new("rename-scan-issues")
        .trigger(
            Button::new("rename-scan-issues-button")
                .small()
                .ghost()
                .label(format!("扫描提示 {count}")),
        )
        .content(move |_, _, _| {
            div()
                .w(px(480.0))
                .flex()
                .flex_col()
                .gap_2()
                .child(div().text_sm().child("以下路径未纳入本次重命名"))
                .child(
                    div()
                        .id("rename-scan-issues-list")
                        .max_h(px(280.0))
                        .overflow_y_scroll()
                        .text_xs()
                        .children(issues.iter().map(|issue| div().py_1().child(issue.clone()))),
                )
        })
}
