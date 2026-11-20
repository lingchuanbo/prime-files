use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, Context, Window};
use nohrs_core::config::ArchiveTool;
use nohrs_core::telemetry::LogErr as _;
use nohrs_services::fs::transfer::TransferProgress;

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

pub(crate) fn compress(
    paths: &[String],
    cwd: &str,
    format: Format,
    progress: &TransferProgress,
) -> Result<PathBuf, String> {
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
    progress.set_detail(output.file_name().unwrap_or_default().to_string_lossy().into_owned());
    if let Err(error) = run_tool(&mut command, progress) {
        // A cancelled or failed compress leaves a half-written archive; it is
        // not the one the user asked for.
        let _ = std::fs::remove_file(&output);
        return Err(error);
    }
    Ok(output)
}

pub(crate) fn extract(
    archive: &str,
    tool: ArchiveTool,
    progress: &TransferProgress,
) -> Result<PathBuf, String> {
    let archive = Path::new(archive);
    if !archive.is_file() {
        return Err(format!("压缩文件不存在：{}", archive.display()));
    }
    let hatchzip = hatchzip();
    let seven = super::preview_listing::seven_zip();
    let chosen = pick_tool(
        &archive.file_name().unwrap_or_default().to_string_lossy(),
        tool,
        hatchzip.is_some(),
        seven.is_some(),
    )
    .ok_or_else(|| {
        format!(
            "未找到可用的解压程序：请把 HatchZip 放到 {}，或安装 7-Zip",
            tools_hint()
        )
    })?;
    let parent = archive.parent().unwrap_or_else(|| Path::new("."));
    let stem = archive
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "解压内容".into());
    let target = unique_directory(parent, &stem);
    std::fs::create_dir_all(&target).map_err(|error| error.to_string())?;
    progress.set_detail(archive.file_name().unwrap_or_default().to_string_lossy().into_owned());
    let result = match chosen {
        Tool::HatchZip => extract_with_hatchzip(
            hatchzip.as_deref().unwrap_or(Path::new("hatchzip.exe")),
            archive,
            &target,
            progress,
        ),
        Tool::SevenZip => extract_with_7z(
            seven.as_deref().unwrap_or(Path::new("7z.exe")),
            archive,
            &target,
            progress,
        ),
    };
    if let Err(error) = result {
        // Whatever the tool wrote before failing or being cancelled is a
        // partial extraction, not an archive.
        let _ = std::fs::remove_dir_all(&target);
        return Err(error);
    }
    Ok(target)
}

/// Which program unpacks an archive file of this name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    HatchZip,
    SevenZip,
}

/// Picks the extractor for `name`: the configured one when it can read the
/// file and is installed, the other when only it can, and `None` when neither
/// is there. HatchZip reads zip, 7z, rar and the tar family, plus split
/// archives, but not the standalone compressed streams (`.gz`, `.xz`, `.bz2`,
/// `.zst`) nor `.cab`/`.iso`, which stay with 7-Zip.
fn pick_tool(
    name: &str,
    configured: ArchiveTool,
    hatchzip_available: bool,
    seven_zip_available: bool,
) -> Option<Tool> {
    let hatchzip_fits = hatchzip_available && hatchzip_reads(name);
    match configured {
        ArchiveTool::HatchZip => {
            if hatchzip_fits {
                Some(Tool::HatchZip)
            } else if seven_zip_available {
                Some(Tool::SevenZip)
            } else {
                None
            }
        }
        ArchiveTool::SevenZip => {
            if seven_zip_available {
                Some(Tool::SevenZip)
            } else if hatchzip_fits {
                Some(Tool::HatchZip)
            } else {
                None
            }
        }
    }
}

/// Whether HatchZip reads a file with this name. The tar family is read in one
/// pass, so `.tar.gz`/`.tar.xz` (and their short forms) count; a bare `.gz` or
/// `.xz` is one compressed file, which HatchZip does not unpack yet.
fn hatchzip_reads(name: &str) -> bool {
    let name = name.to_lowercase();
    if name.ends_with(".tar.gz")
        || name.ends_with(".tgz")
        || name.ends_with(".tar.xz")
        || name.ends_with(".txz")
    {
        return true;
    }
    matches!(
        Path::new(&name)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or(""),
        "zip" | "7z" | "rar" | "tar"
    )
}

/// HatchZip's command line, in `tools\HatchZip` beside the program (or on
/// `PATH`, where its installer puts it).
fn hatchzip() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = &beside {
        candidates.push(dir.join("tools").join("HatchZip").join("hatchzip.exe"));
        candidates.push(dir.join("tools").join("hatchzip.exe"));
    }
    if let Some(paths) = std::env::var_os("PATH") {
        candidates.extend(
            std::env::split_paths(&paths)
                .flat_map(|dir| [dir.join("hatchzip.exe"), dir.join("hz.exe")]),
        );
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn tools_hint() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_default()
        .join("tools")
        .join("HatchZip")
        .display()
        .to_string()
}

