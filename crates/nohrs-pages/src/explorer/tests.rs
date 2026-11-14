//! `TestAppContext`-based behavior tests for the explorer page: navigation and
//! history, search lifecycle, preview loading, sorting/filtering, column resize,
//! and status reporting. They also serve as the reference patterns documented in
//! `docs/testing.md` / `CONTRIBUTING.md`: build the view inside a test window,
//! mutate it through `window.update`, drive async work with the GPUI executor
//! timer + `run_until_parked`, and assert state with `window.read_with` — never
//! `smol::Timer` / `tokio` sleeps, which the GPUI scheduler does not track.

// Test fixtures write files directly; the synchronous-fs ban targets app code.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext, ClipboardItem, Entity, TestAppContext, WindowHandle, point, px};
use gpui_component::Root;
use gpui_component::input::InputState;
use gpui_component::resizable::ResizableState;
use nohrs_core::config;
use nohrs_services::fs::listing::FileEntryDto;
use nohrs_services::fs::ops::ConflictResolution;
use nohrs_services::fs::trash::Ledger as TrashLedger;
use nohrs_store::{KvStore, RedbKvStore, StoreLogConfig};

use nohrs_core::config::SplitDirection;
use nohrs_core::shortcuts::Action;

use super::types::{SortKey, StatusLevel, ViewMode};
use super::{ExplorerPage, ExplorerPane};

/// Build a real `ExplorerPane` inside a test window. The sub-entities
/// (`ResizableState`, `InputState`) are window-bound, so the page is
/// constructed in the `add_window` build closure — the canonical pattern for
/// views whose dependencies need a `Window`.
fn new_explorer(cx: &mut TestAppContext) -> WindowHandle<ExplorerPane> {
    // gpui-component installs the `Theme` global and input/list subsystems its
    // widgets rely on; initialize it once before building any window.
    cx.update(gpui_component::init);
    cx.add_window(|window, cx| {
        let resizable = cx.new(|_| ResizableState::default());
        let search_input = cx.new(|cx| InputState::new(window, cx));
        ExplorerPane::new(
            resizable,
            search_input,
            None,
            TrashLedger::KeptByOs,
            cx.focus_handle(),
        )
    })
}

/// Build the split-view container (`ExplorerPage`), which owns its panes. No KV
/// store, so session save/restore is inert.
fn new_explorer_page(cx: &mut TestAppContext) -> WindowHandle<ExplorerPage> {
    cx.update(gpui_component::init);
    cx.add_window(|window, cx| {
        let resizable = cx.new(|_| ResizableState::default());
        ExplorerPage::new(
            resizable,
            None,
            None,
            TrashLedger::KeptByOs,
            false,
            window,
            cx,
        )
    })
}

/// Build the container backed by a `store`, optionally restoring its session,
/// for the persistence round-trip tests.
fn new_explorer_page_with_store(
    cx: &mut TestAppContext,
    store: Arc<dyn KvStore>,
    restore_tabs: bool,
) -> WindowHandle<ExplorerPage> {
    cx.update(gpui_component::init);
    cx.add_window(|window, cx| {
        let resizable = cx.new(|_| ResizableState::default());
        ExplorerPage::new(
            resizable,
            None,
            Some(store),
            TrashLedger::KeptByOs,
            restore_tabs,
            window,
            cx,
        )
    })
}

fn file(name: &str, kind: &str, size: u64) -> FileEntryDto {
    FileEntryDto {
        name: name.to_string(),
        path: format!("/tmp/{name}"),
        kind: kind.to_string(),
        size,
        modified: 0,
    }
}

#[gpui::test]
async fn explorer_starts_with_default_view_state(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .read_with(cx, |page, _cx| {
            assert!(page.sort_key == SortKey::Name);
            assert!(page.sort_asc);
            assert!(!page.show_hidden);
            assert!(page.view_mode == ViewMode::List);
        })
        .unwrap();
}

#[gpui::test]
async fn set_sort_key_toggles_direction_then_resets(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, cx| {
            // Re-selecting the active key flips the direction...
            page.set_sort_key(SortKey::Name, cx);
            assert!(page.sort_key == SortKey::Name);
            assert!(!page.sort_asc);
            // ...and a new key resets to ascending.
            page.set_sort_key(SortKey::Size, cx);
            assert!(page.sort_key == SortKey::Size);
            assert!(page.sort_asc);
        })
        .unwrap();
}

#[gpui::test]
async fn apply_config_ui_reflects_ui_section(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, cx| {
            let ui = config::Ui {
                app_name: String::new(),
                language: "auto".to_string(),
                default_sort: config::SortOrder::Size,
                default_sort_descending: false,
                show_hidden: true,
                show_quick_access: true,
                icon_pack: "default".to_string(),
                font_family: String::new(),
                name_lines: 0,
                scroll_smoothness: 50,
                audio_waveforms: false,
                preview_autoplay: false,
                welcome_done: false,
                hover_audio: false,
                hover_audio_delay_ms: 400,
                file_type_colors: Vec::new(),
            };
            page.apply_config_ui(&ui, cx);
            assert!(page.show_hidden);
            assert!(page.sort_key == SortKey::Size);
        })
        .unwrap();
}

#[gpui::test]
async fn grouping_counts_headers_when_scrolling_to_an_entry(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.entries = vec![
                file("b.png", "file", 1),
                file("docs", "dir", 0),
                file("a.png", "file", 1),
            ];
            page.set_group_by(super::grouping::GroupBy::Kind);
            let names: Vec<&str> = page.filtered_entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["docs", "a.png", "b.png"]);
            assert_eq!(page.groups().len(), 2);
            // List: column header, "Pastas", docs, "Imagens", a.png, b.png.
            assert_eq!(page.visual_rows_of(2).0, 5);
            // Search results replace the entries without grouping them.
            page.search_results = Some(Vec::new());
            assert!(page.groups().is_empty());
        })
        .unwrap();
}

#[gpui::test]
async fn config_reload_keeps_a_sort_chosen_in_the_session(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, cx| {
            let ui = config::Ui {
                app_name: String::new(),
                language: "auto".to_string(),
                default_sort: config::SortOrder::Name,
                default_sort_descending: false,
                show_hidden: false,
                show_quick_access: false,
                icon_pack: "default".to_string(),
                font_family: String::new(),
                name_lines: 0,
                scroll_smoothness: 50,
                audio_waveforms: false,
                preview_autoplay: false,
                welcome_done: false,
                hover_audio: false,
                hover_audio_delay_ms: 400,
                file_type_colors: Vec::new(),
            };
            page.apply_config_ui(&ui, cx);
            page.set_sort(SortKey::Modified, false, cx);
            // Another section of the file changed (the theme, from the menu).
            page.apply_config_ui(&ui, cx);
            assert!(page.sort_key == SortKey::Modified);
            assert!(!page.sort_asc);
            // The default sort itself changed in the file: that wins.
            let sorted_by_size = config::Ui {
                default_sort: config::SortOrder::Size,
                ..ui
            };
            page.apply_config_ui(&sorted_by_size, cx);
            assert!(page.sort_key == SortKey::Size);
        })
        .unwrap();
}

#[gpui::test]
async fn apply_filter_hides_dotfiles_until_enabled(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.entries = vec![
                file(".hidden", "file", 1),
                file("visible.txt", "file", 2),
                file("docs", "dir", 0),
            ];

            page.show_hidden = false;
            page.apply_filter();
            assert!(
                page.filtered_entries
                    .iter()
                    .all(|entry| entry.name != ".hidden")
            );
            assert_eq!(page.filtered_entries.len(), 2);

            page.show_hidden = true;
            page.apply_filter();
            assert!(
                page.filtered_entries
                    .iter()
                    .any(|entry| entry.name == ".hidden")
            );
            assert_eq!(page.filtered_entries.len(), 3);
        })
        .unwrap();
}

#[gpui::test]
async fn status_message_set_and_clear(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.set_status(StatusLevel::Error, "boom");
            assert_eq!(
                page.status_for_footer(),
                Some(("boom".to_string(), StatusLevel::Error))
            );
            page.clear_status();
            assert_eq!(page.status_for_footer(), None);
        })
        .unwrap();
}

#[gpui::test]
async fn open_preview_loads_text_off_thread(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("note.txt");
    std::fs::write(&file_path, "hello, gpui test world\n").unwrap();
    let path = file_path.to_string_lossy().to_string();

    let window = new_explorer(cx);
    window
        .update(cx, |page, window, cx| {
            page.open_preview(path.clone(), window, cx);
        })
        .unwrap();

    // `open_preview` reads the file on the background executor and applies the
    // result on the foreground thread. Drive both to completion: the executor
    // timer advances the test clock, then `run_until_parked` flushes the spawned
    // tasks. (Using `smol`/`tokio` sleeps here would not be tracked by the GPUI
    // scheduler and `run_until_parked` would return early.)
    cx.background_executor
        .timer(Duration::from_millis(50))
        .await;
    cx.run_until_parked();

    window
        .read_with(cx, |page, _cx| {
            assert_eq!(
                page.preview_text.as_deref(),
                Some("hello, gpui test world\n")
            );
            assert!(page.preview_editor.is_some());
        })
        .unwrap();
}

// The preview follows the arrow keys, as Explorer's does, but only once they
// stop: holding Down through a folder reads the file it stops on, not every
// file it passes.
#[gpui::test]
async fn the_preview_follows_the_arrow_keys_once_they_stop(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in [("a.txt", "A"), ("b.txt", "B"), ("c.txt", "C")] {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let window = open_pane_at(cx, dir.path());
    let half = super::preview::PREVIEW_FOLLOW_DELAY / 2;
    window
        .update(cx, |pane, window, cx| {
            pane.select_single(0);
            pane.move_active_following(1, false, window, cx);
        })
        .unwrap();
    cx.executor().advance_clock(half);
    cx.run_until_parked();
    window
        .update(cx, |pane, window, cx| {
            assert_eq!(pane.preview_path, None, "nothing is read while the keys are moving");
            pane.move_active_following(1, false, window, cx);
        })
        .unwrap();
    cx.executor().advance_clock(half);
    cx.run_until_parked();
    window
        .read_with(cx, |pane, _cx| {
            assert_eq!(pane.preview_path, None, "each key press starts the wait over");
        })
        .unwrap();
    cx.executor().advance_clock(super::preview::PREVIEW_FOLLOW_DELAY);
    cx.run_until_parked();
    window
        .read_with(cx, |pane, _cx| {
            let c = dir.path().join("c.txt").to_string_lossy().into_owned();
            assert_eq!(pane.preview_path.as_deref(), Some(c.as_str()));
            assert_eq!(pane.preview_text.as_deref(), Some("C"));
        })
        .unwrap();
}

// A press on an item the preview lists becomes a drag out only once the
// pointer has moved past the threshold with the button held, as in the
// listing; a release before that stays the click that opens the item.
#[gpui::test]
async fn a_press_on_a_preview_item_becomes_a_drag_past_the_threshold(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    let moved = |x: f32, y: f32, held: bool| gpui::MouseMoveEvent {
        position: gpui::point(gpui::px(x), gpui::px(y)),
        pressed_button: held.then_some(gpui::MouseButton::Left),
        modifiers: Default::default(),
    };
    window
        .update(cx, |pane, _window, _cx| {
            let photo = r"C:\pasta\foto.jpg";
            pane.press_preview_item(photo, gpui::point(gpui::px(100.0), gpui::px(100.0)));
            assert!(pane.preview_drag_ready(&moved(103.0, 102.0, true)).is_none(), "a tremble is not a drag");
            assert!(pane.preview_drag.is_some(), "and the press is still there");
            let dragged = pane.preview_drag_ready(&moved(100.0, 110.0, true)).expect("past the threshold");
            assert_eq!(dragged.path(), std::path::Path::new(photo));
            assert!(pane.preview_drag.is_none(), "a drag starts once");

            // The button came up somewhere the release was not seen.
            pane.press_preview_item(photo, gpui::point(gpui::px(100.0), gpui::px(100.0)));
            assert!(pane.preview_drag_ready(&moved(130.0, 100.0, false)).is_none());
            assert!(pane.preview_drag.is_none());

            pane.press_preview_item(photo, gpui::point(gpui::px(100.0), gpui::px(100.0)));
            pane.release_preview_item();
            assert!(pane.preview_drag_ready(&moved(130.0, 100.0, true)).is_none(), "released: a click");
        })
        .unwrap();
}

// After a drag out of the folder the preview lists — a move, if it landed in
// Explorer on the same drive — the listing is read again where it was.
#[gpui::test]
async fn the_preview_listing_is_read_again_after_a_drag(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("fotos");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("a.jpg"), "a").unwrap();
    std::fs::write(folder.join("b.jpg"), "b").unwrap();
    let folder_path = folder.to_string_lossy().into_owned();
    let window = new_explorer(cx);
    window
        .update(cx, |pane, window, cx| pane.open_preview(folder_path.clone(), window, cx))
        .unwrap();
    cx.run_until_parked();
    let names = |pane: &ExplorerPane| -> Vec<String> {
        pane.preview_listing
            .as_ref()
            .map(|listing| listing.items.iter().map(|item| item.name.clone()).collect())
            .unwrap_or_default()
    };
    window
        .read_with(cx, |pane, _cx| assert_eq!(names(pane), ["a.jpg", "b.jpg"]))
        .unwrap();

    // Moved away by whoever took the drop.
    std::fs::remove_file(folder.join("a.jpg")).unwrap();
    window
        .update(cx, |pane, _window, cx| pane.refresh_preview_listing(cx))
        .unwrap();
    cx.run_until_parked();
    window
        .read_with(cx, |pane, _cx| {
            assert_eq!(names(pane), ["b.jpg"]);
            assert_eq!(pane.preview_path.as_deref(), Some(folder_path.as_str()), "the same folder, still previewed");
        })
        .unwrap();
}

