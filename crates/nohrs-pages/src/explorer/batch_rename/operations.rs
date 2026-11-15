use super::{plan, scan, transaction, window::RenameWindow};
use crate::explorer::ExplorerPane;
use gpui::*;
use nohrs_core::telemetry::LogErr;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

impl RenameWindow {
    pub(super) fn can_apply(&self) -> bool {
        !self.busy
            && !self.pending
            && !self.applied
            && !self.scope.scanning
            && self.scope.ready
            && self.rows.iter().all(|row| row.error.is_none())
            && self.rows.iter().any(|row| row.selected && row.changed())
    }

    pub(super) fn apply(&mut self, cx: &mut Context<Self>) {
        if !self.can_apply() {
            return;
        }
        self.remember(cx);
        let changes: Vec<_> = self
            .rows
            .iter()
            .filter(|row| row.selected && row.changed())
            .map(plan::Preview::change)
            .collect();
        self.busy = true;
        self.status = "正在重命名，请稍候…".into();
        let pane = self.pane.clone();
        cx.spawn(async move |this, cx| {
            let (changes, result) = cx
                .background_spawn(async move {
                    let result = transaction::execute(&changes);
                    (changes, result)
                })
                .await;
            refresh_pane(&pane, &changes, result.is_ok(), cx);
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(undo) => {
                        this.update_entries(&changes);
                        this.undo = Some(Arc::new(undo));
                        this.applied = true;
                        this.status = format!(
                            "已重命名 {} 项，可撤销本次操作（关闭窗口后失效）",
                            changes.len()
                        );
                    }
                    Err(error) => {
                        this.status = format!("重命名失败：{error}");
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn undo(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.pending || self.scope.scanning {
            return;
        }
        let Some(record) = self.undo.clone() else {
            return;
        };
        self.busy = true;
        self.status = "正在撤销…".into();
        let pane = self.pane.clone();
        let changes = record.reverse_changes();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { transaction::undo(&record) })
                .await;
            refresh_pane(&pane, &changes, result.is_ok(), cx);
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        this.update_entries(&changes);
                        this.undo = None;
                        this.schedule_preview(cx);
                    }
                    Err(error) => {
                        this.status = format!("撤销失败：{error}");
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn update_entries(&mut self, changes: &[(std::path::PathBuf, std::path::PathBuf)]) {
        let mapping = change_mapping(changes);
        for entry in self
            .entries
            .iter_mut()
            .chain(self.scope.original.iter_mut())
        {
            if let Some(target) = mapping.get(&scan::path_key(Path::new(&entry.path))) {
                entry.path = target.clone();
                entry.name = Path::new(target)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
            }
        }
    }
}

fn refresh_pane(
    pane: &WeakEntity<ExplorerPane>,
    changes: &[(std::path::PathBuf, std::path::PathBuf)],
    success: bool,
    cx: &mut AsyncApp,
) {
    pane.update(cx, |pane, cx| {
        if success {
            let mapping = change_mapping(changes);
            crate::explorer::tags::Tags::moved_batch(&mapping, cx);
            pane.selection = pane
                .selection
                .iter()
                .map(|path| {
                    mapping
                        .get(&scan::path_key(Path::new(path)))
                        .unwrap_or(path)
                        .clone()
                })
                .collect();
            if let Some(active) = &mut pane.active_path
                && let Some(target) = mapping.get(&scan::path_key(Path::new(active)))
            {
                *active = target.clone();
            }
        }
        pane.reload(cx);
    })
    .log_err();
}

fn change_mapping(changes: &[(PathBuf, PathBuf)]) -> HashMap<String, String> {
    // 同时保留扩展路径和普通路径的键，主界面与工作空间可能使用不同拼写。
    changes
        .iter()
        .flat_map(|(source, target)| {
            [
                (
                    source.to_string_lossy().into_owned(),
                    target.to_string_lossy().into_owned(),
                ),
                (scan::path_key(source), scan::path_text(target)),
            ]
        })
        .collect()
}
