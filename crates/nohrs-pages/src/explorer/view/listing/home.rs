use crate::explorer::ExplorerPane;
use gpui::prelude::*;
use gpui::*;
use gpui_component::{Icon, IconName};
use nohrs_ui::theme::theme;

pub(super) fn render(page: &mut ExplorerPane, cx: &mut Context<ExplorerPane>) -> Option<AnyElement> {
    let cards = drive_cards(page, cx)?;
    let title = page.location_name(cx).unwrap_or_else(|| nohrs_core::t!("Este Computador").to_string());
    let recent = crate::explorer::recent::RecentFolders::all(cx);
    Some(div().id("computer-overview").size_full().overflow_y_scroll()
        .p(px(28.0)).flex().flex_col().gap(px(20.0))
        .child(div().text_size(px(26.0)).font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(theme::fg())).child(title))
        .child(cards)
        .when(!recent.is_empty(), |view| view.child(
            div().flex().flex_col().gap(px(12.0))
                .child(div().text_sm().font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::fg_secondary())).child(nohrs_core::t!("Pastas recentes")))
                .child(div().flex().flex_wrap().gap(px(10.0)).children(
                    recent.into_iter().take(8).enumerate().map(|(index, path)| recent_card(index, path, cx)),
                )),
        )).into_any_element())
}

fn recent_card(index: usize, path: String, cx: &mut Context<ExplorerPane>) -> impl IntoElement + use<> {
    let name = std::path::Path::new(&path).file_name()
        .map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
    let opened = path.clone();
    div().id(("home-recent", index)).w(px(270.0)).p(px(16.0))
        .flex().items_center().gap(px(12.0)).rounded(px(10.0))
        .border_1().border_color(rgb(theme::border())).bg(rgb(theme::bg_secondary()))
        .cursor_pointer().hover(|style| style.bg(rgb(theme::bg_hover())).border_color(rgb(theme::accent())))
        .on_click(cx.listener(move |pane, _, window, cx| pane.change_dir(opened.clone(), window, cx)))
        .child(Icon::new(IconName::Folder).size_5().text_color(rgb(theme::accent())))
        .child(div().flex_1().min_w(px(0.0)).flex().flex_col().gap_1()
            .child(div().text_sm().text_color(rgb(theme::fg())).truncate().child(name))
            .child(div().text_xs().text_color(rgb(theme::muted())).truncate().child(path)))
}

/// The drives of This PC as cards: icon, name, a bar of the used space and
/// "free of total". `None` until the listing holds a drive.
fn drive_cards(page: &mut ExplorerPane, cx: &mut Context<ExplorerPane>) -> Option<AnyElement> {
    use nohrs_ui::theme::theme;
    let drives: Vec<_> = page
        .entries
        .iter()
        .filter(|entry| crate::explorer::drives::drive_root(&entry.path).is_some())
        .map(|entry| (entry.name.clone(), entry.path.clone()))
        .collect();
    if drives.is_empty() {
        return None;
    }
    let cards = drives.into_iter().enumerate().map(|(ix, (name, path))| {
        let space = crate::explorer::drives::space(&path, cx);
        let icon = page.thumbnails.icon(&path, true, 32, cx);
        let opened = path.clone();
        let menu_path = path.clone();
        let selected = page.selection.contains(&path);
        div()
            .id(("drive-card", ix))
            .w(px(270.0))
            .p(px(18.0))
            .flex()
            .gap(px(10.0))
            .items_center()
            .rounded(px(12.0))
            .border_1()
            .border_color(rgb(theme::border()))
            .bg(rgb(if selected { theme::accent_subtle() } else { theme::bg_secondary() }))
            .cursor_pointer()
            .hover(|this| this.bg(rgb(theme::bg_hover())).border_color(rgb(theme::accent())))
            .on_click(cx.listener(move |pane, _, window, cx| pane.change_dir(opened.clone(), window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |pane, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if let Some(ix) = pane.filtered_entries.iter().position(|entry| entry.path == menu_path) {
                        pane.select_single(ix);
                        pane.open_context_menu(Some(ix), event.modifiers.shift, window, cx);
                    }
                }),
            )
            .child(match icon {
                Some(image) => img(ImageSource::Render(image)).size(px(32.0)).flex_shrink_0().into_any_element(),
                None => gpui_component::Icon::new(gpui_component::Icon::empty()).path("icons/database.svg")
                    .size(px(28.0))
                    .text_color(rgb(theme::gray_500()))
                    .into_any_element(),
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(theme::fg()))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(name),
                    )
                    .child(super::usage_bar(space, 5.0))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::fg_secondary()))
                            .whitespace_nowrap()
                            .child(space.map(|space| space.describe()).unwrap_or_default()),
                    ),
            )
    });
    Some(
        div()
            .flex_shrink_0()
            .w_full()
            .px(px(0.0))
            .pt(px(10.0))
            .pb(px(6.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::fg_secondary()))
                    .child(nohrs_core::t!("Unidades e armazenamento")),
            )
            .child(div().flex().flex_wrap().gap(px(8.0)).children(cards))
            .into_any_element(),
    )
}

