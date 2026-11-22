//! The states a listing shows beside a file's name: what the Git or SVN
//! checkout around the folder says about it, and whether a cloud provider
//! (OneDrive and services built on the same file attributes) has its bytes on
//! this device or only online.
//!
//! Neither answer belongs to the folder read: the version-control state comes
//! from outside the folder (the checkout root may be many levels up) and the
//! cloud state comes from each file's attributes, not from the directory
//! listing. Both are gathered here so the pane can ask once per folder, off
//! the foreground thread, and keep the maps for rendering.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// How long a `git` or `svn` call may take before it is killed. Both answer
/// from the local checkout normally; a network path or a locked index is the
/// case this protects against, and dropping the badges beats stalling them.
const TOOL_TIMEOUT: Duration = Duration::from_secs(4);

/// Which version control the folder on screen belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VcsKind {
    /// A Git checkout (`.git`, a directory or a worktree's file).
    Git,
    /// A Subversion working copy (`.svn`).
    Svn,
}

impl VcsKind {
    /// The name a tooltip calls the checkout.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Git => "Git",
            Self::Svn => "SVN",
        }
    }
}

/// What the badge beside a file's name says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum VcsState {
    /// Changed in the working tree.
    Modified,
    /// Added to the next commit.
    Added,
    /// Present but not tracked.
    Untracked,
    /// Gone from the working tree but still tracked.
    Deleted,
    /// Changed on both sides: an unresolved merge or update.
    Conflicted,
    /// Renamed in the working tree.
    Renamed,
    /// Copied in the working tree.
    Copied,
    /// Replaced (SVN's `R`).
    Replaced,
}

impl VcsState {
    /// The letter the badge shows. The first six are Git's own letters, which
    /// somebody who works with it already reads; replaced and renamed borrow
    /// the letters `svn status` uses.
    pub(crate) fn letter(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Untracked => "?",
            Self::Deleted => "D",
            Self::Conflicted => "C",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::Replaced => "R",
        }
    }

    /// How loudly the state asks to be looked at, for a folder, which shows
    /// the loudest state of anything under it.
    fn rank(self) -> u8 {
        match self {
            Self::Conflicted => 7,
            Self::Modified => 6,
            Self::Added => 5,
            Self::Deleted => 4,
            Self::Renamed => 3,
            Self::Copied => 2,
            Self::Replaced => 1,
            Self::Untracked => 0,
        }
    }
}

/// The version-control state of the files of one folder.
#[derive(Debug, Clone)]
pub(crate) struct VcsStatus {
    /// The checkout the folder belongs to.
    pub(crate) kind: VcsKind,
    /// The checkout's root, for the tooltip.
    pub(crate) root: String,
    /// State by lowercased full path. Folders are in here too, carrying the
    /// loudest state of the files under them: Git does not track folders, so
    /// otherwise a folder full of changes would look untouched.
    states: HashMap<String, VcsState>,
}

impl VcsStatus {
    /// The state of `path`, matching the spelling Windows compares paths with.
    pub(crate) fn state_of(&self, path: &str) -> Option<VcsState> {
        self.states.get(&normalize(path)).copied()
    }
}

/// Whether a cloud provider keeps the file's bytes on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncState {
    /// A placeholder: the content is only online and comes down when opened.
    CloudOnly,
    /// Pinned: the provider keeps a copy on this device.
    Pinned,
}

/// Reads the version-control state of `folder`, or `None` when it is not in a
/// checkout. Blocking: run it on a background executor, not while rendering.
pub(crate) fn scan(folder: &Path) -> Option<VcsStatus> {
    let (kind, root) = find_checkout(folder)?;
    let states = match kind {
        VcsKind::Git => git_states(&root, folder)?,
        VcsKind::Svn => svn_states(&root, folder)?,
    };
    Some(VcsStatus {
        kind,
        root: root.display().to_string(),
        states,
    })
}

/// The nearest checkout holding `folder`: the folder itself, then each parent.
/// A `.git` wins over a `.svn` at the same level, since a Subversion working
/// copy inside a Git checkout is the stranger arrangement.
fn find_checkout(folder: &Path) -> Option<(VcsKind, PathBuf)> {
    for ancestor in folder.ancestors() {
        if ancestor.join(".git").exists() {
            return Some((VcsKind::Git, ancestor.to_path_buf()));
        }
        if ancestor.join(".svn").exists() {
            return Some((VcsKind::Svn, ancestor.to_path_buf()));
        }
    }
    None
}

