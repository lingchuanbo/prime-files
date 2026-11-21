//! Bringing the window to the front when another program has the foreground.
//!
//! Windows only lets the foreground process hand the foreground over: a
//! background process' `SetForegroundWindow` just flashes its taskbar button.
//! Answering another program's dialog or revealing a file puts us in exactly
//! that position — the caller owns the foreground. A quick Alt keypress through
//! the input queue is what Windows counts as user input, which is the state
//! that lets the call below move the foreground; Explorer and the shell helpers
//! use the same trick. Without it the window comes up behind the dialog it was
//! asked about and the person sees nothing happen.

use gpui::Window;

/// Brings `window` to the front (and gives it focus), even when another
/// program's window currently has the foreground.
pub(crate) fn bring_to_front(window: &Window) {
    #[cfg(windows)]
    if let Some(hwnd) = crate::ipc::window_hwnd(window) {
        // Off this thread: a program whose dialog is still being created can
        // take the foreground back for a moment, and the retries that outlast
        // it must not hold up the frame being drawn.
        let spawned = std::thread::Builder::new()
            .name("luma-foreground".into())
            .spawn(move || platform::bring_to_front(hwnd));
        if let Err(error) = spawned {
            tracing::debug!("could not take the foreground: {error}");
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, SetForegroundWindow, SwitchToThisWindow,
    };

    pub fn bring_to_front(hwnd: i64) {
        let window = HWND(hwnd as *mut core::ffi::c_void);
        unsafe {
            for _ in 0..8 {
                if GetForegroundWindow() == window {
                    // A dialog that was still opening may take it back once
                    // more; only a short hold counts as done.
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    if GetForegroundWindow() == window {
                        tracing::debug!("the window took the foreground");
                        return;
                    }
                }
                // Counts as user input, which is what lets the next call move
                // the foreground; a quick Alt has no effect of its own.
                keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
                keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
                let moved = SetForegroundWindow(window).as_bool();
                if !moved {
                    // The older switch request still works where the rules
                    // refuse the plain call.
                    SwitchToThisWindow(window, true);
                }
                if let Err(error) = BringWindowToTop(window) {
                    tracing::debug!("the window would not come to the top: {error}");
                }
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            tracing::debug!("the foreground would not stay with the window");
        }
    }
}