//! Root view of the Explorer window — one of the app's two top-level "pillars"
//! (the other being the launcher window, `nohrs-launcher`, added in P3).
//!
//! This lives in `nohrs-pages` rather than the binary because it is the
//! Explorer pillar's window root: it owns the page entities and routes between
//! them, depending only downward on `nohrs-ui` (chrome) and `nohrs-services`
//! (search). The binary just opens a window hosting this view; the future
//! launcher window will be a symmetric root in `nohrs-launcher`.

use nohrs_core::{t, tf};
use crate::explorer::ExplorerPage;
use crate::explorer::StatusLevel;
use crate::{
    PageKind, git::GitPage, s3::S3Page,
    settings::{SettingsPage, SettingsSection},
};
use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, Render, SharedString, WeakEntity, Window, div, prelude::*, px, rgb,
};
use gpui_component::resizable::ResizableState;
use gpui_component::{Icon, IconName, Root, Theme, ThemeMode as GpuiThemeMode, WindowExt as _};
use nohrs_core::config::{self, Config, ConfigOverride, ConfigWatcher};
use nohrs_core::telemetry::LogErr;
use nohrs_services::fs::trash::Ledger as TrashLedger;
use nohrs_services::search::SearchService;
use nohrs_store::KvStore;
use nohrs_ui::components::layout::footer::{FooterProps, StatusTone, footer};
use nohrs_ui::components::layout::unified_toolbar::{
    AccountMenuAction, AccountMenuCommand, unified_toolbar,
};
use nohrs_ui::theme::theme;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

// The explorer's own status levels, in the terms the footer paints in. Two
// vocabularies rather than one because `nohrs-ui` knows nothing about file
// operations and should not: what reaches it is how a message should read, not
// what produced it.
fn tone_of(level: StatusLevel) -> StatusTone {
    match level {
        StatusLevel::Info => StatusTone::Normal,
        StatusLevel::Warning => StatusTone::Warning,
        StatusLevel::Error => StatusTone::Error,
    }
}

/// The application root view that hosts the page sidebar, the active page, and
/// the shared configuration and search state.
pub struct RootView {
    current_page: PageKind,
    focus_handle: FocusHandle,
    // Page entities
    explorer: Entity<ExplorerPage>,
    git: Entity<GitPage>,
    s3: Entity<S3Page>,
    settings: Entity<SettingsPage>,
    // The welcome screen, while `ui.welcome_done` is false: it covers the
    // pages until it is finished or skipped.
    welcome: Option<Entity<crate::welcome::WelcomePage>>,
    search_service: Option<Arc<SearchService>>,
    indexing_progress: Option<f32>,
    // Currently-applied configuration and the inputs needed to recompute it on
    // hot reload: the file path and the env/CLI override layers that sit above
    // the file (config.md §3).
    config: Config,
    config_path: PathBuf,
    config_overrides: Vec<ConfigOverride>,
    // Latest config load error, surfaced in the footer. Held here (not in the
    // explorer's transient status) so it is not cleared by an explorer
    // directory reload and survives across pages.
    config_status: Option<String>,
    // Kept alive for the window's lifetime so the OS watch is not dropped.
    _config_watcher: Option<ConfigWatcher>,
    // The font family `ui.font_family` names, applied to the whole window;
    // empty keeps the system's own.
    font_family: SharedString,
}

impl RootView {
    /// Build the Explorer window root: instantiate the page entities and start
    /// the indexing-progress poll. `resizable` is created at the application
    /// level and the search service is initialized by the binary (it owns the
    /// async runtime), keeping `nohrs-pages` free of runtime concerns.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        resizable: Entity<ResizableState>,
        search_service: Option<Arc<SearchService>>,
        store: Option<Arc<dyn KvStore>>,
        trash_ledger: TrashLedger,
        config: Config,
        config_path: PathBuf,
        config_overrides: Vec<ConfigOverride>,
        config_error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let restore_tabs = config.explorer.restore_tabs;
        let explorer = cx.new(|cx| {
            ExplorerPage::new(
                resizable,
                search_service.clone(),
                store,
                trash_ledger,
                restore_tabs,
                window,
                cx,
            )
        });
        let git = cx.new(|_cx| GitPage::new());
        let s3 = cx.new(|_cx| S3Page::new());
        let settings = cx.new(SettingsPage::new);

