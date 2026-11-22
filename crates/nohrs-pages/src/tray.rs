//! The tray icon: a notification-area icon for LumaFiles with a menu to bring
//! the window back or quit.
//!
//! The icon lives on its own thread with a hidden window of its own, so it can
//! answer clicks while the main window is hidden. Requests it makes (`show`,
//! `quit`) are flags the root view's publisher loop picks up; the window itself
//! is shown by the tray thread directly, because that click is what gives the
//! process the right to come to the front.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};

use nohrs_core::config::settings::TrayIcon;

/// A request from the tray menu, waiting for the UI thread.
static SHOW: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);
/// Whether the main window is hidden in the tray.
static HIDDEN: AtomicBool = AtomicBool::new(false);
/// 0 = always, 1 = only while hidden, 2 = never.
static MODE: AtomicU8 = AtomicU8::new(0);
/// The main window's handle, for showing it from the tray thread.
static MAIN: AtomicI64 = AtomicI64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

/// Starts the tray icon on its own thread. Called once, after the main window
/// exists.
pub fn start(main_window: i64) {
    MAIN.store(main_window, Ordering::SeqCst);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("luma-tray".into())
        .spawn(platform::run);
    if let Err(error) = spawned {
        tracing::warn!("tray icon disabled: {error}");
        STARTED.store(false, Ordering::SeqCst);
    }
}

/// Removes the icon and ends the tray thread. Called on the way out.
pub fn stop() {
    platform::stop();
}

/// Tells the tray thread whether the window is hidden.
pub fn set_hidden(hidden: bool) {
    HIDDEN.store(hidden, Ordering::SeqCst);
    platform::sync();
}

/// Changes when the icon is shown.
pub fn set_mode(mode: TrayIcon) {
    MODE.store(
        match mode {
            TrayIcon::Always => 0,
            TrayIcon::Background => 1,
            TrayIcon::Never => 2,
        },
        Ordering::SeqCst,
    );
    platform::sync();
}

/// Whether the tray asked for the window to be shown.
pub fn take_show() -> bool {
    SHOW.swap(false, Ordering::SeqCst)
}

/// Whether the tray asked LumaFiles to quit.
pub fn take_quit() -> bool {
    QUIT.swap(false, Ordering::SeqCst)
}

/// Brings the window back, from the tray menu or from a second launch.
pub fn show(window: &gpui::Window) {
    if let Some(hwnd) = crate::ipc::window_hwnd(window) {
        platform::show_window(hwnd);
    }
    window.activate_window();
    set_hidden(false);
}

/// Hides the window in the tray, from the window's close request.
pub fn hide(window: &gpui::Window) {
    if let Some(hwnd) = crate::ipc::window_hwnd(window) {
        platform::hide_window(hwnd);
    }
    set_hidden(true);
}

#[cfg(not(windows))]
mod platform {
    pub fn run() {}
    pub fn stop() {}
    pub fn sync() {}
    pub fn show_window(_hwnd: i64) {}
    pub fn hide_window(_hwnd: i64) {}
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! The icon, its hidden window and its menu. `unsafe` stays here.

    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        ExtractIconExW, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
        Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
        DispatchMessageW, GetCursorPos, GetMessageW, IDI_APPLICATION, IsIconic, LoadIconW, MSG,
        MF_SEPARATOR, MF_STRING, PostMessageW, PostQuitMessage, RegisterClassW, SW_HIDE, SW_RESTORE,
        SW_SHOW, SetForegroundWindow, ShowWindow, TrackPopupMenu, TranslateMessage,
        TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY,
        WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    use super::{HIDDEN, MAIN, MODE, QUIT, SHOW};

    const TIP: &str = "LumaFiles";
    const TRAY_CLASS: windows::core::PCWSTR = w!("LumaFiles.Tray");
    /// The shell's callback message for icon events.
    const WM_TRAY: u32 = WM_APP + 1;
    /// Asks the tray thread to recompute whether the icon is shown.
    const WM_SYNC: u32 = WM_APP + 2;
    const MENU_SHOW: usize = 1;
    const MENU_QUIT: usize = 2;

    static WINDOW: AtomicIsize = AtomicIsize::new(0);
    /// The notification area is told about the icon once, then updated.
    static ICON_SHOWN: AtomicBool = AtomicBool::new(false);
    static STOPPING: AtomicBool = AtomicBool::new(false);

    pub fn run() {
        let instance = match unsafe { GetModuleHandleW(None) } {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!("tray icon disabled: {error}");
                return;
            }
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: TRAY_CLASS,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            tracing::warn!("tray icon disabled: RegisterClassW failed");
            return;
        }
        let window = unsafe {
            CreateWindowExW(
                Default::default(),
                TRAY_CLASS,
                TRAY_CLASS,
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )
        };
        let Ok(window) = window else {
            tracing::warn!("tray icon disabled: the hidden window could not be made");
            return;
        };
        WINDOW.store(window.0 as isize, Ordering::SeqCst);
        sync();

