//! What the explorer's shortcuts run, and the "open this folder in another
//! program" commands of the header's menu.
//!
//! The key handler in `view.rs` asks `nohrs_core::shortcuts` which command a
//! key press means and hands it here; the keys themselves live in the
//! shortcut table and `[keybindings]`, never in this file.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{Context, Window};
use nohrs_core::shortcuts::Action;
use nohrs_core::telemetry::LogErr;
use nohrs_core::tf;

use super::ExplorerPane;
use super::types::StatusLevel;

/// A program the header's menu opens the folder in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenIn {
    CommandPrompt,
    PowerShell,
    WindowsTerminal,
    FileExplorer,
}

impl ExplorerPane {
    /// Runs the command a shortcut stands for. Returns false for the ones
    /// the page answers through gpui's actions (tabs, panes), so the key
    /// goes on to them.
    pub(crate) fn run_shortcut(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match action {
            Action::Copy => self.copy_selection(cx),
            Action::Cut => self.cut_selection(cx),
            Action::Paste => self.paste_into_cwd(window, cx),
            Action::Rename => {
                if let Some(index) = self.active_row() {
                    self.begin_rename(index, window, cx);
                }
            }
            Action::Refresh => self.reload(cx),
            Action::Trash => self.trash_selection(cx),
            Action::DeletePermanently => self.request_permanent_delete(window, cx),
            Action::NewFolder => self.create_new_folder(window, cx),
            Action::GoUp => self.go_up(window, cx),
            // The header's recent-folders menu, on the keyboard: the same
            // list its arrow shows.
            Action::RecentFolders => {
                self.recent_menu = !self.recent_menu;
                cx.notify();
            }
            Action::DirectoryMenu => self.open_favorites_menu(window, cx),
            // The preview pane, on the side of the listing: the same key
            // shows it and hides it again, and the choice is kept for the
            // next start (the header's button does the same). In a split, the
            // preview goes to the other pane instead, as Total Commander does.
            Action::TogglePreview => {
                if self.multi_pane {
                    self.toggle_preview_in_other_pane(window, cx);
                } else {
                    self.preview_visible = !self.preview_visible;
                    super::view::panels::save_layout(
                        "preview_visible",
                        self.preview_visible.to_string(),
                        cx,
                    );
                    cx.notify();
                }
            }
            Action::SelectAll => {
                self.select_all();
                cx.notify();
            }
            Action::SearchSubfolders => self.set_quick_recursive(true, window, cx),
            Action::SearchEverything => {
                let on = !self.everything_on();
                self.set_everything(on, cx);
                self.focus_quick_input(None, window, cx);
            }
            Action::SearchIndex => self.toggle_search(window, cx),
            Action::FilterBar => self.toggle_filter_bar(cx),
            Action::ZoomIn => {
                self.zoom_to(super::zoom::step(self.zoom_target, super::zoom::STEP), cx);
                self.flash_zoom_menu(cx);
            }
            Action::ZoomOut => {
                self.zoom_to(super::zoom::step(self.zoom_target, -super::zoom::STEP), cx);
                self.flash_zoom_menu(cx);
            }
            Action::ZoomReset => {
                self.zoom_to(super::zoom::DEFAULT_GRID, cx);
                self.flash_zoom_menu(cx);
            }
            Action::OpenCommandPrompt => self.open_in(OpenIn::CommandPrompt, cx),
            Action::OpenPowerShell => self.open_in(OpenIn::PowerShell, cx),
            Action::OpenFileExplorer => self.open_in(OpenIn::FileExplorer, cx),
            Action::Compress7z => self.compress_active(super::archive_ops::Format::SevenZip, cx),
            Action::CompressZipFast => {
                self.compress_active(super::archive_ops::Format::ZipFast, cx)
            }
            Action::CompressZipMax => self.compress_active(super::archive_ops::Format::ZipMax, cx),
            Action::CompressSelection7z => self.compress_selection(cx),
            Action::ExtractArchive => self.extract_active_and_navigate(window, cx),
            Action::DeselectAll => self.clear_selection_and_notify(cx),
            Action::InvertSelection => self.invert_selection(cx),
            Action::SelectSameExtension => self.select_same_extension(cx),
            Action::TypeFilter => self.open_type_filter(window, cx),
            Action::ToggleHiddenFiles => self.toggle_hidden_files(cx),
            Action::SortByName => self.sort_by(super::types::SortKey::Name, cx),
            Action::SortByExtension => self.sort_by(super::types::SortKey::Type, cx),
            Action::SortBySize => self.sort_by(super::types::SortKey::Size, cx),
            Action::SortByModified => self.sort_by(super::types::SortKey::Modified, cx),
            Action::CopyToOtherPane => self.transfer_selection_to_other_pane(true, cx),
            Action::MoveToOtherPane => self.transfer_selection_to_other_pane(false, cx),
            Action::CopyNameAndPath => self.copy_selected_text(CopyText::NameAndPath, cx),
            Action::CopyName => self.copy_selected_text(CopyText::Name, cx),
            Action::CopyNameWithoutExtension => {
                self.copy_selected_text(CopyText::NameWithoutExtension, cx)
            }
            Action::CopyPath => self.copy_selected_text(CopyText::Path, cx),
            Action::CopyCustomColumns => self.copy_custom_columns(cx),
            Action::FocusAddressBar => self.focus_address_bar(window, cx),
            Action::OpenDesktop => self.open_desktop(cx),
            Action::ColumnMenu => self.open_column_menu(None, window, cx),
            Action::NewTab
            | Action::CloseTab
            | Action::SplitVertical
            | Action::SplitHorizontal
            | Action::FocusPane1
            | Action::FocusPane2
            | Action::NextPane
            | Action::PreviousPane
            | Action::ToggleSidebar
            | Action::MoveTabToRightPane
            | Action::MoveTabToLeftPane
            | Action::OpenFolderInRightPane
            | Action::OpenFolderInLeftPane
            | Action::SwapPanes
            | Action::NextTab
            | Action::PreviousTab
            | Action::TabMenu
            | Action::ShowShortcuts
            | Action::SearchHere
            | Action::ToggleTerminal => return false,
        }
        true
    }

