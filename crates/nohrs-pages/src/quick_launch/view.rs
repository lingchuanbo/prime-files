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
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, FocusHandle, ImageSource, IntoElement, KeyDownEvent,
    PathPromptOptions, Render, RenderImage, SharedString, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, WindowControlArea, div, img, px, rgb,
    uniform_list,
};
use gpui_component::{
    Icon, IconName, WindowExt as _,
    button::{Button, ButtonRounded, ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
};
use nohrs_core::config::quick_launch::{ActionKind, Item, MAX_SLOT, View};
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
    /// The item editor open over the window, if any (§12.3).
    editor: Option<Editor>,
    /// The group whose name is being edited in place, if any (§12.3).
    renaming: Option<Rename>,
    _activation: Subscription,
}

/// The four fields the edit dialog offers (§12.3).
struct Editor {
    group: usize,
    slot: u32,
    name: Entity<InputState>,
    path: Entity<InputState>,
    args: Entity<InputState>,
    /// "Run as administrator", toggled by the checkbox and read on confirm.
    admin: Rc<Cell<bool>>,
}

/// The in-place name editor of one group (§12.3).
struct Rename {
    group: usize,
    input: Entity<InputState>,
    _subscription: Subscription,
}

/// Everything a frame draws that does not change from frame to frame.
struct Frame {
    revision: u64,
    icon_edge: u32,
    groups: Rc<Vec<(String, bool)>>,
    drawn: Rc<Vec<Drawn>>,
    view: View,
    columns: usize,
    cell_size: f32,
    list_columns: usize,
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
            editor: None,
            renaming: None,
            _activation: subscription,
        }
    }

    /// Schedules the hide that follows losing the focus (§11).
    fn activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let columns = super::state(cx)
            .map(|quick_launch| i64::from(quick_launch.config().window.columns))
            .unwrap_or(8);
        if modifiers.alt
            && !modifiers.control
            && !modifiers.platform
            && let Some(index) = group_shortcut(&key)
        {
            super::select_group(cx, index);
            cx.stop_propagation();
            return;
        }
        let handled = match key.as_str() {
            // M1 has no filter, so Escape always hides (§12.2). The window is
            // already in hand, so the hide goes through it: asking GPUI for the
            // window it is currently updating fails with "window not found".
            "escape" => {
                super::hide_in(cx, window);
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
                    super::activate(cx, slot);
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
        let path = input_with(window, cx, item.path.as_deref().unwrap_or_default());
        let args = input_with(window, cx, item.args.as_deref().unwrap_or_default());
        self.editor = Some(Editor {
            group,
            slot,
            name: name.clone(),
            path,
            args,
            admin: Rc::new(Cell::new(item.admin)),
        });
        name.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Saves the four fields through the state layer (§12.3).
    fn confirm_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let name = editor.name.read(cx).value().to_string();
        let path = editor.path.read(cx).value().to_string();
        let args = editor.args.read(cx).value().to_string();
        let admin = editor.admin.get();
        super::edit_item(cx, editor.group, editor.slot, &name, &path, &args, admin);
        cx.notify();
    }

    /// Adds what the file picker returned to the slot the menu was opened on
    /// (§12.3, §12.2): the first path fills that slot, the rest follow.
    fn new_item_at(
        &mut self,
        group: usize,
        slot: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !super::editable(cx) {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some(SharedString::from(nohrs_core::t!(
                "Escolher um arquivo ou uma pasta"
            ))),
        });
        cx.spawn_in(window, async move |this, cx| {
            let picked = match receiver.await {
                Ok(Ok(Some(paths))) => paths,
                _ => return,
            };
            this.update_in(cx, |_view, _window, cx| {
                for (offset, path) in picked.iter().enumerate() {
                    let offset = u32::try_from(offset).unwrap_or(u32::MAX);
                    let target = slot.saturating_add(offset);
                    let item = super::item_from_path(path, target);
                    super::put_item(cx, group, target, item);
                }
                cx.notify();
            })
            .ok();
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
                let close = weak.clone();
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
                        let _ = confirm.update(cx, |_view, cx| {
                            super::delete_group(cx, group, true);
                            cx.notify();
                        });
                        true
                    })
                    .on_close(move |_, _window, cx| {
                        let _ = close.update(cx, |_view, _cx| {});
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
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from(nohrs_core::t!("Escolher um caminho"))),
        });
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

    /// A click selects the cell and starts what is in it (§12.2).
    fn clicked(&mut self, slot: u32, cx: &mut Context<Self>) {
        if let Some(quick_launch) = super::state_mut(cx) {
            quick_launch.slot = Some(slot);
        }
        super::activate(cx, slot);
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
                    .map(|group| group.items.clone())
                    .unwrap_or_default();
                (
                    groups,
                    items,
                    config.window.view,
                    usize::from(config.window.columns.max(1)),
                    f32::from(config.window.cell_size),
                    usize::from(config.window.list_columns.max(1)),
                )
            };
            let (groups, items, view, columns, cell_size, list_columns) = planned;
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
                columns,
                cell_size,
                list_columns,
            });
        }
        let Some(frame) = self.frame.as_ref() else {
            return div().size_full();
        };
        let groups = frame.groups.clone();
        let drawn = frame.drawn.clone();
        let view = frame.view;
        let columns = frame.columns;
        let cell_size = frame.cell_size;
        let list_columns = frame.list_columns;

        let title_bar = self.render_title_bar();
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
        } else if drawn.is_empty() {
            centered_note(nohrs_core::t!("Nenhum item neste grupo"))
        } else if view == View::List {
            list_view(
                &drawn,
                selected,
                group,
                editable,
                &missing,
                list_columns,
                icon_edge,
                &entity,
                &self.scroll_handle,
            )
        } else {
            grid_view(
                &drawn,
                selected,
                group,
                editable,
                &missing,
                columns,
                cell_size,
                icon_edge,
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
                        .child(content),
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
        if let Some(overlay) = editor_overlay(self.editor.as_ref(), &entity, cx) {
            root = root.child(overlay);
        }
        root
    }
}

