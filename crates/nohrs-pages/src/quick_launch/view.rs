//! The quick-launch window's view tree (§11, §12.1, and the M1 rows of §12.2).
//!
//! M1 draws a group bar and either a grid or a list, moves the selection with
//! the arrow keys, launches on a click or `Enter`, switches groups with the bar
//! or `Alt+1..9`, and hides on `Esc`. The title bar is this crate's own — the
//! window has no OS chrome — and carries the drag area and the "⋯" menu that
//! §15.0 requires: with the process resident and the tray icon off, that menu is
//! the only way back to the main window and the only way out.
//!
//! Everything the state owns is read through [`super`]; nothing here keeps a
//! second copy of the configuration (§13).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClickEvent, ClipboardItem, Context, Div, Entity,
    FocusHandle, ImageSource, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, Pixels,
    Point, Render, RenderImage, SharedString, Subscription, Task, UniformListScrollHandle,
    WeakEntity, Window, WindowControlArea, div, ease_out_quint, img, px, rgb, uniform_list,
};
use gpui_component::{
    Icon, IconName, WindowExt as _,
    button::{Button, ButtonRounded, ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
};
use nohrs_core::config::quick_launch::{ActionKind, Item, MAX_SLOT, View};
use nohrs_core::telemetry::LogErr;
use nohrs_ui::theme::theme;

use super::AUTO_HIDE_DELAY;

/// Height of the title bar this crate draws.
const TITLE_BAR_HEIGHT: f32 = 36.0;

/// Width of the group bar on the left (§12.1).
const GROUP_BAR_WIDTH: f32 = 120.0;

/// Height of one name line under a tile.
const NAME_LINE_HEIGHT: f32 = 18.0;

/// Height of one row of the list view.
const LIST_ROW_HEIGHT: f32 = 44.0;

/// Height of the footer that carries a failure (§8.5).
const FOOTER_HEIGHT: f32 = 28.0;

/// The ids of the rows of the drawn menus; `MenuHost::choose` reads them.
mod command {
    pub(super) const NEW_GROUP: u32 = 1;
    pub(super) const RENAME_GROUP: u32 = 2;
    pub(super) const DELETE_GROUP: u32 = 3;
    pub(super) const NEW_ITEM: u32 = 4;
    pub(super) const NEW_ITEM_FOLDER: u32 = 5;
    pub(super) const RUN_AS_ADMIN: u32 = 6;
    pub(super) const REVEAL: u32 = 7;
    pub(super) const EDIT: u32 = 8;
    pub(super) const COPY_PATH: u32 = 9;
    pub(super) const DELETE_ITEM: u32 = 10;
    pub(super) const SHOW_WINDOW: u32 = 11;
    pub(super) const QUIT: u32 = 12;
    /// The rows of the "Move to" submenu encode their target as `MOVE_TO + n`.
    pub(super) const MOVE_TO: u32 = 100;
}

use command::*;

/// What a menu was opened for, so its plain ids can be read as actions (§12.2).
#[derive(Clone, Copy)]
enum MenuScope {
    Group(usize),
    Item { group: usize, slot: u32 },
    Window,
}

/// The quick-launch window's root view.
pub(crate) struct QuickLaunchView {
    focus: FocusHandle,
    /// The pending "the window lost focus" hide (§11). Dropping it cancels it,
    /// which is what a quick return of the focus needs.
    hide_task: Option<Task<()>>,
    scroll_handle: UniformListScrollHandle,
    /// What the last frame drew, kept until the state's revision says it
    /// changed (S6). Rebuilding it clones the group's items, so it is not done
    /// per frame.
    frame: Option<Frame>,
    /// The item the pointer is over, if any: its tile is drawn a little bigger
    /// (§12.1).
    hovered: Option<u32>,
    /// Every item a pointer has been over. A tile that is in here animates back
    /// down when the pointer leaves it; one that is not is drawn at rest, so it
    /// does not shrink into place when the window opens.
    ever_hovered: HashSet<u32>,
    /// The item editor open over the window, if any (§12.3).
    editor: Option<Editor>,
    /// The group whose name is being edited in place, if any (§12.3).
    renaming: Option<Rename>,
    /// The menu a right click (or the title bar's "⋯") opened, if any.
    menu: Option<crate::drawn_menu::Menu>,
    /// What the open menu was opened for, so its ids can be read as actions.
    menu_scope: Option<MenuScope>,
    _activation: Subscription,
}

impl crate::drawn_menu::MenuHost for QuickLaunchView {
    fn menu(&self) -> Option<&crate::drawn_menu::Menu> {
        self.menu.as_ref()
    }

    fn set_menu(&mut self, menu: Option<crate::drawn_menu::Menu>) {
        if menu.is_none() {
            self.menu_scope = None;
        }
        self.menu = menu;
    }

    fn choose(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let scope = self.menu_scope.take();
        match (scope, id) {
            (Some(MenuScope::Group(index)), RENAME_GROUP) => {
                self.start_rename(index, window, cx);
            }
            (Some(MenuScope::Group(index)), DELETE_GROUP) => {
                self.delete_group(index, window, cx);
            }
            (Some(MenuScope::Item { group, slot }), NEW_ITEM) => {
                self.new_item_at(group, slot, super::PathWant::Either, window, cx);
            }
            (Some(MenuScope::Item { group, slot }), NEW_ITEM_FOLDER) => {
                self.new_item_at(group, slot, super::PathWant::Folders, window, cx);
            }
            (Some(MenuScope::Item { slot, .. }), RUN_AS_ADMIN) => {
                self.clicked(slot, super::ClickKind::Admin, cx);
            }
            (Some(MenuScope::Item { slot, .. }), REVEAL) => {
                super::reveal_item(cx, slot);
                super::hide(cx);
            }
            (Some(MenuScope::Item { group, slot }), EDIT) => {
                self.edit_item(group, slot, window, cx);
            }
            (Some(MenuScope::Item { slot, .. }), COPY_PATH) => {
                if let Some(text) = super::copy_text(cx, slot) {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            (Some(MenuScope::Item { group, slot }), MOVE_TO..) => {
                if let Ok(target) = usize::try_from(id - MOVE_TO) {
                    self.drop_item_on_group(&ItemDrag { group, slot }, target, cx);
                }
            }
            (Some(MenuScope::Item { group, slot }), DELETE_ITEM) => {
                self.delete_item(group, slot, cx);
            }
            (_, NEW_GROUP) => self.add_group(window, cx),
            (_, SHOW_WINDOW) => crate::tray::request_show(),
            (_, QUIT) => crate::tray::request_quit(),
            _ => {}
        }
        cx.notify();
    }
}

/// The four fields the edit dialog offers (§12.3).
struct Editor {
    group: usize,
    slot: u32,
    name: Entity<InputState>,
    path: Entity<InputState>,
    args: Entity<InputState>,
    /// The folder the program starts in; empty keeps the executable's own.
    cwd: Entity<InputState>,
    /// A custom icon path, `.ico`/`.png`; empty uses the program's own.
    icon: Entity<InputState>,
    /// "Run as administrator", toggled by the checkbox and read on confirm.
    admin: Rc<Cell<bool>>,
    /// A `group` item only offers its name here; everything else is edited in
    /// the settings page (§17.2 E.4).
    group_item: bool,
}

/// The in-place name editor of one group (§12.3).
struct Rename {
    group: usize,
    input: Entity<InputState>,
    _subscription: Subscription,
}

/// One item being dragged inside the window (§12.2).
#[derive(Clone)]
struct ItemDrag {
    group: usize,
    slot: u32,
}

/// What the pointer carries while an item is dragged.
struct ItemDragPreview {
    name: SharedString,
}

impl Render for ItemDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .bg(rgb(theme::bg_hover()))
            .border_1()
            .border_color(rgb(theme::border()))
            .text_xs()
            .child(self.name.clone())
    }
}

/// Everything a frame draws that does not change from frame to frame.
struct Frame {
    revision: u64,
    icon_edge: u32,
    groups: Rc<Vec<(String, bool)>>,
    drawn: Rc<Vec<Drawn>>,
    view: View,
    cell_size: f32,
    /// Draw the cells' blocks, empty ones included.
    show_cells: bool,
    /// True while a filter is typed: the grid packs the matches instead of
    /// leaving the holes their slots would leave (§12.2).
    packed: bool,
}

impl QuickLaunchView {
    /// Builds the view and starts watching the window's activation.
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let subscription = cx.observe_window_activation(window, |this, window, cx| {
            this.activation_changed(window, cx);
        });
        // A window being built is not focus-enabled yet, so a `focus()` call
        // from the constructor is silently dropped and no key ever reaches the
        // view. The first frame is the first moment it can take the keyboard —
        // and the first moment it can ask for the foreground, which a background
        // process only gets through the retries in `foreground`.
        cx.on_next_frame(window, |this, window, _cx| {
            window.focus(&this.focus);
            window.activate_window();
            crate::foreground::bring_to_front(window);
        });
        Self {
            focus,
            hide_task: None,
            scroll_handle: UniformListScrollHandle::new(),
            frame: None,
            hovered: None,
            ever_hovered: HashSet::new(),
            editor: None,
            renaming: None,
            menu: None,
            menu_scope: None,
            _activation: subscription,
        }
    }

    /// Schedules the hide that follows losing the focus (§11).
    fn activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A menu does not survive the window losing the focus: its rows act on
        // the state behind it, which the user is no longer looking at.
        crate::drawn_menu::window_deactivated(self, window, cx);

        let Some(quick_launch) = super::state(cx) else {
            return;
        };
        let (auto_hide, topmost, visible) = (
            quick_launch.config().window.auto_hide,
            quick_launch.config().window.topmost,
            quick_launch.visible,
        );
        if !auto_hide || topmost || !visible {
            return;
        }
        if window.is_window_active() {
            // Coming back inside the delay cancels the hide by dropping it, and
            // the keyboard belongs to this window again.
            self.hide_task = None;
            window.focus(&self.focus);
            return;
        }
        self.hide_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(AUTO_HIDE_DELAY).await;
            this.update_in(cx, |this, window, cx| {
                if !window.is_window_active() && super::state(cx).is_some_and(|state| state.visible)
                {
                    this.hide_task = None;
                    super::hide_in(cx, window);
                }
            })
            .ok();
        }));
    }

    /// The keys the window answers (§12.2, M1 rows).
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.to_lowercase();
        let modifiers = event.keystroke.modifiers;
        // The drawn menu takes the keyboard while it is open: Esc closes it and
        // nothing else behind it runs (§12.2).
        if crate::drawn_menu::is_open(self) {
            if key == "escape" {
                crate::drawn_menu::close(self, cx);
            }
            cx.stop_propagation();
            return;
        }
        // A command waiting for its first-run confirmation owns the window: a
        // key must not start what is behind the dialog, and Esc dismisses it
        // rather than hiding the window (§17.2 D.3).
        if super::command_waiting(cx) {
            if key == "escape" {
                super::cancel_pending_command(cx);
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        let (view, cell_size) = super::state(cx)
            .map(|quick_launch| {
                let window = &quick_launch.config().window;
                (window.view, f32::from(window.cell_size))
            })
            .unwrap_or((View::Grid, 100.0));
        let columns = if view == View::List {
            fitted_list_columns(window)
        } else {
            let width = f32::from(window.viewport_size().width) - GROUP_BAR_WIDTH;
            fit_count(width, cell_size)
        };
        let columns = i64::try_from(columns).unwrap_or(8);
        if modifiers.alt
            && !modifiers.control
            && !modifiers.platform
            && let Some(index) = group_shortcut(&key)
        {
            super::select_group(cx, index);
            cx.stop_propagation();
            return;
        }
        // Typing filters the current group (§12.2). Escape then clears the
        // filter before it hides.
        if let Some(text) = event.keystroke.key_char.as_deref()
            && !modifiers.control
            && !modifiers.platform
            && !modifiers.alt
            && text.chars().count() == 1
            && !text.chars().next().is_some_and(char::is_control)
        {
            let mut filter = super::filter(cx).to_string();
            filter.push_str(text);
            super::set_filter(cx, filter);
            super::ensure_selection(cx);
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if key == "backspace" && !super::filter(cx).is_empty() {
            let mut filter = super::filter(cx).to_string();
            filter.pop();
            super::set_filter(cx, filter);
            super::ensure_selection(cx);
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let handled = match key.as_str() {
            // Escape clears the filter first and only then hides (§12.2). The
            // window is already in hand, so the hide goes through it: asking GPUI
            // for the window it is currently updating fails with "window not
            // found".
            "escape" => {
                if !super::filter(cx).is_empty() {
                    super::set_filter(cx, String::new());
                    super::drop_stale_selection(cx);
                    cx.notify();
                } else {
                    super::hide_in(cx, window);
                }
                true
            }
            "left" => {
                super::move_selection(cx, -1);
                true
            }
            "right" => {
                super::move_selection(cx, 1);
                true
            }
            "up" => {
                super::move_selection(cx, -columns);
                true
            }
            "down" => {
                super::move_selection(cx, columns);
                true
            }
            "enter" => {
                if let Some(slot) = super::state(cx).and_then(|quick_launch| quick_launch.slot) {
                    let kind = if modifiers.control {
                        super::ClickKind::Admin
                    } else {
                        super::ClickKind::Plain
                    };
                    super::activate_with(cx, slot, kind);
                }
                true
            }
            _ => false,
        };
        if handled {
            // The window has no text field in M1, but stopping here is what
            // keeps a future one from also acting on a key the grid consumed.
            cx.stop_propagation();
        }
    }

    /// Opens the editor for one item (§12.3).
    ///
    /// Deferred: the menu that calls this runs inside the window's update, and
    /// the dialog puts an input field in that same window (the M1 abort came
    /// from exactly that shape).
    fn edit_item(&mut self, group: usize, slot: u32, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            weak.update(cx, |view, cx| view.edit_item_now(group, slot, window, cx))
                .ok();
        });
    }

    fn edit_item_now(
        &mut self,
        group: usize,
        slot: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = super::item_at(cx, group, slot) else {
            return;
        };
        let name = input_with(window, cx, &item.name);
        // The placeholders are example values: the launcher expands `%VAR%`
        // and `{luma}`/`{config}`/`{portable}` in the path, the working folder
        // and the icon (§7), so a path written with them is not a surprise.
        let path = input_with_placeholder(
            window,
            cx,
            item.path.as_deref().unwrap_or_default(),
            "%ProgramFiles%\\App.exe",
        );
        let args = input_with(window, cx, item.args.as_deref().unwrap_or_default());
        let cwd = input_with_placeholder(
            window,
            cx,
            item.cwd.as_deref().unwrap_or_default(),
            "%USERPROFILE%",
        );
        let icon = input_with_placeholder(
            window,
            cx,
            item.icon.as_deref().unwrap_or_default(),
            "%ProgramFiles%\\App\\app.ico",
        );
        let group_item = item.action_kind() == ActionKind::Group;
        self.editor = Some(Editor {
            group,
            slot,
            name: name.clone(),
            path,
            args,
            cwd,
            icon,
            admin: Rc::new(Cell::new(item.admin)),
            group_item,
        });
        name.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Saves the four fields through the state layer (§12.3).
    ///
    /// A `group` item only writes its name: its path, arguments and working
    /// directory are not part of it, and its entries are chosen in the settings
    /// page (§17.2 E.4).
    fn confirm_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let name = editor.name.read(cx).value().to_string();
        if editor.group_item {
            super::rename_item(cx, editor.group, editor.slot, &name);
            cx.notify();
            return;
        }
        let path = editor.path.read(cx).value().to_string();
        let args = editor.args.read(cx).value().to_string();
        let cwd = editor.cwd.read(cx).value().to_string();
        let icon = editor.icon.read(cx).value().to_string();
        let admin = editor.admin.get();
        super::edit_item_fields(
            cx,
            editor.group,
            editor.slot,
            &name,
            &path,
            &args,
            Some(&cwd),
            Some(&icon),
            admin,
        );
        cx.notify();
    }

    /// Adds what the file picker returned to the slot the menu was opened on
    /// (§12.3, §12.2): the first path fills that slot, the rest follow.
    ///
    /// `want` is what the dialog may pick: the menu offers files and folders
    /// together where the platform allows it, and files alone (plus a folder-only
    /// entry) where it does not (§17.2 R2).
    fn new_item_at(
        &mut self,
        group: usize,
        slot: u32,
        want: super::PathWant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !super::group_editable(cx, group) {
            return;
        }
        let prompt = match want {
            super::PathWant::Folders => nohrs_core::t!("Escolher uma pasta"),
            super::PathWant::Files | super::PathWant::Either => {
                nohrs_core::t!("Escolher um arquivo ou uma pasta")
            }
        };
        let receiver = cx.prompt_for_paths(super::path_prompt_for(cx, want, true, prompt));
        cx.spawn_in(window, async move |this, cx| {
            let picked = match receiver.await {
                Ok(Ok(Some(paths))) => paths,
                _ => return,
            };
            // S1: the kind of each picked path is decided on a background task;
            // the items are placed once the answers are in (and B1 keeps them off
            // the slots that are taken).
            let items = cx
                .background_spawn(async move {
                    picked
                        .iter()
                        .map(|path| {
                            let is_dir = super::path_is_directory(path);
                            nohrs_core::config::quick_launch::item_for_path(path, 0, is_dir)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update_in(cx, |_view, _window, cx| {
                super::place_items(cx, group, slot, items);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// Adds a group and starts the inline rename the specification asks for
    /// (§12.3).
    fn add_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            let Some(index) = super::add_group(cx) else {
                return;
            };
            weak.update(cx, |view, cx| {
                view.start_rename(index, window, cx);
            })
            .ok();
        });
    }

    /// Puts one input field in the group bar and gives it the keyboard (§12.3).
    fn start_rename(&mut self, group: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = super::state(cx)
            .and_then(|state| state.config().groups.get(group))
            .map(|group| group.name.clone())
        else {
            return;
        };
        let input = input_with(window, cx, &name);
        let subscription = cx.subscribe_in(
            &input,
            window,
            |view, _input, event: &InputEvent, _window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    view.commit_rename(cx);
                    cx.notify();
                }
            },
        );
        let focus = input.clone();
        self.renaming = Some(Rename {
            group,
            input,
            _subscription: subscription,
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |state, cx| state.focus(window, cx));
        });
        cx.notify();
    }

    /// An empty name cancels the rename (§12.3).
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(rename) = self.renaming.take() else {
            return;
        };
        let name = rename.input.read(cx).value().to_string();
        super::rename_group(cx, rename.group, &name);
    }

    /// Removes a group; a non-empty one asks first (§12.3). The last group
    /// cannot be removed, and the menu item is greyed out for it.
    fn delete_group(&mut self, group: usize, window: &mut Window, cx: &mut Context<Self>) {
        let empty = super::state(cx)
            .and_then(|state| state.config().groups.get(group))
            .is_some_and(|group| group.items.is_empty());
        if empty {
            super::delete_group(cx, group, false);
            cx.notify();
            return;
        }
        let weak = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            window.open_dialog(cx, move |dialog, _window, _cx| {
                let confirm = weak.clone();
                dialog
                    .confirm()
                    .title(nohrs_core::t!("Excluir grupo"))
                    .button_props(
                        gpui_component::dialog::DialogButtonProps::default()
                            .cancel_text(nohrs_core::t!("Cancelar"))
                            .ok_text(nohrs_core::t!("Excluir")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .child(nohrs_core::t!("O grupo e os itens dele serão excluídos.")),
                    )
                    .on_ok(move |_, _window, cx| {
                        confirm
                            .update(cx, |_view, cx| {
                                super::delete_group(cx, group, true);
                                cx.notify();
                            })
                            .log_err();
                        true
                    })
            });
        });
    }

    /// Removes one item and its usage record (§12.2, §6).
    fn delete_item(&mut self, group: usize, slot: u32, cx: &mut Context<Self>) {
        super::delete_item(cx, group, slot);
        cx.notify();
    }

    /// The "浏览…" button of the editor: picks a path and puts it in the field
    /// (§12.3).
    fn browse_for_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A `folder` item picks a folder; every other kind picks a file (§17.2 R2).
        let folder = self
            .editor
            .as_ref()
            .and_then(|editor| {
                super::state(cx)
                    .and_then(|state| state.config.groups.get(editor.group))
                    .and_then(|group| group.items.iter().find(|item| item.slot == editor.slot))
            })
            .is_some_and(|item| item.kind == "folder");
        let (want, prompt) = if folder {
            (
                super::PathWant::Folders,
                nohrs_core::t!("Escolher uma pasta"),
            )
        } else {
            (
                super::PathWant::Files,
                nohrs_core::t!("Escolher um caminho"),
            )
        };
        let receiver = cx.prompt_for_paths(super::path_prompt_for(cx, want, false, prompt));
        cx.spawn_in(window, async move |this, cx| {
            let picked = match receiver.await {
                Ok(Ok(Some(paths))) => paths,
                _ => return,
            };
            let Some(path) = picked.first() else {
                return;
            };
            let text = path.to_string_lossy().to_string();
            this.update_in(cx, |view, window, cx| {
                let Some(editor) = view.editor.as_ref() else {
                    return;
                };
                let path = editor.path.clone();
                path.update(cx, |state, cx| state.set_value(text, window, cx));
            })
            .ok();
        })
        .detach();
    }

    /// Flips the pin: applies it to the window now and saves it unless the file
    /// is read-only (§11.1).
    fn toggle_lock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let locked = super::toggle_lock(cx);
        super::set_topmost(window, locked);
        cx.notify();
    }

    /// Remembers the tile the pointer is over: that tile zooms and the one it
    /// left shrinks back (§12.1).
    fn set_hovered(&mut self, slot: u32, hovered: bool, cx: &mut Context<Self>) {
        let next = hovered.then_some(slot);
        if self.hovered == next {
            return;
        }
        self.hovered = next;
        if hovered {
            self.ever_hovered.insert(slot);
        }
        cx.notify();
    }

    /// A click selects the cell and does what its modifiers ask for (§12.2).
    fn clicked(&mut self, slot: u32, kind: super::ClickKind, cx: &mut Context<Self>) {
        if let Some(quick_launch) = super::state_mut(cx) {
            quick_launch.slot = Some(slot);
        }
        match kind {
            super::ClickKind::Reveal => {
                // The Explorer takes the foreground; this window goes away the way
                // it does after a launch, unless it is pinned (§11.1).
                super::reveal_item(cx, slot);
                if !super::is_locked(cx) {
                    super::hide(cx);
                }
            }
            kind => super::activate_with(cx, slot, kind),
        }
    }

    /// A file or folder dropped from outside takes the first free slot at or
    /// after the one it landed on, and the rest of the drop follows it. Nothing
    /// already in the group is replaced (§12.2, B1).
    fn drop_paths(
        &mut self,
        group: usize,
        slot: u32,
        paths: &[std::path::PathBuf],
        cx: &mut Context<Self>,
    ) {
        if !super::group_editable(cx, group) || paths.is_empty() {
            return;
        }
        // S1: whether each path is a folder needs the disk, so it is asked on a
        // background task and the items are placed once the answers are in.
        let paths = paths.to_vec();
        cx.spawn(async move |this, cx| {
            let items = cx
                .background_spawn(async move {
                    paths
                        .iter()
                        .map(|path| {
                            let is_dir = super::path_is_directory(path);
                            nohrs_core::config::quick_launch::item_for_path(path, 0, is_dir)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |_view, cx| {
                super::place_items(cx, group, slot, items);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// An item dragged onto another slot: they swap when it is taken (§12.2).
    fn drop_item(&mut self, drag: &ItemDrag, group: usize, slot: u32, cx: &mut Context<Self>) {
        if drag.group == group {
            super::swap_slots(cx, group, drag.slot, slot);
        } else {
            super::move_item_to_group(cx, drag.group, drag.slot, group);
        }
        cx.notify();
    }

    /// An item dragged onto the group bar: first free slot, else appended.
    fn drop_item_on_group(&mut self, drag: &ItemDrag, group: usize, cx: &mut Context<Self>) {
        super::move_item_to_group(cx, drag.group, drag.slot, group);
        cx.notify();
    }
    /// Opens `rows` at `position` for `scope` (§12.2).
    fn open_menu(
        &mut self,
        position: Point<Pixels>,
        scope: MenuScope,
        rows: Vec<crate::drawn_menu::MenuRow>,
        cx: &mut Context<Self>,
    ) {
        self.menu_scope = Some(scope);
        crate::drawn_menu::open_at(self, position, rows, cx);
    }
}

impl Render for QuickLaunchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((revision, selected, configured_icon_size, group, editable)) = super::state(cx)
            .map(|state| {
                (
                    state.revision,
                    state.slot,
                    state.config().window.icon_size,
                    state.group,
                    state.status.can_save(),
                )
            })
        else {
            return div().size_full();
        };
        let loading = super::is_loading(cx);
        let read_only = super::read_only_reason(cx).map(str::to_string);
        let message = super::message(cx).map(str::to_string);
        let icon_edge = super::icon_edge(window, configured_icon_size);
        // G.5: a dynamic group's entries are generated; nothing may add, move or
        // edit them, while its tiles still launch and copy.
        let group_editable = editable && super::group_editable(cx, group);
        let notice = super::group_notice(cx, group);

        // A frame is rebuilt when the state says what it draws changed, or when
        // the window's scale asks for a different icon size (S6).
        let stale = self
            .frame
            .as_ref()
            .is_none_or(|frame| frame.revision != revision || frame.icon_edge != icon_edge);
        if stale {
            // Read inside the borrow, then drop it: `super::icon` needs the
            // state mutably.
            let planned = {
                let Some(state) = super::state(cx) else {
                    return div().size_full();
                };
                let config = state.config();
                let current = state.group;
                let names: Vec<(String, bool)> = config
                    .groups
                    .iter()
                    .enumerate()
                    .map(|(index, group)| (group.name.clone(), index == current))
                    .collect();
                let groups: Rc<Vec<(String, bool)>> = Rc::new(names);
                let items: Vec<Item> = config
                    .groups
                    .get(current)
                    .map(|group| {
                        let kept = super::filtered_slots(cx);
                        group
                            .items
                            .iter()
                            .filter(|item| kept.contains(&item.slot))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                (
                    groups,
                    items,
                    config.window.view,
                    f32::from(config.window.cell_size),
                    config.window.show_cells,
                    !state.filter.trim().is_empty(),
                )
            };
            let (groups, items, view, cell_size, show_cells, packed) = planned;
            // The icons are asked for here, once per rebuild, because
            // `super::icon` needs the mutable state.
            let drawn: Rc<Vec<Drawn>> = Rc::new(
                items
                    .iter()
                    .map(|item| {
                        let source = item
                            .icon
                            .as_deref()
                            .map(str::trim)
                            .filter(|icon| !icon.is_empty())
                            .or(item.path.as_deref());
                        let image = source.and_then(|source| super::icon(cx, source, icon_edge));
                        Drawn {
                            item: item.clone(),
                            image,
                        }
                    })
                    .collect(),
            );
            self.frame = Some(Frame {
                revision,
                icon_edge,
                groups,
                drawn,
                view,
                cell_size,
                show_cells,
                packed,
            });
        }
        let Some(frame) = self.frame.as_ref() else {
            return div().size_full();
        };
        let groups = frame.groups.clone();
        let drawn = frame.drawn.clone();
        let view = frame.view;
        let cell_size = frame.cell_size;
        let show_cells = frame.show_cells;
        let list_columns = fitted_list_columns(window);
        let packed = frame.packed;
        let list_icon = list_icon_size(configured_icon_size);
        let hovered = self.hovered;
        let ever_hovered = Rc::new(self.ever_hovered.clone());

        let filter_text = super::filter(cx).to_string();
        let filtered_count = drawn.len();
        // The window's height less what the rows above and below the grid take;
        // the grid is laid out over the rest.
        let chrome = TITLE_BAR_HEIGHT
            + if filter_text.is_empty() { 0.0 } else { 24.0 }
            + if read_only.is_some() { 30.0 } else { 0.0 }
            + if message.is_some() {
                FOOTER_HEIGHT
            } else {
                0.0
            };
        let viewport = window.viewport_size();
        let grid = grid_metrics(
            f32::from(viewport.width) - GROUP_BAR_WIDTH,
            f32::from(viewport.height) - chrome,
            cell_size,
        );
        let grid_icon = grid_icon_size(configured_icon_size, grid.cell_height);
        let title_bar = self.render_title_bar(editable, cx);
        let group_bar = render_group_bar(&groups, self.renaming.as_ref(), editable, cx);
        let entity = cx.entity().downgrade();
        let missing: Rc<HashMap<u32, bool>> = Rc::new(
            super::state(cx)
                .and_then(|state| state.config().groups.get(group))
                .map(|current| {
                    current
                        .items
                        .iter()
                        .map(|item| (item.slot, super::is_missing(cx, group, item.slot)))
                        .collect()
                })
                .unwrap_or_default(),
        );

        let content: AnyElement = if loading {
            centered_note(nohrs_core::t!("Carregando…"))
        } else if let Some(notice) = notice {
            // G.2, G.3: a dynamic group that has nothing to show says why.
            centered_note(notice)
        } else if drawn.is_empty() {
            centered_note(nohrs_core::t!("Nenhum item neste grupo"))
        } else if view == View::List {
            list_view(
                &drawn,
                selected,
                hovered,
                &ever_hovered,
                group,
                editable,
                group_editable,
                &missing,
                list_columns,
                list_icon,
                &entity,
                &self.scroll_handle,
            )
        } else {
            grid_view(
                &drawn,
                selected,
                hovered,
                &ever_hovered,
                group,
                editable,
                group_editable,
                &missing,
                grid,
                grid_icon,
                show_cells,
                packed,
                &entity,
                &self.scroll_handle,
            )
        };

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::bg()))
            .text_color(rgb(theme::fg()))
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key_down))
            .child(title_bar);
        if !filter_text.is_empty() {
            root = root.child(
                div()
                    .flex_shrink_0()
                    .h(px(24.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .child(nohrs_core::t!("Filtrar"))
                    .child(
                        div()
                            .text_color(rgb(theme::accent()))
                            .child(filter_text.clone()),
                    )
                    .child(
                        div()
                            .text_color(rgb(theme::muted()))
                            .child(nohrs_core::tf!("{n} itens", n = filtered_count)),
                    ),
            );
        }
        if let Some(reason) = read_only {
            root = root.child(
                div()
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(6.0))
                    .text_xs()
                    .bg(rgb(theme::accent_subtle()))
                    .text_color(rgb(theme::danger()))
                    .child(nohrs_core::tf!(
                        "Não foi possível ler quicklaunch.toml: {error}",
                        error = reason
                    )),
            );
        }
        root = root
            .child(
                div().flex_1().min_h(px(0.0)).flex().child(group_bar).child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .child(content)
                        .on_drop(cx.listener(
                            move |view, paths: &gpui::ExternalPaths, _window, cx| {
                                if group_editable {
                                    let target = super::next_free_slot(cx, group);
                                    let paths = paths.paths().to_vec();
                                    view.drop_paths(group, target, &paths, cx);
                                }
                            },
                        )),
                ),
            )
            .when_some(message, |this, message| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .h(px(FOOTER_HEIGHT))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .text_xs()
                        .text_color(rgb(theme::danger()))
                        .child(message),
                )
            });
        if let Some(overlay) = editor_overlay(self.editor.as_ref(), &entity) {
            root = root.child(overlay);
        }
        if let Some(confirmation) = super::command_confirmation(cx) {
            // §17.2 D.3: the script may not take more than 60% of the window.
            let script_height = f32::from(window.bounds().size.height) * 0.6;
            root = root.child(command_overlay(&confirmation, script_height.max(120.0)));
        }
        if let Some(confirmation) = super::group_confirmation(cx) {
            // §17.2 E.1: the untrusted commands of the group share one dialog.
            let script_height = f32::from(window.bounds().size.height) * 0.6;
            root = root.child(group_command_overlay(
                &confirmation,
                script_height.max(120.0),
            ));
        }
        if let Some(prompt) = super::already_running_prompt(cx) {
            // §17.2 F.2: the program is already open; the user picks what
            // happens next.
            root = root.child(already_running_overlay(&prompt));
        }
        // The menu goes over everything else: it is what a click on a row is
        // meant to reach, and its backdrop must cover the window.
        if let Some(menu) = crate::drawn_menu::render(self, window, cx) {
            root = root.child(menu);
        }
        root
    }
}

impl QuickLaunchView {
    /// The title bar: the drag area, the title, and the "⋯" menu (§15.0).
    fn render_title_bar(&mut self, editable: bool, cx: &mut Context<Self>) -> AnyElement {
        div()
            .h(px(TITLE_BAR_HEIGHT))
            .flex_shrink_0()
            .w_full()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(rgb(theme::border()))
            .child(
                div()
                    .id("quick-launch-drag-region")
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .pl(px(12.0))
                    // The empty stretch of the bar moves the window, as the
                    // explorer's title row does (`WindowControlArea::Drag`).
                    .window_control_area(WindowControlArea::Drag)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(nohrs_core::t!("Inicialização rápida")),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .pr(px(6.0))
                    .child(lock_button(editable, cx))
                    .child(
                        Button::new("quick-launch-menu")
                            .icon(Icon::new(IconName::Ellipsis).size_4())
                            .rounded(ButtonRounded::Large)
                            .with_variant(ButtonVariant::Ghost)
                            .on_click(cx.listener(|view, event: &ClickEvent, _window, cx| {
                                view.open_menu(
                                    event.position(),
                                    MenuScope::Window,
                                    vec![
                                        crate::drawn_menu::MenuRow::entry(
                                            nohrs_core::t!("Mostrar janela"),
                                            true,
                                            SHOW_WINDOW,
                                        ),
                                        crate::drawn_menu::MenuRow::rule(),
                                        crate::drawn_menu::MenuRow::entry(
                                            nohrs_core::t!("Sair do LumaFiles"),
                                            true,
                                            QUIT,
                                        ),
                                    ],
                                    cx,
                                );
                            })),
                    ),
            )
            .into_any_element()
    }
}

/// The pin: keeps the window on top and out of its own hiding rules (§11.1).
fn lock_button(editable: bool, cx: &mut Context<QuickLaunchView>) -> AnyElement {
    let locked = super::is_locked(cx);
    // The tooltip says what a click does, and warns that a read-only file keeps
    // the change only for this session.
    let mut tooltip = if locked {
        nohrs_core::t!("Desafixar").to_string()
    } else {
        nohrs_core::t!("Fixar: a janela não se esconde e fica na frente").to_string()
    };
    if !editable {
        tooltip.push(' ');
        tooltip.push_str(nohrs_core::t!("(não será salvo)"));
    }
    Button::new("quick-launch-lock")
        .icon(Icon::empty().path("icons/pin.svg").size_4())
        .tooltip(tooltip)
        .rounded(ButtonRounded::Large)
        .with_variant(ButtonVariant::Ghost)
        .when(locked, |button| button.text_color(rgb(theme::accent())))
        .on_click(cx.listener(move |view, _, window, cx| {
            view.toggle_lock(window, cx);
        }))
        .into_any_element()
}

/// One item as it is drawn: the entry and the picture it got, if any.
#[derive(Clone)]
struct Drawn {
    item: Item,
    image: Option<Arc<RenderImage>>,
}

/// The type icon of an item whose picture is not known yet (§9's placeholder).
fn type_icon(item: &Item) -> Icon {
    match item.action_kind() {
        ActionKind::Folder => Icon::new(IconName::Folder),
        ActionKind::Url => Icon::new(IconName::Globe),
        _ => Icon::new(IconName::File),
    }
}

/// The picture of one item: the extracted icon, or the type's icon, filling the
/// box its tile sizes for it.
///
/// The box is sized in logical pixels: the image may have been extracted at a
/// larger physical size for the display's scale, and filling its box keeps every
/// icon the size the settings ask for.
fn picture(item: &Item, image: Option<&Arc<RenderImage>>) -> AnyElement {
    match image {
        Some(image) => img(ImageSource::Render(image.clone()))
            .size_full()
            .into_any_element(),
        None => type_icon(item).size_full().into_any_element(),
    }
}

/// `Alt+1..9` names a group by position (§12.2).
fn group_shortcut(key: &str) -> Option<usize> {
    if key.chars().count() != 1 {
        return None;
    }
    let digit = key.chars().next()?.to_digit(10)?;
    usize::try_from(digit).ok()?.checked_sub(1)
}

/// The gap between two grid cells, in logical pixels.
const CELL_GAP: f32 = 5.0;

/// The narrowest a list column gets before another one is shown instead: its
/// icon, name and path need that much to stay readable.
const LIST_COLUMN_MIN_WIDTH: f32 = 260.0;

/// The gap between two list columns, in logical pixels (the line's `gap_2`).
const LIST_GAP: f32 = 8.0;

/// The icon edge a cell gets: the configured size, kept small enough that the
/// name's line still fits under it. `.min().max()` rather than `clamp`, which
/// panics when the cell shrinks below the icon's floor and the bounds cross.
fn grid_icon_size(configured: u16, cell_size: f32) -> f32 {
    f32::from(configured)
        .min(cell_size - NAME_LINE_HEIGHT - 12.0)
        .max(16.0)
}

/// The icon edge a list row gets: a row is 44 tall, so the configured size is
/// capped to leave room for its two lines of text.
fn list_icon_size(configured: u16) -> f32 {
    f32::from(configured).clamp(16.0, LIST_ROW_HEIGHT - 16.0)
}

/// How many cells of `cell_size` fit in `available` logical pixels.
fn fit_count(available: f32, cell_size: f32) -> usize {
    let cell = cell_size.max(1.0);
    let fit = ((available + CELL_GAP) / (cell + CELL_GAP)).floor();
    if fit.is_finite() && fit >= 1.0 {
        usize::try_from(fit as u64).unwrap_or(1)
    } else {
        1
    }
}

/// A grid that covers the window.
///
/// The cell size setting says how big a cell should be; the window's size says how
/// many fit; the cells then stretch so the window is filled exactly. An item that
/// does not fit adds rows below, which scroll.
#[derive(Clone, Copy)]
struct Grid {
    columns: usize,
    /// How many rows the window's height shows at once.
    fill_rows: usize,
    cell_width: f32,
    cell_height: f32,
}

/// [`Grid`] for `width` by `height` of usable content area.
fn grid_metrics(width: f32, height: f32, cell_size: f32) -> Grid {
    let columns = fit_count(width, cell_size);
    let fill_rows = fit_count(height, cell_size);
    let cell_width = ((width - CELL_GAP * (columns - 1) as f32) / columns as f32).max(1.0);
    // Every row carries the gap above it, the first one included, so the rows are
    // as far apart as the columns and the grid still ends at the window's edge.
    let cell_height = ((height - CELL_GAP * fill_rows as f32) / fill_rows as f32).max(1.0);
    Grid {
        columns,
        fill_rows,
        cell_width,
        cell_height,
    }
}

/// How many list columns `width` has room for: one more whenever another
/// minimum-width column fits, so the list widens with the window instead of
/// leaving the space empty.
fn fit_list_columns(width: f32) -> usize {
    let fit = ((width + LIST_GAP) / (LIST_COLUMN_MIN_WIDTH + LIST_GAP)).floor();
    if fit.is_finite() && fit >= 1.0 {
        // Cap only to keep a very wide window from making the rows unreadably
        // thin; the columns still share the row's width.
        usize::try_from(fit as u64).unwrap_or(1).min(8)
    } else {
        1
    }
}

/// [`fit_list_columns`] for the window's width, once the group bar is taken out.
fn fitted_list_columns(window: &Window) -> usize {
    fit_list_columns(f32::from(window.viewport_size().width) - GROUP_BAR_WIDTH)
}

/// How long a tile takes to grow into the pointer and back (§12.1).
const HOVER_MS: u64 = 120;

/// The grid icon at rest and grown: a fifth bigger, as far as the cell's name
/// line leaves room.
fn hover_icon_edges(icon_size: f32, cell_size: f32) -> (f32, f32) {
    let grown = (icon_size * 1.2)
        .min(cell_size - NAME_LINE_HEIGHT - 6.0)
        .max(icon_size);
    (icon_size, grown)
}

/// The list row's icon at rest and grown.
fn hover_list_icon_edges(icon_size: f32) -> (f32, f32) {
    (
        icon_size,
        (icon_size * 1.2).min(LIST_ROW_HEIGHT - 10.0).max(icon_size),
    )
}

/// One element's hover zoom: the size it has at rest and when the pointer is
/// over it.
#[derive(Clone, Copy)]
struct Zoom {
    slot: u32,
    hovered: bool,
    ever_hovered: bool,
    rest: f32,
    grown: f32,
}

/// Applies the hover zoom to `element`, growing it while the pointer is over the
/// tile and shrinking it back after the pointer left.
///
/// `ever_hovered` is what keeps a tile that was never hovered at its resting
/// size: it has nothing to shrink from. The element id carries which direction is
/// running, so every change starts a fresh animation; once one finishes its state
/// holds the final value and the frames stop.
fn hover_zoom(
    element: Div,
    name: &'static str,
    zoom: Zoom,
    place: impl Fn(Div, f32) -> Div + Copy + 'static,
) -> AnyElement {
    let animation =
        || Animation::new(Duration::from_millis(HOVER_MS)).with_easing(ease_out_quint());
    let at = move |progress: f32| zoom.rest + (zoom.grown - zoom.rest) * progress;
    let id = |zooming: bool| {
        (
            name,
            usize::try_from(zoom.slot).unwrap_or(usize::MAX) * 2 + usize::from(zooming),
        )
    };
    if zoom.hovered {
        element
            .with_animation(id(true), animation(), move |element, delta| {
                place(element, at(delta))
            })
            .into_any_element()
    } else if zoom.ever_hovered {
        element
            .with_animation(id(false), animation(), move |element, delta| {
                place(element, at(1.0 - delta))
            })
            .into_any_element()
    } else {
        place(element, zoom.rest).into_any_element()
    }
}

/// The grid's cells, in reading order, with the items placed by slot (§12.1).
fn grid_cells(items: &[Item], columns: usize) -> Vec<Option<Item>> {
    let columns = columns.max(1);
    // The row count is `ceil((highest slot + 1) / columns)` plus one row of
    // empty slots, so there is always somewhere to put a new item. The slot is
    // clamped before the arithmetic: a file edited by hand can hold a number
    // the validator would have cut, and one of those as a row count would ask
    // for gigabytes of empty cells.
    let rows = match items.iter().map(|item| item.slot.min(MAX_SLOT)).max() {
        Some(highest) => {
            let used = usize::try_from(highest).unwrap_or(usize::MAX - columns);
            used.saturating_add(1).div_ceil(columns).saturating_add(1)
        }
        None => 1,
    };
    let mut cells = vec![None; rows.saturating_mul(columns)];
    for item in items {
        if item.slot > MAX_SLOT {
            continue;
        }
        let index = usize::try_from(item.slot).unwrap_or(usize::MAX);
        if let Some(cell) = cells.get_mut(index) {
            *cell = Some(item.clone());
        }
    }
    cells
}

/// [`grid_cells`] for the timing test in the parent module (§16.2).
#[cfg(test)]
pub(super) fn grid_cells_for_timing(items: &[Item], columns: usize) -> usize {
    grid_cells(items, columns).len()
}

/// Text in the middle of the content area.
fn centered_note(note: &'static str) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(rgb(theme::muted()))
        .child(note)
        .into_any_element()
}

/// The bar the group names sit in (§12.1), with the M2 menu and the rename
/// field (§12.3).
fn render_group_bar(
    groups: &[(String, bool)],
    renaming: Option<&Rename>,
    editable: bool,
    cx: &mut Context<QuickLaunchView>,
) -> AnyElement {
    let mut bar = div()
        .w(px(GROUP_BAR_WIDTH))
        .flex_shrink_0()
        .h_full()
        .flex()
        .flex_col()
        .px(px(6.0))
        .py(px(6.0))
        .gap_1()
        .border_r_1()
        .border_color(rgb(theme::border()));
    let last_group = groups.len() <= 1;
    for (index, (name, active)) in groups.iter().enumerate() {
        let active = *active;
        // The group being renamed shows its field instead of its name (§12.3).
        if let Some(rename) = renaming.filter(|rename| rename.group == index) {
            bar = bar.child(
                div()
                    .h(px(28.0))
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .child(Input::new(&rename.input)),
            );
            continue;
        }
        let entity = cx.entity().downgrade();
        let entity_drop = entity.clone();
        bar = bar.child(
            div()
                .id(("quick-launch-group", index))
                .h(px(28.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .text_xs()
                // The underline the selected group carries (§12.1).
                .when(active, |this| {
                    this.bg(rgb(theme::bg_hover()))
                        .border_b_2()
                        .border_color(rgb(theme::accent()))
                        .text_color(rgb(theme::fg()))
                })
                .when(!active, |this| this.text_color(rgb(theme::fg_secondary())))
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .on_click(cx.listener(move |_view, _, _window, cx| {
                    super::select_group(cx, index);
                }))
                .on_drop(move |drag: &ItemDrag, _window, cx| {
                    // An item dropped on the bar joins that group: first free
                    // slot, else appended (§12.2). A dynamic group takes no
                    // entries at all (§17.2 G.5).
                    if editable && super::group_editable(cx, index) {
                        entity_drop
                            .update(cx, |view, cx| view.drop_item_on_group(drag, index, cx))
                            .log_err();
                    }
                })
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |view, event: &MouseDownEvent, _window, cx| {
                        cx.stop_propagation();
                        view.open_menu(
                            event.position,
                            MenuScope::Group(index),
                            group_menu_rows(index, editable, last_group),
                            cx,
                        );
                    }),
                )
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(name.clone()),
                ),
        );
    }
    bar.into_any_element()
}

