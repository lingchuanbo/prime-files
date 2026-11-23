//! `noh quick-launch` against a real `quicklaunch.toml` in a temporary
//! configuration directory (docs/QuickDock-技术实现方案与边界约定.md §17.1 C).
//!
//! Every test runs the binary instead of calling the library: the configuration
//! path comes from `XDG_CONFIG_HOME`, which a library-level test could only
//! control by mutating the environment of the whole test process. A child
//! process resolves everything from its own environment, exactly as a user's
//! shell would.

// A test binary has no GPUI foreground thread, and staging real files on disk
// is the point of these tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run `noh` with every XDG directory pointed into `root`, so nothing is read
/// from or written to the real user configuration.
/// The binary under test, copied out of `target/` once per test process.
///
/// A `userdata` folder beside the real build turns on portable mode, and portable
/// mode ignores every XDG variable: without the copy these tests read and write the
/// developer's own quicklaunch.toml.
fn binary_under_test() -> PathBuf {
    static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BINARY.get_or_init(copy_binary_once).clone()
}

/// Copies the binary somewhere it can run without the portable profile.
///
/// The directory is named after the source's size and modification time, so it is
/// reused for as long as the build is the same: a per-process name made every run a
/// brand-new executable for antivirus to scan, which took minutes. An existing copy
/// is used as it is — it may even be the image of a child that has not exited, and
/// overwriting that would fail with "access denied" — and a fresh one is written
/// under a temporary name and renamed, so a half-written file is never run.
fn copy_binary_once() -> PathBuf {
    let source = PathBuf::from(env!("CARGO_BIN_EXE_noh"));
    let name = if cfg!(windows) {
        "noh-under-test.exe"
    } else {
        "noh-under-test"
    };
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
    let binary = directory.join(name);
    if binary.exists() {
        return binary;
    }
    std::fs::create_dir_all(&directory).expect("a directory for the copied binary");
    let partial = directory.join(format!("{name}.partial"));
    std::fs::copy(&source, &partial).expect("copying the binary under test");
    match std::fs::rename(&partial, &binary) {
        Ok(()) => binary,
        Err(error) => {
            // Another test process may have placed the same build first.
            if binary.exists() {
                binary
            } else {
                panic!("placing the copied binary: {error}");
            }
        }
    }
}

fn noh(root: &Path, args: &[&str]) -> Output {
    Command::new(binary_under_test())
        .args(args)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        // An inherited one would change what the child logs, which no assertion
        // here is about.
        .env_remove("RUST_LOG")
        .output()
        .unwrap()
}

/// Where `noh` looks for the Quick Launch file under the redirected
/// `XDG_CONFIG_HOME`.
fn config_file(root: &Path) -> PathBuf {
    root.join("config").join("nohrs").join("quicklaunch.toml")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn exit_code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the child should exit normally")
}

/// A real file to add, so `item_for_path` sees the metadata it expects.
fn fixture_file(root: &Path, name: &str) -> PathBuf {
    let file = root.join(name);
    std::fs::write(&file, b"payload").unwrap();
    file
}

/// A real folder, so the type `item_for_path` derives is `folder`.
fn fixture_dir(root: &Path, name: &str) -> PathBuf {
    let directory = root.join(name);
    std::fs::create_dir(&directory).unwrap();
    directory
}

#[test]
fn an_item_added_by_name_is_listed_with_its_kind_and_path() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let folder = fixture_dir(root, "Editor");

    let added = noh(
        root,
        &[
            "quick-launch",
            "add",
            folder.to_str().unwrap(),
            "--slot",
            "3",
            "--name",
            "Text Editor",
        ],
    );
    assert_eq!(exit_code(&added), 0, "add failed: {}", stderr(&added));

    // The alias has to reach the same command as the long name.
    let listed = noh(root, &["ql", "list"]);
    assert_eq!(exit_code(&listed), 0, "list failed: {}", stderr(&listed));
    let listing = stdout(&listed);
    let path = config_file(root);
    assert_eq!(
        listing.lines().next(),
        Some(path.display().to_string().as_str()),
        "the first line names the file in use: {listing}"
    );
    assert!(listing.contains("general"), "{listing}");
    assert!(
        listing.contains("3\tfolder\tText Editor"),
        "the slot, type and name should be listed: {listing}"
    );
    assert!(listing.contains(folder.to_str().unwrap()), "{listing}");

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("slot = 3"), "{written}");
    assert!(written.contains("type = \"folder\""), "{written}");
    assert!(written.contains("name = \"Text Editor\""), "{written}");
}

#[test]
fn an_occupied_slot_is_refused_and_leaves_the_file_alone() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let first = fixture_file(root, "First.txt");
    let second = fixture_file(root, "Second.txt");

    let added = noh(root, &["ql", "add", first.to_str().unwrap(), "--slot", "1"]);
    assert_eq!(exit_code(&added), 0, "add failed: {}", stderr(&added));
    let before = std::fs::read(config_file(root)).unwrap();

    let refused = noh(
        root,
        &["ql", "add", second.to_str().unwrap(), "--slot", "1"],
    );
    assert_eq!(exit_code(&refused), 1, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("slot 1"),
        "the refusal should name the slot: {}",
        stderr(&refused)
    );
    assert_eq!(
        std::fs::read(config_file(root)).unwrap(),
        before,
        "a refused add must not touch the file"
    );
}

