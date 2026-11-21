//! Picking files for another program's "Open" or "Save" dialog from Nohrs.
//!
//! The dialog itself cannot be replaced: it runs inside the other program, and
//! most programs refuse injected code. So Nohrs watches for the foreground to
//! become a program's standard file dialog, comes forward with a banner, and
//! when a file is chosen here writes its path into the dialog's file name box
//! and presses Open, as a person typing it would.
//!
//! On by default and switched off by `explorer.file_dialog_assist` (Settings ›
//! integrations).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{AsyncWindowContext, Context, WeakEntity, Window};

use crate::root::RootView;

/// Whether another program's dialogs are answered here at all
/// (`explorer.file_dialog_assist`).
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Turns the takeover on or off. Off, a dialog that is already being picked for
/// is given back untouched and no new one is taken.
pub fn set_enabled(enabled: bool) {
    tracing::debug!("file dialog assist enabled: {enabled}");
    ENABLED.store(enabled, Ordering::Relaxed);
    if !enabled {
        set_picking(None);
    }
}

fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// A program's file dialog waiting for Nohrs to choose for it.
#[derive(Clone, Debug)]
pub struct Dialog {
    hwnd: isize,
    /// The dialog's title ("Open", "Abrir"…), for the banner.
    pub title: String,
    /// The program's executable name, for the banner.
    pub browser: String,
}

// The dialog being picked for. A global rather than a field of the root view:
// the explorer tab that activates a file is a different entity, and it only
// needs to know whether to hand the file over instead of opening it.
static PICKING: Mutex<Option<Dialog>> = Mutex::new(None);

/// The dialog being picked for, if any.
pub fn picking() -> Option<Dialog> {
    PICKING.lock().ok().and_then(|picking| picking.clone())
}

fn set_picking(dialog: Option<Dialog>) {
    if let Ok(mut picking) = PICKING.lock() {
        *picking = dialog;
    }
}

/// Hands `paths` to the dialog being picked for and closes it with Open.
/// Returns `false` when there is no such dialog any more.
pub fn deliver(paths: &[String]) -> bool {
    let Some(dialog) = picking() else {
        return false;
    };
    set_picking(None);
    // The dialog is about to close: do not pick it up again in between.
    platform::skip(dialog.hwnd);
    !paths.is_empty() && platform::fill_and_open(dialog.hwnd, paths)
}

/// Stops picking and gives the dialog back to the user, untouched.
pub fn cancel() {
    if let Some(dialog) = picking() {
        set_picking(None);
        platform::skip(dialog.hwnd);
        platform::bring_forward(dialog.hwnd);
    }
}

/// Starts watching for other programs' file dialogs. While one is open Nohrs is
/// in picking mode; the banner and the poll that ends it run on the root view.
pub fn watch(window: &mut Window, cx: &mut Context<RootView>) {
    let (sender, receiver) = async_channel::unbounded::<Dialog>();
    if !platform::start_hook(sender) {
        return;
    }
    cx.spawn_in(
        window,
        async move |this: WeakEntity<RootView>, cx: &mut AsyncWindowContext| {
            loop {
                // Either a dialog came up, or it is time to check that the one
                // being picked for still exists: the user may have chosen in the
                // dialog itself, or cancelled it.
                let arrived = next_dialog_or_tick(&receiver, cx).await;
                let changed = match arrived {
                    Some(dialog) => {
                        tracing::debug!("picking for {} \"{}\"", dialog.browser, dialog.title);
                        set_picking(Some(dialog));
                        true
                    }
                    None => match picking() {
                        Some(dialog) if !platform::is_open(dialog.hwnd) => {
                            set_picking(None);
                            true
                        }
                        _ => false,
                    },
                };
                if changed {
                    let updated = this.update_in(cx, |root, window, cx| {
                        root.show_explorer(cx);
                        if picking().is_some() {
                            // The program whose dialog opened owns the
                            // foreground; this is what lets the window come
                            // over it instead of staying behind.
                            crate::foreground::bring_to_front(window);
                        }
                        cx.notify();
                    });
                    if updated.is_err() {
                        return;
                    }
                }
            }
        },
    )
    .detach();
}

// Waits for the next dialog, but no longer than the poll interval.
async fn next_dialog_or_tick(
    receiver: &async_channel::Receiver<Dialog>,
    cx: &mut AsyncWindowContext,
) -> Option<Dialog> {
    let timer = cx.background_executor().timer(Duration::from_millis(400));
    let next = receiver.recv();
    first_of(next, timer).await
}

