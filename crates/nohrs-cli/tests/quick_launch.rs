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

#[path = "support/binary_under_test.rs"]
mod binary_under_test;

/// Run `noh` with every XDG directory pointed into `root`, so nothing is read
/// from or written to the real user configuration.
fn noh(root: &Path, args: &[&str]) -> Output {
    Command::new(binary_under_test::binary_under_test())
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

/// A file with one command item in the default group.
fn command_file(root: &Path) -> PathBuf {
    let file = config_file(root);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(
        &file,
        r#"
schema_version = 1

[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "command"
name = "Echo"

[group.item.extra]
shell = "cmd"
script = "exit 0"
"#,
    )
    .unwrap();
    file
}

/// D6: a single command is refused without `--yes`, before anything is started.
#[test]
fn running_a_single_command_needs_yes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    command_file(root);

    let refused = noh(root, &["ql", "run", "general", "0"]);
    assert_eq!(exit_code(&refused), 1, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("pass --yes to run a command"),
        "the refusal should name the flag: {}",
        stderr(&refused)
    );
}

/// D6: with `--yes` the check passes and the command is started for real.
///
/// Ignored because it starts a process outside the test's control.
#[test]
#[ignore = "starts a real process"]
fn a_single_command_runs_with_yes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    command_file(root);

    let started = noh(root, &["ql", "run", "general", "0", "--yes"]);
    assert_eq!(exit_code(&started), 0, "{}", stderr(&started));
    assert!(
        stdout(&started).contains("started"),
        "the run should report the start: {}",
        stdout(&started)
    );
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

/// §17.2 B.6: `add --uwp` is the alternative to the positional path, and the
/// AppUserModelID is what the item stores.
#[test]
fn an_item_added_by_app_id_stores_the_id_with_the_uwp_type() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let aumid = "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App";

    let added = noh(
        root,
        &[
            "ql",
            "add",
            "--uwp",
            aumid,
            "--name",
            "Calculadora",
            "--args",
            "arg",
        ],
    );
    assert_eq!(exit_code(&added), 0, "add failed: {}", stderr(&added));

    let listed = noh(root, &["ql", "list"]);
    assert_eq!(exit_code(&listed), 0, "list failed: {}", stderr(&listed));
    assert!(
        stdout(&listed).contains(&format!("0\tuwp\tCalculadora\t{aumid}")),
        "{}",
        stdout(&listed)
    );

    let written = std::fs::read_to_string(config_file(root)).unwrap();
    assert!(written.contains("type = \"uwp\""), "{written}");
    assert!(
        written.contains(&format!("path = \"{aumid}\"")),
        "{written}"
    );
    assert!(written.contains("args = \"arg\""), "{written}");
}

