use super::{
    chrome, controls, plan, preview,
    rules::{CaseMode, Rules},
    scope::Scope,
    session::{self, SessionStore},
    transaction,
};
use crate::explorer::ExplorerPane;
use gpui::{prelude::*, *};
use gpui_component::{
    Root,
    input::{InputEvent, InputState},
};
use nohrs_core::telemetry::LogErr;
use nohrs_services::fs::listing::FileEntryDto;
use nohrs_ui::theme::theme;
use std::sync::Arc;

pub(super) struct RenameWindow {
    pub inputs: Vec<Entity<InputState>>,
    pub rows: Arc<Vec<plan::Preview>>,
    pub entries: Vec<FileEntryDto>,
    pub case: CaseMode,
    pub first_only: bool,
    pub include_extension: bool,
    pub busy: bool,
    pub pending: bool,
    pub applied: bool,
    pub status: String,
    pub undo: Option<Arc<transaction::Undo>>,
    pub(super) generation: u64,
    pub scope: Scope,
    pub(super) pane: WeakEntity<ExplorerPane>,
    _subscriptions: Vec<Subscription>,
    pub(super) preview_task: Option<Task<()>>,
}

impl RenameWindow {
    fn new(
        entries: Vec<FileEntryDto>,
        pane: WeakEntity<ExplorerPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let saved = SessionStore::current(cx);
        let scope = Scope::new(saved.scope, &entries, window, cx);
        let inputs = create_inputs(saved.values, window, cx);
        let mut subscriptions: Vec<_> = inputs
            .iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) && !this.busy {
                        this.schedule_preview(cx);
                    }
                })
            })
            .collect();
        subscriptions.extend(scope.subscriptions(cx));
        subscriptions.push(session::install_hooks(window, cx));
        let focus = inputs[0].clone();
        window
            .on_next_frame(move |window, cx| focus.update(cx, |input, cx| input.focus(window, cx)));
        Self {
            inputs,
            scope,
            rows: Arc::new(Vec::new()),
            entries,
            case: saved.case,
            first_only: saved.first_only,
            include_extension: saved.include_extension,
            busy: false,
            pending: true,
            applied: false,
            status: "正在生成预览…".into(),
            undo: None,
            generation: 0,
            pane,
            _subscriptions: subscriptions,
            preview_task: None,
        }
    }

    pub(super) fn rules(&self, cx: &App) -> Result<Rules, String> {
        let values: Vec<_> = self
            .inputs
            .iter()
            .map(|input| input.read(cx).value().to_string())
            .collect();
        Ok(Rules {
            name: values[0].clone(),
            extension: values[1].clone(),
            find: values[2].clone(),
            replacement: values[3].clone(),
            start: values[4].parse().map_err(|_| "起始值应为非负整数")?,
            step: values[5].parse().map_err(|_| "步长应为非负整数")?,
            digits: values[6].parse().map_err(|_| "位数应为 1–18 的整数")?,
            case: self.case,
            first_only: self.first_only,
            include_extension: self.include_extension,
        })
    }
}

impl Render for RenameWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::bg()))
            .text_color(rgb(theme::fg()))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                if key == "escape" && !this.busy {
                    this.remember(cx);
                    window.remove_window();
                    cx.stop_propagation();
                }
                if key == "enter" && event.keystroke.modifiers.control {
                    this.apply(cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(controls::render(self, window, cx))
                    .child(preview::render(self, cx)),
            )
            .child(chrome::footer(self, cx))
    }
}

#[derive(Default)]
struct OpenRenameWindow(Option<(WindowHandle<Root>, WeakEntity<RenameWindow>)>);
impl Global for OpenRenameWindow {}

pub(super) fn open(
    entries: Vec<FileEntryDto>,
    pane: WeakEntity<ExplorerPane>,
    cx: &mut App,
) -> anyhow::Result<()> {
    if reuse_window(&entries, &pane, cx) {
        return Ok(());
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1220.0), px(820.0)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("Prime Files · 批量重命名".into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(1020.0), px(660.0))),
        ..Default::default()
    };
    let mut current_view = None;
    let handle = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| RenameWindow::new(entries, pane, window, cx));
        current_view = Some(view.downgrade());
        view.update(cx, |view, cx| view.rescan(cx));
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    cx.set_global(OpenRenameWindow(current_view.map(|view| (handle, view))));
    Ok(())
}

fn create_inputs(
    values: [String; 7],
    window: &mut Window,
    cx: &mut App,
) -> Vec<Entity<InputState>> {
    let placeholders = [
        "例如：[B]_[N]_[C]",
        "[E] 保留原扩展名",
        "要替换的文字",
        "留空可删除匹配的文字",
        "1",
        "1",
        "3",
    ];
    values
        .into_iter()
        .zip(placeholders)
        .map(|(value, placeholder)| {
            cx.new(|cx| {
                let mut input = InputState::new(window, cx).placeholder(placeholder);
                input.set_value(value, window, cx);
                input
            })
        })
        .collect()
}

fn reuse_window(entries: &[FileEntryDto], pane: &WeakEntity<ExplorerPane>, cx: &mut App) -> bool {
    let Some((handle, view)) = cx
        .try_global::<OpenRenameWindow>()
        .and_then(|state| state.0.clone())
    else {
        return false;
    };
    handle
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| {
                if !view.busy {
                    view.scope.original = entries.to_vec();
                    view.scope.excluded.clear();
                    let roots = super::scope::initial_roots(entries);
                    view.scope.inputs[0].update(cx, |input, cx| input.set_value(roots, window, cx));
                    view.pane = pane.clone();
                    view.undo = None;
                    view.rescan(cx);
                }
            })
            .log_err();
            window.activate_window();
        })
        .is_ok()
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
