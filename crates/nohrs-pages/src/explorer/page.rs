//! The split-view container that owns the explorer's panes and their tabs.
//!
//! A single [`ExplorerPage`] holds one or two panes (2-way split is the P2 cap;
//! `docs/explorer-essentials.md` §3.1), each with its own list of tabs (§4). Each
//! tab navigates independently and renders its own header / listing / preview.
//! The container arranges the panes (left/right or top/bottom), routes the split
//! / focus / tab shortcuts (§3.2, §4, §6), and mirrors navigation across panes
//! when `synced_panes` is enabled.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Once};
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::resizable::ResizableState;
use nohrs_core::config::{Compact, DockSide, Explorer as ExplorerConfig, Layout, SplitDirection, Ui};
use nohrs_core::telemetry::LogErr;
use nohrs_services::fs::trash::Ledger as TrashLedger;
use nohrs_services::search::SearchService;
use nohrs_store::{KvKey, KvStore, kv_key};
use nohrs_ui::theme::theme;
use serde::{Deserialize, Serialize};

use super::state::ExplorerPane;
use super::types::{PaneEvent, StatusLevel};
use crate::dock::{self, Gesture, Resized};
use crate::pane_group::{GroupDrag, PaneGroup, PaneGroupCallbacks};
use crate::terminal::{TERMINAL_CONTEXT, TerminalEvent, TerminalView};

// Key context the pane shortcuts are bound under, so they only fire while the
// explorer (and not another page) is focused.
const PANES_CONTEXT: &str = "ExplorerPanes";

// Host-KV key under which the tab session is persisted (`docs/persistence.md`
// §3: tab/session restore lives in redb `state.redb`).
const SESSION_KEY: KvKey = kv_key!("session.explorer_tabs");

// How long to coalesce rapid mutations (navigation, tab open/close) before
// writing the session snapshot, so high-frequency changes don't hammer redb's
// fsync-on-commit (`docs/persistence.md` §3).
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

const TERMINAL_DEFAULT_HEIGHT: f32 = 260.0;
const TERMINAL_MIN_HEIGHT: f32 = super::view::panels::TERMINAL_MIN_HEIGHT;
/// As a column beside the panes.
const TERMINAL_DEFAULT_WIDTH: f32 = 460.0;
const TERMINAL_MIN_WIDTH: f32 = super::view::panels::TERMINAL_MIN_WIDTH;
const TERMINAL_HEADER_HEIGHT: f32 = 28.0;
/// Room the panes always keep beside or above the terminal.
const PANES_MIN: f32 = 240.0;

/// A persisted snapshot of one pane: its tabs (each a directory) and which is
/// active.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaneSnapshot {
    tabs: Vec<String>,
    active_tab: usize,
}

/// A persisted snapshot of the whole split for session restore (§4).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionSnapshot {
    panes: Vec<PaneSnapshot>,
    active_pane: usize,
    direction: SplitDirection,
}

actions!(
    explorer_panes,
    [
        /// Split the explorer into left/right panes (or flip an existing split).
        SplitVertical,
        /// Split the explorer into top/bottom panes (or flip an existing split).
        SplitHorizontal,
        /// Move focus to the first pane.
        FocusPane1,
        /// Move focus to the second pane.
        FocusPane2,
        /// Move focus to the next pane, wrapping around.
        FocusNextPane,
        /// Move focus to the previous pane, wrapping around.
        FocusPrevPane,
        /// Open a new tab in the active pane, rooted at home.
        NewTab,
        /// Close the active tab (closing the pane if it was its last tab).
        CloseTab,
        /// Toggle the active tab's left quick-access sidebar.
        ToggleSidebar,
        /// Show or hide the terminal of the focused tab.
        ToggleTerminal,
    ]
);

static BIND_PANE_KEYS: Once = Once::new();

fn bind_pane_keys(cx: &mut App) {
    BIND_PANE_KEYS.call_once(|| {
        cx.bind_keys(vec![
            // Keys the shell needs more than the explorer does, while typing in
            // the terminal: Ctrl+B and Ctrl+[ ] \ are readline and escape keys.
            // A deeper context wins over the explorer's bindings whatever their
            // order, so these hold even after the shortcuts are rebound.
            KeyBinding::new("ctrl-b", NoAction, Some(TERMINAL_CONTEXT)),
            KeyBinding::new("ctrl-[", NoAction, Some(TERMINAL_CONTEXT)),
            KeyBinding::new("ctrl-]", NoAction, Some(TERMINAL_CONTEXT)),
            KeyBinding::new("ctrl-\\", NoAction, Some(TERMINAL_CONTEXT)),
        ]);
    });
    rebind_shortcuts(cx);
}

// The keys `rebind_shortcuts` last bound to the page's actions.
static BOUND_SHORTCUTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Binds the page's actions (tabs, panes, sidebar, terminal) to the keys the
/// shortcut table now gives them. gpui cannot take a binding back, so a key
/// that no longer belongs to any of them is bound to `NoAction`, which a
/// later binding overrides: the newest binding of a key wins.
pub fn rebind_shortcuts(cx: &mut App) {
    use nohrs_core::shortcuts::{Action, chords};
    let page_actions = [
        Action::NewTab,
        Action::CloseTab,
        Action::SplitVertical,
        Action::SplitHorizontal,
        Action::FocusPane1,
        Action::FocusPane2,
        Action::NextPane,
        Action::PreviousPane,
        Action::ToggleSidebar,
        Action::ToggleTerminal,
    ];
    let wanted: Vec<(String, Action)> = page_actions
        .into_iter()
        .flat_map(|action| chords(action).into_iter().map(move |chord| (chord.to_config(), action)))
        // A key gpui cannot read would panic in `KeyBinding::new`.
        .filter(|(keys, _)| Keystroke::parse(keys).is_ok())
        .collect();
    let mut bound = BOUND_SHORTCUTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let context = Some(PANES_CONTEXT);
    let mut bindings: Vec<KeyBinding> = bound
        .iter()
        .filter(|keys| !wanted.iter().any(|(wanted_keys, _)| wanted_keys == *keys))
        .filter(|keys| Keystroke::parse(keys).is_ok())
        .map(|keys| KeyBinding::new(keys, NoAction, context))
        .collect();
    for (keys, action) in &wanted {
        bindings.push(match action {
            Action::NewTab => KeyBinding::new(keys, NewTab, context),
            Action::CloseTab => KeyBinding::new(keys, CloseTab, context),
            Action::SplitVertical => KeyBinding::new(keys, SplitVertical, context),
            Action::SplitHorizontal => KeyBinding::new(keys, SplitHorizontal, context),
            Action::FocusPane1 => KeyBinding::new(keys, FocusPane1, context),
            Action::FocusPane2 => KeyBinding::new(keys, FocusPane2, context),
            Action::NextPane => KeyBinding::new(keys, FocusNextPane, context),
            Action::PreviousPane => KeyBinding::new(keys, FocusPrevPane, context),
            Action::ToggleSidebar => KeyBinding::new(keys, ToggleSidebar, context),
            _ => KeyBinding::new(keys, ToggleTerminal, context),
        });
    }
    cx.bind_keys(bindings);
    *bound = wanted.into_iter().map(|(keys, _)| keys).collect();
}

