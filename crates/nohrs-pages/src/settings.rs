//! The settings page: the parts of `config.toml` that make sense to change
//! with the mouse, plus the shortcut list and the file itself.
//!
//! The page never applies a setting on its own. Every choice is written to
//! `config.toml` (keeping the user's comments, see `config::set_value`) and
//! the hot-reload watcher applies it as if the file had been edited by hand,
//! so there is one path from a setting to the window and the file stays the
//! single source of truth. The root hands the page each config it applies.
//!
//! Every section is built as data (groups of rows: name, explanation,
//! control) before it is drawn. That is what lets the search field at the top
//! show matching rows from every section at once, and what keeps the layout
//! decisions — side list or tabs on top, control beside or under its text —
//! in one place that follows the window's width.

use gpui::{
    AnyElement, Bounds, Context, DispatchPhase, Div, ElementId, Entity, FocusHandle, ImageSource, KeyDownEvent,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Stateful, Window, canvas, div,
    img, linear_color_stop, linear_gradient, prelude::*, px, rgb, MouseButton, Subscription,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_component::{Icon, Sizable as _};
use nohrs_core::config::{self, Config, DockSide, IndexSource, SortOrder, SplitDirection, ThemeMode};
use nohrs_core::i18n::{self, Lang, tr};
use nohrs_core::telemetry::LogErr;
use nohrs_core::shortcuts::{self, Action, Chord};
use nohrs_core::{k, t, tf};
use nohrs_ui::theme::theme;

use crate::color_wheel::{self, Hsv};
use crate::explorer::everything::{self, Source};

/// The sections of the settings page, in the order the side list shows them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsSection {
    /// Language, tabs and panes.
    General,
    /// Theme, accent colour and how the listing moves.
    Appearance,
    /// Compact mode: hiding the bars.
    Compact,
    /// What a folder shows and in which order.
    Files,
    /// Sounds and videos: hover playback, preview, waveforms.
    Media,
    /// The whole-computer search.
    Search,
    /// Opening folders from Windows and other programs here.
    Integrations,
    /// The keyboard shortcuts, for reference.
    Shortcuts,
    /// Version, updates and the config file itself.
    About,
}

impl SettingsSection {
    const ALL: [Self; 9] = [
        Self::General,
        Self::Appearance,
        Self::Compact,
        Self::Files,
        Self::Media,
        Self::Search,
        Self::Integrations,
        Self::Shortcuts,
        Self::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => t!("Geral"),
            Self::Appearance => t!("Aparência"),
            Self::Compact => t!("Modo compacto"),
            Self::Files => t!("Arquivos e pastas"),
            Self::Media => t!("Mídia"),
            Self::Search => t!("Busca"),
            Self::Integrations => t!("Integração com o Windows"),
            Self::Shortcuts => t!("Atalhos"),
            Self::About => t!("Sobre"),
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::General => t!("Idioma, abas e painéis."),
            Self::Appearance => t!("Tema, cores e como a lista se move."),
            Self::Compact => t!("Esconda as barras e deixe só os arquivos na tela."),
            Self::Files => t!("O que aparece em cada pasta e em que ordem."),
            Self::Media => t!("Áudio e vídeo: ouvir passando o mouse, tocar no visualizador e ver a forma de onda."),
            Self::Search => t!("Encontrar qualquer arquivo do computador pelo nome."),
            Self::Integrations => t!("Abrir no Prime Files as pastas do Windows e de outros programas."),
            Self::Shortcuts => t!("Mude as teclas de cada comando do explorador."),
            Self::About => t!("Versão, atualizações e o arquivo de configuração."),
        }
    }

    // SVGs in nohrs-ui's assets: an `IconName` without its file logs an
    // error on every frame.
    fn icon(self) -> &'static str {
        match self {
            Self::General => "icons/settings.svg",
            Self::Appearance => "icons/palette.svg",
            Self::Compact => "icons/layout-dashboard.svg",
            Self::Files => "icons/folder-open.svg",
            Self::Media => "icons/music.svg",
            Self::Search => "icons/search.svg",
            Self::Integrations => "icons/app-window.svg",
            Self::Shortcuts => "icons/case-sensitive.svg",
            Self::About => "icons/info.svg",
        }
    }
}

// Portuguese names for the accents `theme::NAMED_ACCENTS` knows, same order.
pub(crate) const ACCENT_LABELS: &[&str] = &[k!("Laranja"), k!("Azul"), k!("Verde-água"), k!("Verde"), k!("Roxo"), k!("Rosa"), k!("Vermelho")];

// Keys that are not commands of the shortcut table: typing filters, Enter
// opens, the wheel zooms. Listed beside the editable ones so the page still
// tells everything the explorer answers to.
const FIXED_KEYS: &[(&str, &str)] = &[
    (k!("Qualquer letra"), k!("Filtrar a pasta atual")),
    ("Enter", k!("Abrir no programa padrão")),
    ("Esc", k!("Limpar a seleção ou o filtro")),
    (k!("Ctrl+roda do mouse"), k!("Zoom contínuo")),
    ("Ctrl+K", k!("Abrir o lançador")),
    (k!("Espaço"), k!("Visualização rápida")),
    (k!("Botões laterais do mouse"), k!("Voltar e avançar")),
];

// What picking an option of a control does.
#[derive(Clone)]
enum Pick {
    // Writes the value (already in TOML form) to `[section] key`.
    Config(&'static str, &'static str, SharedString),
    // Takes folders over from Explorer, or gives them back.
    DefaultExplorer(bool),
    // Makes Listary open folders here, or in Explorer again.
    Listary(bool),
    // Waits for the next key press to become this command's shortcut.
    RecordShortcut(Action),
    // Leaves the command without a shortcut.
    ClearShortcut(Action),
    // Gives the command its built-in keys back.
    ResetShortcut(Action),
    // Gives every command its built-in keys back.
    ResetAllShortcuts,
    // Starts Prime Files minimized at sign-in, or stops it.
    StartWithWindows(bool),
}

// One setting: its name, what it does, and the control that changes it.
struct Row {
    label: &'static str,
    hint: Option<SharedString>,
    control: AnyElement,
    // Writes the default back; `Some` only while the value differs from it.
    reset: Option<Pick>,
    // The control is laid out under the text even when there is room beside
    // it: a row of colours or of many chips reads badly squeezed to the side.
    wide: bool,
}

impl Row {
    fn new(label: &'static str, hint: Option<SharedString>, control: impl IntoElement) -> Self {
        Self { label, hint, control: control.into_any_element(), reset: None, wide: false }
    }

    fn hint(label: &'static str, hint: impl Into<SharedString>, control: impl IntoElement) -> Self {
        Self::new(label, Some(hint.into()), control)
    }

    fn wide(mut self) -> Self {
        self.wide = true;
        self
    }

    // A reset to `default` (TOML) for `[section] key`, offered when `current`
    // is not already it.
    fn reset(mut self, (section, key): (&'static str, &'static str), is_default: bool, default: &'static str) -> Self {
        if !is_default {
            self.reset = Some(Pick::Config(section, key, SharedString::from(default)));
        }
        self
    }
}

// A card of rows under an optional heading.
struct Group {
    title: Option<&'static str>,
    rows: Vec<Row>,
}

impl Group {
    fn new(rows: Vec<Row>) -> Self {
        Self { title: None, rows }
    }

    fn titled(title: &'static str, rows: Vec<Row>) -> Self {
        Self { title: Some(title), rows }
    }
}

/// The settings page.
pub struct SettingsPage {
    section: SettingsSection,
    // The config last applied by the root; what every control reads.
    config: Config,
    // The outcome of the last file operation, and whether it was an error.
    status: Option<(String, bool)>,
    // "Restaurar padrão" asks for a second click before replacing the file.
    confirm_reset: bool,
    // Whether folders open in Nohrs; `None` until the registry has answered.
    default_explorer: Option<bool>,
    // Whether Prime Files starts at sign-in; `None` until the registry answered.
    starts_with_windows: Option<bool>,
    // What Listary opens folders with; `None` until its file has been read.
    listary: Option<crate::listary::State>,
    updater: Entity<crate::updater::Updater>,
    // The search field, made on the first render (it needs a `Window`).
    search: Option<Entity<InputState>>,
    // Window title field, committed when the user presses Enter or leaves it.
    app_name: Option<Entity<InputState>>,
    _app_name_subscription: Option<Subscription>,
    // The colour wheel, while it is open under the background presets.
    wheel: Option<Wheel>,
    // The font list, made on the first render (it needs a `Window`).
    font_select: Option<Entity<SelectState<SearchableVec<SharedString>>>>,
    // Takes the key presses that record a shortcut.
    focus_handle: FocusHandle,
    // The command whose new keys are being waited for.
    recording: Option<Action>,
}

/// The colour wheel while it is open: the colour the handles point at, and
/// the boxes the ring and the brightness bar were drawn in, so a pointer
/// position can be turned into a hue, a saturation and a brightness.
struct Wheel {
    hsv: Hsv,
    dragging: Option<WheelPart>,
}

/// Which of the wheel's two controls the pointer is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WheelPart {
    /// The ring: hue by angle, saturation by distance from the centre.
    Ring,
    /// The bar under it: brightness, full at its left end.
    Brightness,
}

