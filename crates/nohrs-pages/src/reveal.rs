//! "Show in folder" from other programs, answered by Nohrs.
//!
//! Premiere's "Reveal in Explorer", a browser's "Show in folder" and the Start
//! menu's "Open file location" start Explorer itself (`explorer /select,…` or
//! `SHOpenFolderAndSelectItems`), never the folder verb that makes Nohrs the
//! default, so they kept opening Explorer. explorer.exe cannot be replaced, so
//! Nohrs watches for an Explorer window to appear instead. A reveal has its
//! file selected by the time the folder's items are listed; a folder opened by
//! hand has nothing selected then, and nobody clicks that fast. Such a window
//! is closed and the file shown here.
//!
//! Active whenever Nohrs runs, default folder handler or not: the owner asked
//! for Premiere's reveal without making Nohrs the default.

use gpui::{AsyncWindowContext, Context, WeakEntity, Window};

use crate::root::RootView;

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Turns the takeover on or off (`explorer.reveal_from_other_apps`). Off,
/// the hook keeps running but leaves every Explorer window alone.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Starts watching for Explorer windows opened to reveal a file.
pub fn watch(window: &mut Window, cx: &mut Context<RootView>) {
    let (sender, receiver) = async_channel::unbounded::<String>();
    if !platform::start_hook(sender) {
        return;
    }
    cx.spawn_in(
        window,
        async move |this: WeakEntity<RootView>, cx: &mut AsyncWindowContext| {
            while let Ok(path) = receiver.recv().await {
                let shown = this.update_in(cx, |root, window, cx| root.reveal_from_elsewhere(&path, window, cx));
                if shown.is_err() {
                    return;
                }
            }
        },
    )
    .detach();
}

