//! Dragging files: out of the listing to anywhere Windows accepts files
//! (Explorer, the desktop, Premiere, a browser upload), and into the listing
//! or onto one of its folders.
//!
//! One mechanism serves both directions. A drag out is a real Windows (OLE)
//! drag, so dropping it back on Nohrs arrives through the same door as a drag
//! from Explorer: gpui's drop target, as `ExternalPaths`. The drop side then
//! only has to handle that.

use gpui::{
    Context, ExternalPaths, InteractiveElement, MouseMoveEvent, Pixels, Point, Styled, Window, rgb,
};
use nohrs_core::{t, tf};
use nohrs_ui::theme::theme;
use std::path::{Path, PathBuf};

use super::ExplorerPane;
use super::file_ops::ClipMode;
use super::types::StatusLevel;

/// How far the pointer travels with the button held before a press becomes a
/// drag, as Windows' default drag rectangle (`SM_CXDRAG`, 4 px each way)
/// rounded up for high-DPI screens.
const DRAG_THRESHOLD: f32 = 6.0;

/// Where a drag out started, for what has to be read again once it ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DragFrom {
    Listing,
    Preview,
}

/// A left press on an entry that may turn into a drag.
pub(crate) struct DragCandidate {
    ix: usize,
    origin: Point<Pixels>,
    // The press landed on an entry of a multiple selection without modifiers:
    // the selection is narrowed to it on release, unless the press became a
    // drag of the whole selection, as Explorer does.
    narrow_on_release: bool,
}

/// A left press on an item listed in the preview — a file inside the folder the
/// preview shows — that may turn into a drag. Kept apart from
/// [`DragCandidate`], which addresses the listing by row: the preview's items
/// are not rows of the listing, and there is no selection to drag with them.
pub(crate) struct PreviewDragCandidate {
    path: PathBuf,
    origin: Point<Pixels>,
}

impl PreviewDragCandidate {
    /// What the drag would carry.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl ExplorerPane {
    /// Records a left press on `path`, listed in the preview. A release
    /// without moving stays the click it was: the item opens in the preview.
    pub(crate) fn press_preview_item(&mut self, path: &str, position: Point<Pixels>) {
        self.preview_drag = Some(PreviewDragCandidate {
            path: PathBuf::from(path),
            origin: position,
        });
    }

    /// Ends a press on a preview item that did not become a drag.
    pub(crate) fn release_preview_item(&mut self) {
        self.preview_drag = None;
    }

    /// Starts dragging the pressed preview item out once the pointer has
    /// moved far enough, the same Windows drag the listing starts: dropped on
    /// Premiere it imports, on Explorer it copies or moves, on Nohrs it lands
    /// like any drop.
    pub(crate) fn preview_drag_moved(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(candidate) = self.preview_drag_ready(event) else {
            return;
        };
        self.start_drag_out(vec![candidate.path], DragFrom::Preview, window, cx);
    }

    // The pressed preview item, once the press has moved past the threshold
    // with the button held; a press whose button came up is forgotten.
    pub(crate) fn preview_drag_ready(
        &mut self,
        event: &MouseMoveEvent,
    ) -> Option<PreviewDragCandidate> {
        let candidate = self.preview_drag.as_ref()?;
        if event.pressed_button != Some(gpui::MouseButton::Left) {
            self.preview_drag = None;
            return None;
        }
        let moved = event.position - candidate.origin;
        if f32::from(moved.x).abs() < DRAG_THRESHOLD && f32::from(moved.y).abs() < DRAG_THRESHOLD {
            return None;
        }
        self.preview_drag.take()
    }

    /// Records a left press on row `ix`. Returns whether the selection should
    /// be left as it is for now (the entry is part of a multiple selection).
    pub(crate) fn press_entry(&mut self, ix: usize, position: Point<Pixels>, plain: bool) -> bool {
        self.end_hover_audio();
        let narrow_on_release = plain && self.is_selected(ix) && self.selection.len() > 1;
        self.drag_candidate = Some(DragCandidate {
            ix,
            origin: position,
            narrow_on_release,
        });
        narrow_on_release
    }

    /// Ends a press that did not become a drag.
    pub(crate) fn release_entry(&mut self, cx: &mut Context<Self>) {
        if let Some(candidate) = self.drag_candidate.take()
            && candidate.narrow_on_release
        {
            self.select_single(candidate.ix);
            cx.notify();
        }
    }

    /// Starts a drag once a press has moved far enough.
    pub(crate) fn drag_moved(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(candidate) = &self.drag_candidate else {
            return;
        };
        if event.pressed_button != Some(gpui::MouseButton::Left) {
            self.drag_candidate = None;
            return;
        }
        let moved = event.position - candidate.origin;
        if f32::from(moved.x).abs() < DRAG_THRESHOLD && f32::from(moved.y).abs() < DRAG_THRESHOLD {
            return;
        }
        let Some(candidate) = self.drag_candidate.take() else {
            return;
        };
        if self.renaming.is_some() {
            return;
        }
        let Some(entry) = self.filtered_entries.get(candidate.ix) else {
            return;
        };
        // The selection if the press was on part of it, else just that entry.
        let paths: Vec<PathBuf> = if self.selection.contains(&entry.path) {
            self.selected_paths()
                .into_iter()
                .map(PathBuf::from)
                .collect()
        } else {
            vec![PathBuf::from(&entry.path)]
        };
        self.start_drag_out(paths, DragFrom::Listing, window, cx);
    }

