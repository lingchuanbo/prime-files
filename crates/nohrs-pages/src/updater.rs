//! Automatic updates from this project's GitHub releases.
//!
//! Only an installed copy updates itself: one with the Inno Setup uninstaller
//! (`unins000.exe`) beside it. A build run from `target\` never does, or it
//! would install a release over a developer's own work.
//!
//! The round: ask GitHub for the latest release, and when its version is
//! newer than this one, download its installer into the temporary folder
//! and check it against the release's `SHA256SUMS.txt`; a file that does not
//! match is thrown away. The installer then runs silently either when the app
//! closes (`install_pending_on_exit`, from `main`, once the window is gone)
//! or at once from Settings, with `/RELAUNCH=1` so it opens the app again.
//!
//! The network goes through the `curl.exe` that ships with Windows (10 1803
//! and later; the installer asks for 1903), which handles TLS, proxies and
//! redirects the way the system does, instead of adding an HTTP stack.

use nohrs_core::{t, tf};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use gpui::{App, AppContext as _, Context, Entity, Global};
use semver::Version;
use sha2::{Digest, Sha256};

/// Where releases are published.
pub const REPOSITORY: &str = "CakeDev4k/prime-files";
const INSTALLER_PREFIX: &str = "PrimeFiles-Setup-";
const SUMS_NAME: &str = "SHA256SUMS.txt";

/// What the updater is doing, for the settings page.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Not looked yet.
    Idle,
    /// Running from a build folder: updates are off.
    NotInstalled,
    /// Asking GitHub for the latest release.
    Checking,
    /// This is the newest version.
    UpToDate,
    /// Downloading this version's installer.
    Downloading(String),
    /// Downloaded and checked; installs on close, or now from Settings.
    Ready {
        /// The version downloaded.
        version: String,
        /// Its installer, in the temporary folder.
        installer: PathBuf,
    },
    /// The last look failed, and why.
    Failed(String),
}

/// The installer to run when the app closes. A plain static rather than gpui
/// state: `main` reads it after gpui has shut down.
static PENDING: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Whether this copy was installed (so it may update itself).
pub fn is_installed() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("unins000.exe").is_file()))
        .unwrap_or(false)
}

/// This build's version. `PRIME_FILES_PRETEND_VERSION` overrides it, to try
/// an update against a release that is not really newer.
pub fn current_version() -> Version {
    std::env::var("PRIME_FILES_PRETEND_VERSION")
        .ok()
        .and_then(|value| Version::parse(value.trim()).ok())
        .unwrap_or_else(|| {
            Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version::new(0, 0, 0))
        })
}

fn curl() -> Command {
    let system = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let mut command = Command::new(system.join("System32").join("curl.exe"));
    command.args([
        "--silent",
        "--show-error",
        "--location",
        "--fail",
        "--max-time",
        "600",
    ]);
    command.args(["--user-agent", "PrimeFiles-Updater"]);
    hide_window(&mut command);
    command
}

#[cfg(windows)]
fn hide_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_window(_command: &mut Command) {}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let output = curl()
        .args(["--header", "Accept: application/vnd.github+json", url])
        .output()
        .map_err(|error| format!("curl: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    let output = curl()
        .arg("--output")
        .arg(to)
        .arg(url)
        .output()
        .map_err(|error| format!("curl: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(())
}

/// A newer release, as GitHub describes it.
#[derive(Debug, PartialEq)]
struct Release {
    version: Version,
    installer_name: String,
    installer_url: String,
    sums_url: String,
}

/// Reads the `releases/latest` answer. `None` when it lacks what an update
/// needs (a `v<semver>` tag, the installer and the checksums).
fn parse_release(json: &[u8]) -> Option<Release> {
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    let version = Version::parse(tag.trim_start_matches('v')).ok()?;
    let assets = value.get("assets")?.as_array()?;
    let url_of = |wanted: &dyn Fn(&str) -> bool| {
        assets.iter().find_map(|asset| {
            let name = asset.get("name")?.as_str()?;
            wanted(name).then(|| {
                Some((
                    name.to_string(),
                    asset.get("browser_download_url")?.as_str()?.to_string(),
                ))
            })?
        })
    };
    let expected = format!("{INSTALLER_PREFIX}{version}.exe");
    let (installer_name, installer_url) = url_of(&|name| name == expected)?;
    let (_, sums_url) = url_of(&|name| name == SUMS_NAME)?;
    Some(Release {
        version,
        installer_name,
        installer_url,
        sums_url,
    })
}

/// The hash `SHA256SUMS.txt` lists for `name` (lines of `<hex>  <name>`).
fn listed_hash(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == name).then(|| hash.to_ascii_lowercase())
    })
}

