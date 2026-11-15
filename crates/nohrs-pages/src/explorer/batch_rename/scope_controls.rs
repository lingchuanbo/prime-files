use super::{controls, history, window::RenameWindow};
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
) -> Div {
    controls::section("处理范围", "按所选项目改名，或扫描所选目录内的文件。")
        .child(
            div().flex().gap_2().children(
                [(false, "所选项目"), (true, "目录内文件")]
                    .into_iter()
                    .map(|(mode, label)| {
                        Button::new(if mode {
                            "rename-mode-directory"
                        } else {
                            "rename-mode-selection"
                        })
                        .small()
                        .flex_1()
                        .label(label)
                        .disabled(view.busy)
                        .when(view.scope.options.directory_mode == mode, |button| {
                            button.primary()
                        })
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.scope.options.directory_mode = mode;
                            view.rescan(cx);
                        }))
                    }),
            ),
        )
        .child(
            Checkbox::new("rename-counter-per-directory")
                .debug_selector(|| "rename-counter-per-directory".into())
                .label("每个目录独立编号")
                .checked(view.scope.options.restart_counter)
                .disabled(view.busy)
                .on_click(cx.listener(|view, checked, _, cx| {
                    view.scope.options.restart_counter = *checked;
                    view.schedule_preview(cx);
                })),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child("勾选后各目录从起始值编号；起始 0、位数 5：A\\00000.png、B\\00000.png。"),
        )
        .when(view.scope.options.directory_mode, |section| {
            section
                .child(
                    Checkbox::new("rename-scope-recursive")
                        .label("包含所有子目录文件")
                        .checked(view.scope.options.recursive)
                        .disabled(view.busy)
                        .on_click(cx.listener(|view, checked, _, cx| {
                            view.scope.options.recursive = *checked;
                            view.rescan(cx);
                        })),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::fg_secondary()))
                                .child("文件格式（留空表示全部）"),
                        )
                        .child(history::input(
                            (&view.scope.inputs[1], 8, view.busy),
                            window,
                            cx,
                        )),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::fg_secondary()))
                        .child("支持 png、*.png;*.jpg；只改文件名，保留目录结构。"),
                )
                .child(
                    Button::new("rename-scope-scan")
                        .small()
                        .label(if view.scope.scanning {
                            "取消扫描"
                        } else {
                            "重新扫描"
                        })
                        .disabled(view.busy)
                        .on_click(cx.listener(|view, _, _, cx| {
                            if view.scope.scanning {
                                view.stop_scan(cx);
                            } else {
                                view.rescan(cx);
                            }
                        })),
                )
        })
}
