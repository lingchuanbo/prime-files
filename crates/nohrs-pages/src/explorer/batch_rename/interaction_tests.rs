#![allow(clippy::disallowed_methods)]
use super::{RenameWindow, rename_window};
use crate::explorer::batch_rename::{
    rules::CaseMode,
    session::{Session, SessionStore},
};
use gpui::{AppContext, Modifiers, TestAppContext, point, px};
use std::time::Duration;

#[gpui::test]
fn closing_restores_all_raw_fields_and_options_on_reopen(cx: &mut TestAppContext) {
    let (view, fixture, cx) = rename_window(cx);
    let values = ["[拼音]_[C]", "", "中国", "", "invalid", "0", ""].map(String::from);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            for (input, value) in view.inputs.iter().zip(&values) {
                input.update(cx, |input, cx| input.set_value(value.clone(), window, cx));
            }
            view.case = CaseMode::Upper;
            view.first_only = true;
            view.include_extension = true;
        })
    });
    assert!(cx.simulate_close());
    let bytes = std::fs::read(fixture.path().join("batch-rename.json")).expect("saved on close");
    let saved: Session = serde_json::from_slice(&bytes).expect("saved state");
    assert_eq!(values, saved.values);
    // 模拟应用重启：丢弃内存状态，再从临时状态文件恢复。
    cx.update(|window, cx| {
        cx.remove_global::<SessionStore>();
        cx.set_global(SessionStore {
            data: saved,
            path: None,
        });
        let (entries, pane) = (view.read(cx).entries.clone(), view.read(cx).pane.clone());
        let reopened = cx.new(|cx| RenameWindow::new(entries, pane, window, cx));
        let reopened = reopened.read(cx);
        assert_eq!(
            values,
            std::array::from_fn(|index| reopened.inputs[index].read(cx).value().to_string())
        );
        assert_eq!(CaseMode::Upper, reopened.case);
        assert!(reopened.first_only && reopened.include_extension);
    });
}

#[gpui::test]
fn busy_window_rejects_native_close_without_recording_history(cx: &mut TestAppContext) {
    let (view, _fixture, cx) = rename_window(cx);
    view.update(cx, |view, _| view.busy = true);
    assert!(!cx.simulate_close());
    cx.update(|_, cx| assert!(cx.global::<SessionStore>().data.history[0].is_empty()));
}

#[gpui::test]
async fn history_arrow_and_keyboard_restore_rule_and_refresh_preview(cx: &mut TestAppContext) {
    let (view, _fixture, cx) = rename_window(cx);
    cx.update(|_, cx| {
        cx.global_mut::<SessionStore>().data.history[0] = vec!["[拼音]".into(), "[N]_[C]".into()];
        view.update(cx, |_, cx| cx.notify());
    });
    cx.run_until_parked();
    let field = cx.debug_bounds("rename-field-0").expect("name field");
    cx.simulate_click(
        point(field.right() - px(23.0), field.bottom() - px(16.0)),
        Modifiers::none(),
    );
    cx.simulate_keystrokes("down enter");
    cx.background_executor
        .timer(Duration::from_millis(150))
        .await;
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert_eq!("[拼音]", view.inputs[0].read(cx).value().as_ref());
        assert_eq!("xiangmushuoming.txt", view.rows[0].name);
    });
    cx.simulate_keystrokes("down down down enter");
    cx.background_executor
        .timer(Duration::from_millis(150))
        .await;
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert_eq!("[N]_[C]", view.inputs[0].read(cx).value().as_ref());
        assert_eq!("项目说明_001.txt", view.rows[0].name);
    });
}