impl RootView {
    // The file another program asked Explorer to show, shown here instead.
    fn reveal_from_elsewhere(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((folder, select)) = crate::ipc::folder_and_selection(std::path::Path::new(path)) else {
            return;
        };
        self.show_explorer(cx);
        self.explorer_entity()
            .update(cx, |page, cx| page.open_path(folder, select, window, cx));
        window.activate_window();
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! The hook and the reading of Explorer's selection. `unsafe` is confined
    //! here: none of these interfaces has a safe binding.

    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, IServiceProvider,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
    use windows::Win32::UI::Shell::{
        IFolderView2, IShellBrowser, IShellWindows, IWebBrowser2, SID_STopLevelBrowser, SIGDN_FILESYSPATH,
        SVGIO_ALLVIEW, ShellWindows,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, EVENT_OBJECT_SHOW, GetClassNameW, GetMessageW, IsWindow, MSG,
        OBJID_WINDOW, PostMessageW, SW_HIDE, ShowWindow, TranslateMessage, WINEVENT_OUTOFCONTEXT,
        WINEVENT_SKIPOWNPROCESS, WM_CLOSE,
    };
    use windows::core::Interface;

    /// How long a new window has to list its folder. A slow drive takes a
    /// moment; past this the window is left alone.
    const SETTLE: Duration = Duration::from_secs(5);
    const POLL: Duration = Duration::from_millis(50);
    /// Polls after the listing during which a reveal's selection may still
    /// arrive: 500 ms, while leaving ordinary folder navigation alone.
    const SELECTION_GRACE: u32 = 10;
    /// How long the Explorer window stays on screen after the reveal is handed
    /// over, before it is hidden. The program that asked for the reveal
    /// (`SHOpenFolderAndSelectItems`, `explorer /select,…`, Electron's
    /// `shell.showItemInFolder`) is still inside its shell call, and a window
    /// taken away under that call leaves the caller blocked until its own
    /// timeout: QQ and WeChat show that as a minute-long freeze on the next
    /// "open folder". Hiding keeps the window alive for that call while taking
    /// the flash out of sight.
    const HIDE_DELAY: Duration = Duration::from_millis(400);
    /// How long the hidden window is left before the close; the shell may
    /// still be holding references to it for a moment.
    const CLOSE_DELAY: Duration = Duration::from_millis(2500);
    /// How long the graceful close is given before the window is closed the
    /// hard way instead.
    const CLOSE_WAIT: Duration = Duration::from_millis(1500);

    static SENDER: OnceLock<async_channel::Sender<String>> = OnceLock::new();

    pub fn start_hook(sender: async_channel::Sender<String>) -> bool {
        if SENDER.set(sender).is_err() {
            return false;
        }
        // An out-of-context hook calls back on the thread that set it, from its
        // message loop, so it gets a thread of its own for the whole run.
        std::thread::Builder::new()
            .name("nohrs-reveal-hook".into())
            .spawn(|| unsafe {
                let hook = SetWinEventHook(
                    EVENT_OBJECT_SHOW,
                    EVENT_OBJECT_SHOW,
                    None,
                    Some(on_show),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                );
                if hook.is_invalid() {
                    tracing::warn!("reveal watch disabled: SetWinEventHook failed");
                    return;
                }
                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).as_bool() {
                    _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            })
            .is_ok()
    }

    unsafe extern "system" fn on_show(
        _hook: HWINEVENTHOOK,
        _event: u32,
        hwnd: HWND,
        object: i32,
        child: i32,
        _thread: u32,
        _time: u32,
    ) {
        // Every caret and menu fires this; only whole windows of Explorer's
        // folder class matter.
        if object != OBJID_WINDOW.0 || child != 0 || !super::enabled() || !is_folder_window(hwnd) {
            return;
        }
        let hwnd = hwnd.0 as isize;
        // The view fills in after the window shows. Waited for on a thread of
        // its own, so the hook keeps answering meanwhile.
        let spawned = std::thread::Builder::new()
            .name("nohrs-reveal".into())
            .spawn(move || watch_window(hwnd));
        if let Err(error) = spawned {
            tracing::warn!("could not watch an Explorer window: {error}");
        }
    }

    fn is_folder_window(hwnd: HWND) -> bool {
        let mut class = [0u16; 32];
        let length = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
        String::from_utf16_lossy(&class[..length]) == "CabinetWClass"
    }

    fn watch_window(hwnd: isize) {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok().ok();
        let deadline = Instant::now() + SETTLE;
        // Polls since the folder's items were first listed.
        let mut listed_for = None;
        while Instant::now() < deadline {
            match read_window(hwnd) {
                Some((browser, View::Selected(path))) => {
                    tracing::debug!("reveal of {path} taken from Explorer");
                    if let Some(sender) = SENDER.get()
                        && sender.send_blocking(path).is_ok()
                    {
                        close_after_the_caller_returns(hwnd, &browser);
                    }
                    break;
                }
                Some((_, View::Listed)) => {
                    // A reveal's selection lands with the listing or just
                    // after it; still none a moment later, the window was
                    // opened on a folder and stays the user's.
                    let polls = listed_for.get_or_insert(0);
                    *polls += 1;
                    if *polls > SELECTION_GRACE {
                        break;
                    }
                }
                // Not registered or not listed yet.
                _ => {}
            }
            std::thread::sleep(POLL);
        }
        unsafe { CoUninitialize() };
    }

    // Takes the Explorer window out of sight and closes it once the program that
    // asked for the reveal has returned. Closing it at the moment the selection
    // appears destroys a window the caller's shell call is still talking to,
    // and that call then waits for its own timeout (a minute in QQ and WeChat,
    // seen as a freeze on the next "open folder"). So: leave it for a moment,
    // hide it (out of sight, still a live window), and close it after.
    fn close_after_the_caller_returns(hwnd: isize, browser: &IWebBrowser2) {
        let window = HWND(hwnd as _);
        std::thread::sleep(HIDE_DELAY);
        let _ = unsafe { ShowWindow(window, SW_HIDE) };
        std::thread::sleep(CLOSE_DELAY);
        unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) }.ok();
        let deadline = Instant::now() + CLOSE_WAIT;
        while Instant::now() < deadline {
            if !unsafe { IsWindow(Some(window)) }.as_bool() {
                return;
            }
            std::thread::sleep(POLL);
        }
        // Still there (a window busy with something else); the hard close goes
        // last, and by now nobody is waiting on it.
        tracing::warn!("an Explorer window showing a reveal did not close; asking it to");
        unsafe { browser.Quit() }.ok();
    }

    enum View {
        /// Items are listed and none is selected.
        Listed,
        /// The first selected item, as a path.
        Selected(String),
    }

    // What the Explorer window `hwnd` shows, once it has listed its folder.
    fn read_window(hwnd: isize) -> Option<(IWebBrowser2, View)> {
        let windows: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }.ok()?;
        let count = unsafe { windows.Count() }.ok()?;
        for index in 0..count {
            let Ok(item) = (unsafe { windows.Item(&VARIANT::from(index)) }) else { continue };
            let Ok(browser) = item.cast::<IWebBrowser2>() else { continue };
            if !unsafe { browser.HWND() }.is_ok_and(|handle| handle.0 == hwnd) {
                continue;
            }
            let provider: IServiceProvider = browser.cast().ok()?;
            let shell_browser: IShellBrowser = unsafe { provider.QueryService(&SID_STopLevelBrowser) }.ok()?;
            let view: IFolderView2 = unsafe { shell_browser.QueryActiveShellView() }.ok()?.cast().ok()?;
            if unsafe { view.ItemCount(SVGIO_ALLVIEW) }.ok()? == 0 {
                return None;
            }
            let selection = unsafe { view.GetSelection(false) }.ok();
            let first = selection
                .filter(|selection| unsafe { selection.GetCount() }.is_ok_and(|count| count > 0))
                .and_then(|selection| unsafe { selection.GetItemAt(0) }.ok());
            let Some(first) = first else {
                return Some((browser, View::Listed));
            };
            let name = unsafe { first.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
            let path = unsafe { name.to_string() }.ok();
            unsafe { CoTaskMemFree(Some(name.0 as _)) };
            return Some((browser, View::Selected(path?)));
        }
        None
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn start_hook(_sender: async_channel::Sender<String>) -> bool {
        false
    }
}
