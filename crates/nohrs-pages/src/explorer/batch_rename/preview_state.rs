use super::{plan, window::RenameWindow};
use gpui::*;
use nohrs_core::telemetry::LogErr;
use std::{sync::Arc, time::Duration};

impl RenameWindow {
    pub(super) fn cancel_preview(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.preview_task = None;
        self.pending = false;
        self.applied = false;
    }

    pub(super) fn schedule_preview(&mut self, cx: &mut Context<Self>) {
        if self.scope.scanning || !self.scope.ready {
            return;
        }
        self.cancel_preview();
        self.pending = true;
        let generation = self.generation;
        let rules = if self.list_mode {
            None
        } else {
            Some(self.rules(cx))
        };
        let list_mode = self.list_mode;
        let list_lines = self.list_lines(cx);
        // A list still being typed, or one longer than the files, is said in
        // the status line; it is not a red row on every file.
        let list_blank = list_lines.iter().all(|line| line.trim().is_empty());
        let extra_lines = list_lines
            .iter()
            .skip(self.entries.len())
            .filter(|line| !line.trim().is_empty())
            .count();
        let list_apply = self.list_apply;
        let entries = self.entries.clone();
        let excluded = self.scope.excluded.clone();
        let restart = self.scope.options.restart_counter;
        self.status = "正在生成预览…".into();
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(100)).await;
            let result = cx.background_spawn(async move {
                if list_mode {
                    Ok(plan::preview_list(&entries, &list_lines, list_apply, &excluded))
                } else {
                    // Rule mode always carries its rules: the other branch is the
                    // list mode, which has none (this runs on a background thread).
                    #[allow(clippy::expect_used)]
                    rules.expect("rule mode has rules").map(|rules| plan::preview_selected(&entries, &rules, &excluded, restart))
                }
            }).await;
            this.update(cx, |view, cx| {
                if generation != view.generation { return; }
                view.pending = false;
                match result {
                    Ok(rows) => {
                        let selected = rows.iter().filter(|row| row.selected).count();
                        let errors = rows.iter().filter(|row| row.error.is_some()).count();
                        let changed = rows.iter().filter(|row| row.selected && row.changed() && row.error.is_none()).count();
                        view.status = if list_mode && list_blank {
                            format!("列表为空 · 匹配 {} 项 · 在上方逐行填写新名称", rows.len())
                        } else {
                            format!("匹配 {} 项 · 已勾选 {selected} 项 · 将修改 {changed} 项 · 冲突 {errors} 项", rows.len())
                        };
                        if extra_lines > 0 {
                            view.status.push_str(&format!(" · 列表多出 {extra_lines} 行未使用"));
                        }
                        if !view.scope.issues.is_empty() { view.status.push_str(&format!(" · 扫描提示 {} 项（在预览上方查看）", view.scope.issues.len())); }
                        view.rows = Arc::new(rows);
                    }
                    Err(error) => { view.status = error; view.rows = Arc::new(Vec::new()); }
                }
                cx.notify();
            }).log_err();
        }));
        cx.notify();
    }
}
