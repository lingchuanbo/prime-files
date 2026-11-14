//! A reusable 2-way split / per-pane tab container, decoupled from the explorer.
//!
//! [`PaneGroup<T>`] owns the split/tab/active/direction/resize/render machinery
//! that used to live inside `ExplorerPage`, parameterized over any content type
//! `T` that implements [`PaneItem`]. The explorer adopts it today; a future page
//! (or plugin page) implements [`PaneItem`] and gets the same behaviour for free.
//!
//! Each pane owns an independent list of tabs (`docs/explorer-essentials.md` §4),
//! of which one is active and rendered; the group holds at most two panes
//! (2-way split is the P2 cap, §3.1). The group is plain data owned by an
//! embedding view entity, not an entity itself: its mutators take
//! `&mut App`/`&mut Window` and the embedder calls `cx.notify()` after mutating
//! it. Content-specific concerns the group must stay ignorant of (navigation
//! sync, config replay, per-tab subscriptions) live in the embedder, which drives
//! create/remove through the group's primitives.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{Icon, IconName};
use nohrs_core::config::{Compact, DockSide, SplitDirection};
use nohrs_ui::theme::theme;

use crate::compact::{self, Edge, Reveal};

/// Width the settings button and the three window buttons take at the end
/// of the tab row when it is the title bar: 32 + 4 + 3 x 46.
const TITLE_CONTROLS_WIDTH: f32 = 174.0;

/// Content that a [`PaneGroup`] can host as a tab. Implemented by the views shown
/// inside a tab (today only `ExplorerPane`).
pub trait PaneItem: Render + Focusable + 'static {
    /// Short label shown on the tab (e.g. the current directory name).
    fn tab_title(&self, cx: &App) -> String;

    /// Optional icon shown before the tab title. Defaults to none.
    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        None
    }

    /// Hands the item the `[compact]` settings and its pane's top-edge slide,
    /// which the item's own top bar shares with the tab row. Called on every
    /// render of the active tab, before it renders; defaults to ignoring it.
    fn attach_compact(&mut self, _settings: &Compact, _top: &Rc<Reveal>) {}

    /// Tells the item whether its group has room for another pane, so a
    /// gesture of its own can ask for one. Called on every render of the
    /// active tab; defaults to ignoring it.
    fn set_split_available(&mut self, _available: bool) {}
}

/// Width of the column of vertical tabs as a fresh window opens.
const TAB_COLUMN_DEFAULT: f32 = 184.0;
/// Under this the column shows only the tabs' icons, at [`TAB_COLUMN_ICONS`].
const TAB_COLUMN_LABELS_MIN: f32 = 120.0;
const TAB_COLUMN_ICONS: f32 = 48.0;
const TAB_COLUMN_MAX: f32 = 420.0;
/// A pane dragged smaller than this by the divider closes when let go.
const PANE_COLLAPSE: f32 = 120.0;
const DIVIDER_GRIP: f32 = 7.0;

thread_local! {
    // One width for every pane's column, as Zen keeps one sidebar.
    static TAB_COLUMN_WIDTH: Cell<f32> = const { Cell::new(TAB_COLUMN_DEFAULT) };
}

/// A press on something of the group's own that the mouse then drags: the
/// divider between the panes, or the edge of the column of vertical tabs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GroupDrag {
    /// The divider.
    Divider {
        /// The pane squeezed past [`PANE_COLLAPSE`], which closes when the
        /// button comes up.
        collapsing: Option<usize>,
    },
    /// A double click on the divider: the panes share the room evenly.
    EvenOut,
    /// The tab column's edge.
    TabColumn {
        /// Where the pointer was when the drag started.
        start_x: f32,
        /// The column's width then.
        start_width: f32,
        /// Whether the column hangs on the right, where dragging left widens
        /// it rather than narrowing it.
        from_right: bool,
    },
}

/// Builds a fresh tab entity. Separate from [`PaneItem`] because construction
/// needs embedder-owned services (e.g. the explorer's search service) that the
/// trait should not know about.
type BuildPane<T> = Box<dyn Fn(&mut Window, &mut App) -> Entity<T>>;

/// Maximum panes a group holds. The split is 2-way today (P2 cap, §3.1); 3+ way
/// splits are P3+. (Tabs within a pane are unbounded — see [`Pane`].)
const MAX_PANES: usize = 2;

/// Per-tab appearance and protection, persisted by the embedding page.
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TabMetadata {
    #[serde(default)]
    /// Prevent ordinary close and show in the pinned section.
    pub pinned: bool,
    #[serde(default)]
    /// Show as a compact persistent tab.
    pub resident: bool,
    #[serde(default)]
    /// Optional RGB icon color.
    pub color: Option<u32>,
}

/// One pane: an ordered, non-empty list of tabs with one active.
struct Pane<T: PaneItem> {
    // Invariant: always non-empty.
    tabs: Vec<Entity<T>>,
    /// Index of the tab rendered and acted on within this pane.
    active_tab: usize,
    /// The top edge's slide in compact mode: the tab row and the active
    /// tab's own top bar.
    reveal: Rc<Reveal>,
}

impl<T: PaneItem> Pane<T> {
    fn with_tab(tab: Entity<T>) -> Self {
        Self {
            tabs: vec![tab],
            active_tab: 0,
            reveal: Rc::default(),
        }
    }

    /// The active tab, falling back to the first to avoid panicking should the
    /// `active_tab` invariant ever be violated.
    fn active(&self) -> &Entity<T> {
        self.tabs.get(self.active_tab).unwrap_or(&self.tabs[0])
    }
}

/// The outcome of [`PaneGroup::close_tab`]: which tab entities were removed (so
/// the embedder can drop their per-tab state) and whether the whole pane closed
/// because the last tab was the one closed.
#[derive(Debug, Default)]
pub struct TabCloseOutcome {
    /// Entity ids of every tab removed by the close.
    pub removed: Vec<EntityId>,
    /// Whether closing the tab also closed its pane (it was the pane's last tab).
    pub pane_closed: bool,
}

/// A pane-indexed render hook: `(view, pane, window, cx)`.
type PaneHook<V> = Rc<dyn Fn(&mut V, usize, &mut Window, &mut Context<V>)>;
/// A tab-indexed render hook: `(view, pane, tab, window, cx)`.
type TabHook<V> = Rc<dyn Fn(&mut V, usize, usize, &mut Window, &mut Context<V>)>;
/// A tab-reorder render hook: `(view, pane, from, to, window, cx)`.
type ReorderHook<V> = Rc<dyn Fn(&mut V, usize, usize, usize, &mut Window, &mut Context<V>)>;
/// A press that starts one of the group's drags: `(view, drag, window, cx)`.
type ContextHook<V> = Rc<dyn Fn(&mut V, usize, usize, Point<Pixels>, &mut Window, &mut Context<V>)>;
type DragHook<V> = Rc<dyn Fn(&mut V, GroupDrag, &mut Window, &mut Context<V>)>;

