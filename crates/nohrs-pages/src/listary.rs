//! Making Listary open folders in Prime Files, from the settings page.
//!
//! Listary 6 has no command line or API for this: its "open folders with"
//! choice is `Settings."OpenFolder.Path"` / `"OpenFolder.Param"` in its
//! `Preferences.json`, and leaving them out means Explorer. Listary keeps the
//! file in memory and writes it back when it exits, so an edit made while it
//! runs is lost: it is closed first and started again afterwards.

use nohrs_core::t;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

const PATH_KEY: &str = "OpenFolder.Path";
const PARAM_KEY: &str = "OpenFolder.Param";
// `%1` is the folder; quoted because folder names have spaces.
const PARAM: &str = "\"%1\"";

/// Where Listary keeps its choices for this user, when it is installed.
pub fn preferences_file() -> Option<PathBuf> {
    let file = PathBuf::from(std::env::var_os("APPDATA")?)
        .join("Listary")
        .join("UserProfile")
        .join("Settings")
        .join("Preferences.json");
    file.is_file().then_some(file)
}

/// What Listary does with folders right now, as the settings page shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// No Listary preferences for this user.
    NotInstalled,
    /// Listary opens folders in Prime Files.
    Ours,
    /// Listary opens folders in Explorer or another program.
    NotOurs,
}

/// Reads what Listary opens folders with.
pub fn state() -> State {
    let Some(file) = preferences_file() else {
        return State::NotInstalled;
    };
    match read(&file) {
        Ok(preferences) if opens_with_us(&preferences) => State::Ours,
        Ok(_) => State::NotOurs,
        Err(error) => {
            tracing::warn!("could not read {}: {error}", file.display());
            State::NotOurs
        }
    }
}

/// Makes Listary open folders with `exe`.
pub fn enable(exe: &Path) -> io::Result<()> {
    change(|preferences| set_opener(preferences, exe))
}

/// Gives Listary's folders back to Explorer, unless another program had
/// already taken them over.
pub fn disable() -> io::Result<()> {
    change(|preferences| {
        if opens_with_us(preferences) {
            remove_opener(preferences)
        } else {
            Ok(false)
        }
    })
}

// Applies `edit` to the file with Listary closed, and starts it again if it
// was running. `edit` says whether it changed anything: nothing changed means
// Listary is not bothered at all.
fn change(edit: impl Fn(&mut Value) -> io::Result<bool>) -> io::Result<()> {
    let file = preferences_file().ok_or_else(|| io::Error::other(t!("o Listary não está instalado")))?;
    let mut preferences = read(&file)?;
    if !edit(&mut preferences)? {
        return Ok(());
    }
    let running = platform::running_listary();
    if let Some(exe) = &running {
        close_listary()?;
        tracing::info!("closed Listary ({}) to change its preferences", exe.display());
    }
    // Read again: closing Listary is what writes its latest state to disk.
    let mut preferences = read(&file)?;
    let written = edit(&mut preferences).and_then(|changed| if changed { write(&file, &preferences) } else { Ok(()) });
    if let Some(exe) = running {
        restart_listary(&exe)?;
    }
    written
}

fn read(file: &Path) -> io::Result<Value> {
    let text = std::fs::read_to_string(file)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let preferences: Value = serde_json::from_str(text).map_err(io::Error::other)?;
    if !preferences.is_object() {
        return Err(io::Error::other(t!("Preferences.json do Listary não é um objeto")));
    }
    Ok(preferences)
}

// Through a temporary file and a rename, so a failure halfway never leaves
// Listary with a truncated file (it would start over with defaults).
fn write(file: &Path, preferences: &Value) -> io::Result<()> {
    let text = serde_json::to_string_pretty(preferences).map_err(io::Error::other)?;
    let temporary = file.with_extension("json.prime-files");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, file)
}

fn settings(preferences: &Value) -> Option<&Map<String, Value>> {
    preferences.get("Settings")?.as_object()
}

fn opens_with_us(preferences: &Value) -> bool {
    settings(preferences)
        .and_then(|settings| settings.get(PATH_KEY))
        .and_then(Value::as_str)
        .is_some_and(is_ours)
}

// Keep recognizing older installed names while using LumaFiles.exe going forward.
fn is_ours(opener: &str) -> bool {
    let name = Path::new(opener)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let running = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_name().map(|name| name.to_string_lossy().to_ascii_lowercase()));
    name == "lumafiles.exe"
        || name == "primefiles.exe"
        || name == "nohrs.exe"
        || running.is_some_and(|running| running == name)
}

fn set_opener(preferences: &mut Value, exe: &Path) -> io::Result<bool> {
    let root = preferences
        .as_object_mut()
        .ok_or_else(|| io::Error::other(t!("Preferences.json do Listary não é um objeto")))?;
    let settings = root
        .entry("Settings")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| io::Error::other(t!("\"Settings\" do Listary não é um objeto")))?;
    let path = Value::String(exe.to_string_lossy().into_owned());
    let param = Value::String(PARAM.to_string());
    if settings.get(PATH_KEY) == Some(&path) && settings.get(PARAM_KEY) == Some(&param) {
        return Ok(false);
    }
    settings.insert(PATH_KEY.to_string(), path);
    settings.insert(PARAM_KEY.to_string(), param);
    Ok(true)
}

