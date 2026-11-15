use super::{
    scan::{self, Progress},
    window::RenameWindow,
};
use gpui::*;
use gpui_component::input::{InputEvent, InputState};
use nohrs_core::telemetry::LogErr;
use nohrs_services::fs::listing::FileEntryDto;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Options {
    pub directory_mode: bool,
    pub recursive: bool,
    pub restart_counter: bool,
    pub values: [String; 2],
    pub history: [Vec<String>; 2],
}

pub(super) struct Scope {
    pub options: Options,
    pub inputs: Vec<Entity<InputState>>,
    pub original: Vec<FileEntryDto>,
    pub roots: Vec<PathBuf>,
    pub excluded: HashSet<String>,
    pub issues: Vec<String>,
    pub scanning: bool,
    pub ready: bool,
    pub progress: Progress,
    generation: u64,
    task: Option<Task<()>>,
}

impl Scope {
    pub fn new(
        mut options: Options,
        entries: &[FileEntryDto],
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let roots = initial_roots(entries);
        if !roots.is_empty() {
            options.values[0] = roots;
        }
        let inputs = options
            .values
            .iter()
            .zip([
                "完整目录路径；多个目录用 | 分隔",
                "全部文件，例如 png 或 *.png;*.jpg",
            ])
            .map(|(value, placeholder)| {
                cx.new(|cx| {
                    let mut input = InputState::new(window, cx).placeholder(placeholder);
                    input.set_value(value.clone(), window, cx);
                    input
                })
            })
            .collect();
        Self {
            options,
            inputs,
            original: entries.to_vec(),
            roots: Vec::new(),
            excluded: HashSet::new(),
            issues: Vec::new(),
            scanning: false,
            ready: true,
            progress: Progress::default(),
            generation: 0,
            task: None,
        }
    }

    pub fn subscriptions(&self, cx: &mut Context<RenameWindow>) -> Vec<Subscription> {
        self.inputs
            .iter()
            .map(|input| {
                cx.subscribe(input, |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change)
                        && !view.busy
                        && view.scope.options.directory_mode
                    {
                        view.rescan(cx);
                    }
                })
            })
            .collect()
    }

    pub fn cancel(&mut self) {
        self.progress.cancel.store(true, Ordering::Relaxed);
        self.generation = self.generation.wrapping_add(1);
        self.task = None;
        self.scanning = false;
    }

    pub fn directory_label(&self, path: &str) -> String {
        let parent = Path::new(path).parent().unwrap_or(Path::new(""));
        let root = self.roots.iter().find(|root| parent.starts_with(root));
        match root.and_then(|root| parent.strip_prefix(root).ok()) {
            Some(relative) if relative.as_os_str().is_empty() => "（根目录）".into(),
            Some(relative) => relative.to_string_lossy().into_owned(),
            None => parent.to_string_lossy().into_owned(),
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.progress.cancel.store(true, Ordering::Relaxed);
    }
}

pub(super) fn initial_roots(entries: &[FileEntryDto]) -> String {
    let mut roots: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let path = Path::new(&entry.path);
            if entry.kind == "dir" {
                Some(path)
            } else {
                path.parent()
            }
        })
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    roots.sort();
    roots.dedup();
    roots.join(" | ")
}

impl RenameWindow {
    pub(super) fn rescan(&mut self, cx: &mut Context<Self>) {
        self.scope.cancel();
        self.scope.excluded.clear();
        self.scope.issues.clear();
        self.scope.roots.clear();
        if !self.scope.options.directory_mode {
            self.entries = self.scope.original.clone();
            self.scope.ready = true;
            self.schedule_preview(cx);
            return;
        }
        self.begin_scan(cx);
    }

    fn begin_scan(&mut self, cx: &mut Context<Self>) {
        self.cancel_preview();
        self.scope.scanning = true;
        self.scope.ready = false;
        self.scope.progress = Progress::default();
        self.entries.clear();
        self.rows = Default::default();
        self.status = "正在扫描目录…".into();
        let generation = self.scope.generation;
        let roots = self.scope.inputs[0].read(cx).value().to_string();
        let formats = self.scope.inputs[1].read(cx).value().to_string();
        let recursive = self.scope.options.recursive;
        let progress = self.scope.progress.clone();
        self.scope.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let result =
                cx.background_spawn(
                    async move { scan::scan(&roots, recursive, &formats, &progress) },
                );
            let ticker = progress_ticker(this.clone(), generation, cx);
            let result = result.await;
            drop(ticker);
            this.update(cx, |view, cx| view.finish_scan(generation, result, cx))
                .log_err();
        }));
        cx.notify();
    }

    fn finish_scan(
        &mut self,
        generation: u64,
        result: Result<scan::ScanResult, String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.scope.generation {
            return;
        }
        self.scope.scanning = false;
        match result {
            Ok(result) => {
                self.entries = result.entries;
                self.scope.roots = result.roots;
                self.scope.issues = result.issues;
                self.scope.ready = true;
                self.schedule_preview(cx);
            }
            Err(error) => {
                self.status = error;
                self.pending = false;
            }
        }
        cx.notify();
    }

    pub(super) fn stop_scan(&mut self, cx: &mut Context<Self>) {
        self.scope.cancel();
        self.scope.ready = false;
        self.status = "已取消扫描，请重新扫描后再执行".into();
        self.pending = false;
        cx.notify();
    }

    pub(super) fn select_all(&mut self, selected: bool, cx: &mut Context<Self>) {
        self.scope.excluded = if selected {
            HashSet::new()
        } else {
            self.entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect()
        };
        self.schedule_preview(cx);
    }

    pub(super) fn select_file(&mut self, path: &str, selected: bool, cx: &mut Context<Self>) {
        if selected {
            self.scope.excluded.remove(path);
        } else {
            self.scope.excluded.insert(path.to_owned());
        }
        self.schedule_preview(cx);
    }
}

fn progress_ticker(view: WeakEntity<RenameWindow>, generation: u64, cx: &mut AsyncApp) -> Task<()> {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let keep_running = view
                .update(cx, |view, cx| {
                    if generation != view.scope.generation || !view.scope.scanning {
                        return false;
                    }
                    view.status = format!(
                        "正在扫描目录 · 已找到 {} 个文件",
                        view.scope.progress.found.load(Ordering::Relaxed)
                    );
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !keep_running {
                break;
            }
        }
    })
}