#[test]
fn remove_deletes_the_item_in_the_slot() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let first = fixture_file(root, "First.txt");
    let second = fixture_file(root, "Second.txt");
    for (file, slot) in [(&first, "0"), (&second, "1")] {
        let added = noh(root, &["ql", "add", file.to_str().unwrap(), "--slot", slot]);
        assert_eq!(exit_code(&added), 0, "add failed: {}", stderr(&added));
    }

    let removed = noh(root, &["ql", "remove", "general", "0"]);
    assert_eq!(
        exit_code(&removed),
        0,
        "remove failed: {}",
        stderr(&removed)
    );

    let listing = stdout(&noh(root, &["ql", "list"]));
    assert!(!listing.contains("First"), "{listing}");
    assert!(listing.contains("Second"), "{listing}");

    // Removing the same slot twice is a command failure, not a silent success.
    let again = noh(root, &["ql", "remove", "general", "0"]);
    assert_eq!(exit_code(&again), 1, "{}", stderr(&again));
}

#[test]
fn a_file_that_does_not_parse_is_never_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = config_file(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let malformed = "schema_version = = 1\n";
    std::fs::write(&path, malformed).unwrap();
    let program = fixture_file(root, "Editor.txt");

    let refused = noh(root, &["ql", "add", program.to_str().unwrap()]);
    assert_eq!(exit_code(&refused), 2, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("line 1"),
        "the reason should point at the parse error: {}",
        stderr(&refused)
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        malformed,
        "a file this build cannot parse must be left exactly as it was"
    );
}

#[test]
fn a_file_from_a_newer_build_is_not_rewritten() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = config_file(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let newer = "schema_version = 99\n";
    std::fs::write(&path, newer).unwrap();

    let refused = noh(root, &["ql", "remove", "general", "0"]);
    assert_eq!(exit_code(&refused), 2, "{}", stderr(&refused));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        newer,
        "a newer file must not be rewritten by this build"
    );
}

#[test]
fn list_json_describes_the_file_in_use_and_its_items() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let first = fixture_file(root, "First.txt");
    let second = fixture_file(root, "Second.txt");
    let added = noh(
        root,
        &["ql", "add", first.to_str().unwrap(), "--name", "First App"],
    );
    assert_eq!(exit_code(&added), 0, "add failed: {}", stderr(&added));
    let added = noh(root, &["ql", "add", second.to_str().unwrap()]);
    assert_eq!(exit_code(&added), 0, "add failed: {}", stderr(&added));

    let listed = noh(root, &["ql", "list", "--json"]);
    assert_eq!(exit_code(&listed), 0, "list failed: {}", stderr(&listed));
    let document: serde_json::Value = serde_json::from_str(&stdout(&listed)).unwrap();

    assert_eq!(
        document["path"].as_str().unwrap(),
        config_file(root).to_str().unwrap(),
        "the document names the file in use"
    );
    let groups = document["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["id"], "general");
    let items = groups[0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["slot"].as_u64(), Some(0));
    assert_eq!(items[0]["type"], "file");
    assert_eq!(items[0]["name"], "First App");
    assert_eq!(
        items[0]["path"].as_str().unwrap(),
        first.to_str().unwrap(),
        "the item keeps the path it was added with"
    );
    assert_eq!(items[1]["slot"].as_u64(), Some(1));
}

#[test]
fn check_passes_while_the_target_exists_and_fails_once_it_is_gone() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let program = fixture_file(root, "Editor.txt");
    let added = noh(root, &["ql", "add", program.to_str().unwrap()]);
    assert_eq!(exit_code(&added), 0, "add failed: {}", stderr(&added));

    let healthy = noh(root, &["ql", "check"]);
    assert_eq!(
        exit_code(&healthy),
        0,
        "check failed: {}{}",
        stdout(&healthy),
        stderr(&healthy)
    );

    std::fs::remove_file(&program).unwrap();
    let unhealthy = noh(root, &["ql", "check"]);
    assert_eq!(
        exit_code(&unhealthy),
        1,
        "a missing target must fail the check: {}",
        stdout(&unhealthy)
    );
    assert!(
        stdout(&unhealthy).contains("missing"),
        "{}",
        stdout(&unhealthy)
    );
}

#[test]
#[cfg(windows)]
fn run_names_a_group_that_does_not_exist() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let program = fixture_file(root, "Editor.txt");
    let added = noh(root, &["ql", "add", program.to_str().unwrap()]);
    assert_eq!(exit_code(&added), 0, "add failed: {}", stderr(&added));

    // The group is resolved before anything is started, so this checks the
    // command's own lookup without launching a program.
    let missing = noh(root, &["ql", "run", "dev", "0"]);
    assert_eq!(exit_code(&missing), 1, "{}", stderr(&missing));
    assert!(
        stderr(&missing).contains("there is no group"),
        "{}",
        stderr(&missing)
    );
}
