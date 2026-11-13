use nohrs_core::{t, tf};
use std::sync::Mutex;

use gpui::{AppContext as _, AsyncWindowContext, Context, Pixels, Point, WeakEntity, Window};
use nohrs_core::config::{self, ThemeMode};
use nohrs_core::telemetry::LogErr;

use crate::explorer::commands::{OpenIn, interned, keys_of};
use nohrs_core::shortcuts::Action;
use super::ExplorerPane;
use super::grouping::GroupBy;
use super::types::{SortKey, StatusLevel, ViewMode};
use crate::shell_menu::{self, Choice, Entry, MenuItem, Request, Session, Target};

// Nohrs's own menu entries. The values are the menu ids, so they stay below
// the range the Shell's entries are numbered from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
enum Command {
    ViewList = 1,
    ViewGrid,
    ViewTree,
    ViewCompact,
    ToggleHidden,
    ThemeDark,
    ThemeLight,
    ThemeSystem,
    SortName,
    SortType,
    SortSize,
    SortModified,
    SortAscending,
    SortDescending,
    Refresh,
    Paste,
    NewFolder,
    GroupNone,
    GroupKind,
    GroupModified,
    GroupSize,
    FitColumn,
    FitAllColumns,
    ColumnName,
    ColumnType,
    ColumnSize,
    ColumnModified,
    Cut,
    Copy,
    Rename,
    Delete,
    MoreOptions,
    OpenCommandPrompt,
    OpenPowerShell,
    OpenWindowsTerminal,
    OpenFileExplorer,
    NewTag,
    RenameTag,
    DeleteTag,
    RemoveNetworkPlace,
    RemoveFavorite,
    RenameFavorite,
    OpenWorkspaceCard,
    RemoveWorkspaceCard,
    AddFavoriteCurrent,
}

impl Command {
    const ALL: [Command; 45] = [
        Command::ViewList,
        Command::ViewGrid,
        Command::ViewTree,
        Command::ViewCompact,
        Command::ToggleHidden,
        Command::ThemeDark,
        Command::ThemeLight,
        Command::ThemeSystem,
        Command::SortName,
        Command::SortType,
        Command::SortSize,
        Command::SortModified,
        Command::SortAscending,
        Command::SortDescending,
        Command::Refresh,
        Command::Paste,
        Command::NewFolder,
        Command::GroupNone,
        Command::GroupKind,
        Command::GroupModified,
        Command::GroupSize,
        Command::FitColumn,
        Command::FitAllColumns,
        Command::ColumnName,
        Command::ColumnType,
        Command::ColumnSize,
        Command::ColumnModified,
        Command::Cut,
        Command::Copy,
        Command::Rename,
        Command::Delete,
        Command::MoreOptions,
        Command::OpenCommandPrompt,
        Command::OpenPowerShell,
        Command::OpenWindowsTerminal,
        Command::OpenFileExplorer,
        Command::NewTag,
        Command::RenameTag,
        Command::DeleteTag,
        Command::RemoveNetworkPlace,
        Command::RemoveFavorite,
        Command::RenameFavorite,
        Command::OpenWorkspaceCard,
        Command::RemoveWorkspaceCard,
        Command::AddFavoriteCurrent,
    ];

    fn from_id(id: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|command| *command as u32 == id)
    }
}

// The theme mode the config asks for, as last applied by the root view. The
// pane only needs it to tick the right entry, and `system` cannot be told
// apart from the palette it resolved to.
static THEME_MODE: Mutex<ThemeMode> = Mutex::new(ThemeMode::Dark);

/// Records the configured theme mode, for the menu's Theme entries.
pub(crate) fn note_theme_mode(mode: ThemeMode) {
    if let Ok(mut current) = THEME_MODE.lock() {
        *current = mode;
    }
}

// The directory menu's entries: a shortcut becomes an entry that opens it;
// a submenu the user made up becomes a submenu of the menu, holding its
// own. `counter` numbers the shortcuts in menu order, the same order
// `nth_favorite_path` reads them back in.
fn favorite_menu_entries(items: &[super::favorites::FavoriteItem], counter: &mut u32) -> Vec<Entry> {
    items
        .iter()
        .filter_map(|item| {
            if item.is_submenu() {
                Some(Entry::submenu(
                    interned(item.name.clone()),
                    favorite_menu_entries(&item.children, counter),
                ))
            } else if *counter < FAVORITE_IDS {
                let id = FIRST_FAVORITE + *counter;
                *counter += 1;
                Some(Entry::command(id, interned(item.name.clone())).with_icon("icons/folder.svg"))
            } else {
                None
            }
        })
        .collect()
}

// The path of the shortcut `index` places into the menu, counting depth
// first as `favorite_menu_entries` numbered them.
fn nth_favorite_path(items: &[super::favorites::FavoriteItem], index: usize) -> Option<String> {
    let mut rest = index;
    for item in items {
        if item.is_submenu() {
            if let Some(path) = nth_favorite_path(&item.children, rest) {
                return Some(path);
            }
            rest -= favorite_link_count(&item.children);
        } else if rest == 0 {
            return Some(item.path.clone());
        } else {
            rest -= 1;
        }
    }
    None
}