/// The cells of a filtered grid: the matches packed in slot order, then one
/// empty row (§12.2).
fn packed_cells(items: &[Item], columns: usize) -> Vec<Option<Item>> {
    let columns = columns.max(1);
    let rows = items.len().div_ceil(columns).saturating_add(1);
    let mut cells: Vec<Option<Item>> = items.iter().cloned().map(Some).collect();
    cells.resize(rows.saturating_mul(columns), None);
    cells
}

/// The grid (§12.1), virtualized by row.
#[allow(clippy::too_many_arguments)]
fn grid_view(
    drawn: &[Drawn],
    selected: Option<u32>,
    hovered: Option<u32>,
    ever_hovered: &HashSet<u32>,
    group: usize,
    editable: bool,
    group_editable: bool,
    missing: &HashMap<u32, bool>,
    grid: Grid,
    icon_size: f32,
    show_cells: bool,
    packed: bool,
    entity: &WeakEntity<QuickLaunchView>,
    scroll_handle: &UniformListScrollHandle,
) -> AnyElement {
    let columns = grid.columns;
    let items: Vec<Item> = drawn.iter().map(|drawn| drawn.item.clone()).collect();
    let cells = if packed {
        packed_cells(&items, columns)
    } else {
        grid_cells(&items, columns)
    };
    let rows = cells.len().div_ceil(columns.max(1)).max(grid.fill_rows);
    let cells = Rc::new(cells);
    let drawn = Rc::new(drawn.to_vec());
    // The icon of a cell is the one the item got; the cell index is the slot.
    let pictures_by_slot: HashMap<u32, Option<Arc<RenderImage>>> = drawn
        .iter()
        .map(|entry| (entry.item.slot, entry.image.clone()))
        .collect();
    let pictures_by_slot = Rc::new(pictures_by_slot);
    let missing = Rc::new(missing.clone());
    let ever_hovered = Rc::new(ever_hovered.clone());
    let entity = entity.clone();
    uniform_list(
        "quick-launch-grid",
        rows,
        move |range: Range<usize>, _window, _cx| {
            let mut lines = Vec::with_capacity(range.len());
            for row in range {
                let mut line = div()
                    .h(px(grid.cell_height + CELL_GAP))
                    .pt(px(CELL_GAP))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(CELL_GAP));
                for column in 0..columns.max(1) {
                    let index = row.saturating_mul(columns.max(1)).saturating_add(column);
                    match cells.get(index).and_then(Option::as_ref) {
                        Some(item) => {
                            let selected = selected == Some(item.slot);
                            let image = pictures_by_slot
                                .get(&item.slot)
                                .and_then(|image| image.clone());
                            let missing = missing.get(&item.slot).copied().unwrap_or(false);
                            line = line.child(tile(
                                item,
                                image,
                                selected,
                                hovered == Some(item.slot),
                                ever_hovered.contains(&item.slot),
                                missing,
                                group,
                                editable,
                                group_editable,
                                grid.cell_width,
                                grid.cell_height,
                                icon_size,
                                show_cells,
                                &entity,
                            ));
                        }
                        None if packed => {
                            // While filtering the empty cells are only the
                            // padding of the last row: there is no slot to add to.
                            line = line.child(
                                div()
                                    .w(px(grid.cell_width))
                                    .h(px(grid.cell_height))
                                    .flex_shrink_0()
                                    .rounded(px(8.0)),
                            );
                        }
                        None => {
                            let slot = u32::try_from(index).unwrap_or(MAX_SLOT);
                            line = line.child(empty_cell(
                                slot,
                                group,
                                group_editable,
                                grid.cell_width,
                                grid.cell_height,
                                show_cells,
                                &entity,
                            ));
                        }
                    }
                }
                lines.push(line);
            }
            lines
        },
    )
    .flex_1()
    .track_scroll(scroll_handle.clone())
    .into_any_element()
}

