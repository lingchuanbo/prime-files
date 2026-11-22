//! The right-click menu's contents: the Windows context menu, with Nohrs's own
//! entries added to it.
//!
//! The Shell's menu is the only one that carries what other programs register
//! (Open with, Git, VS Code, 7-Zip, New, Properties), so Nohrs asks the Shell
//! for it — and then, rather than letting Windows draw it, reads it into a
//! [`MenuItem`] tree the explorer draws in its own style ([`prepare`]), and
//! hands the chosen entry back to the Shell to run ([`run`]). A few Shell
//! verbs come back to the explorer instead, so Copy, Cut, Delete and Rename
//! behave exactly like their shortcuts. [`show`] still pops the classic menu,
//! for "Show more options": an extension that draws its own entries leaves
//! nothing to read.

use std::sync::Arc;

use gpui::RenderImage;

/// What the menu is for.
#[derive(Clone)]
pub enum Target {
    /// The selected entries. Only those in the same folder as the first are
    /// included: the Shell builds one menu per folder.
    Items(Vec<String>),
    /// The empty space of a folder's listing.
    Background(String),
    /// Nothing of the Shell's: a menu of Nohrs's own entries only.
    None,
}

/// One of Nohrs's own menu entries.
pub enum Entry {
    Command {
        id: u32,
        label: String,
        checked: bool,
        /// Drawn with a dot instead of a tick, for one-of-many choices.
        radio: bool,
        enabled: bool,
        /// An embedded icon's asset path.
        icon: Option<&'static str>,
        /// The shortcut shown beside the label.
        shortcut: Option<&'static str>,
    },
    Submenu {
        label: String,
        entries: Vec<Entry>,
        icon: Option<&'static str>,
    },
    Separator,
}

impl Entry {
    pub fn command(id: u32, label: &str) -> Self {
        Self::Command {
            id,
            label: label.to_string(),
            checked: false,
            radio: false,
            enabled: true,
            icon: None,
            shortcut: None,
        }
    }

    pub fn radio(id: u32, label: &str, checked: bool) -> Self {
        Self::Command {
            id,
            label: label.to_string(),
            checked,
            radio: true,
            enabled: true,
            icon: None,
            shortcut: None,
        }
    }

    pub fn check(id: u32, label: &str, checked: bool) -> Self {
        Self::Command {
            id,
            label: label.to_string(),
            checked,
            radio: false,
            enabled: true,
            icon: None,
            shortcut: None,
        }
    }

    pub fn enabled(mut self, value: bool) -> Self {
        if let Self::Command { enabled, .. } = &mut self {
            *enabled = value;
        }
        self
    }

    pub fn submenu(label: &str, entries: Vec<Entry>) -> Self {
        Self::Submenu {
            label: label.to_string(),
            entries,
            icon: None,
        }
    }

    /// Gives the entry an icon (an embedded asset's path).
    pub fn with_icon(mut self, path: &'static str) -> Self {
        match &mut self {
            Self::Command { icon, .. } | Self::Submenu { icon, .. } => *icon = Some(path),
            Self::Separator => {}
        }
        self
    }

    /// Shows a shortcut beside the label.
    pub fn with_shortcut(mut self, keys: &'static str) -> Self {
        if let Self::Command { shortcut, .. } = &mut self {
            *shortcut = Some(keys);
        }
        self
    }
}

/// An entry's picture in the drawn menu.
#[derive(Clone)]
pub enum MenuIcon {
    None,
    /// An embedded icon, by asset path.
    Asset(&'static str),
    /// The bitmap an extension gave its entry.
    Image(Arc<RenderImage>),
}

/// One entry of the menu as the explorer draws it.
#[derive(Clone)]
pub struct MenuItem {
    /// What [`run`] is given back. 0 for separators and submenus.
    pub id: u32,
    pub label: String,
    pub shortcut: Option<String>,
    pub icon: MenuIcon,
    pub enabled: bool,
    pub checked: bool,
    pub radio: bool,
    /// The default command (what a double click does), drawn in bold.
    pub bold: bool,
    pub kind: ItemKind,
}

#[derive(Clone)]
pub enum ItemKind {
    Command,
    Separator,
    Submenu(Vec<MenuItem>),
}

impl MenuItem {
    fn separator() -> Self {
        Self {
            id: 0,
            label: String::new(),
            shortcut: None,
            icon: MenuIcon::None,
            enabled: true,
            checked: false,
            radio: false,
            bold: false,
            kind: ItemKind::Separator,
        }
    }

