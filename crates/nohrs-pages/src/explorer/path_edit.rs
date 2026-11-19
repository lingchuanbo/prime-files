//! The path bar as text: a double click on the breadcrumb turns it into a field
//! holding the folder's full path, already selected, as Explorer's address bar
//! does — so Ctrl+C copies it, or a path typed over it is gone to on Enter.

use gpui::{AppContext, Context, Entity, EntityInputHandler, Subscription, Window};
use gpui_component::input::{InputEvent, InputState};
use nohrs_core::tf;
use std::path::Path;

use super::ExplorerPane;
use super::types::StatusLevel;

/// An open path field.
pub(crate) struct PathEditState {
    /// The field showing the path.
    pub input: Entity<InputState>,
    // Drives Enter / Blur while the field is open; dropped with it.
    _subscription: Subscription,
}

impl ExplorerPane {
    /// Replaces the breadcrumb with a field holding the current path, selected.
    pub(crate) fn begin_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_edit.is_some() {
            return;
        }
        let path = self.cwd.clone();
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            // `set_value` leaves the caret at the end with nothing selected;
            // inserting through the IME entry point is the one public way to
            // seed the text with a selection (the rename field does the same).
            // The selection is counted in UTF-16, as the IME counts it.
            let length = path.encode_utf16().count();
            state.replace_and_mark_text_in_range(Some(0..0), &path, Some(0..length), window, cx);
            state.unmark_text(window, cx);
            state.focus(window, cx);
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::PressEnter { .. } => this.commit_path_edit(window, cx),
                    // Clicking away puts the breadcrumb back, as Explorer does,
                    // without going anywhere.
                    InputEvent::Blur => this.cancel_path_edit(window, cx),
                    _ => {}
                }
            },
        );
        self.path_edit = Some(PathEditState {
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Closes the field without navigating.
    pub(crate) fn cancel_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_edit.take().is_some() {
            cx.focus_self(window);
            cx.notify();
        }
    }

    // Goes to the folder typed, or to the folder of a file typed. Anything
    // else leaves the field open so the typo can be fixed.
    pub(crate) fn commit_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = &self.path_edit else {
            return;
        };
        let typed = super::network::normalize_address(&state.input.read(cx).value());
        let path = Path::new(&typed);
        // A network address is gone to without asking the disk here: a
        // server that does not answer would freeze the window for as long,
        // and the listing reads it on a thread of its own (asking for a
        // login if the share wants one).
        let target = if super::network::is_unc(&typed) {
            Some(typed.clone())
        } else if path.is_dir() {
            Some(typed.clone())
        } else if path.is_file() {
            path.parent()
                .map(|parent| parent.to_string_lossy().into_owned())
        } else {
            None
        };
        let Some(target) = target else {
            self.set_status(
                StatusLevel::Error,
                tf!("Pasta não encontrada: {path}", path = typed),
            );
            cx.notify();
            return;
        };
        self.path_edit = None;
        cx.focus_self(window);
        self.change_dir(target, window, cx);
        cx.notify();
    }
}