/// The list (§12.1): `list_columns` columns of rows, virtualized by row.
#[allow(clippy::too_many_arguments)]
fn list_view(
    drawn: &[Drawn],
    selected: Option<u32>,
    hovered: Option<u32>,
    ever_hovered: &HashSet<u32>,
    group: usize,
    editable: bool,
    group_editable: bool,
    missing: &HashMap<u32, bool>,
    list_columns: usize,
    icon_size: f32,
    entity: &WeakEntity<QuickLaunchView>,
    scroll_handle: &UniformListScrollHandle,
) -> AnyElement {
    let list_columns = list_columns.max(1);
    let rows = drawn.len().div_ceil(list_columns);
    let drawn = Rc::new(drawn.to_vec());
    let missing = Rc::new(missing.clone());
    let ever_hovered = Rc::new(ever_hovered.clone());
    let entity = entity.clone();
    uniform_list(
        "quick-launch-list",
        rows,
        move |range: Range<usize>, _window, _cx| {
            let mut lines = Vec::with_capacity(range.len());
            for row in range {
                let mut line = div().h(px(LIST_ROW_HEIGHT)).w_full().flex().gap_2();
                for column in 0..list_columns {
                    let index = row.saturating_mul(list_columns).saturating_add(column);
                    match drawn.get(index) {
                        Some(entry) => {
                            let selected = selected == Some(entry.item.slot);
                            let missing = missing.get(&entry.item.slot).copied().unwrap_or(false);
                            line = line.child(div().flex_1().min_w(px(0.0)).child(row_of(
                                entry,
                                selected,
                                hovered == Some(entry.item.slot),
                                ever_hovered.contains(&entry.item.slot),
                                missing,
                                group,
                                editable,
                                group_editable,
                                icon_size,
                                &entity,
                            )));
                        }
                        None => line = line.child(div().flex_1()),
                    }
                }
                lines.push(line);
            }
            lines
        },
    )
    .flex_1()
    .track_scroll(scroll_handle.clone())
    .into_any_element()
}