impl SettingsPage {
    /// Creates a new settings page on its first section.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let updater = crate::updater::updater(cx);
        cx.observe(&updater, |_page, _updater, cx| cx.notify()).detach();
        // The search section shows the index's count while it is walked.
        cx.observe_global::<crate::explorer::everything::Everything>(|_page, cx| cx.notify()).detach();
        let mut page = Self {
            section: SettingsSection::General,
            config: Config::default(),
            status: None,
            confirm_reset: false,
            default_explorer: None,
            starts_with_windows: None,
            listary: None,
            updater,
            search: None,
            app_name: None,
            _app_name_subscription: None,
            wheel: None,
            font_select: None,
            focus_handle: cx.focus_handle(),
            recording: None,
        };
        page.refresh_default_explorer(cx);
        page.refresh_listary(cx);
        page
    }

    // Asks the registry, off the UI thread: each question is a `reg.exe` run.
    fn refresh_default_explorer(&mut self, cx: &mut Context<Self>) {
        let query = cx.background_spawn(async {
            (crate::default_explorer::is_default(), crate::default_explorer::starts_with_windows())
        });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let (is_default, starts) = query.await;
            this.update(cx, |page, cx| {
                page.default_explorer = Some(is_default);
                page.starts_with_windows = Some(starts);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn set_default_explorer(&mut self, enable: bool, cx: &mut Context<Self>) {
        // The running binary, so the entries point wherever it was built.
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                self.status = Some((tf!("Não foi possível achar o executável do Prime Files: {error}", error = error), true));
                cx.notify();
                return;
            }
        };
        let change = cx.background_spawn(async move {
            if enable {
                crate::default_explorer::enable(&exe)
            } else {
                crate::default_explorer::disable()
            }
        });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let outcome = change.await;
            this.update(cx, |page, cx| {
                page.status = Some(match outcome {
                    Ok(()) if enable => (t!("Pastas, drives e Win+E agora abrem no Prime Files.").to_string(), false),
                    Ok(()) => (t!("Pastas voltaram a abrir no Explorador do Windows.").to_string(), false),
                    Err(error) => (tf!("Não foi possível mudar o registro: {error}", error = error), true),
                });
                page.refresh_default_explorer(cx);
            })
            .log_err();
        })
        .detach();
    }

    fn set_start_with_windows(&mut self, enable: bool, cx: &mut Context<Self>) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                self.status = Some((tf!("Não foi possível achar o executável do Prime Files: {error}", error = error), true));
                cx.notify();
                return;
            }
        };
        let change = cx.background_spawn(async move { crate::default_explorer::set_starts_with_windows(&exe, enable) });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let outcome = change.await;
            this.update(cx, |page, cx| {
                if let Err(error) = outcome {
                    page.status = Some((tf!("Não foi possível mudar o registro: {error}", error = error), true));
                }
                page.refresh_default_explorer(cx);
            })
            .log_err();
        })
        .detach();
    }

    fn refresh_listary(&mut self, cx: &mut Context<Self>) {
        let query = cx.background_spawn(async { crate::listary::state() });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let state = query.await;
            this.update(cx, |page, cx| {
                page.listary = Some(state);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn set_listary(&mut self, enable: bool, cx: &mut Context<Self>) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                self.status = Some((tf!("Não foi possível achar o executável do Prime Files: {error}", error = error), true));
                cx.notify();
                return;
            }
        };
        // Closing and restarting Listary takes a few seconds.
        self.status = Some((t!("Mudando o Listary…").to_string(), false));
        cx.notify();
        let change = cx.background_spawn(async move {
            if enable {
                crate::listary::enable(&exe)
            } else {
                crate::listary::disable()
            }
        });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let outcome = change.await;
            this.update(cx, |page, cx| {
                page.status = Some(match outcome {
                    Ok(()) if enable => (t!("O Listary agora abre pastas no Prime Files.").to_string(), false),
                    Ok(()) => (t!("O Listary voltou a abrir pastas no Explorador.").to_string(), false),
                    Err(error) => (tf!("Não foi possível mudar o Listary: {error}", error = error), true),
                });
                page.refresh_listary(cx);
            })
            .log_err();
        })
        .detach();
    }

    fn pick(&mut self, pick: Pick, cx: &mut Context<Self>) {
        match pick {
            Pick::Config(section, key, value) => self.write(section, key, value.to_string(), cx),
            Pick::DefaultExplorer(enable) => self.set_default_explorer(enable, cx),
            Pick::Listary(enable) => self.set_listary(enable, cx),
            Pick::RecordShortcut(action) => {
                self.recording = if self.recording == Some(action) { None } else { Some(action) };
                cx.notify();
            }
            Pick::ClearShortcut(action) => {
                self.recording = None;
                self.edit_keybindings(vec![(shortcuts::info_of(action).id, Some(String::new()))], None, cx);
            }
            Pick::ResetShortcut(action) => {
                self.recording = None;
                self.edit_keybindings(vec![(shortcuts::info_of(action).id, None)], None, cx);
            }
            Pick::StartWithWindows(enable) => self.set_start_with_windows(enable, cx),
            Pick::ResetAllShortcuts => {
                self.recording = None;
                let edits = shortcuts::ACTIONS.iter().map(|info| (info.id, None)).collect();
                self.edit_keybindings(edits, Some(t!("Todos os atalhos voltaram ao padrão.").to_string()), cx);
            }
        }
    }

    // Writes `[keybindings]` changes one after the other in a single task:
    // each is a read and rewrite of the file, and two at once would lose one.
    // `None` removes the line, so the built-in keys apply again.
    fn edit_keybindings(
        &mut self,
        edits: Vec<(&'static str, Option<String>)>,
        done: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let path = config::paths::config_file();
        let write = cx.background_spawn(async move {
            for (id, value) in edits {
                match value {
                    // A TOML string: `ctrl-\` needs its backslash escaped.
                    Some(keys) => {
                        let quoted = format!("\"{}\"", keys.replace('\\', "\\\\").replace('"', "\\\""));
                        config::set_value(&path, "keybindings", id, &quoted)?
                    }
                    None => config::remove_value(&path, "keybindings", id)?,
                }
            }
            std::io::Result::Ok(())
        });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let outcome = write.await;
            this.update(cx, |page, cx| {
                match outcome {
                    Ok(()) => {
                        if let Some(done) = done {
                            page.status = Some((done, false));
                        }
                    }
                    Err(error) => {
                        page.status = Some((tf!("Não foi possível salvar os atalhos: {error}", error = error), true));
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    // A key pressed while a command waits for its shortcut. Modifiers alone
    // wait for the rest; Esc gives up.
    fn record_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(action) = self.recording else {
            return;
        };
        cx.stop_propagation();
        let keystroke = &event.keystroke;
        let key = keystroke.key.to_lowercase();
        if shortcuts::MODIFIER_KEYS.contains(&key.as_str()) {
            return;
        }
        let modifiers = keystroke.modifiers;
        if key == "escape" && !(modifiers.control || modifiers.alt || modifiers.shift) {
            self.recording = None;
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
            self.status = Some((t!("Letras, números e símbolos precisam de Ctrl ou Alt: sozinhos, eles digitam no filtro.").to_string(), true));
            cx.notify();
            return;
        }
        self.recording = None;
        let taken = shortcuts::conflicts(&self.config.keybindings.bindings, &chord, action);
        let mut edits: Vec<(&'static str, Option<String>)> =
            taken.iter().map(|other| (shortcuts::info_of(*other).id, Some(String::new()))).collect();
        edits.push((shortcuts::info_of(action).id, Some(chord.to_config())));
        let mut message = tf!(
            "{keys} agora: {command}.",
            keys = chord.display(),
            command = tr(shortcuts::info_of(action).label)
        );
        if !taken.is_empty() {
            let others: Vec<&str> = taken.iter().map(|other| tr(shortcuts::info_of(*other).label)).collect();
            message.push(' ');
            message.push_str(&tf!("Tirado de: {commands}.", commands = others.join(", ")));
        }
        self.edit_keybindings(edits, Some(message), cx);
    }

    /// Shows `section`, for the menu entries that lead to one directly.
    pub fn show_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.section = section;
        self.confirm_reset = false;
        // The search page says what Everything answers, and `auto` must hear
        // from it before it can show which index is in use.
        if section == SettingsSection::Search {
            let status = everything::status(cx);
            if status.external.is_none() && status.source != Source::Own {
                everything::probe(cx);
            }
        }
        cx.notify();
    }

    /// Takes the config the window now runs on, so the controls show it.
    pub fn set_config(&mut self, config: Config, cx: &mut Context<Self>) {
        self.config = config;
        cx.notify();
    }

    fn app_name_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = &self.app_name {
            return input.clone();
        }
        let input = cx.new(|cx| {
            let state = InputState::new(window, cx);
            state
        });
        input.update(cx, |state, cx| {
            state.set_placeholder(t!("Nome do aplicativo"), window, cx);
            state.set_value(self.config.ui.app_name.clone(), window, cx);
        });
        let subscription = cx.subscribe_in(&input, window, |page, input, event: &InputEvent, _window, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let value = input.read(cx).value().trim().to_string();
                if let Ok(value) = serde_json::to_string(&value) {
                    page.write("ui", "app_name", value, cx);
                }
            }
        });
        self.app_name = Some(input.clone());
        self._app_name_subscription = Some(subscription);
        input
    }

    // Writes one value; the watcher brings it back through `set_config`.
    fn write(&mut self, section: &'static str, key: &'static str, value: String, cx: &mut Context<Self>) {
        let path = config::paths::config_file();
        let write = cx.background_spawn(async move { config::set_value(&path, section, key, &value) });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            if let Err(error) = write.await {
                this.update(cx, |page, cx| {
                    page.status = Some((tf!("Não foi possível salvar {section}.{key}: {error}", section = section, key = key, error = error), true));
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    fn open_config_file(&mut self, cx: &mut Context<Self>) {
        let path = config::paths::config_file();
        // A first run may not have written it yet, and there is nothing to
        // open until it exists.
        if let Err(error) = config::ensure_exists(&path) {
            self.status = Some((tf!("Não foi possível criar {path}: {error}", path = path.display(), error = error), true));
            cx.notify();
            return;
        }
        // `explorer.exe <file>` hands it to whatever program `.toml` is
        // associated with, the same way a double click in the listing does.
        if let Err(error) = std::process::Command::new("explorer.exe").arg(&path).spawn() {
            self.status = Some((tf!("Não foi possível abrir {path}: {error}", path = path.display(), error = error), true));
            cx.notify();
        }
    }

    fn show_config_folder(&mut self, cx: &mut Context<Self>) {
        let path = config::paths::config_file();
        let mut command = std::process::Command::new("explorer.exe");
        if path.exists() {
            command.arg(format!("/select,{}", path.display()));
        } else {
            command.arg(config::paths::config_dir());
        }
        if let Err(error) = command.spawn() {
            self.status = Some((tf!("Não foi possível abrir a pasta: {error}", error = error), true));
            cx.notify();
        }
    }

    fn reset_config(&mut self, cx: &mut Context<Self>) {
        if !self.confirm_reset {
            self.confirm_reset = true;
            cx.notify();
            return;
        }
        self.confirm_reset = false;
        let path = config::paths::config_file();
        let reset = cx.background_spawn(async move { config::reset(&path) });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let outcome = reset.await;
            this.update(cx, |page, cx| {
                page.status = Some(match outcome {
                    Ok(Some(backup)) => (
                        tf!("Configurações restauradas. A versão anterior ficou em {path}", path = backup.display()),
                        false,
                    ),
                    Ok(None) => (t!("Configurações restauradas.").to_string(), false),
                    Err(error) => (tf!("Não foi possível restaurar: {error}", error = error), true),
                });
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    // ---------------------------------------------------------------------
    // The sections, as data

    fn groups(&self, section: SettingsSection, cx: &mut Context<Self>) -> Vec<Group> {
        match section {
            SettingsSection::General => self.general(cx),
            SettingsSection::Appearance => self.appearance(cx),
            SettingsSection::Compact => self.compact(cx),
            SettingsSection::Files => self.files(cx),
            SettingsSection::Media => self.media(cx),
            SettingsSection::Search => self.search_section(cx),
            SettingsSection::Integrations => self.integrations(cx),
            SettingsSection::Shortcuts => self.shortcuts(cx),
            SettingsSection::About => self.about(cx),
        }
    }

    fn general(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let explorer = &self.config.explorer;
        let layout = &self.config.layout;
        let ui = &self.config.ui;
        vec![
            Group::new(vec![self.language_row(cx)]),
            Group::titled(
                t!("Aplicativo"),
                vec![
                    Row::hint(
                        t!("Nome do aplicativo"),
                        t!("Mostrado na barra de título. Deixe vazio para usar o nome padrão; pressione Enter ou saia do campo para salvar."),
                        self.app_name
                            .as_ref()
                            .map(|input| Input::new(input).small().w(px(320.0)).cleanable(true).into_any_element())
                            .unwrap_or_else(|| div().into_any_element()),
                    )
                    .wide(),
                ],
            ),
            Group::titled(
                t!("Abas e painéis"),
                vec![
                    Row::hint(
                        t!("Reabrir as abas ao iniciar"),
                        t!("Volta com as abas da última vez. Vale na próxima vez que o Prime Files abrir."),
                        self.switch("restore-tabs", explorer.restore_tabs, ("explorer", "restore_tabs"), cx),
                    )
                    .reset(("explorer", "restore_tabs"), explorer.restore_tabs, "true"),
                    Row::hint(
                        t!("Divisão de painéis"),
                        t!("Para onde Ctrl+\\ divide a janela."),
                        self.config_choice(
                            "split-direction",
                            &[
                                (t!("Lado a lado"), explorer.split_direction == SplitDirection::Vertical, "\"vertical\""),
                                (t!("Em cima e embaixo"), explorer.split_direction == SplitDirection::Horizontal, "\"horizontal\""),
                            ],
                            ("explorer", "split_direction"),
                            cx,
                        ),
                    )
                    .reset(("explorer", "split_direction"), explorer.split_direction == SplitDirection::Vertical, "\"vertical\""),
                    Row::hint(
                        t!("Painéis sincronizados"),
                        t!("Navegar num painel leva o outro para a mesma pasta."),
                        self.switch("synced-panes", explorer.synced_panes, ("explorer", "synced_panes"), cx),
                    )
                    .reset(("explorer", "synced_panes"), !explorer.synced_panes, "false"),
                ],
            ),
            Group::titled(
                t!("Layout"),
                vec![
                    self.dock_row("sidebar", "dock-sidebar", t!("Barra lateral"), layout.sidebar, DockSide::Left, cx),
                    Row::hint(
                        t!("Acesso rápido"),
                        t!("Mostra as pastas fixadas e frequentes do Windows no começo da barra lateral."),
                        self.switch("quick-access", ui.show_quick_access, ("ui", "show_quick_access"), cx),
                    )
                    .reset(("ui", "show_quick_access"), ui.show_quick_access, "true"),
                    self.dock_row("preview", "dock-preview", t!("Visualizador"), layout.preview, DockSide::Right, cx),
                    self.dock_row("terminal", "dock-terminal", t!("Terminal"), layout.terminal, DockSide::Bottom, cx),
                    Row::hint(
                        t!("Abas verticais"),
                        t!("As abas numa coluna ao lado da lista, como no Zen. Estreita, mostra só os ícones."),
                        self.switch("vertical-tabs", layout.vertical_tabs, ("layout", "vertical_tabs"), cx),
                    )
                    .reset(("layout", "vertical_tabs"), !layout.vertical_tabs, "false"),
                    Row::hint(
                        t!("Lado das abas verticais"),
                        t!("De que lado da janela fica a coluna das abas."),
                        self.config_choice(
                            "vertical-tabs-side",
                            &[
                                (t!("Esquerda"), layout.vertical_tabs_side == DockSide::Left, "\"left\""),
                                (t!("Direita"), layout.vertical_tabs_side == DockSide::Right, "\"right\""),
                            ],
                            ("layout", "vertical_tabs_side"),
                            cx,
                        ),
                    )
                    .reset(
                        ("layout", "vertical_tabs_side"),
                        layout.vertical_tabs_side == DockSide::Left,
                        "\"left\"",
                    ),
                    Row::hint(
                        t!("Alça para mover painéis"),
                        t!("Uma alça no meio da borda de cada painel, para arrastá-lo até outro lado."),
                        self.switch("move-grips", layout.move_grips, ("layout", "move_grips"), cx),
                    )
                    .reset(("layout", "move_grips"), !layout.move_grips, "false"),
                ],
            ),
        ]
    }

    // The background colours offered as dots; the first is the palette's own.
    // Any other colour goes in `[theme] background` in config.toml, where the
    // `#rrggbb` form is read as it is.
    fn background_swatches(
        &self,
        cx: &mut Context<Self>,
        chosen: &str,
        own: bool,
    ) -> Div {
        let palette = if theme::is_dark() { 0x1E1F22 } else { 0xFFFFFF };
        let presets: [(&str, u32); 6] = [
            ("", palette),
            ("#1E1F22", 0x1E1F22),
            ("#000000", 0x000000),
            ("#22272E", 0x22272E),
            ("#101828", 0x101828),
            ("#F5F1E8", 0xF5F1E8),
        ];
        div().flex().flex_row().flex_wrap().gap_1().children(presets.into_iter().map(|(value, colour)| {
            let selected = if value.is_empty() { !own } else { chosen.eq_ignore_ascii_case(value) };
            div()
                .id(ElementId::Name(SharedString::from(format!("background-{value}"))))
                .w(px(36.0))
                .h(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .on_click(cx.listener(move |page, _event, _window, cx| {
                    page.write("theme", "background", format!("\"{value}\""), cx);
                }))
                .child(
                    div()
                        .size(px(20.0))
                        .rounded_full()
                        .bg(rgb(colour))
                        .border_1()
                        .border_color(rgb(if selected { theme::fg() } else { theme::border() })),
                )
        }))
    }

    // The font list: every family the system knows, by the name it is
    // installed under, searchable. Made on the first render (it needs a
    // `Window`); choosing one writes `ui.font_family`, and the root applies it.
    fn ensure_font_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.font_select.is_some() {
            return;
        }
        let mut fonts: Vec<SharedString> =
            cx.text_system().all_font_names().into_iter().map(SharedString::from).collect();
        fonts.sort_by(|left, right| left.to_lowercase().cmp(&right.to_lowercase()));
        fonts.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        let state = cx.new(|cx| SelectState::new(SearchableVec::new(fonts), None, window, cx));
        cx.subscribe_in(
            &state,
            window,
            |page, _state, event: &SelectEvent<SearchableVec<SharedString>>, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                let family = value.clone().unwrap_or_default();
                page.write("ui", "font_family", format!("\"{family}\""), cx);
            },
        )
        .detach();
        self.font_select = Some(state);
    }

    // The row's control: the searchable list of installed families.
    fn font_select(&self) -> Div {
        let current = self.config.ui.font_family.trim().to_string();
        let shown = if current.is_empty() { t!("Padrão").to_string() } else { current };
        div().flex().items_center().gap_2().children(
            self.font_select
                .clone()
                .map(|state| Select::new(&state).w(px(240.0)).placeholder(shown)),
        )
    }

    // The wheel's button, beside the presets: it opens the ring under them and
    // shows the colour in use.
    fn wheel_button(&self, background: String, cx: &mut Context<Self>) -> Stateful<Div> {
        let open = self.wheel.is_some();
        let color = theme::parse_color(&background)
            .unwrap_or(if theme::is_dark() { 0x1E1F22 } else { 0xFFFFFF });
        div()
            .id("background-wheel-open")
            .flex()
            .items_center()
            .gap_2()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if open { theme::accent() } else { theme::border() }))
            .hover(|style| style.bg(rgb(theme::bg_hover())))
            .on_click(cx.listener(|page, _event, _window, cx| {
                page.toggle_wheel(cx);
            }))
            .child(div().size(px(16.0)).rounded_full().bg(rgb(color)).border_1().border_color(rgb(theme::border())))
            .child(div().text_sm().text_color(rgb(theme::fg())).child(t!("Personalizada")))
    }

    // Opens or closes the wheel, starting it on the colour in use.
    fn toggle_wheel(&mut self, cx: &mut Context<Self>) {
        if self.wheel.take().is_some() {
            cx.notify();
            return;
        }
        let color = theme::parse_color(&self.config.theme.background)
            .unwrap_or(if theme::is_dark() { 0x1E1F22 } else { 0xFFFFFF });
        self.wheel = Some(Wheel {
            hsv: Hsv::from_rgb(color),
            dragging: None,
        });
        cx.notify();
    }

    // The wheel itself, as a row of its own under the presets.
    //
    // The ring and the bar take their pointer events the way every drag in
    // this program does: a `canvas` hands out the element's own bounds and
    // listens on the window, because a listener hung on the element inside a
    // scrolling page never sees the press.
    fn wheel_row(&self, cx: &mut Context<Self>) -> Option<Row> {
        let wheel = self.wheel.as_ref()?;
        let color = wheel.hsv.to_rgb();
        let hue = wheel.hsv.to_hsla();
        let handle_angle = wheel.hsv.h.to_radians();
        let handle_distance = wheel.hsv.s;
        let entity = cx.entity();

        let radius = color_wheel::SIZE / 2.0 - 2.0;
        let handle_x = color_wheel::SIZE / 2.0 + handle_angle.cos() * handle_distance * radius;
        let handle_y = color_wheel::SIZE / 2.0 + handle_angle.sin() * handle_distance * radius;

        let ring = div()
            .id("background-wheel-ring")
            .debug_selector(|| "background-wheel-ring".into())
            .relative()
            .size(px(color_wheel::SIZE))
            .flex_shrink_0()
            .cursor_pointer()
            .child(img(ImageSource::Render(color_wheel::image())).size(px(color_wheel::SIZE)))
            // Where the chosen hue and saturation sit on the ring.
            .child(
                div()
                    .absolute()
                    .left(px(handle_x - 6.0))
                    .top(px(handle_y - 6.0))
                    .size(px(12.0))
                    .rounded_full()
                    .border_2()
                    .border_color(rgb(0xFFFFFF))
                    .bg(rgb(color))
                    .shadow_sm(),
            )
            .child(pointer_events(entity.clone(), WheelPart::Ring));

        let brightness = div()
            .id("background-wheel-brightness")
            .debug_selector(|| "background-wheel-brightness".into())
            .relative()
            .w(px(220.0))
            .h(px(16.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(rgb(theme::border()))
            .cursor_pointer()
            .bg(linear_gradient(90.0, linear_color_stop(hue, 0.0), linear_color_stop(gpui::black(), 1.0)))
            .child(pointer_events(entity, WheelPart::Brightness));

        let panel = div()
            .flex()
            .items_center()
            .gap_4()
            .child(ring)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().text_sm().text_color(rgb(theme::muted())).child(t!("Brilho")))
                    .child(brightness)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .size(px(28.0))
                                    .rounded(px(6.0))
                                    .bg(rgb(color))
                                    .border_1()
                                    .border_color(rgb(theme::border())),
                            )
                            .child(div().text_sm().text_color(rgb(theme::fg_secondary())).child(wheel.hsv.hex())),
                    ),
            );

        Some(Row::hint(t!("Escolher a cor"), t!("Arraste no anel e na barra."), panel).wide())
    }

    // A press on the ring or the brightness bar: the colour follows the
    // pointer from here on, and the window previews it at once.
    fn wheel_press(&mut self, part: WheelPart, bounds: Bounds<Pixels>, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(wheel) = self.wheel.as_mut() else {
            return;
        };
        wheel.dragging = Some(part);
        self.wheel_apply(part, bounds, position, cx);
    }

    // The pointer let go: what the wheel shows is written to the config, the
    // one path every other setting takes.
    fn wheel_release(&mut self, cx: &mut Context<Self>) {
        let Some(wheel) = self.wheel.as_mut() else {
            return;
        };
        if wheel.dragging.take().is_none() {
            return;
        }
        let hex = wheel.hsv.hex();
        self.write("theme", "background", format!("\"{hex}\""), cx);
        cx.notify();
    }

    fn wheel_apply(&mut self, part: WheelPart, bounds: Bounds<Pixels>, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(wheel) = self.wheel.as_mut() else {
            return;
        };
        match part {
            WheelPart::Ring => {
                let Some((hue, saturation)) = color_wheel::ring_hue_saturation(bounds, position) else {
                    return;
                };
                wheel.hsv.h = hue;
                wheel.hsv.s = saturation;
            }
            WheelPart::Brightness => {
                let Some(value) = color_wheel::bar_brightness(bounds, position) else {
                    return;
                };
                wheel.hsv.v = value;
            }
        }
        // Shown in the whole window while the pointer moves; written on
        // release, so dragging a colour never touches the config file.
        theme::set_background(Some(wheel.hsv.to_rgb()));
        cx.refresh_windows();
    }
    // Which side of the window a panel docks to. The same choice a bubble of
    // the listing's edges makes, and the grip when it is shown.
    fn dock_row(
        &self,
        key: &'static str,
        id: &'static str,
        label: &'static str,
        side: DockSide,
        default: DockSide,
        cx: &mut Context<Self>,
    ) -> Row {
        let options = [
            (t!("Esquerda"), DockSide::Left),
            (t!("Direita"), DockSide::Right),
            (t!("Em cima"), DockSide::Top),
            (t!("Embaixo"), DockSide::Bottom),
        ];
        let options: Vec<(&'static str, bool, &'static str)> = options
            .into_iter()
            .map(|(name, option)| {
                let value: &'static str = match option {
                    DockSide::Left => "\"left\"",
                    DockSide::Right => "\"right\"",
                    DockSide::Top => "\"top\"",
                    DockSide::Bottom => "\"bottom\"",
                };
                (name, side == option, value)
            })
            .collect();
        let default_value = match default {
            DockSide::Left => "\"left\"",
            DockSide::Right => "\"right\"",
            DockSide::Top => "\"top\"",
            DockSide::Bottom => "\"bottom\"",
        };
        Row::hint(
            label,
            t!("Também dá para puxar uma borda da lista e soltar na bolha do painel."),
            self.config_choice(id, &options, ("layout", key), cx),
        )
        .reset(("layout", key), side == default, default_value)
    }

    // Languages are offered by their own names, untranslated, so a person who
    // cannot read the current one still finds theirs.
    fn language_row(&self, cx: &mut Context<Self>) -> Row {
        let chosen = Lang::parse(&self.config.ui.language);
        let mut options: Vec<(&'static str, bool, SharedString)> =
            vec![(t!("Automático"), chosen.is_none(), SharedString::from("\"auto\""))];
        for lang in Lang::ALL {
            options.push((lang.native_name(), chosen == Some(lang), SharedString::from(format!("\"{}\"", lang.code()))));
        }
        let options: Vec<_> = options
            .into_iter()
            .map(|(label, selected, value)| (label, selected, Pick::Config("ui", "language", value)))
            .collect();
        Row::hint(
            t!("Idioma"),
            tf!(
                "Automático segue o formato regional do Windows (Hora e idioma › Região); neste computador, {language}.",
                language = i18n::detected().native_name()
            ),
            self.chips("language", &options, false, cx),
        )
        .wide()
        .reset(("ui", "language"), chosen == Some(Lang::Zh), "\"zh-CN\"")
    }

    fn appearance(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let mode = self.config.theme.mode;
        let accent = self.config.theme.accent.trim().to_string();
        let custom = accent.starts_with('#') && theme::parse_accent(&accent).is_some();
        let background = self.config.theme.background.trim().to_lowercase();
        let own_background = !background.is_empty() && theme::parse_color(&background).is_some();
        let ui = &self.config.ui;

        let swatches = div().flex().flex_row().flex_wrap().gap_1().children(
            theme::NAMED_ACCENTS.iter().zip(ACCENT_LABELS).map(|(&(name, color), &label)| {
                let selected = accent.eq_ignore_ascii_case(name)
                    || (color.is_none() && theme::parse_accent(&accent).is_none());
                let swatch = color.unwrap_or_else(theme::palette_accent);
                div()
                    .id(ElementId::Name(SharedString::from(format!("accent-{name}"))))
                    .w(px(64.0))
                    .py(px(6.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme::bg_hover())))
                    .on_click(cx.listener(move |page, _event, _window, cx| {
                        page.write("theme", "accent", format!("\"{name}\""), cx);
                    }))
                    .child(
                        div()
                            .size(px(28.0))
                            .rounded_full()
                            .bg(rgb(swatch))
                            .border_2()
                            .border_color(rgb(if selected { theme::fg() } else { theme::bg_secondary() })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if selected { theme::fg() } else { theme::fg_secondary() }))
                            .child(tr(label)),
                    )
            }),
        );

        let mut groups = vec![Group::titled(
            t!("Cores"),
            vec![
                Row::new(
                    t!("Tema"),
                    None,
                    self.config_choice(
                        "theme-mode",
                        &[
                            (t!("Escuro"), mode == ThemeMode::Dark, "\"dark\""),
                            (t!("Claro"), mode == ThemeMode::Light, "\"light\""),
                            (t!("Igual ao Windows"), mode == ThemeMode::System, "\"system\""),
                        ],
                        ("theme", "mode"),
                        cx,
                    ),
                )
                .reset(("theme", "mode"), mode == ThemeMode::Dark, "\"dark\""),
                Row::hint(
                    t!("Cor de destaque"),
                    if custom {
                        t!("O config.toml tem uma cor própria; escolher uma aqui a substitui.")
                    } else {
                        t!("Seleção, abas ativas e botões.")
                    },
                    swatches,
                )
                .wide()
                .reset(("theme", "accent"), accent.eq_ignore_ascii_case("orange"), "\"orange\""),
                Row::hint(
                    t!("Cor de fundo"),
                    t!("As áreas de conteúdo. Qualquer cor em #rrggbb no config.toml vale; o texto se ajusta ao fundo."),
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(self.background_swatches(cx, &background, own_background))
                        // The wheel, for any colour the presets do not hold.
                        .child(self.wheel_button(background.clone(), cx)),
                )
                .wide()
                .reset(("theme", "background"), !own_background, "\"\""),
                Row::hint(
                    t!("Fonte da interface"),
                    t!("A fonte de todo o programa. O padrão é a do sistema."),
                    self.font_select(),
                )
                .wide()
                .reset(("ui", "font_family"), ui.font_family.trim().is_empty(), "\"\""),
            ],
        )];
        // The ring itself, under the presets, while it is open.
        if let Some(row) = self.wheel_row(cx) {
            groups[0].rows.push(row);
        }
        groups.push(Group::titled(
            t!("Lista"),
            vec![
                Row::hint(
                    t!("Rolagem suave"),
                    t!("Quanto a roda do mouse desliza a lista em vez de pular. Desligada rola em degraus."),
                    self.config_choice(
                        "scroll-smoothness",
                        &[
                            (t!("Desligada"), ui.scroll_smoothness == 0, "0"),
                            (t!("Dura"), ui.scroll_smoothness == 25, "25"),
                            (t!("Média"), ui.scroll_smoothness == 50, "50"),
                            (t!("Suave"), ui.scroll_smoothness == 75, "75"),
                            (t!("Muito suave"), ui.scroll_smoothness == 100, "100"),
                        ],
                        ("ui", "scroll_smoothness"),
                        cx,
                    ),
                )
                .wide()
                .reset(("ui", "scroll_smoothness"), ui.scroll_smoothness == 50, "50"),
                Row::hint(
                    t!("Linhas do nome nos ícones"),
                    t!("Quantas linhas um nome ocupa no modo Ícones, em qualquer zoom; o resto vira \"…\"."),
                    self.config_choice(
                        "name-lines",
                        &[
                            (t!("Sem limite"), ui.name_lines == 0, "0"),
                            ("1", ui.name_lines == 1, "1"),
                            ("2", ui.name_lines == 2, "2"),
                            ("3", ui.name_lines == 3, "3"),
                            ("4", ui.name_lines == 4, "4"),
                        ],
                        ("ui", "name_lines"),
                        cx,
                    ),
                )
                .reset(("ui", "name_lines"), ui.name_lines == 0, "0"),
            ],
        ));
        groups
    }

    // Each choice offers the default among its options, so a fresh config
    // always shows one selected.
    fn compact(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let compact = &self.config.compact;
        let enabled = compact.enabled;
        let mut groups = vec![Group::new(vec![
            Row::hint(
                t!("Modo compacto"),
                t!("Esconde as barras escolhidas, deixando só a lista; cada uma desliza para fora quando o mouse encosta na borda dela, como no Zen Browser."),
                self.switch("compact-enabled", enabled, ("compact", "enabled"), cx),
            )
            .reset(("compact", "enabled"), !enabled, "false"),
        ])];
        if !enabled {
            return groups;
        }
        groups.push(Group::titled(
            t!("O que esconder"),
            vec![
                Row::hint(
                    t!("Barras"),
                    t!("Abas e barra de rota saem do topo, a barra lateral da esquerda, o visualizador da direita e a barra de baixo (filtro, contagem, zoom) de baixo. Só as abas é o jeito antigo."),
                    self.config_toggles(
                        "compact-hide",
                        "compact",
                        &[
                            (t!("Abas"), compact.hide_tabs, "hide_tabs"),
                            (t!("Barra de rota"), compact.hide_header, "hide_header"),
                            (t!("Barra lateral"), compact.hide_sidebar, "hide_sidebar"),
                            (t!("Visualizador"), compact.hide_preview, "hide_preview"),
                            (t!("Barra de baixo"), compact.hide_bottom_bar, "hide_bottom_bar"),
                        ],
                        cx,
                    ),
                )
                .wide(),
                Row::hint(
                    t!("Como as barras aparecem"),
                    t!("Por cima da lista, sem mexer nela, ou empurrando a lista para abrir espaço, sem cobrir nada dela."),
                    self.config_choice(
                        "compact-overlay",
                        &[(t!("Por cima"), compact.overlay, "true"), (t!("Empurrando"), !compact.overlay, "false")],
                        ("compact", "overlay"),
                        cx,
                    ),
                )
                .reset(("compact", "overlay"), compact.overlay, "true"),
                Row::hint(
                    t!("Moldura"),
                    t!("A lista fica num cartão de cantos arredondados, com uma borda da cor da janela em volta, como no Zen."),
                    self.config_choice(
                        "compact-frame",
                        &[
                            (t!("Sem"), compact.frame_px == 0, "0"),
                            (t!("Fina"), compact.frame_px == 4, "4"),
                            (t!("Média"), compact.frame_px == 8, "8"),
                            (t!("Larga"), compact.frame_px == 12, "12"),
                        ],
                        ("compact", "frame_px"),
                        cx,
                    ),
                )
                .reset(("compact", "frame_px"), compact.frame_px == 8, "8"),
            ],
        ));
        groups.push(Group::titled(
            t!("Tempo e movimento"),
            vec![
                Row::hint(
                    t!("Espera para aparecer"),
                    t!("Quanto tempo o mouse fica na borda antes de a barra sair."),
                    self.config_choice(
                        "compact-reveal-delay",
                        &[
                            (t!("Nenhuma"), compact.reveal_delay_ms == 0, "0"),
                            (t!("Curta"), compact.reveal_delay_ms == 100, "100"),
                            (t!("Média"), compact.reveal_delay_ms == 300, "300"),
                            (t!("Longa"), compact.reveal_delay_ms == 600, "600"),
                        ],
                        ("compact", "reveal_delay_ms"),
                        cx,
                    ),
                )
                .reset(("compact", "reveal_delay_ms"), compact.reveal_delay_ms == 100, "100"),
                Row::hint(
                    t!("Espera para sumir"),
                    t!("Quanto tempo a barra fica depois que o mouse sai dela."),
                    self.config_choice(
                        "compact-hide-delay",
                        &[
                            (t!("Nenhuma"), compact.hide_delay_ms == 0, "0"),
                            (t!("Curta"), compact.hide_delay_ms == 150, "150"),
                            (t!("Média"), compact.hide_delay_ms == 400, "400"),
                            (t!("Longa"), compact.hide_delay_ms == 1000, "1000"),
                        ],
                        ("compact", "hide_delay_ms"),
                        cx,
                    ),
                )
                .reset(("compact", "hide_delay_ms"), compact.hide_delay_ms == 400, "400"),
                Row::hint(
                    t!("Animação"),
                    t!("Duração do deslizar. Desligada, as barras aparecem e somem de uma vez."),
                    self.config_choice(
                        "compact-animation",
                        &[
                            (t!("Desligada"), compact.animation_ms == 0, "0"),
                            (t!("Rápida"), compact.animation_ms == 120, "120"),
                            (t!("Média"), compact.animation_ms == 220, "220"),
                            (t!("Lenta"), compact.animation_ms == 400, "400"),
                        ],
                        ("compact", "animation_ms"),
                        cx,
                    ),
                )
                .reset(("compact", "animation_ms"), compact.animation_ms == 220, "220"),
                Row::hint(
                    t!("Área de ativação"),
                    t!("Largura da faixa em cada borda que chama a barra."),
                    self.config_choice(
                        "compact-edge",
                        &[
                            (t!("Fina"), compact.edge_px == 4, "4"),
                            (t!("Média"), compact.edge_px == 8, "8"),
                            (t!("Larga"), compact.edge_px == 16, "16"),
                        ],
                        ("compact", "edge_px"),
                        cx,
                    ),
                )
                .reset(("compact", "edge_px"), compact.edge_px == 8, "8"),
            ],
        ));
        groups
    }

    fn files(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let ui = &self.config.ui;
        vec![Group::new(vec![
            Row::hint(
                t!("Mostrar itens ocultos"),
                t!("Arquivos e pastas marcados como ocultos."),
                self.switch("show-hidden", ui.show_hidden, ("ui", "show_hidden"), cx),
            )
            .reset(("ui", "show_hidden"), !ui.show_hidden, "false"),
            Row::hint(
                t!("Ordenação padrão"),
                t!("Como cada pasta é ordenada ao abrir."),
                self.config_choice(
                    "default-sort",
                    &[
                        (t!("Nome"), ui.default_sort == SortOrder::Name, "\"name\""),
                        (t!("Modificado"), ui.default_sort == SortOrder::Modified, "\"modified\""),
                        (t!("Tamanho"), ui.default_sort == SortOrder::Size, "\"size\""),
                        (t!("Tipo"), ui.default_sort == SortOrder::Kind, "\"kind\""),
                    ],
                    ("ui", "default_sort"),
                    cx,
                ),
            )
            .reset(("ui", "default_sort"), ui.default_sort == SortOrder::Name, "\"name\""),
            Row::hint(
                t!("Direção da ordenação"),
                t!("Crescente: A→Z, menor e mais antigo primeiro. Pastas em que você mudou a ordem guardam a delas."),
                self.config_choice(
                    "default-sort-direction",
                    &[
                        (t!("Crescente"), !ui.default_sort_descending, "false"),
                        (t!("Decrescente"), ui.default_sort_descending, "true"),
                    ],
                    ("ui", "default_sort_descending"),
                    cx,
                ),
            )
            .reset(("ui", "default_sort_descending"), !ui.default_sort_descending, "false"),
        ])]
    }

    fn media(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let ui = &self.config.ui;
        let delay = ui.hover_audio_delay_ms;
        let mut hover = vec![
            Row::hint(
                t!("Tocar ao passar o mouse"),
                t!("Deixe o mouse parado sobre um arquivo de som na lista para ouvi-lo; tirar o mouse para. Um som por vez, no volume do visualizador."),
                self.switch("hover-audio", ui.hover_audio, ("ui", "hover_audio"), cx),
            )
            .reset(("ui", "hover_audio"), ui.hover_audio, "true"),
        ];
        if ui.hover_audio {
            hover.push(
                Row::hint(
                    t!("Tempo parado antes de tocar"),
                    t!("Curto toca enquanto você passa pela lista; longo só toca o arquivo em que você parou."),
                    self.config_choice(
                        "hover-audio-delay",
                        &[
                            (t!("Na hora"), delay == 0, "0"),
                            ("0,2 s", delay == 200, "200"),
                            ("0,4 s", delay == 400, "400"),
                            ("0,7 s", delay == 700, "700"),
                            ("1 s", delay == 1000, "1000"),
                            ("2 s", delay == 2000, "2000"),
                        ],
                        ("ui", "hover_audio_delay_ms"),
                        cx,
                    ),
                )
                .wide()
                .reset(("ui", "hover_audio_delay_ms"), delay == 400, "400"),
            );
        }
        vec![
            Group::titled(t!("Ouvir passando o mouse"), hover),
            Group::titled(
                t!("Visualizador e ícones"),
                vec![
                    Row::hint(
                        t!("Tocar ao abrir no visualizador"),
                        t!("Vídeos e sons começam a tocar assim que o visualizador os abre."),
                        self.switch("preview-autoplay", ui.preview_autoplay, ("ui", "preview_autoplay"), cx),
                    )
                    .reset(("ui", "preview_autoplay"), !ui.preview_autoplay, "false"),
                    Row::hint(
                        t!("Forma de onda nos ícones"),
                        t!("No modo Ícones, arquivos de som mostram a forma de onda no lugar do ícone."),
                        self.switch("audio-waveforms", ui.audio_waveforms, ("ui", "audio_waveforms"), cx),
                    )
                    .reset(("ui", "audio_waveforms"), !ui.audio_waveforms, "false"),
                ],
            ),
        ]
    }

    fn search_section(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let status = everything::status(cx);
        let source = status.source;
        // The own index's count while it is walked or read; with Everything
        // answering, what its index holds instead.
        let state = if source == Source::Everything {
            everything::external_text(status.external.as_ref())
        } else if status.service {
            // The names live in the service: this process holds none, and the
            // count is the service's to keep to itself.
            t!("O serviço de indexação guarda os nomes e responde às buscas.").to_string()
        } else {
            match (status.building, status.entries) {
                (Some(found), _) => tf!("Indexando… {n} itens até agora", n = found),
                (None, Some(entries)) => match status.built.and_then(|built| built.elapsed().ok()) {
                    Some(age) => tf!("{n} itens, atualizado {ago}", n = entries, ago = ago(age)),
                    None => tf!("{n} itens", n = entries),
                },
                (None, None) => t!("O índice é criado na primeira busca.").to_string(),
            }
        };
        let building = status.building.is_some();
        let own_index = source != Source::Everything;
        let picked = self.config.indexing.source;
        let mut rows = vec![
            Row::hint(
                t!("Buscar em todo o computador"),
                t!("Clique na lupa da barra de baixo ou aperte Ctrl+E e digite qualquer parte do nome. Vários termos filtram juntos, * e ? funcionam, e um termo com \\ procura no caminho."),
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_2()
                    .child(div().text_xs().text_color(rgb(theme::fg_secondary())).child(state))
                    .when(own_index && !building, |this| {
                        this.child(button("everything-rebuild", t!("Reindexar agora"), false).on_click(cx.listener(
                            |_page, _event, _window, cx| crate::explorer::everything::rebuild(cx),
                        )))
                    }),
            ),
            Row::hint(
                t!("Índice do computador"),
                t!("De onde vêm os nomes da busca em todo o computador. Com o Everything aberto, o índice dele serve e nada é criado aqui; o automático decide isso na primeira busca."),
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_2()
                    .child(self.config_choice(
                        "index-source",
                        &[
                            (t!("Automático"), picked == IndexSource::Auto, "\"auto\""),
                            (t!("Neste programa"), picked == IndexSource::PrimeFiles, "\"prime-files\""),
                            ("Everything", picked == IndexSource::Everything, "\"everything\""),
                        ],
                        ("indexing", "source"),
                        cx,
                    ))
                    .when(picked == IndexSource::Auto && source == Source::Own, |this| {
                        this.child(
                            div()
                                .max_w(px(280.0))
                                .text_xs()
                                .text_right()
                                .text_color(rgb(theme::fg_secondary()))
                                .child(everything::auto_note(status.external.as_ref())),
                        )
                    }),
            ),
        ];
        if own_index {
            use crate::explorer::everything::mft;
            let mft_state = mft::state();
            let (mft_busy, mft_error) = cx
                .try_global::<crate::explorer::everything::Everything>()
                .map_or((false, None), |state| (state.mft_busy, state.mft_error.clone()));
            let mft_text = match (mft_busy, mft_error, mft_state) {
                (true, _, _) => t!("Aguardando a permissão do Windows…").to_string(),
                (false, Some(error), _) => tf!("Não deu certo: {error}", error = error),
                (false, None, mft::State::On) => t!("Ligada").to_string(),
                (false, None, mft::State::Stopped) => t!("O serviço está instalado mas parado.").to_string(),
                (false, None, mft::State::Off) => t!("Desligada").to_string(),
            };
            let mft_on = mft_state == mft::State::On;
            rows.push(Row::hint(
                t!("Indexação instantânea"),
                t!("Lê a tabela dos discos NTFS como o Everything: o índice do computador fica pronto em segundos, também depois de reiniciar. Instala um serviço do Windows, com permissão de administrador uma vez."),
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_2()
                    // A failure's text can be a whole path: it wraps in a
                    // narrow column instead of pushing the description into
                    // one word a line.
                    .child(
                        div()
                            .max_w(px(280.0))
                            .text_xs()
                            .text_right()
                            .text_color(rgb(theme::fg_secondary()))
                            .child(mft_text),
                    )
                    .when(!mft_busy, |this| {
                        this.child(
                            button("everything-mft", if mft_on { t!("Desativar") } else { t!("Ativar") }, false)
                                .on_click(cx.listener(move |_page, _event, _window, cx| mft::set(!mft_on, cx))),
                        )
                    }),
            ));
            rows.push(Row::hint(
                t!("Como o índice é feito"),
                t!("Os nomes de todas as unidades fixas ficam guardados neste computador. Com o programa aberto, o que é criado ou renomeado entra na hora; com o serviço, ele guarda os nomes e os relê a cada 30 minutos, e sem o serviço o índice deste processo é refeito quando passa de 6 horas. Nada sai do computador."),
                div(),
            ));
        }
        vec![
            Group::new(rows),
            Group::titled(
                t!("Outras buscas"),
                vec![
                    Row::hint(
                        t!("Filtrar a pasta"),
                        t!("Comece a digitar na lista: só os nomes da pasta aberta que combinam ficam."),
                        key_cap(t!("Qualquer letra")),
                    ),
                    Row::hint(
                        t!("Buscar nas subpastas"),
                        t!("Procura a partir da pasta aberta, lendo o disco na hora."),
                        key_cap_owned(shortcuts::display(Action::SearchSubfolders).unwrap_or_else(|| t!("Nenhum").to_string())),
                    ),
                ],
            ),
        ]
    }

    fn integrations(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let reveal = self.config.explorer.reveal_from_other_apps;
        vec![Group::new(vec![
            Row::hint(
                t!("\"Mostrar na pasta\" abre aqui"),
                t!("Premiere (Revelar no Explorador), Chrome, Edge e Firefox (Mostrar na pasta, depois de baixar) e o menu Iniciar (Abrir local do arquivo) abrem a pasta no Prime Files, com o arquivo selecionado e piscando em destaque."),
                self.switch("reveal-from-other-apps", reveal, ("explorer", "reveal_from_other_apps"), cx),
            )
            .reset(("explorer", "reveal_from_other_apps"), reveal, "true"),
            Row::hint(
                t!("Iniciar com o Windows"),
                t!("Abre minimizado quando você entra no Windows, para que \"Mostrar na pasta\" de qualquer programa caia aqui na hora."),
                match self.starts_with_windows {
                    Some(on) => self.switch_pick("start-with-windows", on, Pick::StartWithWindows(!on), cx).into_any_element(),
                    None => checking().into_any_element(),
                },
            ),
            Row::hint(
                t!("Explorador padrão do Windows"),
                t!("Pastas, drives, Win+E e o ícone do Explorador na barra de tarefas abrem no Prime Files. Só neste usuário do Windows; desligar devolve tudo ao Explorador."),
                match self.default_explorer {
                    Some(is_default) => {
                        self.switch_pick("default-explorer", is_default, Pick::DefaultExplorer(!is_default), cx).into_any_element()
                    }
                    None => checking().into_any_element(),
                },
            ),
            Row::hint(
                "Listary",
                t!("Pastas abertas pelo Listary abrem no Prime Files. O Listary é reiniciado ao mudar."),
                match self.listary {
                    None => checking().into_any_element(),
                    Some(crate::listary::State::NotInstalled) => div()
                        .text_xs()
                        .text_color(rgb(theme::fg_secondary()))
                        .child(t!("Não instalado"))
                        .into_any_element(),
                    Some(state) => {
                        let ours = state == crate::listary::State::Ours;
                        self.switch_pick("listary", ours, Pick::Listary(!ours), cx).into_any_element()
                    }
                },
            ),
        ])]
    }

    fn shortcuts(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let bindings = &self.config.keybindings.bindings;
        let customized = shortcuts::ACTIONS.iter().any(|info| bindings.contains_key(info.id));
        let mut groups = vec![Group::new(vec![Row::hint(
            t!("Como mudar"),
            t!("Clique na tecla de um comando e aperte a combinação nova; Esc desiste. Um atalho que já era de outro comando sai dele. Vale na hora."),
            div().when(customized, |this| {
                this.child(button("shortcuts-reset-all", t!("Restaurar todos"), false).on_click(cx.listener(
                    |page, _event, _window, cx| page.pick(Pick::ResetAllShortcuts, cx),
                )))
            }),
        )])];
        let mut current: Option<(&'static str, Vec<Row>)> = None;
        for info in shortcuts::ACTIONS {
            if current.as_ref().is_some_and(|(group, _)| *group != info.group) {
                if let Some((group, rows)) = current.take() {
                    groups.push(Group::titled(tr(group), rows));
                }
            }
            let row = self.shortcut_row(info, bindings.contains_key(info.id), cx);
            current.get_or_insert_with(|| (info.group, Vec::new())).1.push(row);
        }
        if let Some((group, rows)) = current {
            groups.push(Group::titled(tr(group), rows));
        }
        groups.push(Group::titled(
            t!("Fixos"),
            FIXED_KEYS.iter().map(|(keys, what)| Row::new(tr(what), None, key_cap(tr(keys)))).collect(),
        ));
        groups
    }

    // A command, its keys as a button that records new ones, and a button to
    // leave it with none.
    fn shortcut_row(&self, info: &'static shortcuts::ActionInfo, customized: bool, cx: &mut Context<Self>) -> Row {
        let action = info.action;
        let recording = self.recording == Some(action);
        let keys: Vec<String> = shortcuts::chords(action).iter().map(Chord::display).collect();
        let has_keys = !keys.is_empty();
        let shown = if recording {
            t!("Aperte as teclas…").to_string()
        } else if has_keys {
            // Spellings of one key on other layouts are not worth a line.
            keys.into_iter().next().unwrap_or_default()
        } else {
            t!("Nenhum").to_string()
        };
        let control = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .id(ElementId::Name(SharedString::from(format!("shortcut-{}", info.id))))
                    .min_w(px(110.0))
                    .h(px(28.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .border_1()
                    .cursor_pointer()
                    .text_sm()
                    .whitespace_nowrap()
                    .map(|this| {
                        if recording {
                            this.border_color(rgb(theme::accent()))
                                .bg(rgb(theme::accent_subtle()))
                                .text_color(rgb(theme::accent()))
                        } else {
                            this.border_color(rgb(theme::border_hover()))
                                .bg(rgb(theme::bg()))
                                .text_color(rgb(if has_keys { theme::fg() } else { theme::muted() }))
                                .hover(|style| style.bg(rgb(theme::bg_hover())))
                        }
                    })
                    .on_click(cx.listener(move |page, _event, window, cx| {
                        window.focus(&page.focus_handle);
                        page.pick(Pick::RecordShortcut(action), cx);
                    }))
                    .child(shown),
            )
            .when(has_keys && !recording, |this| {
                this.child(
                    div()
                        .id(ElementId::Name(SharedString::from(format!("shortcut-clear-{}", info.id))))
                        .size(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(5.0))
                        .cursor_pointer()
                        .text_color(rgb(theme::muted()))
                        .hover(|style| style.bg(rgb(theme::bg_hover())).text_color(rgb(theme::fg())))
                        .tooltip(|window, cx| gpui_component::tooltip::Tooltip::new(t!("Remover o atalho")).build(window, cx))
                        .on_click(cx.listener(move |page, _event, _window, cx| page.pick(Pick::ClearShortcut(action), cx)))
                        .child(Icon::new(Icon::empty()).path("icons/close.svg").size_3()),
                )
            });
        let mut row = Row::new(tr(info.label), None, control);
        if customized {
            row.reset = Some(Pick::ResetShortcut(action));
        }
        row
    }

    fn about(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let path = config::paths::config_file();
        vec![
            Group::titled(
                t!("Atualizações"),
                vec![
                    Row::new(
                        t!("Versão"),
                        None,
                        div().text_sm().text_color(rgb(theme::fg())).child(env!("CARGO_PKG_VERSION")),
                    ),
                    self.updates_row(cx),
                    Row::hint(
                        t!("Atualizar automaticamente"),
                        t!("Procura uma versão nova ao abrir, baixa em segundo plano e instala quando você fecha o Prime Files."),
                        self.switch("updates-automatic", self.config.updates.automatic, ("updates", "automatic"), cx),
                    )
                    .reset(("updates", "automatic"), self.config.updates.automatic, "true"),
                ],
            ),
            Group::titled(
                t!("Tela de boas-vindas"),
                vec![Row::hint(
                    t!("Configuração inicial"),
                    t!("Os quatro passos do primeiro uso: idioma, tema, explorador padrão e integrações."),
                    button("open-welcome", t!("Abrir de novo"), false).on_click(cx.listener(|page, _event, _window, cx| {
                        page.write("ui", "welcome_done", "false".to_string(), cx);
                    })),
                )],
            ),
            Group::titled(
                t!("Arquivo de configuração"),
                vec![
                    Row::hint(
                        "config.toml",
                        t!("Tudo desta página fica nele, e editá-lo à mão vale na hora."),
                        div()
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap_2()
                            .child(div().text_xs().text_color(rgb(theme::fg_secondary())).child(path.display().to_string()))
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(button("open-config", t!("Abrir config.toml"), false).on_click(
                                        cx.listener(|page, _event, _window, cx| page.open_config_file(cx)),
                                    ))
                                    .child(button("show-config", t!("Mostrar na pasta"), false).on_click(
                                        cx.listener(|page, _event, _window, cx| page.show_config_folder(cx)),
                                    )),
                            ),
                    ),
                    Row::hint(
                        t!("Restaurar padrão"),
                        t!("Troca o arquivo pelo padrão; o atual fica guardado ao lado."),
                        button(
                            "reset-config",
                            if self.confirm_reset { t!("Clique de novo para confirmar") } else { t!("Restaurar padrão") },
                            self.confirm_reset,
                        )
                        .on_click(cx.listener(|page, _event, _window, cx| page.reset_config(cx))),
                    ),
                ],
            ),
        ]
    }

    fn updates_row(&self, cx: &mut Context<Self>) -> Row {
        use crate::updater::Status;
        let status = self.updater.read(cx).status.clone();
        let (text, error) = match &status {
            Status::NotInstalled => (t!("Esta cópia não foi instalada pelo instalador, então não se atualiza sozinha.").to_string(), false),
            Status::Idle => (t!("Ainda não procurou.").to_string(), false),
            Status::Checking => (t!("Procurando…").to_string(), false),
            Status::UpToDate => (t!("Você está na versão mais nova.").to_string(), false),
            Status::Downloading(version) => (tf!("Baixando a versão {version}…", version = version), false),
            Status::Ready { version, .. } => (tf!("A versão {version} está pronta para instalar.", version = version), false),
            Status::Failed(error) => (tf!("Não deu para atualizar: {error}", error = error), true),
        };
        let busy = matches!(status, Status::Checking | Status::Downloading(_));
        let can_check = !busy && status != Status::NotInstalled;
        let ready = matches!(status, Status::Ready { .. });
        Row::hint(
            t!("Procurar versão nova"),
            t!("Vêm das releases do Prime Files no GitHub; cada instalador é conferido pelo SHA256SUMS.txt antes de rodar."),
            div()
                .flex()
                .flex_col()
                .items_end()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .max_w(px(320.0))
                        .text_color(rgb(if error { theme::danger() } else { theme::fg_secondary() }))
                        .child(text),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .when(can_check, |row| {
                            row.child(button("check-updates", t!("Procurar agora"), false).on_click(cx.listener(
                                |page, _event, _window, cx| {
                                    page.updater.update(cx, |updater, cx| updater.check(cx));
                                },
                            )))
                        })
                        .when(ready, |row| {
                            row.child(button("install-update", t!("Atualizar agora"), false).on_click(cx.listener(
                                |page, _event, _window, cx| {
                                    page.updater.update(cx, |updater, cx| updater.install_now(cx));
                                },
                            )))
                        }),
                ),
        )
    }

    // ---------------------------------------------------------------------
    // Controls

    // An on/off switch for a boolean `[section] key`.
    fn switch(
        &self,
        id: &'static str,
        on: bool,
        (section, key): (&'static str, &'static str),
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let value = if on { "false" } else { "true" };
        self.switch_pick(id, on, Pick::Config(section, key, SharedString::from(value)), cx)
    }

    // An on/off switch whose click does `pick` (what turns it the other way).
    fn switch_pick(&self, id: &'static str, on: bool, pick: Pick, cx: &mut Context<Self>) -> Stateful<Div> {
        const WIDTH: f32 = 38.0;
        const HEIGHT: f32 = 22.0;
        const KNOB: f32 = 16.0;
        let inset = (HEIGHT - KNOB) / 2.0;
        div()
            .id(id)
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .on_click(cx.listener(move |page, _event, _window, cx| page.pick(pick.clone(), cx)))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(if on { theme::fg() } else { theme::fg_secondary() }))
                    .child(if on { t!("Ligado") } else { t!("Desligado") }),
            )
            .child(
                div()
                    .relative()
                    .w(px(WIDTH))
                    .h(px(HEIGHT))
                    .rounded_full()
                    .border_1()
                    .map(|track| {
                        if on {
                            track.bg(rgb(theme::accent())).border_color(rgb(theme::accent()))
                        } else {
                            track.bg(rgb(theme::bg())).border_color(rgb(theme::border_hover()))
                        }
                    })
                    .child(
                        div()
                            .absolute()
                            .top(px(inset - 1.0))
                            .left(px(if on { WIDTH - KNOB - inset - 1.0 } else { inset - 1.0 }))
                            .size(px(KNOB))
                            .rounded_full()
                            .bg(rgb(if on { theme::accent_fg() } else { theme::fg_secondary() })),
                    ),
            )
    }

    // A row of exclusive options whose every option writes its TOML value to
    // `key` (a `(section, key)` pair).
    fn config_choice(
        &self,
        id: &'static str,
        options: &[(&'static str, bool, &'static str)],
        (section, key): (&'static str, &'static str),
        cx: &mut Context<Self>,
    ) -> Div {
        let options: Vec<_> = options
            .iter()
            .map(|&(label, selected, value)| (label, selected, Pick::Config(section, key, SharedString::from(value))))
            .collect();
        self.chips(id, &options, false, cx)
    }

    // Independent on/off switches in one row: each chip flips its own key
    // (a `(section, key)` pair) between `true` and `false`.
    fn config_toggles(
        &self,
        id: &'static str,
        section: &'static str,
        options: &[(&'static str, bool, &'static str)],
        cx: &mut Context<Self>,
    ) -> Div {
        let options: Vec<_> = options
            .iter()
            .map(|&(label, on, key)| {
                (label, on, Pick::Config(section, key, SharedString::from(if on { "false" } else { "true" })))
            })
            .collect();
        self.chips(id, &options, true, cx)
    }

    // The row of chips behind `config_choice` and `config_toggles`. A
    // selected chip only answers a click when the chips are toggles, which
    // also show a check so "on" reads apart from "chosen".
    fn chips(&self, id: &'static str, options: &[(&'static str, bool, Pick)], toggles: bool, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            // Only long rows wrap: a short row beside its text measured at
            // its narrowest and broke onto two lines.
            .when(options.len() > 5, |this| this.flex_wrap())
            .p(px(2.0))
            .gap(px(2.0))
            .rounded(px(7.0))
            .bg(rgb(theme::bg()))
            .border_1()
            .border_color(rgb(theme::border()))
            .children(options.iter().enumerate().map(|(ix, (label, selected, pick))| {
                let selected = *selected;
                let pick = pick.clone();
                div()
                    .id(ElementId::NamedInteger(id.into(), ix as u64))
                    .h(px(26.0))
                    .px(px(11.0))
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .text_sm()
                    .whitespace_nowrap()
                    .map(|this| {
                        if selected {
                            this.bg(rgb(theme::accent())).text_color(rgb(theme::accent_fg()))
                        } else {
                            this.text_color(rgb(theme::fg_secondary()))
                                .hover(|style| style.bg(rgb(theme::bg_hover())).text_color(rgb(theme::fg())))
                        }
                    })
                    .when(!selected || toggles, |this| {
                        this.on_click(cx.listener(move |page, _event, _window, cx| page.pick(pick.clone(), cx)))
                    })
                    .when(toggles && selected, |this| this.child(Icon::new(Icon::empty()).path("icons/check.svg").size_3()))
                    .child(*label)
            }))
    }

    // ---------------------------------------------------------------------
    // Layout

    fn search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        let input = match &self.search {
            Some(input) => input.clone(),
            None => {
                let input = cx.new(|cx| InputState::new(window, cx));
                self.search = Some(input.clone());
                input
            }
        };
        // Set each render: the language may have changed since.
        input.update(cx, |state, cx| state.set_placeholder(t!("Buscar uma configuração…"), window, cx));
        input
    }

    fn render_search_field(&self, input: &Entity<InputState>) -> Div {
        div()
            .h(px(32.0))
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px(px(10.0))
            .rounded(px(7.0))
            .bg(rgb(theme::bg()))
            .border_1()
            .border_color(rgb(theme::border()))
            .child(Icon::new(Icon::empty()).path("icons/search.svg").size_4().text_color(rgb(theme::muted())))
            .child(div().flex_1().min_w(px(0.0)).child(Input::new(input).small().appearance(false).cleanable(true)))
    }

    fn render_nav_item(&self, section: SettingsSection, horizontal: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let active = section == self.section;
        div()
            .id(ElementId::Name(SharedString::from(format!("settings-nav-{section:?}"))))
            .h(px(34.0))
            .px(px(10.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .rounded(px(7.0))
            .cursor_pointer()
            .text_sm()
            .whitespace_nowrap()
            .map(|this| {
                if active {
                    this.bg(rgb(theme::accent_subtle())).text_color(rgb(theme::accent()))
                } else {
                    this.text_color(rgb(theme::fg_secondary()))
                        .hover(|style| style.bg(rgb(theme::bg_hover())).text_color(rgb(theme::fg())))
                }
            })
            .when(!horizontal, |this| this.w_full())
            .on_click(cx.listener(move |page, _event, window, cx| {
                // Picking a section ends a search: what was looked for is
                // now the section on screen.
                if let Some(input) = &page.search {
                    input.update(cx, |state, cx| state.set_value("", window, cx));
                }
                page.show_section(section, cx);
            }))
            .child(Icon::new(Icon::empty()).path(section.icon()).size_4())
            .child(section.label())
    }

    fn render_group(&self, group: Group, stacked: bool, cx: &mut Context<Self>) -> Div {
        let count = group.rows.len();
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .when_some(group.title, |this, title| {
                this.child(
                    div()
                        .px(px(4.0))
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::fg_secondary()))
                        .child(title),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .rounded(px(10.0))
                    .bg(rgb(theme::bg_secondary()))
                    .border_1()
                    .border_color(rgb(theme::border()))
                    .children(group.rows.into_iter().enumerate().map(|(ix, row)| {
                        self.render_row(row, stacked, ix + 1 < count, cx)
                    })),
            )
    }

    fn render_row(&self, row: Row, stacked: bool, divider: bool, cx: &mut Context<Self>) -> Div {
        let stacked = stacked || row.wide;
        let reset = row.reset.map(|pick| {
            div()
                .id(ElementId::Name(SharedString::from(format!("reset-{}", row.label))))
                .size(px(24.0))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(px(5.0))
                .cursor_pointer()
                .text_color(rgb(theme::muted()))
                .hover(|style| style.bg(rgb(theme::bg_hover())).text_color(rgb(theme::fg())))
                .tooltip(|window, cx| gpui_component::tooltip::Tooltip::new(t!("Voltar ao padrão")).build(window, cx))
                .on_click(cx.listener(move |page, _event, _window, cx| page.pick(pick.clone(), cx)))
                .child(Icon::new(Icon::empty()).path("icons/refresh-cw.svg").size_3())
        });
        // Stacked, the reset sits by the name (under it is the control);
        // side by side, between the text and the control.
        let (reset_by_label, reset_beside) = if stacked { (reset, None) } else { (None, reset) };
        let text = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .flex_1()
            .min_w(px(0.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().text_sm().font_weight(gpui::FontWeight::MEDIUM).text_color(rgb(theme::fg())).child(row.label))
                    // Stacked, the reset sits by the name: under it is the control.
                    .children(reset_by_label),
            )
            .when_some(row.hint, |this, hint| {
                this.child(div().text_xs().line_height(px(17.0)).text_color(rgb(theme::fg_secondary())).child(hint))
            });
        div()
            .px(px(16.0))
            .py(px(13.0))
            .when(divider, |this| this.border_b_1().border_color(rgb(theme::border())))
            .map(|this| {
                if stacked {
                    this.flex().flex_col().gap(px(10.0)).child(text).child(div().child(row.control))
                } else {
                    this.flex()
                        .items_center()
                        .justify_between()
                        .gap(px(20.0))
                        .child(text)
                        .children(reset_beside)
                        .child(div().flex_shrink_0().child(row.control))
                }
            })
    }
}

impl SettingsPage {
    fn render_section(&self, section: SettingsSection, stacked: bool, cx: &mut Context<Self>) -> Div {
        let groups = self.groups(section, cx);
        div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .size(px(40.0))
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .rounded(px(10.0))
                            .bg(rgb(theme::accent_subtle()))
                            .text_color(rgb(theme::accent()))
                            .child(Icon::new(Icon::empty()).path(section.icon()).size_5()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::fg()))
                                    .child(section.label()),
                            )
                            .child(div().text_sm().text_color(rgb(theme::fg_secondary())).child(section.description())),
                    ),
            )
            .children(groups.into_iter().map(|group| self.render_group(group, stacked, cx)))
    }

    // Every row whose name, explanation or section matches `query`, under its
    // section's name.
    fn render_results(&self, query: &str, stacked: bool, cx: &mut Context<Self>) -> Div {
        let folded = fold(query);
        let needle = folded.as_str();
        let mut found = 0;
        let mut body = div().flex().flex_col().gap(px(22.0));
        for section in SettingsSection::ALL {
            let section_matches =
                fold(section.label()).contains(needle) || fold(section.description()).contains(needle);
            let rows: Vec<Row> = self
                .groups(section, cx)
                .into_iter()
                .flat_map(|group| {
                    let group_matches = group.title.is_some_and(|title| fold(title).contains(needle));
                    group.rows.into_iter().filter(move |row| {
                        section_matches
                            || group_matches
                            || fold(row.label).contains(needle)
                            || row.hint.as_ref().is_some_and(|hint| fold(hint).contains(needle))
                    })
                })
                .collect();
            if rows.is_empty() {
                continue;
            }
            found += rows.len();
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id(ElementId::Name(SharedString::from(format!("result-section-{section:?}"))))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px(px(4.0))
                            .cursor_pointer()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::fg()))
                            .hover(|style| style.text_color(rgb(theme::accent())))
                            .on_click(cx.listener(move |page, _event, window, cx| {
                                if let Some(input) = &page.search {
                                    input.update(cx, |state, cx| state.set_value("", window, cx));
                                }
                                page.show_section(section, cx);
                            }))
                            .child(Icon::new(Icon::empty()).path(section.icon()).size_4().text_color(rgb(theme::accent())))
                            .child(section.label()),
                    )
                    .child(self.render_group(Group::new(rows), stacked, cx)),
            );
        }
        if found == 0 {
            body = body.child(
                div()
                    .py(px(40.0))
                    .flex()
                    .justify_center()
                    .text_sm()
                    .text_color(rgb(theme::fg_secondary()))
                    .child(tf!("Nenhuma configuração com \"{query}\".", query = query.trim())),
            );
        }
        body
    }

    fn render_status(&self, cx: &mut Context<Self>) -> Option<Div> {
        let (message, error) = self.status.clone()?;
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px(px(14.0))
                .py(px(10.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(rgb(if error { theme::danger() } else { theme::border() }))
                .bg(rgb(theme::bg_secondary()))
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(rgb(if error { theme::danger() } else { theme::fg() }))
                        .child(message),
                )
                .child(
                    div()
                        .id("settings-status-close")
                        .size(px(22.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .text_color(rgb(theme::muted()))
                        .hover(|style| style.bg(rgb(theme::bg_hover())).text_color(rgb(theme::fg())))
                        .on_click(cx.listener(|page, _event, _window, cx| {
                            page.status = None;
                            cx.notify();
                        }))
                        .child(Icon::new(Icon::empty()).path("icons/close.svg").size_3()),
                ),
        )
    }
}

// Lowercase without accents, so "audio" finds "Áudio" and "configuracao"
// finds "configuração".
fn fold(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            'ß' => 's',
            other => other,
        })
        .collect()
}

