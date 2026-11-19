use crate::explorer::ExplorerPane;
use crate::explorer::types::SearchType;
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::Input;
use gpui_component::list::ListItem;
use gpui_component::{Icon, IconName};
use nohrs_core::{t, tf};
use nohrs_services::search::SearchScope;
use nohrs_ui::theme::theme;

/// Renders the search bar with the query input and search option toggles.
pub fn render(page: &mut ExplorerPane, cx: &mut Context<ExplorerPane>) -> impl IntoElement + use<> {
    let current_text = page.search_input.read(cx).text().to_string();
    if current_text != page.search_query {
        page.search_query = current_text;
        // If query changed, revert to file list filter until user explicitly triggers search?
        // Or we could auto-search? Auto-search is expensive for full content.
        // So default to file filter.
        page.search_results = None;
        page.apply_filter();
    }

    let is_empty = page.search_query.is_empty();
    let match_count = if let Some(results) = &page.search_results {
        results.iter().map(|r| r.matches.len()).sum()
    } else {
        page.filtered_entries.len()
    };
    let is_full_search = page.search_results.is_some();
    let status_text = if is_full_search {
        tf!("{n} resultados no conteúdo", n = match_count)
    } else if !is_empty {
        tf!("{n} arquivos filtrados", n = match_count)
    } else {
        String::new()
    };

    let _search_scope = page.search_scope;
    let _search_type = page.search_type;
    let match_case = page.match_case;
    let match_whole_word = page.match_whole_word;
    let use_regex = page.use_regex;

    div()
        .w_full()
        .bg(rgb(theme::bg()))
        .border_b_1()
        .border_color(rgb(theme::border()))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|_this, _ev: &gpui::MouseDownEvent, _window, cx| {
                cx.stop_propagation();
            }),
        )
        .on_mouse_up(
            gpui::MouseButton::Left,
            cx.listener(|_this, _ev: &gpui::MouseUpEvent, _window, cx| {
                cx.stop_propagation();
            }),
        )
        .on_mouse_move(
            cx.listener(|_this, _ev: &gpui::MouseMoveEvent, _window, cx| {
                cx.stop_propagation();
            }),
        )
        .on_scroll_wheel(cx.listener(|_this, _ev, _window, cx| {
            cx.stop_propagation();
        }))
        .child(
            // Scope Selection
            div()
                .flex()
                .gap_4()
                .px(px(24.0))
                .pt(px(8.0))
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .items_center()
                        .child(t!("Onde:"))
                        .child(render_scope_button(
                            page,
                            SearchScope::Home,
                            t!("Pasta pessoal"),
                            cx,
                        ))
                        .child(render_scope_button(page, SearchScope::Root, t!("Raiz"), cx)),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .items_center()
                        .child(t!("Tipo:"))
                        .child(render_type_button(page, SearchType::All, t!("Tudo"), cx))
                        .child(render_type_button(
                            page,
                            SearchType::Filename,
                            t!("Nome"),
                            cx,
                        ))
                        .child(render_type_button(
                            page,
                            SearchType::Content,
                            t!("Conteúdo"),
                            cx,
                        )),
                ),
        )
        .child(
            div()
                .flex()
                .items_start()
                .gap_2()
                .px(px(24.0))
                .py(px(10.0))
                .child(
                    Icon::new(IconName::Search)
                        .size_4()
                        .text_color(rgb(theme::fg_secondary()))
                        .mt(px(8.0)), // Align with input
                )
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child({
                            let si = page.search_input.clone();
                            div()
                                .on_key_down(cx.listener(
                                    |this, event: &gpui::KeyDownEvent, window, cx| {
                                        if event.keystroke.key == "enter" {
                                            this.trigger_search(window, cx);
                                        } else if event.keystroke.key == "escape" {
                                            this.toggle_search(window, cx);
                                        }
                                    },
                                ))
                                .child(Input::new(&si))
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(render_toggle_button(
                                    "Aa",
                                    match_case,
                                    |this, cx| this.toggle_match_case(cx),
                                    cx,
                                ))
                                .child(render_toggle_button(
                                    "ab",
                                    match_whole_word,
                                    |this, cx| this.toggle_match_whole_word(cx),
                                    cx,
                                ))
                                .child(render_toggle_button(
                                    ".*",
                                    use_regex,
                                    |this, cx| this.toggle_use_regex(cx),
                                    cx,
                                ))
                                .child(div().flex_1())
                                .child(
                                    // Run Search Button
                                    div()
                                        .cursor_pointer()
                                        .bg(rgb(theme::accent()))
                                        .text_color(rgb(theme::accent_fg()))
                                        .px(px(10.0))
                                        .py(px(4.0))
                                        .rounded(px(6.0))
                                        .text_xs()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(t!("Buscar"))
                                        .hover(|this| this.bg(rgb(theme::accent_hover())))
                                        .on_mouse_down(
                                            gpui::MouseButton::Left,
                                            cx.listener(|this, _, window, cx| {
                                                this.trigger_search(window, cx);
                                            }),
                                        ),
                                ),
                        )
                        .child(
                            div().h(px(18.0)).child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::fg_secondary()))
                                    .when(!status_text.is_empty(), |this| this.child(status_text)),
                            ),
                        ),
                )
                .child(
                    ListItem::new("close-search")
                        .px(px(6.0))
                        .py(px(6.0))
                        .rounded(px(6.0))
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.toggle_search(window, cx);
                        }))
                        .child(
                            Icon::new(IconName::Close)
                                .size_4()
                                .text_color(rgb(theme::fg_secondary())),
                        ),
                ),
        )
}