fn favorite_link_count(items: &[super::favorites::FavoriteItem]) -> usize {
    items
        .iter()
        .map(|item| if item.is_submenu() { favorite_link_count(&item.children) } else { 1 })
        .sum()
}

fn theme_mode() -> ThemeMode {
    THEME_MODE.lock().map_or(ThemeMode::Dark, |mode| *mode)
}

/// Which Shell verbs the explorer runs itself. Copy and Cut go through Nohrs's
/// clipboard, so its own Paste and the conflict dialog see them; Delete goes to
/// the recycle bin with Nohrs's undo; Rename edits inline. Open on a folder
/// navigates here instead of opening another window.
fn intercepted(target_is_one_folder: bool) -> &'static [&'static str] {
    if target_is_one_folder {
        &["copy", "cut", "delete", "rename", "open", "explore"]
    } else {
        &["copy", "cut", "delete", "rename"]
    }
}

/// One of the icons in the row above a file's menu, as in Windows 11.
pub(crate) struct QuickAction {
    pub id: u32,
    pub icon: &'static str,
    pub label: &'static str,
}

/// What an open menu was opened for, which decides what its entries do.
#[derive(Clone)]
pub(crate) enum MenuPurpose {
    /// The listing: selected entries, or the folder's empty space.
    Listing {
        target: Target,
        one_folder: bool,
        extended: bool,
    },
    /// A place in the sidebar.
    Place(super::places::Place),
    /// The column headings; the column right-clicked, if any.
    Columns(Option<usize>),
    /// The funnel's kinds.
    Filter,
    /// A tag in the sidebar.
    Tag(u32),
    /// A network place added by hand.
    NetworkPlace(String),
    /// A favorite shortcut in the sidebar, by its entry id.
    FavoriteLink(u32),
    /// A favorite submenu in the sidebar, by its entry id.
    FavoriteSubmenu(u32),
    /// The directory menu (Ctrl+D), listing the favorite folders.
    FavoritesMenu,
    /// A folder card of a workspace page: the workspace it is in, and the
    /// folder it points at.
    WorkspaceCard { tag: u32, path: String },
}

/// A menu on screen: what it shows, where, and what runs its entries.
pub(crate) struct OpenMenu {
    pub items: Vec<MenuItem>,
    pub quick: Vec<QuickAction>,
    pub position: Point<Pixels>,
    /// The open submenu at each level, by index.
    pub open_path: Vec<usize>,
    session: Option<Session>,
    purpose: MenuPurpose,
}

// The filter menu's ids, past the commands' range.
const FIRST_KIND: u32 = 0x800;
// A tag's tick in a file's Tags submenu: this plus the tag's id, for the
// first TAG_IDS tags (the Shell's own entries start at 0x1000).
const FIRST_TAG: u32 = 0x900;
const TAG_IDS: u32 = 0x400;
// A colour in a tag's menu: this plus its place in `tags::COLORS`.
const FIRST_TAG_COLOR: u32 = 0x8F0;
// A favorite folder in the directory menu (Ctrl+D): this plus its place
// among the shortcuts, counted depth first. Past the tags' range, below the
// Shell's.
const FIRST_FAVORITE: u32 = 0xD00;
const FAVORITE_IDS: u32 = 0x100;
// A submenu the directory menu's "add current folder" can put the folder
// into: this plus its place among the tree's submenus, counted depth first.
const FIRST_SUBMENU_TARGET: u32 = 0xE00;
const SUBMENU_TARGETS: u32 = 0x100;

const SHELL_HIDDEN_FOR_ITEMS: &[&str] = &["paste", "cut", "copy", "rename", "delete"];

impl ExplorerPane {
    /// Opens the right-click menu: for the row `row` (selecting it first if
    /// it is not part of the selection), or for the folder's empty space.
    pub(crate) fn open_context_menu(
        &mut self,
        row: Option<usize>,
        extended: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = row
            && !self.is_selected(row)
        {
            self.select_single(row);
            cx.notify();
        }
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        let paths = if row.is_some() { self.selected_paths() } else { Vec::new() };
        let one_folder = match paths.as_slice() {
            [path] => self
                .filtered_entries
                .iter()
                .any(|entry| entry.kind == "dir" && &entry.path == path),
            _ => false,
        };
        let purpose = MenuPurpose::Listing {
            target: if paths.is_empty() {
                Target::Background(self.cwd.clone())
            } else {
                Target::Items(paths)
            },
            one_folder,
            extended,
        };
        let own = self.listing_entries(&purpose, cx);
        self.show_menu(purpose, own, hwnd, window, cx);
    }