// A keycap for text built at run time.
fn key_cap_owned(keys: String) -> Div {
    div()
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(5.0))
        .bg(rgb(theme::bg()))
        .border_1()
        .border_color(rgb(theme::border_hover()))
        .text_xs()
        .text_color(rgb(theme::fg()))
        .whitespace_nowrap()
        .child(keys)
}

// A key or key combination drawn as a keycap.
fn key_cap(keys: &'static str) -> Div {
    div()
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(5.0))
        .bg(rgb(theme::bg()))
        .border_1()
        .border_color(rgb(theme::border_hover()))
        .text_xs()
        .text_color(rgb(theme::fg()))
        .whitespace_nowrap()
        .child(keys)
}

fn checking() -> Div {
    div().text_xs().text_color(rgb(theme::fg_secondary())).child(t!("Verificando…"))
}

// "agora", "há 3 min", "há 2 h", "há 4 dias".
fn ago(age: std::time::Duration) -> String {
    let minutes = age.as_secs() / 60;
    if minutes < 1 {
        t!("agora mesmo").to_string()
    } else if minutes < 60 {
        tf!("há {n} min", n = minutes)
    } else if minutes < 48 * 60 {
        tf!("há {n} h", n = minutes / 60)
    } else {
        tf!("há {n} dias", n = minutes / (24 * 60))
    }
}