fn render_scope_button(
    page: &ExplorerPane,
    scope: SearchScope,
    label: &str,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    let is_active = page.search_scope == scope;
    chip(label, is_active).on_mouse_down(
        gpui::MouseButton::Left,
        cx.listener(move |this, _, _, cx| {
            this.set_search_scope(scope, cx);
        }),
    )
}

/// A small selectable pill used by the scope and type rows, so both read the
/// same and the active one keeps `ACCENT_FG` on `ACCENT` for legibility.
fn chip(label: &str, is_active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(format!("search-chip-{label}")))
        .cursor_pointer()
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(10.0))
        .when(is_active, |this| {
            this.bg(rgb(theme::accent()))
                .text_color(rgb(theme::accent_fg()))
                .font_weight(gpui::FontWeight::MEDIUM)
        })
        .when(!is_active, |this| {
            this.text_color(rgb(theme::fg_secondary()))
                .hover(|s| s.bg(rgb(theme::bg_hover())))
        })
        .child(label.to_string())
}

fn render_type_button(
    page: &ExplorerPane,
    search_type: SearchType,
    label: &str,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    let is_active = page.search_type == search_type;
    chip(label, is_active).on_mouse_down(
        gpui::MouseButton::Left,
        cx.listener(move |this, _, _, cx| {
            this.search_type = search_type;
            cx.notify();
        }),
    )
}

fn render_toggle_button(
    label: &str,
    active: bool,
    on_click: impl Fn(&mut ExplorerPane, &mut Context<ExplorerPane>) + 'static + Copy,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement {
    div()
        .cursor_pointer()
        .flex()
        .items_center()
        .justify_center()
        .w(px(26.0))
        .h(px(22.0))
        .border_1()
        .border_color(if active {
            rgb(theme::accent())
        } else {
            rgb(theme::border())
        })
        .bg(if active {
            rgb(theme::accent_subtle())
        } else {
            rgb(theme::bg())
        })
        .text_color(if active {
            rgb(theme::accent())
        } else {
            rgb(theme::fg_secondary())
        })
        .rounded(px(6.0))
        .text_xs()
        .font_family("Mono")
        .child(label.to_string())
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                on_click(this, cx);
            }),
        )
}