/// Home directory used as the root of a freshly opened tab (§4).
///
/// Windows has no `HOME`, so reading it fell through to the process's current
/// directory: a new tab opened wherever the app was launched from (the repo
/// folder, under `cargo run`). `dirs` asks the Shell for the profile folder.
/// The current directory is no longer a fallback, for the same reason.
pub(crate) fn home_dir() -> String {
    dirs::home_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "C:\\".into())
}

/// Clamp `index` into `0..len` (or `0` when `len` is zero), for restoring a
/// possibly-stale active index from a snapshot.
fn clamp_index(index: usize, len: usize) -> usize {
    if len == 0 { 0 } else { index.min(len - 1) }
}

/// The explorer page: a 2-way split container over panes, each holding its own
/// tabs. With a single pane and a single tab it renders exactly like an unsplit
/// explorer.
///
/// The split/tab/resize machinery lives in the content-agnostic [`PaneGroup`];
/// `ExplorerPage` adds the explorer-specific layer on top: replaying `[ui]`
/// config onto new tabs and mirroring navigation across panes when
/// `synced_panes` is enabled (§3.2). It keeps a per-tab subscription map keyed by
/// the tab's `EntityId` by funneling every tab create/remove through the helpers
/// here, so the map stays consistent across the 2-D pane/tab structure.
pub struct ExplorerPage {
    group: PaneGroup<ExplorerPane>,
    // Navigation-event subscriptions, one per live tab, keyed by tab entity id.
    tab_subscriptions: HashMap<EntityId, Subscription>,
    /// Whether navigation in one pane mirrors into the others (§3.2).
    synced_panes: bool,
    // Last-applied `[ui]` settings, replayed onto tabs opened later so they match
    // config rather than reverting to pane defaults.
    ui: Ui,
    // Host KV store for session persistence (§4). `None` disables save/restore
    // (e.g. when the store failed to open, or in tests).
    store: Option<Arc<dyn KvStore>>,
    // The pending debounced session save, dropped (cancelling it) whenever a new
    // mutation reschedules.
    save_task: Option<Task<()>>,
    focus_handle: FocusHandle,
    // Each tab's terminals, started the first time the panel shows that tab.
    terminals: HashMap<EntityId, TerminalSet>,
    // The pending move of an unused terminal to its tab's folder, replaced
    // (and so cancelled) by each new navigation.
    terminal_follow: Option<Task<()>>,
    terminal_open: bool,
    // Where the terminal docks, and its size as a band (height) and as a
    // column (width), each kept while it is on the other kind of side.
    terminal_side: DockSide,
    terminal_height: Pixels,
    terminal_width: Pixels,
    // A drag of the terminal's inner edge, of its header toward another
    // side, or of the edge it waits at while closed.
    terminal_drag: Option<TerminalDrag>,
    // The area the panes and the terminal share, measured at paint.
    page_bounds: Rc<std::cell::Cell<Bounds<Pixels>>>,
    // A restarted terminal takes the focus the old one had, on the next
    // render, which is where the window is at hand.
    focus_terminal_on_next_render: Option<Entity<TerminalView>>,
}

impl Focusable for ExplorerPage {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ExplorerPage {
    /// Builds the explorer with a single pane holding a single tab. The pane
    /// resizable is supplied by the application (it owns the app-level state);
    /// each tab creates its own listing/preview resizable and search input.
    pub fn new(
        _pane_resizable: Entity<ResizableState>,
        search_service: Option<Arc<SearchService>>,
        store: Option<Arc<dyn KvStore>>,
        trash_ledger: TrashLedger,
        restore_tabs: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        bind_pane_keys(cx);
        // Before any tab reads a folder, so the first one already opens as it
        // was left.
        super::folder_views::FolderViews::install(store.clone(), cx);
        super::waveforms::install_store(store.clone());
        super::tags::Tags::install(store.clone(), cx);
        super::network::NetworkPlaces::install(store.clone(), cx);
        super::favorites::Favorites::install(store.clone(), cx);
        // The credential window for a share opens over this one. Not in
        // tests: their windows have no platform handle to ask.
        #[cfg(not(test))]
        if let Some(hwnd) = crate::ipc::window_hwnd(window) {
            super::network::set_owner(hwnd);
        }
        let build_search_service = search_service.clone();
        let (group, first_tab) = PaneGroup::new(
            Box::new(move |window, cx| {
                let search_service = build_search_service.clone();
                let trash_ledger = trash_ledger.clone();
                cx.new(|cx| ExplorerPane::build(search_service, trash_ledger, window, cx))
            }),
            window,
            cx,
        );
        let mut page = Self {
            group,
            tab_subscriptions: HashMap::new(),
            synced_panes: false,
            ui: Ui::default(),
            store,
            save_task: None,
            focus_handle: cx.focus_handle(),
            terminals: HashMap::new(),
            terminal_follow: None,
            terminal_open: false,
            terminal_side: DockSide::Bottom,
            terminal_height: px(TERMINAL_DEFAULT_HEIGHT),
            terminal_width: px(TERMINAL_DEFAULT_WIDTH),
            terminal_drag: None,
            page_bounds: Rc::default(),
            focus_terminal_on_next_render: None,
        };
        page.subscribe_tab(&first_tab, cx);
        // The root pane's tab shows its sidebar; tabs opened later default to
        // hidden (issue #164, §2).
        first_tab.update(cx, |pane, _cx| pane.sidebar_visible = true);

        // Restore the previous session's tabs (§4), unless disabled by config.
        // A one-time synchronous read at startup is acceptable; ongoing writes go
        // through a background task (see `schedule_save`).
        if restore_tabs && let Some(snapshot) = page.load_session() {
            page.restore_session(snapshot, &first_tab, window, cx);
        }
        page
    }

    // Reads and deserializes the persisted session snapshot, if any.
    fn load_session(&self) -> Option<SessionSnapshot> {
        let bytes = self.store.as_ref()?.get(&SESSION_KEY).log_err()??;
        serde_json::from_slice(&bytes).log_err()
    }

    // Rebuilds panes and tabs from a restored snapshot. `first_tab` is the single
    // tab the group was constructed with; it is reused as pane 0's first tab.
    fn restore_session(
        &mut self,
        snapshot: SessionSnapshot,
        first_tab: &Entity<ExplorerPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(first_pane) = snapshot.panes.first() else {
            return;
        };
        self.group.set_direction(snapshot.direction);

        // Pane 0 reuses the existing first tab for its first directory, then adds
        // the rest. The root pane keeps its sidebar visible (issue #164).
        if let Some(cwd) = first_pane.tabs.first() {
            self.configure_tab(first_tab, Some(cwd.clone()), true, cx);
        }
        for cwd in first_pane.tabs.iter().skip(1) {
            if let Some(tab) = self.group.add_tab(0, window, cx) {
                self.subscribe_tab(&tab, cx);
                self.configure_tab(&tab, Some(cwd.clone()), true, cx);
            }
        }
        let active = clamp_index(first_pane.active_tab, self.group.tab_count(0));
        self.group.set_active_tab(0, active, window, cx);

        // Pane 1, if the snapshot had a split. `add_explorer_pane` collapses its
        // sidebar (split-created), matching a freshly split second pane.
        if let Some(second_pane) = snapshot.panes.get(1)
            && let Some(index) =
                self.add_explorer_pane(second_pane.tabs.first().cloned(), window, cx)
        {
            for cwd in second_pane.tabs.iter().skip(1) {
                if let Some(tab) = self.group.add_tab(index, window, cx) {
                    self.subscribe_tab(&tab, cx);
                    self.configure_tab(&tab, Some(cwd.clone()), false, cx);
                }
            }
            let active = clamp_index(second_pane.active_tab, self.group.tab_count(index));
            self.group.set_active_tab(index, active, window, cx);
        }

        let active_pane = clamp_index(snapshot.active_pane, self.group.pane_count());
        self.group.set_active(active_pane, window, cx);
    }