// A normal extraction: the archive is kept exactly as it was, as 7-Zip did
// before it. `-y` and `--keep-archive` skip the confirmation HatchZip would
// otherwise ask a console nobody sees. The space it keeps free is smaller than
// its 2 GB default so an extraction on a nearly full disk still runs, as it
// does with 7-Zip, while the disk is not filled to zero.
fn extract_with_hatchzip(
    program: &Path,
    archive: &Path,
    target: &Path,
    progress: &TransferProgress,
) -> Result<(), String> {
    let mut command = super::preview_listing::hidden_command(program);
    command
        .arg("extract")
        .arg(archive)
        .arg("-o")
        .arg(target)
        .arg("--keep-archive")
        .arg("--keep-free")
        .arg("256MB")
        .arg("-y")
        .arg("-q");
    run_tool(&mut command, progress).map_err(|error| hatchzip_error(&error))
}

fn extract_with_7z(
    program: &Path,
    archive: &Path,
    target: &Path,
    progress: &TransferProgress,
) -> Result<(), String> {
    let mut command = super::preview_listing::hidden_command(program);
    command
        .arg("x")
        .arg("-y")
        .arg(format!("-o{}", target.display()))
        .arg("--")
        .arg(archive);
    run_tool(&mut command, progress)
}

/// Runs an archive tool to completion, polling so the dialog moves and a Cancel
/// kills it. Its output goes to a temporary log file rather than a pipe: a tool
/// that lists every extracted file would fill a pipe while nothing reads it, and
/// block before it ever exits.
fn run_tool(command: &mut std::process::Command, progress: &TransferProgress) -> Result<(), String> {
    let log = ToolLog::new()?;
    command
        .stdout(std::process::Stdio::from(log.writer()?))
        .stderr(std::process::Stdio::from(log.writer()?));
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    loop {
        if progress.cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            log.drop_file();
            return Err(String::new());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    log.drop_file();
                    return Ok(());
                }
                let text = log.read();
                log.drop_file();
                return Err(command_error(&text));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(80)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                log.drop_file();
                return Err(error.to_string());
            }
        }
    }
}

/// The temporary file an archive tool's output is collected in.
struct ToolLog {
    path: PathBuf,
}