    // Nohrs's entries for a listing menu: the view and sort submenus on the
    // folder's empty space, "Show more options" on files.
    fn listing_entries(&self, purpose: &MenuPurpose, cx: &mut Context<Self>) -> Vec<Entry> {
        let more = Entry::command(Command::MoreOptions as u32, t!("Mostrar mais opções"))
            .with_icon("icons/ellipsis.svg")
            .with_shortcut("Shift+F10");
        match purpose {
            MenuPurpose::Listing { target: Target::Background(_), .. } => {
                let mut entries = self.background_entries(cx);
                entries.push(Entry::Separator);
                entries.push(more);
                entries
            }
            MenuPurpose::Listing { target: Target::Items(paths), .. } => {
                // Ticked when every selected item has it; a click puts it on
                // all of them, or takes it off all of them.
                let mut tags: Vec<Entry> = super::tags::Tags::all(cx)
                    .into_iter()
                    .filter(|tag| tag.id < TAG_IDS)
                    .map(|tag| {
                        Entry::check(FIRST_TAG + tag.id, &tag.name, super::tags::Tags::all_have(paths, tag.id, cx))
                    })
                    .collect();
                tags.push(Entry::Separator);
                tags.push(Entry::command(Command::NewTag as u32, t!("Novo espaço de trabalho…")).with_icon("icons/plus.svg"));
                vec![Entry::submenu(t!("Espaços de trabalho"), tags).with_icon("icons/tag.svg"), Entry::Separator, more]
            }
            _ => vec![more],
        }
    }