/// An empty cell: the drop target of the next milestone's drags, and the place
/// the "new item" menu hangs from (§12.2, §12.3).
fn empty_cell(
    slot: u32,
    group: usize,
    group_editable: bool,
    cell_width: f32,
    cell_height: f32,
    show_cells: bool,
    entity: &WeakEntity<QuickLaunchView>,
) -> AnyElement {
    let drop_handle = entity.clone();
    let item_handle = entity.clone();
    div()
        .id(("quick-launch-empty", slot))
        .w(px(cell_width))
        .h(px(cell_height))
        .flex_shrink_0()
        .rounded(px(8.0))
        .when(show_cells, |this| this.bg(rgb(theme::bg_secondary())))
        .cursor_pointer()
        .when(show_cells, |this| {
            this.hover(|style| style.bg(rgb(theme::bg_hover())))
        })
        // A file or folder dropped on the empty cell lands in it, instead of
        // in the group's first free slot as the container's own handler does.
        .drag_over::<gpui::ExternalPaths>(|style, _, _, _| {
            style
                .bg(rgb(theme::accent_subtle()))
                .border_1()
                .border_color(rgb(theme::accent()))
        })
        .on_drop(move |paths: &gpui::ExternalPaths, _window, cx| {
            let paths = paths.paths().to_vec();
            drop_handle
                .update(cx, |view, cx| view.drop_paths(group, slot, &paths, cx))
                .log_err();
        })
        // An item dragged onto an empty cell moves there (it swaps when the
        // cell already holds one; `swap_slots` moves it when it does not).
        .drag_over::<ItemDrag>(|style, _, _, _| {
            style
                .bg(rgb(theme::accent_subtle()))
                .border_1()
                .border_color(rgb(theme::accent()))
        })
        .on_drop(move |drag: &ItemDrag, _window, cx| {
            let drag = drag.clone();
            item_handle
                .update(cx, |view, cx| view.drop_item(&drag, group, slot, cx))
                .log_err();
        })
        .on_mouse_down(MouseButton::Right, {
            let handle = entity.clone();
            move |event: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
                let mixed = cx.can_select_mixed_files_and_dirs();
                handle
                    .update(cx, |view, cx| {
                        view.open_menu(
                            event.position,
                            MenuScope::Item { group, slot },
                            cell_menu_rows(group_editable, mixed),
                            cx,
                        );
                    })
                    .log_err();
            }
        })
        .into_any_element()
}