    // Subscribes to a tab's navigation events, keyed by its entity id so the map
    // stays consistent regardless of pane/tab position.
    fn subscribe_tab(&mut self, tab: &Entity<ExplorerPane>, cx: &mut Context<Self>) {
        let subscription = cx.subscribe(tab, Self::on_pane_event);
        self.tab_subscriptions.insert(tab.entity_id(), subscription);
    }

    // Replays the active `[ui]` config onto a freshly built tab and, when given,
    // roots it at `cwd` (forcing a reload). Shared by split, new-tab, and restore.
    fn configure_tab(
        &self,
        tab: &Entity<ExplorerPane>,
        cwd: Option<String>,
        sidebar_visible: bool,
        cx: &mut Context<Self>,
    ) {
        let ui = self.ui.clone();
        tab.update(cx, |pane, cx| {
            pane.apply_config_ui(&ui, cx);
            pane.sidebar_visible = sidebar_visible;
            if let Some(cwd) = cwd {
                pane.cwd = cwd;
                // Force a reload of the new root on the next render.
                pane.loaded = false;
            }
        });
    }

    // Creates a pane (with one tab) through the group, subscribes to the tab, and
    // replays the active `[ui]` config. Returns the new pane's index.
    fn add_explorer_pane(
        &mut self,
        cwd: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let (index, tab) = self.group.add_pane(window, cx)?;
        self.subscribe_tab(&tab, cx);
        // Split-created panes start with the sidebar collapsed (issue #164),
        // and the preview too: a second pane is for a second folder, and its
        // own preview would halve what is left of it (decided 05/11/2026).
        self.configure_tab(&tab, cwd, false, cx);
        tab.update(cx, |pane, _| pane.preview_visible = false);
        Some(index)
    }