// A fitted image is drawn from the Shell's picture at the pane's size; the
// file is decoded whole only where that is what is needed. A 48 MP photo
// decoded whole took the process from ~122 to ~677 MB (PERFORMANCE.md,
// passada 7).
#[gpui::test]
async fn a_fitted_image_is_not_decoded_whole(cx: &mut TestAppContext) {
    use super::view::preview::fitted_picture;
    let window = new_explorer(cx);
    window
        .update(cx, |page, window, cx| {
            let photo = r"C:\nao-existe\foto.jpg";
            assert!(fitted_picture(page, photo, window, cx).is_some(), "fitted: the Shell's picture");
            page.preview_zoom = 2.0;
            assert!(fitted_picture(page, photo, window, cx).is_none(), "zoomed in: the whole file");
            page.preview_zoom = 1.0;
            assert!(
                fitted_picture(page, r"C:\nao-existe\anima.gif", window, cx).is_none(),
                "animated: the Shell's picture is a single frame"
            );
        })
        .unwrap();
}

#[test]
fn a_fitted_image_is_asked_for_in_steps_that_cover_the_pane() {
    use super::view::preview::fitted_edge;
    // The pane's longer side in device pixels, rounded up to a step.
    assert_eq!(fitted_edge(340.0, 400.0, 1.0), 512);
    assert_eq!(fitted_edge(340.0, 789.0, 1.0), 1024);
    assert_eq!(fitted_edge(340.0, 789.0, 1.5), 2048);
    // Past the top step, the top step.
    assert_eq!(fitted_edge(340.0, 2160.0, 2.0), 2048);
}

#[gpui::test]
async fn a_hidden_preview_does_not_follow_the_arrow_keys(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "A").unwrap();
    std::fs::write(dir.path().join("b.txt"), "B").unwrap();
    let window = open_pane_at(cx, dir.path());
    window
        .update(cx, |pane, window, cx| {
            pane.preview_visible = false;
            pane.select_single(0);
            pane.move_active_following(1, false, window, cx);
        })
        .unwrap();
    cx.executor().advance_clock(super::preview::PREVIEW_FOLLOW_DELAY * 2);
    cx.run_until_parked();
    window
        .read_with(cx, |pane, _cx| assert_eq!(pane.preview_path, None))
        .unwrap();
}

// A click while the preview waits on the arrow keys wins: the click previews
// its row at once, and the wait that ends after it must not take the preview
// back to the row the keys were on.
#[gpui::test]
async fn a_click_during_the_wait_is_not_undone_by_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "A").unwrap();
    std::fs::write(dir.path().join("b.txt"), "B").unwrap();
    let window = open_pane_at(cx, dir.path());
    let a = dir.path().join("a.txt").to_string_lossy().into_owned();
    window
        .update(cx, |pane, window, cx| {
            pane.select_single(0);
            pane.move_active_following(1, false, window, cx);
            // The click on a.txt, as `row.rs` handles it.
            pane.select_single(0);
            pane.open_preview(a.clone(), window, cx);
        })
        .unwrap();
    cx.executor().advance_clock(super::preview::PREVIEW_FOLLOW_DELAY * 2);
    cx.run_until_parked();
    window
        .read_with(cx, |pane, _cx| {
            assert_eq!(pane.preview_path.as_deref(), Some(a.as_str()));
            assert_eq!(pane.preview_text.as_deref(), Some("A"));
        })
        .unwrap();
}

#[gpui::test]
async fn navigation_history_supports_back_forward_and_truncation(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::create_dir(root.join("other")).unwrap();
    std::fs::write(root.join("a.txt"), "x").unwrap();
    let root_s = root.to_string_lossy().to_string();
    let sub_s = root.join("sub").to_string_lossy().to_string();
    let other_s = root.join("other").to_string_lossy().to_string();

    let window = new_explorer(cx);
    window
        .update(cx, |page, window, cx| {
            page.change_dir(root_s.clone(), window, cx);
            assert!(page.entries.iter().any(|e| e.name == "a.txt"));
            assert!(page.entries.iter().any(|e| e.name == "sub"));

            // Navigating to the current directory is a no-op.
            let len = page.history.len();
            page.change_dir(root_s.clone(), window, cx);
            assert_eq!(page.history.len(), len);

            page.change_dir(sub_s.clone(), window, cx);
            assert_eq!(page.cwd, sub_s);

            page.go_back(window, cx);
            assert_eq!(page.cwd, root_s);
            page.go_forward(window, cx);
            assert_eq!(page.cwd, sub_s);

            // Back, then a new navigation drops the forward ("sub") entry.
            page.go_back(window, cx);
            page.change_dir(other_s.clone(), window, cx);
            assert_eq!(page.cwd, other_s);
            let cwd = page.cwd.clone();
            page.go_forward(window, cx);
            assert_eq!(page.cwd, cwd, "no forward history remains");
        })
        .unwrap();
}

#[gpui::test]
async fn activate_entry_routes_by_kind(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("folder")).unwrap();
    std::fs::write(root.join("file.txt"), "hi").unwrap();
    let folder = FileEntryDto {
        name: "folder".into(),
        path: root.join("folder").to_string_lossy().to_string(),
        kind: "dir".into(),
        size: 0,
        modified: 0,
    };
    let doc = FileEntryDto {
        name: "file.txt".into(),
        path: root.join("file.txt").to_string_lossy().to_string(),
        kind: "file".into(),
        size: 2,
        modified: 0,
    };

    let window = new_explorer(cx);
    window
        .update(cx, |page, window, cx| {
            page.activate_entry(folder.clone(), window, cx);
            assert_eq!(page.cwd, folder.path, "activating a dir navigates into it");
        })
        .unwrap();
    window
        .update(cx, |page, window, cx| {
            page.activate_entry(doc.clone(), window, cx);
            // A file opens in its default program, as in Explorer; the preview
            // is for selection, not activation.
            let opened = super::navigation::OPENED_WITH_DEFAULT_APP
                .with(|opened| opened.borrow().clone());
            assert_eq!(opened, vec![doc.path.clone()]);
            assert_eq!(page.cwd, folder.path, "activating a file does not navigate");
        })
        .unwrap();
    cx.run_until_parked();
}

#[gpui::test]
async fn activating_an_archive_opens_it_as_a_folder(cx: &mut TestAppContext) {
    use std::io::Write as _;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let path = root.join("pacote.zip");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("leiame.txt", options).unwrap();
        zip.write_all(b"oi").unwrap();
        zip.add_directory("docs/", options).unwrap();
        zip.finish().unwrap();
    }
    let archive = path.to_string_lossy().to_string();
    let entry = FileEntryDto {
        name: "pacote.zip".into(),
        path: archive.clone(),
        kind: "file".into(),
        size: 100,
        modified: 0,
    };

    let window = new_explorer(cx);
    window
        .update(cx, |page, window, cx| {
            page.activate_entry(entry.clone(), window, cx);
            assert_eq!(
                page.cwd,
                super::archive::location(&archive, ""),
                "an archive opens like a folder instead of in another program"
            );
            let opened_for_program = super::navigation::OPENED_WITH_DEFAULT_APP.with(|opened| opened.borrow().clone());
            assert!(opened_for_program.is_empty(), "nothing was handed to another program");
        })
        .unwrap();
    cx.run_until_parked();

    // The listing then shows what is inside it, folders first.
    window
        .read_with(cx, |page, _cx| {
            let names: Vec<String> = page.filtered_entries.iter().map(|entry| entry.name.clone()).collect();
            assert_eq!(names, ["docs", "leiame.txt"], "{names:?}");
        })
        .unwrap();

    // Up from the archive's root leads to the folder holding it.
    window
        .update(cx, |page, window, cx| {
            page.go_up(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .read_with(cx, |page, _cx| {
            assert_eq!(page.cwd, root.to_string_lossy().to_string());
        })
        .unwrap();
}

#[gpui::test]
async fn search_visibility_open_close_toggle(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, window, cx| {
            assert!(!page.search_visible);
            page.open_search(window, cx);
            assert!(page.search_visible);
            page.close_search(window, cx);
            assert!(!page.search_visible);
            page.toggle_search(window, cx);
            assert!(page.search_visible);
            page.toggle_search(window, cx);
            assert!(!page.search_visible);
        })
        .unwrap();
}

#[gpui::test]
async fn trigger_search_empty_clears_and_no_service_degrades(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, window, cx| {
            // Empty query clears results without raising an error.
            page.search_query = String::new();
            page.trigger_search(window, cx);
            assert!(page.search_results.is_none());
            assert!(page.status_for_footer().is_none());

            // A real query with no search service falls back to name filtering and
            // surfaces an error status.
            page.search_query = "needle".into();
            page.trigger_search(window, cx);
            assert!(page.search_results.is_none());
            let (_, level) = page.status_for_footer().expect("degraded status set");
            assert_eq!(level, StatusLevel::Error);
        })
        .unwrap();
}

/// Lands a view switch at once. `set_view_mode` glides there over the next
/// frames, and a test has no frames.
fn settle_zoom(page: &mut ExplorerPane, cx: &mut gpui::Context<ExplorerPane>) {
    page.set_zoom(page.zoom_target, cx);
}

