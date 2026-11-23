//! `noh log` is the *reader* of the log file, so running it must not write one.
//!
//! These go through the real binary rather than `log::Session` because the bug
//! they pin lives in `main`'s ordering: the subscriber is installed before the
//! command runs, so a unit test on `Session` cannot see it. Cargo builds the
//! binary for integration tests and hands us its path in `CARGO_BIN_EXE_noh`.
//!
//! Where the log lands depends on the platform (docs/logging.md): on Windows it is
//! the executable's own directory, which is why these tests run the copy
//! `binary_under_test` makes; elsewhere it is the state directory the child is
//! pointed at with `XDG_STATE_HOME`. Every test gets a state directory of its own,
//! so the developer's real log directory is never read or written.

// `clippy.toml` bans the synchronous `std::fs` helpers so that blocking IO does
// not reach the GPUI foreground thread. These tests must stage and inspect real
// log files on disk — that is what they are checking — and there is no UI
// thread in a test binary to keep responsive.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use nohrs_core::telemetry::logging::current_log_file_name;

#[path = "support/binary_under_test.rs"]
mod binary_under_test;

/// Takes the turn of this test on Windows, where the tests share one log directory.
///
/// The log directory there is the directory of the copy, which both this file and
/// the quick-launch tests run from, so the four tests take turns and each starts by
/// clearing the logs an earlier run left. On the other platforms every test has its
/// own state directory and no turn is needed.
#[cfg(windows)]
fn turn(state: &Path) -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let guard = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap();
    std::fs::create_dir_all(log_dir(state)).unwrap();
    clear_logs(state);
    guard
}

/// Nothing to take elsewhere: each test has a log directory of its own.
#[cfg(not(windows))]
fn turn(_state: &Path) {}

/// The directory `noh` logs into when it is pointed at `state`.
fn log_dir(_state: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        // The executable's own directory (docs/logging.md): the copy's.
        binary_under_test::binary_under_test()
            .parent()
            .expect("the copy has a directory")
            .to_path_buf()
    }
    #[cfg(not(windows))]
    {
        _state.join("nohrs").join("logs")
    }
}

/// Whether a directory entry is the copy of `noh` rather than a log file.
fn is_copy(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with("noh-under-test")
}

/// The log files left in `state`'s log directory, without the copy of `noh` itself.
fn log_files(state: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(log_dir(state))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| !is_copy(&entry.file_name()))
                .collect()
        })
        .unwrap_or_default()
}

/// Deletes every log file an earlier run left behind.
fn clear_logs(state: &Path) {
    for entry in log_files(state) {
        if let Err(error) = std::fs::remove_file(entry.path()) {
            eprintln!("removing a stale log file: {error}");
        }
    }
}

/// Run `noh` with `args`, pointed at `state` for everything it would write.
fn noh(state: &Path, args: &[&str]) -> std::process::Output {
    Command::new(binary_under_test::binary_under_test())
        .args(args)
        // The log is the thing under test; the state directory is the test's.
        .env("XDG_STATE_HOME", state)
        // The stderr sink honours `RUST_LOG`; an inherited one would change
        // what the child prints and has nothing to do with what is asserted.
        .env_remove("RUST_LOG")
        .output()
        .unwrap()
}

#[test]
fn show_on_a_fresh_install_reports_nothing_and_creates_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path();
    let _turn = turn(state);

    let output = noh(state, &["log", "show"]);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("no log files"), "{stdout}");
    assert!(
        log_files(state).is_empty(),
        "reading the log must not create one: the file sink would make `show` \
         report a file it had just built itself"
    );
}

#[test]
fn clear_removes_the_files_and_leaves_none_behind() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path();
    let _turn = turn(state);
    std::fs::create_dir_all(log_dir(state)).unwrap();
    let rotated = log_dir(state).join("nohrs.log.2026-09-06");
    std::fs::write(&rotated, "{}\n").unwrap();

    let output = noh(state, &["log", "clear", "--force"]);

    assert!(output.status.success(), "{output:?}");
    assert!(
        !rotated.exists(),
        "a rotated file no writer holds must be unlinked"
    );
    // The bug this guards: with the sink open, `clear` deletes the file this
    // very process is appending to and the appender then writes to an unlinked
    // inode, so a file reappears, or the removal is silently undone.
    let remaining: Vec<_> = log_files(state)
        .into_iter()
        .map(|entry| entry.file_name())
        .collect();
    assert!(
        remaining.is_empty(),
        "clear should leave the directory without logs, found {remaining:?}"
    );
}

#[test]
fn clear_empties_the_open_file_without_taking_its_name() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path();
    let _turn = turn(state);
    // The case the whole truncate-instead-of-unlink rule exists for: a GUI is
    // holding today's file open while `noh log clear --force` runs. Unlinking it
    // would leave that writer appending to an inode with no name, and everything
    // it logged until the next rotation would be invisible.
    std::fs::create_dir_all(log_dir(state)).unwrap();
    let today = log_dir(state).join(current_log_file_name().expect("today's file name"));
    std::fs::write(&today, "{\"message\":\"before\"}\n").unwrap();
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&today)
        .unwrap();

    let output = noh(state, &["log", "clear", "--force"]);
    assert!(output.status.success(), "{output:?}");

    assert!(today.exists(), "the open file must keep its name");
    assert_eq!(
        std::fs::metadata(&today).unwrap().len(),
        0,
        "the open file must be emptied"
    );

    // The writer never learned anything happened; its next record has to land in
    // the file `noh log show` will read, not in a lost inode.
    writer.write_all(b"{\"message\":\"after\"}\n").unwrap();
    writer.flush().unwrap();
    let body = std::fs::read_to_string(&today).unwrap();
    assert!(
        body.contains("after"),
        "a later write must be visible: {body:?}"
    );
    assert!(
        !body.contains("before"),
        "the old records must be gone: {body:?}"
    );
}

#[test]
fn a_command_that_is_not_log_does_record_itself() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path();
    let _turn = turn(state);
    let workspace = tempfile::tempdir().unwrap();
    let target = workspace.path().join("notes.txt");
    std::fs::write(&target, "x").unwrap();

    // Permanent so it does not need a desktop trash, which CI has not got.
    let output = noh(state, &["rm", "--permanent", &target.display().to_string()]);

    assert!(output.status.success(), "{output:?}");
    assert!(!target.exists());
    let files = log_files(state);
    assert!(
        !files.is_empty(),
        "a command that does real work must open the log"
    );
    let body = std::fs::read_to_string(files[0].path()).unwrap();
    assert!(body.contains("fs.delete"), "{body}");
}
