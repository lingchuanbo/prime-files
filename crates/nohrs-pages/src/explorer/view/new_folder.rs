use std::path::Path;

use gpui::*;
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogButtonProps;
use gpui_component::input::{InputEvent, InputState};

mod history_menu;
use history_menu::folder_name_input;
use nohrs_core::t;

use crate::explorer::ExplorerPane;

const HISTORY_LIMIT: usize = 20;

#[derive(Default)]
pub(crate) struct FolderNameHistory(Vec<String>);
impl Global for FolderNameHistory {}

impl FolderNameHistory {
    pub(crate) fn all(cx: &App) -> Vec<String> {
        cx.try_global::<Self>()
            .map(|history| history.0.clone())
            .unwrap_or_default()
    }

    pub(crate) fn remember(name: String, cx: &mut App) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        cx.update_global(|history: &mut Self, _| {
            history.0.retain(|kept| !kept.eq_ignore_ascii_case(&name));
            history.0.insert(0, name);
            history.0.truncate(HISTORY_LIMIT);
        });
    }
}

impl ExplorerPane {
    pub(crate) fn new_folder_default_name(&self) -> String {
        let mut selected = self
            .filtered_entries
            .iter()
            .filter(|entry| self.selection.contains(&entry.path));
        let entry = selected
            .clone()
            .find(|entry| self.active_path.as_ref() == Some(&entry.path))
            .or_else(|| selected.next());
        let Some(entry) = entry else {
            return t!("Nova pasta").to_string();
        };
        if entry.kind == "dir" {
            return entry.name.clone();
        }
        Path::new(&entry.name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or(&entry.name)
            .to_string()
    }

    /// Opens the name editor used by the toolbar, context menu, and shortcuts.
    pub(crate) fn open_new_folder_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // `open_dialog` updates the window's shared Root entity. Defer that
        // update until the current Explorer event has finished so toolbar,
        // context-menu, and shortcut handlers cannot update Root reentrantly.
        let weak = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            let _ = weak.update(cx, |pane, cx| pane.open_new_folder_dialog_now(window, cx));
        });
    }

    fn open_new_folder_dialog_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_folder_dialog.is_some() {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        let default_name = self.new_folder_default_name();
        input.update(cx, |state, cx| {
            state.replace_and_mark_text_in_range(
                Some(0..0),
                &default_name,
                Some(0..default_name.encode_utf16().count()),
                window,
                cx,
            );
            state.unmark_text(window, cx);
        });

        let subscription = cx.subscribe_in(
            &input,
            window,
            |pane, _input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let Some(state) = pane.new_folder_dialog.as_ref() else {
                        return;
                    };
                    let name = state.input.read(cx).value().to_string();
                    if pane.create_folder_named(name, cx) {
                        pane.new_folder_dialog = None;
                        window.close_dialog(cx);
                    }
                }
            },
        );
        self.new_folder_dialog = Some(crate::explorer::state::NewFolderDialogState {
            input: input.clone(),
            _subscription: subscription,
        });

        let weak = cx.entity().downgrade();
        let dialog_input = input.clone();
        let focus_input = input.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            let submit_weak = weak.clone();
            let submit_input = dialog_input.clone();
            let close_weak = weak.clone();
            dialog
                .confirm()
                .button_props(
                    DialogButtonProps::default()
                        .cancel_text(t!("Cancelar"))
                        .ok_text(t!("Confirmar")),
                )
                .title(t!("Nova pasta"))
                .on_close(move |_, _window, cx| {
                    close_weak
                        .update(cx, |pane, _cx| pane.new_folder_dialog = None)
                        .ok();
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(t!("Digite o nome da nova pasta."))
                        .child(folder_name_input(&dialog_input, window, cx)),
                )
                .on_ok(move |_, _window, cx| {
                    let name = submit_input.read(cx).value().to_string();
                    submit_weak
                        .update(cx, |pane, cx| {
                            let created = pane.create_folder_named(name, cx);
                            if created {
                                pane.new_folder_dialog = None;
                            }
                            created
                        })
                        .is_ok_and(|created| created)
                })
        });
        // `open_dialog` focuses the modal container. Move focus to the editor
        // immediately so the first key event after opening is delivered to the
        // input instead of being consumed by the dialog's key context.
        input.update(cx, |state, cx| state.focus(window, cx));
        // The dialog owns the focus handle while it is rendered. Focus the
        // actual input again after that frame so keyboard text keeps reaching
        // InputState across platforms with a delayed modal render.
        window.on_next_frame(move |window, cx| {
            focus_input.update(cx, |state, cx| state.focus(window, cx));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{FolderNameHistory, HISTORY_LIMIT};
    use gpui::TestAppContext;

    #[gpui::test]
    fn new_folder_history_is_shared_deduplicated_and_bounded(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(FolderNameHistory::all(cx).is_empty());
            for index in 0..HISTORY_LIMIT + 5 {
                FolderNameHistory::remember(format!("Folder{index}"), cx);
            }
            FolderNameHistory::remember("folder5".into(), cx);
            let history = FolderNameHistory::all(cx);
            assert_eq!(HISTORY_LIMIT, history.len());
            assert_eq!("folder5", history[0]);
            assert_eq!("Folder24", history[1]);
            assert!(
                !history
                    .iter()
                    .any(|name| name == "Folder5" || name == "Folder0")
            );
        });
    }
}