async fn first_of(
    next: impl std::future::Future<Output = Result<Dialog, async_channel::RecvError>>,
    timer: impl std::future::Future<Output = ()>,
) -> Option<Dialog> {
    use std::pin::pin;
    use std::task::Poll;
    let mut next = pin!(next);
    let mut timer = pin!(timer);
    std::future::poll_fn(|context| {
        if let Poll::Ready(dialog) = next.as_mut().poll(context) {
            return Poll::Ready(dialog.ok());
        }
        if timer.as_mut().poll(context).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! The watch thread and the dialog automation. `unsafe` is confined here:
    //! none of this has a safe binding.

    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BN_CLICKED, FindWindowExW, GetClassNameW, GetDlgItem, GetForegroundWindow, GetWindowTextW,
        GetWindowThreadProcessId, IDOK, IsWindow, SendMessageW, SetForegroundWindow, WM_COMMAND,
        WM_SETTEXT,
    };
    use windows::core::{PCWSTR, PWSTR, w};

    use super::Dialog;

    /// Control id of the file name combo box in the common Open/Save dialog
    /// (`cmb13`); the Vista-style dialog keeps it for compatibility. Its
    /// presence is what tells a file dialog from any other `#32770` box.
    const FILE_NAME_COMBO: i32 = 1148;
    /// The file name edit box of the older dialog layout (`edt1`).
    const FILE_NAME_EDIT: i32 = 1152;

    /// How often the foreground is looked at. A dialog box takes the foreground
    /// before its controls exist, so a single event would arrive too early; the
    /// poll costs nothing and sees the finished dialog within a moment.
    const POLL: Duration = Duration::from_millis(120);

    /// Processes whose dialogs are not taken: the shell's own windows (their
    /// dialogs are about the desktop, not a folder to pick from) and this app's
    /// own dialogs. Every other program's standard Open/Save dialog is answered
    /// while `explorer.file_dialog_assist` is on.
    const IGNORED_APPS: &[&str] = &["explorer.exe", "searchapp.exe", "lumafiles.exe"];

    static SENDER: OnceLock<async_channel::Sender<Dialog>> = OnceLock::new();
    /// Dialogs the person sent back to their program; the watcher leaves them
    /// alone until they close.
    static SKIPPED: Mutex<Vec<isize>> = Mutex::new(Vec::new());

    pub fn start_hook(sender: async_channel::Sender<Dialog>) -> bool {
        if SENDER.set(sender).is_err() {
            return false;
        }
        std::thread::Builder::new()
            .name("nohrs-file-dialogs".into())
            .spawn(|| {
                tracing::debug!("file dialog watch started");
                loop {
                    std::thread::sleep(POLL);
                    if !super::enabled() {
                        continue;
                    }
                    let hwnd = unsafe { GetForegroundWindow() };
                    if hwnd.is_invalid() {
                        continue;
                    }
                    let Some(dialog) = answered_file_dialog(hwnd) else {
                        continue;
                    };
                    // Already being picked for (the user came back to the
                    // dialog), or handed back to the program: leave it alone.
                    if super::picking().is_some_and(|picking| picking.hwnd == dialog.hwnd) {
                        continue;
                    }
                    if skipped(dialog.hwnd) {
                        continue;
                    }
                    deliver(dialog);
                }
            })
            .is_ok()
    }

    fn skipped(hwnd: isize) -> bool {
        let mut skipped = SKIPPED.lock().unwrap_or_else(|error| error.into_inner());
        skipped.retain(|open| is_open(*open));
        skipped.contains(&hwnd)
    }

    /// Leaves the dialog `hwnd` to its program for the rest of its life.
    pub fn skip(hwnd: isize) {
        let mut skipped = SKIPPED.lock().unwrap_or_else(|error| error.into_inner());
        if !skipped.contains(&hwnd) {
            skipped.push(hwnd);
        }
    }

    fn deliver(dialog: Dialog) {
        tracing::debug!(
            "file dialog found: {} \"{}\" for {}",
            dialog.hwnd,
            dialog.title,
            dialog.browser
        );
        if let Some(sender) = SENDER.get() {
            sender.try_send(dialog).ok();
        }
    }

    fn answered_file_dialog(hwnd: HWND) -> Option<Dialog> {
        if window_class(hwnd) != "#32770" {
            return None;
        }
        unsafe { GetDlgItem(Some(hwnd), FILE_NAME_COMBO) }.ok()?;
        let browser = process_name(hwnd)?;
        if IGNORED_APPS.contains(&browser.as_str()) {
            return None;
        }
        let mut title = [0u16; 256];
        let length = unsafe { GetWindowTextW(hwnd, &mut title) } as usize;
        Some(Dialog {
            hwnd: hwnd.0 as isize,
            title: String::from_utf16_lossy(&title[..length]),
            browser,
        })
    }

    fn window_class(hwnd: HWND) -> String {
        let mut class = [0u16; 32];
        let length = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
        String::from_utf16_lossy(&class[..length])
    }

    fn process_name(hwnd: HWND) -> Option<String> {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut path = [0u16; 1024];
        let mut length = path.len() as u32;
        let queried = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        };
        unsafe { CloseHandle(process) }.ok();
        queried.ok()?;
        let path = String::from_utf16_lossy(&path[..length as usize]);
        let name = std::path::Path::new(&path).file_name()?.to_string_lossy().to_lowercase();
        Some(name)
    }

    pub fn is_open(hwnd: isize) -> bool {
        unsafe { IsWindow(Some(HWND(hwnd as *mut _))) }.as_bool()
    }

    pub fn bring_forward(hwnd: isize) {
        // Nohrs is in front when this runs, which is what lets it hand the
        // foreground over.
        unsafe { SetForegroundWindow(HWND(hwnd as *mut _)) }.ok().ok();
    }

    pub fn fill_and_open(hwnd: isize, paths: &[String]) -> bool {
        let dialog = HWND(hwnd as *mut _);
        if !is_open(hwnd) {
            return false;
        }
        let Some(edit) = file_name_edit(dialog) else {
            return false;
        };
        // One file goes in as its full path. Several are quoted names, which
        // the dialog reads against its current folder, so it is sent to that
        // folder first; a dialog that does not allow several takes the first.
        let text = match paths {
            [single] => single.clone(),
            many => {
                let folder = std::path::Path::new(&many[0])
                    .parent()
                    .map(|folder| folder.to_string_lossy().into_owned())
                    .unwrap_or_default();
                set_text(edit, &folder);
                press_open(dialog);
                // The dialog moves to the folder asynchronously.
                std::thread::sleep(std::time::Duration::from_millis(300));
                many.iter()
                    .filter_map(|path| std::path::Path::new(path).file_name())
                    .map(|name| format!("\"{}\"", name.to_string_lossy()))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        };
        set_text(edit, &text);
        press_open(dialog);
        bring_forward(hwnd);
        true
    }

    fn file_name_edit(dialog: HWND) -> Option<HWND> {
        if let Ok(combo) = unsafe { GetDlgItem(Some(dialog), FILE_NAME_COMBO) }
            && let Ok(edit) = unsafe { FindWindowExW(Some(combo), None, w!("Edit"), PCWSTR::null()) }
        {
            return Some(edit);
        }
        unsafe { GetDlgItem(Some(dialog), FILE_NAME_EDIT) }.ok()
    }

    fn set_text(edit: HWND, text: &str) {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            SendMessageW(edit, WM_SETTEXT, Some(WPARAM(0)), Some(LPARAM(wide.as_ptr() as isize)));
        }
    }

    // The Open button's own click, sent as the command it would send, so it
    // works without the dialog having focus.
    fn press_open(dialog: HWND) {
        let button = unsafe { GetDlgItem(Some(dialog), IDOK.0) }.ok();
        let command = (IDOK.0 as usize & 0xFFFF) | ((BN_CLICKED as usize) << 16);
        unsafe {
            SendMessageW(
                dialog,
                WM_COMMAND,
                Some(WPARAM(command)),
                Some(LPARAM(button.map(|button| button.0 as isize).unwrap_or(0))),
            );
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::Dialog;

    pub fn start_hook(_sender: async_channel::Sender<Dialog>) -> bool {
        false
    }
    pub fn skip(_hwnd: isize) {}
    pub fn is_open(_hwnd: isize) -> bool {
        false
    }
    pub fn bring_forward(_hwnd: isize) {}
    pub fn fill_and_open(_hwnd: isize, _paths: &[String]) -> bool {
        false
    }
}