#[gpui::test]
async fn view_mode_and_match_toggles_flip_state(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, cx| {
            assert!(page.view_mode == ViewMode::List);
            page.set_view_mode(ViewMode::Grid, cx);
            settle_zoom(page, cx);
            assert!(page.view_mode == ViewMode::Grid);

            assert!(!page.match_case && !page.use_regex && !page.match_whole_word);
            page.toggle_match_case(cx);
            page.toggle_use_regex(cx);
            page.toggle_match_whole_word(cx);
            assert!(page.match_case && page.use_regex && page.match_whole_word);
        })
        .unwrap();
}

#[gpui::test]
async fn column_resize_applies_delta_with_min_clamp(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            let start = page.col_name_width;
            page.start_column_resize(0, point(px(100.0), px(0.0)));
            page.update_column_resize(point(px(150.0), px(0.0)));
            assert_eq!(page.col_name_width, start + 50.0);

            // A large negative drag clamps to the minimum column width.
            page.update_column_resize(point(px(-10_000.0), px(0.0)));
            assert_eq!(page.col_name_width, nohrs_core::config::MIN_COLUMN_WIDTH);

            page.stop_column_resize();
            assert!(page.resizing_column.is_none());
        })
        .unwrap();
}

#[gpui::test]
async fn item_sizes_track_entries_and_table_width_sums_columns(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.filtered_entries = vec![file("a", "file", 1), file("b", "file", 2)];
            page.update_item_sizes();
            assert_eq!(page.item_sizes.len(), 2);

            let expected = page.col_name_width
                + page.col_type_width
                + page.col_size_width
                + page.col_modified_width
                + page.col_action_width
                + nohrs_core::config::TABLE_HORIZONTAL_PADDING;
            assert_eq!(page.total_table_width(), expected);

            page.record_click(1, 2);
            assert!(page.last_click_info.is_some());
        })
        .unwrap();
}

#[gpui::test]
async fn reload_reports_error_for_unreadable_dir(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, cx| {
            page.cwd = "/nonexistent/nohrs/dir".to_string();
            page.loaded = false;
            page.reload(cx);
            let (_, level) = page.status_for_footer().expect("error status set");
            assert_eq!(level, StatusLevel::Error);
            assert!(page.entries.is_empty());
        })
        .unwrap();
}

#[gpui::test]
async fn explorer_page_starts_with_single_pane(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .read_with(cx, |page, _cx| {
            assert_eq!(page.pane_count(), 1);
            assert_eq!(page.active_index(), 0);
            assert!(!page.is_synced());
        })
        .unwrap();
}

#[gpui::test]
async fn split_opens_second_pane_then_reorients(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.split(SplitDirection::Vertical, window, cx);
            assert_eq!(page.pane_count(), 2, "first split opens a second pane");
            assert_eq!(page.active_index(), 1, "the new pane becomes active");
            assert_eq!(page.direction(), SplitDirection::Vertical);

            // The opposite split shortcut flips orientation without adding a pane
            // (2-way cap, §3.1).
            page.split(SplitDirection::Horizontal, window, cx);
            assert_eq!(page.pane_count(), 2);
            assert_eq!(page.direction(), SplitDirection::Horizontal);
        })
        .unwrap();
}

#[gpui::test]
async fn the_split_key_opens_and_closes_the_second_pane(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.toggle_split(SplitDirection::Vertical, window, cx);
            assert_eq!(page.pane_count(), 2, "the key opens the split");
            assert_eq!(page.active_index(), 1, "the new pane is the one in use");

            // The same key closes the split, keeping the pane being worked in.
            page.toggle_split(SplitDirection::Vertical, window, cx);
            assert_eq!(page.pane_count(), 1, "the key closes the split");
            assert_eq!(page.active_index(), 0, "the pane in use is the one kept");

            // And it opens again, so the key really is a toggle.
            page.toggle_split(SplitDirection::Vertical, window, cx);
            assert_eq!(page.pane_count(), 2);
        })
        .unwrap();
}

// Alt+Q: the same key shows and hides the preview beside the listing.
#[gpui::test]
async fn the_preview_key_hides_and_shows_the_preview(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |pane, window, cx| {
            assert!(pane.preview_visible, "open to begin with");
            assert!(pane.run_shortcut(Action::TogglePreview, window, cx), "the pane handles the key itself");
            assert!(!pane.preview_visible, "the key hides the preview");
            pane.run_shortcut(Action::TogglePreview, window, cx);
            assert!(pane.preview_visible, "and shows it again");
        })
        .unwrap();
}

#[gpui::test]
async fn close_pane_keeps_at_least_one(cx: &mut TestAppContext) {    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.split(SplitDirection::Vertical, window, cx);
            assert_eq!(page.pane_count(), 2);

            page.close_pane(1, window, cx);
            assert_eq!(page.pane_count(), 1);
            assert_eq!(page.active_index(), 0);

            // The final pane can never be closed.
            page.close_pane(0, window, cx);
            assert_eq!(page.pane_count(), 1);
        })
        .unwrap();
}

#[gpui::test]
async fn set_active_selects_pane_in_bounds(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.split(SplitDirection::Vertical, window, cx);
            page.set_active(0, window, cx);
            assert_eq!(page.active_index(), 0);

            // Out-of-range indices are ignored rather than panicking.
            page.set_active(5, window, cx);
            assert_eq!(page.active_index(), 0);
        })
        .unwrap();
}

#[gpui::test]
async fn panes_navigate_independently_by_default(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("left")).unwrap();
    std::fs::create_dir(root.join("right")).unwrap();
    let left = root.join("left").to_string_lossy().to_string();
    let right = root.join("right").to_string_lossy().to_string();

    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.split(SplitDirection::Vertical, window, cx);
            let pane0 = page.pane(0);
            let pane1 = page.pane(1);
            pane0.update(cx, |pane, cx| pane.change_dir(left.clone(), window, cx));
            pane1.update(cx, |pane, cx| pane.change_dir(right.clone(), window, cx));
        })
        .unwrap();
    cx.run_until_parked();
    window
        .read_with(cx, |page, cx| {
            assert_eq!(page.pane_cwd(0, cx).as_deref(), Some(left.as_str()));
            assert_eq!(page.pane_cwd(1, cx).as_deref(), Some(right.as_str()));
        })
        .unwrap();
}

#[gpui::test]
async fn synced_panes_mirror_navigation(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("shared")).unwrap();
    let shared = root.join("shared").to_string_lossy().to_string();

    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.split(SplitDirection::Vertical, window, cx);
            let synced = config::Explorer {
                split_direction: SplitDirection::Vertical,
                synced_panes: true,
                restore_tabs: true,
                reveal_from_other_apps: true,
            };
            page.apply_config_explorer(&synced, cx);
            assert!(page.is_synced());

            let pane0 = page.pane(0);
            pane0.update(cx, |pane, cx| pane.change_dir(shared.clone(), window, cx));
        })
        .unwrap();
    // Navigation events are delivered on the next effect flush; mirror happens there.
    cx.run_until_parked();
    window
        .read_with(cx, |page, cx| {
            assert_eq!(page.pane_cwd(0, cx).as_deref(), Some(shared.as_str()));
            assert_eq!(
                page.pane_cwd(1, cx).as_deref(),
                Some(shared.as_str()),
                "sibling pane mirrors the active pane's path"
            );
        })
        .unwrap();
}

#[gpui::test]
async fn synced_navigation_clears_stale_search_state(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("shared")).unwrap();
    let shared = root.join("shared").to_string_lossy().to_string();

    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.split(SplitDirection::Vertical, window, cx);
            let synced = config::Explorer {
                split_direction: SplitDirection::Vertical,
                synced_panes: true,
                restore_tabs: true,
                reveal_from_other_apps: true,
            };
            page.apply_config_explorer(&synced, cx);

            // Leave the mirrored pane with a stale filter and results from a
            // prior search so the reset is actually exercised (not vacuous).
            page.pane(1).update(cx, |pane, _cx| {
                pane.search_query = "stale".to_string();
                pane.search_visible = true;
                pane.search_results = Some(Vec::new());
            });
            page.pane(0)
                .update(cx, |pane, cx| pane.change_dir(shared.clone(), window, cx));
        })
        .unwrap();
    cx.run_until_parked();
    window
        .read_with(cx, |page, cx| {
            let pane1 = page.pane(1);
            let pane1 = pane1.read(cx);
            assert!(
                pane1.search_query.is_empty(),
                "stale filter cleared on sync"
            );
            assert!(!pane1.search_visible);
            assert!(pane1.search_results.is_none());
        })
        .unwrap();
}

#[gpui::test]
async fn split_pane_inherits_applied_ui_config(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            let ui = config::Ui {
                app_name: String::new(),
                language: "auto".to_string(),
                default_sort: config::SortOrder::Size,
                default_sort_descending: false,
                show_hidden: true,
                show_quick_access: true,
                icon_pack: "default".to_string(),
                font_family: String::new(),
                name_lines: 0,
                scroll_smoothness: 50,
                audio_waveforms: false,
                preview_autoplay: false,
                welcome_done: false,
                hover_audio: false,
                hover_audio_delay_ms: 400,
                file_type_colors: Vec::new(),
            };
            page.apply_config_ui(&ui, cx);

            // A pane opened by a later split should pick up the applied config
            // rather than reverting to pane defaults.
            page.split(SplitDirection::Vertical, window, cx);
            let pane1 = page.pane(1);
            let pane1 = pane1.read(cx);
            assert!(pane1.show_hidden, "new pane inherits show_hidden");
            assert!(pane1.sort_key == SortKey::Size, "new pane inherits sort");
        })
        .unwrap();
}

#[gpui::test]
async fn a_window_opens_with_the_preview_it_was_left_with(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, _window, cx| {
            assert!(page.pane(0).read(cx).preview_visible, "shown until a config says otherwise");
            // What the last run wrote into `[layout]` when the preview was
            // closed before the window was.
            let mut layout = config::Layout::default();
            layout.preview_visible = false;
            page.apply_config_layout(&layout, cx);
            assert!(
                !page.pane(0).read(cx).preview_visible,
                "the pane already open follows the saved state"
            );
            // And shown again, when that is what was left.
            let layout = config::Layout::default();
            page.apply_config_layout(&layout, cx);
            assert!(page.pane(0).read(cx).preview_visible);
        })
        .unwrap();
}

#[gpui::test]
async fn root_pane_shows_sidebar_but_split_pane_hides_it(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            assert!(
                page.pane(0).read(cx).sidebar_visible,
                "the root pane shows its sidebar by default (§2)"
            );

            page.split(SplitDirection::Vertical, window, cx);
            assert!(
                !page.pane(1).read(cx).sidebar_visible,
                "a pane opened by a split starts with the sidebar collapsed"
            );
            assert!(
                !page.pane(1).read(cx).preview_visible,
                "and with its preview closed"
            );
            assert!(page.pane(0).read(cx).preview_visible, "the original pane keeps its preview");
            assert!(
                page.pane(0).read(cx).sidebar_visible,
                "splitting does not disturb the original pane's sidebar"
            );
        })
        .unwrap();
}

#[gpui::test]
async fn toggle_sidebar_is_independent_per_pane(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.split(SplitDirection::Vertical, window, cx);
            let pane0 = page.pane(0); // sidebar visible (root)
            let pane1 = page.pane(1); // sidebar hidden (split)

            pane1.update(cx, |pane, cx| pane.toggle_sidebar(cx));
            assert!(pane1.read(cx).sidebar_visible, "pane 1 toggled on");
            assert!(
                pane0.read(cx).sidebar_visible,
                "toggling pane 1 leaves pane 0 untouched"
            );

            pane0.update(cx, |pane, cx| pane.toggle_sidebar(cx));
            assert!(!pane0.read(cx).sidebar_visible, "pane 0 toggled off");
            assert!(pane1.read(cx).sidebar_visible, "pane 1 unchanged");
        })
        .unwrap();
}