    /// Shows the zoom menu for a moment after the zoom changed by wheel or
    /// key, so the slider says where it went, as File Pilot does. A menu
    /// opened by a click stays until a click elsewhere; one shown this way
    /// goes away by itself unless the mouse is on it.
    pub(crate) fn flash_zoom_menu(&mut self, cx: &mut Context<Self>) {
        if self.zoom_menu_open && self.zoom_menu_flash.is_none() {
            return;
        }
        self.zoom_menu_open = true;
        // Replacing the task drops the previous timer: the menu stays while
        // the wheel keeps turning.
        self.zoom_menu_flash = Some(cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1300))
                .await;
            this.update(cx, |pane, cx| {
                if pane.zoom_menu_hovered || pane.zoom_dragging {
                    // Taken over by the mouse: it closes like a clicked menu.
                    pane.zoom_menu_flash = None;
                    return;
                }
                pane.zoom_menu_open = false;
                pane.zoom_menu_flash = None;
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    /// The folder on screen as a place a program can start in: This PC has
    /// none, so programs other than Explorer start in the home folder.
    fn folder_for_programs(&self) -> PathBuf {
        let cwd = PathBuf::from(&self.cwd);
        if cwd.is_dir() {
            cwd
        } else {
            std::env::var_os("USERPROFILE").map_or_else(|| PathBuf::from("C:\\"), PathBuf::from)
        }
    }

    /// Opens the folder on screen in `program`, in a window of its own.
    pub(crate) fn open_in(&mut self, program: OpenIn, cx: &mut Context<Self>) {
        self.open_in_menu = false;
        let folder = self.folder_for_programs();
        let outcome = match program {
            OpenIn::FileExplorer => {
                // Explorer takes This PC by its shell name too.
                let target = if self.cwd.starts_with("::") {
                    std::ffi::OsString::from(&self.cwd)
                } else {
                    folder.as_os_str().to_owned()
                };
                std::process::Command::new("explorer.exe")
                    .arg(target)
                    .spawn()
                    .map(drop)
            }
            OpenIn::CommandPrompt => {
                console(std::process::Command::new("cmd.exe").arg("/K"), &folder)
            }
            OpenIn::PowerShell => {
                let mut command = std::process::Command::new(powershell());
                command.args(["-NoExit", "-NoLogo"]);
                console(&mut command, &folder)
            }
            OpenIn::WindowsTerminal => std::process::Command::new("wt.exe")
                .arg("-d")
                .arg(&folder)
                .spawn()
                .map(drop),
        };
        if let Err(error) = outcome {
            self.set_status(
                StatusLevel::Error,
                tf!("Não foi possível abrir o programa: {error}", error = error),
            );
        }
        cx.notify();
    }
}

// Starts a console program in a console window of its own: Prime Files has
// none to lend it.
fn console(command: &mut std::process::Command, folder: &std::path::Path) -> std::io::Result<()> {
    command.current_dir(folder);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        command.creation_flags(CREATE_NEW_CONSOLE);
    }
    command.spawn().map(drop)
}

/// PowerShell 7 when installed, Windows PowerShell otherwise, as the
/// terminal panel picks.
fn powershell() -> PathBuf {
    let installed = std::env::var_os("ProgramFiles")
        .map(|folder| {
            PathBuf::from(folder)
                .join("PowerShell")
                .join("7")
                .join("pwsh.exe")
        })
        .filter(|path| path.is_file());
    installed.unwrap_or_else(|| PathBuf::from("powershell.exe"))
}

/// A `&'static str` for text built at run time, for the menus whose
/// entries hold static text. Each distinct text is kept once: a handful of
/// labels and key names, however often the menus open.
pub(crate) fn interned(text: String) -> &'static str {
    static SEEN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(kept) = seen.iter().find(|kept| **kept == text) {
        return kept;
    }
    let kept: &'static str = Box::leak(text.into_boxed_str());
    seen.push(kept);
    kept
}

