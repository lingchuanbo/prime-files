//! Application entry / startup sequence.
//!
//! The binary is the only layer allowed to depend on every crate, so it wires
//! the pillars together: it builds the shared window chrome from `nohrs-ui`,
//! initializes services that need the async runtime, and opens the Explorer
//! window hosting `nohrs_pages::RootView` and the launcher window hosting
//! `nohrs_launcher::LauncherView`. Neither pillar references the other; the
//! binary owns the keybinding that summons the launcher from anywhere.

use crate::cli::{Cli, Command};
use gpui::{App, AppContext, Application, Bounds, KeyBinding, WindowAppearance, actions, px, size};
use gpui_component::Root;
use gpui_component::resizable::ResizableState;
use nohrs_core::config::{self, ConfigOverride, ThemeMode};
use nohrs_core::telemetry::logging::{FileLogConfig, init_logging_with_file};
use nohrs_launcher::{LauncherIndex, ToggleLauncher};
use nohrs_pages::RootView;
use nohrs_services::fs::trash;
use nohrs_services::search::SearchService;
use nohrs_store::{KvStore, RedbKvStore, StoreLogConfig};
use nohrs_ui::assets::Assets;
use nohrs_ui::components::layout::unified_toolbar::UNIFIED_TOOLBAR_HEIGHT;
use nohrs_ui::window::{self, traffic_lights::TrafficLightsHook};
use std::sync::Arc;

pub struct NohrsApp;

actions!(nohrs, [OpenStandaloneSearch]);

