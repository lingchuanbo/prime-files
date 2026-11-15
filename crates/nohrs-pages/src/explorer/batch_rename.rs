mod chrome;
mod controls;
mod history;
mod operations;
mod plan;
mod preview;
mod preview_state;
mod rules;
mod scan;
mod scope;
mod scope_controls;
#[cfg(test)]
mod scope_tests;
mod session;
mod transaction;
mod window;
use super::{ExplorerPane, StatusLevel};
use gpui::{Context, Window};
use nohrs_core::telemetry::LogErr;

impl ExplorerPane {
    pub(crate) fn open_batch_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries: Vec<_> = self
            .filtered_entries
            .iter()
            .filter(|entry| self.selection.contains(&entry.path))
            .cloned()
            .collect();
        if entries.is_empty() {
            self.set_status(StatusLevel::Info, "请先选中需要重命名的文件或文件夹");
            cx.notify();
            return;
        }
        let pane = cx.entity().downgrade();
        window.defer(cx, move |_, cx| {
            if let Err(error) = window::open(entries, pane.clone(), cx) {
                pane.update(cx, |pane, cx| {
                    pane.set_status(StatusLevel::Error, format!("无法打开批量重命名：{error}"));
                    cx.notify();
                })
                .log_err();
            }
        });
    }
}
#[cfg(test)]
mod tests;