        let mut view = RootView {
            current_page: PageKind::Explorer,
            focus_handle,
            explorer,
            git,
            s3,
            settings,
            welcome: None,
            search_service,
            indexing_progress: Some(1.0), // Start as hidden/done
            // Start from defaults so the initial `apply_config` below treats the
            // loaded config as a change and applies theme + ui uniformly.
            config: Config::default(),
            config_path,
            config_overrides,
            config_status: None,
            _config_watcher: None,
            font_family: SharedString::default(),
        };
        view.start_progress_loop(window, cx);
        view.apply_config(config, config_error, window, cx);
        view.start_config_watch(window, cx);
        crate::ipc::serve(window, cx);
        crate::file_dialog::watch(window, cx);
        crate::reveal::watch(window, cx);
        view.start_updates(window, cx);
        view
    }

    // Looks for an update a few seconds after the window is up (so it never
    // competes with the first listing), and says so when one is ready.
    fn start_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let updater = crate::updater::updater(cx);
        let mut announced = false;
        cx.observe_in(&updater, window, move |_root, updater, window, cx| {
            if let crate::updater::Status::Ready { version, .. } = &updater.read(cx).status
                && !announced
            {
                announced = true;
                let message = if updater.read(cx).automatic {
                    tf!("A versão {version} foi baixada e será instalada quando você fechar o Prime Files.", version = version)
                } else {
                    tf!("A versão {version} foi baixada. Instale em Configurações › Sobre.", version = version)
                };
                window.push_notification(
                    gpui_component::notification::Notification::info(message).title(t!("Atualização pronta")),
                    cx,
                );
            }
        })
        .detach();
        if !self.config.updates.automatic {
            return;
        }
        cx.spawn(async move |_root, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(5)).await;
            if let Err(error) = updater.update(cx, |updater, cx| updater.check(cx)) {
                tracing::debug!("app closed before the update check: {error}");
            }
        })
        .detach();
    }

    /// Apply a freshly-merged configuration to the live UI: switch the theme
    /// mode, propagate `[ui]` settings to the explorer, and surface any load
    /// error in the status bar. Safe to call repeatedly (hot reload).
    pub fn apply_config(
        &mut self,
        config: Config,
        config_error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Resolved on every apply rather than only when the setting changed:
        // the starting `Config::default()` is already `system`, so on a dark OS
        // the first apply would otherwise find nothing to do and leave both
        // palettes light.
        let mode = match config.theme.mode {
            config::ThemeMode::Light => GpuiThemeMode::Light,
            config::ThemeMode::Dark => GpuiThemeMode::Dark,
            // `system` follows the OS appearance reported by the window.
            config::ThemeMode::System => GpuiThemeMode::from(window.appearance()),
        };
        crate::explorer::note_theme_mode(config.theme.mode);
        // Every view reads the language while it renders, so a change only
        // needs all windows to draw again (the settings page, the menus, the
        // title bar — not only the explorer this function updates).
        if nohrs_core::i18n::apply_preference(&config.ui.language) {
            cx.refresh_windows();
        }
        // The explorer reads the keymap on every key press and the menus show
        // its keys; only the page's own actions need binding again.
        if nohrs_core::shortcuts::apply(&config.keybindings.bindings) {
            crate::explorer::rebind_shortcuts(cx);
            cx.refresh_windows();
        }
        // Before `restyle_components`, which reads these tokens.
        theme::set_dark(mode.is_dark());
        theme::set_accent(theme::parse_accent(&config.theme.accent));
        theme::set_background(theme::parse_color(&config.theme.background));
        Theme::change(mode, Some(window), cx);
        // The widgets gpui_component draws read their family from its theme;
        // everything else inherits it from the root element in `render`.
        self.font_family = config.ui.font_family.trim().to_string().into();
        Theme::global_mut(cx).font_family = if self.font_family.is_empty() {
            ".SystemUIFont".into()
        } else {
            self.font_family.clone()
        };

        // Applied unconditionally: `Theme::change` restores the shipped palette,
        // and on the first call the mode often already matches, so this cannot
        // hang off the branch above.
        Self::restyle_components(cx);

        // Condense the (possibly multi-line) diagnostic to a single line plus the
        // file path for the one-line status bar; full detail is in the logs.
        self.config_status = config_error.as_ref().map(|error| {
            let summary = error.lines().next().unwrap_or(error.as_str());
            format!("config: {summary} ({})", self.config_path.display())
        });

        crate::reveal::set_enabled(config.explorer.reveal_from_other_apps);
        // Where the whole-computer search gets its names may have changed;
        // the explorer's index state follows the file like any other setting.
        crate::explorer::everything::set_engine(config.indexing.source, cx);
        let ui = config.ui.clone();
        let explorer_cfg = config.explorer.clone();
        let compact = config.compact.clone();
        let layout = config.layout.clone();
        self.explorer.update(cx, |page, cx| {
            page.apply_config_ui(&ui, cx);
            page.apply_config_explorer(&explorer_cfg, cx);
            page.apply_config_compact(&compact, cx);
            page.apply_config_layout(&layout, cx);
        });
        let shown = config.clone();
        self.settings.update(cx, |page, cx| page.set_config(shown, cx));
        if config.ui.welcome_done {
            self.welcome = None;
        } else {
            match &self.welcome {
                Some(welcome) => {
                    let shown = config.clone();
                    welcome.update(cx, |page, cx| page.set_config(shown, cx));
                }
                None => {
                    let shown = config.clone();
                    self.welcome = Some(cx.new(|cx| crate::welcome::WelcomePage::new(shown, cx)));
                }
            }
        }
        let automatic = config.updates.automatic;
        crate::updater::updater(cx).update(cx, |updater, cx| updater.set_automatic(automatic, cx));

        self.config = config;
        cx.notify();
    }

    /// Reconcile `gpui_component`'s palette with this app's tokens, for the two
    /// places where its defaults read wrong here. Must be re-applied after every
    /// `Theme::change`, which restores the shipped values.
    ///
    /// - `overlay` ships at 5% black, invisible against these white surfaces, so
    ///   the file-operation dialogs read as floating panels rather than modals.
    /// - `primary` ships near-black and drives every accent-bearing widget
    ///   (buttons, checkbox, switch, radio, tabs), which left the dialogs
    ///   looking unrelated to the rest of the window.
    fn restyle_components(cx: &mut App) {
        let palette = Theme::global_mut(cx);
        palette.overlay = gpui::Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: theme::MODAL_SCRIM_ALPHA,
        }
        .into();
        palette.primary = rgb(theme::accent()).into();
        palette.primary_hover = rgb(theme::accent_hover()).into();
        palette.primary_active = rgb(theme::accent_hover()).into();
        palette.primary_foreground = rgb(theme::accent_fg()).into();

        // The shipped dark palette is near-black, and its code-editor style
        // paints the line-number gutter in its own editor background: the
        // preview showed a black strip down the side of every text file. Both
        // follow this app's surfaces instead.
        palette.background = rgb(theme::bg()).into();
        palette.foreground = rgb(theme::fg()).into();
        palette.border = rgb(theme::border()).into();
        palette.muted_foreground = rgb(theme::fg_secondary()).into();
        let editor = &mut Arc::make_mut(&mut palette.highlight_theme).style;
        editor.editor_background = Some(rgb(theme::bg()).into());
        editor.editor_active_line = Some(rgb(theme::bg_hover()).into());
        editor.editor_line_number = Some(rgb(theme::muted()).into());
        editor.editor_active_line_number = Some(rgb(theme::fg_secondary()).into());
    }

    /// The strip shown while a browser's Open dialog waits on Nohrs: what is
    /// waiting, how to answer it, and a way to give the dialog back.
    fn render_picking_banner(
        &self,
        dialog: crate::file_dialog::Dialog,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let browser = dialog.browser.trim_end_matches(".exe").to_string();
        div()
            .id("picking-banner")
            .flex_shrink_0()
            .h(px(36.0))
            .px(px(16.0))
            .flex()
            .items_center()
            .gap_3()
            .bg(rgb(theme::accent_subtle()))
            .border_b_1()
            .border_color(rgb(theme::accent()))
            .text_sm()
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(rgb(theme::accent()))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(tf!("Escolhendo para {browser} ({title})", browser = browser, title = dialog.title)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(theme::fg_secondary()))
                    .child(t!("Duplo clique ou Enter num arquivo para enviá-lo. Esc devolve a janela.")),
            )
            .child(
                div()
                    .id("picking-cancel")
                    .flex_shrink_0()
                    .px(px(10.0))
                    .py(px(3.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .text_xs()
                    .border_1()
                    .border_color(rgb(theme::accent()))
                    .text_color(rgb(theme::accent()))
                    .hover(|button| button.bg(rgb(theme::bg_hover())))
                    .on_click(cx.listener(|_root, _, _, cx| {
                        crate::file_dialog::cancel();
                        cx.notify();
                    }))
                    .child(t!("Cancelar")),
            )
    }

    /// Watch `config.toml` and re-apply on change. The `notify` callback runs on
    /// a background thread and only pings a channel; a foreground poll loop does
    /// the reload so all entity updates happen on the GPUI thread.
    fn start_config_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (sender, receiver) = mpsc::channel::<()>();
        match ConfigWatcher::new(&self.config_path, move || {
            // A closed channel just means the app is shutting down.
            sender.send(()).log_err();
        }) {
            Ok(watcher) => self._config_watcher = Some(watcher),
            Err(error) => {
                tracing::warn!("config hot reload disabled: {error}");
                return;
            }
        }

        cx.spawn_in(
            window,
            move |this: WeakEntity<RootView>, cx: &mut AsyncWindowContext| {
                let mut cx = cx.clone();
                async move {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(400))
                            .await;
                        let mut changed = false;
                        let mut disconnected = false;
                        loop {
                            match receiver.try_recv() {
                                Ok(()) => changed = true,
                                Err(mpsc::TryRecvError::Empty) => break,
                                // The watcher was dropped; stop polling rather
                                // than spinning every 400ms forever.
                                Err(mpsc::TryRecvError::Disconnected) => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                        if disconnected {
                            break;
                        }
                        if !changed {
                            continue;
                        }
                        let update = this.update_in(&mut cx, |this, window, cx| {
                            let (mut config, diagnostics) =
                                config::load_from_path(&this.config_path);
                            for over in &this.config_overrides {
                                config.apply_override(over);
                            }
                            let config_error = config::report_diagnostics(&diagnostics);
                            this.apply_config(config, config_error, window, cx);
                        });
                        if update.is_err() {
                            break; // The view (and window) is gone.
                        }
                    }
                }
            },
        )
        .detach();
    }

    /// The explorer page, for requests arriving over the IPC channel.
    pub(crate) fn explorer_entity(&self) -> Entity<ExplorerPage> {
        self.explorer.clone()
    }

    /// Switches to the file explorer, from whichever page is showing.
    pub(crate) fn show_explorer(&mut self, cx: &mut Context<Self>) {
        self.set_page(PageKind::Explorer, cx);
    }

    /// Switches the active page, notifying for a redraw only if it changed.
    pub fn set_page(&mut self, page: PageKind, cx: &mut Context<Self>) {
        if self.current_page != page {
            self.current_page = page;
            cx.notify();
        }
    }

    fn start_progress_loop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Without a search service there is no indexing progress to poll, so avoid
        // rescheduling a per-frame no-op forever.
        if self.search_service.is_none() {
            return;
        }

        if let Some(progress) = self.check_progress_update() {
            self.indexing_progress = Some(progress);
            cx.notify();
        }

        // Poll every frame (simple and effective for this case)
        cx.on_next_frame(window, |view: &mut RootView, window, cx| {
            view.start_progress_loop(window, cx);
        });
    }

    fn check_progress_update(&self) -> Option<f32> {
        let service = self.search_service.as_ref()?;
        let rx = service.progress_subscription();
        // Just read current value
        let val = *rx.borrow();
        Some(val)
    }
}

