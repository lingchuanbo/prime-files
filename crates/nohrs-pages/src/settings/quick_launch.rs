//! The "Quick Launch" section of the settings page (§13, §17.1 B).
//!
//! Four cards: the window (the summon chord and the layout), the groups, the
//! form that edits one entry, and the file itself. Everything here writes the
//! same [`crate::quick_launch`] global the floating window draws, so an edit on
//! either side is visible on the other at the next frame; the file is written by
//! the state's own throttle, never from this module.
//!
//! When `quicklaunch.toml` cannot be written back, every control of the first
//! three cards is greyed and inert, and the file card opens with the reason.

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Div, ElementId, Entity, KeyDownEvent, PathPromptOptions, SharedString,
    Stateful, Window, div, px, rgb,
};
use gpui_component::Sizable as _;
use gpui_component::input::{Input, InputState};
use nohrs_core::config::paths;
use nohrs_core::config::quick_launch::{
    Group as LaunchGroup, Item, OnRunning, View, WindowSettings,
};
use nohrs_core::shortcuts::Chord;
use nohrs_core::telemetry::LogErr;
use nohrs_core::{t, tf};
use nohrs_services::launch::{
    ComApartment, ExpandContext, InstalledApp, KIND_COMMAND, KIND_UWP, LaunchMode, LaunchRegistry,
    LaunchRequest, installed_apps,
};
use nohrs_ui::theme::theme;

use super::{Pick, Row, SettingsPage};
use crate::quick_launch;

/// The grid column counts offered, inside the `2..=16` of §4.
const COLUMNS: [(&str, u8); 7] = [
    ("2", 2),
    ("4", 4),
    ("6", 6),
    ("8", 8),
    ("10", 10),
    ("12", 12),
    ("16", 16),
];

/// The cell sizes offered, inside the `64..=160` of §4.
const CELL_SIZES: [(&str, u16); 7] = [
    ("64", 64),
    ("80", 80),
    ("96", 96),
    ("112", 112),
    ("128", 128),
    ("144", 144),
    ("160", 160),
];

/// The icon sizes offered; the file accepts only these (§4).
const ICON_SIZES: [(&str, u16); 6] = [
    ("16", 16),
    ("24", 24),
    ("32", 32),
    ("48", 48),
    ("64", 64),
    ("96", 96),
];

/// The list-view column counts offered, inside the `1..=3` of §4.
const LIST_COLUMNS: [(&str, u8); 3] = [("1", 1), ("2", 2), ("3", 3)];

