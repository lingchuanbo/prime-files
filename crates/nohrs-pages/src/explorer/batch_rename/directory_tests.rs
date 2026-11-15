#![allow(clippy::disallowed_methods)]
use super::rename_window;
use gpui::{Modifiers, TestAppContext, point, px, size};
use std::{fs, time::Duration};

#[gpui::test]
async fn recursive_directory_filter_checkbox_and_counters_drive_preview(cx: &mut TestAppContext) {
    let (view, fixture, cx) = rename_window(cx);
    fs::create_dir(fixture.path().join("子目录")).unwrap();
    fs::write(fixture.path().join("a.PNG"), "png").unwrap();
    fs::write(fixture.path().join("子目录/b.png"), "nested").unwrap();
    cx.simulate_resize(size(px(1220.0), px(820.0)));
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.inputs[0].update(cx, |input, cx| input.set_value("[B]_[C]", window, cx));
            view.scope.inputs[1].update(cx, |input, cx| input.set_value("png", window, cx));
            view.scope.options.directory_mode = true;
            view.scope.options.recursive = true;
            view.scope.options.restart_counter = true;
            view.rescan(cx);
        })
    });
    cx.background_executor
        .timer(Duration::from_millis(600))
        .await;
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(2, view.rows.len());
        assert!(view.can_apply());
        assert_eq!(
            "子目录",
            view.scope.directory_label(&view.rows[1].entry.path)
        );
        assert_eq!("子目录_001.png", view.rows[1].name);
    });
    let row = cx.debug_bounds("rename-row-0").expect("file row");
    cx.simulate_click(
        point(row.left() + px(20.0), row.center().y),
        Modifiers::none(),
    );
    cx.background_executor
        .timer(Duration::from_millis(150))
        .await;
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(!view.rows[0].selected);
        assert!(view.rows[1].selected);
        assert_eq!("子目录_001.png", view.rows[1].name);
    });
}

#[gpui::test]
async fn cancelled_scan_and_changed_scope_never_reuse_stale_preview(cx: &mut TestAppContext) {
    let (view, _fixture, cx) = rename_window(cx);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.inputs[0].update(cx, |input, cx| input.set_value("[N]_[C]", window, cx));
            view.scope.options.directory_mode = true;
            view.rescan(cx);
            view.stop_scan(cx);
            assert!(!view.can_apply());
        })
    });
    cx.background_executor
        .timer(Duration::from_millis(600))
        .await;
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(view.rows.is_empty());
        assert!(!view.scope.ready);
        assert!(!view.can_apply());
    });
    view.update(cx, |view, cx| {
        view.scope.options.directory_mode = false;
        view.rescan(cx);
    });
    cx.background_executor
        .timer(Duration::from_millis(150))
        .await;
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(1, view.rows.len());
        assert!(view.can_apply());
        assert_eq!("项目说明_001.txt", view.rows[0].name);
    });
}

#[gpui::test]
async fn rule_edits_only_rebuild_preview_without_scanning_new_files(cx: &mut TestAppContext) {
    let (view, fixture, cx) = rename_window(cx);
    view.update(cx, |view, cx| {
        view.scope.options.directory_mode = true;
        view.rescan(cx);
    });
    cx.background_executor
        .timer(Duration::from_millis(600))
        .await;
    cx.run_until_parked();
    fs::write(fixture.path().join("later.txt"), "later").unwrap();
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.inputs[0].update(cx, |input, cx| input.set_value("[拼音]_[C]", window, cx));
        })
    });
    cx.background_executor
        .timer(Duration::from_millis(150))
        .await;
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(1, view.rows.len());
        assert_eq!("xiangmushuoming_001.txt", view.rows[0].name);
        assert!(!view.scope.scanning);
    });
}

#[gpui::test]
async fn scanned_renames_keep_original_selection_paths_in_sync(cx: &mut TestAppContext) {
    let (view, fixture, cx) = rename_window(cx);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.inputs[0].update(cx, |input, cx| input.set_value("[N]_[C]", window, cx));
            view.scope.options.directory_mode = true;
            view.rescan(cx);
        })
    });
    cx.background_executor
        .timer(Duration::from_millis(600))
        .await;
    cx.run_until_parked();
    view.update(cx, |view, cx| view.apply(cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(view.applied);
        assert_eq!("项目说明_001.txt", view.scope.original[0].name);
        assert!(std::path::Path::new(&view.scope.original[0].path).exists());
    });
    assert!(fixture.path().join("项目说明_001.txt").exists());
}

#[gpui::test]
async fn per_directory_option_restarts_zero_padded_preview(cx: &mut TestAppContext) {
    let (view, fixture, cx) = rename_window(cx);
    for directory in ["A", "B"] {
        fs::create_dir(fixture.path().join(directory)).unwrap();
        fs::write(fixture.path().join(directory).join("original.png"), "png").unwrap();
    }
    cx.simulate_resize(size(px(1220.0), px(820.0)));
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            for (index, value) in [(0, "[C]"), (4, "0"), (6, "5")] {
                view.inputs[index].update(cx, |input, cx| input.set_value(value, window, cx));
            }
            view.scope.inputs[1].update(cx, |input, cx| input.set_value("png", window, cx));
            view.scope.options.directory_mode = true;
            view.scope.options.recursive = true;
            view.rescan(cx);
        });
    });
    cx.background_executor
        .timer(Duration::from_millis(600))
        .await;
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!("00000.png", view.rows[0].name);
        assert_eq!("00001.png", view.rows[1].name);
    });
    let checkbox = cx
        .debug_bounds("rename-counter-per-directory")
        .expect("visible directory option");
    cx.simulate_click(
        point(checkbox.left() + px(8.0), checkbox.center().y),
        Modifiers::none(),
    );
    cx.background_executor
        .timer(Duration::from_millis(150))
        .await;
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(view.scope.options.restart_counter);
        assert!(view.can_apply());
        assert!(view.rows.iter().all(|row| row.name == "00000.png"));
    });
}