/// The rows of the menu a group's right click opens (§12.3).
fn group_menu_rows(
    index: usize,
    editable: bool,
    last_group: bool,
) -> Vec<crate::drawn_menu::MenuRow> {
    let _ = index;
    vec![
        crate::drawn_menu::MenuRow::entry(nohrs_core::t!("Novo grupo"), editable, NEW_GROUP),
        crate::drawn_menu::MenuRow::entry(nohrs_core::t!("Renomear"), editable, RENAME_GROUP),
        crate::drawn_menu::MenuRow::rule(),
        crate::drawn_menu::MenuRow::entry(
            nohrs_core::t!("Excluir"),
            editable && !last_group,
            DELETE_GROUP,
        ),
    ]
}

/// The rows of the menu an empty cell opens (§12.3, §17.2 R2).
fn cell_menu_rows(group_editable: bool, mixed_selection: bool) -> Vec<crate::drawn_menu::MenuRow> {
    let mut rows = vec![crate::drawn_menu::MenuRow::entry(
        nohrs_core::t!("Novo item"),
        group_editable,
        NEW_ITEM,
    )];
    // §17.2 R2: where the dialog can only pick one of the two, folders get an
    // entry of their own.
    if super::folder_entry_offered(mixed_selection) {
        rows.push(crate::drawn_menu::MenuRow::entry(
            nohrs_core::t!("Novo item de pasta"),
            group_editable,
            NEW_ITEM_FOLDER,
        ));
    }
    rows
}