/// Runs `git status` for the files at or under `folder` and maps what it
/// reports onto full paths.
fn git_states(root: &Path, folder: &Path) -> Option<HashMap<String, VcsState>> {
    // Run from the folder and limit the status to it (`-- .` is relative to
    // the working directory), so a checkout of a whole disk is not walked for
    // one folder. Porcelain paths are always relative to the checkout root.
    let output = run(
        "git",
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--",
            ".",
        ],
        folder,
    )?;
    Some(parse_git_porcelain(&output, root))
}

/// Turns `git status --porcelain=v1 -z` output into states by full path.
fn parse_git_porcelain(output: &[u8], root: &Path) -> HashMap<String, VcsState> {
    let mut states = HashMap::new();
    let mut records = output.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        // `XY path`: two status letters, a space, then the path.
        if record.len() < 4 {
            continue;
        }
        let Some(status) = record.get(..2) else {
            continue;
        };
        let Some(path) = record.get(3..) else {
            continue;
        };
        let path = String::from_utf8_lossy(path).into_owned();
        // A rename or a copy follows its own record with the original name,
        // which carries no status letters of its own.
        if status.iter().any(|letter| matches!(letter, b'R' | b'C')) {
            records.next();
        }
        let Some(state) = classify_git(status) else {
            continue;
        };
        insert_with_folders(&mut states, root, &root.join(path), state);
    }
    states
}

/// The state a two-letter porcelain status stands for. Unmerged entries win
/// over everything else: they are the only ones that block work.
fn classify_git(status: &[u8]) -> Option<VcsState> {
    let x = status.first().copied().unwrap_or(b' ');
    let y = status.get(1).copied().unwrap_or(b' ');
    let either = |letter: u8| x == letter || y == letter;
    if either(b'U') || (x == b'D' && y == b'D') || (x == b'A' && y == b'A') {
        return Some(VcsState::Conflicted);
    }
    if x == b'?' || y == b'?' {
        return Some(VcsState::Untracked);
    }
    if x == b'!' || y == b'!' {
        return None;
    }
    if either(b'D') {
        return Some(VcsState::Deleted);
    }
    if x == b'A' {
        return Some(VcsState::Added);
    }
    if x == b'R' {
        return Some(VcsState::Renamed);
    }
    if x == b'C' {
        return Some(VcsState::Copied);
    }
    if either(b'M') || either(b'T') {
        return Some(VcsState::Modified);
    }
    None
}

/// Runs `svn status` in `folder` and maps what it reports onto full paths.
fn svn_states(root: &Path, folder: &Path) -> Option<HashMap<String, VcsState>> {
    let output = run(
        "svn",
        &["status", "--non-interactive", "--ignore-externals", "."],
        folder,
    )?;
    Some(parse_svn_status(
        &String::from_utf8_lossy(&output),
        root,
        folder,
    ))
}

/// Turns `svn status` output into states by full path. Each line is seven
/// status columns, a space, then the path (relative to `folder`).
fn parse_svn_status(text: &str, root: &Path, folder: &Path) -> HashMap<String, VcsState> {
    let mut states = HashMap::new();
    for line in text.lines() {
        let Some(flags) = line.get(..7) else {
            continue;
        };
        let Some(path) = line.get(8..).map(str::trim) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        let Some(state) = classify_svn(flags) else {
            continue;
        };
        let path = Path::new(path);
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            folder.join(path)
        };
        insert_with_folders(&mut states, root, &full, state);
    }
    states
}

/// The state a seven-column `svn status` line stands for. Column 7 is a tree
/// conflict and outranks the file's own status, as in Git.
fn classify_svn(flags: &str) -> Option<VcsState> {
    let mut columns = flags.chars();
    let item = columns.next().unwrap_or(' ');
    let properties = columns.next().unwrap_or(' ');
    let tree = flags.chars().nth(6).unwrap_or(' ');
    if item == 'C' || properties == 'C' || tree == 'C' {
        return Some(VcsState::Conflicted);
    }
    if item == 'D' || item == '!' {
        return Some(VcsState::Deleted);
    }
    if item == 'A' {
        return Some(VcsState::Added);
    }
    if item == 'R' {
        return Some(VcsState::Replaced);
    }
    if item == '?' {
        return Some(VcsState::Untracked);
    }
    if item == 'I' || item == 'X' {
        return None;
    }
    if item == 'M' || properties == 'M' || item == '~' {
        return Some(VcsState::Modified);
    }
    None
}