/// The keys of `action`, as a menu entry shows them; empty when it has none.
pub(crate) fn keys_of(action: Action) -> &'static str {
    nohrs_core::shortcuts::display(action).map_or("", interned)
}

/// Whether Windows Terminal is there to offer.
pub(crate) fn has_windows_terminal() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        std::env::var_os("LOCALAPPDATA")
            .map(|folder| {
                PathBuf::from(folder)
                    .join("Microsoft")
                    .join("WindowsApps")
                    .join("wt.exe")
            })
            .is_some_and(|path| path.exists())
    })
}

/// How long a revealed file glows.
const GLOW: std::time::Duration = std::time::Duration::from_millis(3000);

impl ExplorerPane {
    /// Whether a revealed file is still glowing, so the listing keeps drawing
    /// frames; forgets it once the glow is over.
    pub(crate) fn tick_reveal_glow(&mut self) -> bool {
        match &self.reveal_highlight {
            Some((_, since)) if since.elapsed() < GLOW => true,
            Some(_) => {
                self.reveal_highlight = None;
                false
            }
            None => false,
        }
    }

    /// A glow over the entry for `path` when it is the file just revealed:
    /// an outline and a wash in the accent colour that pulse twice and fade.
    /// Drawn over the entry without taking its clicks.
    pub(crate) fn reveal_glow(&self, path: &str) -> Option<gpui::AnyElement> {
        use gpui::{Hsla, IntoElement as _, Styled as _, div, px, rgb};
        let (revealed, since) = self.reveal_highlight.as_ref()?;
        if !super::same_path(revealed, path) {
            return None;
        }
        let t = since.elapsed().as_secs_f32() / GLOW.as_secs_f32();
        if t >= 1.0 {
            return None;
        }
        // Three pulses on a fade: bright at once, gone by the end. Strong
        // enough to read over the selection, which is the accent already.
        let pulse = 0.5 + 0.5 * (t * std::f32::consts::TAU * 3.0).cos();
        let strength = (1.0 - t).powf(1.2) * pulse;
        let mut wash: Hsla = rgb(nohrs_ui::theme::theme::accent()).into();
        let mut outline: Hsla = rgb(nohrs_ui::theme::theme::accent_hover()).into();
        wash.a = 0.5 * strength;
        outline.a = strength.min(1.0);
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .rounded(px(6.0))
                .bg(wash)
                .border_3()
                .border_color(outline)
                .into_any_element(),
        )
    }
}

/// What a copy-name shortcut puts on the clipboard.
#[derive(Clone, Copy)]
enum CopyText {
    NameAndPath,
    Name,
    NameWithoutExtension,
    Path,
}

impl ExplorerPane {
    fn clear_selection_and_notify(&mut self, cx: &mut Context<Self>) {
        self.clear_selection();
        cx.notify();
    }