/// The rows of the menu one item opens (§12.2, M2 rows).
fn item_menu_rows(
    cx: &App,
    group: usize,
    editable: bool,
    group_editable: bool,
) -> Vec<crate::drawn_menu::MenuRow> {
    let mut rows = vec![
        crate::drawn_menu::MenuRow::entry(
            nohrs_core::t!("Executar como administrador"),
            editable,
            RUN_AS_ADMIN,
        ),
        crate::drawn_menu::MenuRow::entry(nohrs_core::t!("Abrir localização"), true, REVEAL),
        crate::drawn_menu::MenuRow::rule(),
        crate::drawn_menu::MenuRow::entry(nohrs_core::t!("Editar"), group_editable, EDIT),
        crate::drawn_menu::MenuRow::entry(nohrs_core::t!("Copiar caminho"), true, COPY_PATH),
    ];
    // "Mover para ▸" lists every other group; with a single group there is
    // nowhere to move to and the entry is left out (§12.2).
    if let Some(state) = super::state(cx) {
        let targets: Vec<crate::drawn_menu::MenuRow> = state
            .config()
            .groups
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != group)
            .map(|(index, target)| {
                // G.5: neither a copy nor a generated entry may be moved into
                // or out of a dynamic group.
                crate::drawn_menu::MenuRow::entry(
                    SharedString::from(target.name.clone()),
                    group_editable && super::group_editable(cx, index),
                    MOVE_TO + u32::try_from(index).unwrap_or(u32::MAX - MOVE_TO),
                )
            })
            .collect();
        if !targets.is_empty() {
            rows.push(crate::drawn_menu::MenuRow::submenu(
                nohrs_core::t!("Mover para"),
                targets,
            ));
        }
    }
    rows.push(crate::drawn_menu::MenuRow::rule());
    rows.push(crate::drawn_menu::MenuRow::entry(
        nohrs_core::t!("Excluir"),
        group_editable,
        DELETE_ITEM,
    ));
    rows
}

/// One grid tile.
#[allow(clippy::too_many_arguments)]
fn tile(
    item: &Item,
    image: Option<Arc<RenderImage>>,
    selected: bool,
    hovered: bool,
    ever_hovered: bool,
    missing: bool,
    group: usize,
    editable: bool,
    group_editable: bool,
    cell_width: f32,
    cell_height: f32,
    icon_size: f32,
    show_cells: bool,
    entity: &WeakEntity<QuickLaunchView>,
) -> AnyElement {
    let handle = entity.clone();
    let menu_handle = entity.clone();
    let entity_drop = entity.clone();
    let slot = item.slot;
    let drag_name = SharedString::from(item.name.clone());
    let drop_handle = entity.clone();
    let hover_handle = entity.clone();
    let (icon_rest, icon_grown) = hover_icon_edges(icon_size, cell_height);
    let icon = div()
        .flex_none()
        .size(px(icon_rest))
        .child(picture(item, image.as_ref()));
    let icon = hover_zoom(
        icon,
        "quick-launch-icon",
        Zoom {
            slot,
            hovered,
            ever_hovered,
            rest: icon_rest,
            grown: icon_grown,
        },
        |icon, edge| icon.size(px(edge)),
    );
    div()
        .id(("quick-launch-tile", slot))
        .w(px(cell_width))
        .h(px(cell_height))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .rounded(px(8.0))
        .when(show_cells, |this| this.bg(rgb(theme::bg_secondary())))
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme::accent_subtle())))
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .on_click(move |event, _window, cx| {
            handle
                .update(cx, |view, cx| view.clicked(slot, click_kind(event), cx))
                .log_err();
        })
        .on_hover(move |over: &bool, _window, cx| {
            hover_handle
                .update(cx, |view, cx| view.set_hovered(slot, *over, cx))
                .log_err();
        })
        .when(group_editable, |this| {
            this.on_drag(ItemDrag { group, slot }, move |_, _, _, cx| {
                cx.new(|_| ItemDragPreview {
                    name: drag_name.clone(),
                })
            })
        })
        .on_drop(move |drag: &ItemDrag, _window, cx| {
            if group_editable {
                drop_handle
                    .update(cx, |view, cx| view.drop_item(drag, group, slot, cx))
                    .log_err();
            }
        })
        .on_drop(move |paths: &gpui::ExternalPaths, _window, cx| {
            if group_editable {
                let paths = paths.paths().to_vec();
                entity_drop
                    .update(cx, |view, cx| view.drop_paths(group, slot, &paths, cx))
                    .log_err();
            }
        })
        .on_mouse_down(MouseButton::Right, {
            let handle = menu_handle.clone();
            move |event: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
                handle
                    .update(cx, |view, cx| {
                        view.open_menu(
                            event.position,
                            MenuScope::Item { group, slot },
                            item_menu_rows(cx, group, editable, group_editable),
                            cx,
                        );
                    })
                    .log_err();
            }
        })
        .child(icon)
        .child(
            div()
                .max_w(px(cell_width - 6.0))
                .h(px(NAME_LINE_HEIGHT))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_xs()
                // An item that is known to be broken is grey (§12.1).
                .when(missing, |this| this.text_color(rgb(theme::muted())))
                .child(item.name.clone()),
        )
        .when(missing, |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::danger()))
                    .child(nohrs_core::t!("Indisponível")),
            )
        })
        .into_any_element()
}

/// One list row.
#[allow(clippy::too_many_arguments)]
fn row_of(
    drawn: &Drawn,
    selected: bool,
    hovered: bool,
    ever_hovered: bool,
    missing: bool,
    group: usize,
    editable: bool,
    group_editable: bool,
    icon_size: f32,
    entity: &WeakEntity<QuickLaunchView>,
) -> AnyElement {
    let handle = entity.clone();
    let menu_handle = entity.clone();
    let entity_drop = entity.clone();
    let drop_handle = entity.clone();
    let drag_name = SharedString::from(drawn.item.name.clone());
    let item = &drawn.item;
    let slot = item.slot;
    let hover_handle = entity.clone();
    let (icon_rest, icon_grown) = hover_list_icon_edges(icon_size);
    let icon = div()
        .flex_none()
        .size(px(icon_rest))
        .child(picture(item, drawn.image.as_ref()));
    let icon = hover_zoom(
        icon,
        "quick-launch-row-icon",
        Zoom {
            slot,
            hovered,
            ever_hovered,
            rest: icon_rest,
            grown: icon_grown,
        },
        |icon, edge| icon.size(px(edge)),
    );
    let subtitle = match item.action_kind() {
        ActionKind::Unknown(_) => nohrs_core::t!("Tipo não suportado").to_string(),
        _ => item.path.clone().unwrap_or_default(),
    };
    div()
        .id(("quick-launch-row", slot))
        .h(px(LIST_ROW_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .px(px(8.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme::accent_subtle())))
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .on_click(move |event, _window, cx| {
            handle
                .update(cx, |view, cx| view.clicked(slot, click_kind(event), cx))
                .log_err();
        })
        .on_hover(move |over: &bool, _window, cx| {
            hover_handle
                .update(cx, |view, cx| view.set_hovered(slot, *over, cx))
                .log_err();
        })
        .when(group_editable, |this| {
            this.on_drag(ItemDrag { group, slot }, move |_, _, _, cx| {
                cx.new(|_| ItemDragPreview {
                    name: drag_name.clone(),
                })
            })
        })
        .on_drop(move |drag: &ItemDrag, _window, cx| {
            if group_editable {
                drop_handle
                    .update(cx, |view, cx| view.drop_item(drag, group, slot, cx))
                    .log_err();
            }
        })
        .on_drop(move |paths: &gpui::ExternalPaths, _window, cx| {
            if group_editable {
                let paths = paths.paths().to_vec();
                entity_drop
                    .update(cx, |view, cx| view.drop_paths(group, slot, &paths, cx))
                    .log_err();
            }
        })
        .on_mouse_down(MouseButton::Right, {
            let handle = menu_handle.clone();
            move |event: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
                handle
                    .update(cx, |view, cx| {
                        view.open_menu(
                            event.position,
                            MenuScope::Item { group, slot },
                            item_menu_rows(cx, group, editable, group_editable),
                            cx,
                        );
                    })
                    .log_err();
            }
        })
        .child(icon)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_sm()
                        .when(missing, |this| this.text_color(rgb(theme::muted())))
                        .child(item.name.clone()),
                )
                .when(!subtitle.is_empty(), |this| {
                    this.child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(rgb(theme::muted()))
                            .child(subtitle),
                    )
                })
                .when(missing, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::danger()))
                            .child(nohrs_core::t!("Indisponível")),
                    )
                }),
        )
        .into_any_element()
}