    // Reads the menu for `purpose` and opens it at the pointer. The Shell is
    // asked between updates, as its extensions may pump messages.
    fn show_menu(
        &mut self,
        purpose: MenuPurpose,
        own: Vec<Entry>,
        hwnd: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = window.mouse_position();
        cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let (target, own_first, extended, intercept, hide, quick) = match &purpose {
                    MenuPurpose::Listing { target, one_folder, extended } => {
                        let files = matches!(target, Target::Items(_));
                        (
                            target.clone(),
                            !files,
                            *extended,
                            intercepted(*one_folder).to_vec(),
                            // Paste is Nohrs's own; on files the four in the
                            // row above replace the Shell's.
                            if files { SHELL_HIDDEN_FOR_ITEMS } else { &["paste"][..] },
                            files,
                        )
                    }
                    MenuPurpose::Place(place) => (
                        Target::Items(vec![place.parsing_name.clone()]),
                        false,
                        false,
                        if place.path.is_some() { vec!["open", "explore"] } else { Vec::new() },
                        &[][..],
                        false,
                    ),
                    MenuPurpose::Columns(_) | MenuPurpose::Filter | MenuPurpose::Tag(_) | MenuPurpose::NetworkPlace(_)
                    | MenuPurpose::FavoriteLink(_) | MenuPurpose::FavoriteSubmenu(_) | MenuPurpose::FavoritesMenu
                    | MenuPurpose::WorkspaceCard { .. } => {
                        (Target::None, true, false, Vec::new(), &[][..], false)
                    }
                };
                let prepared = shell_menu::prepare(&Request {
                    hwnd: hwnd as isize,
                    target: &target,
                    own: &own,
                    own_first,
                    extended,
                    dark: true,
                    intercept: &intercept,
                    hide,
                });
                this.update(cx, |pane, cx| {
                    pane.context_menu = Some(OpenMenu {
                        items: prepared.items,
                        quick: if quick { quick_actions() } else { Vec::new() },
                        position,
                        open_path: Vec::new(),
                        session: Some(prepared.session),
                        purpose,
                    });
                    cx.notify();
                })
                .log_err();
            },
        )
        .detach();
    }

    /// Closes the open menu without choosing.
    pub(crate) fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    /// The pointer is over entry `index` of `level`: its submenu opens, and
    /// any deeper one closes.
    pub(crate) fn hover_menu_entry(&mut self, level: usize, index: usize, submenu: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let wanted: Vec<usize> = menu
            .open_path
            .iter()
            .take(level)
            .copied()
            .chain(submenu.then_some(index))
            .collect();
        if menu.open_path != wanted {
            menu.open_path = wanted;
            cx.notify();
        }
    }

    /// Runs entry `id` of the open menu and closes it.
    pub(crate) fn choose_menu_item(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.take() else {
            return;
        };
        cx.notify();
        if id == Command::MoreOptions as u32 {
            self.show_classic_menu(menu.purpose, window, cx);
            return;
        }
        let purpose = menu.purpose;
        let Some(session) = menu.session else {
            self.apply_choice(purpose, Choice::Own(id), window, cx);
            return;
        };
        // A Shell command may open its own dialogs: it runs between updates.
        cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let choice = shell_menu::run(session, id);
                this.update_in(cx, |pane, window, cx| pane.apply_choice(purpose, choice, window, cx))
                    .log_err();
            },
        )
        .detach();
    }

    // What an entry does, by what the menu was opened for.
    fn apply_choice(&mut self, purpose: MenuPurpose, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        match purpose {
            MenuPurpose::Listing { .. } => self.apply_menu_choice(choice, window, cx),
            MenuPurpose::Place(place) => {
                match choice {
                    Choice::Verb(_) => self.open_place(&place, window, cx),
                    // Unpinning, renaming or deleting changes the list.
                    Choice::Ran => self.refresh_places(),
                    Choice::Dismissed | Choice::Own(_) => {}
                }
                cx.notify();
            }
            MenuPurpose::Columns(column) => {
                if let Choice::Own(id) = choice {
                    match Command::from_id(id) {
                        Some(Command::FitColumn) => {
                            if let Some(column) = column {
                                self.fit_column(column);
                            }
                        }
                        Some(Command::FitAllColumns) => (0..4).for_each(|column| self.fit_column(column)),
                        Some(Command::ColumnType) => self.toggle_column(1),
                        Some(Command::ColumnSize) => self.toggle_column(2),
                        Some(Command::ColumnModified) => self.toggle_column(3),
                        _ => {}
                    }
                }
                cx.notify();
            }
            MenuPurpose::Tag(tag) => {
                if let Choice::Own(id) = choice {
                    match Command::from_id(id) {
                        Some(Command::RenameTag) => {
                            let name = super::tags::Tags::get(tag, cx).map(|tag| tag.name).unwrap_or_default();
                            self.begin_sidebar_input(super::places::SidebarInputPurpose::RenameTag(tag), name, window, cx);
                        }
                        Some(Command::DeleteTag) => {
                            super::tags::Tags::delete(tag, cx);
                            if super::tags::tag_of_location(&self.cwd) == Some(tag) {
                                self.go_back(window, cx);
                            }
                        }
                        _ => {
                            if let Some((color, _)) = id
                                .checked_sub(FIRST_TAG_COLOR)
                                .and_then(|index| super::tags::COLORS.get(index as usize))
                            {
                                super::tags::Tags::recolor(tag, *color, cx);
                            }
                        }
                    }
                }
                cx.notify();
            }
            MenuPurpose::NetworkPlace(address) => {
                if let Choice::Own(id) = choice
                    && Command::from_id(id) == Some(Command::RemoveNetworkPlace)
                {
                    super::network::NetworkPlaces::remove(&address, cx);
                }
                cx.notify();
            }
            // Taking a favorite out only drops the shortcut: the folder
            // itself is never touched. Renaming opens a field in the
            // sidebar; a submenu takes the pane's current folder in.
            MenuPurpose::FavoriteLink(favorite) => {
                if let Choice::Own(id) = choice {
                    match Command::from_id(id) {
                        Some(Command::RenameFavorite) => self.begin_favorite_rename(favorite, window, cx),
                        Some(Command::RemoveFavorite) => super::favorites::Favorites::remove(favorite, cx),
                        _ => {}
                    }
                }
                cx.notify();
            }
            MenuPurpose::FavoriteSubmenu(favorite) => {
                if let Choice::Own(id) = choice {
                    match Command::from_id(id) {
                        Some(Command::RenameFavorite) => self.begin_favorite_rename(favorite, window, cx),
                        Some(Command::AddFavoriteCurrent) => {
                            super::favorites::Favorites::add_link(Some(favorite), &self.cwd, cx);
                        }
                        Some(Command::RemoveFavorite) => super::favorites::Favorites::remove(favorite, cx),
                        _ => {}
                    }
                }
                cx.notify();
            }
            // The directory menu (Ctrl+D): an entry goes to its folder;
            // "add current folder" puts the pane's folder at the top of the
            // list or into one of the submenus.
            MenuPurpose::FavoritesMenu => {
                if let Choice::Own(id) = choice {
                    if let Some(index) = id.checked_sub(FIRST_FAVORITE).filter(|offset| *offset < FAVORITE_IDS) {
                        if let Some(path) = nth_favorite_path(&super::favorites::Favorites::all(cx), index as usize) {
                            self.change_dir(path, window, cx);
                        }
                    } else if Command::from_id(id) == Some(Command::AddFavoriteCurrent) {
                        super::favorites::Favorites::add_link(None, &self.cwd, cx);
                    } else if let Some(index) = id
                        .checked_sub(FIRST_SUBMENU_TARGET)
                        .filter(|offset| *offset < SUBMENU_TARGETS)
                        && let Some((submenu, _)) = super::favorites::Favorites::submenus(cx).get(index as usize)
                    {
                        super::favorites::Favorites::add_link(Some(*submenu), &self.cwd, cx);
                    }
                }
                cx.notify();
            }
            // A folder card of a workspace: opening it, or taking it out of
            // the workspace — the folder itself is never touched.
            MenuPurpose::WorkspaceCard { tag, path } => {
                if let Choice::Own(id) = choice {
                    match Command::from_id(id) {
                        Some(Command::OpenWorkspaceCard) => self.change_dir(path, window, cx),
                        Some(Command::RemoveWorkspaceCard) => {
                            super::tags::Tags::remove_path(&path, tag, cx);
                            self.refresh_workspace(cx);
                            self.reload(cx);
                        }
                        _ => {}
                    }
                }
                cx.notify();
            }
            MenuPurpose::Filter => {
                use super::filter::KindFilter;
                if let Choice::Own(id) = choice
                    && let Some(kind) = id
                        .checked_sub(FIRST_KIND)
                        .and_then(|index| KindFilter::ALL.get(index as usize).copied())
                {
                    self.set_kind_filter(kind, cx);
                }
            }
        }
    }

    // "Show more options": the classic Windows menu, with every entry an
    // extension draws itself, as Windows 11 keeps it.
    fn show_classic_menu(&mut self, purpose: MenuPurpose, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        let MenuPurpose::Listing { target, one_folder, extended } = purpose.clone() else {
            return;
        };
        let own = match &target {
            Target::Background(_) => self.background_entries(cx),
            _ => Vec::new(),
        };
        let dark = nohrs_ui::theme::theme::is_dark();
        cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let choice = shell_menu::show(&Request {
                    hwnd: hwnd as isize,
                    target: &target,
                    own: &own,
                    own_first: matches!(target, Target::Background(_)),
                    extended,
                    dark,
                    intercept: intercepted(one_folder),
                    hide: &["paste"],
                });
                this.update_in(cx, |pane, window, cx| pane.apply_choice(purpose, choice, window, cx))
                    .log_err();
            },
        )
        .detach();
    }

    /// The Windows menu for a sidebar place: Unpin from Quick access, Open
    /// in new window, Properties and the rest, as in Explorer's pane.
    pub(crate) fn open_place_menu(
        &mut self,
        place: super::places::Place,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        self.show_menu(MenuPurpose::Place(place), Vec::new(), hwnd, window, cx);
    }

    /// A tag's menu in the sidebar: rename, colour, delete.
    pub(crate) fn open_tag_menu(&mut self, tag: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        let current = super::tags::Tags::get(tag, cx).map(|tag| tag.color);
        let colors = super::tags::COLORS
            .iter()
            .enumerate()
            .map(|(index, (color, name))| {
                Entry::radio(FIRST_TAG_COLOR + index as u32, nohrs_core::i18n::tr(name), current == Some(*color))
            })
            .collect();
        let own = vec![
            Entry::command(Command::RenameTag as u32, t!("Renomear")).with_icon("icons/pencil.svg"),
            Entry::submenu(t!("Cor"), colors).with_icon("icons/palette-2.svg"),
            Entry::Separator,
            Entry::command(Command::DeleteTag as u32, t!("Excluir espaço de trabalho")).with_icon("icons/trash-2.svg"),
        ];
        self.show_menu(MenuPurpose::Tag(tag), own, hwnd, window, cx);
    }

    /// A network place's menu in the sidebar.
    pub(crate) fn open_network_place_menu(&mut self, address: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        let own = vec![
            Entry::command(Command::RemoveNetworkPlace as u32, t!("Remover dos locais de rede")).with_icon("icons/trash-2.svg"),
        ];
        self.show_menu(MenuPurpose::NetworkPlace(address), own, hwnd, window, cx);
    }

    /// A folder card's menu on a workspace page: open it, or take it out of
    /// the workspace. The folder itself is never touched.
    pub(crate) fn open_workspace_card_menu(
        &mut self,
        tag: u32,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        let own = vec![
            Entry::command(Command::OpenWorkspaceCard as u32, t!("Abrir")).with_icon("icons/folder-open.svg"),
            Entry::Separator,
            Entry::command(Command::RemoveWorkspaceCard as u32, t!("Remover do espaço de trabalho"))
                .with_icon("icons/trash-2.svg"),
        ];
        self.show_menu(MenuPurpose::WorkspaceCard { tag, path }, own, hwnd, window, cx);
    }

    /// A favorite shortcut's menu in the sidebar: rename, or take the
    /// shortcut out. The folder itself is never touched.
    pub(crate) fn open_favorite_link_menu(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        let own = vec![
            Entry::command(Command::RenameFavorite as u32, t!("Renomear")).with_icon("icons/pencil.svg"),
            Entry::Separator,
            Entry::command(Command::RemoveFavorite as u32, t!("Remover das pastas favoritas"))
                .with_icon("icons/trash-2.svg"),
        ];
        self.show_menu(MenuPurpose::FavoriteLink(id), own, hwnd, window, cx);
    }

    /// A favorite submenu's menu in the sidebar: rename, take the pane's
    /// current folder into it, or delete it with everything it holds.
    pub(crate) fn open_favorite_submenu_menu(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        let own = vec![
            Entry::command(Command::RenameFavorite as u32, t!("Renomear")).with_icon("icons/pencil.svg"),
            Entry::command(Command::AddFavoriteCurrent as u32, t!("Adicionar pasta atual"))
                .with_icon("icons/plus.svg"),
            Entry::Separator,
            Entry::command(Command::RemoveFavorite as u32, t!("Remover das pastas favoritas"))
                .with_icon("icons/trash-2.svg"),
        ];
        self.show_menu(MenuPurpose::FavoriteSubmenu(id), own, hwnd, window, cx);
    }

    // Opens a field in the sidebar holding entry `id`'s name, selected.
    fn begin_favorite_rename(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let name = super::favorites::Favorites::get(id, cx).map(|item| item.name).unwrap_or_default();
        self.sidebar_visible = true;
        self.begin_sidebar_input(super::places::SidebarInputPurpose::RenameFavorite(id), name, window, cx);
    }

    /// The directory menu (Ctrl+D), as Total Commander's: the favorite
    /// folders in a popup, submenus the user made up as submenus of it, and
    /// the pane's current folder added to the list from there.
    pub(crate) fn open_favorites_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        let mut own = favorite_menu_entries(&super::favorites::Favorites::all(cx), &mut 0);
        if !own.is_empty() {
            own.push(Entry::Separator);
        }
        // "Add the current folder": at the top of the list, or into one of
        // the submenus already made. With no submenu to choose from, the
        // entry is the one command instead of a submenu of one.
        let submenus = super::favorites::Favorites::submenus(cx);
        own.push(if submenus.is_empty() {
            Entry::command(Command::AddFavoriteCurrent as u32, t!("Adicionar pasta atual"))
                .with_icon("icons/plus.svg")
        } else {
            let mut entries = vec![
                Entry::command(Command::AddFavoriteCurrent as u32, t!("No início da lista")),
                Entry::Separator,
            ];
            for (index, (_, label)) in submenus.iter().take(SUBMENU_TARGETS as usize).enumerate() {
                entries.push(Entry::command(FIRST_SUBMENU_TARGET + index as u32, label));
            }
            Entry::submenu(t!("Adicionar pasta atual"), entries).with_icon("icons/plus.svg")
        });
        self.show_menu(MenuPurpose::FavoritesMenu, own, hwnd, window, cx);
    }

    /// The column header's menu, as Explorer's: fit one column or all, and
    /// which columns to show. `column` is the one right-clicked, if any.
    pub(crate) fn open_column_menu(
        &mut self,
        column: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        let own = vec![
            Entry::command(Command::FitColumn as u32, t!("Ajustar coluna ao conteúdo"))
                .with_icon("icons/columns.svg")
                .enabled(column.is_some_and(|column| self.column_visible(column))),
            Entry::command(Command::FitAllColumns as u32, t!("Ajustar todas as colunas ao conteúdo"))
                .with_icon("icons/table.svg"),
            Entry::Separator,
            Entry::check(Command::ColumnName as u32, t!("Nome"), true).enabled(false),
            Entry::check(Command::ColumnType as u32, t!("Tipo"), self.column_visible(1)),
            Entry::check(Command::ColumnSize as u32, t!("Tamanho"), self.column_visible(2)),
            Entry::check(
                Command::ColumnModified as u32,
                t!("Data de modificação"),
                self.column_visible(3),
            ),
        ];
        self.show_menu(MenuPurpose::Columns(column), own, hwnd, window, cx);
    }

    /// The bottom bar's filter menu: one entry per kind, the current one
    /// ticked; picking one shows only that kind.
    pub(crate) fn open_filter_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use super::filter::KindFilter;
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        let current = self.kind_filter;
        let mut own: Vec<Entry> = Vec::new();
        for (index, kind) in KindFilter::ALL.into_iter().enumerate() {
            let icon = match kind {
                KindFilter::All => "icons/layout-dashboard.svg",
                KindFilter::Folders => "icons/folder.svg",
                KindFilter::Files => "icons/file.svg",
                KindFilter::Images => "icons/image.svg",
                KindFilter::Videos => "icons/film.svg",
                KindFilter::Audio => "icons/music.svg",
                KindFilter::Documents => "icons/file-text.svg",
                KindFilter::Archives => "icons/archive.svg",
                KindFilter::Programs => "icons/app-window.svg",
            };
            own.push(
                Entry::check(FIRST_KIND + index as u32, kind.menu_label(), kind == current)
                    .with_icon(icon),
            );
            // "Tudo", then folders and files, then the kinds of file.
            if matches!(kind, KindFilter::All | KindFilter::Files) {
                own.push(Entry::Separator);
            }
        }
        self.show_menu(MenuPurpose::Filter, own, hwnd, window, cx);
    }

    fn background_entries(&self, cx: &mut Context<Self>) -> Vec<Entry> {
        let theme = theme_mode();
        let sort = |command, label, key| Entry::radio(command as u32, label, self.sort_key == key);
        let group = |command, label, by| Entry::radio(command as u32, label, self.group_by == by);
        vec![
            Entry::submenu(
                t!("Exibir"),
                vec![
                    Entry::radio(
                        Command::ViewGrid as u32,
                        t!("Ícones"),
                        self.view_mode == ViewMode::Grid,
                    ),
                    Entry::radio(
                        Command::ViewCompact as u32,
                        t!("Lista"),
                        self.view_mode == ViewMode::Compact,
                    ),
                    Entry::radio(
                        Command::ViewTree as u32,
                        t!("Árvore"),
                        self.view_mode == ViewMode::Tree,
                    ),
                    Entry::radio(
                        Command::ViewList as u32,
                        t!("Detalhes"),
                        self.view_mode == ViewMode::List,
                    ),
                    Entry::Separator,
                    Entry::check(Command::ToggleHidden as u32, t!("Itens ocultos"), self.show_hidden),
                    Entry::Separator,
                    Entry::submenu(
                        t!("Tema"),
                        vec![
                            Entry::radio(
                                Command::ThemeDark as u32,
                                t!("Escuro"),
                                theme == ThemeMode::Dark,
                            ),
                            Entry::radio(
                                Command::ThemeLight as u32,
                                t!("Claro"),
                                theme == ThemeMode::Light,
                            ),
                            Entry::radio(
                                Command::ThemeSystem as u32,
                                t!("Igual ao Windows"),
                                theme == ThemeMode::System,
                            ),
                        ],
                    )
                    .with_icon("icons/palette-2.svg"),
                ],
            )
            .with_icon("icons/eye.svg"),
            Entry::submenu(
                t!("Classificar por"),
                vec![
                    sort(Command::SortName, t!("Nome"), SortKey::Name),
                    sort(Command::SortType, t!("Tipo"), SortKey::Type),
                    sort(Command::SortSize, t!("Tamanho"), SortKey::Size),
                    sort(Command::SortModified, t!("Data de modificação"), SortKey::Modified),
                    Entry::Separator,
                    Entry::radio(Command::SortAscending as u32, t!("Crescente"), self.sort_asc),
                    Entry::radio(Command::SortDescending as u32, t!("Decrescente"), !self.sort_asc),
                ],
            )
            .with_icon("icons/arrow-up-down.svg"),
            Entry::submenu(
                t!("Agrupar por"),
                vec![
                    group(Command::GroupNone, t!("Nenhum"), GroupBy::None),
                    Entry::Separator,
                    group(Command::GroupKind, t!("Tipo"), GroupBy::Kind),
                    group(Command::GroupModified, t!("Data de modificação"), GroupBy::Modified),
                    group(Command::GroupSize, t!("Tamanho"), GroupBy::Size),
                ],
            )
            .with_icon("icons/layers.svg"),
            Entry::command(Command::Refresh as u32, t!("Atualizar"))
                .with_icon("icons/refresh-cw.svg")
                .with_shortcut(keys_of(Action::Refresh)),
            Entry::Separator,
            Entry::command(Command::Paste as u32, t!("Colar"))
                .with_icon("icons/clipboard.svg")
                .with_shortcut(keys_of(Action::Paste))
                .enabled(self.can_paste(cx)),
            Entry::command(Command::NewFolder as u32, t!("Nova pasta"))
                .with_icon("icons/folder-plus.svg")
                .with_shortcut(keys_of(Action::NewFolder)),
            Entry::Separator,
            Entry::submenu(t!("Abrir esta pasta no"), {
                let mut programs = vec![
                    Entry::command(Command::OpenCommandPrompt as u32, t!("Prompt de Comando"))
                        .with_icon("icons/square-terminal.svg")
                        .with_shortcut(keys_of(Action::OpenCommandPrompt)),
                    Entry::command(Command::OpenPowerShell as u32, "PowerShell")
                        .with_icon("icons/square-terminal.svg")
                        .with_shortcut(keys_of(Action::OpenPowerShell)),
                ];
                if crate::explorer::commands::has_windows_terminal() {
                    programs.push(
                        Entry::command(Command::OpenWindowsTerminal as u32, "Windows Terminal")
                            .with_icon("icons/app-terminal.svg"),
                    );
                }
                programs.push(
                    Entry::command(Command::OpenFileExplorer as u32, t!("Explorador do Windows"))
                        .with_icon("icons/folder-open.svg")
                        .with_shortcut(keys_of(Action::OpenFileExplorer)),
                );
                programs
            })
            .with_icon("icons/external-link.svg"),
        ]
    }

    fn apply_menu_choice(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        match choice {
            Choice::Dismissed => {}
            Choice::Own(id) => {
                if let Some(command) = Command::from_id(id) {
                    self.run_command(command, window, cx);
                } else if (FIRST_TAG..FIRST_TAG + TAG_IDS).contains(&id) {
                    let tag = id - FIRST_TAG;
                    let paths = self.selected_paths();
                    super::tags::Tags::toggle(&paths, tag, cx);
                }
            }
            Choice::Verb(verb) => match verb.as_str() {
                "copy" => self.copy_selection(cx),
                "cut" => self.cut_selection(cx),
                "delete" => self.trash_selection(cx),
                "rename" => {
                    if let Some(row) = self.active_row() {
                        self.begin_rename(row, window, cx);
                    }
                }
                "open" | "explore" => {
                    let folder = self
                        .selected_paths()
                        .into_iter()
                        .next()
                        .and_then(|path| {
                            self.filtered_entries.iter().find(|entry| entry.path == path).cloned()
                        });
                    if let Some(folder) = folder {
                        self.activate_entry(folder, window, cx);
                    }
                }
                _ => {}
            },
            // The Shell may have created, moved or deleted something ("New",
            // "Send to", an archiver); the listing is read again either way.
            Choice::Ran => self.reload(cx),
        }
        cx.notify();
    }

    fn run_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::ViewList => self.set_view_mode(ViewMode::List, cx),
            Command::ViewGrid => self.set_view_mode(ViewMode::Grid, cx),
            Command::ViewTree => self.set_view_mode(ViewMode::Tree, cx),
            Command::ViewCompact => self.set_view_mode(ViewMode::Compact, cx),
            // Written to the config like the theme, so it lasts; the reload
            // that follows is what applies it.
            Command::ToggleHidden => {
                let value = if self.show_hidden { "false" } else { "true" };
                self.write_config("ui", "show_hidden", value.to_string(), cx);
            }
            Command::ThemeDark => self.write_config("theme", "mode", "\"dark\"".into(), cx),
            Command::ThemeLight => self.write_config("theme", "mode", "\"light\"".into(), cx),
            Command::ThemeSystem => self.write_config("theme", "mode", "\"system\"".into(), cx),
            Command::SortName => self.set_sort(SortKey::Name, self.sort_asc, cx),
            Command::SortType => self.set_sort(SortKey::Type, self.sort_asc, cx),
            Command::SortSize => self.set_sort(SortKey::Size, self.sort_asc, cx),
            Command::SortModified => self.set_sort(SortKey::Modified, self.sort_asc, cx),
            Command::SortAscending => self.set_sort(self.sort_key, true, cx),
            Command::SortDescending => self.set_sort(self.sort_key, false, cx),
            Command::Refresh => self.reload(cx),
            Command::Paste => self.paste_into_cwd(window, cx),
            Command::NewFolder => self.create_new_folder(window, cx),
            Command::GroupNone => self.set_group_by(GroupBy::None),
            Command::GroupKind => self.set_group_by(GroupBy::Kind),
            Command::GroupModified => self.set_group_by(GroupBy::Modified),
            Command::GroupSize => self.set_group_by(GroupBy::Size),
            Command::Cut => self.cut_selection(cx),
            Command::Copy => self.copy_selection(cx),
            Command::Delete => self.trash_selection(cx),
            Command::Rename => {
                if let Some(row) = self.active_row() {
                    self.begin_rename(row, window, cx);
                }
            }
            Command::NewTag => {
                // Made, put on the selection, and named in the sidebar.
                let paths = self.selected_paths();
                let name = t!("Novo espaço de trabalho").to_string();
                let id = super::tags::Tags::create(name.clone(), &paths, cx);
                self.sidebar_visible = true;
                self.begin_sidebar_input(super::places::SidebarInputPurpose::RenameTag(id), name, window, cx);
            }
            Command::FitColumn
            | Command::FitAllColumns
            | Command::ColumnName
            | Command::ColumnType
            | Command::ColumnSize
            | Command::ColumnModified
            | Command::RenameTag
            | Command::DeleteTag
            | Command::RemoveNetworkPlace
            | Command::RemoveFavorite
            | Command::RenameFavorite
            | Command::AddFavoriteCurrent
            | Command::OpenWorkspaceCard
            | Command::RemoveWorkspaceCard
            | Command::MoreOptions => {}
            Command::OpenCommandPrompt => self.open_in(OpenIn::CommandPrompt, cx),
            Command::OpenPowerShell => self.open_in(OpenIn::PowerShell, cx),
            Command::OpenWindowsTerminal => self.open_in(OpenIn::WindowsTerminal, cx),
            Command::OpenFileExplorer => self.open_in(OpenIn::FileExplorer, cx),
        }
    }

    fn write_config(&mut self, section: &'static str, key: &'static str, value: String, cx: &mut Context<Self>) {
        let path = config::paths::config_file();
        let write = cx.background_spawn(async move { config::set_value(&path, section, key, &value) });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            if let Err(error) = write.await {
                this.update(cx, |pane, cx| {
                    pane.set_status(
                        StatusLevel::Error,
                        tf!("Não foi possível salvar {section}.{key}: {error}", section = section, key = key, error = error),
                    );
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }
}

// "Copiar (Ctrl+C)", with whatever keys the command has now.
fn labelled(label: &str, action: Action) -> &'static str {
    interned(nohrs_core::shortcuts::with_keys(label, action))
}

fn quick_actions() -> Vec<QuickAction> {
    vec![
        QuickAction { id: Command::Cut as u32, icon: "icons/scissors.svg", label: labelled(t!("Recortar"), Action::Cut) },
        QuickAction { id: Command::Copy as u32, icon: "icons/copy.svg", label: labelled(t!("Copiar"), Action::Copy) },
        QuickAction { id: Command::Rename as u32, icon: "icons/pencil.svg", label: labelled(t!("Renomear"), Action::Rename) },
        QuickAction { id: Command::Delete as u32, icon: "icons/trash-2.svg", label: labelled(t!("Excluir"), Action::Trash) },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_ids_round_trip_and_stay_below_the_shell_range() {
        for command in Command::ALL {
            assert_eq!(Command::from_id(command as u32), Some(command));
            assert!((command as u32) < 0x1000);
        }
        assert_eq!(Command::from_id(0), None);
    }
}