impl ToolLog {
    fn new() -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "nohrs-archive-{}-{nanos}.log",
            std::process::id()
        ));
        std::fs::File::create(&path).map_err(|error| error.to_string())?;
        Ok(Self { path })
    }

    fn writer(&self) -> Result<std::fs::File, String> {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|error| error.to_string())
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }

    fn drop_file(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// HatchZip's error, as the status bar shows it: its own "error: " prefix off,
/// and the one message that matters to this form translated.
fn hatchzip_error(raw: &str) -> String {
    if raw.contains("password protected") || raw.contains("--password") {
        return "压缩包已加密：HatchZip 需要密码，暂不支持带密码解压".into();
    }
    let raw = raw.trim();
    raw.strip_prefix("error: ").unwrap_or(raw).to_string()
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

fn command_error(text: &str) -> String {
    text.trim().to_string()
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
        self.run_archive_task("压缩", "正在压缩…", cx, move |progress| {
            compress(&[path], &cwd, format, &progress)
        });
    }

    pub(crate) fn compress_selection(&mut self, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        let cwd = self.cwd.clone();
        self.run_archive_task("压缩", "正在压缩所选项目…", cx, move |progress| {
            compress(&paths, &cwd, Format::SevenZip, &progress)
        });
    }

    // Runs an archive tool on the background executor under the progress
    // dialog: the board it updates is what the dialog draws, and its Cancel
    // kills the tool (see `run_tool`).
    fn run_archive_task<F>(
        &mut self,
        title: &'static str,
        status: &str,
        cx: &mut Context<Self>,
        task: F,
    ) where
        F: FnOnce(Arc<TransferProgress>) -> Result<PathBuf, String> + Send + 'static,
    {
        self.set_status(StatusLevel::Info, status);
        let progress = self.begin_transfer(title, 1);
        let worker = progress.clone();
        let work = cx.background_spawn(async move { task(worker) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let cancelled = progress.cancelled();
            this.update(cx, |pane, cx| {
                pane.end_transfer(&progress);
                match result {
                    _ if cancelled => {
                        pane.set_status(StatusLevel::Info, format!("已取消：{title}"));
                        pane.reload(cx);
                    }
                    Ok(path) => {
                        pane.set_status(
                            StatusLevel::Info,
                            format!("已完成：{}", path.display()),
                        );
                        pane.reload(cx);
                    }
                    Err(error) => pane.set_status(StatusLevel::Error, error),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
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
        let tool = self.archive_tool;
        self.set_status(StatusLevel::Info, "正在解压…");
        let progress = self.begin_transfer("解压", 1);
        let worker = progress.clone();
        let work = cx.background_spawn(async move { extract(&archive, tool, &worker) });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let cancelled = progress.cancelled();
            this.update_in(cx, |pane, window, cx| {
                pane.end_transfer(&progress);
                match result {
                    _ if cancelled => {
                        pane.set_status(StatusLevel::Info, "已取消：解压");
                        pane.reload(cx);
                    }
                    Ok(path) => {
                        pane.change_dir(path.to_string_lossy().into_owned(), window, cx);
                    }
                    Err(error) => pane.set_status(StatusLevel::Error, error),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{Tool, hatchzip_reads, pick_tool};
    use nohrs_core::config::ArchiveTool;

    // The extraction helpers take a board; the tests only need one that is
    // never cancelled.
    fn no_progress() -> std::sync::Arc<nohrs_services::fs::transfer::TransferProgress> {
        nohrs_services::fs::transfer::TransferProgress::new(1)
    }

    #[test]
    fn hatchzip_reads_archives_but_not_bare_compressed_streams() {
        for name in [
            "a.zip",
            "A.ZIP",
            "a.7z",
            "a.rar",
            "a.tar",
            "a.tar.gz",
            "a.tgz",
            "a.tar.xz",
            "a.txz",
        ] {
            assert!(hatchzip_reads(name), "{name}");
        }
        for name in [
            "a.gz",
            "a.xz",
            "a.bz2",
            "a.tar.bz2",
            "a.zst",
            "a.cab",
            "a.iso",
            "a.txt",
        ] {
            assert!(!hatchzip_reads(name), "{name}");
        }
    }

    #[test]
    fn the_configured_tool_wins_and_the_other_fills_in() {
        // Both installed: the configured one.
        assert_eq!(
            pick_tool("a.zip", ArchiveTool::HatchZip, true, true),
            Some(Tool::HatchZip)
        );
        assert_eq!(
            pick_tool("a.zip", ArchiveTool::SevenZip, true, true),
            Some(Tool::SevenZip)
        );
        // A format only 7-Zip reads goes to it whatever the setting says.
        assert_eq!(
            pick_tool("a.iso", ArchiveTool::HatchZip, true, true),
            Some(Tool::SevenZip)
        );
        // The configured tool is missing: the other one, where it can read
        // the file.
        assert_eq!(
            pick_tool("a.7z", ArchiveTool::SevenZip, true, false),
            Some(Tool::HatchZip)
        );
        assert_eq!(
            pick_tool("a.zip", ArchiveTool::HatchZip, false, true),
            Some(Tool::SevenZip)
        );
        // Nothing that can read it: an error the caller reports.
        assert_eq!(pick_tool("a.iso", ArchiveTool::HatchZip, true, false), None);
        assert_eq!(pick_tool("a.zip", ArchiveTool::HatchZip, false, false), None);
    }

    /// The bundled HatchZip unpacks a zip with UTF-8 names, keeps the archive
    /// and leaves no temporary files. Skipped where the tools folder is not
    /// beside the build (CI and other platforms).
    #[test]
    fn bundled_hatchzip_unpacks_and_keeps_the_archive() {
        use std::io::Write as _;

        let program = super::hatchzip().or_else(|| {
            let beside_build = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("target")
                .join("release")
                .join("tools")
                .join("HatchZip")
                .join("hatchzip.exe");
            beside_build.is_file().then_some(beside_build)
        });
        let Some(program) = program else {
            return;
        };
        let root = tempfile::tempdir().expect("temp folder");
        let archive = root.path().join("pacote.zip");
        {
            let file = std::fs::File::create(&archive).expect("create");
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.add_directory("子目录/", options).expect("folder");
            zip.start_file("子目录/中文名.txt", options).expect("entry");
            zip.write_all("中文内容".as_bytes()).expect("write");
            zip.finish().expect("finish");
        }
        let before = std::fs::read(&archive).expect("read archive");

        let target = root.path().join("saida");
        std::fs::create_dir_all(&target).expect("target");
        super::extract_with_hatchzip(&program, &archive, &target, &no_progress()).expect("extracted");
        assert_eq!(
            std::fs::read_to_string(target.join("子目录").join("中文名.txt")).expect("read"),
            "中文内容"
        );
        assert_eq!(std::fs::read(&archive).expect("archive kept"), before);
        // Only the archive and the output are there: no progress file.
        let leftovers: Vec<_> = std::fs::read_dir(root.path())
            .expect("list")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "pacote.zip" && name != "saida")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The 7-Zip fallback builds the command HatchZip's is: `x`, `-y`, `-o`
    /// and the archive after `--`. Skipped where 7-Zip is not installed.
    #[test]
    fn installed_seven_zip_unpacks_as_the_fallback() {
        let Some(program) = crate::explorer::preview_listing::seven_zip() else {
            return;
        };
        let root = tempfile::tempdir().expect("temp folder");
        let archive = root.path().join("pacote.7z");
        // 7-Zip itself creates the archive, so the test does not depend on
        // another writer for the format.
        let source = root.path().join("origem");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::write(source.join("nota.txt"), "oi").expect("write");
        let status = std::process::Command::new(&program)
            .arg("a")
            .arg(&archive)
            .arg(source.join("nota.txt"))
            .output()
            .expect("run 7z");
        assert!(status.status.success(), "{:?}", status);

        let target = root.path().join("saida");
        std::fs::create_dir_all(&target).expect("target");
        super::extract_with_7z(&program, &archive, &target, &no_progress()).expect("extracted");
        assert_eq!(
            std::fs::read_to_string(target.join("nota.txt")).expect("read"),
            "oi"
        );
    }
}