/// The view-state hooks a [`PaneGroup`] needs to render its interactive chrome.
/// Bundled into one struct because the tab bar wires up six distinct actions;
/// each closure runs against the embedding view `V`. Closures are `Rc`-wrapped so
/// the struct can be cheaply cloned across the two panes and their tabs.
pub struct PaneGroupCallbacks<V: 'static> {
    /// A pane was clicked: `(pane)`.
    pub on_activate_pane: PaneHook<V>,
    /// A pane's close button was clicked: `(pane)`.
    pub on_close_pane: PaneHook<V>,
    /// The new-tab (`+`) button was clicked: `(pane)`.
    pub on_new_tab: PaneHook<V>,
    /// A tab was clicked: `(pane, tab)`.
    pub on_activate_tab: TabHook<V>,
    /// A tab's close button was clicked: `(pane, tab)`.
    pub on_close_tab: TabHook<V>,
    /// A tab was dragged onto another within the same pane: `(pane, from, to)`.
    pub on_reorder_tab: ReorderHook<V>,
    /// A tab was right-clicked at a window position.
    pub on_tab_context: ContextHook<V>,
    /// The divider or the tab column's edge was pressed: the embedder hands
    /// the drag to [`PaneGroup::begin_drag`] and the mouse after it to
    /// [`PaneGroup::drag_to`] and [`PaneGroup::finish_drag`].
    pub on_group_drag: DragHook<V>,
}

// Manual `Clone` (derive would wrongly demand `V: Clone`; `Rc` clones regardless).
impl<V: 'static> Clone for PaneGroupCallbacks<V> {
    fn clone(&self) -> Self {
        Self {
            on_activate_pane: self.on_activate_pane.clone(),
            on_close_pane: self.on_close_pane.clone(),
            on_new_tab: self.on_new_tab.clone(),
            on_activate_tab: self.on_activate_tab.clone(),
            on_close_tab: self.on_close_tab.clone(),
            on_reorder_tab: self.on_reorder_tab.clone(),
            on_tab_context: self.on_tab_context.clone(),
            on_group_drag: self.on_group_drag.clone(),
        }
    }
}

/// The drag payload for a tab reorder: identifies the tab being dragged by its
/// `(pane, tab)` position. Matched by type at the drop site.
#[derive(Clone)]
struct TabDrag {
    pane: usize,
    tab: usize,
}

/// The view rendered under the cursor while a tab is dragged.
struct TabDragPreview {
    title: SharedString,
}

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .bg(rgb(theme::toolbar_active_bg()))
            .text_color(rgb(theme::toolbar_active_text()))
            .text_sm()
            .child(self.title.clone())
    }
}

/// A 2-way split container over panes, each with its own independently-navigating
/// tabs of type `T`.
pub struct PaneGroup<T: PaneItem> {
    // Invariant: always non-empty and at most two entries (2-way cap).
    panes: Vec<Pane<T>>,
    tab_metadata: HashMap<EntityId, TabMetadata>,
    /// Index of the pane keyboard input and shortcuts act on.
    active: usize,
    /// Orientation a split uses; toggled by the split shortcuts.
    direction: SplitDirection,
    /// The first pane's share of the room, across or down.
    ratio: f32,
    /// The area the panes share, measured at paint, for the divider's drag.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    drag: Option<GroupDrag>,
    build_pane: BuildPane<T>,
    /// Compact mode: tab rows hidden until the pointer reaches them.
    compact: Compact,
    /// Tabs in a column down each pane's side, as in Zen.
    vertical_tabs: bool,
    /// The side that column runs down: `Left` (the default) or `Right`.
    vertical_tabs_side: DockSide,
    /// Whether the tab row may be the window's title bar. Not while a panel
    /// beside the panes (the terminal on a side) reaches the window's top:
    /// the window's buttons would sit in the middle of the window.
    title_bar_allowed: bool,
    /// Product name shown when the tab row itself is the window title bar.
    app_name: String,
}

impl<T: PaneItem> PaneGroup<T> {
    /// Builds a group with a single pane holding a single tab. Returns that tab
    /// so the embedder can wire it up (subscribe, replay config, set initial
    /// state).
    pub fn new(build_pane: BuildPane<T>, window: &mut Window, cx: &mut App) -> (Self, Entity<T>) {
        let first = build_pane(window, cx);
        let group = Self {
            panes: vec![Pane::with_tab(first.clone())],
            tab_metadata: HashMap::new(),
            active: 0,
            direction: SplitDirection::default(),
            ratio: 0.5,
            bounds: Rc::default(),
            drag: None,
            build_pane,
            compact: Compact::default(),
            vertical_tabs: false,
            vertical_tabs_side: DockSide::Left,
            title_bar_allowed: true,
            app_name: nohrs_ui::window::APP_NAME.to_string(),
        };
        (group, first)
    }

    /// Adopts the `[compact]` settings. Turning compact mode on hides the
    /// rows at once rather than sliding them away under the pointer that
    /// just clicked the setting.
    pub fn set_compact(&mut self, compact: Compact) {
        if compact.enabled && !self.compact.enabled {
            for pane in &self.panes {
                pane.reveal.hide_now();
            }
        }
        self.compact = compact;
    }

    /// Builds a pane (with one tab) and appends it, enforcing the 2-way cap.
    /// Returns the new pane's index and its first tab (or `None` when already at
    /// [`MAX_PANES`]) so the embedder can subscribe to the tab and apply per-pane
    /// configuration. Does not change the active pane or notify; that is the
    /// embedder's responsibility.
    pub fn add_pane(&mut self, window: &mut Window, cx: &mut App) -> Option<(usize, Entity<T>)> {
        self.add_pane_at(self.panes.len(), window, cx)
    }

    /// [`Self::add_pane`] at `index` (0 puts it before the open one, for a
    /// pane dragged out of the left or top edge). The active pane keeps
    /// pointing at the same pane.
    pub fn add_pane_at(&mut self, index: usize, window: &mut Window, cx: &mut App) -> Option<(usize, Entity<T>)> {
        if self.panes.len() >= MAX_PANES {
            return None;
        }
        let index = index.min(self.panes.len());
        let tab = (self.build_pane)(window, cx);
        self.panes.insert(index, Pane::with_tab(tab.clone()));
        if index <= self.active && self.panes.len() > 1 {
            self.active += 1;
        }
        Some((index, tab))
    }