/// Records `state` for `path` and for each folder between it and `root`,
/// keeping the loudest state a folder can carry.
fn insert_with_folders(
    states: &mut HashMap<String, VcsState>,
    root: &Path,
    path: &Path,
    state: VcsState,
) {
    let root = path_key(root);
    for candidate in path.ancestors() {
        let key = path_key(candidate);
        let louder = states
            .get(&key)
            .is_none_or(|existing| state.rank() > existing.rank());
        if louder {
            states.insert(key.clone(), state);
        }
        if key == root {
            break;
        }
    }
}

/// The key the state maps are addressed by. `git status` prints paths with `/`
/// and the listing hands out `\`; lowercasing is how Windows compares them.
fn path_key(path: &Path) -> String {
    normalize(&path.to_string_lossy())
}

/// Normalizes a path for the state maps: one separator, one case (Windows).
pub(crate) fn normalize(text: &str) -> String {
    #[cfg(windows)]
    {
        text.replace('/', "\\").to_lowercase()
    }
    #[cfg(not(windows))]
    {
        text.to_string()
    }
}

/// Reads the cloud state of each path, leaving out the ones no provider
/// manages. Blocking: run it on a background executor.
pub(crate) fn scan_sync(paths: &[String]) -> HashMap<String, SyncState> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        paths
            .iter()
            .filter_map(|path| {
                let attributes = std::fs::metadata(path).ok()?.file_attributes();
                sync_state_of(attributes).map(|state| (normalize(path), state))
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        let _ = paths;
        HashMap::new()
    }
}

// Windows' file attributes for cloud placeholders, as `GetFileAttributes`
// reports them. A provider that keeps files in the cloud marks them here
// rather than in any registry LumaFiles could read cheaply.
#[cfg_attr(not(windows), allow(dead_code))]
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
#[cfg_attr(not(windows), allow(dead_code))]
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
#[cfg_attr(not(windows), allow(dead_code))]
const FILE_ATTRIBUTE_PINNED: u32 = 0x0008_0000;
#[cfg_attr(not(windows), allow(dead_code))]
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// The cloud state a set of file attributes describes. Pinning is a choice the
/// user made about the file, so it is reported over the placeholder flags a
/// provider sets while the bytes are not on the device yet.
#[cfg_attr(not(windows), allow(dead_code))]
fn sync_state_of(attributes: u32) -> Option<SyncState> {
    if attributes & FILE_ATTRIBUTE_PINNED != 0 {
        return Some(SyncState::Pinned);
    }
    let placeholder = FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    (attributes & placeholder != 0).then_some(SyncState::CloudOnly)
}

