use crate::explorer::ExplorerPane;
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::Input;
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, IconName, Sizable as _};
use nohrs_core::{t, tf};
use nohrs_ui::theme::theme;

/// 搜索框在普通与紧凑模式下均放在底栏左侧。
pub(super) fn render(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    let input = page.quick_input(window, cx);
    let everything = page.everything_on();
    let recursive = page.quick_recursive;
    let placeholder = if everything {
        page.everything_status(cx).unwrap_or_else(|| t!("Buscar em todo o computador…").to_string())
    } else if recursive {
        t!("Buscar nas subpastas…").to_string()
    } else {
        tf!("Filtrar {files} arquivos…", files = page.entries.iter().filter(|entry| entry.kind != "dir").count())
    };
    input.update(cx, |state, cx| state.set_placeholder(placeholder, window, cx));
    let typed = input.read(cx).value().to_string();
    if everything {
        page.search_everything(typed, cx);
    } else if recursive {
        page.search_subfolders(typed, cx);
    } else if typed != page.name_filter {
        page.name_filter = typed;
        page.apply_filter();
    }
    div()
        .id("directory-search")
        .w(px(if page.compact.enabled { 240.0 } else { 280.0 }))
        .min_w(px(120.0))
        .h(px(24.0))
        .flex().items_center().gap_1().px(px(6.0))
        .rounded(px(8.0)).bg(rgb(theme::bg_secondary()))
        .border_1().border_color(rgb(theme::border()))
        .child(everything_button(everything, cx))
        .child(div().flex_1().min_w(px(0.0)).child(
            Input::new(&input).xsmall().appearance(false).cleanable(true).text_xs(),
        ))
        .child(recursive_button(recursive, cx))
}

fn everything_button(on: bool, cx: &mut Context<ExplorerPane>) -> impl IntoElement + use<> {
    search_button("quick-bar-everything", on)
        .tooltip(|window, cx| {
            Tooltip::new(nohrs_core::shortcuts::with_keys(
                t!("Buscar em todo o computador"), nohrs_core::shortcuts::Action::SearchEverything,
            )).build(window, cx)
        })
        .on_click(cx.listener(|page, _, window, cx| {
            let on = !page.everything_on();
            page.set_everything(on, cx);
            page.focus_quick_input(None, window, cx);
        }))
        .child(Icon::new(IconName::Search).size_3p5())
}

fn recursive_button(on: bool, cx: &mut Context<ExplorerPane>) -> impl IntoElement + use<> {
    search_button("quick-bar-subfolders", on)
        .tooltip(|window, cx| Tooltip::new(t!("Buscar nas subpastas…")).build(window, cx))
        .on_click(cx.listener(|page, _, window, cx| {
            page.set_quick_recursive(!page.quick_recursive, window, cx);
        }))
        .child(Icon::new(IconName::FolderOpen).size_3p5())
}

fn search_button(id: &'static str, on: bool) -> Stateful<Div> {
    div().id(id).size(px(22.0)).flex().flex_shrink_0()
        .items_center().justify_center().rounded(px(5.0)).cursor_pointer()
        .text_color(rgb(if on { theme::accent() } else { theme::muted() }))
        .when(on, |style| style.bg(rgb(theme::accent_subtle())))
        .hover(|style| style.bg(rgb(theme::bg_hover())).text_color(rgb(theme::fg())))
}
