use gpui::*;
use gpui_component::button::Button;
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::popover::Popover;
use gpui_component::{Disableable as _, IconName};
use nohrs_core::t;

use super::FolderNameHistory;

#[derive(Default)]
struct HistoryMenu {
    open: bool,
    menu: Option<Entity<PopupMenu>>,
}

pub(super) fn folder_name_input(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement + use<> {
    let history = FolderNameHistory::all(cx);
    let state = window.use_keyed_state(("folder-history", input.entity_id()), cx, |_, _| {
        HistoryMenu::default()
    });
    let popover = history_popover(input, &state, history, cx);
    let input = input.clone();
    div()
        .flex()
        .items_center()
        .gap_1()
        .w_full()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Input::new(&input).cleanable(true)),
        )
        .child(popover)
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key != "down"
                || event.keystroke.modifiers != Modifiers::default()
                || !input.focus_handle(cx).is_focused(window)
                || FolderNameHistory::all(cx).is_empty()
                || input
                    .update(cx, |input, cx| input.marked_text_range(window, cx))
                    .is_some()
            {
                return;
            }
            state.update(cx, |state, cx| {
                state.open = true;
                cx.notify();
            });
            cx.stop_propagation();
        })
}

fn history_popover(
    input: &Entity<InputState>,
    state: &Entity<HistoryMenu>,
    history: Vec<String>,
    cx: &App,
) -> Popover {
    let open = state.read(cx).open;
    let input = input.clone();
    let content_state = state.clone();
    let change_state = state.clone();
    Popover::new("new-folder-history-popover")
        .appearance(false)
        .overlay_closable(false)
        .open(open)
        .trigger(
            Button::new("new-folder-history")
                .icon(IconName::ChevronDown)
                .tooltip(t!("Histórico"))
                .disabled(history.is_empty()),
        )
        .on_open_change(move |open, _, cx| {
            change_state.update(cx, |state, cx| {
                state.open = *open;
                if !open {
                    state.menu = None;
                }
                cx.notify();
            });
        })
        .content(move |_, window, cx| {
            if let Some(menu) = content_state.read(cx).menu.clone() {
                return menu;
            }
            let menu = build_menu(&input, &history, window, cx);
            content_state.update(cx, |state, _| state.menu = Some(menu.clone()));
            menu.focus_handle(cx).focus(window);
            let state = content_state.clone();
            let input = input.clone();
            window
                .subscribe(&menu, cx, move |_, _: &DismissEvent, window, cx| {
                    state.update(cx, |state, cx| {
                        state.open = false;
                        state.menu = None;
                        cx.notify();
                    });
                    input.update(cx, |input, cx| input.focus(window, cx));
                })
                .detach();
            menu
        })
}

fn build_menu(
    input: &Entity<InputState>,
    history: &[String],
    window: &mut Window,
    cx: &mut App,
) -> Entity<PopupMenu> {
    PopupMenu::build(window, cx, |mut menu, _, _| {
        for name in history {
            let input = input.clone();
            let name = name.clone();
            menu = menu.item(
                PopupMenuItem::new(name.clone()).on_click(move |_, window, cx| {
                    let input = input.clone();
                    let name = name.clone();
                    // 等菜单关闭后再回填并选中文字，避免关闭动作覆盖焦点。
                    window.defer(cx, move |window, cx| {
                        input.update(cx, |state, cx| {
                            // 先清空，避免组件将旧文本长度累加到新选区。
                            state.set_value("", window, cx);
                            state.replace_and_mark_text_in_range(
                                Some(0..0),
                                &name,
                                Some(0..name.encode_utf16().count()),
                                window,
                                cx,
                            );
                            state.unmark_text(window, cx);
                            state.focus(window, cx);
                        });
                    });
                }),
            );
        }
        menu
    })
}