    fn invert_selection(&mut self, cx: &mut Context<Self>) {
        let all: Vec<String> = self
            .filtered_entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        self.selection = all
            .into_iter()
            .filter(|path| !self.selection.contains(path))
            .collect();
        self.selection_anchor = None;
        cx.notify();
    }

    fn select_same_extension(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active_path.clone() else {
            return;
        };
        let Some(extension) = std::path::Path::new(&active)
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase())
        else {
            return;
        };
        self.selection = self
            .filtered_entries
            .iter()
            .filter(|entry| {
                std::path::Path::new(&entry.path)
                    .extension()
                    .map(|candidate| candidate.to_string_lossy().to_lowercase())
                    .as_deref()
                    == Some(extension.as_str())
            })
            .map(|entry| entry.path.clone())
            .collect();
        self.selection_anchor = Some(active);
        cx.notify();
    }

    fn toggle_hidden_files(&mut self, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        let value = self.show_hidden.to_string();
        self.write_config("ui", "show_hidden", value, cx);
        self.apply_filter();
        cx.notify();
    }

    fn open_type_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_quick_input(Some("*."), window, cx);
    }

    fn sort_by(&mut self, key: super::types::SortKey, cx: &mut Context<Self>) {
        self.set_sort_key(key, cx);
        cx.notify();
    }

    fn copy_selected_text(&mut self, kind: CopyText, cx: &mut Context<Self>) {
        let mut paths = self.selected_paths();
        if paths.is_empty()
            && let Some(active) = self.active_path.clone()
        {
            paths.push(active);
        }
        if paths.is_empty() {
            return;
        }
        let text = paths
            .iter()
            .map(|path| {
                let path_buf = std::path::Path::new(path);
                let name = path_buf
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone());
                match kind {
                    CopyText::NameAndPath => format!("{name}\t{path}"),
                    CopyText::Name => name,
                    CopyText::NameWithoutExtension => path_buf
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().to_string())
                        .unwrap_or(name),
                    CopyText::Path => path.clone(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }

    fn copy_custom_columns(&mut self, cx: &mut Context<Self>) {
        let mut paths = self.selected_paths();
        if paths.is_empty()
            && let Some(active) = self.active_path.clone()
        {
            paths.push(active);
        }
        if paths.is_empty() {
            return;
        }
        let text = paths
            .iter()
            .map(|path| {
                match self
                    .filtered_entries
                    .iter()
                    .find(|entry| &entry.path == path)
                {
                    Some(entry) => format!(
                        "{}\t{}\t{}\t{}",
                        entry.name, entry.kind, entry.size, entry.modified
                    ),
                    None => path.clone(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }

    fn transfer_selection_to_other_pane(&mut self, copy: bool, cx: &mut Context<Self>) {
        let Some(sibling) = self.sibling_pane.clone() else {
            return;
        };
        let dest = PathBuf::from(sibling.read(cx).cwd.clone());
        let sources: Vec<PathBuf> = self
            .selected_paths()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        if sources.is_empty() {
            return;
        }
        let mode = if copy {
            super::file_ops::ClipMode::Copy
        } else {
            super::file_ops::ClipMode::Cut
        };
        if self.prepare_transfer(mode, sources, dest, false) {
            while self
                .resolve_current_conflict(nohrs_services::fs::ops::ConflictResolution::Rename, cx)
            {
            }
        }
        self.execute_paste_plan(cx);
        cx.notify();
    }

    fn focus_address_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_edit.is_none() {
            self.begin_path_edit(window, cx);
        }
    }

    fn open_desktop(&mut self, cx: &mut Context<Self>) {
        let desktop = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .map(|home| home.join("Desktop"))
            .filter(|desktop| desktop.is_dir());
        let Some(desktop) = desktop else {
            return;
        };
        self.navigate_to_synced(desktop.to_string_lossy().to_string(), cx);
    }

    /// Shift+D and the like: opens that drive, or says why it cannot.
    pub(crate) fn open_drive(&mut self, letter: char, cx: &mut Context<Self>) {
        let letter = letter.to_ascii_uppercase();
        let root = format!("{letter}:\\");
        if std::path::Path::new(&root).exists() {
            self.navigate_to_synced(root, cx);
        } else {
            self.set_status(
                StatusLevel::Error,
                tf!("Não foi possível abrir o drive {letter}:", letter = letter),
            );
            cx.notify();
        }
    }
}
