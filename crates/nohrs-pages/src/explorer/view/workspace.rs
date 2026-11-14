//! The workspace page: what a workspace (the old tags, renamed) opens to.
//!
//! Where the listing shows rows, a workspace shows its folders as cards — the
//! name, the folder it comes from, how much is inside and when it last
//! changed — with a card at the end to drop more folders on. Counting what a
//! folder holds is a read of its own, so it happens off the UI thread the
//! first time the workspace opens and is kept until it changes.

use nohrs_core::i18n;
use nohrs_core::{t, tf};
use std::time::{Duration, SystemTime};

use gpui::prelude::*;
use gpui::*;
use gpui_component::{Icon, IconName};
use nohrs_ui::theme::theme;

use super::super::ExplorerPane;
use super::super::tags;

/// What a card shows of one folder.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Card {
    pub path: String,
    pub name: String,
    /// The folder it sits in, as the card's second line.
    pub parent: String,
    /// What is directly inside it, and how big that is.
    pub items: usize,
    pub size: u64,
    /// Last change of the folder itself.
    pub changed: Option<SystemTime>,
}

/// Reads a card for each path. Called off the UI thread: one folder read
/// each, and a big folder's read is as slow as the disk makes it.
#[allow(clippy::disallowed_methods)]
pub(crate) fn read_cards(paths: Vec<String>) -> Vec<Card> {
    paths
        .into_iter()
        .map(|path| {
            let metadata = std::fs::metadata(&path).ok();
            let name = std::path::Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            let parent = std::path::Path::new(&path)
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (mut items, mut size) = (0usize, 0u64);
            if let Ok(read) = std::fs::read_dir(&path) {
                for entry in read.flatten() {
                    items += 1;
                    if let Ok(metadata) = entry.metadata()
                        && !metadata.is_dir()
                    {
                        size = size.saturating_add(metadata.len());
                    }
                }
            }
            Card {
                path,
                name,
                parent,
                items,
                size,
                changed: metadata.and_then(|metadata| metadata.modified().ok()),
            }
        })
        .collect()
}

/// How recently `changed` was, in the words the settings page uses.
fn ago(changed: Option<SystemTime>) -> String {
    let Some(changed) = changed else {
        return String::new();
    };
    let elapsed = changed.elapsed().unwrap_or(Duration::ZERO);
    if elapsed < Duration::from_secs(60) {
        t!("agora").to_string()
    } else if elapsed < Duration::from_secs(60 * 60) {
        i18n::plural(elapsed.as_secs().div_ceil(60) as usize, "{n} minuto", "{n} minutos")
    } else if elapsed < Duration::from_secs(24 * 60 * 60) {
        i18n::plural(elapsed.as_secs().div_ceil(3600) as usize, "{n} hora", "{n} horas")
    } else if elapsed < Duration::from_secs(31 * 24 * 60 * 60) {
        i18n::plural(elapsed.as_secs().div_ceil(86_400) as usize, "{n} dia", "{n} dias")
    } else {
        i18n::plural((elapsed.as_secs() / (30 * 86_400)).max(1) as usize, "{n} mês", "{n} meses")
    }
}

/// A card dragged onto another: which folder is being moved in the order.
#[derive(Clone)]
pub(crate) struct CardDrag {
    pub path: String,
}

/// The chip the pointer carries while a card is dragged.
pub(crate) struct CardDragPreview {
    pub name: String,
}

impl Render for CardDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .bg(rgb(theme::toolbar_active_bg()))
            .text_color(rgb(theme::toolbar_active_text()))
            .text_sm()
            .child(self.name.clone())
    }
}

/// Asks Windows for a folder, with the window as the dialog's owner: what the
/// add card does. `None` when the dialog was cancelled.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn pick_folder(owner: i64) -> Option<String> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW, SHGetPathFromIDListW};

    let mut display = [0u16; 260];
    let title: Vec<u16> = format!("{}\0", nohrs_core::t!("Escolher uma pasta")).encode_utf16().collect();
    let browse = BROWSEINFOW {
        hwndOwner: HWND(owner as *mut std::ffi::c_void),
        pszDisplayName: PWSTR(display.as_mut_ptr()),
        lpszTitle: PCWSTR(title.as_ptr()),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        ..Default::default()
    };
    let pidl = unsafe { SHBrowseForFolderW(&browse) };
    if pidl.is_null() {
        return None;
    }
    let mut path = [0u16; 260];
    let read = unsafe { SHGetPathFromIDListW(pidl, &mut path) }.as_bool();
    unsafe { CoTaskMemFree(Some(pidl.cast())) };
    if !read {
        return None;
    }
    let end = path.iter().position(|unit| *unit == 0).unwrap_or(path.len());
    Some(String::from_utf16_lossy(&path[..end]))
}

