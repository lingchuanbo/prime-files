//! The `noh` binary these tests run, copied out of `target/` (spec §4.4).
//!
//! A `userdata` folder beside the real build turns on portable mode, and portable
//! mode ignores every XDG variable: without the copy the tests read and write the
//! developer's own configuration.

#![allow(clippy::expect_used, clippy::disallowed_methods)]

use std::path::{Path, PathBuf};

/// Copies the binary into `directory` for one test, under a name of its own.
///
/// The log tests need a directory whose only other file is the binary, so they
/// cannot share the copy `binary_under_test` hands out. The name carries the process
/// and a timestamp, so nothing is ever renamed over: an antivirus holding the file it
/// just scanned refuses the rename, and that is the failure mode this avoids.
// Used by the log tests; the quick_launch target compiles this module and only needs
// binary_under_test, so it sees this one as unused.
#[allow(dead_code)]
pub fn copy_for_test(directory: &Path) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_BIN_EXE_noh"));
    let extension = if cfg!(windows) { ".exe" } else { "" };
    let mut last = String::new();
    for attempt in 0..20 {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let target = directory.join(format!(
            "noh-under-test-{}-{stamp}-{attempt}{extension}",
            std::process::id()
        ));
        match std::fs::copy(&source, &target) {
            Ok(_) => return target,
            Err(error) => {
                last = error.to_string();
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    panic!("copying the binary for a test: {last}");
}

/// Copies the binary into `directory`, under a stable name it can be reused from.
///
/// A plain `std::fs::copy` onto a fixed name is flaky on Windows: an antivirus can
/// hold the freshly written file, and the answer is "access denied". The copy is
/// written under a unique name and renamed into place, with a few attempts, and a
/// complete copy that only resisted the rename is used where it is.
fn copy_into(directory: &Path) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_BIN_EXE_noh"));
    let name = if cfg!(windows) {
        "noh-under-test.exe"
    } else {
        "noh-under-test"
    };
    let binary = directory.join(name);
    if binary.exists() {
        return binary;
    }
    std::fs::create_dir_all(directory).expect("a directory for the copied binary");
    let mut placed = None;
    for attempt in 0..10 {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let partial = directory.join(format!(
            "{name}.{}.{stamp}.{attempt}.partial",
            std::process::id()
        ));
        if let Err(error) = std::fs::copy(&source, &partial) {
            eprintln!("copying the binary (attempt {attempt}): {error}");
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        match std::fs::rename(&partial, &binary) {
            Ok(()) => return binary,
            Err(error) => {
                if binary.exists() {
                    return binary;
                }
                eprintln!("placing the copied binary (attempt {attempt}): {error}");
                placed = Some(partial);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    // A complete copy that only resisted the rename can still be run.
    if let Some(partial) = placed
        && partial.exists()
    {
        return partial;
    }
    panic!("copying the binary under test");
}

/// Copies the binary somewhere it can run without the portable profile.
///
/// The directory is named after the source's size and modification time, so it is
/// reused for as long as the build is the same: a per-process name made every run a
/// brand-new executable for antivirus to scan, which took minutes.
// Used by the quick_launch test target; this module is compiled into each integration
// test, and the log tests only need copy_into, so this target sees it as unused.
#[allow(dead_code)]
pub fn binary_under_test() -> PathBuf {
    let source = PathBuf::from(env!("CARGO_BIN_EXE_noh"));
    let metadata = std::fs::metadata(&source).expect("the binary cargo just built");
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    let directory = std::env::temp_dir().join(format!(
        "noh-quick-launch-test-{:016x}-{modified:016x}",
        metadata.len()
    ));
    copy_into(&directory)
}