impl NohrsApp {
    pub fn run(cli: &Cli) {
        // Before anything can panic. A released window has no console, so a
        // crash with `diagnostics.runtime_log` off (the default) would leave
        // nothing behind; this writes the one line that says where it died.
        nohrs_core::telemetry::logging::install_panic_log(nohrs_core::config::paths::log_dir());
        // Load configuration before opening the window: defaults < file < env <
        // CLI (config.md §3). A missing file is created with defaults so users
        // have something to edit and the watcher has a target. Parse failures are
        // non-fatal — we fall back to defaults and surface the error in the UI.
        let config_path = config::paths::freeze_active_config_dir().join("config.toml");
        let config_creation = config::ensure_exists(&config_path);
        let (mut config, diagnostics) = config::load_from_files(
            &config_path,
            &config::paths::appearance_file(),
            &config::paths::search_file(),
        );
        let config_overrides = vec![ConfigOverride::from_env(), cli.overrides()];
        for over in &config_overrides {
            config.apply_override(over);
        }
        let _log_guard = init_logging_with_file(&FileLogConfig {
            enabled: config.diagnostics.runtime_log,
            ..FileLogConfig::default()
        });
        if let Err(error) = config_creation {
            tracing::warn!("could not create {}: {error}", config_path.display());
        }
        let config_error = config::report_diagnostics(&diagnostics);

        // Before any window draws a word: "auto" needs to know the answer.
        nohrs_pages::locale::detect_and_record();
        nohrs_core::i18n::apply_preference(&config.ui.language);
        nohrs_core::shortcuts::apply(&config.keybindings.bindings);

        // GPUI drives the app (replacing `#[tokio::main]`; ADR 0004, async-runtime.md
        // §7). The search service is now tokio-free — its file watcher and progress
        // channels run on std threads and runtime-agnostic channels — so no async
        // runtime needs to be entered here.
        let launcher_only = matches!(cli.command, Some(Command::Launcher));
        let initial_path = cli.path.clone();
        let background = cli.background;

        Application::new().with_assets(Assets).run(move |app: &mut App| {
            gpui_component::init(app);
            apply_theme_mode(config.theme.mode, app);
            // The launcher window draws with the same tokens as the explorer.
            nohrs_ui::theme::theme::set_accent(nohrs_ui::theme::theme::parse_accent(&config.theme.accent));
            nohrs_ui::theme::theme::set_background(nohrs_ui::theme::theme::parse_color(&config.theme.background));

            // The name index is built once for the whole run and shared by every
            // launcher window, so summoning the launcher never waits on a scan.
            let launcher = LauncherIndex::start(app);
            // Two ways in (docs/launcher.md §2): a chord the OS routes to nohrs
            // from any application, and one GPUI handles while nohrs is in front.
            launcher.install_global_hotkey(app);
            install_in_app_launcher_key(launcher.clone(), app);
            install_standalone_search_key(app);
            install_quick_launch_action(app);

            if launcher_only {
                // Resident mode: nohrs is a background process whose only job is
                // to answer the global hotkey, so it must outlive each dismissal
                // of the launcher window.
                // §15.1: the quick launch runs here too, with the same
                // initialization function but no store: redb is a single-process
                // lock, and the main program must keep its tabs and session.
                nohrs_pages::quick_launch::init(
                    app,
                    None,
                    QUICK_LAUNCH_HOTKEY,
                    nohrs_pages::quick_launch::Mode::Standalone,
                );
                if let Err(error) = nohrs_launcher::open_keep_alive_window(app) {
                    tracing::error!("failed to open the keep-alive window: {error}");
                }
                if let Err(error) = launcher.toggle(app) {
                    tracing::error!("failed to open launcher window: {error}");
                }
                return;
            }

            let resizable = app.new(|_| ResizableState::default());
            let bounds = Bounds::centered(
                None,
                size(px(config::WINDOW_WIDTH), px(config::WINDOW_HEIGHT)),
                app,
            );
            let traffic_lights = TrafficLightsHook::new().center_vertically(UNIFIED_TOOLBAR_HEIGHT);
            let window_options = window::unified_window_options(bounds, &traffic_lights);

            // Open the host KV store (`state.redb`) for tab/session restore
            // (docs/persistence.md §3). Failure is non-fatal: the app starts
            // without session persistence rather than crashing.
            let store: Option<Arc<dyn KvStore>> = open_host_store();

            // Quick launch: its state, its file watcher and the chord that
            // summons it (docs/QuickDock-… §5, §10, §15.0). The store is shared
            // with the tabs: the window position and the last group live there
            // (§6).
            nohrs_pages::quick_launch::init(
                app,
                store.clone(),
                QUICK_LAUNCH_HOTKEY,
                nohrs_pages::quick_launch::Mode::Main,
            );

            // The trash ledger, on the platforms whose OS trash records nothing
            // (macOS). Without it the explorer's Delete is a one-way door: the
            // item is in `~/.Trash` but nothing knows where it came from, so
            // neither `noh trash restore` nor a future in-app restore can put it
            // back. Failing to open it is not fatal to the app and does not
            // become "no ledger needed" either — `Ledger` carries the difference
            // so Delete can refuse rather than silently lose the way back.
            let trash_ledger = trash::Ledger::open();

            let opened = app.open_window(window_options, {
                let config = config.clone();
                let config_path = config_path.clone();
                let config_overrides = config_overrides.clone();
                let config_error = config_error.clone();
                let store = store.clone();
                let trash_ledger = trash_ledger.clone();
                move |window, cx| {
                    // Initialize SearchService. Failure is non-fatal: the app starts
                    // with full-text search disabled rather than crashing.
                    let search_service: Option<Arc<SearchService>> = match SearchService::new() {
                        Ok(service) => Some(Arc::new(service)),
                        Err(e) => {
                            tracing::error!(
                                "Failed to initialize search service; starting with search disabled: {}",
                                e
                            );
                            None
                        }
                    };

                    // Kick off initial indexing on GPUI's background executor, which
                    // is a thread pool (replacing tokio::task::spawn_blocking;
                    // async-runtime.md §2).
                    if let Some(service) = &search_service
                        && let Some(job) = service.take_initial_indexing_job()
                    {
                        cx.background_spawn(async move { job.run() }).detach();
                    }

                    let view = cx.new(|cx| {
                        RootView::new(
                            resizable.clone(),
                            search_service,
                            store,
                            trash_ledger,
                            config,
                            config_path,
                            config_overrides,
                            config_error,
                            window,
                            cx,
                        )
                    });
                    if let Some(path) = &initial_path {
                        view.update(cx, |root, cx| root.open_initial_path(path, window, cx));
                    }
                    // At sign-in: on the taskbar, out of the way, ready for the
                    // first folder another program asks for.
                    if background {
                        window.minimize_window();
                    }
                    cx.new(|cx| Root::new(view, window, cx))
                }
            });
            if let Err(error) = opened {
                tracing::error!("failed to open main window: {error}");
            }
        });
    }
}

/// Binds the launcher's in-app summon key and handles it at application level.
///
/// This is the half of docs/launcher.md §2 that GPUI can serve: it only fires
/// while a nohrs window has focus. Summoning the launcher over *other*
/// applications is the OS-level registration in `nohrs_launcher::hotkey`.
///
/// The handler is global rather than attached to a window so the same key works
/// from the explorer, from a launcher window (where it toggles the launcher
/// closed), and from any window added later — without either pillar having to
/// know the other exists.
///
/// `cmd-k` is the platform key on macOS and `ctrl-k` its equivalent elsewhere;
/// both are bound so the binding is right on either platform.
fn install_in_app_launcher_key(launcher: LauncherIndex, app: &mut App) {
    app.bind_keys([
        KeyBinding::new("cmd-k", ToggleLauncher, None),
        KeyBinding::new("ctrl-k", ToggleLauncher, None),
    ]);
    app.on_action(move |_: &ToggleLauncher, cx: &mut App| {
        if let Err(error) = launcher.toggle(cx) {
            tracing::error!("failed to toggle launcher window: {error}");
        }
    });
}