/// What one click on the section does.
#[derive(Clone, Copy)]
pub(super) enum Action {
    /// A new layout for the grid.
    View(View),
    /// A new grid column count.
    Columns(u8),
    /// A new grid cell edge.
    CellSize(u16),
    /// A new icon edge.
    IconSize(u16),
    /// A new list column count.
    ListColumns(u8),
    /// Hide the window when it loses focus, or stop hiding it.
    AutoHide(bool),
    /// Hide the window after a launch, or keep it.
    CloseAfterLaunch(bool),
    /// Pin the window, or let it go.
    Topmost(bool),
    /// What to do when the program is already running.
    OnRunning(OnRunning),
    /// Keep the process alive after the main window closes.
    KeepRunning(bool),
    /// Wait for the next key press to become the summon chord.
    RecordHotkey,
    /// Add a group at the end and start its rename.
    NewGroup,
    /// Start renaming the group at this index.
    RenameGroup(usize),
    /// Write the renamed group back.
    SaveRename,
    /// Leave the group's name as it was.
    CancelRename,
    /// Delete the group at this index, asking once when it has items.
    DeleteGroup(usize),
    /// Move the group at this index one place up.
    MoveGroupUp(usize),
    /// Move the group at this index one place down.
    MoveGroupDown(usize),
    /// Show the group at this index in the item editor.
    SelectGroup(usize),
    /// Open the form of one item of the chosen group.
    SelectItem(u32),
    /// Change the kind of the item being edited.
    SetKind(&'static str),
    /// Include the item in `slot` in the group being edited, or leave it out.
    ToggleGroupItem(u32),
    /// Open the installed-app picker of an `uwp` item, or close it.
    ChooseApp,
    /// Fill the form with the installed app at this index.
    PickApp(usize),
    /// Flip "run as administrator" of the item being edited.
    ToggleAdmin,
    /// Change the shell of the command being edited.
    SetShell(&'static str),
    /// Flip "run without a window" of the command being edited.
    ToggleHidden,
    /// Flip "keep the console open" of the command being edited.
    ToggleKeepOpen,
    /// Remove the trust record of the command being edited.
    RevokeTrust,
    /// Write the item form back.
    SaveItem,
    /// Open `quicklaunch.toml` in its own program.
    OpenFile,
    /// Show `quicklaunch.toml` in LumaFiles.
    RevealFile,
}

/// The section's own state: which item is being edited, which group is being
/// renamed, and the key recording that the page's `record_key` hands to.
#[derive(Default)]
pub(super) struct Section {
    /// The group the item editor is on, by id: a reorder does not move it.
    group: Option<String>,
    /// The item waiting for a window to make its fields.
    select: Option<(String, u32)>,
    /// The form of the item being edited.
    form: Option<Form>,
    /// The group whose name is being edited, by id.
    renaming: Option<String>,
    /// The field of that group, made on the first frame after the click.
    rename_input: Option<Entity<InputState>>,
    /// The group whose deletion waits for a second click, by id.
    confirm_delete: Option<String>,
    /// Waiting for the next key press to become the summon chord.
    recording: bool,
    /// Whether the `uwp` app picker is open.
    apps_picker: bool,
    /// The installed apps, once the background enumeration has answered.
    apps: Option<Vec<InstalledApp>>,
    /// Why the enumeration failed, while there is still no list.
    apps_error: Option<String>,
    /// Whether the background enumeration is running.
    apps_loading: bool,
    /// The picker's search field, made on the first frame after it opens.
    apps_query: Option<Entity<InputState>>,
    /// The app a click chose, waiting for the next frame to fill the fields.
    picked_app: Option<InstalledApp>,
}

impl Section {
    /// Whether a key press belongs to the summon chord rather than to a
    /// command's shortcut.
    pub(super) fn is_recording(&self) -> bool {
        self.recording
    }

    /// The groups as cards of rows, for the search field and the section.
    pub(super) fn groups(
        &self,
        page: &SettingsPage,
        cx: &mut Context<SettingsPage>,
    ) -> Vec<super::Group> {
        let window = quick_launch::window_settings(cx);
        let groups = quick_launch::groups(cx);
        let enabled = quick_launch::editable(cx);
        let read_only = quick_launch::read_only_reason(cx).map(str::to_string);
        vec![
            self.window_group(page, &window, enabled, cx),
            self.groups_group(&groups, enabled, cx),
            self.items_group(page, &groups, enabled, cx),
            self.file_group(&read_only, cx),
        ]
    }

    /// The group the item editor shows, resolved by id so a reorder does not
    /// change it.
    fn chosen_group(&self, groups: &[LaunchGroup]) -> Option<usize> {
        match &self.group {
            Some(id) => groups.iter().position(|group| &group.id == id),
            None => (!groups.is_empty()).then_some(0),
        }
    }

    /// The window card: the chord first, then the layout of the grid or list.
    fn window_group(
        &self,
        page: &SettingsPage,
        window: &WindowSettings,
        enabled: bool,
        cx: &mut Context<SettingsPage>,
    ) -> super::Group {
        let view_options = [
            (
                t!("Grade"),
                window.view == View::Grid,
                Pick::QuickLaunch(Action::View(View::Grid)),
            ),
            (
                t!("Lista"),
                window.view == View::List,
                Pick::QuickLaunch(Action::View(View::List)),
            ),
        ];
        let running_options = [
            (
                t!("Ativar"),
                window.on_running == OnRunning::Activate,
                Pick::QuickLaunch(Action::OnRunning(OnRunning::Activate)),
            ),
            (
                t!("Abrir outra vez"),
                window.on_running == OnRunning::New,
                Pick::QuickLaunch(Action::OnRunning(OnRunning::New)),
            ),
        ];
        let rows = vec![
            Row::hint(
                t!("Atalho global"),
                t!("Clique e aperte a combinação que abre a inicialização rápida de qualquer programa; Esc desiste."),
                self.hotkey_control(window, enabled, cx),
            )
            .wide(),
            Row::new(
                t!("Exibir"),
                None,
                page.chips_when(
                    "quick-launch-view",
                    &view_options,
                    false,
                    enabled,
                    cx,
                ),
            ),
            Row::new(
                t!("Colunas"),
                None,
                page.chips_when(
                    "quick-launch-columns",
                    &numbered(&COLUMNS, window.columns, Action::Columns),
                    false,
                    enabled,
                    cx,
                ),
            ),
            Row::new(
                t!("Tamanho da célula"),
                None,
                page.chips_when(
                    "quick-launch-cell-size",
                    &numbered(&CELL_SIZES, window.cell_size, Action::CellSize),
                    false,
                    enabled,
                    cx,
                ),
            ),
            Row::new(
                t!("Tamanho do ícone"),
                None,
                page.chips_when(
                    "quick-launch-icon-size",
                    &numbered(&ICON_SIZES, window.icon_size, Action::IconSize),
                    false,
                    enabled,
                    cx,
                ),
            ),
            Row::new(
                t!("Colunas da lista"),
                None,
                page.chips_when(
                    "quick-launch-list-columns",
                    &numbered(&LIST_COLUMNS, window.list_columns, Action::ListColumns),
                    false,
                    enabled,
                    cx,
                ),
            ),
            Row::new(
                t!("Esconder ao perder o foco"),
                None,
                page.switch_pick_when(
                    "quick-launch-auto-hide",
                    window.auto_hide,
                    Pick::QuickLaunch(Action::AutoHide(!window.auto_hide)),
                    enabled,
                    cx,
                ),
            ),
            Row::new(
                t!("Fechar depois de abrir"),
                None,
                page.switch_pick_when(
                    "quick-launch-close-after-launch",
                    window.close_after_launch,
                    Pick::QuickLaunch(Action::CloseAfterLaunch(!window.close_after_launch)),
                    enabled,
                    cx,
                ),
            ),
            Row::hint(
                t!("Manter na frente"),
                t!("Fixar: a janela não se esconde e fica na frente"),
                page.switch_pick_when(
                    "quick-launch-topmost",
                    window.topmost,
                    Pick::QuickLaunch(Action::Topmost(!window.topmost)),
                    enabled,
                    cx,
                ),
            ),
            Row::new(
                t!("Quando o programa já está aberto"),
                None,
                page.chips_when(
                    "quick-launch-on-running",
                    &running_options,
                    false,
                    enabled,
                    cx,
                ),
            ),
            Row::hint(
                t!("Continuar em segundo plano"),
                t!("Depois de fechar a janela principal, o processo continua rodando para o atalho continuar funcionando."),
                page.switch_pick_when(
                    "quick-launch-keep-running",
                    window.keep_running,
                    Pick::QuickLaunch(Action::KeepRunning(!window.keep_running)),
                    enabled,
                    cx,
                ),
            ),
        ];
        super::Group::titled(t!("Janela"), rows)
    }

    /// The chord control: what is registered, the recording state, and why the
    /// last registration failed.
    fn hotkey_control(
        &self,
        window: &WindowSettings,
        enabled: bool,
        cx: &mut Context<SettingsPage>,
    ) -> AnyElement {
        let shown = if self.recording {
            t!("Aperte as teclas…").to_string()
        } else if window.hotkey.trim().is_empty() {
            t!("Nenhum").to_string()
        } else {
            window.hotkey.clone()
        };
        let button = div()
            .id("quick-launch-hotkey")
            .min_w(px(170.0))
            .h(px(28.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .border_1()
            .text_sm()
            .whitespace_nowrap()
            .map(|this| {
                if self.recording {
                    this.border_color(rgb(theme::accent()))
                        .bg(rgb(theme::accent_subtle()))
                        .text_color(rgb(theme::accent()))
                } else {
                    this.border_color(rgb(theme::border_hover()))
                        .bg(rgb(theme::bg()))
                        .text_color(rgb(theme::fg()))
                        .hover(|style| style.bg(rgb(theme::bg_hover())))
                }
            })
            .when(enabled, |this| {
                this.cursor_pointer()
                    .on_click(cx.listener(|page, _event, window, cx| {
                        window.focus(&page.focus_handle);
                        page.pick(Pick::QuickLaunch(Action::RecordHotkey), cx);
                    }))
            })
            .when(!enabled, |this| this.opacity(0.45))
            .child(shown);
        let mut control = div().flex().flex_col().items_end().gap_1().child(button);
        if quick_launch::hotkey_error(cx).is_some() {
            if quick_launch::hotkey_conflicts_with_launcher(cx) {
                control =
                    control.child(status_line(t!("Conflita com o atalho do buscador."), true));
            } else {
                control = control.child(status_line(
                    t!("Não foi possível registrar o atalho."),
                    true,
                ));
                if let Some(error) = quick_launch::hotkey_error(cx) {
                    control = control.child(status_line(error.to_string(), true));
                }
            }
        } else if quick_launch::registered_hotkey(cx).is_some() {
            control = control.child(status_line(t!("Registrado"), false));
        }
        control.into_any_element()
    }

    /// The groups card: one line per group with rename, delete and move, and
    /// the button that adds another.
    fn groups_group(
        &self,
        groups: &[LaunchGroup],
        enabled: bool,
        cx: &mut Context<SettingsPage>,
    ) -> super::Group {
        let mut entries: Vec<AnyElement> = groups
            .iter()
            .enumerate()
            .map(|(index, group)| self.group_entry(group, index, groups.len(), enabled, cx))
            .collect();
        entries.push(
            action_button(
                "quick-launch-group-add",
                t!("Novo grupo"),
                false,
                enabled,
                Action::NewGroup,
                cx,
            )
            .into_any_element(),
        );
        let control = div().flex().flex_col().gap_1().w_full().children(entries);
        super::Group::new(vec![Row::new(t!("Grupos"), None, control).wide()])
    }

    /// One group line. While it is being renamed, the line is the field and
    /// the two buttons; a non-empty group asks once before it is deleted.
    fn group_entry(
        &self,
        group: &LaunchGroup,
        index: usize,
        count: usize,
        enabled: bool,
        cx: &mut Context<SettingsPage>,
    ) -> AnyElement {
        if self.renaming.as_deref() == Some(group.id.as_str()) {
            let Some(input) = self.rename_input.as_ref() else {
                return div().into_any_element();
            };
            return div()
                .flex()
                .items_center()
                .gap_2()
                .w_full()
                .child(Input::new(input).small().w(px(240.0)).into_any_element())
                .child(action_button(
                    "quick-launch-rename-save",
                    t!("Salvar"),
                    false,
                    enabled,
                    Action::SaveRename,
                    cx,
                ))
                .child(action_button(
                    "quick-launch-rename-cancel",
                    t!("Cancelar"),
                    false,
                    enabled,
                    Action::CancelRename,
                    cx,
                ))
                .into_any_element();
        }
        let confirming = self.confirm_delete.as_deref() == Some(group.id.as_str());
        let delete_label = if confirming {
            t!("Confirmar exclusão")
        } else {
            t!("Excluir")
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .py_1()
            .w_full()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_sm()
                    .text_color(rgb(theme::fg()))
                    .child(group.name.clone()),
            )
            .child(action_button(
                ElementId::Name(SharedString::from(format!(
                    "quick-launch-group-up-{}",
                    group.id
                ))),
                "↑",
                false,
                enabled && index > 0,
                Action::MoveGroupUp(index),
                cx,
            ))
            .child(action_button(
                ElementId::Name(SharedString::from(format!(
                    "quick-launch-group-down-{}",
                    group.id
                ))),
                "↓",
                false,
                enabled && index + 1 < count,
                Action::MoveGroupDown(index),
                cx,
            ))
            .child(action_button(
                ElementId::Name(SharedString::from(format!(
                    "quick-launch-group-rename-{}",
                    group.id
                ))),
                t!("Renomear"),
                false,
                enabled,
                Action::RenameGroup(index),
                cx,
            ))
            .child(action_button(
                ElementId::Name(SharedString::from(format!(
                    "quick-launch-group-delete-{}",
                    group.id
                ))),
                delete_label,
                confirming,
                enabled && count > 1,
                Action::DeleteGroup(index),
                cx,
            ))
            .into_any_element()
    }

    /// The items card: the group choice, the items of that group, then the form
    /// of the selected one.
    fn items_group(
        &self,
        page: &SettingsPage,
        groups: &[LaunchGroup],
        enabled: bool,
        cx: &mut Context<SettingsPage>,
    ) -> super::Group {
        let chosen = self.chosen_group(groups);
        let Some(index) = chosen else {
            return super::Group::new(vec![Row::new(
                t!("Itens"),
                None,
                status_line(t!("Nenhum"), false),
            )]);
        };
        let Some(group) = groups.get(index) else {
            return super::Group::new(vec![Row::new(
                t!("Itens"),
                None,
                status_line(t!("Nenhum"), false),
            )]);
        };
        let mut rows = vec![
            Row::new(
                t!("Grupo"),
                None,
                self.group_tabs(groups, chosen, enabled, cx),
            )
            .wide(),
        ];
        let mut list = self.item_list(group, enabled, cx);
        if group.items.is_empty() {
            list = list.child(status_line(t!("Nenhum item neste grupo."), false));
        } else if self.form.is_none() {
            list = list.child(status_line(t!("Selecione um item para editar."), false));
        }
        rows.push(Row::new(t!("Itens"), None, list).wide());
        if let Some(form) = self.form.as_ref().filter(|form| form.group == group.id) {
            rows.extend(self.form_rows(page, form, enabled, cx));
        }
        super::Group::titled(t!("Itens"), rows)
    }

    /// The chips that choose which group the item editor shows. The names are
    /// the author's, so this is not the shared chip row: its labels are static.
    fn group_tabs(
        &self,
        groups: &[LaunchGroup],
        chosen: Option<usize>,
        enabled: bool,
        cx: &mut Context<SettingsPage>,
    ) -> AnyElement {
        if groups.is_empty() {
            return status_line(t!("Nenhum"), false).into_any_element();
        }
        div()
            .flex()
            .flex_wrap()
            .gap_1()
            .children(groups.iter().enumerate().map(|(index, group)| {
                let selected = chosen == Some(index);
                let pick = Pick::QuickLaunch(Action::SelectGroup(index));
                div()
                    .id(ElementId::Name(SharedString::from(format!(
                        "quick-launch-group-tab-{}",
                        group.id
                    ))))
                    .h(px(26.0))
                    .px(px(11.0))
                    .flex()
                    .items_center()
                    .rounded(px(5.0))
                    .text_sm()
                    .whitespace_nowrap()
                    .when(enabled, |this| this.cursor_pointer())
                    .map(|this| {
                        if selected {
                            this.bg(rgb(theme::accent()))
                                .text_color(rgb(theme::accent_fg()))
                        } else {
                            this.text_color(rgb(theme::fg_secondary())).hover(|style| {
                                style
                                    .bg(rgb(theme::bg_hover()))
                                    .text_color(rgb(theme::fg()))
                            })
                        }
                    })
                    .when(enabled, |this| {
                        this.on_click(
                            cx.listener(move |page, _event, _window, cx| {
                                page.pick(pick.clone(), cx)
                            }),
                        )
                    })
                    .when(!enabled, |this| this.opacity(0.45))
                    .child(group.name.clone())
            }))
            .into_any_element()
    }

    /// The entries of one group: name, kind and path, clickable to edit.
    fn item_list(&self, group: &LaunchGroup, enabled: bool, cx: &mut Context<SettingsPage>) -> Div {
        let list = div().flex().flex_col().gap_1().w_full();
        if group.items.is_empty() {
            return list;
        }
        list.children(group.items.iter().map(|item| {
            let selected = self
                .form
                .as_ref()
                .is_some_and(|form| form.group == group.id && form.slot == item.slot);
            let pick = Pick::QuickLaunch(Action::SelectItem(item.slot));
            let path = item.path.clone().unwrap_or_default();
            div()
                .id(ElementId::Name(SharedString::from(format!(
                    "quick-launch-item-{}-{}",
                    group.id, item.slot
                ))))
                .flex()
                .items_center()
                .gap_2()
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .map(|this| {
                    if selected {
                        this.bg(rgb(theme::accent_subtle()))
                            .text_color(rgb(theme::accent()))
                    } else {
                        this.text_color(rgb(theme::fg()))
                            .hover(|style| style.bg(rgb(theme::bg_hover())))
                    }
                })
                .when(enabled, |this| this.cursor_pointer())
                .when(enabled, |this| {
                    this.on_click(
                        cx.listener(move |page, _event, _window, cx| page.pick(pick.clone(), cx)),
                    )
                })
                .when(!enabled, |this| this.opacity(0.45))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_sm()
                        .child(item.name.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .max_w(px(280.0))
                        .text_xs()
                        .whitespace_nowrap()
                        .text_color(rgb(theme::fg_secondary()))
                        .child(format!("{} · {path}", item.kind)),
                )
        }))
    }

    /// The rows of the item form (§17.1 B, §17.2 B, §17.2 D): kind, name, the
    /// fields of the kind with browsing, elevation, icon and tags. An `uwp` item
    /// names an AppUserModelID instead of a path, picks it from the Apps folder,
    /// and has no working directory or elevation. A `command` item has no path
    /// or arguments, shows its shell, script and two flags instead, and carries
    /// its trust record (§17.2 D.6, D.7).
    fn form_rows(
        &self,
        page: &SettingsPage,
        form: &Form,
        enabled: bool,
        cx: &mut Context<SettingsPage>,
    ) -> Vec<Row> {
        let kind_control = if matches!(
            form.kind.as_str(),
            "app" | "folder" | "file" | "url" | KIND_UWP | KIND_COMMAND | quick_launch::KIND_GROUP
        ) {
            let options = [
                (
                    t!("Aplicativo"),
                    form.kind == "app",
                    Pick::QuickLaunch(Action::SetKind("app")),
                ),
                (
                    t!("Pasta"),
                    form.kind == "folder",
                    Pick::QuickLaunch(Action::SetKind("folder")),
                ),
                (
                    t!("Arquivo"),
                    form.kind == "file",
                    Pick::QuickLaunch(Action::SetKind("file")),
                ),
                (
                    t!("URL"),
                    form.kind == "url",
                    Pick::QuickLaunch(Action::SetKind("url")),
                ),
                (
                    t!("Aplicativo UWP"),
                    form.kind == KIND_UWP,
                    Pick::QuickLaunch(Action::SetKind(KIND_UWP)),
                ),
                (
                    t!("Comando"),
                    form.kind == KIND_COMMAND,
                    Pick::QuickLaunch(Action::SetKind(KIND_COMMAND)),
                ),
                (
                    t!("Grupo"),
                    form.kind == quick_launch::KIND_GROUP,
                    Pick::QuickLaunch(Action::SetKind(quick_launch::KIND_GROUP)),
                ),
            ];
            page.chips_when("quick-launch-item-kind", &options, false, enabled, cx)
        } else {
            // A kind this milestone cannot build (a plugin) is shown as it is:
            // the form cannot offer it, and changing it would change what the
            // entry means.
            status_line(form.kind.clone(), false)
        };
        let uwp = form.kind == KIND_UWP;
        let command = form.kind == KIND_COMMAND;
        let group = form.kind == quick_launch::KIND_GROUP;
        let path_label = if uwp {
            t!("ID do aplicativo")
        } else {
            t!("Caminho")
        };
        let cwd_enabled = enabled && !uwp;
        let mut rows = vec![
            Row::new(t!("Tipo"), None, kind_control),
            Row::new(t!("Nome"), None, input_row(&form.name, enabled)).wide(),
        ];
        if !command && !group {
            rows.push(
                Row::new(
                    path_label,
                    None,
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .child(input_row(&form.path, enabled))
                        .child(if uwp {
                            action_button(
                                "quick-launch-choose-app",
                                t!("Escolher aplicativo…"),
                                false,
                                enabled,
                                Action::ChooseApp,
                                cx,
                            )
                            .into_any_element()
                        } else {
                            browse_button(Field::Path, enabled, cx)
                        }),
                )
                .wide(),
            );
            if uwp && self.apps_picker {
                rows.push(
                    Row::new(t!("Aplicativo"), None, self.apps_picker_row(enabled, cx)).wide(),
                );
            }
            rows.push(Row::new(t!("Argumentos"), None, input_row(&form.args, enabled)).wide());
        }
        if group {
            rows.push(
                Row::hint(
                    t!("Incluir itens"),
                    t!("A ordem dos cliques é a ordem de início."),
                    self.group_picker(form, enabled, cx),
                )
                .wide(),
            );
        }
        if command {
            let shell_options = [
                (
                    "cmd",
                    form.shell == "cmd",
                    Pick::QuickLaunch(Action::SetShell("cmd")),
                ),
                (
                    "pwsh",
                    form.shell == "pwsh",
                    Pick::QuickLaunch(Action::SetShell("pwsh")),
                ),
                (
                    "powershell",
                    form.shell == "powershell",
                    Pick::QuickLaunch(Action::SetShell("powershell")),
                ),
                (
                    "python",
                    form.shell == "python",
                    Pick::QuickLaunch(Action::SetShell("python")),
                ),
            ];
            rows.push(Row::new(
                t!("Shell"),
                None,
                page.chips_when(
                    "quick-launch-command-shell",
                    &shell_options,
                    false,
                    enabled,
                    cx,
                ),
            ));
            rows.push(Row::new(t!("Script"), None, input_row(&form.script, enabled)).wide());
            rows.push(Row::new(
                t!("Executar sem janela"),
                None,
                page.switch_pick_when(
                    "quick-launch-command-hidden",
                    form.hidden,
                    Pick::QuickLaunch(Action::ToggleHidden),
                    enabled,
                    cx,
                ),
            ));
            rows.push(Row::new(
                t!("Manter o console aberto"),
                None,
                page.switch_pick_when(
                    "quick-launch-command-keep-open",
                    form.keep_open,
                    Pick::QuickLaunch(Action::ToggleKeepOpen),
                    enabled,
                    cx,
                ),
            ));
        }
        if !group {
            rows.push(
                Row::new(
                    t!("Pasta de trabalho"),
                    None,
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .child(input_row(&form.cwd, cwd_enabled))
                        .child(browse_button(Field::Cwd, cwd_enabled, cx)),
                )
                .wide(),
            );
        }
        if !group && command {
            rows.push(
                Row::hint(
                    t!("Executar como administrador"),
                    t!("Comandos não podem ser executados como administrador"),
                    page.switch_pick_when(
                        "quick-launch-item-admin",
                        form.admin,
                        Pick::QuickLaunch(Action::ToggleAdmin),
                        false,
                        cx,
                    ),
                )
                .wide(),
            );
            let trust = quick_launch::command_trust(cx, &form.group, form.slot);
            let mut control = div().flex().items_center().gap_2().child(status_line(
                if trust == Some(true) {
                    t!("Confiável")
                } else {
                    t!("Não confiável")
                },
                false,
            ));
            if trust == Some(true) {
                control = control.child(action_button(
                    "quick-launch-command-revoke-trust",
                    t!("Revogar confiança"),
                    true,
                    enabled,
                    Action::RevokeTrust,
                    cx,
                ));
            }
            rows.push(Row::new(t!("Confiança"), None, control).wide());
        } else if !group {
            rows.push(Row::new(
                t!("Executar como administrador"),
                None,
                page.switch_pick_when(
                    "quick-launch-item-admin",
                    form.admin,
                    Pick::QuickLaunch(Action::ToggleAdmin),
                    enabled && !uwp,
                    cx,
                ),
            ));
        }
        rows.extend([
            Row::new(t!("Ícone"), None, input_row(&form.icon, enabled)).wide(),
            Row::new(
                t!("Etiquetas (separadas por vírgula)"),
                None,
                input_row(&form.tags, enabled),
            )
            .wide(),
            Row::new(
                t!("Salvar"),
                None,
                action_button(
                    "quick-launch-item-save",
                    t!("Salvar"),
                    false,
                    enabled,
                    Action::SaveItem,
                    cx,
                ),
            )
            .wide(),
        ]);
        rows
    }

    /// The installed-app picker under the path row (§17.2 B.5): a search field
    /// and the apps whose name matches it. The list is enumerated once per
    /// session by a background task, never here.
    fn apps_picker_row(&self, enabled: bool, cx: &mut Context<SettingsPage>) -> AnyElement {
        let query = self
            .apps_query
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let mut list = div()
            .id("quick-launch-apps")
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(240.0))
            .min_h(px(0.0))
            .overflow_y_scroll();
        if let Some(input) = self.apps_query.as_ref() {
            list = list.child(Input::new(input).small().w(px(360.0)));
        }
        if self.apps_loading {
            list = list.child(status_line(t!("Carregando…"), false));
        } else if let Some(error) = &self.apps_error {
            list = list.child(status_line(
                tf!(
                    "Não foi possível listar os aplicativos: {error}",
                    error = error
                ),
                true,
            ));
        } else if let Some(apps) = &self.apps {
            let matching = matching_apps(apps, &query);
            if matching.is_empty() {
                list = list.child(status_line(t!("Nenhum"), false));
            } else {
                list = list.children(matching.into_iter().map(|(index, app)| {
                    let pick = Pick::QuickLaunch(Action::PickApp(index));
                    div()
                        .id(ElementId::Name(SharedString::from(format!(
                            "quick-launch-app-{index}"
                        ))))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px(px(8.0))
                        .py(px(5.0))
                        .rounded(px(5.0))
                        .text_sm()
                        .text_color(rgb(theme::fg()))
                        .when(enabled, |this| {
                            this.cursor_pointer()
                                .hover(|style| style.bg(rgb(theme::bg_hover())))
                                .on_click(cx.listener(move |page, _event, _window, cx| {
                                    page.pick(pick.clone(), cx)
                                }))
                        })
                        .when(!enabled, |this| this.opacity(0.45))
                        .child(div().flex_1().child(app.name.clone()))
                        .child(
                            div()
                                .flex_shrink_0()
                                .max_w(px(320.0))
                                .text_xs()
                                .text_color(rgb(theme::fg_secondary()))
                                .child(app.aumid.clone()),
                        )
                }));
            }
        }
        list.into_any_element()
    }

    /// The multi-select of a `group` item's entries (§17.2 E.3): the other items
    /// of the same group, by slot, each showing the place it was picked in. The
    /// pick order is the launch order.
    fn group_picker(
        &self,
        form: &Form,
        enabled: bool,
        cx: &mut Context<SettingsPage>,
    ) -> AnyElement {
        let groups = quick_launch::groups(cx);
        let Some(group) = groups.iter().find(|group| group.id == form.group) else {
            return status_line(t!("Nenhum"), false).into_any_element();
        };
        let mut entries: Vec<&Item> = group
            .items
            .iter()
            .filter(|item| item.slot != form.slot)
            .collect();
        entries.sort_by_key(|item| item.slot);
        if entries.is_empty() {
            return status_line(t!("Nenhum"), false).into_any_element();
        }
        let list = div().flex().flex_col().gap_1().w_full();
        list.children(entries.into_iter().map(|item| {
            let order = form.group_slots.iter().position(|slot| *slot == item.slot);
            let pick = Pick::QuickLaunch(Action::ToggleGroupItem(item.slot));
            div()
                .id(ElementId::Name(SharedString::from(format!(
                    "quick-launch-group-item-{}",
                    item.slot
                ))))
                .flex()
                .items_center()
                .gap_2()
                .px(px(8.0))
                .py(px(5.0))
                .rounded(px(5.0))
                .map(|this| {
                    if order.is_some() {
                        this.bg(rgb(theme::accent_subtle()))
                            .text_color(rgb(theme::accent()))
                    } else {
                        this.text_color(rgb(theme::fg()))
                            .hover(|style| style.bg(rgb(theme::bg_hover())))
                    }
                })
                .when(enabled, |this| this.cursor_pointer())
                .when(enabled, |this| {
                    this.on_click(
                        cx.listener(move |page, _event, _window, cx| page.pick(pick.clone(), cx)),
                    )
                })
                .when(!enabled, |this| this.opacity(0.45))
                .child(div().w(px(22.0)).flex_shrink_0().text_xs().child(
                    order.map_or_else(|| "·".to_string(), |order| format!("{}.", order + 1)),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_sm()
                        .child(item.name.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(rgb(theme::fg_secondary()))
                        .child(item.kind.clone()),
                )
        }))
        .into_any_element()
    }

    /// The file card (§17.1 B): the path, the two buttons and the warning that
    /// an edit through the interface rewrites the file; the read-only reason
    /// opens the card in red.
    fn file_group(
        &self,
        read_only: &Option<String>,
        cx: &mut Context<SettingsPage>,
    ) -> super::Group {
        let path = paths::quick_launch_file();
        let mut rows = Vec::new();
        if let Some(reason) = read_only {
            rows.push(
                Row::new(
                    t!("Somente leitura"),
                    None,
                    status_line(reason.clone(), true),
                )
                .wide(),
            );
        }
        rows.push(
            Row::hint(
                t!("Arquivo quicklaunch.toml"),
                t!("Editar pela interface reescreve o arquivo e remove os comentários."),
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::fg_secondary()))
                            .child(path.display().to_string()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(action_button(
                                "quick-launch-open-file",
                                t!("Abrir"),
                                false,
                                true,
                                Action::OpenFile,
                                cx,
                            ))
                            .child(action_button(
                                "quick-launch-reveal-file",
                                t!("Mostrar no LumaFiles"),
                                false,
                                true,
                                Action::RevealFile,
                                cx,
                            )),
                    ),
            )
            .wide(),
        );
        super::Group::titled(t!("Arquivo"), rows)
    }
}

/// The item form as the fields hold it while it is open.
struct Form {
    /// The group the item belongs to, by id.
    group: String,
    /// The slot of the item in that group.
    slot: u32,
    /// The kind shown by the chips; a kind outside the six stays as it was.
    kind: String,
    /// The slots a `group` item starts, in the order they were picked
    /// (§17.2 E.3). Empty for every other kind.
    group_slots: Vec<u32>,
    /// Whether the item is started elevated.
    admin: bool,
    name: Entity<InputState>,
    path: Entity<InputState>,
    args: Entity<InputState>,
    cwd: Entity<InputState>,
    icon: Entity<InputState>,
    tags: Entity<InputState>,
    /// The `command` fields: the shell, the script and the two flags (§17.2 D).
    shell: String,
    script: Entity<InputState>,
    hidden: bool,
    keep_open: bool,
}

/// Which field the file picker under the form fills.
#[derive(Clone, Copy)]
enum Field {
    Path,
    Cwd,
}

/// The whole section, with the state the page's click handlers go through.
pub(super) fn apply(page: &mut SettingsPage, action: Action, cx: &mut Context<SettingsPage>) {
    match action {
        Action::View(view) => set_window(cx, move |window| window.view = view),
        Action::Columns(columns) => set_window(cx, move |window| window.columns = columns),
        Action::CellSize(size) => set_window(cx, move |window| window.cell_size = size),
        Action::IconSize(size) => set_window(cx, move |window| window.icon_size = size),
        Action::ListColumns(columns) => set_window(cx, move |window| window.list_columns = columns),
        Action::AutoHide(on) => set_window(cx, move |window| window.auto_hide = on),
        Action::CloseAfterLaunch(on) => {
            set_window(cx, move |window| window.close_after_launch = on)
        }
        Action::Topmost(on) => set_window(cx, move |window| window.topmost = on),
        Action::OnRunning(on_running) => {
            set_window(cx, move |window| window.on_running = on_running)
        }
        Action::KeepRunning(on) => set_window(cx, move |window| window.keep_running = on),
        Action::RecordHotkey => {
            page.quick_launch.recording = !page.quick_launch.recording;
            cx.notify();
        }
        Action::SelectGroup(index) => {
            let groups = quick_launch::groups(cx);
            let Some(group) = groups.get(index) else {
                return;
            };
            page.quick_launch.group = Some(group.id.clone());
            page.quick_launch.form = None;
            close_apps_picker(&mut page.quick_launch);
            cx.notify();
        }
        Action::SelectItem(slot) => {
            let groups = quick_launch::groups(cx);
            let Some(index) = page.quick_launch.chosen_group(&groups) else {
                return;
            };
            let Some(group) = groups.get(index) else {
                return;
            };
            page.quick_launch.select = Some((group.id.clone(), slot));
            page.quick_launch.form = None;
            close_apps_picker(&mut page.quick_launch);
            cx.notify();
        }
        Action::SetKind(kind) => {
            if let Some(form) = page.quick_launch.form.as_mut() {
                form.kind = kind.to_string();
            }
            if kind != KIND_UWP {
                close_apps_picker(&mut page.quick_launch);
            }
            cx.notify();
        }
        Action::ToggleGroupItem(slot) => {
            if let Some(form) = page.quick_launch.form.as_mut() {
                match form
                    .group_slots
                    .iter()
                    .position(|selected| *selected == slot)
                {
                    Some(index) => {
                        form.group_slots.remove(index);
                    }
                    None => form.group_slots.push(slot),
                }
            }
            cx.notify();
        }
        Action::ChooseApp => choose_app(page, cx),
        Action::PickApp(index) => pick_app(page, index, cx),
        Action::ToggleAdmin => {
            if let Some(form) = page.quick_launch.form.as_mut() {
                form.admin = !form.admin;
            }
            cx.notify();
        }
        Action::SetShell(shell) => {
            if let Some(form) = page.quick_launch.form.as_mut() {
                form.shell = shell.to_string();
            }
            cx.notify();
        }
        Action::ToggleHidden => {
            if let Some(form) = page.quick_launch.form.as_mut() {
                form.hidden = !form.hidden;
            }
            cx.notify();
        }
        Action::ToggleKeepOpen => {
            if let Some(form) = page.quick_launch.form.as_mut() {
                form.keep_open = !form.keep_open;
            }
            cx.notify();
        }
        Action::RevokeTrust => {
            if let Some(form) = page.quick_launch.form.as_ref() {
                let (group, slot) = (form.group.clone(), form.slot);
                if quick_launch::revoke_command_trust(cx, &group, slot) {
                    page.status = None;
                }
            }
            cx.notify();
        }
        Action::NewGroup => {
            if !quick_launch::editable(cx) || quick_launch::add_group(cx).is_none() {
                return;
            }
            let groups = quick_launch::groups(cx);
            let Some(group) = groups.last() else {
                return;
            };
            page.quick_launch.renaming = Some(group.id.clone());
            page.quick_launch.rename_input = None;
            cx.refresh_windows();
            cx.notify();
        }
        Action::RenameGroup(index) => {
            let groups = quick_launch::groups(cx);
            let Some(group) = groups.get(index) else {
                return;
            };
            page.quick_launch.renaming = Some(group.id.clone());
            page.quick_launch.rename_input = None;
            cx.notify();
        }
        Action::SaveRename => save_rename(page, cx),
        Action::CancelRename => {
            page.quick_launch.renaming = None;
            page.quick_launch.rename_input = None;
            cx.notify();
        }
        Action::DeleteGroup(index) => delete_group(page, index, cx),
        Action::MoveGroupUp(index) => move_group(cx, index, index.checked_sub(1)),
        Action::MoveGroupDown(index) => move_group(cx, index, index.checked_add(1)),
        Action::SaveItem => save_item(page, cx),
        Action::OpenFile => open_file(cx),
        Action::RevealFile => reveal_file(cx),
    }
}

/// Closes the `uwp` picker and forgets what it held for the frame that was
/// waiting to fill the fields.
fn close_apps_picker(section: &mut Section) {
    section.apps_picker = false;
    section.apps_query = None;
    section.picked_app = None;
}

/// Opens or closes the installed-app picker (§17.2 B.5).
///
/// The Apps folder is enumerated on a background task the first time the
/// picker opens, and the answer is kept for the session: a foreground thread
/// must not do this work (§18), and walking the folder again for every click
/// would only add latency.
fn choose_app(page: &mut SettingsPage, cx: &mut Context<SettingsPage>) {
    let section = &mut page.quick_launch;
    if section.apps_picker {
        close_apps_picker(section);
        cx.notify();
        return;
    }
    section.apps_picker = true;
    if section.apps.is_some() || section.apps_loading {
        cx.notify();
        return;
    }
    section.apps_loading = true;
    section.apps_error = None;
    let enumeration = cx.background_spawn(async {
        let _apartment = ComApartment::enter()?;
        installed_apps()
    });
    cx.spawn(async move |this, cx| {
        let outcome = enumeration.await;
        this.update(cx, |page, cx| {
            let section = &mut page.quick_launch;
            section.apps_loading = false;
            match outcome {
                Ok(apps) => section.apps = Some(apps),
                Err(error) => section.apps_error = Some(error.to_string()),
            }
            cx.notify();
        })
        .log_err();
    })
    .detach();
    cx.notify();
}

/// Fills the form with the installed app at `index` (§17.2 B.5).
///
/// The click only stores the app: writing the input fields needs the window,
/// which the next frame's [`ensure_inputs`] has.
fn pick_app(page: &mut SettingsPage, index: usize, cx: &mut Context<SettingsPage>) {
    let Some(app) = page
        .quick_launch
        .apps
        .as_ref()
        .and_then(|apps| apps.get(index))
        .cloned()
    else {
        return;
    };
    page.quick_launch.picked_app = Some(app);
    page.quick_launch.apps_picker = false;
    cx.notify();
}

/// The installed apps whose name holds `query`, ignoring case and surrounding
/// spaces, each with its index in `apps` so a click can name it again.
fn matching_apps<'a>(apps: &'a [InstalledApp], query: &str) -> Vec<(usize, &'a InstalledApp)> {
    let query = query.trim().to_lowercase();
    apps.iter()
        .enumerate()
        .filter(|(_, app)| query.is_empty() || app.name.to_lowercase().contains(&query))
        .collect()
}

/// A key pressed while the summon chord waits for one: Esc gives up, the
/// modifiers alone wait, and a key that cannot be a global chord says so.
pub(super) fn record_key(
    page: &mut SettingsPage,
    event: &KeyDownEvent,
    cx: &mut Context<SettingsPage>,
) {
    cx.stop_propagation();
    let keystroke = &event.keystroke;
    let key = keystroke.key.to_lowercase();
    if nohrs_core::shortcuts::MODIFIER_KEYS.contains(&key.as_str()) {
        return;
    }
    let modifiers = keystroke.modifiers;
    if key == "escape" && !(modifiers.control || modifiers.alt || modifiers.shift) {
        page.quick_launch.recording = false;
        cx.notify();
        return;
    }
    let chord = Chord {
        ctrl: modifiers.control || modifiers.platform,
        alt: modifiers.alt,
        shift: modifiers.shift,
        key,
    };
    if chord.is_typing_key() {
        page.status = Some((
            t!("Letras, números e símbolos precisam de Ctrl ou Alt: sozinhos, eles digitam no filtro.").to_string(),
            true,
        ));
        cx.notify();
        return;
    }
    // The one conversion lives in `nohrs-core` (§17.1 B, v2.7): a recorded
    // `ctrl-alt-space` becomes the registration's `ctrl+alt+Space`, and a key the
    // registration cannot name is refused.
    let Some(text) = nohrs_core::shortcuts::gpui_keys_to_hotkey(&chord.to_config()) else {
        page.status = Some((
            t!("Tecla não suportada como atalho global.").to_string(),
            true,
        ));
        cx.notify();
        return;
    };
    page.quick_launch.recording = false;
    page.status = None;
    quick_launch::set_window(cx, move |window| window.hotkey = text);
    cx.refresh_windows();
    cx.notify();
}

/// Makes the fields the previous frame asked for: the form of a selected item
/// and the rename field of a group.
pub(super) fn ensure_inputs(
    page: &mut SettingsPage,
    window: &mut Window,
    cx: &mut Context<SettingsPage>,
) {
    let groups = quick_launch::groups(cx);
    if let Some((group_id, slot)) = page.quick_launch.select.take() {
        let found = groups.iter().find(|group| group.id == group_id);
        let item = found
            .and_then(|found| found.items.iter().find(|item| item.slot == slot))
            .cloned();
        let group_items = found.map(|found| found.items.clone()).unwrap_or_default();
        page.quick_launch.form = item
            .as_ref()
            .map(|item| form_of(&group_id, item, &group_items, window, cx));
    }
    // A form whose item is gone (deleted, moved, or edited by hand in the file)
    // closes rather than editing whatever took its place.
    let stale = page.quick_launch.form.as_ref().is_some_and(|form| {
        !groups.iter().any(|group| {
            group.id == form.group && group.items.iter().any(|item| item.slot == form.slot)
        })
    });
    if stale {
        page.quick_launch.form = None;
    }
    if page.quick_launch.form.is_none() {
        close_apps_picker(&mut page.quick_launch);
    }
    // A click on the app picker stores the app; the fields it fills are made
    // with a window, so they are written here (§17.2 B.5).
    if let Some(app) = page.quick_launch.picked_app.take() {
        if let Some(form) = page.quick_launch.form.as_ref() {
            form.path.update(cx, |state, cx| {
                state.set_value(app.aumid.clone(), window, cx)
            });
            if form.name.read(cx).value().trim().is_empty() {
                form.name.update(cx, |state, cx| {
                    state.set_value(app.name.clone(), window, cx)
                });
            }
        }
        close_apps_picker(&mut page.quick_launch);
    }
    if page.quick_launch.apps_picker && page.quick_launch.apps_query.is_none() {
        page.quick_launch.apps_query = Some(field(window, cx, t!("Filtrar"), ""));
    }
    match page.quick_launch.renaming.clone() {
        Some(id) => {
            if !groups.iter().any(|group| group.id == id) {
                page.quick_launch.renaming = None;
                page.quick_launch.rename_input = None;
                return;
            }
            if page.quick_launch.rename_input.is_none() {
                let name = groups
                    .iter()
                    .find(|group| group.id == id)
                    .map(|group| group.name.clone())
                    .unwrap_or_default();
                page.quick_launch.rename_input = Some(field(window, cx, t!("Nome"), &name));
            }
        }
        None => {
            page.quick_launch.rename_input = None;
        }
    }
}

/// The form of one item, filled from the item the file holds now.
fn form_of(
    group: &str,
    item: &Item,
    group_items: &[Item],
    window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Form {
    // A command's `extra` is read through the same parser the launcher uses, so
    // a broken entry opens as `cmd` with an empty script and is refused on save.
    let command = nohrs_core::config::quick_launch::command_spec(item).ok();
    let script = item
        .extra
        .get("script")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let script_input = cx.new(|cx| InputState::new(window, cx).multi_line(true).rows(4));
    script_input.update(cx, |state, cx| {
        state.set_placeholder(t!("Script"), window, cx);
        state.set_value(script, window, cx);
    });
    // A `group`'s stored references are ids; the form works with the slots of
    // the items they name, which is what the picker shows (§17.2 E.3).
    let group_slots: Vec<u32> = item
        .items
        .iter()
        .filter_map(|id| {
            group_items
                .iter()
                .find(|candidate| candidate.id.as_deref() == Some(id.as_str()))
                .map(|candidate| candidate.slot)
        })
        .collect();
    Form {
        group: group.to_string(),
        slot: item.slot,
        kind: item.kind.clone(),
        group_slots,
        admin: item.admin,
        name: field(window, cx, t!("Nome"), &item.name),
        path: field(
            window,
            cx,
            t!("Caminho"),
            item.path.as_deref().unwrap_or_default(),
        ),
        args: field(
            window,
            cx,
            t!("Argumentos"),
            item.args.as_deref().unwrap_or_default(),
        ),
        cwd: field(
            window,
            cx,
            t!("Pasta de trabalho"),
            item.cwd.as_deref().unwrap_or_default(),
        ),
        icon: field(
            window,
            cx,
            t!("Ícone"),
            item.icon.as_deref().unwrap_or_default(),
        ),
        tags: field(window, cx, t!("Etiquetas"), &item.tags.join(", ")),
        shell: command
            .as_ref()
            .map_or("cmd", |spec| spec.shell.as_str())
            .to_string(),
        script: script_input,
        hidden: command.as_ref().is_some_and(|spec| spec.hidden),
        keep_open: command.as_ref().is_some_and(|spec| spec.keep_open),
    }
}

/// One text field of the form, made with its value already in it.
fn field(
    window: &mut Window,
    cx: &mut Context<SettingsPage>,
    placeholder: &'static str,
    value: &str,
) -> Entity<InputState> {
    let input = cx.new(|cx| InputState::new(window, cx));
    input.update(cx, |state, cx| {
        state.set_placeholder(placeholder, window, cx);
        state.set_value(value.to_string(), window, cx);
    });
    input
}

/// One form field as an element, sized for the row.
fn input_row(input: &Entity<InputState>, enabled: bool) -> AnyElement {
    div()
        .flex_1()
        .min_w(px(0.0))
        .when(!enabled, |this| this.opacity(0.45))
        .child(Input::new(input).small().w(px(360.0)))
        .into_any_element()
}

/// The "browse…" button beside a path field.
fn browse_button(field: Field, enabled: bool, cx: &mut Context<SettingsPage>) -> AnyElement {
    let id = match field {
        Field::Path => "quick-launch-browse-path",
        Field::Cwd => "quick-launch-browse-cwd",
    };
    div()
        .id(id)
        .h(px(28.0))
        .px(px(12.0))
        .flex()
        .flex_shrink_0()
        .items_center()
        .rounded(px(6.0))
        .text_sm()
        .whitespace_nowrap()
        .border_1()
        .border_color(rgb(theme::border_hover()))
        .bg(rgb(theme::bg()))
        .text_color(rgb(theme::fg()))
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .on_click(cx.listener(move |_page, _event, window, cx| browse(field, window, cx)))
        })
        .when(!enabled, |this| this.opacity(0.45))
        .child(t!("Procurar…"))
        .into_any_element()
}

/// Opens a file picker for one of the form's two path fields, on the window.
fn browse(field: Field, window: &mut Window, cx: &mut Context<SettingsPage>) {
    let cwd = matches!(field, Field::Cwd);
    let receiver = cx.prompt_for_paths(PathPromptOptions {
        files: !cwd,
        directories: true,
        multiple: false,
        prompt: Some(SharedString::from(t!("Escolher um arquivo ou uma pasta"))),
    });
    cx.spawn_in(window, async move |this, cx| {
        let Ok(Ok(Some(paths))) = receiver.await else {
            return;
        };
        let Some(path) = paths.first() else {
            return;
        };
        let value = path.to_string_lossy().to_string();
        this.update_in(cx, |page, window, cx| {
            let Some(form) = page.quick_launch.form.as_ref() else {
                return;
            };
            let input = match field {
                Field::Path => form.path.clone(),
                Field::Cwd => form.cwd.clone(),
            };
            input.update(cx, |state, cx| state.set_value(value.clone(), window, cx));
        })
        .log_err();
    })
    .detach();
}

/// Writes the rename field back; an empty name cancels, as in §12.3.
fn save_rename(page: &mut SettingsPage, cx: &mut Context<SettingsPage>) {
    if !quick_launch::editable(cx) {
        return;
    }
    let (Some(input), Some(id)) = (
        page.quick_launch.rename_input.clone(),
        page.quick_launch.renaming.clone(),
    ) else {
        page.quick_launch.renaming = None;
        page.quick_launch.rename_input = None;
        cx.notify();
        return;
    };
    let name = input.read(cx).value().trim().to_string();
    page.quick_launch.renaming = None;
    page.quick_launch.rename_input = None;
    if name.is_empty() {
        cx.notify();
        return;
    }
    let groups = quick_launch::groups(cx);
    if let Some(index) = groups.iter().position(|group| group.id == id) {
        quick_launch::rename_group(cx, index, &name);
        cx.refresh_windows();
    }
    cx.notify();
}

/// Deletes the group at `index`; a group with items asks for a second click.
fn delete_group(page: &mut SettingsPage, index: usize, cx: &mut Context<SettingsPage>) {
    if !quick_launch::editable(cx) {
        return;
    }
    let groups = quick_launch::groups(cx);
    let Some(group) = groups.get(index) else {
        return;
    };
    let id = group.id.clone();
    if page.quick_launch.confirm_delete.as_deref() != Some(id.as_str()) {
        page.quick_launch.confirm_delete = Some(id);
        cx.notify();
        return;
    }
    page.quick_launch.confirm_delete = None;
    if !quick_launch::delete_group(cx, index, true) {
        cx.notify();
        return;
    }
    if page.quick_launch.group.as_deref() == Some(id.as_str()) {
        page.quick_launch.group = None;
    }
    if page
        .quick_launch
        .form
        .as_ref()
        .is_some_and(|form| form.group == id)
    {
        page.quick_launch.form = None;
    }
    cx.refresh_windows();
    cx.notify();
}

/// Moves one group to `to`, then tells both windows about the new order.
fn move_group(cx: &mut Context<SettingsPage>, from: usize, to: Option<usize>) {
    if !quick_launch::editable(cx) {
        return;
    }
    let Some(to) = to else {
        return;
    };
    if quick_launch::move_group(cx, from, to) {
        cx.refresh_windows();
        cx.notify();
    }
}

/// Writes the item form back through the state's own edit path.
fn save_item(page: &mut SettingsPage, cx: &mut Context<SettingsPage>) {
    if !quick_launch::editable(cx) {
        return;
    }
    let Some(form) = page.quick_launch.form.as_ref() else {
        return;
    };
    let groups = quick_launch::groups(cx);
    let Some(index) = groups.iter().position(|group| group.id == form.group) else {
        page.status = Some((t!("O grupo deste item não existe mais.").to_string(), true));
        cx.notify();
        return;
    };
    let slot = form.slot;
    let kind = form.kind.clone();
    let edit = quick_launch::ItemEdit {
        kind: form.kind.clone(),
        name: form.name.read(cx).value().to_string(),
        path: form.path.read(cx).value().to_string(),
        args: form.args.read(cx).value().to_string(),
        cwd: form.cwd.read(cx).value().to_string(),
        admin: form.admin,
        icon: form.icon.read(cx).value().to_string(),
        tags: form
            .tags
            .read(cx)
            .value()
            .split(',')
            .map(str::to_string)
            .collect(),
        command: (kind == KIND_COMMAND).then(|| quick_launch::CommandEdit {
            shell: form.shell.clone(),
            script: form.script.read(cx).value().to_string(),
            hidden: form.hidden,
            keep_open: form.keep_open,
        }),
        // The picker's order is the order the group starts its entries in.
        group_items: (kind == quick_launch::KIND_GROUP).then(|| form.group_slots.clone()),
    };
    if quick_launch::update_item(cx, index, slot, &edit) {
        page.status = None;
        cx.refresh_windows();
    } else if kind == KIND_COMMAND {
        page.status = Some((
            t!("Não foi possível salvar o comando. Confira o shell e o script.").to_string(),
            true,
        ));
    } else if kind == quick_launch::KIND_GROUP {
        page.status = Some((
            t!("Não foi possível salvar o grupo. Confira o nome e os itens.").to_string(),
            true,
        ));
    } else {
        page.status = Some((
            t!("Não foi possível salvar o item. Confira o nome e o caminho.").to_string(),
            true,
        ));
    }
    cx.notify();
}

/// Opens `quicklaunch.toml` with its own program, off the foreground (§18):
/// the file's own launch action hands it to the Shell with the default verb.
fn open_file(cx: &mut Context<SettingsPage>) {
    let path = paths::quick_launch_file();
    let opened = cx.background_spawn(async move {
        let mut item = Item::new(0, "file", "quicklaunch.toml");
        item.path = Some(path.to_string_lossy().to_string());
        let request = LaunchRequest {
            mode: LaunchMode::Normal,
            on_running: OnRunning::Activate,
            expand: ExpandContext::current(),
        };
        LaunchRegistry::with_builtins().launch(&item, &request)
    });
    cx.spawn(async move |this, cx| {
        let outcome = opened.await;
        this.update(cx, |page, cx| {
            page.status = match outcome {
                Ok(_) => None,
                Err(error) => Some((
                    tf!(
                        "Não foi possível abrir o quicklaunch.toml: {error}",
                        error = error
                    ),
                    true,
                )),
            };
            cx.notify();
        })
        .log_err();
    })
    .detach();
}

/// Shows `quicklaunch.toml` in LumaFiles, through the same reveal path the
/// quick-launch window uses.
fn reveal_file(cx: &mut Context<SettingsPage>) {
    quick_launch::open_in_explorer(cx, paths::quick_launch_file());
}

/// Sends one window-settings change to the state and repaints both sides.
fn set_window(cx: &mut Context<SettingsPage>, change: impl FnOnce(&mut WindowSettings)) {
    quick_launch::set_window(cx, change);
    cx.refresh_windows();
    cx.notify();
}

/// The chip options of one numeric setting.
fn numbered<T: Copy + PartialEq>(
    choices: &[(&'static str, T)],
    chosen: T,
    action: impl Fn(T) -> Action,
) -> Vec<(&'static str, bool, Pick)> {
    choices
        .iter()
        .map(|&(label, value)| (label, value == chosen, Pick::QuickLaunch(action(value))))
        .collect()
}

/// A small line of text under a control, red when it is a reason the setting
/// cannot be applied.
fn status_line(text: impl Into<SharedString>, error: bool) -> Div {
    div()
        .text_xs()
        .text_color(rgb(if error {
            theme::danger()
        } else {
            theme::fg_secondary()
        }))
        .child(text.into())
}

/// A button of this section: greyed and inert while `enabled` is false.
fn action_button(
    id: impl Into<ElementId>,
    label: &'static str,
    danger: bool,
    enabled: bool,
    action: Action,
    cx: &mut Context<SettingsPage>,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px(px(12.0))
        .flex()
        .flex_shrink_0()
        .items_center()
        .rounded(px(6.0))
        .text_sm()
        .whitespace_nowrap()
        .border_1()
        .bg(rgb(theme::bg()))
        .map(|this| {
            if danger {
                this.border_color(rgb(theme::danger()))
                    .text_color(rgb(theme::danger()))
            } else {
                this.border_color(rgb(theme::border_hover()))
                    .text_color(rgb(theme::fg()))
            }
        })
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .on_click(cx.listener(move |page, _event, _window, cx| {
                    page.pick(Pick::QuickLaunch(action), cx)
                }))
        })
        .when(!enabled, |this| this.opacity(0.45))
        .child(label)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// §17.1 B: the section builds its four cards even before the quick launch has
    /// a state, and its labels are the searchable static ones the field looks at.
    #[gpui::test]
    fn the_section_builds_its_four_cards(cx: &mut gpui::TestAppContext) {
        let page = cx.new(crate::settings::SettingsPage::new);
        page.update(cx, |page, cx| {
            page.show_section(crate::settings::SettingsSection::QuickLaunch, cx);
            let groups = page.groups(crate::settings::SettingsSection::QuickLaunch, cx);
            assert_eq!(groups.len(), 4, "window, groups, items and file");
            assert!(
                groups[0]
                    .rows
                    .iter()
                    .any(|row| row.label == t!("Atalho global")),
                "the chord has its row"
            );
            assert_eq!(groups[3].title, Some(t!("Arquivo")));
            let labels: Vec<&str> = groups
                .iter()
                .flat_map(|group| &group.rows)
                .map(|row| row.label)
                .collect();
            assert!(labels.contains(&t!("Grupos")));
            assert!(labels.contains(&t!("Itens")));
        });
    }

    /// The item editor follows the group by id, so a reorder does not move it.
    #[test]
    fn the_editor_follows_its_group_by_id() {
        let mut section = Section::default();
        assert_eq!(section.chosen_group(&[]), None);
        let groups = vec![
            LaunchGroup {
                id: "first".to_string(),
                name: "Primeiro".to_string(),
                items: Vec::new(),
                source: None,
            },
            LaunchGroup {
                id: "second".to_string(),
                name: "Segundo".to_string(),
                items: Vec::new(),
                source: None,
            },
        ];
        assert_eq!(section.chosen_group(&groups), Some(0));
        section.group = Some("second".to_string());
        assert_eq!(section.chosen_group(&groups), Some(1));
        section.group = Some("gone".to_string());
        assert_eq!(section.chosen_group(&groups), None);
    }

    /// The numeric controls only offer values inside the limits of §4.
    #[test]
    fn the_number_controls_stay_inside_the_limits_of_the_file() {
        for &(_, value) in &COLUMNS {
            assert!((2..=16).contains(&value));
        }
        for &(_, value) in &CELL_SIZES {
            assert!((64..=160).contains(&value));
        }
        for &(_, value) in &ICON_SIZES {
            assert!(matches!(value, 16 | 24 | 32 | 48 | 64 | 96));
        }
        for &(_, value) in &LIST_COLUMNS {
            assert!((1..=3).contains(&value));
        }
    }

    /// §17.2 B.5: the picker narrows the installed apps by the typed name,
    /// without regard to case.
    #[test]
    fn the_app_picker_filters_by_name_without_regard_to_case() {
        let apps = vec![
            InstalledApp {
                name: "Calculadora".to_string(),
                aumid: "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".to_string(),
            },
            InstalledApp {
                name: "Fotos".to_string(),
                aumid: "Microsoft.Windows.Photos_8wekyb3d8bbwe!App".to_string(),
            },
        ];
        let matching = matching_apps(&apps, "calc");
        assert_eq!(matching.len(), 1, "only the calculator matches");
        assert_eq!(matching[0].0, 0, "the index names the full list");
        assert_eq!(matching[0].1.name, "Calculadora");
        assert_eq!(matching_apps(&apps, "FOTOS")[0].0, 1);
        assert_eq!(
            matching_apps(&apps, "   ").len(),
            2,
            "an empty query shows all"
        );
        assert!(matching_apps(&apps, "nada").is_empty());
    }
}
