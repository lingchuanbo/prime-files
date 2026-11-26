/// Platform traffic-light (window control button) configuration and hooks.
pub mod traffic_lights;

use gpui::{Bounds, Pixels, WindowBounds, WindowOptions};

use self::traffic_lights::TrafficLightsHook;

/// Construct window options that enable the unified toolbar styling and apply the provided
/// traffic light configuration.
pub fn unified_window_options(
    bounds: Bounds<Pixels>,
    traffic_lights: &TrafficLightsHook,
) -> WindowOptions {
    let mut options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // The explorer's window class; every other window that scripts drive
        // names its own (see `nohrs_core::window_class`).
        window_class: Some(nohrs_core::window_class::MAIN),
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