/// Answers the `ToggleQuickLaunch` action, which is how the tray menu and (from
/// M3) the settings page ask for the window.
fn install_quick_launch_action(app: &mut App) {
    app.on_action(|_: &nohrs_pages::ToggleQuickLaunch, cx: &mut App| {
        nohrs_pages::quick_launch::toggle(cx);
    });
}

fn install_standalone_search_key(app: &mut App) {
    // The launcher already owns Ctrl+Shift+Space as its OS-level shortcut.
    app.bind_keys([KeyBinding::new("ctrl-shift-f", OpenStandaloneSearch, None)]);
    app.on_action(|_: &OpenStandaloneSearch, cx: &mut App| {
        if let Err(error) = nohrs_pages::search_window::toggle(cx) {
            tracing::error!("failed to toggle standalone search window: {error}");
        }
    });
}

/// The quick launch's own chord, as the window's state asks for it (§10).
///
/// The registration itself lives here because `nohrs-pages` cannot depend on
/// `nohrs-launcher` (§3.4): the window decides *which* chord, this side owns the
/// mechanism. It is registered and re-registered from the read of
/// `quicklaunch.toml`, which the window does off the foreground thread (§5.1).
const QUICK_LAUNCH_HOTKEY: nohrs_pages::quick_launch::HotkeyHooks =
    nohrs_pages::quick_launch::HotkeyHooks {
        register: register_quick_launch_hotkey,
        unregister: unregister_quick_launch_hotkey,
    };

fn register_quick_launch_hotkey(cx: &mut App, text: &str) -> Result<u64, String> {
    let chord = nohrs_launcher::parse_hotkey(text).map_err(|error| error.to_string())?;
    // The Launcher's own chord is refused with a reason instead of letting the
    // OS report it as taken (§10).
    if chord.id() == nohrs_launcher::hotkey::default_chord().hotkey.id() {
        return Err(format!(
            "{text} is the Launcher's own shortcut ({}), which it already owns",
            nohrs_launcher::describe_hotkey(&chord)
        ));
    }
    let id = nohrs_launcher::register(cx, chord, |cx| nohrs_pages::quick_launch::toggle(cx))
        .map_err(|error| format!("{error:#}"))?;
    Ok(u64::from(id.get()))
}

fn unregister_quick_launch_hotkey(cx: &mut App, id: u64) -> Result<(), String> {
    let raw = u32::try_from(id).map_err(|_| format!("{id} is not a hotkey id"))?;
    nohrs_launcher::unregister(cx, nohrs_launcher::HotkeyId::from_raw(raw))
        .map_err(|error| format!("{error:#}"))
}

/// Opens the host KV store at `<data_dir>/state.redb`, creating the data
/// directory if needed. Returns `None` (and logs) on any failure so a broken or
/// unwritable store degrades to "no session persistence" rather than a crash.
fn open_host_store() -> Option<Arc<dyn KvStore>> {
    let data_dir = config::paths::data_dir();
    if let Err(error) = std::fs::create_dir_all(&data_dir) {
        tracing::error!("could not create data dir {}: {error}", data_dir.display());
        return None;
    }
    let path = data_dir.join("state.redb");
    match RedbKvStore::open(&path, &StoreLogConfig::default()) {
        Ok(store) => Some(Arc::new(store)),
        Err(error) => {
            tracing::error!("could not open KV store {}: {error}", path.display());
            None
        }
    }
}

/// Resolves `theme.mode` to light or dark and applies it to both palettes the
/// UI draws from: nohrs's own tokens and gpui-component's widgets.
///
/// The Explorer window does this again, and on every config reload, in
/// `RootView::apply_config`. This copy is for the launcher-only mode, which
/// never builds that root and would otherwise always open light.
fn apply_theme_mode(mode: ThemeMode, app: &mut App) {
    let dark = match mode {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::System => matches!(
            app.window_appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
    };
    nohrs_ui::theme::theme::set_dark(dark);
    let widget_mode = if dark {
        gpui_component::ThemeMode::Dark
    } else {
        gpui_component::ThemeMode::Light
    };
    gpui_component::Theme::change(widget_mode, None, app);
}