#[gpui::test]
async fn close_pane_keeps_subscriptions_aligned(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    let shared = shared.to_string_lossy().to_string();

    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.split(SplitDirection::Vertical, window, cx);
            assert_eq!(page.pane_count(), 2);
            assert_eq!(page.subscription_count(), 2, "one subscription per pane");

            // Closing index 0 must drop the subscription at the same index so the
            // Vec stays aligned with the surviving pane.
            page.close_pane(0, window, cx);
            assert_eq!(page.pane_count(), 1);
            assert_eq!(page.subscription_count(), 1);

            // Reuse the container: split again and enable sync. If the
            // subscription Vec had desynced, the mirror below would target the
            // wrong pane (or none).
            page.split(SplitDirection::Vertical, window, cx);
            assert_eq!(page.subscription_count(), 2);
            let synced = config::Explorer {
                split_direction: SplitDirection::Vertical,
                synced_panes: true,
                restore_tabs: true,
                reveal_from_other_apps: true,
            };
            page.apply_config_explorer(&synced, cx);
            page.pane(0)
                .update(cx, |pane, cx| pane.change_dir(shared.clone(), window, cx));
        })
        .unwrap();
    cx.run_until_parked();
    window
        .read_with(cx, |page, cx| {
            assert_eq!(
                page.pane_cwd(1, cx).as_deref(),
                Some(shared.as_str()),
                "the sibling still mirrors after a close/split cycle"
            );
        })
        .unwrap();
}

#[gpui::test]
async fn new_tab_appends_and_activates(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            assert_eq!(page.tab_count(0), 1);
            page.test_new_tab(0, window, cx);
            assert_eq!(page.tab_count(0), 2, "a new tab is appended");
            assert_eq!(page.active_tab(0), 1, "the new tab becomes active");
            // A new tab defaults to home (§4).
            let home = super::page::home_dir();
            assert_eq!(page.tab_cwd(0, 1, cx).as_deref(), Some(home.as_str()));
            assert!(std::path::Path::new(&home).is_dir(), "home is a real folder");

            // Switching back to the first tab makes it active again.
            page.test_activate_tab(0, 0, window, cx);
            assert_eq!(page.active_tab(0), 0);
        })
        .unwrap();
}

#[gpui::test]
async fn close_tab_keeps_pane_when_others_remain(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.test_new_tab(0, window, cx);
            page.test_new_tab(0, window, cx);
            assert_eq!(page.tab_count(0), 3);
            page.test_close_tab(0, 1, window, cx);
            assert_eq!(
                page.tab_count(0),
                2,
                "closing a non-last tab keeps the pane"
            );
            assert_eq!(page.pane_count(), 1);
        })
        .unwrap();
}

#[gpui::test]
async fn closing_last_tab_closes_its_pane(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            page.split(SplitDirection::Vertical, window, cx);
            assert_eq!(page.pane_count(), 2);
            assert_eq!(page.tab_count(1), 1);

            // Closing pane 1's only tab closes the pane (§3.1 / §4).
            page.test_close_tab(1, 0, window, cx);
            assert_eq!(page.pane_count(), 1);

            // The final tab of the final pane is never closed.
            page.test_close_tab(0, 0, window, cx);
            assert_eq!(page.pane_count(), 1);
            assert_eq!(page.tab_count(0), 1);
        })
        .unwrap();
}

#[gpui::test]
async fn reorder_tab_swaps_order_and_follows_active(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let first = first.to_string_lossy().to_string();
    let second = second.to_string_lossy().to_string();

    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            // Tab 0 -> first, tab 1 (new, active) -> second.
            page.pane(0)
                .update(cx, |pane, _cx| pane.cwd = first.clone());
            page.test_new_tab(0, window, cx);
            page.pane(0)
                .update(cx, |pane, _cx| pane.cwd = second.clone());
            assert_eq!(page.tab_cwd(0, 0, cx).as_deref(), Some(first.as_str()));
            assert_eq!(page.tab_cwd(0, 1, cx).as_deref(), Some(second.as_str()));
            assert_eq!(page.active_tab(0), 1);

            // Drag tab 0 to position 1; the active tab (second) follows its entity.
            page.test_reorder_tab(0, 0, 1, cx);
            assert_eq!(page.tab_cwd(0, 0, cx).as_deref(), Some(second.as_str()));
            assert_eq!(page.tab_cwd(0, 1, cx).as_deref(), Some(first.as_str()));
            assert_eq!(page.active_tab(0), 0, "the active tab follows the reorder");
        })
        .unwrap();
}

#[gpui::test]
async fn subscriptions_track_every_tab(cx: &mut TestAppContext) {
    let window = new_explorer_page(cx);
    window
        .update(cx, |page, window, cx| {
            assert_eq!(page.subscription_count(), 1);
            page.test_new_tab(0, window, cx);
            assert_eq!(page.subscription_count(), 2, "one subscription per tab");
            page.split(SplitDirection::Vertical, window, cx);
            assert_eq!(page.subscription_count(), 3, "the split's pane adds a tab");
            page.test_close_tab(0, 1, window, cx);
            assert_eq!(
                page.subscription_count(),
                2,
                "closing a tab drops its subscription"
            );
        })
        .unwrap();
}

#[gpui::test]
async fn tabs_round_trip_through_the_store(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let alpha = dir.path().join("alpha");
    std::fs::create_dir(&alpha).unwrap();
    let alpha = alpha.to_string_lossy().to_string();
    let store: Arc<dyn KvStore> =
        Arc::new(RedbKvStore::open_in_memory(&StoreLogConfig::default()).unwrap());

    // First session: open a second tab and navigate it, then let the debounced
    // save fire and write to the store.
    let window = new_explorer_page_with_store(cx, store.clone(), true);
    window
        .update(cx, |page, window, cx| {
            page.test_new_tab(0, window, cx);
            page.pane(0)
                .update(cx, |pane, cx| pane.change_dir(alpha.clone(), window, cx));
        })
        .unwrap();
    // Drive the debounce timer past `SAVE_DEBOUNCE`, then flush the spawned save.
    cx.background_executor
        .timer(Duration::from_millis(600))
        .await;
    cx.run_until_parked();

    // Second session with the same store restores both tabs.
    let restored = new_explorer_page_with_store(cx, store.clone(), true);
    restored
        .read_with(cx, |page, cx| {
            assert_eq!(page.tab_count(0), 2, "both tabs are restored");
            assert_eq!(page.tab_cwd(0, 1, cx).as_deref(), Some(alpha.as_str()));
        })
        .unwrap();
}

#[gpui::test]
async fn restore_disabled_ignores_saved_session(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let beta = dir.path().join("beta");
    std::fs::create_dir(&beta).unwrap();
    let beta = beta.to_string_lossy().to_string();
    let store: Arc<dyn KvStore> =
        Arc::new(RedbKvStore::open_in_memory(&StoreLogConfig::default()).unwrap());

    let window = new_explorer_page_with_store(cx, store.clone(), true);
    window
        .update(cx, |page, window, cx| {
            page.test_new_tab(0, window, cx);
            page.pane(0)
                .update(cx, |pane, cx| pane.change_dir(beta.clone(), window, cx));
        })
        .unwrap();
    cx.background_executor
        .timer(Duration::from_millis(600))
        .await;
    cx.run_until_parked();

    // With restore disabled, a fresh session starts with a single default tab.
    let fresh = new_explorer_page_with_store(cx, store.clone(), false);
    fresh
        .read_with(cx, |page, _cx| {
            assert_eq!(page.tab_count(0), 1, "restore disabled: no extra tabs");
            assert_eq!(page.pane_count(), 1);
        })
        .unwrap();
}

#[gpui::test]
async fn selection_single_toggle_range_and_paths(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.filtered_entries = vec![
                file("a", "file", 1),
                file("b", "file", 2),
                file("c", "file", 3),
                file("d", "file", 4),
            ];

            // Single selection re-anchors and becomes active.
            page.select_single(1);
            assert!(page.is_selected(1));
            assert_eq!(page.selection.len(), 1);
            assert_eq!(page.active_row(), Some(1));

            // Shift range spans from the anchor (row 1) to row 3 inclusive.
            page.select_range_to(3);
            assert_eq!(page.selected_paths(), vec!["/tmp/b", "/tmp/c", "/tmp/d"]);
            assert_eq!(page.active_row(), Some(3));

            // Cmd/Ctrl toggle removes one without clearing the rest and adds
            // another, re-anchoring at the toggled row.
            page.toggle_select(2);
            assert!(!page.is_selected(2) && page.is_selected(1) && page.is_selected(3));
            page.toggle_select(0);
            assert!(page.is_selected(0));

            // `selected_paths` follows row order (rows 0, 1, 3).
            assert_eq!(page.selected_paths(), vec!["/tmp/a", "/tmp/b", "/tmp/d"]);
        })
        .unwrap();
}

#[gpui::test]
async fn selection_all_clear_and_arrow_navigation(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.filtered_entries = vec![
                file("a", "file", 1),
                file("b", "file", 2),
                file("c", "file", 3),
            ];

            page.select_all();
            assert_eq!(page.selection.len(), 3);
            assert_eq!(page.active_row(), Some(2));

            page.clear_selection();
            assert!(page.selection.is_empty());
            assert_eq!(page.active_row(), None);
            assert_eq!(page.selection_anchor, None);

            // Arrow-down from an empty selection lands on the first row.
            page.move_active(1, false);
            assert_eq!(page.active_row(), Some(0));
            page.move_active(1, false);
            assert_eq!(page.active_row(), Some(1));

            // Shift+down extends the range from the anchor; up is clamped at 0.
            page.move_active(1, true);
            assert_eq!(page.selected_paths(), vec!["/tmp/b", "/tmp/c"]);
            page.select_single(0);
            page.move_active(-1, false);
            assert_eq!(page.active_row(), Some(0));
        })
        .unwrap();
}

#[gpui::test]
async fn sorting_keeps_the_selection_on_the_entries_it_was_made_on(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, cx| {
            page.entries = vec![
                file("a", "file", 1),
                file("b", "file", 2),
                file("c", "file", 3),
            ];
            page.apply_filter();

            // Pick the first two rows, then reverse the sort. "Select, then sort,
            // then act" used to be impossible: the selection was addressed by row
            // index, so every rebuild had to throw it away.
            page.select_single(0);
            page.select_range_to(1);
            assert_eq!(page.selected_paths(), vec!["/tmp/a", "/tmp/b"]);

            page.set_sort_key(SortKey::Name, cx);
            assert_eq!(
                page.filtered_entries[0].path, "/tmp/c",
                "the sort did reverse"
            );
            assert_eq!(page.selected_paths(), vec!["/tmp/b", "/tmp/a"]);
            assert!(page.is_selected(1) && page.is_selected(2) && !page.is_selected(0));
            assert_eq!(page.active_row(), Some(1), "the active entry moved with it");
        })
        .unwrap();
}

#[gpui::test]
async fn filtering_drops_entries_that_are_no_longer_visible(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.entries = vec![file("a", "file", 1), file("b", "file", 2)];
            page.apply_filter();
            page.select_all();

            // What the user cannot see cannot stay selected, or a later
            // operation would act on an entry that is not on screen.
            page.search_query = "a".to_string();
            page.apply_filter();
            assert_eq!(page.selected_paths(), vec!["/tmp/a"]);

            page.search_query = "zzz".to_string();
            page.apply_filter();
            assert!(page.selection.is_empty());
            assert_eq!(page.active_row(), None);
            assert_eq!(page.selection_anchor, None);
        })
        .unwrap();
}