fn button(id: &'static str, label: &'static str, danger: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .text_sm()
        .whitespace_nowrap()
        .border_1()
        .bg(rgb(theme::bg()))
        .map(|this| {
            if danger {
                this.border_color(rgb(theme::danger())).text_color(rgb(theme::danger()))
            } else {
                this.border_color(rgb(theme::border_hover())).text_color(rgb(theme::fg()))
            }
        })
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .child(label)
}

/// Window narrower than this puts the sections in a row of tabs on top.
const NARROW: f32 = 760.0;
/// Content narrower than this puts each control under its text.
const STACK: f32 = 620.0;
const NAV_WIDTH: f32 = 232.0;

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = self.search_input(window, cx);
        self.app_name_input(window, cx);
        // The font list needs the window's text system, so it is made here.
        self.ensure_font_select(window, cx);
        let query = input.read(cx).value().to_string();
        let searching = !query.trim().is_empty();
        let width = f32::from(window.viewport_size().width);
        let narrow = width < NARROW;
        let content_width = if narrow { width } else { width - NAV_WIDTH };
        let stacked = content_width < STACK;

        let body = if searching {
            self.render_results(&query, stacked, cx)
        } else {
            self.render_section(self.section, stacked, cx)
        };
        let status = self.render_status(cx);

        let nav = if narrow {
            div()
                .w_full()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap_2()
                .px(px(12.0))
                .pt(px(10.0))
                .pb(px(8.0))
                .border_b_1()
                .border_color(rgb(theme::border()))
                .bg(rgb(theme::toolbar_bg()))
                .child(self.render_search_field(&input))
                .child(
                    div()
                        .id("settings-nav-row")
                        .w_full()
                        .flex()
                        .gap_1()
                        .overflow_x_scroll()
                        .children(SettingsSection::ALL.into_iter().map(|section| self.render_nav_item(section, true, cx))),
                )
        } else {
            div()
                .w(px(NAV_WIDTH))
                .flex_shrink_0()
                .h_full()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .p(px(12.0))
                .border_r_1()
                .border_color(rgb(theme::border()))
                .bg(rgb(theme::toolbar_bg()))
                // Flex, so the field takes the full width: in a plain block
                // its `w_full` resolved to its content's width.
                .child(div().pb(px(10.0)).flex().flex_col().child(self.render_search_field(&input)))
                .children(SettingsSection::ALL.into_iter().map(|section| self.render_nav_item(section, false, cx)))
        };

        div()
            .size_full()
            .flex()
            .map(|this| if narrow { this.flex_col() } else { this.flex_row() })
            .bg(rgb(theme::bg()))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|page, event: &KeyDownEvent, _window, cx| page.record_key(event, cx)))
            .child(nav)
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .px(px(if narrow { 14.0 } else { 32.0 }))
                    .py(px(24.0))
                    .child(
                        div()
                            .max_w(px(820.0))
                            .flex()
                            .flex_col()
                            .gap_4()
                            .children(status)
                            .child(body),
                    ),
            )
    }
}