    fn start_drag_out(
        &mut self,
        paths: Vec<PathBuf>,
        from: DragFrom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hwnd) = crate::ipc::window_hwnd(window) else {
            return;
        };
        // From a task rather than from this handler: the drag runs its own
        // message loop until the drop, during which gpui keeps painting and
        // receives the drop itself when it lands on this window. Here the app
        // is borrowed for the handler; in a task, between polls, it is not.
        cx.spawn_in(window, async move |this, cx| {
            let outcome = platform::drag(hwnd, &paths);
            this.update(cx, |pane, cx| {
                if let Err(error) = outcome {
                    pane.set_status(
                        StatusLevel::Error,
                        tf!("Não foi possível arrastar: {error}", error = error),
                    );
                }
                // A move to another program takes the files out of this folder.
                pane.reload(cx);
                if from == DragFrom::Preview {
                    // And out of the folder the preview lists.
                    pane.refresh_preview_listing(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Copies or moves files dropped on the window into `dest`. Explorer's
    /// rules: Shift moves, Ctrl copies, and otherwise a drop on the same drive
    /// moves and one from another drive copies; with `ui.drag_copies` set, a
    /// plain drop copies too.
    pub(crate) fn drop_paths(
        &mut self,
        paths: &[PathBuf],
        dest: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = window.modifiers();
        let mode = if modifiers.shift {
            ClipMode::Cut
        } else if modifiers.control || self.drag_copies {
            ClipMode::Copy
        } else if paths.iter().all(|path| same_volume(path, &dest)) {
            ClipMode::Cut
        } else {
            ClipMode::Copy
        };
        let mut into_itself = false;
        let sources: Vec<PathBuf> = paths
            .iter()
            .filter(|source| {
                // A folder dropped into itself or one of its own subfolders.
                let nested = dest.starts_with(source);
                into_itself |= nested;
                // A move to where the file already is does nothing.
                let in_place = mode == ClipMode::Cut && source.parent() == Some(dest.as_path());
                !nested && !in_place
            })
            .cloned()
            .collect();
        if into_itself {
            self.set_status(
                StatusLevel::Warning,
                t!("Uma pasta não pode ir para dentro dela mesma."),
            );
            cx.notify();
        }
        if sources.is_empty() {
            return;
        }
        if self.prepare_transfer(mode, sources, dest, false) {
            self.open_conflict_dialog(window, cx);
        } else {
            self.execute_paste_plan(cx);
        }
    }
}

/// Makes `element` take files dropped on it into `folder`, tinted while a
/// drag is over it.
pub(crate) fn drop_into<E: InteractiveElement + Styled>(
    element: E,
    folder: &str,
    cx: &mut Context<ExplorerPane>,
) -> E {
    let dest = PathBuf::from(folder);
    element
        .drag_over::<ExternalPaths>(|style, _, _, _| {
            style
                .bg(rgb(theme::accent_subtle()))
                .border_2()
                .border_color(rgb(theme::accent()))
        })
        .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
            this.drop_paths(paths.paths(), dest.clone(), window, cx);
        }))
}

// Whether `a` and `b` are on the same drive, by their prefix (`C:`, or a UNC
// share); compared without case, as Windows does.
fn same_volume(a: &Path, b: &Path) -> bool {
    let prefix = |path: &Path| match path.components().next() {
        Some(std::path::Component::Prefix(prefix)) => {
            Some(prefix.as_os_str().to_string_lossy().to_ascii_lowercase())
        }
        _ => None,
    };
    prefix(a).is_some() && prefix(a) == prefix(b)
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! The OLE drag. `unsafe` is confined to this module: the Shell's drag
    //! calls have no safe binding. The data object is the one Explorer itself builds for a
    //! set of files (`BHID_DataObject` on a shell item array), so every target
    //! that takes a drag from Explorer takes this one, with the same formats.

    use std::path::PathBuf;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::IDataObject;
    use windows::Win32::System::Ole::{
        DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, IDropSource,
    };
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        BHID_DataObject, ILFree, SHCreateShellItemArrayFromIDLists, SHDoDragDrop,
        SHParseDisplayName,
    };
    use windows::core::HSTRING;

    pub fn drag(hwnd: i64, paths: &[PathBuf]) -> windows::core::Result<()> {
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
        let result = (|| {
            for path in paths {
                let mut pidl = std::ptr::null_mut();
                unsafe {
                    SHParseDisplayName(&HSTRING::from(path.as_os_str()), None, &mut pidl, 0, None)?
                };
                pidls.push(pidl);
            }
            let items: Vec<*const ITEMIDLIST> =
                pidls.iter().map(|pidl| pidl.cast_const()).collect();
            unsafe {
                let array = SHCreateShellItemArrayFromIDLists(&items)?;
                let data: IDataObject = array.BindToHandler(None, &BHID_DataObject)?;
                // With no drop source of our own the Shell supplies its own,
                // with the drag image and the cursor Explorer shows.
                SHDoDragDrop(
                    Some(HWND(hwnd as *mut _)),
                    &data,
                    None::<&IDropSource>,
                    DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
                )?;
            }
            Ok(())
        })();
        for pidl in pidls {
            unsafe { ILFree(Some(pidl.cast_const())) };
        }
        result
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::PathBuf;

    pub fn drag(_hwnd: i64, _paths: &[PathBuf]) -> Result<(), std::io::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::same_volume;
    use std::path::Path;

    #[test]
    fn volumes_compare_by_drive_without_case() {
        assert!(same_volume(Path::new(r"C:\a\b.txt"), Path::new(r"c:\x")));
        assert!(!same_volume(Path::new(r"C:\a"), Path::new(r"D:\a")));
        assert!(!same_volume(
            Path::new(r"I:\Meu Drive\a.mp4"),
            Path::new(r"C:\Users")
        ));
    }
}