/// What a click's modifiers ask for (§12.2).
fn click_kind(event: &ClickEvent) -> super::ClickKind {
    match event {
        ClickEvent::Mouse(mouse) if mouse.down.modifiers.control => super::ClickKind::Admin,
        ClickEvent::Mouse(mouse) if mouse.down.modifiers.shift => super::ClickKind::Reveal,
        _ => super::ClickKind::Plain,
    }
}

/// One labelled input of the editor (§12.3).
fn field(label: &'static str, input: &Entity<InputState>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(rgb(theme::muted())).child(label))
        .child(Input::new(input))
        .into_any_element()
}

/// One labelled line of the editor: the label in a fixed column, the field
/// beside it, the way Windows' shortcut properties dialog lays its fields out.
fn labeled_field(label: &'static str, input: &Entity<InputState>) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(96.0))
                .flex_shrink_0()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(label),
        )
        .child(div().flex_1().min_w(px(0.0)).child(Input::new(input)))
        .into_any_element()
}

/// The editor card: the fields of §12.3 and its two buttons.
///
/// It is drawn inside the window instead of through `window.open_dialog` because
/// "浏览…" has to put the picked path into an input field after an async platform
/// call, and only the view can hand out the window for that.
fn editor_overlay(
    editor: Option<&Editor>,
    entity: &WeakEntity<QuickLaunchView>,
) -> Option<AnyElement> {
    let editor = editor?;
    let name = editor.name.clone();
    let path = editor.path.clone();
    let args = editor.args.clone();
    let cwd = editor.cwd.clone();
    let icon = editor.icon.clone();
    let admin = editor.admin.clone();
    let admin_value = admin.get();
    let browse = entity.clone();
    let confirm = entity.clone();
    let cancel = entity.clone();
    let fields = if editor.group_item {
        // §17.2 E.4: only the name changes here; the rest belongs to the
        // settings page, which is where the group's entries are chosen.
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(field(nohrs_core::t!("Nome"), &name))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::muted()))
                    .child(nohrs_core::t!(
                        "Só o nome pode ser alterado aqui; edite os outros campos na página de configurações."
                    )),
            )
    } else {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(labeled_field(nohrs_core::t!("Nome"), &name))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(96.0))
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(rgb(theme::muted()))
                            .child(nohrs_core::t!("Caminho")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(Input::new(&path)),
                    )
                    .child(
                        Button::new("quick-launch-browse")
                            .label(nohrs_core::t!("Procurar…"))
                            .on_click(move |_, window, cx| {
                                browse
                                    .update(cx, |view, cx| {
                                        view.browse_for_path(window, cx);
                                    })
                                    .log_err();
                            }),
                    ),
            )
            .child(labeled_field(nohrs_core::t!("Argumentos"), &args))
            .child(labeled_field(nohrs_core::t!("Pasta de trabalho"), &cwd))
            .child(labeled_field(nohrs_core::t!("Ícone"), &icon))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(96.0)).flex_shrink_0())
                    .child(
                        Checkbox::new("quick-launch-admin")
                            .label(nohrs_core::t!("Executar como administrador"))
                            .checked(admin_value)
                            .on_click(move |checked, _window, cx| {
                                admin.set(*checked);
                                cx.refresh_windows();
                            }),
                    ),
            )
            .child(
                // The variables the launcher expands (§7), said where they are
                // typed rather than only in the manual.
                div()
                    .pl(px(104.0))
                    .text_xs()
                    .text_color(rgb(theme::muted()))
                    .child(
                        "路径、参数、工作文件夹和图标支持 %环境变量%（如 %ProgramFiles%）以及 {luma}、{config}、{portable}",
                    ),
            )
    };
    Some(
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x00000080))
            .child(
                div()
                    .w(px(520.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p(px(16.0))
                    .rounded(px(10.0))
                    .bg(rgb(theme::bg()))
                    .border_1()
                    .border_color(rgb(theme::border()))
                    .child(div().text_sm().child(nohrs_core::t!("Editar item")))
                    .child(fields)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("quick-launch-cancel")
                                    .label(nohrs_core::t!("Cancelar"))
                                    .on_click(move |_, _window, cx| {
                                        cancel
                                            .update(cx, |view, cx| {
                                                view.editor = None;
                                                cx.notify();
                                            })
                                            .log_err();
                                    }),
                            )
                            .child(
                                Button::new("quick-launch-confirm")
                                    .label(nohrs_core::t!("Confirmar"))
                                    .on_click(move |_, _window, cx| {
                                        confirm
                                            .update(cx, |view, cx| {
                                                view.confirm_editor(cx);
                                            })
                                            .log_err();
                                    }),
                            ),
                    ),
            )
            .into_any_element(),
    )
}

/// The first-run confirmation of a `command` (§17.2 D.3): what will run, as it
/// was written, before anything starts.
///
/// `script_height` is the 60% of the window the script box may take (§17.2 D.3),
/// computed by the caller from the window being drawn.
fn command_overlay(confirmation: &super::CommandConfirmation, script_height: f32) -> AnyElement {
    let cwd = confirmation.cwd.clone().unwrap_or_else(|| "—".to_string());
    let mut card = div()
        .w(px(560.0))
        .flex()
        .flex_col()
        .gap_3()
        .p(px(16.0))
        .rounded(px(10.0))
        .bg(rgb(theme::bg()))
        .border_1()
        .border_color(rgb(theme::border()))
        .child(div().text_sm().child(nohrs_core::t!("Executar este comando?")))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(nohrs_core::t!(
                    "Este comando roda com as suas permissões. Confira o conteúdo antes de continuar."
                )),
        )
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(confirmation.name.clone()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child(format!(
                    "{}: {}",
                    nohrs_core::t!("Shell"),
                    confirmation.shell
                ))
                .child(format!(
                    "{}: {cwd}",
                    nohrs_core::t!("Pasta de trabalho")
                )),
        );
    if confirmation.hidden {
        card = card.child(
            div()
                .rounded(px(6.0))
                .p(px(8.0))
                .text_xs()
                .bg(rgb(theme::accent_subtle()))
                .text_color(rgb(theme::danger()))
                .child(nohrs_core::t!(
                    "Vai rodar em segundo plano, sem janela visível."
                )),
        );
    }
    card = card.child(
        div()
            .id("quick-launch-command-script")
            .max_h(px(script_height))
            .overflow_y_scroll()
            .p(px(10.0))
            .rounded(px(6.0))
            .bg(rgb(theme::gray_100()))
            .border_1()
            .border_color(rgb(theme::border()))
            .font(crate::terminal::code_font())
            .text_size(px(12.0))
            .line_height(px(16.0))
            .text_color(rgb(theme::fg()))
            .child(confirmation.script.clone()),
    );
    if !confirmation.remembered {
        card = card.child(
            div()
                .text_xs()
                .text_color(rgb(theme::warning()))
                .child(nohrs_core::t!(
                    "Sem armazenamento: a confirmação vale até fechar o programa."
                )),
        );
    }
    card = card.child(
        div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("quick-launch-command-cancel")
                    .label(nohrs_core::t!("Cancelar"))
                    .on_click(|_, _window, cx| {
                        super::cancel_pending_command(cx);
                    }),
            )
            .child(
                Button::new("quick-launch-command-run")
                    .label(nohrs_core::t!("Executar"))
                    .on_click(|_, _window, cx| {
                        super::confirm_pending_command(cx);
                    }),
            ),
    );
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(0x00000080))
        .child(card)
        .into_any_element()
}

/// The first-run confirmation of a group (§17.2 E.1): every untrusted command
/// the batch would run, listed together before anything starts.
///
/// `script_height` is the part of the window the list may take, as in the
/// single-command dialog.
fn group_command_overlay(
    confirmation: &super::GroupConfirmation,
    script_height: f32,
) -> AnyElement {
    let mut list = div()
        .id("quick-launch-group-commands")
        .max_h(px(script_height))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap_2();
    for command in &confirmation.commands {
        let cwd = command.cwd.clone().unwrap_or_else(|| "—".to_string());
        let mut entry = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(command.name.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_xs()
                    .text_color(rgb(theme::fg_secondary()))
                    .child(format!("{}: {}", nohrs_core::t!("Shell"), command.shell))
                    .child(format!("{}: {cwd}", nohrs_core::t!("Pasta de trabalho"))),
            );
        if command.hidden {
            entry = entry.child(
                div()
                    .rounded(px(6.0))
                    .p(px(8.0))
                    .text_xs()
                    .bg(rgb(theme::accent_subtle()))
                    .text_color(rgb(theme::danger()))
                    .child(nohrs_core::t!(
                        "Vai rodar em segundo plano, sem janela visível."
                    )),
            );
        }
        entry = entry.child(
            div()
                .p(px(10.0))
                .rounded(px(6.0))
                .bg(rgb(theme::gray_100()))
                .border_1()
                .border_color(rgb(theme::border()))
                .font(crate::terminal::code_font())
                .text_size(px(12.0))
                .line_height(px(16.0))
                .text_color(rgb(theme::fg()))
                .child(command.script.clone()),
        );
        list = list.child(entry);
    }
    let mut card = div()
        .w(px(560.0))
        .flex()
        .flex_col()
        .gap_3()
        .p(px(16.0))
        .rounded(px(10.0))
        .bg(rgb(theme::bg()))
        .border_1()
        .border_color(rgb(theme::border()))
        .child(
            div()
                .text_sm()
                .child(nohrs_core::t!("Executar estes comandos?")),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(nohrs_core::t!(
                    "Este comando roda com as suas permissões. Confira o conteúdo antes de continuar."
                )),
        )
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(confirmation.name.clone()),
        )
        .child(list);
    if !confirmation.remembered {
        card = card.child(
            div()
                .text_xs()
                .text_color(rgb(theme::warning()))
                .child(nohrs_core::t!(
                    "Sem armazenamento: a confirmação vale até fechar o programa."
                )),
        );
    }
    card = card.child(
        div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("quick-launch-group-cancel")
                    .label(nohrs_core::t!("Cancelar"))
                    .on_click(|_, _window, cx| {
                        super::cancel_pending_command(cx);
                    }),
            )
            .child(
                Button::new("quick-launch-group-run")
                    .label(nohrs_core::t!("Executar"))
                    .on_click(|_, _window, cx| {
                        super::confirm_pending_group(cx);
                    }),
            ),
    );
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(0x00000080))
        .child(card)
        .into_any_element()
}

