/// Platform traffic-light (window control button) configuration and hooks.
pub mod traffic_lights;

use gpui::{Bounds, Pixels, WindowBounds, WindowOptions};

use self::traffic_lights::TrafficLightsHook;

/// The class Windows registers the explorer window under (Nohrs; see
/// `third_party/gpui/NOHRS.md`). A script that drives a window by its class —
/// AutoHotkey's `ahk_class`, or any Win32 caller — matches on this name.
///
/// The other windows that are told apart this way name their own class where
/// they are opened: see [`QUICK_LAUNCH_WINDOW_CLASS`] and
/// [`SEARCH_WINDOW_CLASS`].
pub const MAIN_WINDOW_CLASS: &str = "LumaFiles_WinClass";

/// The class Windows registers the quick launch panel under.
pub const QUICK_LAUNCH_WINDOW_CLASS: &str = "LumaFiles_QuickLauncher_WinClass";

/// The class Windows registers the standalone search window under.
pub const SEARCH_WINDOW_CLASS: &str = "LumaFiles_Search_WinClass";

/// Construct window options that enable the unified toolbar styling and apply the provided
/// traffic light configuration.
pub fn unified_window_options(
    bounds: Bounds<Pixels>,
    traffic_lights: &TrafficLightsHook,
) -> WindowOptions {
    let mut options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_class: Some(MAIN_WINDOW_CLASS),
        ..Default::default()
    };

    traffic_lights.apply(&mut options);
    // The name Windows shows on the taskbar, in Alt+Tab and Task Manager.
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some(APP_NAME.into());
    }
    options
}

/// The product's name, as people see it.
pub const APP_NAME: &str = "轻寻文件 · LumaFiles";
