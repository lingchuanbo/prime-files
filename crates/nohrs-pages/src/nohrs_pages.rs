//! Top-level page views for the nohrs application, including the explorer,
//! git, S3, and settings pages, along with the root view that
//! hosts them.

use gpui::AnyElement;

/// Making Nohrs the folder handler of the Windows user.
pub mod default_explorer;
/// The file explorer page and its supporting state and views.
pub mod explorer;
/// The git page.
pub mod git;
/// Picking files for a browser's "Open" dialog from Nohrs.
pub mod file_dialog;
/// The local channel other programs (a second `nohrs.exe`, the Listary
/// plugin) reach the running instance through.
pub mod ipc;
pub mod listary;
/// Which language "auto" means on this machine.
pub mod locale;
/// A reusable 2-way split/tab container shared across pages.
pub mod compact;
/// Panels docked to any side: the shared gestures and drawing.
pub(crate) mod dock;
pub mod pane_group;
/// Automatic updates from the project's GitHub releases.
pub mod updater;
/// Other programs' "show in folder", answered by Nohrs.
pub mod reveal;
/// The standalone, indexed full-computer search window.
pub mod search_window;
/// The application root view that hosts the sidebar and active page.
pub mod root;
/// The S3 page.
pub mod s3;
// removed search
/// The settings page.
pub mod settings;

/// The colour wheel of the settings page.
pub mod color_wheel;
/// The right-click menu: the Windows context menu with Nohrs's entries.
pub(crate) mod shell_menu;
/// The terminal panel under the explorer, one shell per tab.
pub mod terminal;
/// The welcome screen of the first run.
pub mod welcome;

pub use root::RootView;

/// Identifies which top-level page is currently selected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageKind {
    /// The file explorer page.
    Explorer,

    /// The git page.
    Git,
    /// The S3 page.
    S3,
    /// The settings page.
    Settings,
}

impl PageKind {
    /// Returns the human-readable label for this page.
    pub fn label(&self) -> &'static str {
        match self {
            PageKind::Explorer => "Explorer",
            PageKind::Git => "Git",
            PageKind::S3 => "S3",
            PageKind::Settings => "Settings",
        }
    }

    /// Returns the asset path of the icon representing this page.
    pub fn icon_path(&self) -> &'static str {
        match self {
            PageKind::Explorer => "icons/folder.svg",

            PageKind::Git => "icons/github.svg",
            PageKind::S3 => "icons/database.svg",
            PageKind::Settings => "icons/settings.svg",
        }
    }

    /// Returns all page kinds in display order.
    pub fn all() -> Vec<PageKind> {
        vec![
            PageKind::Explorer,
            PageKind::Git,
            PageKind::S3,
            PageKind::Settings,
        ]
    }
}

/// Trait for page rendering
pub trait Page {
    /// Renders the page into an element tree.
    fn render(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> AnyElement
    where
        Self: Sized;
}