fn file_hash(path: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path)?;
    io::copy(&mut file, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Looks for a newer release and downloads it. `Ok(None)` when this is the
/// newest. Blocking: run it off the UI thread.
fn check_and_download(on_progress: &dyn Fn(Status)) -> Result<Option<(Version, PathBuf)>, String> {
    let latest = fetch(&format!(
        "https://api.github.com/repos/{REPOSITORY}/releases/latest"
    ))?;
    let Some(release) = parse_release(&latest) else {
        return Err(t!("a última release não tem o instalador ou o SHA256SUMS.txt").into());
    };
    if release.version <= current_version() {
        return Ok(None);
    }
    on_progress(Status::Downloading(release.version.to_string()));

    let sums = String::from_utf8_lossy(&fetch(&release.sums_url)?).to_string();
    let Some(expected) = listed_hash(&sums, &release.installer_name) else {
        return Err(tf!(
            "{installer} não está no {sums}",
            installer = release.installer_name,
            sums = SUMS_NAME
        ));
    };
    let folder = std::env::temp_dir().join("PrimeFiles-Update");
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let installer = folder.join(&release.installer_name);
    // A download from an earlier run is reused when it is whole.
    if file_hash(&installer).ok().as_deref() != Some(expected.as_str()) {
        let partial = folder.join(format!("{}.part", release.installer_name));
        download(&release.installer_url, &partial)?;
        let got = file_hash(&partial).map_err(|error| error.to_string())?;
        if got != expected {
            if let Err(error) = std::fs::remove_file(&partial) {
                tracing::warn!(
                    "could not delete the bad download {}: {error}",
                    partial.display()
                );
            }
            return Err(tf!(
                "o instalador baixado não confere com o {sums}",
                sums = SUMS_NAME
            ));
        }
        std::fs::rename(&partial, &installer).map_err(|error| error.to_string())?;
    }
    Ok(Some((release.version, installer)))
}

/// Starts the installer silently, detached from this process. With
/// `relaunch`, it opens the app again when done.
pub fn run_installer(installer: &Path, relaunch: bool) -> io::Result<()> {
    let mut command = Command::new(installer);
    command.args([
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/CURRENTUSER",
        "/CLOSEAPPLICATIONS",
    ]);
    if relaunch {
        command.arg("/RELAUNCH=1");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map(drop)
}

/// Runs the update downloaded during this session, if automatic updates
/// left one waiting. Called by `main` after the window has closed.
pub fn install_pending_on_exit() {
    let pending = PENDING
        .lock()
        .map(|mut pending| pending.take())
        .unwrap_or(None);
    if let Some(installer) = pending {
        tracing::info!("installing the update {} on exit", installer.display());
        if let Err(error) = run_installer(&installer, false) {
            tracing::error!(
                "could not start the update {}: {error}",
                installer.display()
            );
        }
    }
}

fn set_pending(installer: Option<PathBuf>) {
    match PENDING.lock() {
        Ok(mut pending) => *pending = installer,
        Err(error) => tracing::error!("update state poisoned: {error}"),
    }
}

/// The updater's state, shared by the root view (which starts it and tells
/// the person when an update is ready) and the settings page.
pub struct Updater {
    /// What it is doing.
    pub status: Status,
    /// Install on close, from `[updates] automatic`.
    pub automatic: bool,
}

struct GlobalUpdater(Entity<Updater>);
impl Global for GlobalUpdater {}

/// The updater, made on first use.
pub fn updater(cx: &mut App) -> Entity<Updater> {
    if let Some(global) = cx.try_global::<GlobalUpdater>() {
        return global.0.clone();
    }
    let status = if is_installed() {
        Status::Idle
    } else {
        Status::NotInstalled
    };
    let entity = cx.new(|_| Updater {
        status,
        automatic: true,
    });
    cx.set_global(GlobalUpdater(entity.clone()));
    entity
}

impl Updater {
    /// Follows `[updates] automatic`: an update already downloaded installs
    /// on close only while it is on.
    pub fn set_automatic(&mut self, automatic: bool, cx: &mut Context<Self>) {
        self.automatic = automatic;
        match &self.status {
            Status::Ready { installer, .. } if automatic => set_pending(Some(installer.clone())),
            _ => set_pending(None),
        }
        cx.notify();
    }

    /// Looks for an update in the background. Does nothing on a copy that is
    /// not installed, or while a look is already under way.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.status,
            Status::NotInstalled | Status::Checking | Status::Downloading(_)
        ) {
            return;
        }
        self.status = Status::Checking;
        cx.notify();
        let (progress_tx, progress_rx) = async_channel::unbounded::<Status>();
        let work = cx.background_spawn(async move {
            check_and_download(&|status| {
                if let Err(error) = progress_tx.try_send(status) {
                    tracing::debug!("update progress not delivered: {error}");
                }
            })
        });
        cx.spawn(async move |this, cx| {
            // The sender goes with the background work, so this ends with it.
            while let Ok(status) = progress_rx.recv().await {
                let delivered = this.update(cx, |updater, cx| {
                    updater.status = status;
                    cx.notify();
                });
                if delivered.is_err() {
                    break;
                }
            }
            let result = work.await;
            let outcome = this.update(cx, |updater, cx| {
                updater.status = match result {
                    Ok(None) => Status::UpToDate,
                    Ok(Some((version, installer))) => {
                        if updater.automatic {
                            set_pending(Some(installer.clone()));
                        }
                        Status::Ready {
                            version: version.to_string(),
                            installer,
                        }
                    }
                    Err(error) => {
                        tracing::warn!("update check failed: {error}");
                        Status::Failed(error)
                    }
                };
                cx.notify();
            });
            if let Err(error) = outcome {
                tracing::debug!("updater gone before the check finished: {error}");
            }
        })
        .detach();
    }

    /// Installs the downloaded update now and closes the app; the installer
    /// opens it again.
    pub fn install_now(&mut self, cx: &mut Context<Self>) {
        let Status::Ready { installer, .. } = &self.status else {
            return;
        };
        set_pending(None);
        match run_installer(installer, true) {
            Ok(()) => cx.quit(),
            Err(error) => {
                self.status = Status::Failed(tf!(
                    "não foi possível abrir o instalador: {error}",
                    error = error
                ));
                cx.notify();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{listed_hash, parse_release};

    const LATEST: &str = r#"{
        "tag_name": "v0.2.0",
        "assets": [
            {"name": "PrimeFiles-Setup-0.2.0.exe", "browser_download_url": "https://example.test/setup.exe"},
            {"name": "SHA256SUMS.txt", "browser_download_url": "https://example.test/sums.txt"}
        ]
    }"#;

    #[test]
    fn reads_version_installer_and_checksums_from_the_latest_release() {
        let release = parse_release(LATEST.as_bytes()).expect("a complete release");
        assert_eq!(release.version.to_string(), "0.2.0");
        assert_eq!(release.installer_name, "PrimeFiles-Setup-0.2.0.exe");
        assert_eq!(release.installer_url, "https://example.test/setup.exe");
        assert_eq!(release.sums_url, "https://example.test/sums.txt");
    }

    #[test]
    fn a_release_without_its_installer_is_not_an_update() {
        let json = LATEST.replace("PrimeFiles-Setup-0.2.0.exe", "PrimeFiles-Setup-0.1.9.exe");
        assert_eq!(parse_release(json.as_bytes()), None);
        assert_eq!(
            parse_release(b"{\"tag_name\": \"nightly\", \"assets\": []}"),
            None
        );
    }

    #[test]
    fn finds_the_hash_on_windows_and_unix_lines() {
        let sums = "AbC123  PrimeFiles-Setup-0.2.0.exe\r\nfff *other.exe\r\n";
        assert_eq!(
            listed_hash(sums, "PrimeFiles-Setup-0.2.0.exe").as_deref(),
            Some("abc123")
        );
        assert_eq!(listed_hash(sums, "other.exe").as_deref(), Some("fff"));
        assert_eq!(listed_hash(sums, "missing.exe"), None);
    }
}