    /// Sets the first pane's share of the room.
    pub fn set_ratio(&mut self, ratio: f32) {
        self.ratio = ratio.clamp(0.1, 0.9);
    }

    /// Lays the tabs out in a column on each pane's side (`true`) or in a
    /// row along its top.
    pub fn set_vertical_tabs(&mut self, vertical: bool) {
        self.vertical_tabs = vertical;
    }

    /// The side the tab column runs down. Anything but `Right` is the left.
    pub fn set_vertical_tabs_side(&mut self, side: DockSide) {
        self.vertical_tabs_side = if side == DockSide::Right { DockSide::Right } else { DockSide::Left };
    }

    /// Whether the tabs are laid out in a column.
    pub fn vertical_tabs(&self) -> bool {
        self.vertical_tabs
    }

    /// Lets the tab row be the title bar (`true`), or keeps it a row of tabs
    /// under the window's own title bar.
    pub fn set_title_bar_allowed(&mut self, allowed: bool) {
        self.title_bar_allowed = allowed;
    }

    /// Sets the product name shown before the tabs when they occupy the title bar.
    pub fn set_app_name(&mut self, app_name: &str) {
        self.app_name = if app_name.trim().is_empty() {
            nohrs_ui::window::APP_NAME.to_string()
        } else {
            app_name.trim().to_string()
        };
    }

    /// Starts one of the group's drags.
    pub fn begin_drag(&mut self, drag: GroupDrag) {
        if drag == GroupDrag::EvenOut {
            self.ratio = 0.5;
            self.drag = None;
            return;
        }
        self.drag = Some(drag);
    }

    /// Whether one of the group's drags is under way.
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Follows the group's drag to `pointer`. Returns whether one is under way.
    pub fn drag_to(&mut self, pointer: Point<Pixels>) -> bool {
        match self.drag {
            Some(GroupDrag::Divider { .. }) => {
                let bounds = self.bounds.get();
                let (start, length, at) = match self.direction {
                    SplitDirection::Vertical => (bounds.left(), bounds.size.width, pointer.x),
                    SplitDirection::Horizontal => (bounds.top(), bounds.size.height, pointer.y),
                };
                let length = f32::from(length);
                if length <= 0.0 {
                    return true;
                }
                let first = f32::from(at - start);
                let collapsing = if first < PANE_COLLAPSE {
                    Some(0)
                } else if length - first < PANE_COLLAPSE {
                    Some(1)
                } else {
                    None
                };
                self.ratio = (first / length).clamp(0.02, 0.98);
                self.drag = Some(GroupDrag::Divider { collapsing });
                true
            }
            Some(GroupDrag::TabColumn { start_x, start_width, from_right }) => {
                // A column on the right grows as the pointer moves left.
                let travelled = f32::from(pointer.x) - start_x;
                let width = start_width + if from_right { -travelled } else { travelled };
                // Under the labels' minimum the column snaps to its icons.
                let width = if width < TAB_COLUMN_LABELS_MIN { TAB_COLUMN_ICONS } else { width.min(TAB_COLUMN_MAX) };
                TAB_COLUMN_WIDTH.set(width);
                true
            }
            Some(GroupDrag::EvenOut) | None => false,
        }
    }

    /// Ends the group's drag. Returns the pane the divider squeezed shut,
    /// for the embedder to close (it owns the per-tab state to drop).
    pub fn finish_drag(&mut self) -> Option<usize> {
        match self.drag.take() {
            Some(GroupDrag::Divider { collapsing: Some(index) }) => {
                self.ratio = 0.5;
                Some(index)
            }
            Some(GroupDrag::Divider { collapsing: None }) => {
                self.ratio = self.ratio.clamp(0.1, 0.9);
                None
            }
            _ => None,
        }
    }

