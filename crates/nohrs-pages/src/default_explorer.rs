//! Making Nohrs the folder handler of the current Windows user, from the
//! settings page. The same registry entries `integrations\windows\tornar-padrao.bat`
//! writes, plus Win+E and the taskbar's Explorer button.
//!
//! All of it lives under `HKCU\Software\Classes`, so it needs no administrator
//! and touches no other account; turning it off deletes only what was added
//! here. Written through `reg.exe` rather than the registry API so each entry
//! reads exactly like the line in the `.bat` that a person can check by hand.

use nohrs_core::t;
use std::io;
use std::path::Path;
use std::process::Command;

// The verb added under `Directory\shell` and `Drive\shell`; the key's default
// value names it, which makes it the double-click action.
const VERB: &str = "nohrs";
const FOLDER_CLASSES: [&str; 2] = ["Directory", "Drive"];
// What Win+E and the taskbar's Explorer button launch ("This PC" in a new
// window). A `command` with an empty `DelegateExecute` under it replaces the
// COM handler with a plain program for this user, which is how replacement
// file managers take those two over.
const NEW_WINDOW_KEY: &str =
    r"HKCU\Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow";

fn reg(args: &[&str]) -> io::Result<std::process::Output> {
    let mut command = Command::new("reg.exe");
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console flashing up over the window for each call.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.output()
}

fn reg_ok(args: &[&str]) -> io::Result<()> {
    let output = reg(args)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(String::from_utf8_lossy(&output.stderr).trim().to_string()))
    }
}

/// Whether folders currently open in Nohrs for this user.
pub fn is_default() -> bool {
    reg(&["query", r"HKCU\Software\Classes\Directory\shell", "/ve"])
        .map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.contains("REG_SZ") && line.trim_end().ends_with(VERB))
        })
        .unwrap_or(false)
}

/// Makes `exe` open folders, drives, Win+E and the taskbar's Explorer button.
pub fn enable(exe: &Path) -> io::Result<()> {
    let exe = exe.to_string_lossy();
    let quoted = format!("\"{exe}\"");
    for class in FOLDER_CLASSES {
        let shell = format!(r"HKCU\Software\Classes\{class}\shell");
        let verb = format!(r"{shell}\{VERB}");
        reg_ok(&["add", &verb, "/ve", "/d", t!("Abrir no LumaFiles"), "/f"])?;
        reg_ok(&["add", &verb, "/v", "Icon", "/d", &quoted, "/f"])?;
        reg_ok(&["add", &format!(r"{verb}\command"), "/ve", "/d", &format!("{quoted} \"%1\""), "/f"])?;
        reg_ok(&["add", &shell, "/ve", "/d", VERB, "/f"])?;
    }
    // Without an argument Nohrs brings its open window forward, or opens one.
    let command = format!(r"{NEW_WINDOW_KEY}\command");
    reg_ok(&["add", &command, "/ve", "/d", &quoted, "/f"])?;
    reg_ok(&["add", &command, "/v", "DelegateExecute", "/d", "", "/f"])?;
    Ok(())
}

/// Gives folders back to Explorer, removing only what [`enable`] added.
pub fn disable() -> io::Result<()> {
    for class in FOLDER_CLASSES {
        let shell = format!(r"HKCU\Software\Classes\{class}\shell");
        // Not `reg_ok`: a key that is not there fails the delete, and is
        // already undone.
        reg(&["delete", &format!(r"{shell}\{VERB}"), "/f"])?;
        if is_ours(&shell) {
            reg_ok(&["delete", &shell, "/ve", "/f"])?;
        }
    }
    if is_ours(&format!(r"{NEW_WINDOW_KEY}\command")) {
        reg_ok(&["delete", NEW_WINDOW_KEY, "/f"])?;
    }
    Ok(())
}

// Whether the default value of `key` points at this app, so a value some
// other program set is left alone. The verb's name says "nohrs"; a command
// names the executable, which the installer calls LumaFiles.exe.
fn is_ours(key: &str) -> bool {
    let exe_name = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_name().map(|name| name.to_string_lossy().to_ascii_lowercase()));
    reg(&["query", key, "/ve"])
        .map(|output| {
            String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                let line = line.to_ascii_lowercase();
                line.contains("reg_sz")
                    && (line.contains("nohrs") || exe_name.as_deref().is_some_and(|name| line.contains(name)))
            })
        })
        .unwrap_or(false)
}

// Where Windows starts programs at sign-in for this user, and the value name.
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Prime Files";

/// Whether LumaFiles starts, minimized, when this user signs in.
pub fn starts_with_windows() -> bool {
    reg(&["query", RUN_KEY, "/v", RUN_VALUE]).is_ok_and(|output| output.status.success())
}

/// Makes `exe` start minimized at sign-in, or stops it. Started that way it
/// is already running when Premiere or a browser asks for a folder, so the
/// file opens here at once instead of in Explorer.
pub fn set_starts_with_windows(exe: &Path, enable: bool) -> io::Result<()> {
    if enable {
        let command = format!("\"{}\" --background", exe.display());
        reg_ok(&["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &command, "/f"])
    } else if starts_with_windows() {
        reg_ok(&["delete", RUN_KEY, "/v", RUN_VALUE, "/f"])
    } else {
        Ok(())
    }
}
