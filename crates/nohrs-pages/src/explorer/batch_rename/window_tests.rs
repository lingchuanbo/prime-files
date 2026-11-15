#![allow(clippy::disallowed_methods)]

use super::RenameWindow;
use crate::explorer::ExplorerPane;
use gpui::{AppContext, Entity, Modifiers, TestAppContext, VisualTestContext, point, px, size};
use gpui_component::{Root, input::InputState, resizable::ResizableState};
use nohrs_services::fs::{listing::FileEntryDto, trash::Ledger};
use std::time::Duration;

fn rename_window(
    cx: &mut TestAppContext,
) -> (
    Entity<RenameWindow>,
    tempfile::TempDir,
    &mut VisualTestContext,
) {
    let fixture = tempfile::tempdir().expect("test directory");
    let path = fixture.path().join("项目说明.txt");
    std::fs::write(&path, "layout preview").expect("test file");
    cx.update(gpui_component::init);
    let mut rename = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let resizable = cx.new(|_| ResizableState::default());
        let search = cx.new(|cx| InputState::new(window, cx));
        let pane = cx.new(|cx| {
            ExplorerPane::new(resizable, search, None, Ledger::KeptByOs, cx.focus_handle())
        });
        let entries = vec![FileEntryDto {
            name: "项目说明.txt".into(),
            path: path.to_string_lossy().into_owned(),
            kind: "file".into(),
            size: 10,
            modified: 0,
        }];
        let view = cx.new(|cx| RenameWindow::new(entries, pane.downgrade(), window, cx));
        rename = Some(view.clone());
        Root::new(view, window, cx)
    });
    (rename.expect("rename view"), fixture, cx)
}

#[gpui::test]
fn batch_rename_layout_keeps_preview_and_footer_visible_at_supported_sizes(
    cx: &mut TestAppContext,
) {
    let (_view, _fixture, cx) = rename_window(cx);
    for dimensions in [(1220.0, 820.0), (1020.0, 660.0)] {
        let size = size(px(dimensions.0), px(dimensions.1));
        cx.simulate_resize(size);
        cx.run_until_parked();
        let settings = cx.debug_bounds("rename-settings").expect("settings panel");
        let preview = cx.debug_bounds("rename-preview").expect("preview panel");
        let footer = cx.debug_bounds("rename-footer").expect("footer");
        let name = cx.debug_bounds("rename-field-0").expect("name input");
        assert!(
            settings.right() <= preview.left(),
            "panels must not overlap"
        );
        assert!(
            preview.size.width >= px(600.0),
            "preview needs readable columns"
        );
        assert!(
            preview.bottom() <= footer.top(),
            "footer must stay outside preview"
        );
        assert!(
            footer.bottom() <= size.height,
            "footer must stay inside window"
        );
        assert!(name.top() >= settings.top() && name.bottom() <= settings.bottom());
        assert!(
            name.size.height >= px(40.0),
            "input label and control must not collapse"
        );
    }
}

#[gpui::test]
async fn batch_rename_name_input_updates_preview_without_losing_extension(cx: &mut TestAppContext) {
    let (view, fixture, cx) = rename_window(cx);
    let parent = fixture
        .path()
        .file_name()
        .expect("directory name")
        .to_string_lossy();
    let name = cx.debug_bounds("rename-field-0").expect("name input");
    cx.simulate_click(
        point(name.center().x, name.bottom() - px(12.0)),
        Modifiers::none(),
    );
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("[B]_[N]_[C]");
    cx.background_executor
        .timer(Duration::from_millis(150))
        .await;
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert_eq!("[B]_[N]_[C]", view.inputs[0].read(cx).value().as_ref());
        assert!(!view.pending);
        assert_eq!(1, view.rows.len());
        assert_eq!(format!("{parent}_项目说明_001.txt"), view.rows[0].name);
        assert!(view.can_apply());
    });
}
