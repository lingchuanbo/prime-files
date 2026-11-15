use super::{rules::CaseMode, window::RenameWindow};
use gpui::{App, Context, Global, Subscription, Window};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const HISTORY_LIMIT: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Session {
    pub values: [String; 7],
    pub case: CaseMode,
    pub first_only: bool,
    pub include_extension: bool,
    pub history: [Vec<String>; 7],
    pub scope: super::scope::Options,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            values: ["[N]", "[E]", "", "", "1", "1", "3"].map(String::from),
            case: CaseMode::Keep,
            first_only: false,
            include_extension: false,
            history: Default::default(),
            scope: Default::default(),
        }
    }
}

impl Session {
    fn record(&mut self) {
        for (history, value) in self.history.iter_mut().zip(&self.values) {
            // 空值也属于可恢复的输入，但不占历史菜单条目。
            if !value.is_empty() {
                history.retain(|item| item != value);
                history.insert(0, value.clone());
            }
            history.truncate(HISTORY_LIMIT);
        }
        for (history, value) in self.scope.history.iter_mut().zip(&self.scope.values) {
            if !value.is_empty() {
                history.retain(|item| item != value);
                history.insert(0, value.clone());
            }
            history.truncate(HISTORY_LIMIT);
        }
    }
}

pub(super) struct SessionStore {
    pub data: Session,
    pub path: Option<PathBuf>,
}
impl Global for SessionStore {}

impl SessionStore {
    pub fn current(cx: &mut App) -> Session {
        if !cx.has_global::<Self>() {
            let path = nohrs_core::config::paths::state_dir().join("batch-rename.json");
            cx.set_global(Self {
                data: read(&path),
                path: Some(path),
            });
        }
        cx.global::<Self>().data.clone()
    }
}

impl RenameWindow {
    pub(super) fn remember(&self, cx: &mut App) {
        let mut data = SessionStore::current(cx);
        data.values = std::array::from_fn(|index| self.inputs[index].read(cx).value().to_string());
        data.case = self.case;
        data.first_only = self.first_only;
        data.include_extension = self.include_extension;
        data.scope = self.scope.options.clone();
        data.scope.values =
            std::array::from_fn(|index| self.scope.inputs[index].read(cx).value().to_string());
        data.record();
        let store = cx.global_mut::<SessionStore>();
        store.data = data;
        if let Some(path) = &store.path
            && let Err(error) = write(path, &store.data)
        {
            tracing::error!(%error, "无法保存批量重命名输入和历史记录");
        }
    }
}

pub(super) fn install_hooks(window: &mut Window, cx: &mut Context<RenameWindow>) -> Subscription {
    let weak = cx.weak_entity();
    window.on_window_should_close(cx, move |_, cx| {
        weak.update(cx, |view, cx| {
            if view.busy {
                return false;
            }
            view.remember(cx);
            true
        })
        .unwrap_or(true)
    });
    cx.on_app_quit(|view, cx| {
        view.remember(cx);
        async {}
    })
}

// 仅在打开 / 关闭窗口或执行重命名时读写这一小份状态，避免逐键写磁盘。
#[allow(clippy::disallowed_methods)]
fn read(path: &Path) -> Session {
    let result = std::fs::read(path)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| Ok(serde_json::from_slice::<Session>(&bytes)?));
    match result {
        Ok(mut data) => {
            for history in data.history.iter_mut().chain(data.scope.history.iter_mut()) {
                history.truncate(HISTORY_LIMIT);
            }
            data
        }
        Err(error) => {
            if !error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                tracing::warn!(%error, "无法读取批量重命名状态，使用默认值");
            }
            Session::default()
        }
    }
}

#[allow(clippy::disallowed_methods)]
fn write(path: &Path, data: &Session) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(data)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