    /// Opens a new tab in `pane_index`, rooted at home (§4), and activates it.
    fn new_tab_in_pane(&mut self, pane_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // Inherit the sidebar visibility of the pane's currently active tab so the
        // new tab looks continuous with what the user was just seeing.
        let sidebar_visible = self
            .group
            .tab(pane_index, self.group.active_tab(pane_index))
            .map(|tab| tab.read(cx).sidebar_visible)
            .unwrap_or(false);
        let Some(tab) = self.group.add_tab(pane_index, window, cx) else {
            return;
        };
        self.subscribe_tab(&tab, cx);
        self.configure_tab(&tab, Some(home_dir()), sidebar_visible, cx);
        // Make the pane active and focus its new (now active) tab.
        self.group.set_active(pane_index, window, cx);
        self.group.focus_active(window, cx);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Closes the tab at `(pane_index, tab_index)`. When it is the pane's last
    /// tab, the whole pane closes (§3.1, §4).
    fn close_tab_in_pane(
        &mut self,
        pane_index: usize,
        tab_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let outcome = self.group.close_tab(pane_index, tab_index, window, cx);
        if outcome.removed.is_empty() {
            return;
        }
        // Dropping each removed tab's subscription deregisters its event handler.
        for id in &outcome.removed {
            self.tab_subscriptions.remove(id);
            self.terminals.remove(id);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// Activates `(pane_index, tab_index)`, making its pane active too.
    fn activate_tab(
        &mut self,
        pane_index: usize,
        tab_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.group.set_active_tab(pane_index, tab_index, window, cx) {
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// Moves a tab within its pane (drag reorder, §4).
    fn reorder_tab(&mut self, pane_index: usize, from: usize, to: usize, cx: &mut Context<Self>) {
        if self.group.reorder_tab(pane_index, from, to) {
            self.schedule_save(cx);
            cx.notify();
        }
    }

    // Mirrors a tab's navigation into the other panes while syncing is enabled.
    fn on_pane_event(
        &mut self,
        source: Entity<ExplorerPane>,
        event: &PaneEvent,
        cx: &mut Context<Self>,
    ) {
        let path = match event {
            PaneEvent::Navigated(path) => path,
            PaneEvent::SplitRequested { side, fraction } => {
                self.split_from_edge(*side, *fraction, cx);
                return;
            }
            PaneEvent::TerminalRequested { side, size } => {
                self.open_terminal_at(*side, *size, cx);
                return;
            }
        };
        // A tab changed directory: persist the new session (a tab's cwd is part
        // of the snapshot), regardless of whether syncing is on.
        self.schedule_save(cx);
        if !self.synced_panes {
            return;
        }
        // Only mirror navigation from a visible (active) tab; a background tab
        // (e.g. one restored on startup) must not move the other panes.
        if !self.group.is_active_tab(source.entity_id()) {
            return;
        }
        for tab in self.group.other_active_tabs(source.entity_id()) {
            let path = path.clone();
            // `navigate_to_synced` (not `change_dir`) is deliberate: it does not
            // re-emit `PaneEvent::Navigated`, which would mirror back to the
            // source and loop indefinitely. Don't replace it with a regular
            // navigation method.
            tab.update(cx, |pane, cx| pane.navigate_to_synced(path, cx));
        }
    }

    /// Ensures a split exists and uses `direction`. With one pane it opens a
    /// second rooted at the active tab's directory; with two it just re-orients
    /// (so the other split shortcut flips horizontal/vertical). Always keeps the
    /// 2-way cap (§3.1).
    pub fn split(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.group.set_direction(direction);
        if self.group.pane_count() < 2 {
            let cwd = self.group.active_pane().read(cx).cwd.clone();
            if let Some(index) = self.add_explorer_pane(Some(cwd), window, cx) {
                self.group.set_active(index, window, cx);
            }
        } else {
            self.group.focus_active(window, cx);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// The split key, as one key that turns the split on and off: with one
    /// pane it opens a second rooted at the pane in use; with two it closes
    /// the one nobody is working in, so focus stays where it was.
    pub(crate) fn toggle_split(&mut self, direction: SplitDirection, window: &mut Window, cx: &mut Context<Self>) {
        if self.group.pane_count() >= 2 {
            let other = if self.group.active() == 0 { 1 } else { 0 };
            self.close_pane(other, window, cx);
            self.group.set_direction(direction);
            self.schedule_save(cx);
            cx.notify();
        } else {
            self.split(direction, window, cx);
        }
    }

    /// Opens a second pane out of the `side` edge a pane was dragged by,
    /// taking `fraction` of the room on that side; it shows the folder the
    /// active tab shows, as the split shortcuts do.
    fn split_from_edge(&mut self, side: DockSide, fraction: f32, cx: &mut Context<Self>) {
        if self.group.pane_count() >= 2 {
            return;
        }
        let Some(window) = cx.windows().into_iter().next() else {
            return;
        };
        let direction = if side.is_column() { SplitDirection::Vertical } else { SplitDirection::Horizontal };
        let first = matches!(side, DockSide::Left | DockSide::Top);
        let this = cx.entity().downgrade();
        // The event arrives inside the pane's update; the new pane is built
        // with the window, which is reached from a task of its own.
        cx.spawn(async move |_, cx| {
            window
                .update(cx, |_, window, cx| {
                    this.update(cx, |page, cx| {
                        page.group.set_direction(direction);
                        let cwd = page.group.active_pane().read(cx).cwd.clone();
                        let index = if first { 0 } else { 1 };
                        let Some((index, tab)) = page.group.add_pane_at(index, window, cx) else {
                            return;
                        };
                        page.subscribe_tab(&tab, cx);
                        page.configure_tab(&tab, Some(cwd), false, cx);
                        // Without its own preview, as `add_explorer_pane` opens one.
                        tab.update(cx, |pane, _| pane.preview_visible = false);
                        page.group.set_ratio(if first { fraction } else { 1.0 - fraction });
                        page.group.set_active(index, window, cx);
                        page.schedule_save(cx);
                        cx.notify();
                    })
                    .log_err();
                })
                .log_err();
        })
        .detach();
    }

    /// Opens the terminal on `side`, `size` deep, from its bubble on an edge
    /// of the listing; docking it there is remembered in the config.
    fn open_terminal_at(&mut self, side: DockSide, size: f32, cx: &mut Context<Self>) {
        if self.terminal_side != side {
            self.terminal_side = side;
            super::view::panels::save_layout("terminal", format!("\"{}\"", side.as_str()), cx);
        }
        self.set_terminal_size(size);
        if self.terminal_open {
            cx.notify();
            return;
        }
        let Some(window) = cx.windows().into_iter().next() else {
            return;
        };
        let this = cx.entity().downgrade();
        // As for a split: the event arrives inside the pane's update, and
        // opening focuses the shell, which needs the window.
        cx.spawn(async move |_, cx| {
            window
                .update(cx, |_, window, cx| {
                    this.update(cx, |page, cx| {
                        if !page.terminal_open {
                            page.toggle_terminal(window, cx);
                        }
                    })
                    .log_err();
                })
                .log_err();
        })
        .detach();
    }

    /// Closes a whole pane (its × button), keeping at least one open (§3.1).
    /// No-op for the last pane.
    pub fn close_pane(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let removed = self.group.remove_pane(index, window, cx);
        if removed.is_empty() {
            return;
        }
        for id in &removed {
            self.tab_subscriptions.remove(id);
            self.terminals.remove(id);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// Makes `index` the active pane and moves keyboard focus to it.
    pub fn set_active(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.group.set_active(index, window, cx) {
            self.schedule_save(cx);
            cx.notify();
        }
    }

    fn focus_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.group.focus_next(window, cx) {
            self.schedule_save(cx);
            cx.notify();
        }
    }

    fn focus_prev(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.group.focus_prev(window, cx) {
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// Applies the `[ui]` config section to every tab (§5 of `config.md`).
    pub fn apply_config_ui(&mut self, ui: &Ui, cx: &mut Context<Self>) {
        self.ui = ui.clone();
        for tab in self.group.all_tabs() {
            tab.update(cx, |pane, cx| pane.apply_config_ui(ui, cx));
        }
    }

    /// Applies the `[explorer]` config section: the default split orientation and
    /// the synced-panes opt-in. Enabling sync immediately mirrors the active
    /// tab's directory into the other panes.
    pub fn apply_config_explorer(&mut self, explorer: &ExplorerConfig, cx: &mut Context<Self>) {
        // Only adopt the configured orientation while unsplit, so a config reload
        // does not silently flip a split the user arranged via the shortcuts.
        if self.group.pane_count() < 2 {
            self.group.set_direction(explorer.split_direction);
        }
        let enabling = explorer.synced_panes && !self.synced_panes;
        self.synced_panes = explorer.synced_panes;
        if enabling {
            let active_id = self.group.active_pane().entity_id();
            let path = self.group.active_pane().read(cx).cwd.clone();
            for tab in self.group.other_active_tabs(active_id) {
                let path = path.clone();
                tab.update(cx, |pane, cx| pane.navigate_to_synced(path, cx));
            }
            // Mirroring changed the other tabs' directories, but
            // `navigate_to_synced` deliberately emits no `Navigated` event, so
            // nothing else schedules a save. Persist the new session here.
            self.schedule_save(cx);
        }
        cx.notify();
    }

    /// Applies the `[layout]` config section: where the sidebar, the preview
    /// and the terminal dock, and the tabs' layout.
    pub fn apply_config_layout(&mut self, layout: &Layout, cx: &mut Context<Self>) {
        super::view::panels::set_sides(layout.sidebar, layout.preview);
        self.terminal_side = layout.terminal;
        self.group.set_vertical_tabs(layout.vertical_tabs);
        self.group.set_vertical_tabs_side(layout.vertical_tabs_side);
        super::view::panels::set_move_grips(layout.move_grips);
        cx.notify();
    }

    /// Applies the `[compact]` config section to the tab rows.
    pub fn apply_config_compact(&mut self, compact: &Compact, cx: &mut Context<Self>) {
        self.group.set_compact(compact.clone());
        cx.notify();
    }

    /// Footer status for the active tab (a config error in `RootView` still
    /// takes precedence over this).
    pub fn status_for_footer(&self, cx: &App) -> Option<(String, StatusLevel)> {
        self.group.active_pane().read(cx).status_for_footer()
    }

    /// Selection and listing counts for the active tab, as `(selected, total)`,
    /// for the footer's "N selected" / "N items" indicators.
    pub fn selection_counts(&self, cx: &App) -> (usize, usize) {
        let pane = self.group.active_pane().read(cx);
        (pane.selection.len(), pane.filtered_entries.len())
    }

    /// Folders and files in the active tab, for the footer: `(folders, files
    /// shown, files in the folder)`. Shown is after the filter or search.
    pub fn listing_counts(&self, cx: &App) -> (usize, usize, usize) {
        let pane = self.group.active_pane().read(cx);
        let is_file = |entry: &&nohrs_services::fs::listing::FileEntryDto| entry.kind != "dir";
        let folders = pane.filtered_entries.iter().filter(|entry| entry.kind == "dir").count();
        let shown = pane.filtered_entries.iter().filter(is_file).count();
        // Search results come from other folders too: compared with this
        // folder's files they read as "1121 of 272".
        let total = if pane.search_results.is_some() {
            shown
        } else {
            pane.entries.iter().filter(is_file).count()
        };
        (folders, shown, total)
    }

    /// Listing zoom of the active tab, for the footer's zoom control.
    pub fn zoom(&self, cx: &App) -> f32 {
        self.group.active_pane().read(cx).zoom
    }

    /// Sets the listing zoom of the active tab.
    pub fn set_zoom(&self, zoom: f32, cx: &mut App) {
        self.group
            .active_pane()
            .update(cx, |pane, cx| pane.set_zoom(zoom, cx));
    }

    /// Shows `folder` and selects `select` in it, the way another program
    /// asks for a path: a new tab in the active pane, unless the active tab is
    /// already showing that folder.
    pub fn open_path(
        &mut self,
        folder: String,
        select: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane_index = self.group.active();
        let current = self.group.active_pane().clone();
        let already_there = super::same_path(&current.read(cx).cwd, &folder);
        let tab = if already_there {
            current
        } else {
            let sidebar_visible = current.read(cx).sidebar_visible;
            let Some(tab) = self.group.add_tab(pane_index, window, cx) else {
                return;
            };
            self.subscribe_tab(&tab, cx);
            self.configure_tab(&tab, Some(folder), sidebar_visible, cx);
            self.group.set_active(pane_index, window, cx);
            tab
        };
        tab.update(cx, |pane, cx| {
            // Loaded now rather than on the next frame, so the file asked for is
            // there to select.
            pane.ensure_loaded(cx);
            if let Some(select) = &select {
                pane.reveal(select);
            }
            cx.notify();
        });
        self.group.focus_active(window, cx);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Points the tab with entity id `tab_id` at `folder`, selecting `select`
    /// in it. Returns whether such a tab exists.
    pub fn navigate_tab(
        &mut self,
        tab_id: u64,
        folder: String,
        select: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self
            .group
            .all_tabs()
            .into_iter()
            .find(|tab| tab.entity_id().as_u64() == tab_id)
        else {
            return false;
        };
        if let Some((pane_index, tab_index)) = self.group.locate_tab(tab.entity_id()) {
            self.group.set_active_tab(pane_index, tab_index, window, cx);
            self.group.set_active(pane_index, window, cx);
        }
        tab.update(cx, |pane, cx| {
            pane.change_dir(folder, window, cx);
            pane.ensure_loaded(cx);
            if let Some(select) = &select {
                pane.reveal(select);
            }
        });
        self.group.focus_active(window, cx);
        cx.notify();
        true
    }

    /// Every open tab as `(entity id, folder, whether it is the active one)`.
    pub fn tabs(&self, cx: &App) -> Vec<(u64, String, bool)> {
        let active = self.group.active_pane().entity_id();
        self.group
            .all_tabs()
            .into_iter()
            .map(|tab| {
                let id = tab.entity_id();
                (id.as_u64(), tab.read(cx).cwd.clone(), id == active)
            })
            .collect()
    }

    /// Absolute path of the active tab's current directory, for the footer.
    pub fn current_path(&self, cx: &App) -> String {
        self.group.active_pane().read(cx).cwd.clone()
    }

    // Captures the current panes/tabs into a persistable snapshot (§4).
    fn session_snapshot(&self, cx: &App) -> SessionSnapshot {
        let panes = (0..self.group.pane_count())
            .map(|pane_index| {
                let tabs = (0..self.group.tab_count(pane_index))
                    .filter_map(|tab_index| {
                        self.group
                            .tab(pane_index, tab_index)
                            .map(|tab| tab.read(cx).cwd.clone())
                    })
                    .collect();
                PaneSnapshot {
                    tabs,
                    active_tab: self.group.active_tab(pane_index),
                }
            })
            .collect();
        SessionSnapshot {
            panes,
            active_pane: self.group.active(),
            direction: self.group.direction(),
        }
    }

    // Debounced session save: every mutation reschedules this, so a burst of
    // changes results in a single redb write after `SAVE_DEBOUNCE` of quiet. The
    // blocking `put` runs on the background executor, off the UI thread.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if self.store.is_none() {
            return;
        }
        self.save_task = Some(cx.spawn(move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let cx = cx.clone();
            async move {
                cx.background_executor().timer(SAVE_DEBOUNCE).await;
                let prepared = this
                    .read_with(&cx, |this, cx| {
                        let store = this.store.clone()?;
                        let bytes = serde_json::to_vec(&this.session_snapshot(cx)).log_err()?;
                        Some((store, bytes))
                    })
                    .ok()
                    .flatten();
                let Some((store, bytes)) = prepared else {
                    return;
                };
                cx.background_spawn(async move {
                    store.put(&SESSION_KEY, &bytes).log_err();
                })
                .await;
            }
        }));
    }

    // Builds the render callbacks routing every tab/pane interaction back here.
    fn pane_callbacks() -> PaneGroupCallbacks<Self> {
        PaneGroupCallbacks {
            on_activate_pane: Rc::new(|this: &mut Self, pane, window, cx| {
                this.set_active(pane, window, cx)
            }),
            on_close_pane: Rc::new(|this: &mut Self, pane, window, cx| {
                this.close_pane(pane, window, cx)
            }),
            on_new_tab: Rc::new(|this: &mut Self, pane, window, cx| {
                this.new_tab_in_pane(pane, window, cx)
            }),
            on_activate_tab: Rc::new(|this: &mut Self, pane, tab, window, cx| {
                this.activate_tab(pane, tab, window, cx)
            }),
            on_close_tab: Rc::new(|this: &mut Self, pane, tab, window, cx| {
                this.close_tab_in_pane(pane, tab, window, cx)
            }),
            on_reorder_tab: Rc::new(|this: &mut Self, pane, from, to, _window, cx| {
                this.reorder_tab(pane, from, to, cx)
            }),
            on_group_drag: Rc::new(|this: &mut Self, drag, _window, cx| {
                this.group.begin_drag(drag);
                if drag == GroupDrag::EvenOut {
                    this.schedule_save(cx);
                }
                cx.notify();
            }),
        }
    }
}

/// One explorer tab's terminals: the panel's own tabs, one shown.
struct TerminalSet {
    terminals: Vec<(Entity<TerminalView>, Subscription)>,
    active: usize,
}

impl TerminalSet {
    fn active(&self) -> Option<&Entity<TerminalView>> {
        self.terminals.get(self.active).or(self.terminals.last()).map(|(terminal, _)| terminal)
    }

    fn index_of(&self, id: EntityId) -> Option<usize> {
        self.terminals.iter().position(|(terminal, _)| terminal.entity_id() == id)
    }
}

/// How long the folder must stay put before an unused terminal follows it:
/// arrowing through folders would otherwise start and kill a shell at each.
const TERMINAL_FOLLOW_DELAY: Duration = Duration::from_millis(350);

impl ExplorerPage {
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_open = !self.terminal_open;
        if self.terminal_open {
            let mut terminal = self.focused_terminal(cx);
            // Opening shows the tab's folder at once; the follow delay is for
            // moving through folders with the panel open.
            let tab = self.group.active_pane().clone();
            let wanted = terminal_directory(&tab.read(cx).cwd);
            let view = terminal.read(cx);
            if !view.is_used() && !view.has_exited() && view.directory() != wanted.as_path() {
                self.replace_terminal(tab.entity_id(), terminal.entity_id(), false, cx);
                terminal = self.focused_terminal(cx);
            }
            window.focus(&terminal.focus_handle(cx));
        } else {
            self.group.focus_active(window, cx);
        }
        cx.notify();
    }

    // The focused tab's shown terminal, started in that tab's folder if it
    // has none.
    fn focused_terminal(&mut self, cx: &mut Context<Self>) -> Entity<TerminalView> {
        let tab = self.group.active_pane().clone();
        if let Some(terminal) = self.terminals.get(&tab.entity_id()).and_then(TerminalSet::active) {
            return terminal.clone();
        }
        self.add_terminal(&tab, cx)
    }

    // A new terminal in the tab's folder, shown.
    fn add_terminal(&mut self, tab: &Entity<ExplorerPane>, cx: &mut Context<Self>) -> Entity<TerminalView> {
        let (terminal, subscription) = self.spawn_terminal(tab, cx);
        let set = self
            .terminals
            .entry(tab.entity_id())
            .or_insert_with(|| TerminalSet { terminals: Vec::new(), active: 0 });
        set.terminals.push((terminal.clone(), subscription));
        set.active = set.terminals.len() - 1;
        terminal
    }

    fn spawn_terminal(
        &mut self,
        tab: &Entity<ExplorerPane>,
        cx: &mut Context<Self>,
    ) -> (Entity<TerminalView>, Subscription) {
        let directory = terminal_directory(&tab.read(cx).cwd);
        let terminal = cx.new(|cx| TerminalView::new(directory, cx));
        let tab_id = tab.entity_id();
        let subscription = cx.subscribe(&terminal, move |page: &mut Self, terminal, event, cx| match event {
            TerminalEvent::RestartRequested => page.replace_terminal(tab_id, terminal.entity_id(), true, cx),
        });
        (terminal, subscription)
    }

    // A new shell in the folder the tab is in now, in place of `terminal_id`.
    fn replace_terminal(&mut self, tab_id: EntityId, terminal_id: EntityId, take_focus: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.group.all_tabs().into_iter().find(|tab| tab.entity_id() == tab_id) else {
            return;
        };
        let Some(index) = self.terminals.get(&tab_id).and_then(|set| set.index_of(terminal_id)) else {
            return;
        };
        let replacement = self.spawn_terminal(&tab, cx);
        let terminal = replacement.0.clone();
        if let Some(slot) = self.terminals.get_mut(&tab_id).and_then(|set| set.terminals.get_mut(index)) {
            // Dropping the old view ends its shell.
            *slot = replacement;
        }
        if take_focus {
            self.focus_terminal_on_next_render = Some(terminal);
        }
        cx.notify();
    }

    fn close_terminal(&mut self, tab_id: EntityId, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(set) = self.terminals.get_mut(&tab_id) else {
            return;
        };
        if index < set.terminals.len() {
            // Dropping the view and its subscription ends the shell.
            drop(set.terminals.remove(index));
        }
        if set.terminals.is_empty() {
            // The last one closed: the panel goes with it, as in VS Code.
            self.terminals.remove(&tab_id);
            self.terminal_open = false;
            self.group.focus_active(window, cx);
        } else {
            set.active = set.active.min(set.terminals.len() - 1);
            if let Some(terminal) = set.active() {
                window.focus(&terminal.focus_handle(cx));
            }
        }
        cx.notify();
    }

    // The shown terminal of the focused tab, if nothing was typed into it and
    // the tab is now in another folder, moves there: a fresh shell, after the
    // folder has stayed put a moment. Asked on every render of the panel, so
    // navigating, switching tabs and opening the panel all land here.
    fn follow_tab_folder(&mut self, terminal: &Entity<TerminalView>, window: &Window, cx: &mut Context<Self>) {
        let tab = self.group.active_pane().clone();
        let wanted = terminal_directory(&tab.read(cx).cwd);
        let view = terminal.read(cx);
        if view.is_used() || view.has_exited() || view.directory() == wanted.as_path() {
            self.terminal_follow = None;
            return;
        }
        if self.terminal_follow.is_some() {
            return;
        }
        let take_focus = terminal.focus_handle(cx).is_focused(window);
        let (tab_id, terminal_id) = (tab.entity_id(), terminal.entity_id());
        self.terminal_follow = Some(cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            cx.background_executor().timer(TERMINAL_FOLLOW_DELAY).await;
            this.update(cx, |page, cx| {
                page.terminal_follow = None;
                // Still the shown one, still unused: a key pressed during the
                // wait keeps it.
                let shown = page.terminals.get(&tab_id).and_then(TerminalSet::active).cloned();
                if shown.is_some_and(|shown| shown.entity_id() == terminal_id && !shown.read(cx).is_used()) {
                    page.replace_terminal(tab_id, terminal_id, take_focus, cx);
                }
            })
            .log_err();
        }));
    }

    // `AnyElement`, not `impl IntoElement`: that would keep `self` and `cx`
    // borrowed for as long as the element lives.
    fn render_terminal_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let terminal = self.focused_terminal(cx);
        if self.focus_terminal_on_next_render.take().is_some() {
            window.focus(&terminal.focus_handle(cx));
        }
        self.follow_tab_folder(&terminal, window, cx);
        let tab_id = self.group.active_pane().entity_id();
        // The folder, not the title the shell sets: PowerShell sets it to
        // its own executable's path, which says nothing about where it is.
        let label = terminal.read(cx).directory().display().to_string();
        let (names, active) = match self.terminals.get(&tab_id) {
            Some(set) => (
                set.terminals
                    .iter()
                    .map(|(terminal, _)| {
                        let directory = terminal.read(cx).directory().to_path_buf();
                        directory
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| directory.display().to_string())
                    })
                    .collect::<Vec<_>>(),
                set.active,
            ),
            None => (Vec::new(), 0),
        };

        let tabs = names.into_iter().enumerate().map(|(index, name)| {
            let shown = index == active;
            div()
                .id(("terminal-tab", index))
                .flex_shrink_0()
                .h(px(22.0))
                .pl(px(8.0))
                .pr(px(4.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .rounded(px(4.0))
                .text_xs()
                .cursor_pointer()
                .when(shown, |tab| tab.bg(rgb(theme::bg_hover())).text_color(rgb(theme::fg())))
                .when(!shown, |tab| {
                    tab.text_color(rgb(theme::fg_secondary())).hover(|tab| tab.bg(rgb(theme::bg_hover())))
                })
                .on_click(cx.listener(move |page, _, window, cx| {
                    if let Some(set) = page.terminals.get_mut(&tab_id) {
                        set.active = index;
                        if let Some(terminal) = set.active() {
                            window.focus(&terminal.focus_handle(cx));
                        }
                    }
                    cx.notify();
                }))
                .child(div().max_w(px(140.0)).whitespace_nowrap().text_ellipsis().overflow_hidden().child(name))
                .child(
                    div()
                        .id(("terminal-tab-close", index))
                        .p(px(2.0))
                        .rounded(px(3.0))
                        .hover(|button| button.bg(rgb(theme::border())))
                        .on_click(cx.listener(move |page, _, window, cx| {
                            cx.stop_propagation();
                            page.close_terminal(tab_id, index, window, cx);
                        }))
                        .child(
                            gpui_component::Icon::new(gpui_component::IconName::Close)
                                .size_3()
                                .text_color(rgb(theme::fg_secondary())),
                        ),
                )
        });

        let side = self.terminal_side;
        let size = self.fitted_terminal_size();
        let carrying = self.terminal_drag.is_some_and(|drag| drag.gesture == Gesture::Move);
        let resizing = self
            .terminal_drag
            .is_some_and(|drag| matches!(drag.gesture, Gesture::Resize { .. }));
        dock::frame(side, size, 1.0)
            .child(
                div()
                    .h(px(TERMINAL_HEADER_HEIGHT))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .px(px(8.0))
                    .bg(rgb(theme::toolbar_bg()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .child(
                                // The title is the terminal's handle: dragged,
                                // it carries the panel to another side.
                                div()
                                    .id("terminal-handle")
                                    .flex_shrink_0()
                                    .mr(px(4.0))
                                    .px(px(4.0))
                                    .rounded(px(4.0))
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::fg()))
                                    .cursor(CursorStyle::OpenHand)
                                    .hover(|handle| handle.bg(rgb(theme::bg_hover())))
                                    .when(carrying, |handle| handle.bg(rgb(theme::accent_subtle())))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|page, event: &MouseDownEvent, _window, cx| {
                                            cx.stop_propagation();
                                            page.terminal_drag = Some(TerminalDrag {
                                                gesture: Gesture::Move,
                                                pointer: event.position,
                                                collapsing: false,
                                            });
                                            cx.notify();
                                        }),
                                    )
                                    .child("Terminal"),
                            )
                            .children(tabs)
                            .child(
                                // Another shell, in the tab's folder.
                                div()
                                    .id("terminal-new")
                                    .flex_shrink_0()
                                    .p(px(4.0))
                                    .rounded(px(4.0))
                                    .cursor_pointer()
                                    .hover(|button| button.bg(rgb(theme::bg_hover())))
                                    .on_click(cx.listener(|page, _, window, cx| {
                                        let tab = page.group.active_pane().clone();
                                        let terminal = page.add_terminal(&tab, cx);
                                        window.focus(&terminal.focus_handle(cx));
                                        cx.notify();
                                    }))
                                    .child(
                                        gpui_component::Icon::new(gpui_component::IconName::Plus)
                                            .size_3p5()
                                            .text_color(rgb(theme::fg_secondary())),
                                    ),
                            )
                            .child(
                                div()
                                    .ml(px(4.0))
                                    .min_w(px(0.0))
                                    .text_xs()
                                    .text_color(rgb(theme::fg_secondary()))
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .overflow_hidden()
                                    .child(label),
                            ),
                    )
                    .child(
                        div()
                            .id("terminal-close")
                            .flex_shrink_0()
                            .p(px(4.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .hover(|button| button.bg(rgb(theme::bg_hover())))
                            .on_click(cx.listener(|page, _, window, cx| page.toggle_terminal(window, cx)))
                            .child(
                                gpui_component::Icon::new(gpui_component::IconName::Close)
                                    .size_4()
                                    .text_color(rgb(theme::fg_secondary())),
                            ),
                    ),
            )
            .child(div().flex_1().min_h(px(0.0)).child(terminal))
            .child(
                // The inner edge: drag to change the size (past half the
                // smallest it closes), double click for the default. Last
                // so it lies over the header.
                dock::place_on_inner_edge(dock::strip("terminal-resize", side, resizing), side).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|page, event: &MouseDownEvent, _window, cx| {
                        if event.click_count >= 2 {
                            page.set_terminal_size(page.terminal_default_size());
                            page.terminal_drag = None;
                        } else {
                            page.terminal_drag = Some(TerminalDrag {
                                gesture: Gesture::Resize {
                                    start: dock::along(page.terminal_side, event.position),
                                    start_size: page.fitted_terminal_size(),
                                },
                                pointer: event.position,
                                collapsing: false,
                            });
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }),
                ),
            )
            .into_any_element()
    }
}

/// A drag of the terminal's inner edge, of its title toward another side, or
/// of the edge it waits at while closed (`dock::Gesture`).
#[derive(Clone, Copy, Debug)]
struct TerminalDrag {
    gesture: Gesture,
    pointer: Point<Pixels>,
    /// Pulled under half its smallest size: closed when the button comes up.
    collapsing: bool,
}

impl ExplorerPage {
    fn terminal_range(&self) -> (f32, f32) {
        let bounds = self.page_bounds.get();
        if self.terminal_side.is_column() {
            (TERMINAL_MIN_WIDTH, f32::from(bounds.size.width) - PANES_MIN)
        } else {
            (TERMINAL_MIN_HEIGHT, f32::from(bounds.size.height) - PANES_MIN.min(TERMINAL_MIN_HEIGHT * 2.0))
        }
    }

    fn terminal_size(&self) -> f32 {
        f32::from(if self.terminal_side.is_column() { self.terminal_width } else { self.terminal_height })
    }

    fn set_terminal_size(&mut self, size: f32) {
        if self.terminal_side.is_column() {
            self.terminal_width = px(size);
        } else {
            self.terminal_height = px(size);
        }
    }

    fn terminal_default_size(&self) -> f32 {
        if self.terminal_side.is_column() { TERMINAL_DEFAULT_WIDTH } else { TERMINAL_DEFAULT_HEIGHT }
    }

    // The size the terminal is drawn at: the one it was left at, within the
    // room the window has now, so a smaller window does not push the panes out.
    fn fitted_terminal_size(&self) -> f32 {
        let (min, max) = self.terminal_range();
        let size = self.terminal_size();
        if self.page_bounds.get().size.width <= px(0.0) { size } else { size.clamp(min, max.max(min)) }
    }

    // Follows a terminal drag. Returns whether one is under way.
    fn update_terminal_drag(&mut self, pointer: Point<Pixels>) -> bool {
        let Some(mut drag) = self.terminal_drag else {
            return false;
        };
        drag.pointer = pointer;
        let side = self.terminal_side;
        match drag.gesture {
            Gesture::Resize { start, start_size } => {
                let (min, max) = self.terminal_range();
                let raw = start_size + (dock::along(side, pointer) - start) * dock::growth(side);
                match dock::resized(raw, min, max) {
                    Resized::Size(size) => {
                        drag.collapsing = false;
                        self.set_terminal_size(size);
                    }
                    Resized::Collapse => drag.collapsing = true,
                }
            }
            // The terminal is opened from the listing's edges (their bubbles),
            // never pulled out of an edge of its own.
            Gesture::Open | Gesture::Move => {}
        }
        self.terminal_drag = Some(drag);
        true
    }

    // The button came up: ends the divider's drag (closing a pane squeezed
    // shut) and the terminal's (closing it, or docking it where it was let go).
    fn finish_page_drags(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.group.finish_drag() {
            self.close_pane(index, window, cx);
        }
        self.schedule_save(cx);
        if let Some(drag) = self.terminal_drag.take() {
            match drag.gesture {
                Gesture::Resize { start_size, .. } if drag.collapsing => {
                    self.set_terminal_size(start_size);
                    if self.terminal_open {
                        self.toggle_terminal(window, cx);
                    }
                }
                Gesture::Move => {
                    let target = dock::nearest_side(self.page_bounds.get(), drag.pointer);
                    if target != self.terminal_side {
                        self.terminal_side = target;
                        super::view::panels::save_layout("terminal", format!("\"{}\"", target.as_str()), cx);
                    }
                }
                _ => {}
            }
        }
        cx.notify();
    }

    // While the terminal is carried, where it would land.
    fn render_terminal_landing(&self) -> Option<AnyElement> {
        let drag = self.terminal_drag.filter(|drag| drag.gesture == Gesture::Move)?;
        let bounds = self.page_bounds.get();
        let target = dock::nearest_side(bounds, drag.pointer);
        let (room, size, min) = if target.is_column() {
            (f32::from(bounds.size.width), f32::from(self.terminal_width), TERMINAL_MIN_WIDTH)
        } else {
            (f32::from(bounds.size.height), f32::from(self.terminal_height), TERMINAL_MIN_HEIGHT)
        };
        Some(dock::landing(target, size.min(room * 0.45).max(min.min(room))).into_any_element())
    }
}

// The tab's folder when it is one on disk; places like "This PC" or the
// network have no folder a shell can start in.
fn terminal_directory(cwd: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(cwd);
    if path.is_dir() { path } else { std::path::PathBuf::from(home_dir()) }
}

impl Render for ExplorerPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let callbacks = Self::pane_callbacks();
        self.group.set_title_bar_allowed(!self.terminal_beside());
        // The listing's edges offer the terminal while it is closed.
        let terminal_open = self.terminal_open;
        for tab in self.group.active_tabs() {
            tab.update(cx, |pane, _| pane.terminal_shown = terminal_open);
        }
        let body = self.group.render(window, cx, &callbacks);
        let collapsing = self.terminal_drag.is_some_and(|drag| drag.collapsing);
        let terminal_panel = (self.terminal_open && !collapsing).then(|| self.render_terminal_panel(window, cx));
        let side = self.terminal_side;
        let panes = div().flex_1().min_h(px(0.0)).min_w(px(0.0)).flex().flex_col().child(body);
        // The panes and the terminal on its side of them, first in the row
        // or column when it docks left or above.
        let (before, after) = if matches!(side, DockSide::Left | DockSide::Top) {
            (terminal_panel, None)
        } else {
            (None, terminal_panel)
        };
        let arranged = div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .min_w(px(0.0))
            .flex()
            .when(side.is_column(), |this| this.flex_row())
            .when(!side.is_column(), |this| this.flex_col())
            .children(before)
            .child(panes)
            .children(after)
            .child(dock::measure(self.page_bounds.clone()))
            .children(self.render_terminal_landing());

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::bg()))
            .key_context(PANES_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &SplitVertical, window, cx| {
                this.toggle_split(SplitDirection::Vertical, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitHorizontal, window, cx| {
                this.split(SplitDirection::Horizontal, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPane1, window, cx| {
                this.set_active(0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPane2, window, cx| {
                this.set_active(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusNextPane, window, cx| {
                this.focus_next(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPrevPane, window, cx| {
                this.focus_prev(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| {
                let pane = this.group.active();
                this.new_tab_in_pane(pane, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                let pane = this.group.active();
                let tab = this.group.active_tab(pane);
                this.close_tab_in_pane(pane, tab, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                this.toggle_terminal(window, cx);
            }))
            .when(self.terminal_drag.is_some() || self.group.dragging(), |this| {
                this.on_mouse_move(cx.listener(|page, event: &MouseMoveEvent, window, cx| {
                    if event.pressed_button != Some(MouseButton::Left) {
                        // The button came up outside the window.
                        page.finish_page_drags(window, cx);
                        return;
                    }
                    if page.group.drag_to(event.position) {
                        cx.notify();
                    }
                    if page.update_terminal_drag(event.position) {
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|page, _: &MouseUpEvent, window, cx| page.finish_page_drags(window, cx)),
                )
                // Let go over the title bar, which is the root's, not the page's.
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|page, _: &MouseUpEvent, window, cx| page.finish_page_drags(window, cx)),
                )
            })
            .on_action(cx.listener(|this, _: &ToggleSidebar, _window, cx| {
                let pane = this.group.active();
                let tab_index = this.group.active_tab(pane);
                if let Some(tab) = this.group.tab(pane, tab_index) {
                    tab.update(cx, |tab, cx| tab.toggle_sidebar(cx));
                }
            }))
            .child(arranged)
    }
}

impl crate::Page for ExplorerPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        <Self as Render>::render(self, window, cx).into_any_element()
    }
}

impl ExplorerPage {
    /// Whether the tab row draws the window's buttons, so the root leaves
    /// out its own title bar.
    pub fn tabs_in_title_bar(&self) -> bool {
        // Asked by the root before this page renders: worked out from the
        // terminal here, not from what the group was told last frame.
        self.group.pane_count() == 1 && !self.group.vertical_tabs() && !self.terminal_beside()
    }

    // The terminal open on a side of the panes, reaching the window's top.
    fn terminal_beside(&self) -> bool {
        self.terminal_open && self.terminal_side.is_column()
    }
}

#[cfg(test)]
impl ExplorerPage {
    pub(crate) fn pane_count(&self) -> usize {
        self.group.pane_count()
    }

    pub(crate) fn active_index(&self) -> usize {
        self.group.active()
    }

    pub(crate) fn direction(&self) -> SplitDirection {
        self.group.direction()
    }

    pub(crate) fn is_synced(&self) -> bool {
        self.synced_panes
    }

    /// Number of tabs in `pane_index`.
    pub(crate) fn tab_count(&self, pane_index: usize) -> usize {
        self.group.tab_count(pane_index)
    }

    /// Active tab index of `pane_index`.
    pub(crate) fn active_tab(&self, pane_index: usize) -> usize {
        self.group.active_tab(pane_index)
    }

    /// The active tab of `pane_index` (the visible `ExplorerPane`).
    pub(crate) fn pane(&self, index: usize) -> Entity<ExplorerPane> {
        self.group
            .tab(index, self.group.active_tab(index))
            .expect("test requested an out-of-range pane")
    }

    /// The `cwd` of the tab at `(pane_index, tab_index)`.
    pub(crate) fn tab_cwd(&self, pane_index: usize, tab_index: usize, cx: &App) -> Option<String> {
        self.group
            .tab(pane_index, tab_index)
            .map(|tab| tab.read(cx).cwd.clone())
    }

    /// The active tab's `cwd` for `pane_index`.
    pub(crate) fn pane_cwd(&self, index: usize, cx: &App) -> Option<String> {
        self.tab_cwd(index, self.group.active_tab(index), cx)
    }

    /// Test entry points for the tab actions exercised without keybindings.
    pub(crate) fn test_new_tab(
        &mut self,
        pane_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_tab_in_pane(pane_index, window, cx);
    }

    pub(crate) fn test_close_tab(
        &mut self,
        pane_index: usize,
        tab_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_tab_in_pane(pane_index, tab_index, window, cx);
    }

    pub(crate) fn test_activate_tab(
        &mut self,
        pane_index: usize,
        tab_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_tab(pane_index, tab_index, window, cx);
    }

    pub(crate) fn test_reorder_tab(
        &mut self,
        pane_index: usize,
        from: usize,
        to: usize,
        cx: &mut Context<Self>,
    ) {
        self.reorder_tab(pane_index, from, to, cx);
    }

    /// Number of live per-tab subscriptions; used to assert the subscription map
    /// stays consistent across splits/closes/new tabs.
    pub(crate) fn subscription_count(&self) -> usize {
        self.tab_subscriptions.len()
    }
}