impl Focusable for RootView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // On the explorer with one pane the tab row is the title bar, as in
        // the Explorer of Windows 11; elsewhere the toolbar holds the buttons.
        let own_title_bar = self.welcome.is_some()
            || self.current_page != PageKind::Explorer
            || !self.explorer.read(cx).tabs_in_title_bar();
        let toolbar = own_title_bar.then(|| unified_toolbar(window, cx));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::bg()))
            .relative()
            // Every text in the window inherits the family chosen in Settings.
            .when(!self.font_family.is_empty(), |this| this.font_family(self.font_family.clone()))
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::handle_account_action))
            .children(toolbar)
            .when_some(crate::file_dialog::picking(), |this, dialog| {
                this.child(self.render_picking_banner(dialog, cx))
            })
            .child(
                // Main content: toolbar + page
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .min_h(px(0.0))
                    .child(
                        // Main content area - render active page
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .min_w(px(0.0))
                            .child(match &self.welcome {
                                Some(welcome) => welcome.clone().into_any_element(),
                                None => self.render_active_page(window, cx),
                            }),
                    ),
            )
            .children(
                // Footer status bar
                {
                    // A config load error takes precedence over the explorer's
                    // transient status and is always shown as an error.
                    let (status_message, status_tone) = match &self.config_status {
                        Some(message) => (Some(message.clone()), StatusTone::Error),
                        None => match self.explorer.read(cx).status_for_footer(cx) {
                            Some((text, level)) => (Some(text), tone_of(level)),
                            None => (None, StatusTone::Normal),
                        },
                    };
                    let (selected_count, total_count) = self.explorer.read(cx).selection_counts(cx);
                    let current_path = self.explorer.read(cx).current_path(cx);
                    let props = FooterProps {
                        selected_count,
                        total_count,
                        current_path,
                        indexing_progress: self.indexing_progress,
                        status_message,
                        status_tone,
                        ..Default::default()
                    };
                    // The explorer has its own bar (filter, counts, zoom), as
                    // File Pilot does; the footer stays for the other pages
                    // and for a config file that failed to load.
                    let shown = self.welcome.is_none()
                        && (self.current_page != PageKind::Explorer || self.config_status.is_some());
                    shown.then(|| footer(props, None, cx))
                },
            )
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

