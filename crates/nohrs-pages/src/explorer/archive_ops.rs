use std::path::{Path, PathBuf};

use gpui::{AppContext as _, Context, Window};

use super::{ExplorerPane, StatusLevel};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    SevenZip,
    ZipFast,
    ZipMax,
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Self::SevenZip => "7z",
            Self::ZipFast | Self::ZipMax => "zip",
        }
    }

    fn level(self) -> &'static str {
        match self {
            Self::SevenZip | Self::ZipMax => "9",
            Self::ZipFast => "0",
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Self::SevenZip => "7z",
            Self::ZipFast | Self::ZipMax => "zip",
        }
    }
}

pub(crate) fn compress(paths: &[String], cwd: &str, format: Format) -> Result<PathBuf, String> {
    let working_directory = Path::new(cwd);
    let source_paths: Vec<PathBuf> = paths
        .iter()
        .map(|path| {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                working_directory.join(path)
            }
        })
        .collect();
    if source_paths.is_empty() {
        return Err("没有可压缩的项目".into());
    }
    let base = source_paths
        .first()
        .and_then(|path| path.file_stem())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "archive".into());
    let output = Path::new(cwd).join(format!("{base}.{}", format.extension()));
    if output.exists() {
        return Err(format!("目标文件已存在：{}", output.display()));
    }
    let seven = super::preview_listing::seven_zip()
        .ok_or_else(|| "未找到 7z.exe，请先安装 7-Zip".to_string())?;
    let mut command = super::preview_listing::hidden_command(&seven);
    command
        .current_dir(cwd)
        .arg("a")
        .arg(format!("-t{}", format.kind()))
        .arg(format!("-mx={}", format.level()))
        .arg(&output)
        .arg("--");
    for path in &source_paths {
        if !path.exists() {
            return Err(format!("项目不存在：{}", path.display()));
        }
        command.arg(path);
    }
    let output_status = command.output().map_err(|error| error.to_string())?;
    if output_status.status.success() {
        Ok(output)
    } else {
        Err(command_error(&output_status.stderr, &output_status.stdout))
    }
}

pub(crate) fn extract(archive: &str) -> Result<PathBuf, String> {
    let archive = Path::new(archive);
    if !archive.is_file() {
        return Err(format!("压缩文件不存在：{}", archive.display()));
    }
    let seven = super::preview_listing::seven_zip()
        .ok_or_else(|| "未找到 7z.exe，请先安装 7-Zip".to_string())?;
    let parent = archive.parent().unwrap_or_else(|| Path::new("."));
    let stem = archive
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "解压内容".into());
    let target = unique_directory(parent, &stem);
    std::fs::create_dir_all(&target).map_err(|error| error.to_string())?;
    let output = super::preview_listing::hidden_command(&seven)
        .arg("x")
        .arg("-y")
        .arg(format!("-o{}", target.display()))
        .arg("--")
        .arg(archive)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(target)
    } else {
        let _ = std::fs::remove_dir(&target);
        Err(command_error(&output.stderr, &output.stdout))
    }
}

fn unique_directory(parent: &Path, stem: &str) -> PathBuf {
    let first = parent.join(stem);
    if !first.exists() {
        return first;
    }
    for index in 1..10_000 {
        let candidate = parent.join(format!("{stem} ({index})"));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{stem}-解压"))
}

fn command_error(stderr: &[u8], stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr).trim().to_string();
    if text.is_empty() {
        String::from_utf8_lossy(stdout).trim().to_string()
    } else {
        text
    }
}

impl ExplorerPane {
    pub(crate) fn compress_active(&mut self, format: Format, cx: &mut Context<Self>) {
        let Some(index) = self.active_row() else {
            self.set_status(StatusLevel::Error, "没有当前项目");
            cx.notify();
            return;
        };
        let Some(entry) = self.filtered_entries.get(index) else {
            return;
        };
        if entry.kind != "dir" {
            self.set_status(StatusLevel::Error, "请将光标放在要压缩的目录上");
            cx.notify();
            return;
        }
        let path = entry.path.clone();
        let cwd = self.cwd.clone();
        self.run_archive_task("正在压缩…", cx, async move {
            compress(&[path], &cwd, format)
        });
    }

    pub(crate) fn compress_selection(&mut self, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        let cwd = self.cwd.clone();
        self.run_archive_task("正在压缩所选项目…", cx, async move {
            compress(&paths, &cwd, Format::SevenZip)
        });
    }

    fn run_archive_task<F>(&mut self, status: &str, cx: &mut Context<Self>, task: F)
    where
        F: std::future::Future<Output = Result<PathBuf, String>> + Send + 'static,
    {
        self.set_status(StatusLevel::Info, status);
        let work = cx.background_spawn(task);
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |pane, cx| {
                match result {
                    Ok(path) => {
                        pane.set_status(StatusLevel::Info, format!("已完成：{}", path.display()));
                        pane.reload(cx);
                    }
                    Err(error) => pane.set_status(StatusLevel::Error, error),
                }
                cx.notify();
            })
        })
        .detach();
    }

    pub(crate) fn extract_active_and_navigate(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.active_row() else {
            self.set_status(StatusLevel::Error, "没有当前项目");
            cx.notify();
            return;
        };
        let Some(entry) = self.filtered_entries.get(index) else {
            return;
        };
        if !super::archive::is_archive_file(&entry.path) {
            self.set_status(StatusLevel::Error, "当前项目不是支持的压缩文件");
            cx.notify();
            return;
        }
        let archive = entry.path.clone();
        let work = cx.background_spawn(async move { extract(&archive) });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            this.update_in(cx, |pane, window, cx| {
                match result {
                    Ok(path) => pane.change_dir(path.to_string_lossy().into_owned(), window, cx),
                    Err(error) => pane.set_status(StatusLevel::Error, error),
                }
                cx.notify();
            })
        })
        .detach();
    }
}