fn remove_opener(preferences: &mut Value) -> io::Result<bool> {
    let Some(settings) = preferences.get_mut("Settings").and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    // `shift_remove`: the rest of the file keeps Listary's own order.
    let path = settings.shift_remove(PATH_KEY);
    let param = settings.shift_remove(PARAM_KEY);
    Ok(path.is_some() || param.is_some())
}

fn taskkill(args: &[&str]) -> io::Result<()> {
    let mut command = Command::new("taskkill.exe");
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.output().map(|_| ())
}

// Asked politely first, so Listary saves what it holds; forced only when it
// ignores that (a tray program may have no window to receive WM_CLOSE).
fn close_listary() -> io::Result<()> {
    taskkill(&["/im", "Listary.exe"])?;
    if wait_until_gone(Duration::from_secs(3)) {
        return Ok(());
    }
    taskkill(&["/f", "/im", "Listary.exe"])?;
    if wait_until_gone(Duration::from_secs(3)) {
        Ok(())
    } else {
        Err(io::Error::other(t!("o Listary não fechou")))
    }
}

fn wait_until_gone(limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if platform::running_listary().is_none() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    platform::running_listary().is_none()
}

// Without its own standard handles: Listary outlives this process, and an
// inherited pipe kept open by it left whoever ran `forget-listary` waiting
// for an end of output that never came.
fn restart_listary(exe: &Path) -> io::Result<()> {
    use std::process::Stdio;
    Command::new(exe).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use std::path::PathBuf;

    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    /// The executable of the running Listary, to start the same one again.
    pub fn running_listary() -> Option<PathBuf> {
        // SAFETY: the snapshot handle is closed below on every path, and the
        // entry is a plain struct sized as the API requires.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut found = None;
            let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
            while more {
                let length = entry.szExeFile.iter().position(|&unit| unit == 0).unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(entry.szExeFile.get(..length).unwrap_or_default());
                if name.eq_ignore_ascii_case("Listary.exe") {
                    found = Some(image_path(entry.th32ProcessID).unwrap_or_else(default_path));
                    break;
                }
                more = Process32NextW(snapshot, &mut entry).is_ok();
            }
            if let Err(error) = CloseHandle(snapshot) {
                tracing::debug!("closing the process snapshot: {error}");
            }
            found
        }
    }

    fn image_path(process_id: u32) -> Option<PathBuf> {
        // SAFETY: the process handle is closed before returning, and the
        // buffer length passed is the buffer's own.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
            let mut buffer = [0u16; 1024];
            let mut length = buffer.len() as u32;
            let queried =
                QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut length);
            if let Err(error) = CloseHandle(process) {
                tracing::debug!("closing the Listary process handle: {error}");
            }
            queried.ok()?;
            let path = String::from_utf16_lossy(buffer.get(..length as usize)?);
            Some(PathBuf::from(path))
        }
    }

    // Where its installer puts it, for when the running copy cannot be asked.
    fn default_path() -> PathBuf {
        PathBuf::from(r"C:\Program Files\Listary\Listary.exe")
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn running_listary() -> Option<std::path::PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        serde_json::from_str(
            r#"{"AppConfigs":{},"Settings":{"Hotkey.QuickSwitch":0,"General.X":1},"Extensions":{}}"#,
        )
        .unwrap()
    }

    #[test]
    fn enabling_adds_both_keys_and_keeps_the_order_of_the_rest() {
        let mut preferences = sample();
        let exe = Path::new(r"C:\Users\someone\AppData\Local\Programs\Prime Files\PrimeFiles.exe");
        assert!(set_opener(&mut preferences, exe).unwrap());
        assert!(opens_with_us(&preferences));
        let keys: Vec<_> = settings(&preferences).unwrap().keys().cloned().collect();
        assert_eq!(keys, ["Hotkey.QuickSwitch", "General.X", PATH_KEY, PARAM_KEY]);
        let top: Vec<_> = preferences.as_object().unwrap().keys().cloned().collect();
        assert_eq!(top, ["AppConfigs", "Settings", "Extensions"]);
        assert!(!set_opener(&mut preferences, exe).unwrap(), "already set: nothing to write");
    }

    #[test]
    fn disabling_removes_only_our_opener() {
        let mut preferences = sample();
        set_opener(&mut preferences, Path::new(r"C:\x\PrimeFiles.exe")).unwrap();
        assert!(remove_opener(&mut preferences).unwrap());
        assert_eq!(settings(&preferences).unwrap().len(), 2);

        let mut other = sample();
        set_opener(&mut other, Path::new(r"C:\FilePilot\FPilot.exe")).unwrap();
        assert!(!opens_with_us(&other));
    }

    #[test]
    fn a_development_build_counts_as_ours() {
        assert!(is_ours(r"C:\src\nohrs\target\release\nohrs.exe"));
        assert!(is_ours(r"C:\Programs\Prime Files\PrimeFiles.exe"));
        assert!(is_ours(r"C:\Programs\LumaFiles\LumaFiles.exe"));
        assert!(!is_ours(r"C:\Windows\explorer.exe"));
    }
}