impl QuickLaunchView {
    /// The title bar: the drag area, the title, and the "⋯" menu (§15.0).
    fn render_title_bar(&self) -> AnyElement {
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
                    .child(
                        Button::new("quick-launch-menu")
                            .icon(Icon::new(IconName::Ellipsis).size_4())
                            .rounded(ButtonRounded::Large)
                            .with_variant(ButtonVariant::Ghost)
                            .dropdown_menu(|menu, _window, _cx| {
                                menu.min_w(px(220.0))
                                    .item(
                                        PopupMenuItem::new(nohrs_core::t!("Mostrar janela"))
                                            .on_click(|_, _window, _cx| {
                                                crate::tray::request_show();
                                            }),
                                    )
                                    .separator()
                                    .item(
                                        PopupMenuItem::new(nohrs_core::t!("Sair do LumaFiles"))
                                            .on_click(|_, _window, _cx| {
                                                crate::tray::request_quit();
                                            }),
                                    )
                            }),
                    ),
            )
            .into_any_element()
    }
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

/// The picture of one item: the extracted icon, or the type's icon.
fn picture(item: &Item, image: Option<&Arc<RenderImage>>, edge: u32) -> AnyElement {
    match image {
        Some(image) => img(ImageSource::Render(image.clone()))
            .size(px(edge as f32))
            .into_any_element(),
        None => type_icon(item).size(px(edge as f32)).into_any_element(),
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
        let menu_entity = entity.clone();
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
                .context_menu(move |menu, _window, _cx| {
                    let add = menu_entity.clone();
                    let rename = menu_entity.clone();
                    let delete = menu_entity.clone();
                    menu.item(
                        PopupMenuItem::new(nohrs_core::t!("Novo grupo"))
                            .disabled(!editable)
                            .on_click(move |_, window, cx| {
                                let _ = add.update(cx, |view, cx| view.add_group(window, cx));
                            }),
                    )
                    .item(
                        PopupMenuItem::new(nohrs_core::t!("Renomear"))
                            .disabled(!editable)
                            .on_click(move |_, window, cx| {
                                let _ = rename
                                    .update(cx, |view, cx| view.start_rename(index, window, cx));
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new(nohrs_core::t!("Excluir"))
                            .disabled(!editable || last_group)
                            .on_click(move |_, window, cx| {
                                let _ = delete
                                    .update(cx, |view, cx| view.delete_group(index, window, cx));
                            }),
                    )
                })
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

/// The grid (§12.1), virtualized by row.
#[allow(clippy::too_many_arguments)]
fn grid_view(
    drawn: &[Drawn],
    selected: Option<u32>,
    group: usize,
    editable: bool,
    missing: &HashMap<u32, bool>,
    columns: usize,
    cell_size: f32,
    icon_edge: u32,
    entity: &WeakEntity<QuickLaunchView>,
    scroll_handle: &UniformListScrollHandle,
) -> AnyElement {
    let items: Vec<Item> = drawn.iter().map(|drawn| drawn.item.clone()).collect();
    let cells = grid_cells(&items, columns);
    let rows = cells.len().div_ceil(columns.max(1));
    let cells = Rc::new(cells);
    let drawn = Rc::new(drawn.to_vec());
    // The icon of a cell is the one the item got; the cell index is the slot.
    let pictures_by_slot: HashMap<u32, Option<Arc<RenderImage>>> = drawn
        .iter()
        .map(|entry| (entry.item.slot, entry.image.clone()))
        .collect();
    let pictures_by_slot = Rc::new(pictures_by_slot);
    let missing = Rc::new(missing.clone());
    let entity = entity.clone();
    uniform_list(
        "quick-launch-grid",
        rows,
        move |range: Range<usize>, _window, _cx| {
            let mut lines = Vec::with_capacity(range.len());
            for row in range {
                let mut line = div()
                    .h(px(cell_size))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1();
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
                                item, image, selected, missing, group, editable, cell_size,
                                icon_edge, &entity,
                            ));
                        }
                        None => {
                            let slot = u32::try_from(index).unwrap_or(MAX_SLOT);
                            line =
                                line.child(empty_cell(slot, group, editable, cell_size, &entity));
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
    group: usize,
    editable: bool,
    missing: &HashMap<u32, bool>,
    list_columns: usize,
    icon_edge: u32,
    entity: &WeakEntity<QuickLaunchView>,
    scroll_handle: &UniformListScrollHandle,
) -> AnyElement {
    let list_columns = list_columns.max(1);
    let rows = drawn.len().div_ceil(list_columns);
    let drawn = Rc::new(drawn.to_vec());
    let missing = Rc::new(missing.clone());
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
                                entry, selected, missing, group, editable, icon_edge, &entity,
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
    editable: bool,
    cell_size: f32,
    entity: &WeakEntity<QuickLaunchView>,
) -> AnyElement {
    let handle = entity.clone();
    div()
        .id(("quick-launch-empty", slot))
        .w(px(cell_size))
        .h(px(cell_size))
        .flex_shrink_0()
        .rounded(px(8.0))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .context_menu(move |menu, _window, _cx| {
            let handle = handle.clone();
            menu.item(
                PopupMenuItem::new(nohrs_core::t!("Novo item"))
                    .disabled(!editable)
                    .on_click(move |_, window, cx| {
                        let _ =
                            handle.update(cx, |view, cx| view.new_item_at(group, slot, window, cx));
                    }),
            )
        })
        .into_any_element()
}

/// The menu of one item (§12.2, M2 rows).
fn item_menu(
    menu: PopupMenu,
    group: usize,
    slot: u32,
    editable: bool,
    entity: &WeakEntity<QuickLaunchView>,
) -> PopupMenu {
    let edit = entity.clone();
    let delete = entity.clone();
    menu.item(
        PopupMenuItem::new(nohrs_core::t!("Editar"))
            .disabled(!editable)
            .on_click(move |_, window, cx| {
                let _ = edit.update(cx, |view, cx| view.edit_item(group, slot, window, cx));
            }),
    )
    .separator()
    .item(
        PopupMenuItem::new(nohrs_core::t!("Excluir"))
            .disabled(!editable)
            .on_click(move |_, _window, cx| {
                let _ = delete.update(cx, |view, cx| view.delete_item(group, slot, cx));
            }),
    )
}

/// One grid tile.
#[allow(clippy::too_many_arguments)]
fn tile(
    item: &Item,
    image: Option<Arc<RenderImage>>,
    selected: bool,
    missing: bool,
    group: usize,
    editable: bool,
    cell_size: f32,
    icon_edge: u32,
    entity: &WeakEntity<QuickLaunchView>,
) -> AnyElement {
    let handle = entity.clone();
    let menu_handle = entity.clone();
    let slot = item.slot;
    div()
        .id(("quick-launch-tile", slot))
        .w(px(cell_size))
        .h(px(cell_size))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .rounded(px(8.0))
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme::accent_subtle())))
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .on_click(move |_, _window, cx| {
            let _ = handle.update(cx, |view, cx| view.clicked(slot, cx));
        })
        .context_menu(move |menu, _window, _cx| {
            item_menu(menu, group, slot, editable, &menu_handle)
        })
        .child(picture(item, image.as_ref(), icon_edge))
        .child(
            div()
                .max_w(px(cell_size - 6.0))
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
fn row_of(
    drawn: &Drawn,
    selected: bool,
    missing: bool,
    group: usize,
    editable: bool,
    icon_edge: u32,
    entity: &WeakEntity<QuickLaunchView>,
) -> AnyElement {
    let handle = entity.clone();
    let menu_handle = entity.clone();
    let item = &drawn.item;
    let slot = item.slot;
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
        .on_click(move |_, _window, cx| {
            let _ = handle.update(cx, |view, cx| view.clicked(slot, cx));
        })
        .context_menu(move |menu, _window, _cx| {
            item_menu(menu, group, slot, editable, &menu_handle)
        })
        .child(picture(item, drawn.image.as_ref(), icon_edge))
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

/// The editor card: the four fields of §12.3 and its two buttons.
///
/// It is drawn inside the window instead of through `window.open_dialog` because
/// "浏览…" has to put the picked path into an input field after an async platform
/// call, and only the view can hand out the window for that.
fn editor_overlay(
    editor: Option<&Editor>,
    entity: &WeakEntity<QuickLaunchView>,
    cx: &mut Context<QuickLaunchView>,
) -> Option<AnyElement> {
    let _ = cx;
    let editor = editor?;
    let name = editor.name.clone();
    let path = editor.path.clone();
    let args = editor.args.clone();
    let admin = editor.admin.clone();
    let admin_value = admin.get();
    let browse = entity.clone();
    let confirm = entity.clone();
    let cancel = entity.clone();
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
                    .child(field(nohrs_core::t!("Nome"), &name))
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .child(field(nohrs_core::t!("Caminho"), &path)),
                            )
                            .child(
                                Button::new("quick-launch-browse")
                                    .label(nohrs_core::t!("Procurar…"))
                                    .on_click(move |_, window, cx| {
                                        let _ = browse.update(cx, |view, cx| {
                                            view.browse_for_path(window, cx);
                                        });
                                    }),
                            ),
                    )
                    .child(field(nohrs_core::t!("Argumentos"), &args))
                    .child(
                        Checkbox::new("quick-launch-admin")
                            .label(nohrs_core::t!("Executar como administrador"))
                            .checked(admin_value)
                            .on_click(move |checked, _window, cx| {
                                admin.set(*checked);
                                cx.refresh_windows();
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("quick-launch-cancel")
                                    .label(nohrs_core::t!("Cancelar"))
                                    .on_click(move |_, _window, cx| {
                                        let _ = cancel.update(cx, |view, cx| {
                                            view.editor = None;
                                            cx.notify();
                                        });
                                    }),
                            )
                            .child(
                                Button::new("quick-launch-confirm")
                                    .label(nohrs_core::t!("Confirmar"))
                                    .on_click(move |_, _window, cx| {
                                        let _ = confirm.update(cx, |view, cx| {
                                            view.confirm_editor(cx);
                                        });
                                    }),
                            ),
                    ),
            )
            .into_any_element(),
    )
}

/// One input field with its opening text.
fn input_with(
    window: &mut Window,
    cx: &mut Context<QuickLaunchView>,
    text: &str,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).default_value(text.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
}