/// Without Windows there is no folder dialog to ask.
#[cfg(not(windows))]
pub(crate) fn pick_folder(_owner: i64) -> Option<String> {
    None
}

/// The page of workspace `tag`, with `cards` for what it holds.
pub(crate) fn page(
    page: &mut ExplorerPane,
    tag: u32,
    cards: &[Card],
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let icon_px = 32;
    let workspace = tags::Tags::get(tag, cx);
    let name = workspace.as_ref().map(|tag| tag.name.clone()).unwrap_or_default();
    let color = workspace.as_ref().map(|tag| tag.color).unwrap_or(theme::accent());
    let icon = workspace.as_ref().map(|tag| tag.icon()).unwrap_or_default();

    let header = div()
        .flex()
        .items_center()
        .gap_4()
        .child(
            div()
                .size(px(56.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(14.0))
                .bg(rgb(color))
                .child(
                    svg().path(icon.path()).size(px(24.0)).text_color(rgb(0xFFFFFF)),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(div().text_size(px(22.0)).text_color(rgb(theme::fg())).child(name))
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(theme::fg_secondary()))
                        .child(i18n::plural(cards.len(), "{n} pasta", "{n} pastas")),
                ),
        );

    fn tile(card: &Card, icon: AnyElement, tag: u32, color: u32, cx: &mut Context<ExplorerPane>) -> AnyElement {
        let path = card.path.clone();
        let menu = card.path.clone();
        // Dragging the card onto another moves it in the workspace's order;
        // the folder itself stays where it is.
        let dragged = card.path.clone();
        let target = card.path.clone();
        let preview = card.name.clone();
        let details = tf!(
            "{items} · {size}",
            items = i18n::plural(card.items, "{n} item", "{n} itens"),
            size = crate::explorer::preview_listing::human_size(card.size)
        );
        let changed = ago(card.changed);
        div()
            .id(SharedString::from(format!("workspace-card-{}", card.path)))
            .w(px(232.0))
            .flex()
            .flex_col()
            .gap_2()
            .p(px(12.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(rgb(color))
            .bg(rgb(theme::bg_secondary()))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::bg_hover())))
            .on_drag(
                CardDrag { path: dragged },
                move |_drag, _offset, _window, cx| {
                    let name = preview.clone();
                    cx.new(|_cx| CardDragPreview { name })
                },
            )
            .on_drop(cx.listener(move |this, drag: &CardDrag, _window, cx| {
                if drag.path != target {
                    tags::Tags::reorder(tag, &drag.path, Some(&target), cx);
                    this.refresh_workspace(cx);
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| this.change_dir(path.clone(), window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.open_workspace_card_menu(tag, menu.clone(), true, window, cx);
                }),
            )
            .child(div().h(px(3.0)).w_full().rounded_full().bg(rgb(color)))
            .child(div().flex().items_center().justify_between().child(icon).child(
                Icon::new(IconName::Ellipsis)
                    .size_4()
                    .text_color(rgb(theme::muted())),
            ))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::fg()))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(card.name.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(rgb(theme::muted()))
                    .child(Icon::new(IconName::Folder).size_3())
                    .child(
                        div()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(card.parent.clone()),
                    ),
            )
            .child(div().text_xs().text_color(rgb(theme::fg_secondary())).child(details))
            .when(!changed.is_empty(), |this| {
                this.child(
                    div()
                        .rounded(px(20.0))
                        .px(px(8.0))
                        .py(px(1.0))
                        .bg(rgb(theme::bg_hover()))
                        .text_xs()
                        .text_color(rgb(theme::muted()))
                        .child(tf!("Alterado {ago}", ago = changed)),
                )
            })
        .into_any_element()
    }

    let add = div()
        .id("workspace-add")
        .w(px(232.0))
        .min_h(px(150.0))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .p(px(12.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(rgb(theme::border()))
        .border_dashed()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .child(
            div()
                .size(px(34.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(rgb(theme::bg_hover()))
                .child(Icon::new(IconName::Plus).size_5().text_color(rgb(theme::muted()))),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme::fg()))
                .child(t!("Adicionar pastas ou arquivos")),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(t!("Arraste pastas para cá")),
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            let owner = crate::ipc::window_hwnd(window).unwrap_or(0);
            if let Some(folder) = pick_folder(owner) {
                tags::Tags::add(&[folder], tag, cx);
                this.refresh_workspace(cx);
                this.reload(cx);
            }
        }))
        .into_any_element();

    // The icons first: each is one lookup into the sidebar's cache, which
    // borrows the pane between tiles.
    let icons: Vec<(AnyElement, u32)> = cards
        .iter()
        .map(|card| {
            let color = tags::Tags::card_color(tag, &card.path, cx).unwrap_or(color);
            let icon = match page.sidebar.icons.icon_in(&card.path, true, icon_px, |pane| &mut pane.sidebar.icons, cx) {
                Some(image) => img(ImageSource::Render(image)).size(px(28.0)).into_any_element(),
                None => Icon::new(IconName::Folder).size_6().text_color(rgb(color)).into_any_element(),
            };
            (icon, color)
        })
        .collect();

    let grid = div()
        .flex()
        .flex_wrap()
        .gap_4()
        .children(cards.iter().zip(icons).map(|(card, (icon, color))| tile(card, icon, tag, color, cx)))
        .child(add);

    // The files the workspace holds: a compact row each, the folder cards
    // above them being the page's subject.
    let file_rows: Vec<AnyElement> = page
        .entries
        .iter()
        .filter(|entry| entry.kind != "dir")
        .map(|entry| {
            let icon = match page.sidebar.icons.icon_in(&entry.path, false, icon_px, |pane| &mut pane.sidebar.icons, cx) {
                Some(image) => img(ImageSource::Render(image)).size(px(16.0)).into_any_element(),
                None => Icon::new(IconName::File).size_4().text_color(rgb(theme::muted())).into_any_element(),
            };
            let item = entry.clone();
            let menu = entry.path.clone();
            div()
                .id(SharedString::from(format!("workspace-file-{}", entry.path)))
                .h(px(30.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap_2()
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .on_click(cx.listener(move |this, _, window, cx| this.activate_entry(item.clone(), window, cx)))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_workspace_card_menu(tag, menu.clone(), false, window, cx);
                    }),
                )
                .child(icon)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_sm()
                        .text_color(rgb(theme::fg()))
                        .child(entry.name.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::muted()))
                        .child(crate::explorer::preview_listing::human_size(entry.size)),
                )
                .into_any_element()
        })
        .collect();

    div()
        .id("workspace-page")
        .size_full()
        .p(px(20.0))
        .flex()
        .flex_col()
        .gap_6()
        .overflow_y_scroll()
        .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(rgb(theme::accent_subtle())))
        .on_drop(cx.listener(move |this, paths: &ExternalPaths, _window, cx| {
            // Dropping folders on the page puts them in the workspace: the
            // page is the workspace, not a folder that could take files.
            let folders: Vec<String> = paths
                .paths()
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .filter(|path| std::path::Path::new(path).is_dir())
                .collect();
            if !folders.is_empty() {
                tags::Tags::add(&folders, tag, cx);
                this.refresh_workspace(cx);
                // The listing behind the cards reads the workspace again, so
                // the new folder is among them.
                this.reload(cx);
            }
        }))
        .child(header)
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(i18n::plural(cards.len(), "{n} pasta", "{n} pastas")),
        )
        .child(grid)
        .when(!file_rows.is_empty(), |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::muted()))
                    .child(i18n::plural(file_rows.len(), "{n} arquivo", "{n} arquivos")),
            )
            .child(div().flex().flex_col().gap(px(1.0)).children(file_rows))
        })
        .into_any_element()
}