/// §17.2 B.6 and B.7: the structure of `apps --json` on a real machine.
#[test]
#[cfg(windows)]
fn apps_json_describes_each_installed_app() {
    let directory = tempfile::tempdir().unwrap();
    let listed = noh(directory.path(), &["ql", "apps", "--json"]);
    assert_eq!(
        exit_code(&listed),
        0,
        "apps failed: {}{}",
        stdout(&listed),
        stderr(&listed)
    );
    let document: serde_json::Value = serde_json::from_str(&stdout(&listed)).unwrap();
    let apps = document["apps"].as_array().expect("an apps array");
    for app in apps {
        assert!(app["name"].is_string(), "{app}");
        let aumid = app["aumid"].as_str().expect("an aumid string");
        assert!(!aumid.is_empty(), "{app}");
    }
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

/// A file with one group whose `bundle` item references `editor` and `script`
/// (§17.2 E.5).
#[cfg(windows)]
fn write_group_file(root: &Path) {
    let path = config_file(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"schema_version = 1

[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "app"
name = "Editor"
id = "editor"
path = "C:\\Windows\\notepad.exe"

[[group.item]]
slot = 1
type = "command"
name = "Script"
id = "script"

[group.item.extra]
shell = "cmd"
script = "echo ok"

[[group.item]]
slot = 2
type = "group"
name = "Pacote"
id = "bundle"
items = ["editor", "script"]
"#,
    )
    .unwrap();
}

/// D6 (§17.2 E.5): a group with a command child is refused as a whole unless
/// `--yes` confirms it, and nothing is started.
#[test]
#[cfg(windows)]
fn run_refuses_a_group_with_a_command_without_yes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_group_file(root);

    let refused = noh(root, &["ql", "run", "general", "2"]);
    assert_eq!(exit_code(&refused), 1, "{}", stderr(&refused));
    let errors = stderr(&refused);
    assert!(errors.contains("contains commands"), "{errors}");
    assert!(errors.contains("--yes"), "{errors}");
    assert!(
        stdout(&refused).is_empty(),
        "the whole group is refused, so nothing is started: {}",
        stdout(&refused)
    );
}

/// §17.2 E.2: a group whose references do not lead anywhere cannot be run.
#[test]
#[cfg(windows)]
fn run_refuses_a_group_with_no_valid_reference() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = config_file(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"schema_version = 1

[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "group"
name = "Vazio"
id = "empty"
items = ["ghost"]
"#,
    )
    .unwrap();

    let refused = noh(root, &["ql", "run", "general", "0"]);
    assert_eq!(exit_code(&refused), 1, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("no item to start"),
        "{}",
        stderr(&refused)
    );
}

/// A file with one ordinary group and the dynamic sources of §17.2 G.
fn write_dynamic_file(root: &Path) {
    let path = config_file(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"schema_version = 1

[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "app"
name = "Editor"
path = "C:\\Windows\\notepad.exe"

[[group]]
id = "recent"
name = "Recentes"
source = "recent"

[[group]]
id = "tools"
name = "Ferramentas"
source = "folder:C:\\Windows"
"#,
    )
    .unwrap();
}

/// §17.2 G.9: the listing marks a dynamic group with its source and shows no
/// items for it, because the file has none.
#[test]
fn list_marks_a_dynamic_group_and_shows_no_items_for_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_dynamic_file(root);

    let listed = noh(root, &["ql", "list"]);
    assert_eq!(exit_code(&listed), 0, "list failed: {}", stderr(&listed));
    let listing = stdout(&listed);
    assert!(listing.contains("recent (dynamic: recent)"), "{listing}");
    assert!(
        listing.contains("tools (dynamic: folder:C:\\Windows)"),
        "{listing}"
    );
    assert!(
        listing.contains("0\tapp\tEditor"),
        "the ordinary group keeps its entry: {listing}"
    );
    let lines: Vec<&str> = listing.lines().collect();
    let dynamic = lines
        .iter()
        .position(|line| line.starts_with("recent (dynamic:"))
        .expect("the recent group is listed");
    assert!(
        !lines
            .get(dynamic + 1)
            .is_some_and(|line| line.starts_with("  ")),
        "a dynamic group lists no entries: {listing}"
    );
}

/// §17.2 G.9: `add` and `remove` refuse a dynamic group with exit code 1 and
/// leave the file alone.
#[test]
fn add_and_remove_refuse_a_dynamic_group() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_dynamic_file(root);
    let before = std::fs::read(config_file(root)).unwrap();
    let program = fixture_file(root, "Editor.txt");

    let added = noh(
        root,
        &["ql", "add", program.to_str().unwrap(), "--group", "recent"],
    );
    assert_eq!(exit_code(&added), 1, "{}", stderr(&added));
    assert!(stderr(&added).contains("dynamic"), "{}", stderr(&added));

    let removed = noh(root, &["ql", "remove", "tools", "0"]);
    assert_eq!(exit_code(&removed), 1, "{}", stderr(&removed));
    assert!(stderr(&removed).contains("dynamic"), "{}", stderr(&removed));

    assert_eq!(
        std::fs::read(config_file(root)).unwrap(),
        before,
        "a refusal must not touch the file"
    );
}

/// §17.2 G.9: `run` refuses a dynamic group and says the window is where it
/// runs.
#[test]
#[cfg(windows)]
fn run_refuses_a_dynamic_group_and_points_at_the_window() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_dynamic_file(root);

    let refused = noh(root, &["ql", "run", "recent", "0"]);
    assert_eq!(exit_code(&refused), 1, "{}", stderr(&refused));
    let errors = stderr(&refused);
    assert!(errors.contains("dynamic"), "{errors}");
    assert!(errors.contains("window"), "{errors}");
    assert!(
        stdout(&refused).is_empty(),
        "nothing is started: {}",
        stdout(&refused)
    );
}