/// The running-program dialog (§17.2 F.2): switch to the instance that is
/// already open, start another one, or cancel.
fn already_running_overlay(prompt: &super::AlreadyRunningPrompt) -> AnyElement {
    let card = div()
        .w(px(440.0))
        .flex()
        .flex_col()
        .gap_3()
        .p(px(16.0))
        .rounded(px(10.0))
        .bg(rgb(theme::bg()))
        .border_1()
        .border_color(rgb(theme::border()))
        .child(
            div()
                .text_sm()
                .child(nohrs_core::t!("Este programa já está aberto.")),
        )
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(prompt.name.clone()),
        )
        .child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("quick-launch-running-cancel")
                        .label(nohrs_core::t!("Cancelar"))
                        .on_click(|_, _window, cx| {
                            super::cancel_already_running(cx);
                        }),
                )
                .child(
                    Button::new("quick-launch-running-another")
                        .label(nohrs_core::t!("Abrir outra vez"))
                        .on_click(|_, _window, cx| {
                            super::start_another_instance(cx);
                        }),
                )
                .child(
                    Button::new("quick-launch-running-switch")
                        .label(nohrs_core::t!("Ir para ele"))
                        .on_click(|_, _window, cx| {
                            super::switch_to_running_program(cx);
                        }),
                ),
        );
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(0x00000080))
        .child(card)
        .into_any_element()
}

/// One input field with its opening text.
fn input_with(
    window: &mut Window,
    cx: &mut Context<QuickLaunchView>,
    text: &str,
) -> Entity<InputState> {
    input_with_placeholder(window, cx, text, "")
}

/// A field of the editor with the example `placeholder` it shows while empty.
fn input_with_placeholder(
    window: &mut Window,
    cx: &mut Context<QuickLaunchView>,
    text: &str,
    placeholder: &str,
) -> Entity<InputState> {
    let placeholder = placeholder.to_string();
    cx.new(|cx| {
        let input = InputState::new(window, cx).placeholder(placeholder);
        input.default_value(text.to_string())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A cell too small for the configured icon shrinks to the 16 px floor instead
    /// of panicking when the two bounds cross (`clamp` would).
    #[test]
    fn a_tiny_cell_never_takes_the_icon_below_its_floor() {
        assert_eq!(grid_icon_size(64, 20.0), 16.0);
        assert_eq!(grid_icon_size(64, 100.0), 64.0);
        assert_eq!(grid_icon_size(128, 100.0), 100.0 - NAME_LINE_HEIGHT - 12.0);
    }

    /// The list shows one more column for every minimum-width column the window
    /// has room for, and never more than its cap.
    #[test]
    fn the_list_shows_the_columns_that_fit_the_window() {
        assert_eq!(fit_list_columns(540.0), 2, "the default list window");
        assert_eq!(fit_list_columns(240.0), 1, "narrower than one column");
        assert_eq!(fit_list_columns(1224.0), 4, "a wide grid-like window");
        assert_eq!(fit_list_columns(10_000.0), 8, "the cap");
        assert_eq!(fit_list_columns(0.0), 1);
    }

    /// The hover grows only the icon, and only as far as the cell's name line
    /// leaves room; the cell itself stays the size it is.
    #[test]
    fn the_hover_zoom_stays_inside_the_cell() {
        let (icon_rest, icon_grown) = hover_icon_edges(64.0, 100.0);
        assert_eq!(icon_rest, 64.0);
        assert!(icon_grown > icon_rest);
        assert!(icon_grown + NAME_LINE_HEIGHT <= 100.0, "{icon_grown}");
        let (row_rest, row_grown) = hover_list_icon_edges(28.0);
        assert_eq!(row_rest, 28.0);
        assert!(row_grown <= LIST_ROW_HEIGHT && row_grown > row_rest);
    }

    /// The grid covers the window: as many cells of the configured size as fit,
    /// stretched so their rows and columns reach the edges, and no fewer than one.
    #[test]
    fn the_grid_fills_the_window() {
        let grid = grid_metrics(860.0, 800.0, 100.0);
        assert_eq!(grid.columns, 8);
        assert_eq!(grid.fill_rows, 7);
        let covered = grid.cell_width * 8.0 + CELL_GAP * 7.0;
        assert!((covered - 860.0).abs() < 0.001, "the cells fill the width");
        let covered = (grid.cell_height + CELL_GAP) * 7.0;
        assert!((covered - 800.0).abs() < 0.001, "the rows fill the height");
        assert!(grid.cell_width >= 100.0, "the cells stretch, not shrink");
        let narrow = grid_metrics(240.0, 100.0, 100.0);
        assert_eq!(narrow.columns, 2);
        assert_eq!(narrow.fill_rows, 1);
        assert_eq!(grid_metrics(10.0, 10.0, 100.0).columns, 1);
    }

    /// The icon is the configured size unless the cell is too small for it.
    #[test]
    fn the_icon_fits_the_cell_it_is_drawn_in() {
        assert_eq!(grid_icon_size(64, 100.0), 64.0);
        assert_eq!(grid_icon_size(64, 64.0), 34.0);
        assert_eq!(list_icon_size(64), 28.0);
    }

    fn item(slot: u32, name: &str) -> Item {
        Item::new(slot, "app", name)
    }

    #[test]
    fn the_grid_places_items_by_slot_and_adds_a_row_of_empty_cells() {
        let items = vec![item(0, "A"), item(3, "B")];
        let cells = grid_cells(&items, 2);
        // Slots 0..=3 need two rows, and §12.1 adds one more for new items.
        assert_eq!(cells.len(), 6);
        assert_eq!(cells[0].as_ref().map(|item| item.name.as_str()), Some("A"));
        assert!(cells[1].is_none(), "slot 1 is empty");
        assert!(cells[2].is_none(), "slot 2 is empty");
        assert_eq!(cells[3].as_ref().map(|item| item.name.as_str()), Some("B"));
    }

    #[test]
    fn a_filtered_grid_packs_the_matches_and_keeps_one_empty_row() {
        let matches = vec![item(2, "A"), item(7, "B"), item(9, "C")];
        let cells = packed_cells(&matches, 2);
        assert_eq!(cells.len(), 6, "two rows of matches and one padding row");
        assert_eq!(cells[0].as_ref().map(|item| item.slot), Some(2));
        assert_eq!(cells[1].as_ref().map(|item| item.slot), Some(7));
        assert_eq!(cells[2].as_ref().map(|item| item.slot), Some(9));
        assert!(cells[3..].iter().all(Option::is_none));
        assert_eq!(packed_cells(&[], 4).len(), 4, "an empty filter keeps a row");
    }

    #[test]
    fn a_group_with_nothing_in_it_still_gets_a_row() {
        let cells = grid_cells(&[], 8);
        assert_eq!(cells.len(), 8);
        assert!(cells.iter().all(Option::is_none));
    }

    #[test]
    fn a_slot_beyond_the_columns_starts_the_next_row() {
        let items = vec![item(8, "Nona")];
        let cells = grid_cells(&items, 8);
        // Slot 8 is the first cell of the second row; the extra row makes three.
        assert_eq!(cells.len(), 24);
        assert_eq!(
            cells[8].as_ref().map(|item| item.name.as_str()),
            Some("Nona")
        );
    }

    #[test]
    fn a_slot_past_the_last_one_does_not_grow_the_grid() {
        let items = vec![item(u32::MAX, "Tarde")];
        let cells = grid_cells(&items, 16);
        assert!(cells.len() >= 16);
        assert!(
            cells.iter().all(Option::is_none),
            "nothing fits past the end"
        );
    }

    #[test]
    fn a_group_shortcut_is_alt_1_to_9() {
        assert_eq!(group_shortcut("1"), Some(0));
        assert_eq!(group_shortcut("9"), Some(8));
        assert_eq!(group_shortcut("0"), None);
        assert_eq!(group_shortcut("10"), None);
        assert_eq!(group_shortcut("a"), None);
    }

    #[test]
    fn the_sizes_of_the_bar_and_the_rows_are_the_ones_the_specification_names() {
        assert_eq!(GROUP_BAR_WIDTH, 120.0);
        assert_eq!(TITLE_BAR_HEIGHT, 36.0);
    }
    /// A group menu's delete is refused for the last group, and every row of a
    /// read-only file is disabled (§12.3).
    #[test]
    fn the_group_menu_disables_what_the_file_cannot_do() {
        let rows = group_menu_rows(2, false, false);
        assert!(rows.iter().all(|row| row.disabled));
        let rows = group_menu_rows(2, true, true);
        let delete = rows
            .iter()
            .find(|row| row.id == Some(DELETE_GROUP))
            .expect("the delete row");
        assert!(delete.disabled, "the last group cannot be removed");
        let rows = group_menu_rows(0, true, false);
        let delete = rows
            .iter()
            .find(|row| row.id == Some(DELETE_GROUP))
            .expect("the delete row");
        assert!(!delete.disabled);
    }

    /// The empty cell's menu offers the folder entry only where the picker
    /// cannot take both (§17.2 R2).
    #[test]
    fn the_empty_cell_menu_follows_the_picker() {
        let rows = cell_menu_rows(true, true);
        assert_eq!(rows.len(), 1, "a mixed picker needs one entry");
        assert_eq!(rows[0].id, Some(NEW_ITEM));
        let rows = cell_menu_rows(false, false);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.disabled));
        assert_eq!(rows[1].id, Some(NEW_ITEM_FOLDER));
    }
}
