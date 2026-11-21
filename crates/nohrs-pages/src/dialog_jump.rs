//! The jump bar under other programs' Open / Save dialogs (Pulse's
//! `dialog_jump.cpp`, ported in the part LumaFiles can do without injecting
//! into the other process).
//!
//! While a program's standard file dialog is in front, a slim bar hangs under
//! it: a search box (LumaFiles' whole-computer name index, plus the folders
//! LumaFiles knows at once), the folder it is showing, the folders open in its
//! tabs, and a Ctrl+G hint. Clicking a chip, choosing a search result or
//! pressing Ctrl+G moves the dialog there — folders through the dialog's
//! address bar (navigating it), never by pressing Open, which some programs
//! would accept as "chosen". A chosen file result is filled into the name box
//! and submitted, as Pulse does.
//!
//! Clicking the search box puts the keyboard on the bar; type, arrow through
//! the results, Enter to jump, Esc to go back to the dialog. With the box empty
//! the bar offers the folders this program's dialogs went to most (remembered
//! between runs) and the places LumaFiles lists.
//!
//! A newly opened dialog can also be moved to LumaFiles' current folder once
//! (`explorer.file_dialog_auto_jump`); after that the person's own navigation is
//! left alone.

use gpui::Window;
use nohrs_core::config::settings::BarSize;

/// One folder the bar offers: its path and the name drawn on the chip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub path: String,
    pub label: String,
}

/// One search result: a folder or file found by the index or by the bar itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    pub label: String,
    pub dir: bool,
}

/// What the bar and the auto-jump need, published by the UI thread.
#[derive(Clone, Debug)]
pub struct Context {
    /// The whole feature (the settings switch).
    pub enabled: bool,
    /// Whether the bar is drawn.
    pub bar: bool,
    /// Whether a new dialog goes to `current` once.
    pub auto_jump: bool,
    /// The active tab's folder.
    pub current: String,
    /// Folders open in LumaFiles, most relevant first.
    pub folders: Vec<Folder>,
    /// Places the sidebar lists and the active tab's subfolders, for instant
    /// searching before the index answers.
    pub places: Vec<Folder>,
    /// The Ctrl+G hint, already in the interface language.
    pub hint: String,
    /// The search box's placeholder, in the interface language.
    pub placeholder: String,
    /// How big the bar is drawn.
    pub size: BarSize,
    /// 0xRRGGBB colours, from the app theme.
    pub bg: u32,
    pub fg: u32,
    pub accent: u32,
}

impl Context {
    const fn empty() -> Self {
        Self {
            enabled: false,
            bar: false,
            auto_jump: false,
            current: String::new(),
            folders: Vec::new(),
            places: Vec::new(),
            hint: String::new(),
            placeholder: String::new(),
            size: BarSize::Auto,
            bg: 0x1f2937,
            fg: 0xf9fafb,
            accent: 0xeab308,
        }
    }
}

/// Publishes what the bar shows. Cheap enough for the publisher loop.
pub fn publish(context: Context) {
    platform::publish(context);
}

/// Brings the bar and its hotkey down (app exit, or the feature disabled).
pub fn stop() {
    platform::stop();
}

/// Starts nothing by itself: the first [`publish`] starts the watcher.
pub fn start() {
    platform::ensure_started();
}

/// The waiting search: the query request id and the text to look for. The UI
/// thread answers with [`set_results`].
pub fn take_query() -> Option<(u64, String)> {
    platform::take_query()
}

/// Delivers the index answers for query `id`; a stale id is ignored.
pub fn set_results(id: u64, results: Vec<Found>) {
    platform::set_results(id, results);
}

/// No-op on other platforms.
pub fn _silence(_window: &Window) {}

#[cfg(not(windows))]
mod platform {
    use super::{BarSize, Context, Folder, Found};