impl RootView {
    fn handle_account_action(
        &mut self,
        action: &AccountMenuAction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let section = match action.command {
            AccountMenuCommand::Settings => SettingsSection::General,
            AccountMenuCommand::Keymap => SettingsSection::Shortcuts,
            AccountMenuCommand::Themes => SettingsSection::Appearance,
        };
        self.settings.update(cx, |page, cx| page.show_section(section, cx));
        self.set_page(PageKind::Settings, cx);
    }

    fn render_active_page(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let page = match self.current_page {
            PageKind::Explorer => return self.explorer.clone().into_any_element(),
            PageKind::Git => self.git.clone().into_any_element(),
            PageKind::S3 => self.s3.clone().into_any_element(),
            PageKind::Settings => self.settings.clone().into_any_element(),
        };
        // The page rail is gone, so the pages the account menu opens carry
        // their own way back; without it they would be a dead end.
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("back-to-explorer")
                    .flex_shrink_0()
                    .h(px(36.0))
                    .px(px(16.0))
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(rgb(theme::fg_secondary()))
                    .border_b_1()
                    .border_color(rgb(theme::border()))
                    .hover(|style| style.text_color(rgb(theme::fg())))
                    .on_click(cx.listener(|view, _event, _window, cx| {
                        view.set_page(PageKind::Explorer, cx);
                    }))
                    .child(Icon::new(IconName::ArrowLeft).size_4())
                    .child(t!("Voltar aos arquivos")),
            )
            .child(div().flex_1().min_h(px(0.0)).child(page))
            .into_any_element()
    }
}

