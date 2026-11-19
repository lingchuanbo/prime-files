use crate::explorer::ExplorerPane;
use crate::explorer::filter::KindFilter;
use gpui::prelude::*;
use gpui::*;
use gpui_component::{Icon, IconName};
use nohrs_core::tf;
use nohrs_ui::theme::theme;

/// Renders the filter bar: a name pattern and one chip per kind of entry.
pub fn render(
    page: &mut ExplorerPane,
    _window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    // The name pattern is typed in the bar under the listing; this bar holds
    // the kinds.
    let shown = page.filtered_entries.len();
    let total = page.entries.len();
    let active = page.kind_filter;

    div()
        .id("filter-bar")
        .flex_shrink_0()
        .w_full()
        .h(px(40.0))
        .px(px(16.0))
        .flex()
        .items_center()
        .gap_2()
        .bg(rgb(theme::bg()))
        .border_b_1()
        .border_color(rgb(theme::border()))
        .child(
            Icon::new(IconName::Search)
                .size_4()
                .text_color(rgb(theme::fg_secondary())),
        )
        .child(
            div()
                .id("filter-kinds")
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap_1()
                .overflow_x_scroll()
                .children(KindFilter::ALL.into_iter().map(|kind| {
                    let selected = kind == active;
                    div()
                        .id(SharedString::from(format!("filter-kind-{}", kind.label())))
                        .flex_shrink_0()
                        .px(px(10.0))
                        .py(px(3.0))
                        .rounded(px(12.0))
                        .text_xs()
                        .cursor_pointer()
                        .border_1()
                        .when(selected, |chip| {
                            chip.bg(rgb(theme::accent_subtle()))
                                .border_color(rgb(theme::accent()))
                                .text_color(rgb(theme::accent()))
                        })
                        .when(!selected, |chip| {
                            chip.border_color(rgb(theme::border()))
                                .text_color(rgb(theme::fg_secondary()))
                                .hover(|chip| chip.bg(rgb(theme::bg_hover())))
                        })
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.set_kind_filter(kind, cx);
                        }))
                        .child(kind.label())
                })),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .whitespace_nowrap()
                .child(tf!("{shown} de {total}", shown = shown, total = total)),
        )
        .child(
            div()
                .id("filter-close")
                .flex_shrink_0()
                .p(px(4.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|button| button.bg(rgb(theme::bg_hover())))
                .on_click(cx.listener(|page, _, _, cx| page.toggle_filter_bar(cx)))
                .child(
                    Icon::new(IconName::Close)
                        .size_4()
                        .text_color(rgb(theme::fg_secondary())),
                ),
        )
}