/// Runs `program` with `args` in `cwd` and returns its stdout when it answers
/// successfully within [`TOOL_TIMEOUT`]. `None` covers a missing program, a
/// non-zero exit and a hung one, all of which mean "no badges to show".
fn run(program: &str, args: &[&str], cwd: &Path) -> Option<Vec<u8>> {
    // `git` and `svn` are console programs: started from this GUI they would
    // flash a console window unless the command is hidden.
    let mut child = super::preview_listing::hidden_command(Path::new(program))
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read on another thread: a large status listing fills the pipe's buffer
    // and would block the child while this thread waited on its exit.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut buffer = Vec::new();
        stdout.read_to_end(&mut buffer).ok()?;
        Some(buffer)
    });
    let deadline = Instant::now() + TOOL_TIMEOUT;
    let exited = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => break None,
        }
    };
    if exited.is_none() {
        child.kill().ok();
    }
    child.wait().ok();
    let output = reader.join().ok().flatten();
    match exited {
        Some(status) if status.success() => output,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The checkout used by the parsing tests, laid out the way the platform
    // spells paths: the app runs on Windows, but the parsing must compile and
    // pass on the other systems too.
    fn repo() -> PathBuf {
        Path::new(if cfg!(windows) { r"C:\repo" } else { "/repo" }).to_path_buf()
    }

    fn key(name: &str) -> String {
        path_key(&repo().join(name))
    }

    fn states_of_git(output: &str) -> HashMap<String, VcsState> {
        parse_git_porcelain(output.as_bytes(), &repo())
    }

    #[test]
    fn git_porcelain_letters_map_to_states() {
        let output = " M src/lib.rs\0?? notes.txt\0A  added.rs\0 D gone.rs\0UU merged.rs\0";
        let states = states_of_git(output);
        assert_eq!(
            states.get(&key("src/lib.rs")).copied(),
            Some(VcsState::Modified)
        );
        assert_eq!(
            states.get(&key("notes.txt")).copied(),
            Some(VcsState::Untracked)
        );
        assert_eq!(states.get(&key("added.rs")).copied(), Some(VcsState::Added));
        assert_eq!(
            states.get(&key("gone.rs")).copied(),
            Some(VcsState::Deleted)
        );
        assert_eq!(
            states.get(&key("merged.rs")).copied(),
            Some(VcsState::Conflicted)
        );
    }

    #[test]
    fn git_rename_skips_the_original_name_record() {
        let output = "R  new name.rs\0old name.rs\0 M other.rs\0";
        let states = states_of_git(output);
        assert_eq!(
            states.get(&key("new name.rs")).copied(),
            Some(VcsState::Renamed)
        );
        // Had the original been taken as a record, its first letters ("ol")
        // would have left `other.rs` unnoticed.
        assert_eq!(
            states.get(&key("other.rs")).copied(),
            Some(VcsState::Modified)
        );
    }

    #[test]
    fn folders_carry_the_loudest_state_under_them() {
        let output = " M src/a.rs\0?? src/new.txt\0 M src/folder/deep.rs\0";
        let states = states_of_git(output);
        assert_eq!(
            states.get(&key("src")).copied(),
            Some(VcsState::Modified),
            "a folder with edits must not look untouched"
        );
        assert_eq!(
            states.get(&key("src/folder")).copied(),
            Some(VcsState::Modified)
        );
        // The checkout root is included: its own row, when shown, is useful too.
        assert_eq!(
            states.get(&path_key(&repo())).copied(),
            Some(VcsState::Modified)
        );
    }

    #[test]
    fn svn_status_lines_map_to_states() {
        let text = "M       src/main.rs\n?       new.txt\nA       added.txt\nD       gone.txt\nC       conflict.txt\nI       ignored.log\n      C  tree.txt\n";
        let states = parse_svn_status(text, &repo(), &repo());
        let state = |name: &str| states.get(&key(name)).copied();
        assert_eq!(state("src/main.rs"), Some(VcsState::Modified));
        assert_eq!(state("new.txt"), Some(VcsState::Untracked));
        assert_eq!(state("added.txt"), Some(VcsState::Added));
        assert_eq!(state("gone.txt"), Some(VcsState::Deleted));
        assert_eq!(state("conflict.txt"), Some(VcsState::Conflicted));
        assert_eq!(state("ignored.log"), None, "ignored files carry no badge");
        assert_eq!(state("tree.txt"), Some(VcsState::Conflicted));
    }

    #[test]
    fn cloud_attributes_read_as_cloud_only_or_pinned() {
        assert_eq!(sync_state_of(0), None);
        assert_eq!(
            sync_state_of(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS),
            Some(SyncState::CloudOnly)
        );
        assert_eq!(
            sync_state_of(FILE_ATTRIBUTE_OFFLINE),
            Some(SyncState::CloudOnly)
        );
        assert_eq!(
            sync_state_of(FILE_ATTRIBUTE_RECALL_ON_OPEN),
            Some(SyncState::CloudOnly)
        );
        assert_eq!(
            sync_state_of(FILE_ATTRIBUTE_PINNED),
            Some(SyncState::Pinned)
        );
        // Pinned and still a placeholder: the pin is what the user chose.
        assert_eq!(
            sync_state_of(FILE_ATTRIBUTE_PINNED | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS),
            Some(SyncState::Pinned)
        );
    }

    #[test]
    fn a_plain_folder_is_not_in_any_checkout() {
        let directory = tempfile::tempdir().unwrap();
        assert!(scan(directory.path()).is_none());
    }

    #[test]
    fn a_git_directory_marks_the_folder_as_a_checkout() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let nested = directory.path().join("src");
        std::fs::create_dir(&nested).unwrap();
        let (kind, root) = find_checkout(&nested).expect("a checkout");
        assert_eq!(kind, VcsKind::Git);
        assert_eq!(root, directory.path());
    }

    #[test]
    fn an_svn_directory_marks_a_working_copy() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".svn")).unwrap();
        let (kind, root) = find_checkout(directory.path()).expect("a working copy");
        assert_eq!(kind, VcsKind::Svn);
        assert_eq!(root, directory.path());
    }

    #[test]
    fn state_lookup_ignores_case_and_separators() {
        let status = VcsStatus {
            kind: VcsKind::Git,
            root: repo().display().to_string(),
            states: HashMap::from([(path_key(&repo().join("readme.md")), VcsState::Modified)]),
        };
        assert_eq!(
            status.state_of(&repo().join("README.md").display().to_string()),
            Some(VcsState::Modified)
        );
        assert_eq!(
            status.state_of(&repo().join("other.md").display().to_string()),
            None
        );
    }
}