    /// Removes the pane at `index`, keeping at least one open. Returns the entity
    /// ids of every tab removed (empty if nothing was removed) so the embedder
    /// can drop the matching per-tab state. Rebinds the active pane and moves
    /// focus to it.
    pub fn remove_pane(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<EntityId> {
        if self.panes.len() <= 1 || index >= self.panes.len() {
            return Vec::new();
        }
        let removed = self.panes.remove(index);
        if self.active >= self.panes.len() {
            self.active = self.panes.len() - 1;
        }
        self.focus_active(window, cx);
        let ids: Vec<_> = removed.tabs.iter().map(|tab| tab.entity_id()).collect();
        for id in &ids { self.tab_metadata.remove(id); }
        ids
    }

    /// Builds a tab, appends it to the pane at `pane_index`, and makes it that
    /// pane's active tab. Returns the new tab (or `None` for an out-of-range
    /// pane) so the embedder can subscribe and configure it. Does not change the
    /// active pane or notify.
    pub fn add_tab(
        &mut self,
        pane_index: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Entity<T>> {
        if pane_index >= self.panes.len() {
            return None;
        }
        let tab = (self.build_pane)(window, cx);
        let pane = &mut self.panes[pane_index];
        pane.tabs.push(tab.clone());
        pane.active_tab = pane.tabs.len() - 1;
        Some(tab)
    }

    /// Closes the tab at `(pane_index, tab_index)`. When it is the pane's last
    /// tab, the whole pane closes (subject to the min-one-pane rule, §3.1).
    /// Returns which tab entities were removed and whether the pane closed, so
    /// the embedder can drop the matching per-tab state.
    pub fn close_tab(
        &mut self,
        pane_index: usize,
        tab_index: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> TabCloseOutcome {
        let tab_len = match self.panes.get(pane_index) {
            Some(pane) if tab_index < pane.tabs.len() => pane.tabs.len(),
            _ => return TabCloseOutcome::default(),
        };

        if tab_len > 1 {
            let removed_id = {
                let pane = &mut self.panes[pane_index];
                let removed = pane.tabs.remove(tab_index);
                if pane.active_tab >= pane.tabs.len() {
                    pane.active_tab = pane.tabs.len() - 1;
                } else if tab_index < pane.active_tab {
                    pane.active_tab -= 1;
                }
                removed.entity_id()
            };
            self.tab_metadata.remove(&removed_id);
            if pane_index == self.active {
                self.focus_active(window, cx);
            }
            return TabCloseOutcome {
                removed: vec![removed_id],
                pane_closed: false,
            };
        }

        // The pane's last tab: closing it closes the pane (a no-op for the last
        // pane, which `remove_pane` guards).
        let removed = self.remove_pane(pane_index, window, cx);
        let pane_closed = !removed.is_empty();
        TabCloseOutcome {
            removed,
            pane_closed,
        }
    }

    /// Makes `tab_index` the active tab of `pane_index` and that pane the active
    /// pane, moving focus to it. Returns whether anything changed.
    pub fn set_active_tab(
        &mut self,
        pane_index: usize,
        tab_index: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        match self.panes.get(pane_index) {
            Some(pane) if tab_index < pane.tabs.len() => {}
            _ => return false,
        }
        let changed = self.active != pane_index || self.panes[pane_index].active_tab != tab_index;
        self.panes[pane_index].active_tab = tab_index;
        self.active = pane_index;
        self.focus_active(window, cx);
        changed
    }

    /// Moves the tab at `from` to `to` within `pane_index`, preserving which tab
    /// is active. Returns whether the order changed.
    pub fn reorder_tab(&mut self, pane_index: usize, from: usize, to: usize) -> bool {
        let Some(pane) = self.panes.get_mut(pane_index) else {
            return false;
        };
        if from >= pane.tabs.len() || to >= pane.tabs.len() || from == to {
            return false;
        }
        let active_id = pane.tabs[pane.active_tab].entity_id();
        let tab = pane.tabs.remove(from);
        pane.tabs.insert(to, tab);
        if let Some(position) = pane
            .tabs
            .iter()
            .position(|tab| tab.entity_id() == active_id)
        {
            pane.active_tab = position;
        }
        true
    }

    /// Sets the orientation a split renders with.
    pub fn set_direction(&mut self, direction: SplitDirection) {
        self.direction = direction;
    }

    /// Makes `index` the active pane and focuses its active tab. Returns whether
    /// the active pane changed (out-of-range or unchanged indices are ignored).
    pub fn set_active(&mut self, index: usize, window: &mut Window, cx: &mut App) -> bool {
        if index >= self.panes.len() || index == self.active {
            return false;
        }
        self.active = index;
        self.focus_active(window, cx);
        true
    }

    /// Moves focus to the next pane, wrapping around. Returns whether it moved.
    pub fn focus_next(&mut self, window: &mut Window, cx: &mut App) -> bool {
        if self.panes.len() < 2 {
            return false;
        }
        self.set_active((self.active + 1) % self.panes.len(), window, cx)
    }

    /// Moves focus to the previous pane, wrapping around. Returns whether it moved.
    pub fn focus_prev(&mut self, window: &mut Window, cx: &mut App) -> bool {
        if self.panes.len() < 2 {
            return false;
        }
        let count = self.panes.len();
        self.set_active((self.active + count - 1) % count, window, cx)
    }

    /// Focuses the active pane's active tab.
    pub fn focus_active(&self, window: &mut Window, cx: &mut App) {
        let handle = self.active_pane().read(cx).focus_handle(cx);
        handle.focus(window);
    }

    /// The active tab of the active pane, falling back to the first to avoid
    /// panicking.
    pub fn active_pane(&self) -> &Entity<T> {
        self.panes
            .get(self.active)
            .unwrap_or(&self.panes[0])
            .active()
    }

    /// The active tab of every pane, in pane order.
    pub fn active_tabs(&self) -> Vec<Entity<T>> {
        self.panes
            .iter()
            .map(|pane| pane.active().clone())
            .collect()
    }

    /// Every tab across every pane, in display order.
    pub fn all_tabs(&self) -> Vec<Entity<T>> {
        self.panes
            .iter()
            .flat_map(|pane| pane.tabs.iter().cloned())
            .collect()
    }

    /// The active tab of every pane except the one containing the tab `id`. Used
    /// by the embedder to mirror navigation across panes.
    pub fn other_active_tabs(&self, id: EntityId) -> Vec<Entity<T>> {
        let source_pane = self.locate_tab(id).map(|(pane_index, _)| pane_index);
        self.panes
            .iter()
            .enumerate()
            .filter(|(pane_index, _)| Some(*pane_index) != source_pane)
            .map(|(_, pane)| pane.active().clone())
            .collect()
    }

    /// Whether the tab `id` is the active tab of its pane.
    pub fn is_active_tab(&self, id: EntityId) -> bool {
        match self.locate_tab(id) {
            Some((pane_index, tab_index)) => self.panes[pane_index].active_tab == tab_index,
            None => false,
        }
    }

    /// Locate a tab by entity id, returning `(pane_index, tab_index)`.
    pub fn locate_tab(&self, id: EntityId) -> Option<(usize, usize)> {
        for (pane_index, pane) in self.panes.iter().enumerate() {
            for (tab_index, tab) in pane.tabs.iter().enumerate() {
                if tab.entity_id() == id {
                    return Some((pane_index, tab_index));
                }
            }
        }
        None
    }

    /// Returns the metadata attached to a tab entity.
    pub fn tab_metadata(&self, id: EntityId) -> TabMetadata {
        self.tab_metadata.get(&id).copied().unwrap_or_default()
    }

    /// Updates metadata for an existing tab entity.
    pub fn set_tab_metadata(&mut self, id: EntityId, metadata: TabMetadata) {
        if self.locate_tab(id).is_some() {
            self.tab_metadata.insert(id, metadata);
        }
    }

    /// Number of open panes (1 or 2).
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Index of the active pane.
    pub fn active(&self) -> usize {
        self.active
    }

    /// Number of tabs in `pane_index` (0 for an out-of-range pane).
    pub fn tab_count(&self, pane_index: usize) -> usize {
        self.panes
            .get(pane_index)
            .map(|pane| pane.tabs.len())
            .unwrap_or(0)
    }

    /// Active tab index of `pane_index` (0 for an out-of-range pane).
    pub fn active_tab(&self, pane_index: usize) -> usize {
        self.panes
            .get(pane_index)
            .map(|pane| pane.active_tab)
            .unwrap_or(0)
    }

    /// The tab at `(pane_index, tab_index)`, if it exists.
    pub fn tab(&self, pane_index: usize, tab_index: usize) -> Option<Entity<T>> {
        self.panes
            .get(pane_index)
            .and_then(|pane| pane.tabs.get(tab_index).cloned())
    }

    /// Current split orientation.
    pub fn direction(&self) -> SplitDirection {
        self.direction
    }

    /// Renders the split. The `callbacks` route every interaction (pane/tab
    /// activate, close, new tab, reorder) back to the embedding view `V` so it
    /// can keep its own state in sync.
    pub fn render<V>(&self, window: &Window, cx: &mut Context<V>, callbacks: &PaneGroupCallbacks<V>) -> AnyElement
    where
        V: 'static,
    {
        if self.panes.len() < 2 {
            return self.render_pane(0, window, cx, callbacks);
        }
        let first = self.render_pane(0, window, cx, callbacks);
        let second = self.render_pane(1, window, cx, callbacks);
        let across = self.direction == SplitDirection::Vertical;
        let collapsing = match self.drag {
            Some(GroupDrag::Divider { collapsing }) => collapsing,
            _ => None,
        };
        let held = matches!(self.drag, Some(GroupDrag::Divider { .. }));
        let bounds = self.bounds.get();
        let length = f32::from(if across { bounds.size.width } else { bounds.size.height });
        // A pane about to close fades, so letting go is no surprise.
        let fade = |index: usize, element: AnyElement| {
            div()
                .size_full()
                .when(collapsing == Some(index), |this| this.opacity(0.35))
                .child(element)
        };
        let on_group_drag = callbacks.on_group_drag.clone();
        let side = if across { nohrs_core::config::DockSide::Left } else { nohrs_core::config::DockSide::Top };
        let at = length * self.ratio - DIVIDER_GRIP / 2.0;
        let divider = crate::dock::strip("pane-divider", side, held)
            .absolute()
            .when(across, |this| this.top_0().bottom_0().w(px(DIVIDER_GRIP)).left(px(at)))
            .when(!across, |this| this.left_0().right_0().h(px(DIVIDER_GRIP)).top(px(at)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let drag = if event.click_count >= 2 {
                        GroupDrag::EvenOut
                    } else {
                        GroupDrag::Divider { collapsing: None }
                    };
                    on_group_drag(this, drag, window, cx);
                }),
            );
        div()
            .relative()
            .size_full()
            .flex()
            .when(across, |this| this.flex_row())
            .when(!across, |this| this.flex_col())
            .child(
                div()
                    .flex_shrink_0()
                    .overflow_hidden()
                    .when(across, |this| this.w(relative(self.ratio)).h_full().border_r_1())
                    .when(!across, |this| this.h(relative(self.ratio)).w_full().border_b_1())
                    .border_color(rgb(theme::border()))
                    .child(fade(0, first)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(fade(1, second)),
            )
            .child(crate::dock::measure(self.bounds.clone()))
            .child(divider)
            .into_any_element()
    }

    /// Whether the tab row slides out of compact mode's frame, floating or
    /// pushing: either way it is part of the frame, with no line of its own.
    fn tabs_in_frame(&self) -> bool {
        self.compact.enabled && self.compact.hide_tabs && self.compact.frame_px > 0
    }

    /// Whether the tab row is the window's title bar: with one pane it
    /// carries the window's buttons and the toolbar above is not drawn.
    pub fn tabs_in_title_bar(&self) -> bool {
        self.panes.len() == 1 && !self.vertical_tabs && self.title_bar_allowed
    }

    fn render_pane<V>(
        &self,
        pane_index: usize,
        window: &Window,
        cx: &mut Context<V>,
        callbacks: &PaneGroupCallbacks<V>,
    ) -> AnyElement
    where
        V: 'static,
    {
        let content = self.panes[pane_index].active().clone();
        let split = self.panes.len() > 1;
        let is_active = split && pane_index == self.active;
        let on_activate_pane = callbacks.on_activate_pane.clone();
        let split_available = self.panes.len() < MAX_PANES;
        content.update(cx, |item, _| item.set_split_available(split_available));
        if self.vertical_tabs {
            // The column stays out in compact mode: its edge is the
            // sidebar's to slide from.
            let reveal = self.panes[pane_index].reveal.clone();
            reveal.set_inset(0.0);
            let compact = Compact { hide_tabs: false, ..self.compact.clone() };
            content.update(cx, |item, _| item.attach_compact(&compact, &reveal));
            let column = self.render_tab_column(pane_index, split, cx, callbacks);
            let body = div().flex_1().min_w(px(0.0)).h_full().overflow_hidden().child(content);
            // The column docks to the side the user chose; the listing takes
            // the rest either way.
            let row = div().flex().flex_row().size_full().min_w(px(0.0)).min_h(px(0.0));
            let row = if self.vertical_tabs_side == DockSide::Right {
                row.child(body).child(column)
            } else {
                row.child(column).child(body)
            };
            return row
                .when(is_active, |this| this.border_t_2().border_color(rgb(theme::accent())))
                .when(split && !is_active, |this| this.border_t_2().border_color(rgb(theme::bg())))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, window, cx| on_activate_pane(this, pane_index, window, cx)),
                )
                .into_any_element();
        }
        let tab_bar = self
            .render_tab_bar(pane_index, split, window, cx, callbacks)
            .into_any_element();
        let reveal = self.panes[pane_index].reveal.clone();
        if self.compact.enabled && reveal.tick(&self.compact, Instant::now()) {
            window.request_animation_frame();
        }
        content.update(cx, |item, _| item.attach_compact(&self.compact, &reveal));
        let (in_flow, layered) = if self.compact.enabled && self.compact.hide_tabs {
            self.render_compact_tab_bar(pane_index, tab_bar, split, cx)
        } else {
            reveal.set_inset(0.0);
            (Some(tab_bar), Vec::new())
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .when(is_active, |this| {
                this.border_t_2().border_color(rgb(theme::accent()))
            })
            .when(split && !is_active, |this| {
                this.border_t_2().border_color(rgb(theme::bg()))
            })
            // Clicking anywhere in a pane makes it the active one.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, window, cx| {
                    on_activate_pane(this, pane_index, window, cx)
                }),
            )
            .relative()
            .children(in_flow)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(content),
            )
            // After the content, so what floats is painted over it.
            .children(layered)
            .into_any_element()
    }

    // Compact mode: the row slides in from the top edge (see `compact`),
    // floating over the content or pushing it down. The edge strip goes
    // last, over everything, so nothing that blocks the mouse (a button
    // that occludes, say) can hide the pointer from it.
    //
    // Returns what goes above the content in the column and what floats
    // over the pane.
    fn render_compact_tab_bar<V>(
        &self,
        pane_index: usize,
        tab_bar: AnyElement,
        split: bool,
        cx: &mut Context<V>,
    ) -> (Option<AnyElement>, Vec<AnyElement>)
    where
        V: 'static,
    {
        let reveal = self.panes[pane_index].reveal.clone();
        let bar_height = if split { 36.0 } else { 40.0 };
        let amount = reveal.amount();
        let shown_height = bar_height * amount;
        let overlay = self.compact.overlay;
        reveal.set_inset(if overlay { shown_height } else { 0.0 });

        let sliding = compact::slider(Edge::Top, 0.0, bar_height, amount, None, div().h(px(bar_height)).child(tab_bar))
            // Floating, it takes the mouse from what lies behind: a click on
            // a tab, or on the window's buttons, must not reach the files
            // under it (they would claim the press, and gpui then drops the
            // minimize/maximize/close it was meant for).
            .when(overlay, |this| this.absolute().top_0().left_0().right_0().occlude())
            // With a frame the row is the frame's colour already and reads as
            // part of it, as the explorer's bars do; a shadow would lift it off.
            .when(overlay && amount > 0.0 && self.compact.frame_px == 0, |this| this.shadow_md());
        // Out of the frame, a line along the row's foot from one side of the
        // card to the other carries on the card's outline, which the row
        // covers while it is out.
        let frame = self.compact.frame_px as f32;
        // Pushing, the card below keeps its own outline and the line would
        // double it.
        let sliding = if self.tabs_in_frame() && overlay {
            sliding.child(
                div()
                    .absolute()
                    .bottom_0()
                    .left(px(frame))
                    .right(px(frame))
                    .h(px(1.0))
                    .bg(rgb(theme::border())),
            )
        } else {
            sliding
        };
        let strip = compact::edge_strip(
            ("compact-tabs-edge", pane_index),
            Edge::Top,
            (self.compact.edge_px as f32).max(shown_height),
            reveal,
            compact::SOURCE_TABS,
            cx.entity_id(),
        );
        if overlay {
            (None, vec![sliding.into_any_element(), strip.into_any_element()])
        } else {
            (Some(sliding.into_any_element()), vec![strip.into_any_element()])
        }
    }

    // The pane-local tab bar: one chip per tab (active highlight, per-tab close
    // when more than one tab, drag-to-reorder), the new-tab `+` button, and the
    // pane-close button which only appears once a split exists.
    fn render_tab_bar<V>(
        &self,
        pane_index: usize,
        split: bool,
        window: &Window,
        cx: &mut Context<V>,
        callbacks: &PaneGroupCallbacks<V>,
    ) -> impl IntoElement
    where
        V: 'static,
    {
        let pane = &self.panes[pane_index];
        let tab_count = pane.tabs.len();
        let active_tab = pane.active_tab;
        let tabs: Vec<AnyElement> = pane
            .tabs
            .iter()
            .enumerate()
            .map(|(tab_index, tab)| {
                let (title, icon) = {
                    let item = tab.read(cx);
                    (item.tab_title(cx), item.tab_icon(cx))
                };
                self.render_tab(
                    pane_index,
                    tab_index,
                    title,
                    icon,
                    tab_index == active_tab,
                    tab_count > 1,
                    cx,
                    callbacks,
                )
            })
            .collect();

        let on_new_tab = callbacks.on_new_tab.clone();
        let on_close_pane = callbacks.on_close_pane.clone();

        // Tabs sit on the bar's bottom edge and the active one takes the
        // content's colour, so it reads as the front of the folder below, as in
        // Explorer and browsers.
        let title_bar = self.tabs_in_title_bar();
        div()
            .flex()
            .flex_row()
            .items_end()
            .h(px(if title_bar { 40.0 } else { 36.0 }))
            .w_full()
            .pl(px(6.0))
            .pr(px(if title_bar { 0.0 } else { 6.0 }))
            .pt(px(if title_bar { 8.0 } else { 4.0 }))
            .bg(rgb(theme::toolbar_bg()))
            // Out of compact mode's frame the row is part of it, as the
            // explorer's bars then are: no line under it.
            .when(!self.tabs_in_frame(), |this| this.border_b_1())
            .border_color(rgb(theme::border()))
            .when(title_bar, |this| {
                this.child(
                    div()
                        .id("pane-title-bar-app-name")
                        .flex_shrink_0()
                        .px(px(10.0))
                        .pb(px(5.0))
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::toolbar_text()))
                        .child(self.app_name.clone()),
                )
            })
            .children(tabs)
            // New-tab affordance (`Cmd/Ctrl+T`, §4 / §6).
            .child(
                div()
                    .id(SharedString::from(format!("new-tab-{pane_index}")))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(26.0))
                    .ml(px(2.0))
                    .mb(px(2.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme::toolbar_hover())))
                    // Swallow the press so the pane-wide `on_mouse_down` does not
                    // also fire (it would still activate the pane, which is fine,
                    // but keeps the interaction crisp).
                    .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        on_new_tab(this, pane_index, window, cx)
                    }))
                    .child(
                        Icon::new(IconName::Plus)
                            .size_4()
                            .text_color(rgb(theme::muted())),
                    ),
            )
            // The empty stretch beside the tabs moves the window, as it does in
            // Explorer and browsers; only the thin bar above did, and the tab
            // strip is where a hand goes to drag. Being a caption to Windows,
            // a double click there maximizes and Aero Snap works.
            .child(
                div()
                    .id(("tab-bar-drag", pane_index))
                    .flex_grow()
                    .h_full()
                    .window_control_area(WindowControlArea::Drag),
            )
            .when(split, |this| {
                this.child(
                    div()
                        .id(("close-pane", pane_index))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(22.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(theme::toolbar_hover())))
                        // Swallow the press so the pane-wide `on_mouse_down`
                        // doesn't activate the pane we're about to close.
                        .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            on_close_pane(this, pane_index, window, cx)
                        }))
                        .child(
                            Icon::new(IconName::Close)
                                .size_4()
                                .text_color(rgb(theme::toolbar_text())),
                        ),
                )
            })
            // One row with the tabs, as the Explorer of Windows 11: the
            // window's buttons reach the top edge, over the top padding.
            .when(title_bar, |this| {
                // Room for them at the end of the row, then the buttons
                // themselves, pinned to the top right corner.
                this.relative()
                    .child(div().flex_shrink_0().w(px(TITLE_CONTROLS_WIDTH)))
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .h(px(40.0))
                            .child(nohrs_ui::components::layout::unified_toolbar::title_bar_controls(window)),
                    )
            })
    }

    // The column of vertical tabs, as Zen's sidebar: one row per tab (icon,
    // name, close), a row to open another, and an edge that resizes it.
    // Narrow, it shows the icons alone. Reordered by dragging, as the row is.
    fn render_tab_column<V>(
        &self,
        pane_index: usize,
        split: bool,
        cx: &mut Context<V>,
        callbacks: &PaneGroupCallbacks<V>,
    ) -> AnyElement
    where
        V: 'static,
    {
        let width = TAB_COLUMN_WIDTH.get();
        let labels = width >= TAB_COLUMN_LABELS_MIN;
        // Copied out for the listeners below, which outlive this borrow.
        let side = self.vertical_tabs_side;
        let pane = &self.panes[pane_index];
        let tab_count = pane.tabs.len();
        let active_tab = pane.active_tab;
        let rows: Vec<(TabMetadata, AnyElement)> = pane
            .tabs
            .iter()
            .enumerate()
            .map(|(tab_index, tab)| {
                let (title, icon) = {
                    let item = tab.read(cx);
                    (item.tab_title(cx), item.tab_icon(cx))
                };
                let metadata = self.tab_metadata(tab.entity_id());
                let is_active = tab_index == active_tab;
                let on_context = callbacks.on_tab_context.clone();
                let on_activate_tab = callbacks.on_activate_tab.clone();
                let on_close_tab = callbacks.on_close_tab.clone();
                let on_middle_close = callbacks.on_close_tab.clone();
                let on_reorder_tab = callbacks.on_reorder_tab.clone();
                let drag_title = SharedString::from(title.clone());
                let tooltip_title = title.clone();
                let group = SharedString::from(format!("vtab-group-{pane_index}-{tab_index}"));
                let show_close = tab_count > 1 && !metadata.pinned && !metadata.resident;
                let row = div()
                    .id(SharedString::from(format!("vtab-{pane_index}-{tab_index}")))
                    .group(group.clone())
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(34.0))
                    .px(px(if labels { 10.0 } else { 0.0 }))
                    .when(metadata.resident, |this| this
                        .w(px(if labels { ((width - 22.0) / 3.0).max(36.0) } else { 32.0 }))
                        .h(px(if labels { 44.0 } else { 34.0 })).justify_center()
                        .bg(rgb(theme::bg_secondary()))
                        .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(tooltip_title.clone()).build(window, cx)))
                    .when(!labels, |this| this.justify_center())
                    .rounded(px(8.0))
                    .text_sm()
                    .cursor_pointer()
                    .when(is_active, |this| {
                        this.bg(rgb(theme::accent_subtle()))
                            .text_color(rgb(theme::accent()))
                            .border_1()
                            .border_color(rgb(theme::border()))
                    })
                    .when(metadata.pinned && !is_active, |this| this.bg(rgb(theme::bg_secondary())))
                    .when(!is_active, |this| {
                        this.text_color(rgb(theme::toolbar_text()))
                            .hover(|style| style.bg(rgb(theme::toolbar_hover())))
                    })
                    .on_mouse_down(MouseButton::Right, cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        on_context(this, pane_index, tab_index, event.position, window, cx);
                    }))
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        on_activate_tab(this, pane_index, tab_index, window, cx)
                    }))
                    .when(show_close, |this| {
                        this.on_mouse_down(
                            MouseButton::Middle,
                            cx.listener(move |this, _event, window, cx| {
                                cx.stop_propagation();
                                on_middle_close(this, pane_index, tab_index, window, cx)
                            }),
                        )
                    })
                    .on_drag(
                        TabDrag {
                            pane: pane_index,
                            tab: tab_index,
                        },
                        move |_drag, _offset, _window, cx| {
                            cx.new(|_cx| TabDragPreview {
                                title: drag_title.clone(),
                            })
                        },
                    )
                    .on_drop(cx.listener(move |this, drag: &TabDrag, window, cx| {
                        if drag.pane == pane_index {
                            on_reorder_tab(this, pane_index, drag.tab, tab_index, window, cx);
                        }
                    }))
                    .child(
                        Icon::new(icon.unwrap_or(IconName::Folder))
                            .size_4()
                            .flex_shrink_0()
                            .text_color(rgb(metadata.color.unwrap_or(if is_active { theme::accent() } else { theme::muted() }))),
                    )
                    .when(labels && !metadata.resident, |this| {
                        this.child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .when(is_active, |this| this.font_weight(gpui::FontWeight::MEDIUM))
                                .child(title),
                        )
                    })
                    .when(labels && show_close, |this| {
                        this.child(
                            div()
                                .id(SharedString::from(format!("vtab-close-{pane_index}-{tab_index}")))
                                .flex()
                                .flex_shrink_0()
                                .items_center()
                                .justify_center()
                                .size(px(20.0))
                                .rounded(px(4.0))
                                .when(!is_active, |this| {
                                    this.opacity(0.0).group_hover(group.clone(), |style| style.opacity(1.0))
                                })
                                .text_color(rgb(theme::muted()))
                                .hover(|style| {
                                    style.bg(rgb(theme::toolbar_hover())).text_color(rgb(theme::fg()))
                                })
                                .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _event, window, cx| {
                                    on_close_tab(this, pane_index, tab_index, window, cx)
                                }))
                                .child(Icon::new(IconName::Close).size_3()),
                        )
                    })
                    .into_any_element();
                (metadata, row)
            })
            .collect();
        let mut resident = Vec::new();
        let mut pinned = Vec::new();
        let mut ordinary = Vec::new();
        for (metadata, row) in rows {
            if metadata.resident { resident.push(row); }
            else if metadata.pinned { pinned.push(row); }
            else { ordinary.push(row); }
        }
        let on_new_tab = callbacks.on_new_tab.clone();
        let on_close_pane = callbacks.on_close_pane.clone();
        let on_group_drag = callbacks.on_group_drag.clone();
        let new_tab = div()
            .id(SharedString::from(format!("vtab-new-{pane_index}")))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(8.0))
            .h(px(32.0))
            .px(px(if labels { 10.0 } else { 0.0 }))
            .when(!labels, |this| this.justify_center())
            .rounded(px(8.0))
            .text_sm()
            .text_color(rgb(theme::muted()))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::toolbar_hover())))
            .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _event, window, cx| on_new_tab(this, pane_index, window, cx)))
            .child(Icon::new(IconName::Plus).size_4().flex_shrink_0())
            .when(labels, |this| this.child(nohrs_core::t!("Nova aba").to_string()));
        div()
            .relative()
            .flex_shrink_0()
            .w(px(width))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(8.0))
            .bg(rgb(theme::toolbar_bg()))
            // The seam between the column and the listing, on the column's
            // own inner edge.
            .when(side == DockSide::Right, |this| {
                this.border_l_1().border_color(rgb(theme::border()))
            })
            .when(side != DockSide::Right, |this| {
                this.border_r_1().border_color(rgb(theme::border()))
            })
            .child(
                div().flex().items_center().justify_between().h(px(36.0)).px(px(6.0))
                    .when(labels, |row| row.child(div().text_xs()
                        .text_color(rgb(theme::muted())).child(nohrs_core::t!("Abas"))))
                    .child(div().id(("vtab-collapse", pane_index)).size(px(24.0))
                        .flex().items_center().justify_center().rounded(px(5.0)).cursor_pointer()
                        .text_color(rgb(theme::muted()))
                        .hover(|style| style.bg(rgb(theme::toolbar_hover())))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            TAB_COLUMN_WIDTH.set(if labels { TAB_COLUMN_ICONS } else { TAB_COLUMN_DEFAULT });
                            cx.notify();
                        }))
                        .child(Icon::new(IconName::Menu).size_4())),
            )
            .when(split, |this| {
                this.child(
                    div()
                        .id(("vtab-close-pane", pane_index))
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .h(px(24.0))
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .text_color(rgb(theme::muted()))
                        .hover(|style| style.bg(rgb(theme::toolbar_hover())))
                        .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            on_close_pane(this, pane_index, window, cx)
                        }))
                        .child(Icon::new(IconName::Close).size_3()),
                )
            })
            .child(
                div()
                    .id(("vtab-list", pane_index))
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .overflow_y_scroll()
                    .when(!resident.is_empty(), |list| list.child(
                        div().flex().flex_wrap().gap(px(3.0)).children(resident)))
                    .when(!pinned.is_empty(), |list| list.child(
                        div().flex().flex_col().gap(px(2.0)).pb(px(6.0))
                            .border_b_1().border_color(rgb(theme::border())).children(pinned)))
                    .child(new_tab)
                    .children(ordinary),
            )
            .child(
                // The resize strip sits on the column's inner edge: the right
                // one of a left-hand column, the left one of a right-hand one.
                crate::dock::place_on_inner_edge(
                    crate::dock::strip(("vtab-edge", pane_index), side, false),
                    self.vertical_tabs_side,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        on_group_drag(
                            this,
                            GroupDrag::TabColumn {
                                start_x: f32::from(event.position.x),
                                start_width: TAB_COLUMN_WIDTH.get(),
                                from_right: side == DockSide::Right,
                            },
                            window,
                            cx,
                        );
                    }),
                ),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_tab<V>(
        &self,
        pane_index: usize,
        tab_index: usize,
        title: String,
        icon: Option<IconName>,
        is_active: bool,
        show_close: bool,
        cx: &mut Context<V>,
        callbacks: &PaneGroupCallbacks<V>,
    ) -> AnyElement
    where
        V: 'static,
    {
        let on_context = callbacks.on_tab_context.clone();
        let metadata = self.tab(pane_index, tab_index).map(|tab| self.tab_metadata(tab.entity_id())).unwrap_or_default();
        let show_close = show_close && !metadata.pinned && !metadata.resident;
        let on_activate_tab = callbacks.on_activate_tab.clone();
        let on_close_tab = callbacks.on_close_tab.clone();
        let on_reorder_tab = callbacks.on_reorder_tab.clone();
        let drag_title = SharedString::from(title.clone());
        let on_middle_close = callbacks.on_close_tab.clone();
        let group = SharedString::from(format!("tab-group-{pane_index}-{tab_index}"));

        div()
            .id(SharedString::from(format!("tab-{pane_index}-{tab_index}")))
            .group(group.clone())
            .relative()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(32.0))
            // A fixed band of widths, as browsers do: many tabs shrink
            // together, and a long folder name cannot push the rest away.
            .min_w(px(110.0))
            .max_w(px(220.0))
            .flex_shrink()
            .pl(px(12.0))
            .pr(px(6.0))
            .rounded_t(px(8.0))
            .text_sm()
            .cursor_pointer()
            .when(is_active, |this| {
                this.bg(rgb(theme::bg()))
                    .text_color(rgb(theme::fg()))
                    .border_1()
                    .border_b_0()
                    .border_color(rgb(theme::border()))
                    // Covers the bar's bottom border, joining the tab to the
                    // content beneath it.
                    .mb(px(-1.0))
                    .h(px(33.0))
            })
            .when(!is_active, |this| {
                this.text_color(rgb(theme::toolbar_text()))
                    .hover(|style| style.bg(rgb(theme::toolbar_hover())))
                    // A hairline between neighbouring inactive tabs.
                    .child(
                        div()
                            .absolute()
                            .right_0()
                            .top(px(9.0))
                            .h(px(14.0))
                            .w(px(1.0))
                            .bg(rgb(theme::border())),
                    )
            })
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                on_context(this, pane_index, tab_index, event.position, window, cx);
            }))
            .on_click(cx.listener(move |this, _event, window, cx| {
                on_activate_tab(this, pane_index, tab_index, window, cx)
            }))
            // Middle click closes, as in every tabbed browser and Explorer.
            .when(show_close, |this| {
                this.on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _event, window, cx| {
                        cx.stop_propagation();
                        on_middle_close(this, pane_index, tab_index, window, cx)
                    }),
                )
            })
            .on_drag(
                TabDrag {
                    pane: pane_index,
                    tab: tab_index,
                },
                move |_drag, _offset, _window, cx| {
                    cx.new(|_cx| TabDragPreview {
                        title: drag_title.clone(),
                    })
                },
            )
            // Only same-pane reorders are supported (§4 — reorder within the tab
            // bar); a drop from another pane is ignored.
            .on_drop(cx.listener(move |this, drag: &TabDrag, window, cx| {
                if drag.pane == pane_index {
                    on_reorder_tab(this, pane_index, drag.tab, tab_index, window, cx);
                }
            }))
            .when_some(icon.or(metadata.color.map(|_| IconName::Folder)), |this, icon| {
                this.child(
                    Icon::new(icon)
                        .size_4()
                        .flex_shrink_0()
                        .text_color(rgb(metadata.color.unwrap_or(if is_active { theme::accent() } else { theme::muted() }))),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .when(is_active, |this| this.font_weight(gpui::FontWeight::MEDIUM))
                    .child(title),
            )
            .when(show_close, |this| {
                this.child(
                    div()
                        .id(SharedString::from(format!(
                            "close-tab-{pane_index}-{tab_index}"
                        )))
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .size(px(20.0))
                        .rounded(px(4.0))
                        // Only on the active tab and the one under the
                        // pointer, so a row of tabs is not a row of crosses.
                        .when(!is_active, |this| {
                            this.opacity(0.0).group_hover(group.clone(), |style| style.opacity(1.0))
                        })
                        .text_color(rgb(theme::muted()))
                        .hover(|style| {
                            style
                                .bg(rgb(theme::toolbar_hover()))
                                .text_color(rgb(theme::fg()))
                        })
                        // Swallow the press so closing a tab doesn't also activate
                        // it (or start a drag).
                        .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            on_close_tab(this, pane_index, tab_index, window, cx)
                        }))
                        .child(Icon::new(IconName::Close).size_3()),
                )
            })
            .into_any_element()
    }
}