    pub fn publish(_context: Context) {}
    pub fn stop() {}
    pub fn ensure_started() {}
    pub fn take_query() -> Option<(u64, String)> {
        None
    }
    pub fn set_results(_id: u64, _results: Vec<Found>) {}
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! The watcher thread, the bar window, the dialog navigation, the search
    //! box and the per-program jump history. `unsafe` is confined here: none of
    //! this has a safe binding.

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    use serde::{Deserialize, Serialize};
    use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateRoundRectRgn,
        CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CALCRECT, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX,
        DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FF_DONTCARE,
        FW_NORMAL, FillRect, GetStockObject, HDC, InvalidateRect, NULL_PEN, OUT_DEFAULT_PRECIS,
        PAINTSTRUCT, RoundRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn, TRANSPARENT,
    };
    use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, INPUT, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
        KEYEVENTF_UNICODE, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, SendInput, SetFocus,
        UnregisterHotKey, VK_BACK, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_MENU, VK_RETURN, VK_UP,
        keybd_event,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BN_CLICKED, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
        DestroyWindow, DispatchMessageW, GetClassNameW, GetClientRect, GetDlgItem,
        GetForegroundWindow, GetPropW, GetSystemMetrics, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
        HWND_TOPMOST, IDOK, IsWindow, MSG, PM_REMOVE, PeekMessageW, PostMessageW, RegisterClassW,
        RemovePropW, SM_CYSCREEN, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
        SetForegroundWindow, SetPropW, SetWindowPos, SetWindowTextW, ShowWindow,
        SwitchToThisWindow, TranslateMessage, WM_CHAR, WM_COMMAND, WM_ERASEBKGND, WM_HOTKEY,
        WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_QUIT, WM_SETFOCUS,
        WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    use super::{BarSize, Context, Folder, Found};

    /// Control id of the file name combo box in the common Open/Save dialog.
    const FILE_NAME_COMBO: i32 = 1148;
    const FILE_NAME_EDIT: i32 = 1152;
    const HOTKEY_G: i32 = 0x4C55;
    const AUTO_PROPERTY: PCWSTR = w!("LumaFiles.DialogJump.AutoVisited.v1");
    const IGNORED_APPS: &[&str] = &["explorer.exe", "searchapp.exe", "lumafiles.exe"];
    const POLL_MS: u64 = 120;
    const MAX_RESULTS: usize = 8;
    const HISTORY_FILE: &str = "dialog_jumps.json";
    const HISTORY_PER_APP: usize = 30;

    // --- jump history ---------------------------------------------------------

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct HistoryEntry {
        path: String,
        count: u32,
        last: u64,
    }

    #[derive(Default, Serialize, Deserialize)]
    struct HistoryFile {
        apps: HashMap<String, Vec<HistoryEntry>>,
    }

    /// The remembered jumps, keyed by the dialog's program.
    static HISTORY: Mutex<Option<HistoryFile>> = Mutex::new(None);

    fn history_path() -> Option<std::path::PathBuf> {
        let dir = nohrs_core::config::paths::state_dir();
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.join(HISTORY_FILE))
    }

    #[allow(clippy::disallowed_methods)]
    fn history_edit<F: FnOnce(&mut HistoryFile)>(edit: F) {
        let mut slot = HISTORY.lock().unwrap_or_else(|error| error.into_inner());
        if slot.is_none() {
            let loaded = history_path()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|text| serde_json::from_str::<HistoryFile>(&text).ok())
                .unwrap_or_default();
            *slot = Some(loaded);
        }
        let Some(history) = slot.as_mut() else {
            return;
        };
        edit(history);
        if let Some(path) = history_path()
            && let Ok(text) = serde_json::to_string(history)
            && let Err(error) = std::fs::write(&path, text)
        {
            tracing::debug!("dialog jump history not saved: {error}");
        }
    }

    fn record_jump(app: &str, path: &str) {
        if app.is_empty() || path.is_empty() {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        history_edit(|history| {
            let entries = history.apps.entry(app.to_owned()).or_default();
            if let Some(entry) = entries.iter_mut().find(|entry| entry.path == path) {
                entry.count = entry.count.saturating_add(1);
                entry.last = now;
            } else {
                entries.push(HistoryEntry {
                    path: path.to_owned(),
                    count: 1,
                    last: now,
                });
            }
            entries.sort_by(|a, b| b.count.cmp(&a.count).then(b.last.cmp(&a.last)));
            entries.truncate(HISTORY_PER_APP);
        });
    }

    /// The folders this program's dialogs went to, most used first.
    #[allow(clippy::disallowed_methods)]
    fn remembered(app: &str) -> Vec<Folder> {
        if app.is_empty() {
            return Vec::new();
        }
        let mut slot = HISTORY.lock().unwrap_or_else(|error| error.into_inner());
        if slot.is_none() {
            let parsed = history_path()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|text| serde_json::from_str::<HistoryFile>(&text).ok());
            *slot = Some(parsed.unwrap_or_default());
        }
        let result: Vec<Folder> = slot
            .as_ref()
            .and_then(|history| history.apps.get(app))
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| Folder {
                        label: folder_name(&entry.path),
                        path: entry.path.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        result
    }

    // --- state ---------------------------------------------------------------

    struct Chip {
        start: i32,
        end: i32,
        path: String,
        label: String,
    }

    struct PaintState {
        chips: Vec<Chip>,
        layout: Vec<Chip>,
        search_box: (i32, i32, i32, i32),
        rows: Vec<(i32, i32)>,
        hint: String,
        placeholder: String,
        bg: u32,
        fg: u32,
        accent: u32,
        hover: isize,
        hover_row: isize,
    }

    impl PaintState {
        const fn empty() -> Self {
            Self {
                chips: Vec::new(),
                layout: Vec::new(),
                search_box: (0, 0, 0, 0),
                rows: Vec::new(),
                hint: String::new(),
                placeholder: String::new(),
                bg: 0x1f2937,
                fg: 0xf9fafb,
                accent: 0xeab308,
                hover: -1,
                hover_row: -1,
            }
        }
    }

    struct SearchState {
        query: String,
        focused: bool,
        id: u64,
        /// Answers found in the folders the bar knows at once.
        local: Vec<Found>,
        /// Answers from the whole-computer index.
        remote: Vec<Found>,
        /// What is drawn: the memory list, or local + remote merged.
        view: Vec<Found>,
        selected: isize,
        /// The remembered folders of the program whose dialog is in front.
        remembered: Vec<Folder>,
    }

    impl SearchState {
        const fn empty() -> Self {
            Self {
                query: String::new(),
                focused: false,
                id: 0,
                local: Vec::new(),
                remote: Vec::new(),
                view: Vec::new(),
                selected: -1,
                remembered: Vec::new(),
            }
        }
    }

    static CONTEXT: Mutex<Context> = Mutex::new(Context::empty());
    static PAINT: Mutex<PaintState> = Mutex::new(PaintState::empty());
    static SEARCH: Mutex<SearchState> = Mutex::new(SearchState::empty());
    static PENDING_QUERY: Mutex<Option<(u64, String)>> = Mutex::new(None);
    static NEXT_QUERY: AtomicU64 = AtomicU64::new(1);
    static STARTED: AtomicBool = AtomicBool::new(false);
    static STOPPING: AtomicBool = AtomicBool::new(false);
    static BAR: AtomicIsize = AtomicIsize::new(0);
    static LAST_DIALOG: AtomicIsize = AtomicIsize::new(0);
    static HOTKEY: AtomicBool = AtomicBool::new(false);
    static SCALE_DPI: AtomicI32 = AtomicI32::new(0);
    /// Whether the bar is above its dialog (a dialog near the screen's bottom).
    static ABOVE: AtomicBool = AtomicBool::new(false);
    /// 100 = the dialog's own scale, 85 = small, 120 = large.
    static SIZE_PERCENT: AtomicI32 = AtomicI32::new(100);
    static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
    static LAST_CTRL: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

    pub fn publish(context: Context) {
        SIZE_PERCENT.store(
            match context.size {
                BarSize::Auto => 100,
                BarSize::Small => 85,
                BarSize::Large => 120,
            },
            Ordering::SeqCst,
        );
        *CONTEXT.lock().unwrap_or_else(|error| error.into_inner()) = context;
        ensure_started();
    }

    pub fn ensure_started() {
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("luma-dialog-jump".into())
            .spawn(run);
        if let Err(error) = spawned {
            tracing::warn!("dialog jump disabled: {error}");
            STARTED.store(false, Ordering::SeqCst);
        }
    }

    pub fn stop() {
        STOPPING.store(true, Ordering::SeqCst);
        let bar = BAR.load(Ordering::SeqCst);
        if bar != 0
            && let Err(error) =
                unsafe { PostMessageW(Some(HWND(bar as *mut _)), WM_QUIT, WPARAM(0), LPARAM(0)) }
        {
            tracing::debug!("the jump bar could not be told to stop: {error}");
        }
    }

    pub fn take_query() -> Option<(u64, String)> {
        PENDING_QUERY
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    pub fn set_results(id: u64, results: Vec<Found>) {
        {
            let mut search = SEARCH.lock().unwrap_or_else(|error| error.into_inner());
            if search.id != id {
                return;
            }
            search.remote = results;
            refresh_view(&mut search);
        }
        invalidate();
    }

    // --- geometry ------------------------------------------------------------

    /// Turns a design pixel into device pixels for the dialog's DPI. Kept in
    /// floating point so 125% and 150% screens are not rounded up to 200%.
    fn dpi_units(units: i32) -> i32 {
        let dpi = match SCALE_DPI.load(Ordering::SeqCst) {
            0 => unsafe { GetDpiForSystem() as i32 },
            dpi => dpi,
        }
        .max(96);
        (units as f32 * dpi as f32 / 96.0 * SIZE_PERCENT.load(Ordering::SeqCst) as f32 / 100.0)
            .round() as i32
    }

    fn bar_height() -> i32 {
        dpi_units(40)
    }

    fn row_height() -> i32 {
        dpi_units(28)
    }

    fn search_width() -> i32 {
        dpi_units(180)
    }

    fn radius() -> i32 {
        dpi_units(8)
    }

    // --- the thread ----------------------------------------------------------

    fn run() {
        let instance = match unsafe { GetModuleHandleW(None) } {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!("dialog jump disabled: {error}");
                return;
            }
        };
        let class_name = w!("LumaFiles.DialogJump.Bar");
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            tracing::warn!("dialog jump disabled: RegisterClassW failed");
            return;
        }
        let bar = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class_name,
                w!(""),
                WS_POPUP,
                0,
                0,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                Some(instance.into()),
                None,
            )
        };
        let Ok(bar) = bar else {
            tracing::warn!("dialog jump disabled: the bar window could not be made");
            return;
        };
        BAR.store(bar.0 as isize, Ordering::SeqCst);
        tracing::debug!("dialog jump watching");

        loop {
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                if message.message == WM_QUIT {
                    STOPPING.store(true, Ordering::SeqCst);
                    break;
                }
                unsafe {
                    let _ = TranslateMessage(&message);
                    let _ = DispatchMessageW(&message);
                }
            }
            if STOPPING.load(Ordering::SeqCst) {
                break;
            }
            tick(bar);
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        }

        if HOTKEY.swap(false, Ordering::SeqCst) {
            unsafe { UnregisterHotKey(Some(bar), HOTKEY_G) }.ok();
        }
        BAR.store(0, Ordering::SeqCst);
        let _ = unsafe { DestroyWindow(bar) };
        unsafe { RemovePropW(bar, AUTO_PROPERTY) }.ok();
    }

    fn tick(bar: HWND) {
        let context = CONTEXT
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if !context.enabled {
            hide(bar);
            forget_dialog();
            return;
        }
        let foreground = unsafe { GetForegroundWindow() };
        // While the person is typing in the bar it is the foreground window;
        // the dialog it belongs to is the last one seen.
        let (dialog, on_bar) = if foreground == bar {
            (HWND(LAST_DIALOG.load(Ordering::SeqCst) as *mut _), true)
        } else {
            (foreground, false)
        };
        if dialog.is_invalid() || !is_file_dialog(dialog) {
            if LAST_DIALOG.swap(0, Ordering::SeqCst) != 0 {
                tracing::debug!("dialog jump: no dialog in front any more");
            }
            hide(bar);
            forget_dialog();
            return;
        }
        let dialog_raw = dialog.0 as isize;
        let app = switch_dialog(dialog, dialog_raw);
        if !on_bar
            && LAST_DIALOG.load(Ordering::SeqCst) != dialog_raw
            && context.auto_jump
            && !context.current.is_empty()
            && claim_auto_jump(dialog)
        {
            tracing::debug!("dialog goes to {} once", context.current);
            navigate_to(dialog, &context.current, Some(&app), false);
        }
        LAST_DIALOG.store(dialog_raw, Ordering::SeqCst);
        if !context.bar {
            hide(bar);
            return;
        }
        update_paint(&context);
        refresh_local(&context);
        place(bar, dialog);
        if !HOTKEY.load(Ordering::SeqCst) {
            match unsafe {
                RegisterHotKey(Some(bar), HOTKEY_G, MOD_CONTROL | MOD_NOREPEAT, 'G' as u32)
            } {
                Ok(()) => {
                    tracing::debug!("dialog jump: Ctrl+G registered");
                    HOTKEY.store(true, Ordering::SeqCst);
                }
                Err(error) => tracing::debug!("dialog jump: Ctrl+G not registered: {error}"),
            }
        }
        double_ctrl();
    }

    /// Keeps the remembered folders in step with the program whose dialog is in
    /// front; returns its executable name.
    fn switch_dialog(dialog: HWND, dialog_raw: isize) -> String {
        let app = process_name(dialog).unwrap_or_default();
        if LAST_DIALOG.load(Ordering::SeqCst) == dialog_raw {
            return app;
        }
        let remembered = remembered(&app);
        let mut search = SEARCH.lock().unwrap_or_else(|error| error.into_inner());
        search.remembered = remembered;
        search.remote.clear();
        search.local.clear();
        search.view.clear();
        search.selected = -1;
        search.query.clear();
        if search.focused {
            refresh_view(&mut search);
        }
        app
    }

    /// Two Ctrl taps while a dialog is in front put the keyboard on the search
    /// box, as Pulse does.
    fn double_ctrl() {
        let down = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
        if down == CTRL_DOWN.swap(down, Ordering::SeqCst) || !down {
            return;
        }
        let now = Instant::now();
        let last = LAST_CTRL.get_or_init(|| Mutex::new(None));
        let mut last = last.lock().unwrap_or_else(|error| error.into_inner());
        if last.is_some_and(|stamp| now.duration_since(stamp) < std::time::Duration::from_millis(600))
        {
            *last = None;
            drop(last);
            focus_search();
        } else {
            *last = Some(now);
        }
    }

    fn forget_dialog() {
        LAST_DIALOG.store(0, Ordering::SeqCst);
        let mut search = SEARCH.lock().unwrap_or_else(|error| error.into_inner());
        search.query.clear();
        search.results_clear();
        search.id = NEXT_QUERY.fetch_add(1, Ordering::SeqCst);
    }

    impl SearchState {
        fn results_clear(&mut self) {
            self.local.clear();
            self.remote.clear();
            self.view.clear();
            self.selected = -1;
        }
    }

    fn hide(bar: HWND) {
        if let Ok(mut search) = SEARCH.lock() {
            search.focused = false;
        }
        let _ = unsafe { ShowWindow(bar, SW_HIDE) };
        if HOTKEY.swap(false, Ordering::SeqCst) {
            unsafe { UnregisterHotKey(Some(bar), HOTKEY_G) }.ok();
        }
    }

    fn update_paint(context: &Context) {
        let mut paint = PAINT.lock().unwrap_or_else(|error| error.into_inner());
        paint.bg = context.bg;
        paint.fg = context.fg;
        paint.accent = context.accent;
        paint.hint = context.hint.clone();
        paint.placeholder = context.placeholder.clone();
        paint.chips.clear();
        let mut seen: Vec<&str> = Vec::new();
        if !context.current.is_empty() {
            paint.chips.push(Chip {
                start: 0,
                end: 0,
                path: context.current.clone(),
                label: folder_name(&context.current),
            });
            seen.push(&context.current);
        }
        for folder in context.folders.iter() {
            if paint.chips.len() >= 8 {
                break;
            }
            if seen
                .iter()
                .any(|path| path.eq_ignore_ascii_case(&folder.path))
            {
                continue;
            }
            paint.chips.push(Chip {
                start: 0,
                end: 0,
                path: folder.path.clone(),
                label: folder.label.clone(),
            });
            seen.push(&folder.path);
        }
    }

    /// Recomputes what the bar itself can answer: the memory list when the box
    /// is empty, otherwise the folders it knows that match the typed text.
    fn refresh_local(context: &Context) {
        let mut search = SEARCH.lock().unwrap_or_else(|error| error.into_inner());
        if search.query.trim().is_empty() {
            search.remote.clear();
            refresh_view(&mut search);
            return;
        }
        let query = search.query.trim().to_lowercase();
        let mut local: Vec<Found> = Vec::new();
        let mut push = |folder: &Folder| {
            if local.len() >= MAX_RESULTS * 2 {
                return;
            }
            let label = folder.label.to_lowercase();
            if (label.contains(&query) || folder.path.to_lowercase().contains(&query))
                && !local.iter().any(|found| found.path == folder.path)
            {
                local.push(Found {
                    path: folder.path.clone(),
                    label: folder.label.clone(),
                    dir: true,
                });
            }
        };
        for folder in context.places.iter() {
            push(folder);
        }
        for folder in search.remembered.iter() {
            push(folder);
        }
        for folder in context.folders.iter() {
            push(folder);
        }
        search.local = local;
        refresh_view(&mut search);
    }

    /// The list drawn: the memory list when the box is empty, otherwise the
    /// local answers with the index answers appended (folders first).
    fn refresh_view(search: &mut SearchState) {
        let mut view: Vec<Found> = if search.query.trim().is_empty() {
            if !search.focused {
                Vec::new()
            } else {
                let mut out: Vec<Found> = search
                    .remembered
                    .iter()
                    .map(|folder| Found {
                        path: folder.path.clone(),
                        label: folder.label.clone(),
                        dir: true,
                    })
                    .collect();
                for folder in CONTEXT
                    .lock()
                    .map(|context| context.places.clone())
                    .unwrap_or_default()
                {
                    if out.len() >= MAX_RESULTS {
                        break;
                    }
                    if !out.iter().any(|found| found.path == folder.path) {
                        out.push(Found {
                            path: folder.path,
                            label: folder.label,
                            dir: true,
                        });
                    }
                }
                out
            }
        } else {
            let mut out = search.local.clone();
            for found in search.remote.iter() {
                if out.iter().any(|known| known.path == found.path) {
                    continue;
                }
                out.push(found.clone());
            }
            // Folders first: the dialog is being moved to one most of the time.
            out.sort_by_key(|found| !found.dir);
            out
        };
        view.truncate(MAX_RESULTS);
        let count = view.len() as isize;
        let selected = if count == 0 {
            -1
        } else if search.selected < 0 {
            0
        } else {
            search.selected.min(count - 1)
        };
        let changed = view != search.view || selected != search.selected;
        search.view = view;
        search.selected = selected;
        if changed {
            invalidate();
        }
    }

    fn query_changed() {
        let text = {
            let search = SEARCH.lock().unwrap_or_else(|error| error.into_inner());
            search.query.clone()
        };
        let id = NEXT_QUERY.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut search) = SEARCH.lock() {
            search.id = id;
            search.remote.clear();
        }
        *PENDING_QUERY.lock().unwrap_or_else(|error| error.into_inner()) = Some((id, text));
        let context = CONTEXT
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        refresh_local(&context);
    }

    fn invalidate() {
        let bar = BAR.load(Ordering::SeqCst);
        if bar != 0 {
            let _ = unsafe { InvalidateRect(Some(HWND(bar as *mut _)), None, false) };
        }
    }

    fn folder_name(path: &str) -> String {
        std::path::Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| path.to_owned())
    }

    // --- placement and navigation --------------------------------------------

    fn place(bar: HWND, dialog: HWND) {
        // `GetWindowRect` includes the invisible resize border a resizable
        // dialog has: using it leaves the bar a few pixels below the dialog and
        // wider than it on both sides. The visible frame is what the bar has to
        // line up with.
        let mut rect = RECT::default();
        let visible = unsafe {
            DwmGetWindowAttribute(
                dialog,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&mut rect as *mut RECT).cast(),
                std::mem::size_of::<RECT>() as u32,
            )
        };
        if visible.is_err() && unsafe { GetWindowRect(dialog, &mut rect) }.is_err() {
            return;
        }
        let dpi = unsafe { GetDpiForWindow(dialog) }.max(96) as i32;
        SCALE_DPI.store(dpi, Ordering::SeqCst);
        let rows = SEARCH
            .lock()
            .map(|search| search.view.len().min(MAX_RESULTS))
            .unwrap_or(0) as i32;
        let height = bar_height() + rows * row_height();
        let width = (rect.right - rect.left).max(260);
        let x = rect.left;
        // Flush with the dialog: one line is all that separates them, as in
        // Pulse's bar. The list grows downward; on a dialog near the bottom
        // edge of the screen the bar goes above it, also flush.
        let screen = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        let mut y = rect.bottom;
        let mut above = false;
        if y + height > screen {
            y = (rect.top - height).max(0);
            above = true;
        }
        ABOVE.store(above, Ordering::SeqCst);
        if let Err(error) = unsafe {
            SetWindowPos(
                bar,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        } {
            tracing::debug!("the jump bar would not be placed: {error}");
        }
        // Round the corners on the far side from the dialog, keep the side that
        // touches it square.
        let r = radius();
        let region = if above {
            unsafe { CreateRoundRectRgn(0, 0, width + 1, height + r + 1, r * 2, r * 2) }
        } else {
            unsafe { CreateRoundRectRgn(0, -r, width + 1, height + 1, r * 2, r * 2) }
        };
        if !region.is_invalid() {
            let _ = unsafe { SetWindowRgn(bar, Some(region), true) };
        }
        let _ = unsafe { ShowWindow(bar, SW_SHOWNOACTIVATE) };
    }

    fn claim_auto_jump(dialog: HWND) -> bool {
        if !unsafe { GetPropW(dialog, AUTO_PROPERTY) }.0.is_null() {
            return false;
        }
        unsafe {
            SetPropW(
                dialog,
                AUTO_PROPERTY,
                Some(HANDLE(std::ptr::dangling_mut::<core::ffi::c_void>())),
            )
        }
        .is_ok()
    }

    /// Puts the keyboard on the bar's search box.
    fn focus_search() {
        let bar = BAR.load(Ordering::SeqCst);
        if bar == 0 {
            return;
        }
        let bar = HWND(bar as *mut _);
        unsafe {
            if GetForegroundWindow() != bar {
                keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
                keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
                let _ = SetForegroundWindow(bar);
                if GetForegroundWindow() != bar {
                    SwitchToThisWindow(bar, true);
                }
                std::thread::sleep(std::time::Duration::from_millis(90));
            }
            let mut focused = SetFocus(Some(bar)).is_ok();
            if !focused {
                std::thread::sleep(std::time::Duration::from_millis(60));
                focused = SetFocus(Some(bar)).is_ok();
            }
            if !focused {
                tracing::debug!("dialog jump: the search box could not take the keyboard");
            }
        }
    }

    fn jump_result(index: usize) {
        let (found, bar_focused) = {
            let search = SEARCH.lock().unwrap_or_else(|error| error.into_inner());
            (search.view.get(index).cloned(), search.focused)
        };
        let Some(found) = found else {
            return;
        };
        let dialog = LAST_DIALOG.load(Ordering::SeqCst);
        if dialog == 0 {
            return;
        }
        let dialog = HWND(dialog as *mut _);
        if !unsafe { IsWindow(Some(dialog)) }.as_bool() {
            return;
        }
        if bar_focused {
            let _ = unsafe { SetForegroundWindow(dialog) };
        }
        let app = process_name(dialog);
        if found.dir {
            navigate_to(dialog, &found.path, app.as_deref(), true);
        } else {
            fill_and_submit(dialog, &found.path);
            if let Some(app) = &app {
                record_jump(app, &folder_of(&found.path));
            }
        }
        let mut search = SEARCH.lock().unwrap_or_else(|error| error.into_inner());
        search.query.clear();
        search.local.clear();
        search.remote.clear();
        search.view.clear();
        search.selected = -1;
    }

    fn folder_of(path: &str) -> String {
        std::path::Path::new(path)
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// A chosen file goes into the dialog's name box and is submitted, which is
    /// what the person asked for by choosing it.
    fn fill_and_submit(dialog: HWND, path: &str) {
        if let Some(edit) = file_name_edit(dialog) {
            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = unsafe { SetWindowTextW(edit, PCWSTR(wide.as_ptr())) };
            let button = unsafe { GetDlgItem(Some(dialog), IDOK.0) }.ok();
            let command = (IDOK.0 as usize & 0xFFFF) | ((BN_CLICKED as usize) << 16);
            unsafe {
                let _ = PostMessageW(
                    Some(dialog),
                    WM_COMMAND,
                    WPARAM(command),
                    LPARAM(button.map(|button| button.0 as isize).unwrap_or(0)),
                );
            }
            return;
        }
        // Vista and later dialogs keep the name box inside their DirectUI
        // layout, with no legacy id to set. Alt+N reaches it in every interface
        // language LumaFiles ships, then the path replaces what is there and
        // Enter submits it.
        if !unsafe { IsWindow(Some(dialog)) }.as_bool() {
            return;
        }
        let _ = unsafe { SetForegroundWindow(dialog) };
        std::thread::sleep(std::time::Duration::from_millis(150));
        unsafe {
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(b'N', 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(b'N', 0, KEYEVENTF_KEYUP, 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
            std::thread::sleep(std::time::Duration::from_millis(120));
            keybd_event(VK_CONTROL.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(b'A', 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(b'A', 0, KEYEVENTF_KEYUP, 0);
            keybd_event(VK_CONTROL.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
        type_text(path);
        std::thread::sleep(std::time::Duration::from_millis(60));
        unsafe {
            keybd_event(VK_RETURN.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_RETURN.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        }
    }

    /// Takes the dialog to `path` through its address bar: Alt+D, the path,
    /// Enter.
    fn navigate_to(dialog: HWND, path: &str, app: Option<&str>, record: bool) {
        tracing::debug!("dialog jump: going to {path}");
        if !unsafe { IsWindow(Some(dialog)) }.as_bool() {
            return;
        }
        let focused_here = SEARCH
            .lock()
            .map(|search| search.focused)
            .unwrap_or(false);
        if focused_here {
            let _ = unsafe { SetForegroundWindow(dialog) };
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        if unsafe { GetForegroundWindow() } != dialog {
            let _ = unsafe { SetForegroundWindow(dialog) };
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        unsafe {
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(b'D', 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(b'D', 0, KEYEVENTF_KEYUP, 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        type_text(path);
        std::thread::sleep(std::time::Duration::from_millis(60));
        unsafe {
            keybd_event(VK_RETURN.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_RETURN.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        }
        if record && let Some(app) = app {
            record_jump(app, path);
        }
    }

    #[allow(clippy::field_reassign_with_default)]
    fn type_text(text: &str) {
        let mut inputs: Vec<INPUT> = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            for up in [false, true] {
                let mut input = INPUT::default();
                input.r#type = INPUT_KEYBOARD;
                input.Anonymous.ki = KEYBDINPUT {
                    wVk: Default::default(),
                    wScan: unit,
                    dwFlags: if up {
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                    } else {
                        KEYEVENTF_UNICODE
                    },
                    time: 0,
                    dwExtraInfo: 0,
                };
                inputs.push(input);
            }
        }
        if inputs.is_empty() {
            return;
        }
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            tracing::debug!("only {sent} of {} keys reached the dialog", inputs.len());
        }
    }

    // --- window messages -----------------------------------------------------

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SETFOCUS => {
                {
                    if let Ok(mut search) = SEARCH.lock() {
                        search.focused = true;
                        refresh_view(&mut search);
                    }
                }
                let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                {
                    if let Ok(mut search) = SEARCH.lock() {
                        search.focused = false;
                        refresh_view(&mut search);
                    }
                }
                let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
                LRESULT(0)
            }
            WM_CHAR => {
                let focused = SEARCH
                    .lock()
                    .map(|search| search.focused)
                    .unwrap_or(false);
                if focused
                    && let Some(character) = char::from_u32(wparam.0 as u32)
                    && !character.is_control()
                {
                    if let Ok(mut search) = SEARCH.lock() {
                        search.query.push(character);
                    }
                    query_changed();
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let focused = SEARCH
                    .lock()
                    .map(|search| search.focused)
                    .unwrap_or(false);
                if !focused {
                    return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
                }
                match wparam.0 as u16 {
                    key if key == VK_ESCAPE.0 => {
                        let dialog = LAST_DIALOG.load(Ordering::SeqCst);
                        if dialog != 0 {
                            let _ = unsafe { SetForegroundWindow(HWND(dialog as *mut _)) };
                        }
                    }
                    key if key == VK_BACK.0 => {
                        if let Ok(mut search) = SEARCH.lock() {
                            search.query.pop();
                        }
                        query_changed();
                    }
                    key if key == VK_RETURN.0 => {
                        let (selected, count, query) = {
                            let search =
                                SEARCH.lock().unwrap_or_else(|error| error.into_inner());
                            (search.selected, search.view.len(), search.query.clone())
                        };
                        if selected >= 0 && (selected as usize) < count {
                            jump_result(selected as usize);
                        } else if !query.trim().is_empty()
                            && std::path::Path::new(query.trim()).is_dir()
                        {
                            let dialog = LAST_DIALOG.load(Ordering::SeqCst);
                            if dialog != 0 {
                                let dialog = HWND(dialog as *mut _);
                                let app = process_name(dialog);
                                navigate_to(dialog, query.trim(), app.as_deref(), true);
                            }
                        }
                    }
                    key if key == VK_DOWN.0 || key == VK_UP.0 => {
                        if let Ok(mut search) = SEARCH.lock() {
                            let count = search.view.len() as isize;
                            if count > 0 {
                                let mut selected = search.selected;
                                if key == VK_DOWN.0 {
                                    selected = (selected + 1).min(count - 1);
                                } else {
                                    selected = (selected - 1).max(0);
                                }
                                search.selected = selected;
                            }
                        }
                        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = low_word(lparam);
                let y = high_word(lparam);
                let bar_h = bar_height();
                let (hover, hover_row) = if y < bar_h {
                    (chip_at(x), -1)
                } else {
                    (-1, ((y - bar_h) / row_height()) as isize)
                };
                let mut paint = PAINT.lock().unwrap_or_else(|error| error.into_inner());
                if paint.hover != hover || paint.hover_row != hover_row {
                    paint.hover = hover;
                    paint.hover_row = hover_row;
                    drop(paint);
                    let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = low_word(lparam);
                let y = high_word(lparam);
                let bar_h = bar_height();
                if y >= bar_h {
                    let index = ((y - bar_h) / row_height()) as usize;
                    let drawn = PAINT.lock().map(|paint| paint.rows.len()).unwrap_or(0);
                    if index < drawn {
                        jump_result(index);
                    }
                    return LRESULT(0);
                }
                let search_box = PAINT
                    .lock()
                    .map(|paint| paint.search_box)
                    .unwrap_or((0, 0, 0, 0));
                if x >= search_box.0 && x < search_box.2 && y >= search_box.1 && y < search_box.3 {
                    unsafe {
                        if GetForegroundWindow() != hwnd {
                            let _ = SetForegroundWindow(hwnd);
                        }
                        let _ = SetFocus(Some(hwnd));
                    }
                    return LRESULT(0);
                }
                if let Some(index) = chip_index(x) {
                    let path = {
                        let paint = PAINT.lock().unwrap_or_else(|error| error.into_inner());
                        paint.layout.get(index).map(|chip| chip.path.clone())
                    };
                    if let Some(path) = path {
                        tracing::debug!("dialog jump: chip {index} -> {path}");
                        let dialog = LAST_DIALOG.load(Ordering::SeqCst);
                        if dialog != 0 {
                            let dialog = HWND(dialog as *mut _);
                            let app = process_name(dialog);
                            navigate_to(dialog, &path, app.as_deref(), true);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                if wparam.0 as i32 == HOTKEY_G {
                    tracing::debug!("dialog jump: Ctrl+G pressed");
                    let dialog = LAST_DIALOG.load(Ordering::SeqCst);
                    let current = CONTEXT
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .current
                        .clone();
                    if dialog != 0 && !current.is_empty() {
                        let dialog = HWND(dialog as *mut _);
                        let app = process_name(dialog);
                        navigate_to(dialog, &current, app.as_deref(), true);
                    }
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    fn low_word(value: LPARAM) -> i32 {
        (value.0 as i32) & 0xFFFF
    }

    fn high_word(value: LPARAM) -> i32 {
        ((value.0 as i32) >> 16) & 0xFFFF
    }

    fn chip_index(x: i32) -> Option<usize> {
        let paint = PAINT.lock().unwrap_or_else(|error| error.into_inner());
        paint
            .layout
            .iter()
            .position(|chip| x >= chip.start && x < chip.end)
    }

    fn chip_at(x: i32) -> isize {
        chip_index(x).map_or(-1, |index| index as isize)
    }

    // --- painting ------------------------------------------------------------

    fn paint(hwnd: HWND) {
        let mut paint_struct = PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(hwnd, &mut paint_struct) };
        if hdc.0.is_null() {
            return;
        }
        let mut client = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut client) };
        let bar_h = bar_height();

        let state = PAINT.lock().unwrap_or_else(|error| error.into_inner());
        let search = SEARCH.lock().unwrap_or_else(|error| error.into_inner());

        let background = unsafe { CreateSolidBrush(colorref(state.bg)) };
        let _ = unsafe { FillRect(hdc, &client, background) };
        let _ = unsafe { DeleteObject(background.into()) };

        // The one line that separates the bar from the dialog it hangs from.
        let line = (dpi_units(1) / 2).max(1);
        let seam = if ABOVE.load(Ordering::SeqCst) {
            RECT {
                left: client.left,
                top: client.bottom - line,
                right: client.right,
                bottom: client.bottom,
            }
        } else {
            RECT {
                left: client.left,
                top: client.top,
                right: client.right,
                bottom: client.top + line,
            }
        };
        let seam_fill = unsafe { CreateSolidBrush(colorref(mix(state.bg, state.fg, 0.28))) };
        let _ = unsafe { FillRect(hdc, &seam, seam_fill) };
        let _ = unsafe { DeleteObject(seam_fill.into()) };

        let font = unsafe {
            CreateFontW(
                -dpi_units(14),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                w!("Microsoft YaHei UI"),
            )
        };
        let previous = unsafe { SelectObject(hdc, font.into()) };
        let _ = unsafe { SetBkMode(hdc, TRANSPARENT) };

        let top = dpi_units(5) + line;
        let height = (bar_h - top * 2).max(dpi_units(18));

        // The search box: a rounded field with a magnifier drawn beside it.
        let field = RECT {
            left: dpi_units(8),
            top,
            right: dpi_units(8) + search_width(),
            bottom: top + height,
        };
        round_fill(
            hdc,
            &field,
            mix(state.bg, state.fg, if search.focused { 0.20 } else { 0.09 }),
        );
        draw_magnifier(hdc, field.left + dpi_units(12), field.top + height / 2, colorref(mix(state.fg, state.bg, 0.35)));
        let mut text_rect = RECT {
            left: field.left + dpi_units(30),
            top: field.top,
            right: field.right - dpi_units(8),
            bottom: field.bottom,
        };
        let (text, muted) = if search.query.is_empty() {
            if search.focused {
                (String::new(), false)
            } else {
                (state.placeholder.clone(), true)
            }
        } else {
            (
                format!("{}{}", search.query, if search.focused { "|" } else { "" }),
                false,
            )
        };
        let _ = unsafe {
            SetTextColor(
                hdc,
                colorref(if muted {
                    mix(state.fg, state.bg, 0.42)
                } else {
                    state.fg
                }),
            )
        };
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        if !wide.is_empty() {
            let _ = unsafe {
                DrawTextW(
                    hdc,
                    &mut wide,
                    &mut text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                )
            };
        }

        // The chips, after the search box, stopping where the hint begins.
        let mut hint_width = 0;
        if !state.hint.is_empty() {
            let mut wide: Vec<u16> = state.hint.encode_utf16().collect();
            let mut measure = RECT::default();
            let _ = unsafe {
                DrawTextW(
                    hdc,
                    &mut wide,
                    &mut measure,
                    DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
                )
            };
            hint_width = measure.right - measure.left + dpi_units(16);
        }
        let chips_limit = client.right - dpi_units(12);
        let mut x = field.right + dpi_units(10);
        let mut chips = Vec::with_capacity(state.chips.len());
        for (index, chip) in state.chips.iter().enumerate() {
            let mut wide: Vec<u16> = chip.label.encode_utf16().collect();
            let mut measure = RECT::default();
            let _ = unsafe {
                DrawTextW(
                    hdc,
                    &mut wide,
                    &mut measure,
                    DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
                )
            };
            let mut width = (measure.right - measure.left).max(dpi_units(10)) + dpi_units(18);
            if x + width > chips_limit {
                width = chips_limit - x;
                if width < dpi_units(60) {
                    break;
                }
            }
            let rect = RECT {
                left: x,
                top,
                right: x + width,
                bottom: top + height,
            };
            let current = index == 0;
            round_fill(
                hdc,
                &rect,
                if current {
                    state.accent
                } else if state.hover == index as isize {
                    mix(state.bg, state.fg, 0.26)
                } else {
                    mix(state.bg, state.fg, 0.14)
                },
            );
            let mut text_rect = RECT {
                left: rect.left + dpi_units(9),
                top: rect.top,
                right: rect.right - dpi_units(6),
                bottom: rect.bottom,
            };
            let _ = unsafe {
                SetTextColor(
                    hdc,
                    colorref(if current { state.bg } else { state.fg }),
                )
            };
            let _ = unsafe {
                DrawTextW(
                    hdc,
                    &mut wide,
                    &mut text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                )
            };
            chips.push(Chip {
                start: rect.left,
                end: rect.right,
                path: chip.path.clone(),
                label: chip.label.clone(),
            });
            x = rect.right + dpi_units(6);
        }

        // The Ctrl+G hint, right aligned, only when it does not crowd the chips.
        let _ = unsafe { SetTextColor(hdc, colorref(mix(state.fg, state.bg, 0.45))) };
        if !state.hint.is_empty() && x + hint_width <= client.right - dpi_units(8) {
            let mut wide: Vec<u16> = state.hint.encode_utf16().collect();
            let mut hint_rect = RECT {
                left: client.left + dpi_units(8),
                top,
                right: client.right - dpi_units(12),
                bottom: top + height,
            };
            let _ = unsafe {
                DrawTextW(
                    hdc,
                    &mut wide,
                    &mut hint_rect,
                    DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                )
            };
        }

        // A hairline between the bar and its list.
        let row_h = row_height();
        let mut rows = Vec::new();
        if !search.view.is_empty() {
            let line = RECT {
                left: 0,
                top: bar_h - 1,
                right: client.right,
                bottom: bar_h,
            };
            let line_fill = unsafe { CreateSolidBrush(colorref(mix(state.bg, state.fg, 0.16))) };
            let _ = unsafe { FillRect(hdc, &line, line_fill) };
            let _ = unsafe { DeleteObject(line_fill.into()) };
        }

        // The list: one row per answer, name left, folder right, in accent where
        // the typed text matches.
        for (index, found) in search.view.iter().take(MAX_RESULTS).enumerate() {
            let row_top = bar_h + index as i32 * row_h;
            if row_top >= client.bottom {
                break;
            }
            let rect = RECT {
                left: 0,
                top: row_top,
                right: client.right,
                bottom: row_top + row_h,
            };
            let selected = search.selected == index as isize;
            if selected {
                let fill =
                    unsafe { CreateSolidBrush(colorref(mix(state.bg, state.accent, 0.32))) };
                let _ = unsafe { FillRect(hdc, &rect, fill) };
                let _ = unsafe { DeleteObject(fill.into()) };
                let strip = RECT {
                    left: 0,
                    top: row_top,
                    right: dpi_units(3),
                    bottom: row_top + row_h,
                };
                let strip_fill = unsafe { CreateSolidBrush(colorref(state.accent)) };
                let _ = unsafe { FillRect(hdc, &strip, strip_fill) };
                let _ = unsafe { DeleteObject(strip_fill.into()) };
            } else if state.hover_row == index as isize {
                let fill = unsafe { CreateSolidBrush(colorref(mix(state.bg, state.fg, 0.08))) };
                let _ = unsafe { FillRect(hdc, &rect, fill) };
                let _ = unsafe { DeleteObject(fill.into()) };
            }

            let glyph = dpi_units(16);
            let glyph_x = dpi_units(10);
            let glyph_y = row_top + (row_h - glyph) / 2;
            if found.dir {
                draw_folder(hdc, glyph_x, glyph_y, glyph, colorref(state.accent));
            } else {
                draw_file(hdc, glyph_x, glyph_y, glyph, colorref(mix(state.fg, state.bg, 0.45)));
            }

            // The parent folder, right aligned and muted first, so the name can
            // be clipped before it.
            let parent = folder_of(&found.path);
            let mut parent_wide: Vec<u16> = parent.encode_utf16().collect();
            let mut parent_rect = RECT {
                left: client.left,
                top: row_top,
                right: client.right - dpi_units(12),
                bottom: row_top + row_h,
            };
            let parent_width = if parent.is_empty() {
                0
            } else {
                let mut measure = RECT::default();
                let _ = unsafe {
                    DrawTextW(
                        hdc,
                        &mut parent_wide,
                        &mut measure,
                        DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
                    )
                };
                let width = measure.right - measure.left;
                parent_rect.left = (parent_rect.right - width).max(glyph_x + glyph + dpi_units(40));
                let _ = unsafe {
                    SetTextColor(hdc, colorref(mix(state.fg, state.bg, 0.48)))
                };
                let _ = unsafe {
                    DrawTextW(
                        hdc,
                        &mut parent_wide,
                        &mut parent_rect,
                        DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                    )
                };
                width
            };

            let name_left = glyph_x + glyph + dpi_units(10);
            let name_right = (client.right - parent_width - dpi_units(24)).max(name_left + dpi_units(40));
            let mut name_rect = RECT {
                left: name_left,
                top: row_top,
                right: name_right,
                bottom: row_top + row_h,
            };
            let _ = unsafe { SetTextColor(hdc, colorref(state.fg)) };
            let name = &found.label;
            match highlight_parts(name, &search.query) {
                Some((before, hit, after)) => {
                    let mut cursor = name_rect.left;
                    for (part, color) in [
                        (before, colorref(state.fg)),
                        (hit, colorref(state.accent)),
                        (after, colorref(state.fg)),
                    ] {
                        if part.is_empty() {
                            continue;
                        }
                        let mut wide: Vec<u16> = part.encode_utf16().collect();
                        let mut measure = RECT::default();
                        let _ = unsafe {
                            DrawTextW(
                                hdc,
                                &mut wide,
                                &mut measure,
                                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
                            )
                        };
                        let mut part_rect = RECT {
                            left: cursor,
                            top: name_rect.top,
                            right: name_rect.right,
                            bottom: name_rect.bottom,
                        };
                        let _ = unsafe { SetTextColor(hdc, color) };
                        let _ = unsafe {
                            DrawTextW(
                                hdc,
                                &mut wide,
                                &mut part_rect,
                                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                            )
                        };
                        cursor += measure.right - measure.left;
                    }
                }
                None => {
                    let mut wide: Vec<u16> = name.encode_utf16().collect();
                    let _ = unsafe {
                        DrawTextW(
                            hdc,
                            &mut wide,
                            &mut name_rect,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                        )
                    };
                }
            }
            rows.push((rect.top, rect.bottom));
        }

        drop(search);
        drop(state);

        if let Ok(mut state) = PAINT.lock() {
            state.layout = chips;
            state.search_box = (field.left, field.top, field.right, field.bottom);
            state.rows = rows;
        }
        unsafe {
            let _ = SelectObject(hdc, previous);
            let _ = DeleteObject(font.into());
            let _ = EndPaint(hwnd, &paint_struct);
        }
    }

    fn highlight_parts(text: &str, query: &str) -> Option<(String, String, String)> {
        let query = query.trim();
        if query.is_empty() {
            return None;
        }
        let haystack: Vec<char> = text.chars().collect();
        let needle: Vec<char> = query.to_lowercase().chars().collect();
        if needle.is_empty() || needle.len() > haystack.len() {
            return None;
        }
        let lowered: Vec<char> = haystack
            .iter()
            .map(|character| character.to_lowercase().next().unwrap_or(*character))
            .collect();
        let start = lowered
            .windows(needle.len())
            .position(|window| window == needle.as_slice())?;
        Some((
            haystack[..start].iter().collect(),
            haystack[start..start + needle.len()].iter().collect(),
            haystack[start + needle.len()..].iter().collect(),
        ))
    }

    /// Fills a rounded rectangle with `color`, keeping the caller's pen/brush.
    fn round_fill(hdc: HDC, rect: &RECT, color: u32) {
        let brush = unsafe { CreateSolidBrush(colorref(color)) };
        unsafe {
            let previous_brush = SelectObject(hdc, brush.into());
            let previous_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
            let _ = RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, radius(), radius());
            let _ = SelectObject(hdc, previous_pen);
            let _ = SelectObject(hdc, previous_brush);
        }
        let _ = unsafe { DeleteObject(brush.into()) };
    }

    fn draw_magnifier(hdc: HDC, center_x: i32, center_y: i32, color: COLORREF) {
        let size = dpi_units(5);
        let brush = unsafe { CreateSolidBrush(color) };
        let pen = unsafe {
            windows::Win32::Graphics::Gdi::CreatePen(
                windows::Win32::Graphics::Gdi::PS_SOLID,
                dpi_units(1).max(1),
                color,
            )
        };
        unsafe {
            let previous_pen = SelectObject(hdc, pen.into());
            let previous_brush = SelectObject(hdc, GetStockObject(windows::Win32::Graphics::Gdi::NULL_BRUSH));
            let _ = windows::Win32::Graphics::Gdi::Ellipse(
                hdc,
                center_x - size,
                center_y - size,
                center_x + size / 2,
                center_y + size / 2,
            );
            let _ = windows::Win32::Graphics::Gdi::MoveToEx(hdc, center_x + size / 2, center_y + size / 2, None);
            let _ = windows::Win32::Graphics::Gdi::LineTo(hdc, center_x + size, center_y + size);
            let _ = SelectObject(hdc, previous_brush);
            let _ = SelectObject(hdc, previous_pen);
            let _ = DeleteObject(pen.into());
            let _ = DeleteObject(brush.into());
        }
    }

    fn draw_folder(hdc: HDC, x: i32, y: i32, size: i32, color: COLORREF) {
        let brush = unsafe { CreateSolidBrush(color) };
        let body = RECT {
            left: x,
            top: y + size / 4,
            right: x + size,
            bottom: y + size,
        };
        let tab = RECT {
            left: x,
            top: y,
            right: x + size / 2,
            bottom: y + size / 3,
        };
        let _ = unsafe { FillRect(hdc, &body, brush) };
        let _ = unsafe { FillRect(hdc, &tab, brush) };
        let _ = unsafe { DeleteObject(brush.into()) };
    }

    fn draw_file(hdc: HDC, x: i32, y: i32, size: i32, color: COLORREF) {
        let brush = unsafe { CreateSolidBrush(color) };
        let body = RECT {
            left: x + size / 8,
            top: y,
            right: x + size - size / 8,
            bottom: y + size,
        };
        let _ = unsafe { FillRect(hdc, &body, brush) };
        let _ = unsafe { DeleteObject(brush.into()) };
    }

    fn file_name_edit(dialog: HWND) -> Option<HWND> {
        use windows::Win32::UI::WindowsAndMessaging::FindWindowExW;
        if let Ok(combo) = unsafe { GetDlgItem(Some(dialog), FILE_NAME_COMBO) }
            && let Ok(edit) = unsafe {
                FindWindowExW(Some(combo), None, windows::core::w!("Edit"), PCWSTR::null())
            }
        {
            return Some(edit);
        }
        unsafe { GetDlgItem(Some(dialog), FILE_NAME_EDIT) }.ok()
    }

    fn colorref(color: u32) -> COLORREF {
        let r = (color >> 16) & 0xFF;
        let g = (color >> 8) & 0xFF;
        let b = color & 0xFF;
        COLORREF(r | (g << 8) | (b << 16))
    }

    fn mix(a: u32, b: u32, amount: f32) -> u32 {
        let channel = |shift: u32| {
            let low = ((a >> shift) & 0xFF) as f32;
            let high = ((b >> shift) & 0xFF) as f32;
            ((low + (high - low) * amount).round().clamp(0.0, 255.0) as u32) << shift
        };
        channel(16) | channel(8) | channel(0)
    }

    fn is_file_dialog(dialog: HWND) -> bool {
        let mut class = [0u16; 32];
        let length = unsafe { GetClassNameW(dialog, &mut class) } as usize;
        if String::from_utf16_lossy(&class[..length]) != "#32770" {
            return false;
        }
        // A dialog runs as the same user when this can be read; one in an
        // elevated program cannot be driven by a normal program's input, so it
        // is left alone rather than shown a bar that would do nothing.
        let Some(name) = process_name(dialog) else {
            return false;
        };
        if IGNORED_APPS.contains(&name.as_str()) {
            return false;
        }
        // The old common dialogs keep the file name in the combo 1148 (or the
        // edit 1152). Vista and later ones (also what most programs and .NET
        // use for "Save as") are drawn with DirectUI instead; they are only
        // recognisable by that child and by the Open/Save button.
        let legacy = unsafe { GetDlgItem(Some(dialog), FILE_NAME_COMBO) }.is_ok()
            || unsafe { GetDlgItem(Some(dialog), FILE_NAME_EDIT) }.is_ok();
        legacy || (child_class_exists(dialog, "DirectUIHWND") && ok_button_opens_or_saves(dialog))
    }

    fn child_class_exists(parent: HWND, class: &str) -> bool {
        unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> windows::core::BOOL {
            let data = unsafe { &mut *(param.0 as *mut (String, bool)) };
            let (wanted, found) = data;
            let mut buffer = [0u16; 64];
            let length = unsafe { GetClassNameW(hwnd, &mut buffer) } as usize;
            if String::from_utf16_lossy(&buffer[..length]).eq_ignore_ascii_case(wanted) {
                *found = true;
                return windows::core::BOOL(0);
            }
            windows::core::BOOL(1)
        }
        let mut data = (class.to_owned(), false);
        let _ = unsafe {
            windows::Win32::UI::WindowsAndMessaging::EnumChildWindows(
                Some(parent),
                Some(visit),
                LPARAM((&mut data) as *mut _ as isize),
            )
        };
        data.1
    }

    /// Whether the dialog's OK button reads Open, Save or Select folder in the
    /// interface languages LumaFiles ships with.
    fn ok_button_opens_or_saves(dialog: HWND) -> bool {
        let Ok(button) = (unsafe { GetDlgItem(Some(dialog), IDOK.0) }) else {
            return false;
        };
        let mut text = [0u16; 64];
        let length = unsafe { GetWindowTextW(button, &mut text) } as usize;
        let mut label = String::from_utf16_lossy(&text[..length]).to_lowercase();
        label.retain(|character| character != '&' && character != '(' && character != ')');
        // Drop the accelerator key that follows the label, e.g. "save(s)".
        if let Some(cut) = label.rfind(|character: char| character.is_ascii_alphabetic())
            && label[cut..].len() <= 2
            && label.len() > 3
        {
            label.truncate(cut);
        }
        const ACTIONS: &[&str] = &[
            "open", "save", "save as", "guardar", "abrir", "salvar", "abrir como",
            "enregistrer", "ouvrir", "speichern", "offnen", "salva", "apri", "salva con nome",
            "открыть", "сохранить", "開く", "名前を付けて保存", "打开", "保存", "另存为",
            "select folder", "selecionar pasta", "seleccionar carpeta", "choisir un dossier",
            "ordner auswählen", "seleziona cartella", "выбрать папку", "选择文件夹",
        ];
        ACTIONS.iter().any(|action| label.contains(action))
    }

    fn process_name(dialog: HWND) -> Option<String> {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(dialog, Some(&mut pid)) };
        if pid == std::process::id() {
            return None;
        }
        let process = unsafe {
            windows::Win32::System::Threading::OpenProcess(
                windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        }
        .ok()?;
        let mut path = [0u16; 1024];
        let mut length = path.len() as u32;
        let queried = unsafe {
            windows::Win32::System::Threading::QueryFullProcessImageNameW(
                process,
                windows::Win32::System::Threading::PROCESS_NAME_WIN32,
                windows::core::PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        };
        unsafe { windows::Win32::Foundation::CloseHandle(process) }.ok();
        queried.ok()?;
        let path = String::from_utf16_lossy(&path[..length as usize]);
        std::path::Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
    }
}