// The quick search is typed in pinyin initials on Windows, where the system
// collation knows each character's reading: `wjj` finds 文件夹.
#[cfg(windows)]
#[gpui::test]
async fn quick_search_finds_chinese_names_by_pinyin_initials(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.entries = vec![
                file("文件夹", "dir", 1),
                file("工作报告.docx", "file", 2),
                file("readme.txt", "file", 3),
            ];
            let shown = |page: &ExplorerPane| -> Vec<String> {
                page.filtered_entries.iter().map(|entry| entry.name.clone()).collect()
            };

            page.search_query = "wjj".to_string();
            page.apply_filter();
            assert_eq!(shown(page), vec!["文件夹"]);

            page.search_query = "bg".to_string();
            page.apply_filter();
            assert_eq!(shown(page), vec!["工作报告.docx"]);

            // What the name itself holds is still matched as it was.
            page.search_query = "read".to_string();
            page.apply_filter();
            assert_eq!(shown(page), vec!["readme.txt"]);

            // A pattern none of the initials spell out finds nothing.
            page.search_query = "zzz".to_string();
            page.apply_filter();
            assert!(shown(page).is_empty());
        })
        .unwrap();
}

// Opens a pane rooted at a real directory and loads its listing, so the file
// operation tests can act on actual files. Mirrors how `reload` is driven
// elsewhere, but anchored at a temp dir instead of the cwd.
fn open_pane_at(cx: &mut TestAppContext, root: &std::path::Path) -> WindowHandle<ExplorerPane> {    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = root.to_string_lossy().to_string();
            pane.reload(cx);
        })
        .unwrap();
    window
}

// Builds an `ExplorerPane` wrapped in a `gpui_component::Root`, the way the real
// app mounts it. Operations that open a dialog (paste conflicts, permanent
// delete) or render a focused inline-rename input go through `Root`, so they
// need the Root layer present — a bare-pane window panics otherwise. Returns the
// pane entity so tests can drive it within the window's `Window`.
fn new_explorer_in_root(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<ExplorerPane>) {
    cx.update(gpui_component::init);
    let mut pane = None;
    let window = cx.add_window(|window, cx| {
        let resizable = cx.new(|_| ResizableState::default());
        let search_input = cx.new(|cx| InputState::new(window, cx));
        let entity = cx.new(|cx| {
            ExplorerPane::new(
                resizable,
                search_input,
                None,
                TrashLedger::KeptByOs,
                cx.focus_handle(),
            )
        });
        pane = Some(entity.clone());
        Root::new(entity, window, cx)
    });
    (window, pane.expect("Root::new build closure ran"))
}

#[gpui::test]
async fn permanent_delete_removes_only_the_selected(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(root.join(name), name).unwrap();
    }
    let window = open_pane_at(cx, root);
    window
        .update(cx, |pane, _window, cx| {
            // Sorted rows: a=0, b=1, c=2. Delete a and c.
            pane.select_single(0);
            pane.toggle_select(2);
            let paths = pane.selected_paths();
            pane.delete_permanent_paths(paths, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert!(!root.join("a.txt").exists());
    assert!(root.join("b.txt").exists(), "unselected file survives");
    assert!(!root.join("c.txt").exists());
}

// Sets up `src` (copied/cut from) and `dst` (pasted into) each containing
// `foo.txt`, so pasting always collides.
fn paste_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir(&src).unwrap();
    std::fs::create_dir(&dst).unwrap();
    std::fs::write(src.join("foo.txt"), "SRC").unwrap();
    std::fs::write(dst.join("foo.txt"), "DST").unwrap();
    (dir, src, dst)
}

#[gpui::test]
async fn paste_copy_with_rename_keeps_both(cx: &mut TestAppContext) {
    let (_dir, src, dst) = paste_fixture();
    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_all();
            pane.copy_selection(cx);

            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            // `prepare_paste` is the dialog-free half of `paste_into_cwd`; the
            // dialog plumbing itself is covered by GUI verification.
            let has_pending = pane.prepare_paste(cx);
            assert!(has_pending, "the name collision is queued");
            let done = pane.resolve_current_conflict(ConflictResolution::Rename, cx);
            assert!(done, "only one conflict to resolve");
            pane.execute_paste_plan(cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(dst.join("foo.txt")).unwrap(), "DST");
    assert_eq!(
        std::fs::read_to_string(dst.join("foo (2).txt")).unwrap(),
        "SRC",
        "rename keeps the existing file and adds a numbered copy"
    );
}

#[gpui::test]
async fn paste_copy_with_overwrite_replaces(cx: &mut TestAppContext) {
    let (_dir, src, dst) = paste_fixture();
    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_all();
            pane.copy_selection(cx);

            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            pane.prepare_paste(cx);
            pane.resolve_current_conflict(ConflictResolution::Overwrite, cx);
            pane.execute_paste_plan(cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(dst.join("foo.txt")).unwrap(), "SRC");
    assert!(!dst.join("foo (2).txt").exists());
}

#[gpui::test]
async fn paste_copy_with_skip_leaves_destination(cx: &mut TestAppContext) {
    let (_dir, src, dst) = paste_fixture();
    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_all();
            pane.copy_selection(cx);

            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            pane.prepare_paste(cx);
            pane.resolve_current_conflict(ConflictResolution::Skip, cx);
            pane.execute_paste_plan(cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(dst.join("foo.txt")).unwrap(), "DST");
    assert!(!dst.join("foo (2).txt").exists());
    assert!(
        src.join("foo.txt").exists(),
        "a copy leaves the source intact"
    );
}

#[gpui::test]
async fn paste_apply_to_all_resolves_every_conflict_at_once(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir(&src).unwrap();
    std::fs::create_dir(&dst).unwrap();
    std::fs::write(src.join("foo.txt"), "S1").unwrap();
    std::fs::write(src.join("bar.txt"), "S2").unwrap();
    std::fs::write(dst.join("foo.txt"), "D1").unwrap();
    std::fs::write(dst.join("bar.txt"), "D2").unwrap();
    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_all();
            pane.copy_selection(cx);

            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            pane.prepare_paste(cx);
            // Two conflicts queued; one Overwrite choice with "apply to all" set
            // resolves both.
            pane.set_apply_to_all(true, cx);
            let done = pane.resolve_current_conflict(ConflictResolution::Overwrite, cx);
            assert!(done, "apply-to-all clears every remaining conflict");
            pane.execute_paste_plan(cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(dst.join("foo.txt")).unwrap(), "S1");
    assert_eq!(std::fs::read_to_string(dst.join("bar.txt")).unwrap(), "S2");
}

#[gpui::test]
async fn cut_paste_without_conflict_moves_the_source(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir(&src).unwrap();
    std::fs::create_dir(&dst).unwrap();
    std::fs::write(src.join("x.txt"), "X").unwrap();
    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_all();
            pane.cut_selection(cx);
        })
        .unwrap();
    window
        .update(cx, |pane, window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            // No collision, so the paste executes immediately with no dialog.
            pane.paste_into_cwd(window, cx);
            assert!(pane.paste_plan.is_none());
        })
        .unwrap();
    cx.run_until_parked();
    assert!(dst.join("x.txt").exists());
    assert!(!src.join("x.txt").exists(), "a cut removes the source");
}

#[gpui::test]
async fn paste_numbers_a_second_source_sharing_an_existing_name(cx: &mut TestAppContext) {
    // Two clipboard sources named `foo.txt`, and `foo.txt` already exists at the
    // destination. Both collide with the same single destination, so resolving
    // the collision as Overwrite must not point both at it — the second would
    // silently replace the first.
    let dir = tempfile::tempdir().unwrap();
    let (first, second, dst) = (
        dir.path().join("a"),
        dir.path().join("b"),
        dir.path().join("dst"),
    );
    for sub in [&first, &second, &dst] {
        std::fs::create_dir(sub).unwrap();
    }
    std::fs::write(first.join("foo.txt"), "A").unwrap();
    std::fs::write(second.join("foo.txt"), "B").unwrap();
    std::fs::write(dst.join("foo.txt"), "D").unwrap();

    let window = new_explorer(cx);
    // Two files in different directories can't be selected in one listing, so
    // seed them through the system-clipboard path text, which `read_clipboard`
    // accepts as a copy.
    cx.update(|cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(format!(
            "{}\n{}",
            first.join("foo.txt").display(),
            second.join("foo.txt").display()
        )));
    });
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            assert!(pane.prepare_paste(cx), "the existing name collides");
            pane.set_apply_to_all(true, cx);
            pane.resolve_current_conflict(ConflictResolution::Overwrite, cx);
            pane.execute_paste_plan(cx);
        })
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        std::fs::read_to_string(dst.join("foo.txt")).unwrap(),
        "A",
        "the conflicting source overwrites the destination"
    );
    assert_eq!(
        std::fs::read_to_string(dst.join("foo (2).txt")).unwrap(),
        "B",
        "the second source sharing that name is numbered, not dropped"
    );
}

#[gpui::test]
async fn a_destination_taken_after_planning_is_numbered_not_overwritten(cx: &mut TestAppContext) {
    // The plan records collision-free sources by name and the copy runs later on
    // the background executor. Whatever takes the name in between must not be
    // written over — the filesystem, not the plan's lexical comparison, decides
    // whether a name is free.
    let dir = tempfile::tempdir().unwrap();
    let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
    for sub in [&src, &dst] {
        std::fs::create_dir(sub).unwrap();
    }
    std::fs::write(src.join("foo.txt"), "SRC").unwrap();

    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_all();
            pane.copy_selection(cx);
        })
        .unwrap();
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            assert!(
                !pane.prepare_paste(cx),
                "the destination name is free when the plan is built"
            );
        })
        .unwrap();
    std::fs::write(dst.join("foo.txt"), "OTHER").unwrap();
    window
        .update(cx, |pane, _window, cx| pane.execute_paste_plan(cx))
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        std::fs::read_to_string(dst.join("foo.txt")).unwrap(),
        "OTHER",
        "whatever took the name is left alone"
    );
    assert_eq!(
        std::fs::read_to_string(dst.join("foo (2).txt")).unwrap(),
        "SRC",
        "and the pasted source lands numbered beside it"
    );
}

#[gpui::test]
async fn pasting_names_differing_only_by_case_keeps_both_sources(cx: &mut TestAppContext) {
    // `foo.txt` and `FOO.txt` are two destinations on a case-sensitive volume
    // and one on a case-insensitive one (macOS, Windows). Either way neither
    // source may be lost: where they collapse the second is numbered. Asserted
    // on contents rather than names so the expectation holds on both.
    let dir = tempfile::tempdir().unwrap();
    let (first, second, dst) = (
        dir.path().join("a"),
        dir.path().join("b"),
        dir.path().join("dst"),
    );
    for sub in [&first, &second, &dst] {
        std::fs::create_dir(sub).unwrap();
    }
    std::fs::write(first.join("foo.txt"), "A").unwrap();
    std::fs::write(second.join("FOO.txt"), "B").unwrap();

    let window = new_explorer(cx);
    // Two files in different directories can't be selected in one listing, so
    // seed them through the system-clipboard path text.
    cx.update(|cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(format!(
            "{}\n{}",
            first.join("foo.txt").display(),
            second.join("FOO.txt").display()
        )));
    });
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            assert!(!pane.prepare_paste(cx), "the destination starts empty");
            pane.execute_paste_plan(cx);
        })
        .unwrap();
    cx.run_until_parked();

    let mut contents: Vec<String> = std::fs::read_dir(&dst)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    contents.sort();
    assert_eq!(
        contents,
        ["A", "B"],
        "neither source was written over the other"
    );
}

