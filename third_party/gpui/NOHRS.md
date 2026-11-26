# gpui 0.2.2, patched for Nohrs

A copy of gpui 0.2.2 from crates.io with four changes. The first, in
`src/platform/windows/window.rs`, `DragEnter`: the CF_HDROP `HGLOBAL` is passed
to `DragQueryFileW` as the `HDROP` it is, instead of being dereferenced as a
pointer to one.

The original only worked for moveable memory, which is what OLE hands over when
the drag comes from another process. A drag that starts in Nohrs and passes over
its own window carries the Shell's fixed memory; the dereference read the start
of the DROPFILES block as a handle and `DragQueryFile` crashed with an access
violation in `windows.storage.dll` (seen 04/11/2026, dump `nohrs.exe.19032.dmp`).

When gpui is upgraded: check whether `DragEnter` still dereferences the handle.
If it does not, delete this folder and the `[patch.crates-io]` entry in the
workspace `Cargo.toml`.

The second, in `src/platform/windows/events.rs`, `handle_nc_mouse_down_msg`:
a press on a `WindowControlArea::Drag` (`HTCAPTION`) always reaches
`DefWindowProc`. gpui first dispatches it as a `MouseDown`, and every element
with `track_focus` calls `prevent_default` on one; the result counted as
handled and the message was swallowed, so a window whose root tracks focus
(Nohrs's does) could not be moved by its title bar at all (04/11/2026).

The same goes for the caption buttons (`HTMINBUTTON`, `HTMAXBUTTON`,
`HTCLOSE`), on the press and on the release: the press records the button in
`nc_button_pressed`, the release acts on it, and a "handled" dispatch skipped
both, so minimize, maximize and close did nothing whenever the root under them
saw the press (seen 04/11/2026; it only worked where an element that occludes
hid the root from the pointer).

The third, in `src/taffy.rs`, `to_grid_repeat`: the literals in
`minmax(length(0.0), fr(1.0))` are written `0.0_f32` and `1.0_f32`. Without
the suffix rustc 1.9x warns that the float falls back to `f32` through a
trait bound it does not satisfy, a fallback it is phasing out and will make
an error (rust-lang/rust#154024). Upstream may already have fixed it.

When gpui is upgraded: check whether a caption or caption-button press, or a
caption-button release, still returns `Some(0)` when the dispatch was
"handled" (the lines are marked `NOHRS`).

The fourth, in `src/platform/windows/window.rs`,
`src/platform/windows/platform.rs`, `src/platform.rs` and `src/window.rs`: the
window classes are named by the program instead of by the toolkit (26/11/2026).
`Zed::Window` became `LumaFiles_WinClass`, `Zed::PlatformWindow` became
`LumaFiles_PlatformWindow`, and `WindowOptions::window_class` was added so a
window can name its own class: the program names its windows (see the
`window_class` module in `nohrs-core`), so a script can tell the explorer, the
quick launch panel and the rest apart. The class is
the name Win32 callers and AutoHotkey's `ahk_class` match, and the program is
not the editor whose toolkit this is: a script written for Zed's windows must
not act on these, and one written for these must not act on Zed's. Each class is
registered the first time a window of that class is created; other platforms
ignore the option (marked `Nohrs`).

When gpui is upgraded: check whether `DEFAULT_WINDOW_CLASS_NAME` and
`PLATFORM_WINDOW_CLASS_NAME` still exist and still say `Zed::`, whether
`WindowOptions` still has `window_class`, and rename or port them again.