    fn is_separator(&self) -> bool {
        matches!(self.kind, ItemKind::Separator)
    }
}

// Nohrs's entries as drawn items.
fn own_items(entries: &[Entry]) -> Vec<MenuItem> {
    entries
        .iter()
        .map(|entry| match entry {
            Entry::Separator => MenuItem::separator(),
            Entry::Command {
                id,
                label,
                checked,
                radio,
                enabled,
                icon,
                shortcut,
            } => MenuItem {
                id: *id,
                label: label.clone(),
                shortcut: shortcut.map(str::to_string),
                icon: icon.map_or(MenuIcon::None, MenuIcon::Asset),
                enabled: *enabled,
                checked: *checked,
                radio: *radio,
                bold: false,
                kind: ItemKind::Command,
            },
            Entry::Submenu {
                label,
                entries,
                icon,
            } => MenuItem {
                id: 0,
                label: label.clone(),
                shortcut: None,
                icon: icon.map_or(MenuIcon::None, MenuIcon::Asset),
                enabled: true,
                checked: false,
                radio: false,
                bold: false,
                kind: ItemKind::Submenu(own_items(entries)),
            },
        })
        .collect()
}

// No separator first, last, or twice in a row, at any depth: joining two
// menus and hiding verbs leaves them.
fn tidy(items: Vec<MenuItem>) -> Vec<MenuItem> {
    let mut out: Vec<MenuItem> = Vec::with_capacity(items.len());
    for mut item in items {
        if let ItemKind::Submenu(children) = item.kind {
            item.kind = ItemKind::Submenu(tidy(children));
        }
        if item.is_separator() && out.last().is_none_or(MenuItem::is_separator) {
            continue;
        }
        out.push(item);
    }
    while out.last().is_some_and(MenuItem::is_separator) {
        out.pop();
    }
    out
}

/// A menu read and ready to draw, and what runs its entries.
pub struct Prepared {
    pub items: Vec<MenuItem>,
    pub session: Session,
}

/// The Shell's side of a prepared menu. Holds the Shell's menu object until
/// an entry is run or the menu closes; it lives on the UI thread, as the
/// Shell's objects must.
pub struct Session {
    shell: Option<platform::ShellSession>,
    intercept: Vec<String>,
}

/// Reads the menu `request` describes, without showing anything.
pub fn prepare(request: &Request) -> Prepared {
    let (shell_items, shell) = match request.target {
        Target::None => (Vec::new(), None),
        _ => match platform::prepare(request) {
            Some((items, session)) => (items, Some(session)),
            None => (Vec::new(), None),
        },
    };
    let own = own_items(request.own);
    let mut items = Vec::new();
    if request.own_first {
        items.extend(own);
        items.push(MenuItem::separator());
        items.extend(shell_items);
    } else {
        items.extend(shell_items);
        items.push(MenuItem::separator());
        items.extend(own);
    }
    Prepared {
        items: tidy(items),
        session: Session {
            shell,
            intercept: request
                .intercept
                .iter()
                .map(|verb| verb.to_string())
                .collect(),
        },
    }
}

/// Runs the entry `id` of a prepared menu. May open the Shell's own dialogs
/// (Properties, a delete confirmation), so like [`show`] it must run between
/// updates, not inside one.
pub fn run(session: Session, id: u32) -> Choice {
    if id < platform::FIRST_SHELL_ID {
        return Choice::Own(id);
    }
    match session.shell {
        Some(shell) => platform::run(shell, id, &session.intercept),
        None => Choice::Dismissed,
    }
}

/// How the menu was closed.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    Dismissed,
    /// One of Nohrs's entries, by id.
    Own(u32),
    /// A Shell verb listed in `intercept`, not run: the caller does it.
    Verb(String),
    /// A Shell command, already run.
    Ran,
}

