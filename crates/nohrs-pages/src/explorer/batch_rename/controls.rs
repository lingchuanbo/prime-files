use super::{history, rules::CaseMode, window::RenameWindow};
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
};
use nohrs_ui::theme::theme;

pub(super) fn render(
    view: &RenameWindow,
    window: &mut Window,
    cx: &mut Context<RenameWindow>,
) -> impl IntoElement + use<> {
    div()
        .id("rename-settings")
        .debug_selector(|| "rename-settings".into())
        .w(px(344.0))
        .h_full()
        .flex_shrink_0()
        .overflow_y_scroll()
        .bg(rgb(theme::bg_secondary()))
        .border_r_1()
        .border_color(rgb(theme::border()))
        .child(
            div()
                .p_4()
                .flex()
                .flex_col()
                .gap_5()
                .child(super::scope_controls::render(view, window, cx))
                .child(naming(view, window, cx))
                .child(replacement(view, window, cx))
                .child(counter(view, window, cx))
                .child(case_options(view, cx)),
        )
}

pub(super) fn section(title: &'static str, description: &'static str) -> Div {
    div().flex().flex_col().gap_3().child(
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(hint(description)),
    )
}

fn hint(text: &'static str) -> Div {
    div()
        .text_xs()
        .text_color(rgb(theme::fg_secondary()))
        .child(text)
}

fn naming(view: &RenameWindow, window: &mut Window, cx: &mut Context<RenameWindow>) -> Div {
    section("命名规则", "输入文字，或点击下方占位符插入名称。")
        .child(field(view, (0, "文件名"), (window, cx)))
        .child(tokens(view, cx))
        .child(field(view, (1, "扩展名"), (window, cx)))
        .child(hint("扩展名不含句点；留空可移除扩展名。"))
        .child(
            div()
                .flex()
                .gap_2()
                .child(preset(view, (0, "目录 + 原名", "[B]_[N]"), cx))
                .child(preset(view, (1, "原名 + 编号", "[N]_[C]"), cx)),
        )
}

fn field(
    view: &RenameWindow,
    field: (usize, &'static str),
    context: (&mut Window, &mut App),
) -> Div {
    let (index, label) = field;
    let (window, cx) = context;
    div()
        .debug_selector(|| format!("rename-field-{index}"))
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child(label),
        )
        .child(history::input(
            (&view.inputs[index], index, view.busy),
            window,
            cx,
        ))
}

fn preset(
    view: &RenameWindow,
    preset: (usize, &'static str, &'static str),
    cx: &Context<RenameWindow>,
) -> Button {
    let (index, label, rule) = preset;
    Button::new(("rename-preset", index))
        .small()
        .label(label)
        .disabled(view.busy)
        .tooltip(format!("将文件名规则设为 {rule}"))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.inputs[0].update(cx, |input, cx| {
                input.set_value(rule, window, cx);
                input.focus(window, cx);
            });
        }))
}

fn tokens(view: &RenameWindow, cx: &Context<RenameWindow>) -> Div {
    let tokens = [
        (
            "[N]",
            "原名",
            "不含扩展名的原名称；[N1-3] 可截取第 1–3 个字",
        ),
        ("[B]", "目录", "文件所在目录的名称"),
        ("[B1]", "上级", "文件所在目录的上一级目录名称"),
        ("[B2]", "上上级", "文件所在目录的上上级目录名称"),
        ("[C]", "编号", "按下方计数器设置生成序号"),
        ("[YMD]", "日期", "文件修改日期"),
        ("[hms]", "时间", "文件修改时间"),
        (
            "[拼音]",
            "全拼",
            "将原名称（不含扩展名）中的汉字转为小写无声调全拼，例如：中国 → zhongguo；其他字符保留，多音字使用默认读音",
        ),
    ];
    div()
        .flex()
        .flex_wrap()
        .gap_2()
        .children(
            tokens
                .into_iter()
                .enumerate()
                .map(|(index, (token, label, help))| {
                    Button::new(("rename-token", index))
                        .small()
                        .label(format!("{token} {label}"))
                        .tooltip(help)
                        .disabled(view.busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.inputs[0].update(cx, |input, cx| {
                                input.replace_text_in_range(None, token, window, cx);
                                input.focus(window, cx);
                            });
                        }))
                }),
        )
}

fn replacement(view: &RenameWindow, window: &mut Window, cx: &mut Context<RenameWindow>) -> Div {
    section("查找与替换", "区分大小写；查找留空时不替换。")
        .child(field(view, (2, "查找"), (window, cx)))
        .child(field(view, (3, "替换为"), (window, cx)))
        .child(
            Checkbox::new("rename-first")
                .label("仅替换第一次匹配")
                .checked(view.first_only)
                .disabled(view.busy)
                .on_click(cx.listener(|this, checked, _, cx| {
                    this.first_only = *checked;
                    this.schedule_preview(cx);
                })),
        )
        .child(
            Checkbox::new("rename-extension")
                .label("同时替换扩展名中的文字")
                .checked(view.include_extension)
                .disabled(view.busy)
                .on_click(cx.listener(|this, checked, _, cx| {
                    this.include_extension = *checked;
                    this.schedule_preview(cx);
                })),
        )
}

fn counter(view: &RenameWindow, window: &mut Window, cx: &mut Context<RenameWindow>) -> Div {
    section("计数器 [C]", "在文件名中插入 [C] 后生效。")
        .child(
            div()
                .flex()
                .gap_2()
                .child(field(view, (4, "起始值"), (window, cx)))
                .child(field(view, (5, "步长"), (window, cx)))
                .child(field(view, (6, "位数"), (window, cx))),
        )
        .child(hint("例如：起始 1、步长 1、位数 3 → 001、002、003"))
}

fn case_options(view: &RenameWindow, cx: &Context<RenameWindow>) -> Div {
    section("大小写", "应用于生成后的名称和扩展名。").children(
        [
            [CaseMode::Keep, CaseMode::Lower],
            [CaseMode::Upper, CaseMode::Title],
        ]
        .into_iter()
        .enumerate()
        .map(|(row, options)| {
            div()
                .flex()
                .gap_2()
                .children(options.into_iter().enumerate().map(|(column, mode)| {
                    Button::new(("rename-case", row * 2 + column))
                        .small()
                        .flex_1()
                        .label(mode.label())
                        .disabled(view.busy)
                        .when(view.case == mode, |button| button.primary())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.case = mode;
                            this.schedule_preview(cx);
                        }))
                }))
        }),
    )
}