impl crate::Page for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        <Self as Render>::render(self, window, cx).into_any_element()
    }
}

// The pointer half of a wheel control: a `canvas` that knows the element's
// own bounds and listens on the window, the way every drag in this program
// does (the zoom slider, the panel grips). A listener hung on the element
// itself is not seen here: the settings page draws its rows inside a
// scrolling body, which parts the hit-testing from the widget.
fn pointer_events(page: Entity<SettingsPage>, part: WheelPart) -> AnyElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let press = page.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble
                    && event.button == MouseButton::Left
                    && bounds.contains(&event.position)
                {
                    press.update(cx, |page, cx| page.wheel_press(part, bounds, event.position, cx));
                }
            });
            let drag = page.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble
                    && event.pressed_button == Some(MouseButton::Left)
                    && drag.read(cx).wheel.as_ref().is_some_and(|wheel| wheel.dragging == Some(part))
                {
                    drag.update(cx, |page, cx| page.wheel_apply(part, bounds, event.position, cx));
                }
            });
            let release = page;
            window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                if release.read(cx).wheel.as_ref().is_some_and(|wheel| wheel.dragging == Some(part)) {
                    release.update(cx, |page, cx| page.wheel_release(cx));
                }
            });
        },
    )
    .absolute()
    // 显式覆盖控件，避免事件层按前面的色轮图片定位到下方。
    .top(px(0.0))
    .left(px(0.0))
    .size_full()
    .into_any_element()
}

#[cfg(test)]
#[path = "settings/tests.rs"]
mod tests;