        let mut message = MSG::default();
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                let _ = DispatchMessageW(&message);
            }
        }
        remove_icon(window);
        WINDOW.store(0, Ordering::SeqCst);
        unsafe { DestroyWindow(window) }.ok();
    }

    pub fn stop() {
        STOPPING.store(true, Ordering::SeqCst);
        let window = WINDOW.load(Ordering::SeqCst);
        if window != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(window as *mut _)),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    pub fn sync() {
        let window = WINDOW.load(Ordering::SeqCst);
        if window != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(window as *mut _)),
                    WM_SYNC,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    pub fn show_window(hwnd: i64) {
        if hwnd == 0 {
            return;
        }
        let window = HWND(hwnd as *mut _);
        unsafe {
            let _ = ShowWindow(window, SW_SHOW);
            if IsIconic(window).as_bool() {
                let _ = ShowWindow(window, SW_RESTORE);
            }
            let _ = SetForegroundWindow(window);
        }
    }

    pub fn hide_window(hwnd: i64) {
        if hwnd != 0 {
            let _ = unsafe { ShowWindow(HWND(hwnd as *mut _), SW_HIDE) };
        }
    }

    extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_TRAY => {
                match (lparam.0 as u32) & 0xFFFF {
                    WM_LBUTTONUP => show_main(),
                    WM_RBUTTONUP => open_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match wparam.0 & 0xFFFF {
                    MENU_SHOW => show_main(),
                    MENU_QUIT => {
                        QUIT.store(true, Ordering::SeqCst);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_SYNC => {
                apply_visibility(hwnd);
                LRESULT(0)
            }
            WM_CLOSE => {
                remove_icon(hwnd);
                unsafe { DestroyWindow(hwnd) }.ok();
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    fn show_main() {
        let main = MAIN.load(Ordering::SeqCst);
        show_window(main);
        SHOW.store(true, Ordering::SeqCst);
    }

    fn open_menu(hwnd: HWND) {
        let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
            return;
        };
        let show: Vec<u16> = nohrs_core::t!("Mostrar janela")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let quit: Vec<u16> = nohrs_core::t!("Sair")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            AppendMenuW(menu, MF_STRING, MENU_SHOW, PCWSTR(show.as_ptr())).ok();
            AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()).ok();
            AppendMenuW(menu, MF_STRING, MENU_QUIT, PCWSTR(quit.as_ptr())).ok();
            let mut point = POINT::default();
            GetCursorPos(&mut point).ok();
            // A menu needs its owner in front, and a null post afterwards, or it
            // stays open when the user clicks elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
                point.x,
                point.y,
                Some(0),
                hwnd,
                None::<*const windows::Win32::Foundation::RECT>,
            )
            .ok();
            let _ = PostMessageW(Some(hwnd), WM_APP + 3, WPARAM(0), LPARAM(0));
            DestroyMenu(menu).ok();
        }
    }

    fn apply_visibility(hwnd: HWND) {
        let shown = match MODE.load(Ordering::SeqCst) {
            0 => true,
            1 => HIDDEN.load(Ordering::SeqCst),
            _ => false,
        };
        if shown && !ICON_SHOWN.load(Ordering::SeqCst) {
            add_icon(hwnd);
        } else if !shown && ICON_SHOWN.load(Ordering::SeqCst) {
            remove_icon(hwnd);
        }
    }

    fn data(hwnd: HWND) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uCallbackMessage: WM_TRAY,
            ..Default::default()
        };
        let tip: Vec<u16> = TIP.encode_utf16().collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        data
    }

    fn add_icon(hwnd: HWND) {
        let icon = app_icon();
        let Ok(icon) = icon else {
            return;
        };
        let mut data = data(hwnd);
        data.uFlags = windows::Win32::UI::Shell::NIF_MESSAGE
            | windows::Win32::UI::Shell::NIF_ICON
            | windows::Win32::UI::Shell::NIF_TIP
            | windows::Win32::UI::Shell::NIF_SHOWTIP;
        data.hIcon = icon;
        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            let mut version = data;
            version.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &version) };
            ICON_SHOWN.store(true, Ordering::SeqCst);
        }
    }

    /// The program's own icon, so the tray shows LumaFiles rather than the
    /// stock application icon; the stock one is the fallback when the exe has
    /// no icon resource.
    fn app_icon() -> windows::core::Result<windows::Win32::UI::WindowsAndMessaging::HICON> {
        let exe = std::env::current_exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let wide: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();
        let mut small = windows::Win32::UI::WindowsAndMessaging::HICON::default();
        let extracted = unsafe {
            ExtractIconExW(
                PCWSTR(wide.as_ptr()),
                0,
                None,
                Some(&mut small),
                1,
            )
        };
        if extracted > 0 && !small.is_invalid() {
            return Ok(small);
        }
        unsafe { LoadIconW(None, IDI_APPLICATION) }
    }

    fn remove_icon(hwnd: HWND) {
        if ICON_SHOWN.swap(false, Ordering::SeqCst) {
            let data = data(hwnd);
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
        }
    }
}