/// What to show and how.
pub struct Request<'a> {
    pub hwnd: isize,
    pub target: &'a Target,
    pub own: &'a [Entry],
    /// Nohrs's entries above the Shell's (the folder background, where they
    /// are the point of the menu) or below them (files, where Open comes
    /// first).
    pub own_first: bool,
    /// Shift was held: the Shell adds its extended verbs, as in Explorer.
    pub extended: bool,
    pub dark: bool,
    /// Shell verbs handed back as [`Choice::Verb`] instead of run.
    pub intercept: &'a [&'a str],
    /// Shell verbs left out, because one of Nohrs's entries replaces them.
    pub hide: &'a [&'a str],
}

/// Shows the menu at the pointer and waits for it to close.
///
/// Runs a modal loop, so it must not be called while the app or the window
/// is borrowed: call it from a spawned task, between updates.
pub fn show(request: &Request) -> Choice {
    platform::show(request)
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! None of the Shell's menu interfaces has a safe binding.

    use std::cell::RefCell;
    use std::sync::OnceLock;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::HBITMAP;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        CMF_CANRENAME, CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_PTINVOKE,
        CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, DefSubclassProc, GCS_VERBW, IContextMenu,
        IContextMenu3, ILFindLastID, IShellFolder, RemoveWindowSubclass, SHBindToParent,
        SHGetDesktopFolder, SHParseDisplayName, SetWindowSubclass,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreatePopupMenu, DeleteMenu, DestroyMenu, GetCursorPos, GetMenuItemCount, GetMenuItemID,
        GetMenuItemInfoW, HMENU, InsertMenuItemW, MENUITEMINFOW, MF_BYPOSITION, MFS_CHECKED,
        MFS_DEFAULT, MFS_DISABLED, MFT_RADIOCHECK, MFT_SEPARATOR, MFT_STRING, MIIM_BITMAP,
        MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, SW_SHOWNORMAL, TPM_RETURNCMD,
        TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM,
        WM_MENUCHAR,
    };
    use windows::core::{Interface, PCSTR, PCWSTR, PSTR};

    use super::{Choice, Entry, ItemKind, MenuIcon, MenuItem, Request, Target};

    // Nohrs's ids sit below this, the Shell's from here up: the two share one
    // popup, and the returned id says whose it was.
    pub const FIRST_SHELL_ID: u32 = 0x1000;
    const LAST_SHELL_ID: u32 = 0x7FFF;
    const SUBCLASS_ID: usize = 0x4E4F_4852;
    /// `CMIC_MASK_UNICODE` (shobjidl.h), missing from the bindings: the
    /// command info carries the wide strings.
    const CMIC_MASK_UNICODE: u32 = 0x4000;

    thread_local! {
        // The menu whose owner-drawn submenus ("Open with", "Send to", "New")
        // are being filled. Those arrive as window messages to the owner while
        // the popup is open, and the Shell has to answer them.
        static OPEN_MENU: RefCell<Option<IContextMenu3>> = const { RefCell::new(None) };
    }

    pub fn show(request: &Request) -> Choice {
        unsafe { show_menu(request) }.unwrap_or_else(|error| {
            tracing::warn!("context menu failed: {error}");
            Choice::Dismissed
        })
    }

    unsafe fn show_menu(request: &Request) -> windows::core::Result<Choice> {
        // Apartment-threaded COM is what Shell extensions expect. Already
        // initialized is the ordinary case; any other failure would surface
        // below, at the first Shell call.
        if let Err(error) = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok() {
            tracing::debug!("CoInitializeEx: {error}");
        }
        let hwnd = HWND(request.hwnd as *mut _);
        set_menu_theme(request.dark);

        let shell = match shell_menu(hwnd, request.target) {
            _ if matches!(request.target, Target::None) => None,
            Ok(shell) => Some(shell),
            Err(error) => {
                // Nohrs's own entries still work without the Shell's.
                tracing::warn!("no Shell menu for this selection: {error}");
                None
            }
        };

        let popup = unsafe { CreatePopupMenu() }?;
        let menu = Menu(popup);
        if request.own_first {
            append_entries(popup, request.own)?;
        }
        if let Some(shell) = &shell {
            if request.own_first && !request.own.is_empty() {
                append_separator(popup)?;
            }
            let mut flags = CMF_NORMAL | CMF_EXPLORE | CMF_CANRENAME;
            if request.extended {
                flags |= CMF_EXTENDEDVERBS;
            }
            let position = unsafe { GetMenuItemCount(Some(popup)) }.max(0) as u32;
            unsafe {
                shell.QueryContextMenu(popup, position, FIRST_SHELL_ID, LAST_SHELL_ID, flags)
            }
            .ok()?;
            remove_verbs(popup, shell, request.hide);
        }
        if !request.own_first && !request.own.is_empty() {
            append_separator(popup)?;
            append_entries(popup, request.own)?;
        }
        tidy_separators(popup);

        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point) }?;

        let forwarding = shell
            .as_ref()
            .and_then(|shell| shell.cast::<IContextMenu3>().ok());
        let subclassed = forwarding.is_some()
            && unsafe { SetWindowSubclass(hwnd, Some(forward_menu_messages), SUBCLASS_ID, 0) }
                .as_bool();
        OPEN_MENU.with(|open| *open.borrow_mut() = forwarding);
        let id = unsafe {
            TrackPopupMenuEx(
                popup,
                (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
                point.x,
                point.y,
                hwnd,
                None,
            )
        }
        .0 as u32;
        OPEN_MENU.with(|open| *open.borrow_mut() = None);
        if subclassed
            && !unsafe { RemoveWindowSubclass(hwnd, Some(forward_menu_messages), SUBCLASS_ID) }
                .as_bool()
        {
            tracing::warn!("could not remove the context menu's window subclass");
        }

        let choice = match (id, &shell) {
            (0, _) => Choice::Dismissed,
            (id, _) if id < FIRST_SHELL_ID => Choice::Own(id),
            (id, Some(shell)) => {
                let offset = id - FIRST_SHELL_ID;
                match verb(shell, offset) {
                    Some(verb) if request.intercept.contains(&verb.as_str()) => Choice::Verb(verb),
                    _ => {
                        invoke(shell, hwnd, offset, &directory_of(request.target), point)?;
                        Choice::Ran
                    }
                }
            }
            (_, None) => Choice::Dismissed,
        };
        drop(menu);
        Ok(choice)
    }

    /// The Shell's menu object and the menu it filled, kept until an entry
    /// runs.
    pub struct ShellSession {
        shell: IContextMenu,
        _menu: Menu,
        hwnd: HWND,
        directory: String,
    }

    pub fn prepare(request: &Request) -> Option<(Vec<MenuItem>, ShellSession)> {
        timed_menu_stage("prepare_total", || prepare_inner(request))
    }

    fn timed_menu_stage<T>(stage: &'static str, operation: impl FnOnce() -> T) -> T {
        let started = std::time::Instant::now();
        tracing::info!(stage, "context_menu stage begin");
        let result = operation();
        tracing::info!(
            stage,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "context_menu stage end"
        );
        result
    }

    fn prepare_inner(request: &Request) -> Option<(Vec<MenuItem>, ShellSession)> {
        let (target_kind, item_count) = match request.target {
            Target::Items(paths) => ("items", paths.len()),
            Target::Background(_) => ("background", 0),
            Target::None => ("none", 0),
        };
        tracing::info!(
            target_kind,
            item_count,
            extended = request.extended,
            "context_menu request"
        );
        if let Err(error) = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok() {
            tracing::debug!("CoInitializeEx: {error}");
        }
        let hwnd = HWND(request.hwnd as *mut _);
        let shell = match timed_menu_stage("get_shell_menu", || shell_menu(hwnd, request.target)) {
            Ok(shell) => shell,
            Err(error) => {
                tracing::warn!("no Shell menu for this selection: {error}");
                return None;
            }
        };
        let popup = unsafe { CreatePopupMenu() }.ok()?;
        let menu = Menu(popup);
        let mut flags = CMF_NORMAL | CMF_EXPLORE | CMF_CANRENAME;
        if request.extended {
            flags |= CMF_EXTENDEDVERBS;
        }
        if let Err(error) = timed_menu_stage("query_context_menu", || {
            unsafe { shell.QueryContextMenu(popup, 0, FIRST_SHELL_ID, LAST_SHELL_ID, flags) }.ok()
        }) {
            tracing::warn!("QueryContextMenu: {error}");
            return None;
        }
        timed_menu_stage("remove_verbs", || remove_verbs(popup, &shell, request.hide));
        let filler = shell.cast::<IContextMenu3>().ok();
        let items = timed_menu_stage("read_menu_tree", || read_menu(popup, filler.as_ref(), 0));
        Some((
            items,
            ShellSession {
                shell,
                _menu: menu,
                hwnd,
                directory: directory_of(request.target),
            },
        ))
    }

    pub fn run(session: ShellSession, id: u32, intercept: &[String]) -> Choice {
        let offset = id - FIRST_SHELL_ID;
        if let Some(verb) = verb(&session.shell, offset)
            && intercept.contains(&verb)
        {
            return Choice::Verb(verb);
        }
        let mut point = POINT::default();
        if let Err(error) = unsafe { GetCursorPos(&mut point) } {
            tracing::debug!("GetCursorPos: {error}");
        }
        match invoke(
            &session.shell,
            session.hwnd,
            offset,
            &session.directory,
            point,
        ) {
            Ok(()) => Choice::Ran,
            Err(error) => {
                tracing::warn!("could not run the menu command: {error}");
                Choice::Dismissed
            }
        }
    }

    // Deep menus are rare; this only guards against an extension whose
    // submenu contains itself.
    const MAX_DEPTH: usize = 6;

    // Reads `menu` into items. Submenus are filled first, as Windows fills
    // them when one opens: "Send to", "New" and "Open with" are empty until
    // the Shell is told the submenu is about to show.
    fn read_menu(menu: HMENU, filler: Option<&IContextMenu3>, depth: usize) -> Vec<MenuItem> {
        let count = unsafe { GetMenuItemCount(Some(menu)) }.max(0);
        let mut items = Vec::new();
        for position in 0..count {
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_STRING | MIIM_BITMAP,
                ..Default::default()
            };
            if unsafe { GetMenuItemInfoW(menu, position as u32, true, &mut info) }.is_err() {
                continue;
            }
            if info.fType.contains(MFT_SEPARATOR) {
                items.push(MenuItem::separator());
                continue;
            }
            let mut text = vec![0u16; info.cch as usize + 1];
            info.fMask = MIIM_STRING;
            info.dwTypeData = windows::core::PWSTR(text.as_mut_ptr());
            info.cch = text.len() as u32;
            let raw = if unsafe { GetMenuItemInfoW(menu, position as u32, true, &mut info) }.is_ok()
            {
                String::from_utf16_lossy(&text[..info.cch as usize])
            } else {
                String::new()
            };
            // Drawn by its extension, with no text to read: shown in the
            // classic menu, under "Show more options".
            if raw.trim().is_empty() {
                continue;
            }
            let (label, shortcut) = match raw.split_once('\t') {
                Some((label, shortcut)) => (label.to_string(), Some(shortcut.trim().to_string())),
                None => (raw, None),
            };
            let label = strip_accelerator(&label);
            let mut full = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_BITMAP,
                ..Default::default()
            };
            if unsafe { GetMenuItemInfoW(menu, position as u32, true, &mut full) }.is_err() {
                continue;
            }
            let kind = if !full.hSubMenu.is_invalid() && depth < MAX_DEPTH {
                if let Some(filler) = filler {
                    let wparam = WPARAM(full.hSubMenu.0 as usize);
                    let lparam = LPARAM(position as isize);
                    tracing::info!(depth, position, menu_label = %label, "context_menu submenu");
                    if let Err(error) = timed_menu_stage("init_submenu", || unsafe {
                        filler.HandleMenuMsg2(WM_INITMENUPOPUP, wparam, lparam, None)
                    }) {
                        tracing::debug!("filling a submenu: {error}");
                    }
                }
                ItemKind::Submenu(read_menu(full.hSubMenu, filler, depth + 1))
            } else {
                ItemKind::Command
            };
            items.push(MenuItem {
                id: full.wID,
                label,
                shortcut,
                icon: timed_menu_stage("read_menu_bitmap", || menu_bitmap(full.hbmpItem)),
                enabled: !full.fState.contains(MFS_DISABLED),
                checked: full.fState.contains(MFS_CHECKED),
                radio: full.fType.contains(MFT_RADIOCHECK),
                bold: full.fState.contains(MFS_DEFAULT),
                kind,
            });
        }
        items
    }

    // "&Abrir" shows as "Abrir"; "&&" is a literal ampersand.
    fn strip_accelerator(label: &str) -> String {
        let mut out = String::with_capacity(label.len());
        let mut characters = label.chars().peekable();
        while let Some(character) = characters.next() {
            if character == '&' {
                if characters.peek() == Some(&'&') {
                    out.push('&');
                    characters.next();
                }
                continue;
            }
            out.push(character);
        }
        out
    }

    // An entry's own bitmap. The small values (1 to 11, and -1) are not
    // bitmaps but requests to draw a system glyph or call back the owner.
    fn menu_bitmap(bitmap: HBITMAP) -> MenuIcon {
        let raw = bitmap.0 as isize;
        if raw == -1 || (0..=11).contains(&raw) {
            return MenuIcon::None;
        }
        crate::explorer::thumbnails::read_bitmap(bitmap).map_or(MenuIcon::None, |image| {
            MenuIcon::Image(std::sync::Arc::new(image))
        })
    }

    fn directory_of(target: &Target) -> String {
        match target {
            Target::Items(paths) => paths
                .first()
                .and_then(|path| std::path::Path::new(path).parent())
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Target::Background(folder) => folder.clone(),
            Target::None => String::new(),
        }
    }

    struct Menu(HMENU);

    impl Drop for Menu {
        fn drop(&mut self) {
            if let Err(error) = unsafe { DestroyMenu(self.0) } {
                tracing::debug!("DestroyMenu: {error}");
            }
        }
    }

    /// A Shell item id list from `SHParseDisplayName`, freed on drop.
    struct Pidl(*mut ITEMIDLIST);

    impl Pidl {
        fn parse(path: &str) -> windows::core::Result<Self> {
            let wide = wide(path);
            let mut pidl = std::ptr::null_mut();
            timed_menu_stage("parse_shell_path", || unsafe {
                SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None)
            })?;
            Ok(Self(pidl))
        }
    }

    impl Drop for Pidl {
        fn drop(&mut self) {
            unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
        }
    }

    fn shell_menu(hwnd: HWND, target: &Target) -> windows::core::Result<IContextMenu> {
        match target {
            Target::Items(paths) => {
                let folder_of = |path: &str| {
                    std::path::Path::new(path)
                        .parent()
                        .map(|parent| parent.to_path_buf())
                };
                let first_folder = paths.first().and_then(|path| folder_of(path));
                let pidls = paths
                    .iter()
                    .filter(|path| folder_of(path) == first_folder)
                    .map(|path| Pidl::parse(path))
                    .collect::<windows::core::Result<Vec<_>>>()?;
                let first = pidls.first().ok_or_else(windows::core::Error::empty)?;
                let parent: IShellFolder =
                    timed_menu_stage("bind_parent", || unsafe { SHBindToParent(first.0, None) })?;
                let children: Vec<*const ITEMIDLIST> = pidls
                    .iter()
                    .map(|pidl| unsafe { ILFindLastID(pidl.0) } as *const ITEMIDLIST)
                    .collect();
                timed_menu_stage("get_ui_object", || unsafe {
                    parent.GetUIObjectOf(hwnd, &children, None)
                })
            }
            Target::None => Err(windows::core::Error::empty()),
            Target::Background(folder) => {
                let pidl = Pidl::parse(folder)?;
                let desktop =
                    timed_menu_stage("get_desktop_folder", || unsafe { SHGetDesktopFolder() })?;
                let shell_folder: IShellFolder = timed_menu_stage("bind_folder", || unsafe {
                    desktop.BindToObject(pidl.0, None)
                })?;
                timed_menu_stage("create_view_object", || unsafe {
                    shell_folder.CreateViewObject(hwnd)
                })
            }
        }
    }

    fn verb(shell: &IContextMenu, offset: u32) -> Option<String> {
        let mut buffer = [0u16; 256];
        unsafe {
            shell.GetCommandString(
                offset as usize,
                GCS_VERBW,
                None,
                PSTR(buffer.as_mut_ptr().cast()),
                buffer.len() as u32,
            )
        }
        .ok()?;
        let length = buffer
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..length]).to_lowercase())
    }

    fn invoke(
        shell: &IContextMenu,
        hwnd: HWND,
        offset: u32,
        directory: &str,
        point: POINT,
    ) -> windows::core::Result<()> {
        let directory_ansi: Vec<u8> = directory.bytes().chain(std::iter::once(0)).collect();
        let directory_wide = wide(directory);
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
            hwnd,
            // The command's offset, passed where a verb string would go, as
            // MAKEINTRESOURCE does.
            lpVerb: PCSTR(offset as usize as *const u8),
            lpVerbW: PCWSTR(offset as usize as *const u16),
            lpDirectory: PCSTR(directory_ansi.as_ptr()),
            lpDirectoryW: PCWSTR(directory_wide.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ptInvoke: point,
            ..Default::default()
        };
        unsafe {
            shell.InvokeCommand(&info as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO)
        }
    }

    fn append_entries(menu: HMENU, entries: &[Entry]) -> windows::core::Result<()> {
        for entry in entries {
            match entry {
                Entry::Separator => append_separator(menu)?,
                Entry::Command {
                    id,
                    label,
                    checked,
                    radio,
                    enabled,
                    ..
                } => {
                    let mut text = wide(&label.replace('&', "&&"));
                    let mut state = Default::default();
                    if *checked {
                        state |= MFS_CHECKED;
                    }
                    if !enabled {
                        state |= MFS_DISABLED;
                    }
                    let info = MENUITEMINFOW {
                        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                        fMask: MIIM_FTYPE | MIIM_ID | MIIM_STATE | MIIM_STRING,
                        fType: if *radio {
                            MFT_STRING | MFT_RADIOCHECK
                        } else {
                            MFT_STRING
                        },
                        fState: state,
                        wID: *id,
                        dwTypeData: windows::core::PWSTR(text.as_mut_ptr()),
                        ..Default::default()
                    };
                    insert_last(menu, &info)?;
                }
                Entry::Submenu { label, entries, .. } => {
                    let submenu = unsafe { CreatePopupMenu() }?;
                    append_entries(submenu, entries)?;
                    let mut text = wide(&label.replace('&', "&&"));
                    let info = MENUITEMINFOW {
                        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                        fMask: MIIM_FTYPE | MIIM_STRING | MIIM_SUBMENU,
                        fType: MFT_STRING,
                        // Owned by the parent from here: destroying the popup
                        // destroys it.
                        hSubMenu: submenu,
                        dwTypeData: windows::core::PWSTR(text.as_mut_ptr()),
                        ..Default::default()
                    };
                    insert_last(menu, &info)?;
                }
            }
        }
        Ok(())
    }

    fn append_separator(menu: HMENU) -> windows::core::Result<()> {
        let info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE,
            fType: MFT_SEPARATOR,
            ..Default::default()
        };
        insert_last(menu, &info)
    }

    fn insert_last(menu: HMENU, info: &MENUITEMINFOW) -> windows::core::Result<()> {
        let position = unsafe { GetMenuItemCount(Some(menu)) }.max(0) as u32;
        unsafe { InsertMenuItemW(menu, position, true, info) }
    }

    fn remove_verbs(menu: HMENU, shell: &IContextMenu, hide: &[&str]) {
        if hide.is_empty() {
            return;
        }
        let count = unsafe { GetMenuItemCount(Some(menu)) };
        for position in (0..count.max(0)).rev() {
            let id = unsafe { GetMenuItemID(menu, position) };
            if !(FIRST_SHELL_ID..=LAST_SHELL_ID).contains(&id) {
                continue;
            }
            if verb(shell, id - FIRST_SHELL_ID).is_some_and(|verb| hide.contains(&verb.as_str()))
                && let Err(error) = unsafe { DeleteMenu(menu, position as u32, MF_BYPOSITION) }
            {
                tracing::debug!("DeleteMenu: {error}");
            }
        }
    }

    // Removing items and joining two menus can leave separators at the ends or
    // two in a row.
    fn tidy_separators(menu: HMENU) {
        let is_separator = |position: i32| {
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE,
                ..Default::default()
            };
            unsafe { GetMenuItemInfoW(menu, position as u32, true, &mut info) }.is_ok()
                && info.fType.contains(MFT_SEPARATOR)
        };
        let mut position = unsafe { GetMenuItemCount(Some(menu)) } - 1;
        let mut previous_was_separator = true;
        while position >= 0 {
            let separator = is_separator(position);
            if separator && previous_was_separator {
                if let Err(error) = unsafe { DeleteMenu(menu, position as u32, MF_BYPOSITION) } {
                    tracing::debug!("DeleteMenu: {error}");
                }
            } else {
                previous_was_separator = separator;
            }
            position -= 1;
        }
        if is_separator(0)
            && let Err(error) = unsafe { DeleteMenu(menu, 0, MF_BYPOSITION) }
        {
            tracing::debug!("DeleteMenu: {error}");
        }
    }

    unsafe extern "system" fn forward_menu_messages(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        if matches!(
            message,
            WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
        ) {
            let handled = OPEN_MENU.with(|open| {
                let open = open.borrow();
                let menu = open.as_ref()?;
                let mut result = LRESULT(0);
                unsafe { menu.HandleMenuMsg2(message, wparam, lparam, Some(&mut result)) }
                    .ok()
                    .map(|()| result)
            });
            if let Some(result) = handled {
                return result;
            }
        }
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }

    /// Light or dark popups, to match the app. Windows has no public switch
    /// for a process's menus; these are the uxtheme exports (by ordinal) that
    /// Explorer and most Win32 apps with a dark mode use, present since
    /// Windows 10 1903. Without them the menu is simply light.
    fn set_menu_theme(dark: bool) {
        type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
        type FlushMenuThemes = unsafe extern "system" fn();
        static EXPORTS: OnceLock<Option<(SetPreferredAppMode, FlushMenuThemes)>> = OnceLock::new();
        let exports = EXPORTS.get_or_init(|| unsafe {
            let module = LoadLibraryW(windows::core::w!("uxtheme.dll")).ok()?;
            let set_mode = GetProcAddress(module, PCSTR(135 as *const u8))?;
            let flush = GetProcAddress(module, PCSTR(136 as *const u8))?;
            Some((
                std::mem::transmute::<unsafe extern "system" fn() -> isize, SetPreferredAppMode>(
                    set_mode,
                ),
                std::mem::transmute::<unsafe extern "system" fn() -> isize, FlushMenuThemes>(flush),
            ))
        });
        if let Some((set_mode, flush)) = exports {
            // 2 = force dark, 3 = force light.
            unsafe {
                set_mode(if dark { 2 } else { 3 });
                flush();
            }
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Choice, MenuItem, Request};

    pub const FIRST_SHELL_ID: u32 = 0x1000;

    pub struct ShellSession;

    pub fn show(_request: &Request) -> Choice {
        Choice::Dismissed
    }

    pub fn prepare(_request: &Request) -> Option<(Vec<MenuItem>, ShellSession)> {
        None
    }

    pub fn run(_session: ShellSession, _id: u32, _intercept: &[String]) -> Choice {
        Choice::Dismissed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn print(items: &[MenuItem], depth: usize) {
        for item in items {
            let icon = match item.icon {
                MenuIcon::None => " ",
                MenuIcon::Asset(_) => "a",
                MenuIcon::Image(_) => "*",
            };
            match &item.kind {
                ItemKind::Separator => println!("{}----", "  ".repeat(depth)),
                ItemKind::Command => println!(
                    "{}{icon} {} [{}]{}{}",
                    "  ".repeat(depth),
                    item.label,
                    item.shortcut.clone().unwrap_or_default(),
                    if item.bold { " (default)" } else { "" },
                    if item.enabled { "" } else { " (disabled)" },
                ),
                ItemKind::Submenu(children) => {
                    println!("{}{icon} {} >", "  ".repeat(depth), item.label);
                    print(children, depth + 1);
                }
            }
        }
    }

    // Reads real menus off this machine; run by hand with `--ignored --nocapture`
    // and NOHRS_MENU_TARGET set to a file.
    #[test]
    #[ignore]
    fn print_a_real_menu() {
        let target =
            std::env::var("NOHRS_MENU_TARGET").unwrap_or_else(|_| r"C:\Windows\notepad.exe".into());
        for target in [
            Target::Items(vec![target.clone()]),
            Target::Background(r"C:\Windows".into()),
        ] {
            let prepared = prepare(&Request {
                hwnd: 0,
                target: &target,
                own: &[],
                own_first: false,
                extended: false,
                dark: true,
                intercept: &[],
                hide: &[],
            });
            print(&prepared.items, 0);
            println!("==========");
        }
    }
}
