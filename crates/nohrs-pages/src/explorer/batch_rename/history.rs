use super::session::SessionStore;
use gpui::*;
use gpui_component::{
    Disableable, IconName, Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    menu::{PopupMenu, PopupMenuItem},
    popover::Popover,
};

#[derive(Default)]
struct HistoryMenu {
    open: bool,
    menu: Option<Entity<PopupMenu>>,
}

pub(super) fn input(
    field: (&Entity<InputState>, usize, bool),
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement + use<> {
    let (input, index, busy) = field;
    let data = &cx.global::<SessionStore>().data;
    let history = if index < 7 {
        data.history[index].clone()
    } else {
        data.scope.history[index - 7].clone()
    };
    let empty = history.is_empty();
    let state = window.use_keyed_state(("rename-history", input.entity_id()), cx, |_, _| {
        HistoryMenu::default()
    });
    let popover = history_popover((input, busy), &state, history, cx);
    let input = input.clone();
    div()
        .on_key_down(move |event, window, cx| {
            if busy
                || empty
                || event.keystroke.key != "down"
                || event.keystroke.modifiers != Modifiers::default()
                || !input.focus_handle(cx).is_focused(window)
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
        .child(Input::new(field.0).disabled(busy).suffix(popover))
}

fn history_popover(
    field: (&Entity<InputState>, bool),
    state: &Entity<HistoryMenu>,
    history: Vec<String>,
    cx: &App,
) -> Popover {
    let (input, busy) = field;
    let open = state.read(cx).open;
    let input = input.clone();
    let content_state = state.clone();
    let change_state = state.clone();
    Popover::new("rename-history-popover")
        .appearance(false)
        .overlay_closable(false)
        .open(open)
        .trigger(
            Button::new("rename-history-arrow")
                .xsmall()
                .ghost()
                .icon(IconName::ChevronDown)
                .tooltip(if history.is_empty() {
                    "暂无历史记录"
                } else {
                    "历史记录（↓）"
                })
                .disabled(busy || history.is_empty()),
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
        .content(move |_, window, cx| menu_content((&input, &history), &content_state, window, cx))
}

fn menu_content(
    field: (&Entity<InputState>, &[String]),
    state: &Entity<HistoryMenu>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PopupMenu> {
    if let Some(menu) = state.read(cx).menu.clone() {
        return menu;
    }
    let menu = build_menu(field, window, cx);
    state.update(cx, |state, _| state.menu = Some(menu.clone()));
    menu.focus_handle(cx).focus(window);
    let state = state.clone();
    let input = field.0.clone();
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
}

fn build_menu(
    field: (&Entity<InputState>, &[String]),
    window: &mut Window,
    cx: &mut App,
) -> Entity<PopupMenu> {
    PopupMenu::build(window, cx, |mut menu, _, _| {
        for value in field.1 {
            let input = field.0.clone();
            let value = value.clone();
            menu = menu.item(
                PopupMenuItem::new(value.clone()).on_click(move |_, window, cx| {
                    let input = input.clone();
                    let value = value.clone();
                    window.defer(cx, move |window, cx| {
                        select_value(&input, &value, (window, cx))
                    });
                }),
            );
        }
        menu
    })
}

fn select_value(input: &Entity<InputState>, value: &str, context: (&mut Window, &mut App)) {
    let (window, cx) = context;
    input.update(cx, |input, cx| {
        // 先清空再回填，避免组件将旧文本长度累加到新选区。
        input.set_value("", window, cx);
        input.replace_and_mark_text_in_range(
            Some(0..0),
            value,
            Some(0..value.encode_utf16().count()),
            window,
            cx,
        );
        input.unmark_text(window, cx);
        input.focus(window, cx);
    });
}