#[gpui::test]
async fn a_paste_during_an_in_flight_cut_finds_nothing_to_paste(cx: &mut TestAppContext) {
    // A cut paste empties the clipboard so a second paste cannot move the same
    // sources twice. The paths stay in the *system* clipboard though, and an
    // empty clipboard is exactly when `read_clipboard` falls back to that text —
    // so the second paste has to find nothing, rather than racing the move it
    // was meant to stand aside for by copying the sources straight back.
    let dir = tempfile::tempdir().unwrap();
    let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
    for sub in [&src, &dst] {
        std::fs::create_dir(sub).unwrap();
    }
    std::fs::write(src.join("x.txt"), "X").unwrap();

    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_all();
            pane.cut_selection(cx);
        })
        .unwrap();
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            pane.prepare_paste(cx);
            // Started, not awaited: the move is still on the background executor.
            pane.execute_paste_plan(cx);
            pane.prepare_paste(cx);
            assert!(
                pane.paste_plan.is_none(),
                "an in-flight cut leaves nothing for a second paste"
            );
        })
        .unwrap();
    cx.run_until_parked();

    assert_eq!(std::fs::read_to_string(dst.join("x.txt")).unwrap(), "X");
    assert!(
        !src.join("x.txt").exists(),
        "the cut moved its source exactly once"
    );
    // The suppression is scoped to the batch: once it finishes, the fallback is
    // live again. `dst/x.txt` is now occupied, so the replanned paste conflicts.
    window
        .update(cx, |pane, _window, cx| {
            assert!(
                pane.prepare_paste(cx),
                "the system-clipboard fallback comes back when the cut completes"
            );
            pane.cancel_paste(cx);
        })
        .unwrap();
}

#[gpui::test]
async fn closing_the_conflict_dialog_leaves_the_plan_for_the_resolving_button(
    cx: &mut TestAppContext,
) {
    // Each conflict button resolves, then calls `window.close_dialog(cx)`, then
    // `execute_paste_plan`. That order is only safe because gpui-component's
    // `WindowExt::close_dialog` pops the dialog stack without running the
    // dialog's own `on_close` — which cancels the paste, and would otherwise
    // take the plan out from under the very paste the button just resolved.
    // Pin it, so an upgrade that starts firing `on_close` from `close_dialog`
    // fails here rather than silently pasting nothing.
    use gpui_component::WindowExt as _;

    let (_dir, src, dst) = paste_fixture();
    let (window, pane) = new_explorer_in_root(cx);
    // Opening the dialog updates the `Root` entity, so the window's own
    // `update` (which already holds it) would panic on the second lease.
    // `VisualTestContext` hands out the `Window` without taking that lease.
    let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);

    pane.update_in(&mut cx, |pane, window, cx| {
        pane.cwd = src.to_string_lossy().to_string();
        pane.reload(cx);
        pane.select_all();
        pane.copy_selection(cx);
        pane.cwd = dst.to_string_lossy().to_string();
        pane.reload(cx);
        pane.paste_into_cwd(window, cx);
    });

    let done = pane.update(&mut cx, |pane, cx| {
        assert!(
            pane.paste_plan.is_some(),
            "the collision opens the dialog with a plan pending"
        );
        pane.resolve_current_conflict(ConflictResolution::Overwrite, cx)
    });
    assert!(done, "the only conflict is resolved");

    cx.update(|window, cx| window.close_dialog(cx));
    pane.update(&mut cx, |pane, cx| {
        assert!(
            pane.paste_plan.is_some(),
            "close_dialog must not cancel the paste it is closing over"
        );
        pane.execute_paste_plan(cx);
    });
    cx.run_until_parked();

    assert_eq!(
        std::fs::read_to_string(dst.join("foo.txt")).unwrap(),
        "SRC",
        "the resolved overwrite actually ran"
    );
}

