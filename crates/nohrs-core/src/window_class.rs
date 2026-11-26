//! The window classes Windows registers this program's windows under.
//!
//! The class is the name Win32 callers and AutoHotkey's `ahk_class` match, so
//! these strings are part of the program's interface: renaming one breaks the
//! scripts that use it. Only Windows has window classes; everywhere else the
//! name is carried to the platform layer and ignored. The list users read is in
//! `docs/external-api.md`.
//!
//! A window that names none of these is registered under [`MAIN`], which is
//! what the toolkit used to call every one of them.

/// The explorer's main window.
pub const MAIN: &str = "LumaFiles_WinClass";

/// The quick launch panel: the summonable grid of launch items.
pub const QUICK_LAUNCH: &str = "LumaFiles_QuickLauncher_WinClass";

/// The standalone search window (`Ctrl+Shift+F`).
pub const SEARCH: &str = "LumaFiles_Search_WinClass";

/// The launcher palette: `Ctrl+Shift+Space` from anywhere, `Ctrl+K` inside the
/// program.
pub const LAUNCHER: &str = "LumaFiles_Launcher_WinClass";

/// The batch rename window.
pub const BATCH_RENAME: &str = "LumaFiles_BatchRename_WinClass";

/// The window resident mode keeps open while it waits for its hotkey.
pub const KEEP_ALIVE: &str = "LumaFiles_KeepAlive_WinClass";