#[gpui::test]
async fn a_failed_cut_does_not_clobber_a_clipboard_claimed_while_it_ran(cx: &mut TestAppContext) {
    // Restoring failed cut sources must not overwrite whatever the user (or an
    // overlapping paste) put on the clipboard while the batch was in flight —
    // that newer state is the one they can see.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir(&src).unwrap();
    std::fs::create_dir(&dst).unwrap();
    std::fs::write(src.join("x.txt"), "X").unwrap();
    std::fs::write(src.join("y.txt"), "Y").unwrap();

    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_single(0); // x.txt
            pane.cut_selection(cx);
        })
        .unwrap();

    // Remove the source so the cut fails, then start the paste without parking.
    std::fs::remove_file(src.join("x.txt")).unwrap();
    window
        .update(cx, |pane, window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            pane.paste_into_cwd(window, cx);
            // Still in flight: the user copies something else.
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_all(); // only y.txt remains
            pane.copy_selection(cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |pane, window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            pane.paste_into_cwd(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        std::fs::read_to_string(dst.join("y.txt")).unwrap(),
        "Y",
        "the copy the user made during the paste is what pastes"
    );
    assert!(
        src.join("y.txt").exists(),
        "and it stays a copy — the failed cut did not replace it"
    );
}

// Randomized scheduling: the two batches below complete in an order the
// dispatcher picks, and the whole point of the generation stamp is that the
// outcome no longer depends on it. Varying the seed exercises both orders.
#[gpui::test(iterations = 25)]
async fn overlapping_cut_pastes_restore_only_the_live_clipboard(cx: &mut TestAppContext) {
    // Two failed cut pastes in flight at once. Both left the clipboard empty, so
    // ownership cannot be read off "is the clipboard empty" — whichever finished
    // first would claim the restore, and with A first that strands B's sources
    // as non-retryable. Keyed on the generation each paste left behind, only B
    // (whose clear is the clipboard's current state) restores, whichever order
    // they finish in.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir(&src).unwrap();
    std::fs::create_dir(&dst).unwrap();
    std::fs::write(src.join("x.txt"), "X").unwrap();
    std::fs::write(src.join("y.txt"), "Y").unwrap();

    let window = new_explorer(cx);
    // Batch A: cut x.txt, delete it so the move fails, start the paste.
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_single(0); // x.txt
            pane.cut_selection(cx);
        })
        .unwrap();
    std::fs::remove_file(src.join("x.txt")).unwrap();
    window
        .update(cx, |pane, window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            pane.paste_into_cwd(window, cx);
        })
        .unwrap();

    // Batch B: same again for y.txt, without parking — so A is still in flight.
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_all(); // only y.txt remains
            pane.cut_selection(cx);
        })
        .unwrap();
    std::fs::remove_file(src.join("y.txt")).unwrap();
    window
        .update(cx, |pane, window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            pane.paste_into_cwd(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    // Restore both sources and retry whatever the clipboard kept.
    std::fs::write(src.join("x.txt"), "X").unwrap();
    std::fs::write(src.join("y.txt"), "Y").unwrap();
    window
        .update(cx, |pane, window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            pane.paste_into_cwd(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        std::fs::read_to_string(dst.join("y.txt")).unwrap(),
        "Y",
        "the later batch's failed source is the one retained"
    );
    assert!(
        !src.join("y.txt").exists(),
        "and it retries as a move, not a copy"
    );
    assert!(
        !dst.join("x.txt").exists(),
        "the earlier batch did not claim the restore"
    );
    assert!(src.join("x.txt").exists());
}

#[gpui::test]
async fn cut_paste_keeps_failed_sources_on_the_clipboard_as_a_cut(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir(&src).unwrap();
    std::fs::create_dir(&dst).unwrap();
    std::fs::write(src.join("x.txt"), "X").unwrap();

    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.cwd = src.to_string_lossy().to_string();
            pane.reload(cx);
            pane.select_all();
            pane.cut_selection(cx);
        })
        .unwrap();

    // Make the move fail by removing the source out from under the paste.
    std::fs::remove_file(src.join("x.txt")).unwrap();
    window
        .update(cx, |pane, window, cx| {
            pane.cwd = dst.to_string_lossy().to_string();
            pane.reload(cx);
            pane.paste_into_cwd(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .read_with(cx, |pane, _cx| {
            let (_, level) = pane.status_for_footer().expect("the failure is reported");
            assert_eq!(level, StatusLevel::Error, "the move failed");
        })
        .unwrap();
    assert!(!dst.join("x.txt").exists());

    // Retrying must still be a *move*. Were the failed source dropped from the
    // clipboard, this would fall back to the system clipboard's path text, which
    // carries no cut/copy distinction and so would copy instead.
    std::fs::write(src.join("x.txt"), "X").unwrap();
    window
        .update(cx, |pane, window, cx| pane.paste_into_cwd(window, cx))
        .unwrap();
    cx.run_until_parked();

    assert_eq!(std::fs::read_to_string(dst.join("x.txt")).unwrap(), "X");
    assert!(
        !src.join("x.txt").exists(),
        "the retry moves the source rather than copying it"
    );
}

#[gpui::test]
async fn rename_collision_falls_back_to_numbering(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    std::fs::write(root.join("b.txt"), "b").unwrap();
    let window = open_pane_at(cx, root);
    window
        .update(cx, |pane, window, cx| {
            // Rename b.txt (row 1) to the already-taken "a.txt".
            pane.begin_rename(1, window, cx);
            let input = pane.renaming.as_ref().unwrap().input.clone();
            input.update(cx, |state, cx| state.set_value("a.txt", window, cx));
            pane.commit_rename(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a");
    assert_eq!(
        std::fs::read_to_string(root.join("a (2).txt")).unwrap(),
        "b",
        "the conflicting rename is numbered rather than overwriting"
    );
    assert!(!root.join("b.txt").exists());
}

// Changing only the case of a name is a rename, not a collision with the file
// itself: on NTFS it used to come out as `A (2).txt`.
#[gpui::test]
async fn a_rename_that_only_changes_case_is_not_numbered(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let window = open_pane_at(cx, dir.path());
    window
        .update(cx, |pane, window, cx| {
            pane.begin_rename(0, window, cx);
            let input = pane.renaming.as_ref().unwrap().input.clone();
            input.update(cx, |state, cx| state.set_value("A.txt", window, cx));
            pane.commit_rename(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["A.txt"]);
}

// The rename asks the disk off the UI thread: a folder on a share that
// stopped answering must not freeze the window on it.
#[gpui::test]
async fn a_rename_runs_off_the_ui_thread(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let window = open_pane_at(cx, dir.path());
    window
        .update(cx, |pane, window, cx| {
            pane.begin_rename(0, window, cx);
            let input = pane.renaming.as_ref().unwrap().input.clone();
            input.update(cx, |state, cx| state.set_value("b.txt", window, cx));
            pane.commit_rename(window, cx);
            // The test executor runs background work only when it is let to.
            assert!(dir.path().join("a.txt").exists(), "nothing touched the disk on the UI thread");
            assert!(pane.renaming.is_none(), "the field closes at once");
        })
        .unwrap();
    cx.run_until_parked();
    assert!(dir.path().join("b.txt").exists());
    assert!(!dir.path().join("a.txt").exists());
}

#[gpui::test]
async fn new_folder_picks_a_unique_name(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("Nova pasta")).unwrap();
    let (window, pane) = new_explorer_in_root(cx);
    window
        .update(cx, |_root, window, cx| {
            pane.update(cx, |pane, cx| {
                pane.cwd = root.to_string_lossy().to_string();
                pane.reload(cx);
                pane.create_new_folder(window, cx);
            });
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        root.join("Nova pasta (2)").is_dir(),
        "a new folder avoids colliding with the existing one"
    );
}

#[gpui::test]
async fn copy_selection_reports_status(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "A").unwrap();
    let window = open_pane_at(cx, root);
    window
        .update(cx, |pane, _window, cx| {
            pane.select_all();
            pane.copy_selection(cx);
            assert_eq!(
                pane.status_for_footer(),
                Some(("1 item copiado".to_string(), StatusLevel::Info))
            );
        })
        .unwrap();
}

#[gpui::test]
async fn paste_copy_into_same_directory_duplicates(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "A").unwrap();
    let window = open_pane_at(cx, root);
    window
        .update(cx, |pane, _window, cx| {
            pane.select_all();
            pane.copy_selection(cx);
            // Pasting back into the same directory duplicates without a dialog.
            let has_pending = pane.prepare_paste(cx);
            assert!(!has_pending);
            pane.execute_paste_plan(cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "A");
    assert_eq!(
        std::fs::read_to_string(root.join("a (2).txt")).unwrap(),
        "A"
    );
}

#[gpui::test]
async fn prepare_paste_without_clipboard_is_noop(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let window = open_pane_at(cx, dir.path());
    window
        .update(cx, |pane, _window, cx| {
            assert!(!pane.prepare_paste(cx));
            assert!(pane.paste_plan.is_none());
        })
        .unwrap();
}

#[gpui::test]
async fn trash_selection_removes_from_source(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "A").unwrap();
    std::fs::write(root.join("b.txt"), "B").unwrap();
    let window = open_pane_at(cx, root);
    window
        .update(cx, |pane, _window, cx| {
            pane.select_single(0); // a.txt
            pane.trash_selection(cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        !root.join("a.txt").exists(),
        "trashed file leaves the source"
    );
    assert!(root.join("b.txt").exists());
}

#[gpui::test]
async fn a_reload_that_fails_takes_the_selection_with_it(cx: &mut TestAppContext) {
    // The error path empties the listing without going through `apply_filter`,
    // so nothing pruned the selection: it survived the empty rows and came back
    // the moment those paths were on screen again, selected by nobody.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("a.txt"), "A").unwrap();
    let window = open_pane_at(cx, &root);
    window
        .update(cx, |pane, _window, cx| {
            pane.select_all();
            assert_eq!(pane.selected_paths().len(), 1);

            pane.cwd = root.join("gone").to_string_lossy().into_owned();
            pane.reload(cx);

            assert!(pane.filtered_entries.is_empty(), "the listing is empty");
            assert!(pane.selection.is_empty(), "and so is the selection");
            assert_eq!(pane.active_row(), None);
            assert_eq!(pane.selection_anchor, None);
        })
        .unwrap();
}

#[gpui::test]
async fn trash_refuses_when_nothing_can_record_where_the_item_came_from(cx: &mut TestAppContext) {
    // On a platform whose OS trash indexes nothing, a ledger that cannot be
    // opened is not the same as not needing one. Trashing would still succeed —
    // and the item would be in the trash with no way back — so it has to be
    // refused, and said out loud, rather than reported as a delete that worked.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "A").unwrap();
    let window = open_pane_at(cx, root);
    window
        .update(cx, |pane, _window, cx| {
            pane.trash_ledger = TrashLedger::Unavailable("the database is locked".into());
            pane.select_single(0);
            pane.trash_selection(cx);
        })
        .unwrap();
    cx.run_until_parked();

    assert!(root.join("a.txt").exists(), "the item must not be trashed");
    window
        .read_with(cx, |pane, _cx| {
            let (text, level) = pane.status_for_footer().expect("a status is reported");
            assert_eq!(level, StatusLevel::Error, "{text}");
            assert!(text.contains("the database is locked"), "{text}");
        })
        .unwrap();
}

#[gpui::test]
async fn delete_permanent_reports_failure_in_status(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let window = open_pane_at(cx, dir.path());
    window
        .update(cx, |pane, _window, cx| {
            pane.delete_permanent_paths(vec!["/no/such/path/xyzzy".to_string()], cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .read_with(cx, |pane, _cx| {
            let (text, level) = pane.status_for_footer().expect("a status was set");
            assert_eq!(level, StatusLevel::Error);
            assert!(text.contains("1 de 1"), "got: {text}");
        })
        .unwrap();
}

#[gpui::test]
async fn rename_to_invalid_name_reports_error(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "A").unwrap();
    let window = open_pane_at(cx, root);
    window
        .update(cx, |pane, window, cx| {
            pane.begin_rename(0, window, cx);
            let input = pane.renaming.as_ref().unwrap().input.clone();
            // A name with a path separator is rejected by `rename_in_place`.
            input.update(cx, |state, cx| state.set_value("bad/name", window, cx));
            pane.commit_rename(window, cx);
        })
        .unwrap();
    // The rename answers from the background (`a_rename_runs_off_the_ui_thread`).
    cx.run_until_parked();
    window
        .read_with(cx, |pane, _cx| {
            let (text, level) = pane.status_for_footer().expect("a status was set");
            assert_eq!(level, StatusLevel::Error);
            assert!(text.contains("Não foi possível renomear"), "got: {text}");
        })
        .unwrap();
    assert!(root.join("a.txt").exists(), "the original is untouched");
}

#[gpui::test]
async fn paste_falls_back_to_system_clipboard_paths(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir(&src).unwrap();
    std::fs::create_dir(&dst).unwrap();
    std::fs::write(src.join("x.txt"), "X").unwrap();
    let src_path = src.join("x.txt").to_string_lossy().to_string();
    let window = open_pane_at(cx, &dst);
    window
        .update(cx, |pane, _window, cx| {
            // No internal clipboard set; a bare path on the system clipboard is
            // read back as a copy source.
            cx.write_to_clipboard(ClipboardItem::new_string(src_path));
            let has_pending = pane.prepare_paste(cx);
            assert!(!has_pending);
            pane.execute_paste_plan(cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        dst.join("x.txt").exists(),
        "pasted from the system clipboard path"
    );
}

#[gpui::test]
async fn paste_numbers_same_basename_sources(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    let dst = dir.path().join("dst");
    for d in [&a, &b, &dst] {
        std::fs::create_dir(d).unwrap();
    }
    std::fs::write(a.join("foo.txt"), "A1").unwrap();
    std::fs::write(b.join("foo.txt"), "B1").unwrap();
    let window = open_pane_at(cx, &dst);
    window
        .update(cx, |pane, _window, cx| {
            // Two sources with the same basename from different directories.
            let paths = format!(
                "{}\n{}",
                a.join("foo.txt").display(),
                b.join("foo.txt").display()
            );
            cx.write_to_clipboard(ClipboardItem::new_string(paths));
            let has_pending = pane.prepare_paste(cx);
            assert!(!has_pending, "batch collisions are numbered, not prompted");
            pane.execute_paste_plan(cx);
        })
        .unwrap();
    cx.run_until_parked();
    // Both land instead of one clobbering the other.
    let mut got = vec![
        std::fs::read_to_string(dst.join("foo.txt")).unwrap(),
        std::fs::read_to_string(dst.join("foo (2).txt")).unwrap(),
    ];
    got.sort();
    assert_eq!(got, vec!["A1".to_string(), "B1".to_string()]);
}

#[gpui::test]
async fn hiding_a_column_keeps_its_width_for_when_it_returns(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.col_size_width = 123.0;
            page.toggle_column(2);
            assert!(!page.column_visible(2));
            assert_eq!(page.col_size_width, 0.0);
            page.toggle_column(2);
            assert!(page.column_visible(2));
            assert_eq!(page.col_size_width, 123.0);
            // Name cannot be hidden.
            page.toggle_column(0);
            assert!(page.column_visible(0));
        })
        .unwrap();
}

#[gpui::test]
async fn fitting_a_column_follows_its_longest_value(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.entries = vec![file("a.txt", "file", 1), file(&"x".repeat(60), "file", 1)];
            page.apply_filter();
            page.fit_column(0);
            let long = page.col_name_width;
            page.entries = vec![file("a.txt", "file", 1)];
            page.apply_filter();
            page.fit_column(0);
            assert!(page.col_name_width < long);
        })
        .unwrap();
}

#[gpui::test]
async fn the_tree_view_splices_an_open_folder_under_it(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, cx| {
            page.entries = vec![file("docs", "dir", 0), file("z.txt", "file", 1)];
            page.set_view_mode(ViewMode::Tree, cx);
            settle_zoom(page, cx);
            let docs = page.filtered_entries[0].path.clone();
            page.tree.children.insert(
                docs.clone(),
                vec![file("b.txt", "file", 1), file("a.txt", "file", 1)],
            );
            page.tree.expanded.insert(docs.clone());
            page.apply_filter();
            let names: Vec<&str> = page.filtered_entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["docs", "a.txt", "b.txt", "z.txt"]);
            assert_eq!(page.tree_depth(&page.filtered_entries[1].path.clone()), 1);
            assert_eq!(page.tree_depth(&docs), 0);
            // Leaving the Tree view puts the folder back to one row.
            page.set_view_mode(ViewMode::List, cx);
            settle_zoom(page, cx);
            assert_eq!(page.filtered_entries.len(), 2);
        })
        .unwrap();
}

#[gpui::test]
async fn choosing_the_view_back_mid_glide_turns_the_glide_around(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, cx| {
            page.set_view_mode(ViewMode::Grid, cx);
            // Still in List on screen: the glide has not started moving.
            assert!(page.view_mode == ViewMode::List);
            page.set_view_mode(ViewMode::List, cx);
            assert_eq!(page.zoom_target, 0.0);
        })
        .unwrap();
}

#[gpui::test]
async fn the_highlight_is_the_literal_part_of_the_filter(cx: &mut TestAppContext) {
    let window = new_explorer(cx);
    window
        .update(cx, |page, _window, _cx| {
            page.name_filter = "*.png".to_string();
            assert_eq!(page.highlight_query(), ".png");
            page.name_filter = "deltarune".to_string();
            assert_eq!(page.highlight_query(), "deltarune");
            page.name_filter.clear();
            assert_eq!(page.highlight_query(), "");
        })
        .unwrap();
}

#[gpui::test]
async fn path_field_shows_the_path_and_goes_where_it_is_told(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // An accent, so the UTF-16 selection is exercised on more than ASCII.
    let sub = root.join("Extensões");
    std::fs::create_dir(&sub).unwrap();
    let window = open_pane_at(cx, root);
    window
        .update(cx, |pane, window, cx| {
            pane.begin_path_edit(window, cx);
            let input = pane.path_edit.as_ref().unwrap().input.clone();
            assert_eq!(input.read(cx).value().to_string(), pane.cwd, "the field starts with the folder's path");

            // A path that does not exist keeps the field open to fix it.
            input.update(cx, |state, cx| state.set_value(r"Z:\nao\existe", window, cx));
            pane.commit_path_edit(window, cx);
            assert!(pane.path_edit.is_some());

            input.update(cx, |state, cx| state.set_value(sub.to_string_lossy().to_string(), window, cx));
            pane.commit_path_edit(window, cx);
            assert!(pane.path_edit.is_none(), "Enter on a real folder closes the field");
            assert_eq!(pane.cwd, sub.to_string_lossy());

            pane.begin_path_edit(window, cx);
            pane.cancel_path_edit(window, cx);
            assert!(pane.path_edit.is_none());
            assert_eq!(pane.cwd, sub.to_string_lossy(), "Escape goes nowhere");
        })
        .unwrap();
}

#[gpui::test]
async fn dropping_files_on_a_folder_moves_them_on_the_same_drive(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = root.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    let window = open_pane_at(cx, root);
    window
        .update(cx, |pane, window, cx| {
            pane.drop_paths(&[root.join("a.txt")], target.clone(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(target.join("a.txt")).unwrap(), "a");
    assert!(!root.join("a.txt").exists(), "a drop on the same drive is a move");
}

#[gpui::test]
async fn a_folder_dropped_into_itself_is_refused(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let folder = root.join("folder");
    std::fs::create_dir_all(folder.join("inner")).unwrap();
    let window = open_pane_at(cx, root);
    window
        .update(cx, |pane, window, cx| {
            pane.drop_paths(&[folder.clone()], folder.join("inner"), window, cx);
            assert!(pane.paste_plan.is_none(), "nothing is planned");
        })
        .unwrap();
    cx.run_until_parked();
    assert!(folder.join("inner").is_dir());
    assert!(!folder.join("inner").join("folder").exists());
}

// A folder that does not answer within `listing_wait` is read in the
// background, and a read overtaken by a later navigation never lands: the
// pane shows the folder navigated to last, not the one that answered last.
#[gpui::test]
async fn a_slow_folder_is_read_in_the_background_and_an_overtaken_read_is_dropped(
    cx: &mut TestAppContext,
) {
    // The read runs on a real thread, so the executor has to be allowed to
    // wait for it.
    cx.executor().allow_parking();
    let first = tempfile::tempdir().unwrap();
    std::fs::write(first.path().join("a.txt"), "A").unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(second.path().join("b.txt"), "B").unwrap();
    let window = new_explorer(cx);
    window
        .update(cx, |pane, _window, cx| {
            pane.listing_wait = std::time::Duration::ZERO;
            pane.cwd = first.path().to_string_lossy().into_owned();
            pane.reload(cx);
            pane.cwd = second.path().to_string_lossy().into_owned();
            pane.reload(cx);
            assert!(pane.listing_pending, "nothing waited, so the read must still be on its way");
        })
        .unwrap();

    let mut names = Vec::new();
    for _ in 0..500 {
        cx.run_until_parked();
        let (pending, listed) = window
            .update(cx, |pane, _window, _cx| {
                let listed: Vec<String> =
                    pane.filtered_entries.iter().map(|entry| entry.name.clone()).collect();
                (pane.listing_pending, listed)
            })
            .unwrap();
        names = listed;
        if !pending {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(names, ["b.txt"]);
}

// A refresh of the folder on screen (F5, or what runs after a delete, paste,
// rename or drop) is not a visit: the folders open in the Tree view stay open,
// and they are read again, so a file deleted inside one leaves. Until
// 04/11/2026 every refresh closed them all.
#[gpui::test]
async fn a_refresh_keeps_tree_folders_open_and_reads_them_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let docs = dir.path().join("docs");
    std::fs::create_dir(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), "a").unwrap();
    std::fs::write(docs.join("b.txt"), "b").unwrap();
    std::fs::write(dir.path().join("z.txt"), "z").unwrap();
    let window = open_pane_at(cx, dir.path());
    let names = |pane: &ExplorerPane| -> Vec<String> {
        pane.filtered_entries.iter().map(|entry| entry.name.clone()).collect()
    };
    window
        .update(cx, |pane, _window, cx| {
            pane.set_view_mode(ViewMode::Tree, cx);
            settle_zoom(pane, cx);
            let docs_path = pane.filtered_entries[0].path.clone();
            pane.toggle_tree_folder(docs_path, cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |pane, _window, _cx| {
            assert_eq!(names(pane), ["docs", "a.txt", "b.txt", "z.txt"]);
        })
        .unwrap();

    std::fs::remove_file(docs.join("b.txt")).unwrap();
    window
        .update(cx, |pane, _window, cx| {
            pane.reload(cx);
            assert_eq!(names(pane), ["docs", "a.txt", "z.txt"], "still open, and read again");
        })
        .unwrap();

    // A folder open in the tree that went away closes instead of failing.
    std::fs::remove_dir_all(&docs).unwrap();
    window
        .update(cx, |pane, _window, cx| {
            pane.reload(cx);
            assert_eq!(names(pane), ["z.txt"]);
            assert!(pane.tree.expanded.is_empty());
        })
        .unwrap();
}

// A file that changed on disk since the last read gets its picture made
// again on the refresh; one that did not keeps it. The cache is by path, and
// a file overwritten in place used to keep the old picture.
#[gpui::test]
async fn a_refresh_forgets_the_picture_of_a_file_that_changed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let changed = dir.path().join("foto.png");
    let kept = dir.path().join("outra.png");
    std::fs::write(&changed, "antes").unwrap();
    std::fs::write(&kept, "igual").unwrap();
    let window = open_pane_at(cx, dir.path());
    let (changed_path, kept_path) = (
        changed.to_string_lossy().into_owned(),
        kept.to_string_lossy().into_owned(),
    );
    window
        .update(cx, |pane, _window, cx| {
            pane.thumbnails.get(&changed_path, 256, cx);
            pane.thumbnails.get(&kept_path, 256, cx);
            assert!(pane.thumbnails.holds(&changed_path) && pane.thumbnails.holds(&kept_path));
        })
        .unwrap();

    std::fs::write(&changed, "depois, com outro tamanho").unwrap();
    window
        .update(cx, |pane, _window, cx| {
            pane.reload(cx);
            assert!(!pane.thumbnails.holds(&changed_path), "changed: asked for again");
            assert!(pane.thumbnails.holds(&kept_path), "unchanged: kept");
        })
        .unwrap();
}

// Going to another folder still starts with every folder closed.
#[gpui::test]
async fn a_visit_to_another_folder_starts_with_the_tree_closed(cx: &mut TestAppContext) {
    let first = tempfile::tempdir().unwrap();
    std::fs::create_dir(first.path().join("docs")).unwrap();
    std::fs::write(first.path().join("docs").join("a.txt"), "a").unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::create_dir(second.path().join("docs")).unwrap();
    let window = open_pane_at(cx, first.path());
    window
        .update(cx, |pane, _window, cx| {
            pane.set_view_mode(ViewMode::Tree, cx);
            settle_zoom(pane, cx);
            let docs_path = pane.filtered_entries[0].path.clone();
            pane.toggle_tree_folder(docs_path, cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |pane, _window, cx| {
            assert!(!pane.tree.expanded.is_empty());
            pane.cwd = second.path().to_string_lossy().into_owned();
            pane.reload(cx);
            assert!(pane.tree.expanded.is_empty());
            assert!(pane.tree.children.is_empty());
        })
        .unwrap();
}

// The preview stays on a refresh while its file is there, and closes when the
// file is gone; going to another folder closes it as before.
#[gpui::test]
async fn a_refresh_keeps_the_preview_while_its_file_is_there(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let note = dir.path().join("nota.txt");
    std::fs::write(&note, "texto").unwrap();
    std::fs::write(dir.path().join("outro.txt"), "x").unwrap();
    let window = open_pane_at(cx, dir.path());
    let note_path = note.to_string_lossy().into_owned();
    window
        .update(cx, |pane, _window, cx| {
            pane.preview_path = Some(note_path.clone());
            pane.preview_text = Some("texto".to_string());
            pane.reload(cx);
            assert_eq!(pane.preview_path.as_deref(), Some(note_path.as_str()));
            assert_eq!(pane.preview_text.as_deref(), Some("texto"));
        })
        .unwrap();

    std::fs::remove_file(&note).unwrap();
    window
        .update(cx, |pane, _window, cx| {
            pane.reload(cx);
            assert_eq!(pane.preview_path, None);
            assert_eq!(pane.preview_text, None);
        })
        .unwrap();
}

// Explorer keeps the row it just renamed selected; the selection is kept by
// path, so the refresh after a rename used to drop it.
#[gpui::test]
async fn a_renamed_row_stays_selected(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b").unwrap();
    let window = open_pane_at(cx, dir.path());
    window
        .update(cx, |pane, window, cx| {
            pane.select_single(1);
            pane.begin_rename(1, window, cx);
            let input = pane.renaming.as_ref().unwrap().input.clone();
            input.update(cx, |state, cx| state.set_value("c.txt", window, cx));
            pane.commit_rename(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |pane, _window, _cx| {
            let renamed = dir.path().join("c.txt").to_string_lossy().into_owned();
            assert_eq!(pane.active_path.as_deref(), Some(renamed.as_str()));
            assert_eq!(pane.selected_paths(), [renamed]);
        })
        .unwrap();
}

// A folder opens as it was left (zoom and sort), as in Explorer; a folder
// never changed opens with the configured sort and keeps the tab's zoom; and a
// refresh does not undo a change made in the folder.
#[gpui::test]
async fn each_folder_opens_with_the_zoom_and_sort_it_was_left_with(cx: &mut TestAppContext) {
    let photos = tempfile::tempdir().unwrap();
    std::fs::write(photos.path().join("a.png"), "A").unwrap();
    let music = tempfile::tempdir().unwrap();
    std::fs::write(music.path().join("b.wav"), "B").unwrap();
    let photos_path = photos.path().to_string_lossy().into_owned();
    let music_path = music.path().to_string_lossy().into_owned();

    cx.update(|cx| super::folder_views::FolderViews::install(None, cx));
    let window = new_explorer(cx);
    window
        .update(cx, |page, window, cx| {
            let ui = config::Ui {
                app_name: String::new(),
                language: "auto".to_string(),
                default_sort: config::SortOrder::Name,
                default_sort_descending: true,
                show_hidden: false,
                show_quick_access: false,
                icon_pack: "default".to_string(),
                font_family: String::new(),
                name_lines: 0,
                scroll_smoothness: 50,
                audio_waveforms: false,
                preview_autoplay: false,
                welcome_done: false,
                hover_audio: false,
                hover_audio_delay_ms: 400,
                file_type_colors: Vec::new(),
            };
            page.apply_config_ui(&ui, cx);

            page.change_dir(photos_path.clone(), window, cx);
            // Never changed: the configured default, descending.
            assert!(page.sort_key == SortKey::Name && !page.sort_asc);
            page.set_zoom(0.6, cx);
            page.set_sort(SortKey::Size, true, cx);
            page.reload(cx);
            assert!(page.sort_key == SortKey::Size && page.sort_asc, "a refresh keeps it");

            page.change_dir(music_path.clone(), window, cx);
            assert!(page.sort_key == SortKey::Name && !page.sort_asc);
            assert_eq!(page.zoom, 0.6, "a folder never changed keeps the tab's zoom");
            page.set_zoom(0.0, cx);

            page.change_dir(photos_path.clone(), window, cx);
            assert!(page.sort_key == SortKey::Size && page.sort_asc);
            assert_eq!(page.zoom, 0.6);
            assert!(page.view_mode == ViewMode::Grid);

            page.go_back(window, cx);
            assert_eq!(page.zoom, 0.0);
            assert!(page.view_mode == ViewMode::List);
        })
        .unwrap();
}

// A file another program saves in the folder on screen shows up without F5,
// and one it deletes goes (folder_watch.rs). Until 05/11/2026 neither did.
#[gpui::test]
async fn the_folder_on_screen_follows_what_other_programs_do(cx: &mut TestAppContext) {
    // The notifications and the read come on real threads.
    cx.executor().allow_parking();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "A").unwrap();
    let window = open_pane_at(cx, dir.path());
    let names = |cx: &mut TestAppContext| -> Vec<String> {
        window
            .read_with(cx, |pane, _cx| pane.filtered_entries.iter().map(|entry| entry.name.clone()).collect())
            .unwrap()
    };
    let wait_for = |cx: &mut TestAppContext, wanted: &[&str]| {
        for _ in 0..300 {
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            if names(cx) == wanted {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    };
    assert!(wait_for(cx, &["a.txt"]), "the first listing: {:?}", names(cx));

    std::fs::write(dir.path().join("b.txt"), "saved by someone else").unwrap();
    assert!(wait_for(cx, &["a.txt", "b.txt"]), "after a save elsewhere: {:?}", names(cx));

    std::fs::remove_file(dir.path().join("a.txt")).unwrap();
    assert!(wait_for(cx, &["b.txt"]), "after a delete elsewhere: {:?}", names(cx));
}
