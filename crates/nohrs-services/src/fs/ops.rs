//! Filesystem mutation operations: copy, move, rename, create, trash, and
//! permanent delete, with cross-volume awareness and conflict-name resolution.
//!
//! These functions are synchronous filesystem IO. Callers that must stay
//! responsive during large operations should run them on a background executor
//! (mirroring how `search` offloads work via `cx.background_spawn`). The UI
//! layer is expected to route all mutations through this module rather than
//! calling `std::fs` directly (see `docs/explorer-essentials.md` §8).

use crate::fs::trash;
use crate::fs::transfer::{self, TransferProgress};
use nohrs_core::errors::{Error, Result};
use nohrs_store::TrashLedger;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// How a [`move_path`] operation was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveKind {
    /// The move stayed on a single filesystem and used `rename(2)`.
    Rename,
    /// The move was carried out as a recursive copy followed by deleting the
    /// source: source and destination are on different filesystems, or — for
    /// [`move_path_no_replace`] — no no-replace rename was available.
    CrossVolume,
}

/// A failed [`copy_path_no_replace`] or [`move_path_no_replace`], carrying what
/// it left at the destination alongside why it failed.
///
/// The error alone cannot say. Every write in a no-replace copy takes its path
/// with a call that fails if the name is already there, and the kernel reports
/// that as `EEXIST` without naming the path — so "the destination was already
/// someone else's" and "a name inside the tree this call had started writing was
/// taken" arrive as the same [`std::io::ErrorKind::AlreadyExists`]. A caller
/// rolling back has to tell them apart: clearing away its own half-written
/// destination is what lets the original come back, and clearing away a
/// stranger's destroys data nothing can recover.
#[derive(Debug)]
pub struct ClaimFailure {
    error: Error,
    leftover: Leftover,
    // The entry the destination was when this operation claimed it, so a
    // cleanup can tell it is still removing its own. Only meaningful alongside
    // [`Leftover::PartialDestination`]. `None` where the identity could not be
    // read — failing to read back what was just created is itself a sign
    // something reached the destination in between, so it permits nothing.
    claimed: Option<EntryIdentity>,
}

// A filesystem entry's identity: the same entry under any name, and a different
// entry at the same name after something swaps it.
type EntryIdentity = (u64, u64);

// The entry at `path` now — and, read straight after a claim, the entry that
// claim created.
#[cfg(unix)]
fn entry_identity(path: &Path) -> Option<EntryIdentity> {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path)
        .ok()
        .map(|entry| (entry.dev(), entry.ino()))
}

#[cfg(not(unix))]
fn entry_identity(_path: &Path) -> Option<EntryIdentity> {
    None
}

// The same through an open handle. `fstat` names the entry this call created
// whatever happens to the path afterwards, where a second `stat` by path could
// pick up something that replaced it in between.
#[cfg(unix)]
fn handle_identity(file: &fs::File) -> Option<EntryIdentity> {
    use std::os::unix::fs::MetadataExt;
    file.metadata().ok().map(|entry| (entry.dev(), entry.ino()))
}

// Windows claims a copied file inside `CopyFileExW` and never holds the handle.
#[cfg(not(any(unix, windows)))]
fn handle_identity(_file: &fs::File) -> Option<EntryIdentity> {
    None
}

// Removes `dst` while it is still the entry `claimed` names, reaching it through
// descriptors rather than by resolving the path a second time. Backs
// [`ClaimFailure::discard_partial_destination`], whose doc comment carries the
// reasoning and the two gaps this narrows without closing.
//
// A `None` claim permits nothing. It means the identity could not be read back
// straight after the claim, and failing to read back what was just created is
// itself a sign that something reached the destination in between.
#[cfg(unix)]
fn discard_claimed(dst: &Path, claimed: Option<EntryIdentity>) -> Result<()> {
    use rustix::fs::{AtFlags, unlinkat};
    let Some(claimed) = claimed else {
        return Ok(());
    };
    let (parent, name) = parent_and_name(dst)?;
    // Opened once and held for the rest of the call. Every step below names its
    // target relative to this descriptor, which is the directory itself rather
    // than a route to it, so nothing that happens to `parent` as a path in the
    // meantime is followed.
    let parent = open_directory(parent)?;
    let Some(entry) = open_claimed(&parent, name, claimed)? else {
        return Ok(());
    };
    if entry.metadata()?.is_dir() {
        // Emptied through `entry`, so the recursion below never consults the
        // name again and cannot be steered out of the tree it is clearing.
        empty_dir(&entry)?;
        // The name is needed once more here, and this is the one place where
        // that costs nothing: `rmdir` refuses a directory with anything in it,
        // and the directory this call just emptied is the only empty one that
        // can be sitting at the name.
        already_gone_is_fine(unlinkat(&parent, name, AtFlags::REMOVEDIR))?;
    } else {
        // Whereas here the name really is resolved a second time, because
        // `unlink` has no other form. Holding the parent removes every step of
        // the lookup above this last one.
        already_gone_is_fine(unlinkat(&parent, name, AtFlags::empty()))?;
    }
    Ok(())
}

// `ENOENT` from a removal is not a failure: the entry is gone, which is what the
// call wanted, and something else got there first. Treating it as one would
// abort a cleanup over the very race the cleanup is written to survive, leaving
// the partial destination standing.
//
// Anything else is a real failure, `ENOTEMPTY` included: an entry created inside
// a directory after this call emptied it means the partial is still there, and
// reporting that as done would be untrue.
#[cfg(unix)]
fn already_gone_is_fine(result: rustix::io::Result<()>) -> Result<()> {
    match result {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(Error::Io(error.into())),
    }
}

// Opens `path` as a directory, for use as the base of `openat` and `unlinkat`.
//
// `O_DIRECTORY` so nothing else can be opened by mistake, and `O_NONBLOCK` so a
// FIFO left at the name cannot make this wait for a writer that never comes —
// belt and braces, since `O_DIRECTORY` is refused before a FIFO's open would
// block, but the cost is nothing and the failure mode is a hung file operation.
//
// Deliberately *not* `O_NOFOLLOW`, which review suggested and which belongs on
// the entry rather than here. A parent whose last component is a symbolic link
// to a directory is an ordinary thing for a user to have navigated into, and
// refusing it would abandon the partial rather than clear it. Nor does following
// one endanger anything: what decides the removal is the identity check on the
// entry, and it only passes when `parent` is the directory actually holding the
// claimed inode.
#[cfg(unix)]
fn open_directory(path: &Path) -> Result<fs::File> {
    use rustix::fs::{Mode, OFlags, open};
    let directory = open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(fs::File::from(directory))
}

// Where there is no entry identity to compare, a cleanup has only the path it
// was given to go on. Withholding the removal instead would leave every partial
// destination standing, which is the failure this whole mechanism exists to
// stop.
#[cfg(not(unix))]
fn discard_claimed(dst: &Path, _claimed: Option<EntryIdentity>) -> Result<()> {
    delete_permanent(dst)
}

// Splits `path` into the directory to open and the name to look up inside it.
#[cfg(unix)]
fn parent_and_name(path: &Path) -> Result<(&Path, &OsStr)> {
    let name = path.file_name().ok_or_else(|| {
        Error::Other(format!(
            "cannot remove a path that names no entry: {}",
            path.display()
        ))
    })?;
    let parent = match path.parent() {
        // `Path::parent` reports a bare name's parent as the empty path, which
        // names nothing and cannot be opened. What a bare name resolves against
        // is the working directory.
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    Ok((parent, name))
}

// Opens `name` inside `parent`, and hands it back only while it is still the
// entry that was claimed.
//
// `O_NOFOLLOW` so a symbolic link left at the name cannot stand in for whatever
// it points at, and `O_NONBLOCK` so a FIFO left there cannot make this wait for
// a writer that never arrives. Neither could pass the identity check below, but
// both would have to be opened before it could run.
#[cfg(unix)]
fn open_claimed(
    parent: &fs::File,
    name: &OsStr,
    claimed: EntryIdentity,
) -> Result<Option<fs::File>> {
    use rustix::fs::{Mode, OFlags, openat};
    let entry = match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(entry) => fs::File::from(entry),
        // Nothing at the name, or a symbolic link, which `O_NOFOLLOW` reports as
        // `ELOOP` rather than opening its target. Neither is what `create_new`
        // or `mkdir` made, so there is nothing here to undo.
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP) => return Ok(None),
        Err(error) => return Err(Error::Io(error.into())),
    };
    // Through the open handle, so what is compared is the entry this call is
    // holding rather than whatever the name resolves to later on.
    if handle_identity(&entry) != Some(claimed) {
        return Ok(None);
    }
    Ok(Some(entry))
}

// Removes everything inside the directory `dir` refers to, leaving the directory
// itself. Each descent opens its child from the descriptor above it, so the walk
// stays inside the tree however that tree is rearranged while it runs.
#[cfg(unix)]
fn empty_dir(dir: &fs::File) -> Result<()> {
    use rustix::fs::{AtFlags, Dir, Mode, OFlags, openat, unlinkat};
    // Read to the end before removing anything. Whether a directory being read
    // reports entries that are removed from it after the read began is left
    // unspecified, so the listing is finished first and acted on second.
    let mut names = Vec::new();
    for entry in Dir::read_from(dir).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        names.push(name.to_owned());
    }
    for name in &names {
        let name = name.as_c_str();
        // Which of the two removals applies is asked of the kernel rather than
        // read from `d_type`, which some filesystems leave as `DT_UNKNOWN`.
        // `O_DIRECTORY` answers it and hands back the descriptor the recursion
        // needs in the same call.
        match openat(
            dir,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(child) => {
                empty_dir(&fs::File::from(child))?;
                already_gone_is_fine(unlinkat(dir, name, AtFlags::REMOVEDIR))?;
            }
            // Not a directory. A symbolic link lands here too: `O_NOFOLLOW`
            // refuses to open its target, and `O_DIRECTORY` is answered first,
            // so that refusal arrives as `ENOTDIR` rather than the `ELOOP` the
            // same flag gives on its own in `open_claimed`, which carries no
            // `O_DIRECTORY`. Measured on both supported targets. `LOOP` stays
            // matched because the order is the kernel's to choose rather than
            // anything this call guarantees, and
            // `a_symlink_is_refused_by_both_entry_opens` is what would notice a
            // kernel choosing otherwise.
            Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
                already_gone_is_fine(unlinkat(dir, name, AtFlags::empty()))?;
            }
            // Gone between the listing and here, so there is nothing to remove.
            Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(Error::Io(error.into())),
        }
    }
    Ok(())
}

/// What a failed no-replace copy or move left behind, and so what a caller
/// undoing it may touch. Exactly one of the three licenses a cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leftover {
    /// The destination was never taken, and the source is as it was. Anything
    /// at the destination belongs to whoever did take it, and removing it is
    /// unrecoverable.
    Nothing,
    /// The destination is this operation's own half-written work and the source
    /// it was reading is untouched — nothing writes to a destination before
    /// claiming it, and nothing removes a source before the copy is whole. This
    /// is the one case a caller may clear away, and doing so is what lets a
    /// rollback put the original back.
    PartialDestination,
    /// The copy landed whole and only removing the source failed, so the
    /// destination is a complete copy. `remove_dir_all` is not atomic: the
    /// source it gave up on may already be a fragment, which would make this the
    /// only whole copy there is. Neither path is a caller's to touch.
    WholeDestination,
}

/// What a no-replace copy or move returns: the outcome, or a [`ClaimFailure`].
pub type ClaimResult<T> = std::result::Result<T, ClaimFailure>;

impl ClaimFailure {
    fn untouched(error: impl Into<Error>) -> Self {
        Self {
            error: error.into(),
            leftover: Leftover::Nothing,
            claimed: None,
        }
    }

    fn partial(error: impl Into<Error>, claimed: Option<EntryIdentity>) -> Self {
        Self {
            error: error.into(),
            leftover: Leftover::PartialDestination,
            claimed,
        }
    }

    fn whole(error: impl Into<Error>) -> Self {
        Self {
            error: error.into(),
            leftover: Leftover::WholeDestination,
            claimed: None,
        }
    }

    /// What the failed operation left behind.
    pub fn leftover(&self) -> Leftover {
        self.leftover
    }

    /// Whether the destination is this operation's own unfinished work, which
    /// removing undoes — the one case a caller may clear away.
    pub fn left_a_partial_destination(&self) -> bool {
        self.leftover == Leftover::PartialDestination
    }

    /// Removes the half-written destination the failed operation left at `dst`,
    /// and nothing else. Callers rolling back should come through here rather
    /// than deciding for themselves and calling [`delete_permanent`].
    ///
    /// Does nothing unless [`Leftover::PartialDestination`] is what it left, and
    /// nothing if `dst` no longer names the entry the operation claimed. A tree
    /// takes time to write, and in that time another process can remove what
    /// this call created and put its own entry at the same name; deleting by
    /// path alone would take theirs.
    ///
    /// The entry is opened once and everything after that runs against the
    /// descriptor: the identity is read with `fstat`, and a directory is emptied
    /// through `openat` and `unlinkat` relative to it. So the path is resolved
    /// once rather than twice, and nothing done to it afterwards — an ancestor
    /// renamed, a component swapped for a symbolic link — can steer the removal
    /// anywhere else.
    ///
    /// Two gaps are narrowed rather than closed, so read the sentence above as
    /// what this aims at rather than a guarantee it can keep.
    ///
    /// `unlink` takes a name in a directory and never an inode — a body can
    /// carry several names, so "remove this descriptor" does not exist even in
    /// principle, on this platform or any other — which leaves one last lookup
    /// of the name in the pinned parent. For a directory that lookup can only
    /// ever cost the call: the contents are already gone by then, and `rmdir`
    /// refuses anything but an empty directory, so the worst a swap can
    /// substitute is another empty one. For everything else it is a real
    /// window, of two syscalls rather than the whole path resolution it
    /// replaces. Separately, an entry created at the same name after this one
    /// was removed can be handed the same inode number, in which case it is
    /// indistinguishable from what was claimed. What the identity does rule out
    /// is the whole length of the copy, where the replacement exists alongside
    /// the partial and so cannot share its number.
    ///
    /// Refusing to remove anything where that cannot be guaranteed is not the
    /// safer choice it looks like: leaving the partial standing is not a race
    /// but a certainty on every failed write, and it is what #277 removed.
    ///
    /// One more thing this does *not* do: inside a destination it claimed, it
    /// removes everything, including entries another writer created there while
    /// the copy ran. Those are what the nested `EEXIST` above is about. They are
    /// under a name this call created with `mkdir` and owns, and sparing them
    /// would leave the directory non-empty, fail the `rmdir`, and block the
    /// rollback that the whole claim exists to make possible — stranding the
    /// caller's original at a scratch path to save a file that arrived
    /// uninvited.
    pub fn discard_partial_destination(&self, dst: &Path) -> Result<()> {
        if self.leftover != Leftover::PartialDestination {
            return Ok(());
        }
        discard_claimed(dst, self.claimed)
    }

    /// The failure itself, without the claim.
    pub fn into_error(self) -> Error {
        self.error
    }
}

impl std::fmt::Display for ClaimFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for ClaimFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl From<ClaimFailure> for Error {
    fn from(failure: ClaimFailure) -> Self {
        failure.error
    }
}

/// How a name collision at the destination should be resolved when copying or
/// moving. The resolution itself is applied by the caller; this module only
/// provides the building blocks ([`would_conflict`], [`unique_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Keep both items by writing to a non-colliding name (see [`unique_name`]).
    Rename,
    /// Replace the existing destination.
    Overwrite,
    /// Leave the destination untouched and skip this item.
    Skip,
}

/// Returns whether `dst` is already occupied, the condition that triggers
/// conflict resolution before a copy or move.
///
/// Unlike [`Path::exists`], this probes the entry itself rather than following
/// symlinks, so a dangling symlink at `dst` still counts as occupied. An
/// ambiguous error (e.g. a permission failure while stat-ing) is treated
/// conservatively as occupied so the caller surfaces the conflict path rather
/// than silently overwriting.
pub fn would_conflict(dst: &Path) -> bool {
    path_occupied(dst)
}

/// Whether renaming `src` to `dst` would collide with something other than
/// `src` itself.
///
/// On a case-insensitive filesystem — NTFS, FAT, exFAT, APFS by default —
/// `Foto.jpg` is taken by `foto.jpg`, so [`would_conflict`] says yes to the
/// rename that only changes case, and the explorer numbered it into
/// `Foto (1).jpg` (04/11/2026). A rename onto its own entry is no collision:
/// the rename itself changes the stored case. A link at `dst` counts as a
/// collision even when it points at `src`, since renaming over it would
/// destroy the link.
pub fn would_collide_with_another(src: &Path, dst: &Path) -> bool {
    if !path_occupied(dst) {
        return false;
    }
    let link = fs::symlink_metadata(dst).is_ok_and(|entry| entry.is_symlink());
    link || !is_this_entry(src, dst)
}

// Whether `one` and `other` reach the same entry, case and links aside. On
// Windows `canonicalize` asks the filesystem for the final path of what it
// opens, with the case as stored, so both names come back the same when they
// are the same file.
#[cfg(windows)]
fn is_this_entry(one: &Path, other: &Path) -> bool {
    match (fs::canonicalize(one), fs::canonicalize(other)) {
        (Ok(one), Ok(other)) => one == other,
        _ => false,
    }
}

#[cfg(not(windows))]
fn is_this_entry(one: &Path, other: &Path) -> bool {
    is_same_entry(one, other)
}

// Whether a filesystem entry exists at `path`, detecting the entry itself
// (including a broken symlink) rather than following links. Ambiguous errors
// are reported as occupied; only a definitive "not found" is reported as free.
fn path_occupied(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

// Validates that `name` is a single, normal path component — not empty, not `.`
// or `..`, and free of path separators — so child-name inputs cannot escape the
// target directory when joined.
fn ensure_plain_name(name: &OsStr) -> Result<()> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if component == name => Ok(()),
        _ => Err(Error::Other(format!("invalid file name: {name:?}"))),
    }
}

/// The path `name` would take inside `dir`, refusing a `name` that is anything
/// but a single plain component.
///
/// Destination paths are assembled from names the caller did not choose — the
/// basename of a clipboard entry, of a dropped path — and joining one blindly is
/// how `..` reaches a directory the user never pointed at. Callers that build a
/// destination inside a directory should come through here rather than calling
/// [`Path::join`], the way [`rename_in_place`] and [`create_dir`] already do.
pub fn destination_in(dir: &Path, name: &OsStr) -> Result<PathBuf> {
    ensure_plain_name(name)?;
    Ok(dir.join(name))
}

/// Refuses an operation whose destination is the source itself or lies inside
/// it.
///
/// Copying a directory into its own subtree walks into the copy it is writing
/// and only stops when the disk is full; copying a file onto itself truncates it
/// before a byte is read. Enforced here rather than in the explorer so every
/// caller — paste today, drag and drop later — is covered by one check.
fn ensure_destination_outside_source(src: &Path, dst: &Path) -> Result<()> {
    let refuse = || {
        Err(Error::Other(format!(
            "cannot copy or move {} into itself: {}",
            src.display(),
            dst.display()
        )))
    };
    // Names can differ while the file does not: a hard link is a second name for
    // the same inode, and a symbolic link resolves to one. Either way the write
    // would land on what the copy is about to read, and a replacing write opens
    // the destination truncating — so the source is emptied before a byte of it
    // is read.
    // One entry under two names is still one entry, and copying it onto itself
    // is never what was meant — a symbolic link included, where the write would
    // otherwise land on the link and the read resolve to its target, so the
    // check below sees two different inodes and lets it through.
    if is_same_entry(src, dst) || writes_over_what_it_reads(src, dst) {
        return refuse();
    }
    if destination_path(dst).starts_with(source_path(src)) {
        return refuse();
    }
    Ok(())
}

// Whether the file a write to `dst` would land on is the one the copy reads from
// `src`, compared by identity rather than by name.
//
// What the copy reads is what `src` resolves to, links and all. What a write
// lands on depends on the source: for a symbolic link source, only the link is
// read and only a link is written, so the entry `dst` names is what would be
// destroyed — another link to the same target is a different entry, and copying
// one onto the other is an ordinary copy. For anything else the write follows
// `dst` the way `fs::copy` does.
#[cfg(unix)]
fn writes_over_what_it_reads(src: &Path, dst: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let source_is_link = fs::symlink_metadata(src).is_ok_and(|source| source.is_symlink());
    let destination = if source_is_link {
        fs::symlink_metadata(dst)
    } else {
        fs::metadata(dst)
    };
    match (fs::metadata(src), destination) {
        (Ok(source), Ok(destination)) => {
            source.dev() == destination.dev() && source.ino() == destination.ino()
        }
        _ => false,
    }
}

#[cfg(not(unix))]
fn writes_over_what_it_reads(_src: &Path, _dst: &Path) -> bool {
    false
}

// Whether both paths name the one filesystem entry, links unresolved.
#[cfg(unix)]
fn is_same_entry(one: &Path, other: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(one), fs::symlink_metadata(other)) {
        (Ok(one), Ok(other)) => one.dev() == other.dev() && one.ino() == other.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn is_same_entry(one: &Path, other: &Path) -> bool {
    containment_path(one) == containment_path(other)
}

// Where the source sits, with its own final component left unresolved: a copy
// recreates a symbolic link rather than following it, so copying a link *into
// the directory it points at* is not copying something into itself, and
// resolving the link would make it look like it was.
fn source_path(src: &Path) -> PathBuf {
    containment_path(src)
}

// Where a write to `dst` would actually land. Unlike the source, a symbolic link
// already sitting there is followed, because `create_dir_all` and `fs::copy`
// follow it too: a link pointing back into the source is a write into the
// source, however unrelated the path it is spelled with looks.
fn destination_path(dst: &Path) -> PathBuf {
    fs::canonicalize(dst).unwrap_or_else(|_| containment_path(dst))
}

// The path to compare containment with: every leading component that exists is
// resolved, so neither `..` nor an intermediate symlink can hide that one path
// is inside the other, and the components that do not exist yet (a destination
// is usually one of them) are appended as written.
fn containment_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut trailing = Vec::new();
    let mut current = absolute.as_path();
    loop {
        // `file_name` is `None` exactly where there is no final component to hold
        // back — a path ending in `..`, or the root. Resolving the whole thing is
        // then both safe and necessary: `dir/src/..` *is* `dir`, and left as
        // written it compares as something below `dir/src`, which is how a copy
        // of `dir` into its own child would slip past the guard.
        let Some(name) = current.file_name() else {
            if let Ok(resolved) = fs::canonicalize(current) {
                return with_trailing(resolved, &trailing);
            }
            break;
        };
        let Some(parent) = current.parent() else {
            break;
        };
        trailing.push(name);
        if let Ok(resolved) = fs::canonicalize(parent) {
            return with_trailing(resolved, &trailing);
        }
        current = parent;
    }
    absolute
}

fn with_trailing(mut resolved: PathBuf, trailing: &[&OsStr]) -> PathBuf {
    for name in trailing.iter().rev() {
        resolved.push(name);
    }
    resolved
}

/// Returns `true` when `src` and `dst_dir` reside on different filesystems, in
/// which case a move cannot use `rename(2)` and must copy then delete.
///
/// On non-Unix platforms device ids are not consulted, so this conservatively
/// returns `false`; [`move_path`] still detects the cross-device error from
/// `rename` and falls back to copy + delete regardless.
pub fn is_cross_volume(src: &Path, dst_dir: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // `rename(2)` acts on the source directory entry itself, so use the
        // entry's own device (don't follow a symlink). The destination is the
        // directory the entry lands in, so its resolved device is what matters.
        let src_dev = fs::symlink_metadata(src)?.dev();
        let dst_dev = fs::metadata(dst_dir)?.dev();
        Ok(src_dev != dst_dev)
    }
    #[cfg(not(unix))]
    {
        let _ = (src, dst_dir);
        Ok(false)
    }
}

/// Produces a file name within `dir` that does not collide with an existing
/// entry, deriving it from `name` by inserting ` (N)` before the extension
/// (`report.pdf` becomes `report (2).pdf`), trying `N = 2, 3, ...` until a free
/// name is found. Returns `name` unchanged when there is no collision.
pub fn unique_name(dir: &Path, name: &str) -> String {
    if !path_occupied(&dir.join(name)) {
        return name.to_string();
    }
    let path = Path::new(name);
    let extension = path.extension().and_then(|ext| ext.to_str());
    // `file_stem` is `None` only for empty names; fall back to the full name so
    // we never panic and always make progress.
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let mut counter: u32 = 2;
    loop {
        let candidate = match extension {
            Some(extension) => format!("{stem} ({counter}).{extension}"),
            None => format!("{stem} ({counter})"),
        };
        if !path_occupied(&dir.join(&candidate)) {
            return candidate;
        }
        counter += 1;
    }
}

/// Recursively copies `src` (a file, symbolic link, or directory) to `dst`,
/// where `dst` is the full destination path rather than its parent directory.
/// Missing parent directories are created; an existing destination is
/// overwritten. Fails if `dst` is `src` or sits inside it.
#[tracing::instrument(target = "nohrs::op", name = "fs.copy", level = "debug", skip_all, fields(src = %src.display(), dst = %dst.display()))]
pub fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    copy_path_inner(src, dst, None)
}

/// [`copy_path`] while reporting bytes to `progress`, and stopping with
/// [`Error::Cancelled`] when the user cancels.
#[tracing::instrument(target = "nohrs::op", name = "fs.copy", level = "debug", skip_all, fields(src = %src.display(), dst = %dst.display()))]
pub fn copy_path_reported(src: &Path, dst: &Path, progress: &TransferProgress) -> Result<()> {
    copy_path_inner(src, dst, Some(progress))
}

fn copy_path_inner(src: &Path, dst: &Path, progress: Option<&TransferProgress>) -> Result<()> {
    ensure_destination_outside_source(src, dst)?;
    // Before the parent is created, so a copy of something that is not there
    // leaves no empty directory behind.
    transfer::check(progress)?;
    let metadata = fs::symlink_metadata(src)?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if metadata.is_symlink() {
        copy_symlink(src, dst, Existing::Replace, progress)
    } else if metadata.is_dir() {
        copy_dir_all(src, dst, Existing::Replace, progress)
    } else {
        copy_file(src, dst, Existing::Replace, progress)
    }
}

/// What a recursive copy does about an entry already sitting at a destination
/// path.
///
/// Symbolic links are recreated as links under either, never followed. Following
/// one was what made copying an ordinary folder fail: a link to a directory is
/// not itself a directory, so it went to `fs::copy`, which opened the directory
/// behind it and gave up with `EISDIR` partway through the copy. Recreating also
/// settles the two questions following raises — a link to an ancestor would
/// recurse forever, and one pointing out of the tree would pull in data the user
/// never asked to copy — by never reading through one. It is what `cp -R` and
/// the Finder do with a link inside a folder they copy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Existing {
    /// Replace it. [`copy_path`] is documented to overwrite, and the
    /// cross-volume half of [`move_path`] has to land where `rename(2)` would
    /// have.
    Replace,
    /// Refuse it, the guarantee [`move_path_no_replace`] rests on: every
    /// destination is claimed by a call that fails if the name is taken.
    Refuse,
}

// Recreates the symbolic link `from` at `to`.
fn copy_symlink(
    from: &Path,
    to: &Path,
    existing: Existing,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    transfer::check(progress)?;
    let target = fs::read_link(from)?;
    // A directory is left alone: removing one to make room for a link would
    // discard whatever it holds, which is more than an overwrite was asked to do.
    if existing == Existing::Replace
        && let Ok(metadata) = fs::symlink_metadata(to)
        && !metadata.is_dir()
    {
        fs::remove_file(to)?;
    }
    symlink_no_replace(&target, to)?;
    if let Some(progress) = progress {
        progress.file_done();
    }
    Ok(())
}

// Copies the regular file `from` to `to`.
fn copy_file(
    from: &Path,
    to: &Path,
    existing: Existing,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    if existing == Existing::Replace && progress.is_none() {
        // The fast path with nothing to report: one kernel call.
        fs::copy(from, to)?;
        return Ok(());
    }
    if existing == Existing::Replace {
        return copy_file_replacing_reported(from, to, progress);
    }
    copy_file_claiming(from, to, progress).map_err(ClaimFailure::into_error)
}

/// Copies the regular file `from` to `to`, taking `to` with a call that fails if
/// the name is already there, and says which side of that claim a failure fell
/// on.
///
/// On Windows that is `CopyFileExW` with `COPY_FILE_FAIL_IF_EXISTS`, which
/// creates the destination with `CREATE_NEW` inside the call: the same claim as
/// `create_new`, so nothing that took the name first is written over. Until
/// 04/11/2026 this was `create_new` plus `std::io::copy` here too, which wrote
/// the file again in user-mode chunks and stamped it with the time of the
/// paste. `CopyFileExW` is what Explorer's copy and `fs::copy` use: it keeps
/// the modification time, the attributes and the alternate streams, lets a
/// network share copy on the server side, and picks its own buffers
/// (`examples/copy_ab.rs`; `docs/PERFORMANCE.md`, passada 4).
#[cfg(windows)]
#[allow(unsafe_code)]
fn copy_file_claiming(
    from: &Path,
    to: &Path,
    progress: Option<&TransferProgress>,
) -> ClaimResult<()> {
    use windows::Win32::Storage::FileSystem::COPY_FILE_FAIL_IF_EXISTS;
    let Some(progress) = progress else {
        use windows::Win32::Storage::FileSystem::CopyFileExW;
        use windows::core::PCWSTR;
        let (source, destination) = (wide(from), wide(to));
        // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the call; no
        // progress routine, data or cancel flag is passed.
        let copied = unsafe {
            CopyFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(destination.as_ptr()),
                None,
                None,
                None,
                COPY_FILE_FAIL_IF_EXISTS,
            )
        };
        return claim_copy_result(copied.map_err(win32_io_error), to);
    };
    // With a dialog to feed, the same call carries the progress routine, which
    // also answers the user's Cancel.
    let copied = copy_file_ex(from, to, true, progress);
    if progress.cancelled() {
        // CopyFileExW's own cleanup of an aborted copy is not something to
        // rely on (the destination may be there, part-written) and the
        // identity based cleanup below does nothing on Windows; the name is
        // ours — it was claimed with CREATE_NEW inside the call — so it goes.
        let _ = fs::remove_file(to);
        return Err(ClaimFailure::untouched(Error::Cancelled));
    }
    claim_copy_result(copied, to)
}

#[cfg(windows)]
fn claim_copy_result(copied: std::io::Result<()>, to: &Path) -> ClaimResult<()> {
    let Err(error) = copied else {
        return Ok(());
    };
    // "The name was taken" is answered before anything is written, so it never
    // leaves a destination of ours. Any other failure may come after the
    // claim: CopyFileExW removes what it wrote when it gives up, but if
    // something is at the name now it is reported as a partial destination.
    // The identity is unknown here, as it always was on Windows, so a caller's
    // cleanup removes nothing (`discard_claimed`); the report just says where
    // it is.
    if error.kind() == std::io::ErrorKind::AlreadyExists || fs::symlink_metadata(to).is_err() {
        Err(ClaimFailure::untouched(error))
    } else {
        Err(ClaimFailure::partial(error, None))
    }
}

/// `CopyFileExW` with the progress routine installed, for a copy that reports
/// bytes and can be cancelled mid-file.
#[cfg(windows)]
#[allow(unsafe_code)]
fn copy_file_ex(
    from: &Path,
    to: &Path,
    fail_if_exists: bool,
    progress: &TransferProgress,
) -> std::io::Result<()> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        COPY_FILE_FAIL_IF_EXISTS, COPYFILE_FLAGS, COPYPROGRESSROUTINE_PROGRESS, CopyFileExW,
        LPPROGRESS_ROUTINE_CALLBACK_REASON, PROGRESS_CANCEL, PROGRESS_CONTINUE,
    };
    use windows::core::PCWSTR;

    // What the routine writes back: bytes already reported per file stream, so
    // each callback adds the difference instead of the running total again.
    struct Ticker<'a> {
        progress: &'a TransferProgress,
        reported: [i64; 16],
    }

    unsafe extern "system" fn tick(
        _total: i64,
        transferred: i64,
        _stream_size: i64,
        _stream_transferred: i64,
        stream: u32,
        _reason: LPPROGRESS_ROUTINE_CALLBACK_REASON,
        _source: HANDLE,
        _destination: HANDLE,
        data: *const core::ffi::c_void,
    ) -> COPYPROGRESSROUTINE_PROGRESS {
        // SAFETY: `data` is the `Ticker` `copy_file_ex` passes to
        // `CopyFileExW`, which hands the same pointer back untouched, and no
        // Rust reference to it is alive across the call.
        let ticker = unsafe { &mut *(data as *mut Ticker) };
        if ticker.progress.cancelled() {
            return PROGRESS_CANCEL;
        }
        let slot = stream as usize;
        if slot < ticker.reported.len() {
            let last = ticker.reported[slot];
            if transferred > last {
                ticker.progress.add_bytes((transferred - last) as u64);
                ticker.reported[slot] = transferred;
            }
        }
        PROGRESS_CONTINUE
    }

    let (source, destination) = (wide(from), wide(to));
    let mut ticker = Ticker {
        progress,
        reported: [0; 16],
    };
    let flags = if fail_if_exists {
        COPY_FILE_FAIL_IF_EXISTS
    } else {
        COPYFILE_FLAGS(0)
    };
    // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the call, the
    // ticker lives across it, and the routine only touches that ticker.
    let copied = unsafe {
        CopyFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            Some(tick),
            Some(&mut ticker as *mut Ticker as *const core::ffi::c_void),
            None,
            flags,
        )
    };
    copied.map_err(win32_io_error)
}

/// A copy that overwrites, reporting as it goes; the `Existing::Replace` side
/// of [`copy_file`] when a dialog is watching.
#[cfg(windows)]
fn copy_file_replacing_reported(
    from: &Path,
    to: &Path,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    let Some(progress) = progress else {
        return Err(Error::Other("no progress to report".into()));
    };
    // The name is free (the paste plan claimed it), so asking for "fail if it
    // exists" keeps the same guarantee as the claim.
    let copied = copy_file_ex(from, to, true, progress);
    if progress.cancelled() {
        let _ = fs::remove_file(to);
        return Err(Error::Cancelled);
    }
    copied.map_err(Error::Io)
}

#[cfg(not(windows))]
fn copy_file_claiming(
    from: &Path,
    to: &Path,
    progress: Option<&TransferProgress>,
) -> ClaimResult<()> {
    // Opening the source first, so a source that cannot be read fails before the
    // destination name is taken.
    let source = fs::File::open(from).map_err(ClaimFailure::untouched)?;
    let destination = claim_file(to).map_err(ClaimFailure::untouched)?;
    // Through the handle rather than the path: this is the entry `create_new`
    // itself made, so nothing that takes the name in between can be recorded as
    // the claim and later deleted as though it were.
    let claimed = handle_identity(&destination);
    fill_file(from, to, source, destination, progress)
        .map_err(|error| ClaimFailure::partial(error, claimed))
}

#[cfg(not(windows))]
fn copy_file_replacing_reported(
    from: &Path,
    to: &Path,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    let source = fs::File::open(from)?;
    let destination = fs::File::create(to)?;
    fill_file(from, to, source, destination, progress)
}

// Takes the name `to` with a call that fails if it is already there — the point
// past which everything at that path was written by this copy.
#[cfg(not(windows))]
fn claim_file(to: &Path) -> Result<fs::File> {
    Ok(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?)
}

// Writes `from` into the already-claimed `to`. With a progress board the bytes
// go through a chunk loop, so the dialog moves and Cancel is answered inside a
// single large file rather than at the next one.
#[cfg(not(windows))]
fn fill_file(
    from: &Path,
    to: &Path,
    mut source: fs::File,
    mut destination: fs::File,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    match progress {
        None => {
            std::io::copy(&mut source, &mut destination)?;
        }
        Some(progress) => {
            use std::io::{Read as _, Write as _};
            const CHUNK: usize = 256 * 1024;
            let mut buffer = vec![0_u8; CHUNK];
            loop {
                transfer::check(Some(progress))?;
                let read = source.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                destination.write_all(&buffer[..read])?;
                progress.add_bytes(read as u64);
            }
        }
    }
    // After the contents, so a read-only mode cannot stop the copy that has to
    // happen first.
    fs::set_permissions(to, fs::metadata(from)?.permissions())?;
    Ok(())
}

// Recursively copies the contents of directory `src` into `dst`, creating `dst`
// itself. Under `Refuse` every entry is claimed by a call that fails if the name
// is taken — not just the root: a tree takes time to write, and the guarantee is
// worth nothing if something else can create an entry inside it meanwhile and
// have it silently replaced.
fn copy_dir_all(
    src: &Path,
    dst: &Path,
    existing: Existing,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    claim_dir(dst, existing)?;
    fill_dir(src, dst, existing, progress)
}

// Creates `dst` itself — the point past which everything under that path was
// written by this copy.
fn claim_dir(dst: &Path, existing: Existing) -> Result<()> {
    match existing {
        // `create_dir_all` so intermediate directories a caller left out are
        // created, matching what `copy_path` documents.
        Existing::Replace => fs::create_dir_all(dst)?,
        // Refuses an existing directory, and a symbolic link standing in for one,
        // which `create_dir_all` would have followed.
        Existing::Refuse => fs::create_dir(dst)?,
    }
    Ok(())
}

// Copies the contents of `src` into the already-claimed `dst`.
fn fill_dir(
    src: &Path,
    dst: &Path,
    existing: Existing,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    for entry in fs::read_dir(src)? {
        // Between entries: a folder copy is the case with an item to reach.
        transfer::check(progress)?;
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if let Some(progress) = progress {
            progress.set_detail(entry.file_name().to_string_lossy().into_owned());
        }
        if file_type.is_symlink() {
            copy_symlink(&from, &to, existing, progress)?;
        } else if file_type.is_dir() {
            copy_dir_all(&from, &to, existing, progress)?;
        } else {
            copy_file(&from, &to, existing, progress)?;
            if let Some(progress) = progress {
                progress.file_done();
            }
        }
    }
    // `create_dir_all` derives the mode from the umask, so a private `0700`
    // directory would otherwise arrive world-readable and expose what it holds.
    // Applied last: a directory the source made unwritable must not become
    // unwritable here until everything is inside it.
    fs::set_permissions(dst, fs::metadata(src)?.permissions())?;
    Ok(())
}

/// Moves `src` to `dst`. Uses `rename(2)` when both reside on the same
/// filesystem; otherwise (a cross-volume move) copies `src` recursively to
/// `dst` and then removes the source, reporting which path was taken. Fails if
/// `dst` is `src` or sits inside it.
#[tracing::instrument(target = "nohrs::op", name = "fs.move", level = "debug", skip_all, fields(src = %src.display(), dst = %dst.display()))]
pub fn move_path(src: &Path, dst: &Path) -> Result<MoveKind> {
    ensure_destination_outside_source(src, dst)?;
    match fs::rename(src, dst) {
        Ok(()) => Ok(MoveKind::Rename),
        Err(error) if is_cross_device(&error) => {
            copy_path(src, dst)?;
            delete_permanent(src)?;
            Ok(MoveKind::CrossVolume)
        }
        Err(error) => Err(Error::Io(error)),
    }
}

/// Moves `src` to `dst` like [`move_path`], but fails instead of replacing
/// anything that is already at `dst`.
///
/// Checking with [`would_conflict`] and then calling [`move_path`] is not the
/// same thing: `rename(2)` replaces, so a file created in between is silently
/// destroyed. Restoring from the trash writes to a path the user last saw empty
/// minutes or days ago, which is exactly when something else may have taken it.
///
/// On Linux, macOS and Windows the rename itself carries the no-replace flag, so
/// the whole move is atomic against a concurrent create. Across drives — and on
/// filesystems that reject the flag — it falls back to [`copy_path_no_replace`]
/// plus deleting the source, which claims the destination atomically too; a
/// check followed by a replacing `rename` would not, which is the very hole this
/// function exists to close.
///
/// A failure reports what it left at the destination (see [`ClaimFailure`]),
/// which is what a caller rolling back needs and cannot get from the error.
/// Failing to remove the source is never reported as a partial destination: the
/// destination holds the whole copy by then, and `remove_dir_all` is not atomic,
/// so the source it could not finish removing may be a fragment — which would
/// make that copy the only whole one there is.
pub fn move_path_no_replace(src: &Path, dst: &Path) -> ClaimResult<MoveKind> {
    move_path_no_replace_inner(src, dst, None)
}

/// [`move_path_no_replace`] while reporting bytes to `progress`, and stopping
/// with [`Error::Cancelled`] when the user cancels. A cut within one volume is
/// a rename and finishes at once; the reporting matters for the cross-volume
/// copy-and-delete half.
#[tracing::instrument(target = "nohrs::op", name = "fs.move", level = "debug", skip_all, fields(src = %src.display(), dst = %dst.display()))]
pub fn move_path_no_replace_reported(
    src: &Path,
    dst: &Path,
    progress: &TransferProgress,
) -> ClaimResult<MoveKind> {
    move_path_no_replace_inner(src, dst, Some(progress))
}

fn move_path_no_replace_inner(
    src: &Path,
    dst: &Path,
    progress: Option<&TransferProgress>,
) -> ClaimResult<MoveKind> {
    ensure_destination_outside_source(src, dst).map_err(ClaimFailure::untouched)?;
    match rename_no_replace(src, dst) {
        Some(Ok(())) => return Ok(MoveKind::Rename),
        // `rename(2)` either moves the entry or leaves everything as it was, so
        // a failure here never took the destination.
        Some(Err(error)) if !is_cross_device(&error) => {
            return Err(ClaimFailure::untouched(error));
        }
        // Cross-device, or no no-replace rename to be had: copy and delete.
        Some(Err(_)) | None => {}
    }
    copy_path_no_replace_inner(src, dst, progress)?;
    // A Cancel that arrived during the copy leaves the source where it is: the
    // copy is cleared away here rather than left as a second, unasked-for copy.
    if transfer::check(progress).is_err() {
        let _ = delete_permanent(dst);
        return Err(ClaimFailure::untouched(Error::Cancelled));
    }
    delete_permanent(src).map_err(ClaimFailure::whole)?;
    Ok(MoveKind::CrossVolume)
}

/// `rename(2)` with the platform's no-replace flag, or `None` where the
/// platform or the filesystem does not offer one and the caller must fall back.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_no_replace(src: &Path, dst: &Path) -> Option<std::io::Result<()>> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    match renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE) {
        Ok(()) => Some(Ok(())),
        // The flag reaches the filesystem driver, and not every driver takes
        // it: a non-APFS macOS volume reports `ENOTSUP`, a driver that does not
        // implement it `EINVAL`. `ENOSYS` is the older and separate case of the
        // kernel having no `renameat2` at all, before Linux 3.15 — the syscall
        // has existed since then, and 4.9 is only when `RENAME_NOREPLACE`
        // reached many of the filesystems that had been answering `EINVAL`.
        //
        // Not overlayfs, which was named here for years and does not belong:
        // measured on 6.18 it takes the flag and answers `EEXIST` like ext4. It
        // does reach the fallback, but through `EXDEV` on a lower-layer
        // directory, which `move_path_no_replace` sends down the cross-device
        // branch rather than this one.
        Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP) => {
            None
        }
        Err(error) => Some(Err(std::io::Error::from(error))),
    }
}

/// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` is Windows' no-replace
/// rename: the filesystem refuses an occupied name inside the rename itself
/// (`ERROR_ALREADY_EXISTS`), files and directories alike, and a destination on
/// another drive answers `ERROR_NOT_SAME_DEVICE`, which sends the caller to the
/// copy. `std::fs::rename` always passes the replacing flag, so it cannot be
/// used here.
///
/// Until 04/11/2026 this answered `None` on Windows, and every move took the
/// copy-and-delete fallback even within one drive: Cut + Paste of a folder
/// wrote it all a second time and needed room for a second copy, and a folder
/// holding a link or junction could not be moved at all, since the copy
/// refuses links here. `examples/move_ab.rs`, C:, median of 5 alternated
/// rounds with Premiere open: 2,000 files of 64 KB, 3,222 -> 3.9 ms; one file
/// of 512 MB, 2,307 -> 2.5 ms (`docs/PERFORMANCE.md`, passada 2).
///
/// No `MOVEFILE_COPY_ALLOWED`: with it Windows would copy across drives by
/// itself, replacing nothing but also claiming nothing entry by entry, which
/// is the guarantee [`copy_path_no_replace`] gives the cross-drive case.
#[cfg(windows)]
#[allow(unsafe_code)]
fn rename_no_replace(src: &Path, dst: &Path) -> Option<std::io::Result<()>> {
    use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
    use windows::core::PCWSTR;
    let (from, to) = (wide(src), wide(dst));
    // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the call,
    // which only reads them.
    let moved = unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), MOVE_FILE_FLAGS(0)) };
    Some(moved.map_err(win32_io_error))
}

/// NUL-terminated UTF-16 for a Win32 call.
#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

// The HRESULT wraps the Win32 code (0x8007xxxx, negative as an i32);
// `is_cross_device` and the callers' `AlreadyExists` checks read the raw code,
// so it is unwrapped here. The crate's own `From` keeps the whole HRESULT as
// the "OS error", which matches nothing.
#[cfg(windows)]
fn win32_io_error(error: windows::core::Error) -> std::io::Error {
    let code = u32::from_ne_bytes(error.code().0.to_ne_bytes());
    match i32::try_from(code & 0xFFFF) {
        Ok(win32) if code >> 16 == 0x8007 => std::io::Error::from_raw_os_error(win32),
        _ => std::io::Error::other(error),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn rename_no_replace(_src: &Path, _dst: &Path) -> Option<std::io::Result<()>> {
    None
}

/// Copies `src` to `dst` like [`copy_path`], but claims the destination rather
/// than writing over whatever is there.
///
/// Every branch takes `dst` with a call that fails if the name is taken —
/// `symlink`, `create_dir`, `create_new` — so nothing that appeared since the
/// caller looked is destroyed. Checking with [`would_conflict`] and then calling
/// [`copy_path`] is not the same thing: `fs::copy` opens the destination
/// truncating, so a file created in between loses its contents before a byte is
/// read. Permissions are carried across as well, which is what makes this the
/// copying half of [`move_path_no_replace`]: a move has to hand back what it was
/// given.
///
/// A failure reports which side of that claim it fell on (see [`ClaimFailure`]).
/// Only the operation itself knows: `EEXIST` is what the destination being taken
/// first and a name colliding inside a tree this call had already claimed both
/// come back as, and only the second leaves something a caller may remove.
pub fn copy_path_no_replace(src: &Path, dst: &Path) -> ClaimResult<()> {
    copy_path_no_replace_inner(src, dst, None)
}

/// [`copy_path_no_replace`] while reporting bytes to `progress`, and stopping
/// with [`Error::Cancelled`] when the user cancels.
#[tracing::instrument(target = "nohrs::op", name = "fs.copy", level = "debug", skip_all, fields(src = %src.display(), dst = %dst.display()))]
pub fn copy_path_no_replace_reported(
    src: &Path,
    dst: &Path,
    progress: &TransferProgress,
) -> ClaimResult<()> {
    copy_path_no_replace_inner(src, dst, Some(progress))
}

fn copy_path_no_replace_inner(
    src: &Path,
    dst: &Path,
    progress: Option<&TransferProgress>,
) -> ClaimResult<()> {
    ensure_destination_outside_source(src, dst).map_err(ClaimFailure::untouched)?;
    transfer::check(progress).map_err(ClaimFailure::untouched)?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(ClaimFailure::untouched)?;
    }
    let metadata = fs::symlink_metadata(src).map_err(ClaimFailure::untouched)?;
    if metadata.is_symlink() {
        // `symlink(2)` takes the name and writes the link in the one call, so it
        // did both or neither: a failure never leaves the destination ours.
        return copy_symlink(src, dst, Existing::Refuse, progress)
            .map_err(ClaimFailure::untouched);
    }
    if metadata.is_dir() {
        claim_dir(dst, Existing::Refuse).map_err(ClaimFailure::untouched)?;
        // Read straight after the claim, so what a cleanup compares against is
        // the entry this call created rather than whatever ends up at the name.
        // `mkdir` hands back no handle, so this one is a `stat` by path and
        // carries the gap the cleanup's doc comment owns up to.
        let claimed = entry_identity(dst);
        return fill_dir(src, dst, Existing::Refuse, progress).map_err(|error| {
            // A cancelled tree copy leaves a half-filled destination. On
            // Windows the identity above is None, so the generic cleanup cannot
            // take it; the name was free until this call claimed it, so what is
            // there is the copy's own work.
            if matches!(error, Error::Cancelled) {
                let _ = fs::remove_dir_all(dst);
                ClaimFailure::untouched(error)
            } else {
                ClaimFailure::partial(error, claimed)
            }
        });
    }
    copy_file_claiming(src, dst, progress)
}

// Recreates a symlink to `target` at `link`, failing if `link` is taken.
#[cfg(unix)]
fn symlink_no_replace(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

// Linux and macOS are the platforms the explorer targets, and the only ones a
// recursive copy can hand a symbolic link back on. Refusing beats the
// alternative of silently replacing a link with a copy of whatever it pointed
// at.
#[cfg(not(unix))]
fn symlink_no_replace(_target: &Path, link: &Path) -> Result<()> {
    Err(Error::Other(format!(
        "cannot recreate a symbolic link on this platform: {}",
        link.display()
    )))
}

// Whether an IO error from `rename` indicates the source and destination are on
// different devices, the signal to fall back to copy + delete.
#[cfg(unix)]
fn is_cross_device(error: &std::io::Error) -> bool {
    // EXDEV ("Invalid cross-device link") is 18 on Linux and macOS.
    error.raw_os_error() == Some(18)
}

#[cfg(windows)]
fn is_cross_device(error: &std::io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE.
    error.raw_os_error() == Some(17)
}

#[cfg(not(any(unix, windows)))]
fn is_cross_device(_error: &std::io::Error) -> bool {
    false
}

/// Renames `src` to `new_name` within its current directory, returning the new
/// full path. `new_name` must be a bare file name, not a path with separators.
#[tracing::instrument(target = "nohrs::op", name = "fs.rename", level = "debug", skip_all, fields(src = %src.display(), new_name))]
pub fn rename_in_place(src: &Path, new_name: &str) -> Result<PathBuf> {
    let parent = src.parent().ok_or_else(|| {
        Error::Other(format!(
            "cannot rename path without a parent: {}",
            src.display()
        ))
    })?;
    let dst = destination_in(parent, OsStr::new(new_name))?;
    fs::rename(src, &dst)?;
    Ok(dst)
}

/// Creates a new directory named `name` inside `parent`, returning its full
/// path. Fails if a file or directory of that name already exists.
#[tracing::instrument(target = "nohrs::op", name = "fs.create_dir", level = "debug", skip_all, fields(parent = %parent.display(), name))]
pub fn create_dir(parent: &Path, name: &str) -> Result<PathBuf> {
    let dst = destination_in(parent, OsStr::new(name))?;
    fs::create_dir(&dst)?;
    Ok(dst)
}

/// Moves `path` to the operating system's trash/recycle bin.
///
/// `ledger` receives a record of where the item came from, so it can be restored
/// later. Pass `None` where nothing will read it: on Linux and Windows the OS
/// trash keeps the same facts and [`crate::fs::trash::OsStore`] restores from
/// those, so a second copy would grow without bound with no reader.
/// [`crate::fs::trash::OS_INDEX_AVAILABLE`] is how a caller decides.
#[tracing::instrument(target = "nohrs::op", name = "fs.trash", level = "debug", skip_all, fields(path = %path.display()))]
pub fn trash_path(path: &Path, ledger: Option<&dyn TrashLedger>) -> Result<()> {
    // Captured before the move, while the item is still at its original
    // location — that is the whole point of the record. Failing here fails the
    // whole operation: nothing has moved yet, and deleting an item we already
    // know we cannot record would make it unrestorable on this platform.
    let captured = match ledger {
        Some(_) => Some(trash::capture(path)?),
        None => None,
    };
    os_trash_delete(path)?;
    // Past this point the item is in the trash, so a ledger write that fails
    // cannot be reported as a failed delete — but it must not vanish either.
    if let (Some(ledger), Some(captured)) = (ledger, captured)
        && let Err(error) = record_trashed(ledger, &captured)
    {
        tracing::warn!(
            path = %path.display(),
            %error,
            "could not record the trashed item; it will not appear in `noh trash list`"
        );
    }
    Ok(())
}

/// Moves every path in `paths` to the trash, returning the ones that could not
/// go, each with why.
///
/// Where the OS keeps the trash index (`ledger` is `None`, see [`trash_path`])
/// the whole selection is one call to the system trash. On Windows that is one
/// `IFileOperation` and one thread for its COM apartment, where item by item
/// paid both per file: 1,000 files went from 35.8 to 9.9 s, 200 from 7.0 to
/// 1.9 s (`examples/trash_ab.rs`, median of 3 alternated rounds; what is left,
/// ~10 ms an item, is the Recycle Bin's own work). `docs/PERFORMANCE.md`,
/// passada 3.
/// Where a ledger has to be written, each item is captured and recorded on its
/// own, as [`trash_path`] does.
pub fn trash_paths(paths: &[PathBuf], ledger: Option<&dyn TrashLedger>) -> Vec<(PathBuf, Error)> {
    trash_paths_reported(paths, ledger, None)
}

/// [`trash_paths`] while counting items for `progress` and stopping between
/// chunks when the user cancels. The batch call is kept for speed and done in
/// chunks, so a long empty-the-selection can be stopped without falling back to
/// one call per file.
pub fn trash_paths_reported(
    paths: &[PathBuf],
    ledger: Option<&dyn TrashLedger>,
    progress: Option<&TransferProgress>,
) -> Vec<(PathBuf, Error)> {
    // One shell call per chunk: large enough that the per-call cost disappears,
    // small enough that Cancel is answered within a second or two.
    const CHUNK: usize = 64;
    let mut failed = Vec::new();
    for chunk in paths.chunks(CHUNK) {
        if transfer::check(progress).is_err() {
            break;
        }
        if let Some(progress) = progress {
            if let Some(first) = chunk.first().and_then(|path| path.file_name()) {
                progress.set_detail(first.to_string_lossy().into_owned());
            }
        }
        match ledger {
            Some(ledger) => {
                for path in chunk {
                    if transfer::check(progress).is_err() {
                        break;
                    }
                    if let Err(error) = trash_path(path, Some(ledger)) {
                        failed.push((path.clone(), error));
                    }
                }
            }
            None => failed.extend(trash_in_one_batch(
                chunk,
                |path| fs::symlink_metadata(path).is_ok(),
                os_trash_delete_all,
                os_trash_delete,
            )),
        }
        if let Some(progress) = progress {
            progress.items_added(chunk.len());
        }
    }
    failed
}

// The plan behind [`trash_paths`] without a ledger, apart from the system calls
// so a test can count them.
//
// The batch is all-or-nothing in what it reports, not in what it does: the
// `trash` crate gives up before starting if any path cannot be opened, and if
// one item fails midway the others still go and all it says is "some
// operations were aborted". So a path already missing never enters the batch
// (it would sink the rest), and after a failed batch whatever is still on disk
// is tried alone — which is what names the file that failed and says why, as
// the item-by-item delete always did. What the batch already took is gone and
// counts as done.
fn trash_in_one_batch(
    paths: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
    trash_all: impl FnOnce(&[PathBuf]) -> Result<()>,
    trash_one: impl Fn(&Path) -> Result<()>,
) -> Vec<(PathBuf, Error)> {
    let (present, missing): (Vec<PathBuf>, Vec<PathBuf>) =
        paths.iter().cloned().partition(|path| exists(path));
    let mut failed = Vec::new();
    // Tried anyway rather than reported here, so the message is the system's
    // own, as it was before the batch.
    for path in missing {
        if let Err(error) = trash_one(&path) {
            failed.push((path, error));
        }
    }
    if present.is_empty() {
        return failed;
    }
    if let Err(batch_error) = trash_all(&present) {
        tracing::debug!(%batch_error, items = present.len(), "the batch trash failed; retrying what is left one by one");
        for path in present {
            if !exists(&path) {
                continue;
            }
            if let Err(error) = trash_one(&path) {
                failed.push((path, error));
            }
        }
    }
    failed
}

// On Windows the `trash` crate joins COM as a single-threaded apartment on the
// calling thread and *panics* if that thread already sits in the other kind.
// The UI calls this from gpui's executor, which runs on the Windows thread pool:
// a pool thread that some earlier work left in the multithreaded apartment (the
// PDF preview did) turned a plain Delete into a panic, and a panic there cannot
// unwind out of the pool callback, so it aborted the whole app (04/11, D:\).
// A thread of its own starts with no apartment at all, and joining it turns
// any panic left in the crate into an ordinary error.
#[cfg(windows)]
fn on_own_thread(work: impl FnOnce() -> std::result::Result<(), ::trash::Error> + Send) -> Result<()> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("nohrs-trash".into())
            .spawn_scoped(scope, work)
            .map_err(|error| Error::Other(format!("failed to move to trash: {error}")))?
            .join()
            .map_err(|_| Error::Other("failed to move to trash: the system trash call crashed".into()))?
            .map_err(|error| Error::Other(format!("failed to move to trash: {error}")))
    })
}

#[cfg(not(windows))]
fn on_own_thread(work: impl FnOnce() -> std::result::Result<(), ::trash::Error>) -> Result<()> {
    work().map_err(|error| Error::Other(format!("failed to move to trash: {error}")))
}

fn os_trash_delete(path: &Path) -> Result<()> {
    on_own_thread(|| ::trash::delete(path))
}

fn os_trash_delete_all(paths: &[PathBuf]) -> Result<()> {
    on_own_thread(|| ::trash::delete_all(paths))
}

fn record_trashed(ledger: &dyn TrashLedger, captured: &nohrs_store::TrashEntry) -> Result<()> {
    ledger
        .append(captured)
        .map(|_| ())
        .map_err(|error| Error::Other(format!("could not write the trash ledger: {error}")))
}

/// Permanently deletes `path`, whether it is a file, symlink, or directory
/// tree. This cannot be undone.
#[tracing::instrument(target = "nohrs::op", name = "fs.delete", level = "debug", skip_all, fields(path = %path.display()))]
pub fn delete_permanent(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// [`delete_permanent`] while counting the files it removes and stopping with
/// [`Error::Cancelled`] when the user cancels.
///
/// A cancelled tree delete leaves part of the tree in place: what was deleted is
/// deleted, and the rest is reported by the caller. That is the only honest
/// outcome — the entries are gone the moment each `remove` returns.
pub fn delete_permanent_reported(
    path: &Path,
    progress: &TransferProgress,
) -> Result<()> {
    transfer::check(Some(progress))?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        delete_tree_reported(path, progress)
    } else {
        note_deleting(progress, path);
        remove_file_clearing_readonly(path)?;
        progress.file_done();
        Ok(())
    }
}

fn delete_tree_reported(dir: &Path, progress: &TransferProgress) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        // Between entries, so a large tree can be stopped partway.
        transfer::check(Some(progress))?;
        let entry = entry?;
        let path = entry.path();
        note_deleting(progress, &path);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            delete_tree_reported(&path, progress)?;
        } else {
            remove_file_clearing_readonly(&path)?;
            progress.file_done();
        }
    }
    fs::remove_dir(dir)?;
    Ok(())
}

fn note_deleting(progress: &TransferProgress, path: &Path) {
    if let Some(name) = path.file_name() {
        progress.set_detail(name.to_string_lossy().into_owned());
    }
}

// Removes a file, clearing a read-only attribute first when that is what stops
// it: Explorer deletes a read-only file after asking, and a cancelled permanent
// delete should not halt on one.
fn remove_file_clearing_readonly(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            if let Ok(metadata) = fs::symlink_metadata(path) {
                let mut permissions = metadata.permissions();
                if permissions.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(false);
                    fs::set_permissions(path, permissions)?;
                }
            }
            Ok(fs::remove_file(path)?)
        }
        other => Ok(other?),
    }
}

/// Bytes in `paths` and everything under them, for a copy dialog's bar. Symbolic
/// links are skipped, as a copy never follows them. Fails with
/// [`Error::Cancelled`] if the measuring itself is cancelled, before anything
/// is written.
pub fn bytes_to_copy(paths: &[PathBuf], progress: Option<&TransferProgress>) -> Result<u64> {
    let mut total = 0_u64;
    for path in paths {
        transfer::check(progress)?;
        let Ok(metadata) = fs::symlink_metadata(path) else {
            continue;
        };
        if metadata.is_file() {
            total += metadata.len();
        } else if metadata.is_dir() {
            total += tree_bytes(path, progress)?;
        }
    }
    Ok(total)
}

fn tree_bytes(dir: &Path, progress: Option<&TransferProgress>) -> Result<u64> {
    let mut total = 0_u64;
    for entry in fs::read_dir(dir)? {
        transfer::check(progress)?;
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            total += tree_bytes(&entry.path(), progress)?;
        } else if let Ok(metadata) = entry.metadata() {
            total += metadata.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn unique_name_returns_input_when_no_collision() {
        let dir = tempdir().unwrap();
        assert_eq!(unique_name(dir.path(), "report.pdf"), "report.pdf");
    }

    #[test]
    fn unique_name_inserts_counter_before_extension() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("report.pdf"), "a").unwrap();
        assert_eq!(unique_name(dir.path(), "report.pdf"), "report (2).pdf");

        fs::write(dir.path().join("report (2).pdf"), "b").unwrap();
        assert_eq!(unique_name(dir.path(), "report.pdf"), "report (3).pdf");
    }

    #[test]
    fn unique_name_handles_extensionless_names() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("folder")).unwrap();
        assert_eq!(unique_name(dir.path(), "folder"), "folder (2)");
    }

    #[test]
    fn would_conflict_reflects_existence() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("x");
        assert!(!would_conflict(&target));
        fs::write(&target, "x").unwrap();
        assert!(would_conflict(&target));
    }

    #[test]
    fn copy_path_copies_a_file() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("b.txt");
        fs::write(&src, "hello").unwrap();
        copy_path(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(&dst).unwrap(), "hello");
        assert!(src.exists(), "source is preserved on copy");
    }

    #[test]
    fn a_reported_copy_counts_bytes_and_files() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("pasta");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), "12345").unwrap();
        fs::write(src.join("sub").join("b.txt"), "1234567890").unwrap();
        let dst = dir.path().join("copia");
        let progress = TransferProgress::new(1);
        let total = bytes_to_copy(&[src.clone()], Some(&progress)).unwrap();
        assert_eq!(total, 15);
        progress.set_total(total);
        copy_path_reported(&src, &dst, &progress).unwrap();
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.done, 15);
        assert_eq!(snapshot.files_done, 2);
        assert_eq!(
            fs::read_to_string(dst.join("sub").join("b.txt")).unwrap(),
            "1234567890"
        );
    }

    #[test]
    fn a_cancelled_copy_stops_before_writing_anything() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("big.txt");
        fs::write(&src, vec![7_u8; 1024 * 1024]).unwrap();
        let dst = dir.path().join("copia.txt");
        let progress = TransferProgress::new(1);
        progress.cancel();
        assert!(matches!(
            bytes_to_copy(&[src.clone()], Some(&progress)),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            copy_path_reported(&src, &dst, &progress),
            Err(Error::Cancelled)
        ));
        assert!(!dst.exists(), "a cancelled copy leaves no destination");
    }

    #[test]
    fn a_reported_delete_counts_files_and_stops_when_cancelled() {
        let dir = tempdir().unwrap();
        let tree = dir.path().join("arvore");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("one.txt"), "a").unwrap();
        fs::write(tree.join("sub").join("two.txt"), "b").unwrap();
        let progress = TransferProgress::new(1);
        delete_permanent_reported(&tree, &progress).unwrap();
        assert_eq!(progress.snapshot().files_done, 2);
        assert!(!tree.exists());

        let cancelled = TransferProgress::new(1);
        cancelled.cancel();
        let kept = dir.path().join("kept.txt");
        fs::write(&kept, "k").unwrap();
        assert!(matches!(
            delete_permanent_reported(&kept, &cancelled),
            Err(Error::Cancelled)
        ));
        assert!(kept.exists(), "a cancelled delete leaves the entry");
    }

    #[test]
    fn copy_path_copies_a_directory_tree() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("top.txt"), "top").unwrap();
        fs::create_dir(src.join("nested")).unwrap();
        fs::write(src.join("nested").join("deep.txt"), "deep").unwrap();

        let dst = dir.path().join("dst");
        copy_path(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("top.txt")).unwrap(), "top");
        assert_eq!(
            fs::read_to_string(dst.join("nested").join("deep.txt")).unwrap(),
            "deep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copying_a_tree_gets_past_a_link_to_a_directory() {
        // The link is not a directory, so a copy that followed it handed it to
        // `fs::copy`, which read the directory behind it and failed with
        // `EISDIR` — dragging an ordinary folder was enough to hit this.
        let dir = tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("kept.txt"), "kept").unwrap();

        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("plain.txt"), "plain").unwrap();
        std::os::unix::fs::symlink(&elsewhere, src.join("link-to-dir")).unwrap();

        let dst = dir.path().join("dst");
        copy_path(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("plain.txt")).unwrap(), "plain");
        let copied_link = dst.join("link-to-dir");
        assert!(fs::symlink_metadata(&copied_link).unwrap().is_symlink());
        assert_eq!(fs::read_link(&copied_link).unwrap(), elsewhere);
        // Recreated rather than walked into: the copy holds a link, not a second
        // copy of the directory behind it.
        assert!(!fs::symlink_metadata(&copied_link).unwrap().is_dir());
        assert!(elsewhere.join("kept.txt").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_pointing_out_of_the_tree_is_not_followed_out_of_it() {
        // Nothing outside the source is read or written, so a link to an
        // ancestor cannot recurse and one to another volume cannot drag it in.
        let dir = tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "not mine to copy").unwrap();

        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        std::os::unix::fs::symlink(&outside, src.join("escape")).unwrap();
        std::os::unix::fs::symlink(dir.path(), src.join("up")).unwrap();

        let dst = dir.path().join("dst");
        copy_path(&src, &dst).unwrap();

        assert!(
            fs::symlink_metadata(dst.join("escape"))
                .unwrap()
                .is_symlink()
        );
        assert!(fs::symlink_metadata(dst.join("up")).unwrap().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn copying_a_link_into_the_directory_it_points_at_is_allowed() {
        // The containment guard must not mistake this for copying something
        // into itself: only the link is copied, never what it points at.
        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let dst = target.join("link");
        copy_path(&link, &dst).unwrap();
        assert!(fs::symlink_metadata(&dst).unwrap().is_symlink());
    }

    #[test]
    fn copying_or_moving_a_directory_into_itself_is_refused() {
        // Without this the recursion walks into the copy it is writing and only
        // stops when the volume is full.
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("nested").join("deep.txt"), "deep").unwrap();

        for dst in [
            src.join("copy-of-src"),
            src.join("nested").join("copy-of-src"),
            // `..` and a `.` must not be able to dress the same path up as a
            // different one.
            src.join("nested").join("..").join("copy-of-src"),
            src.clone(),
        ] {
            assert!(copy_path(&src, &dst).is_err(), "copy allowed {dst:?}");
            assert!(move_path(&src, &dst).is_err(), "move allowed {dst:?}");
            assert!(
                move_path_no_replace(&src, &dst).is_err(),
                "no-replace move allowed {dst:?}"
            );
        }
        assert_eq!(
            fs::read_to_string(src.join("nested").join("deep.txt")).unwrap(),
            "deep",
            "a refused operation leaves the source alone"
        );

        // A sibling destination is still fine.
        copy_path(&src, &dir.path().join("beside")).unwrap();
    }

    #[test]
    fn a_trailing_dot_dot_cannot_dress_a_source_up_as_something_else() {
        // `dir/src/..` is `dir`, so this is copying `dir` into its own child.
        // `Path::file_name` is `None` for it, which is what made the guard leave
        // it unresolved and compare it as something *below* `dir/src`.
        let dir = tempdir().unwrap();
        let inner = dir.path().join("src");
        fs::create_dir(&inner).unwrap();
        fs::write(inner.join("deep.txt"), "deep").unwrap();

        let disguised = inner.join("..");
        let dst = inner.join("copy-of-dir");
        assert!(copy_path(&disguised, &dst).is_err());
        assert!(move_path(&disguised, &dst).is_err());
        assert!(move_path_no_replace(&disguised, &dst).is_err());
        assert!(!dst.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_link_pointing_back_into_the_source_is_refused() {
        // The destination's own final component is followed, unlike the
        // source's: `create_dir_all` and `fs::copy` follow it, so a link back
        // into the source is a write into the source however unrelated the path
        // it is spelled with looks. Left unchecked this copied a directory into
        // itself, and copied a file onto itself.
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("kept.txt"), "kept").unwrap();

        let link_to_source = dir.path().join("link-to-source");
        std::os::unix::fs::symlink(&source, &link_to_source).unwrap();
        assert!(copy_path(&source, &link_to_source).is_err());

        let file = dir.path().join("notes.txt");
        fs::write(&file, "payload").unwrap();
        let link_to_file = dir.path().join("link-to-notes");
        std::os::unix::fs::symlink(&file, &link_to_file).unwrap();
        assert!(copy_path(&file, &link_to_file).is_err());

        assert_eq!(fs::read_to_string(&file).unwrap(), "payload");
        assert_eq!(fs::read_to_string(source.join("kept.txt")).unwrap(), "kept");
    }

    #[cfg(unix)]
    #[test]
    fn one_link_can_be_copied_over_another_to_the_same_target() {
        // The identity check is about what the copy *reads*. A symbolic link
        // source is recreated, never opened, so a second link to the same target
        // is a different entry and replacing it is an ordinary copy — refusing
        // this would be the check firing on a safe operation.
        let dir = tempdir().unwrap();
        let target = dir.path().join("target.txt");
        fs::write(&target, "pointed at").unwrap();
        let one = dir.path().join("one");
        let other = dir.path().join("other");
        std::os::unix::fs::symlink(&target, &one).unwrap();
        std::os::unix::fs::symlink(&target, &other).unwrap();

        copy_path(&one, &other).unwrap();
        assert!(fs::symlink_metadata(&other).unwrap().is_symlink());
        assert_eq!(fs::read_link(&other).unwrap(), target);

        // The link onto itself is refused, like any other entry onto itself,
        // even though the read resolves to the target and the write lands on
        // the link — two different inodes.
        assert!(copy_path(&one, &one).is_err());
        assert!(move_path(&one, &one).is_err());

        // Onto the target itself is still refused: recreating the link there
        // would remove the file it points at and leave a link to nothing.
        assert!(copy_path(&one, &target).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "pointed at");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_name_for_the_same_file_is_refused() {
        // A hard link is a different path and the same inode. A replacing copy
        // opens the destination truncating, so the source is emptied before a
        // byte of it is read — the same loss as copying a file onto itself, with
        // nothing in the two paths to hint at it.
        let dir = tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        fs::write(&file, "payload").unwrap();
        let hard_link = dir.path().join("also-notes.txt");
        fs::hard_link(&file, &hard_link).unwrap();

        assert!(copy_path(&file, &hard_link).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "payload");
    }

    #[test]
    fn the_no_replace_copy_refuses_a_destination_it_did_not_create() {
        // Every entry is claimed, not just the root — a tree takes time to
        // write, and an entry appearing inside it meanwhile used to be replaced
        // by `fs::copy` / `create_dir_all` on the way past. A root that is
        // already there is as far as this gets: `create_dir` refuses it, so
        // nothing below is written and nothing there is this copy's.
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested").join("deep.txt"), "mine").unwrap();

        let dst = dir.path().join("dst");
        fs::create_dir_all(dst.join("nested")).unwrap();
        fs::write(dst.join("nested").join("deep.txt"), "theirs").unwrap();

        let failure = copy_path_no_replace(&source, &dst).unwrap_err();
        assert_eq!(
            failure.leftover(),
            Leftover::Nothing,
            "a destination this copy never created is not its to remove"
        );
        assert_eq!(
            fs::read_to_string(dst.join("nested").join("deep.txt")).unwrap(),
            "theirs",
            "what was already there has to survive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_no_replace_copy_says_which_side_of_the_claim_it_failed_on() {
        // What a caller rolling back needs and the error cannot give it: both
        // failures below are a bare IO error with no path on them, and the one
        // this test has to distinguish — a name taken *inside* a tree this copy
        // created — is the very same `AlreadyExists` as a destination that was
        // taken before it started.
        //
        // That nested collision needs a second process writing into the tree
        // mid-copy, so it is not reproducible here. What is reproducible is the
        // thing that decides it: the claim is set by *where* the failure fell,
        // not by what the error says. A socket cannot be opened as a file, which
        // stops a copy at a known point past the destination's creation — the
        // same side of the claim the nested collision falls on.
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("page.txt"), "mine").unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(source.join("ipc")).unwrap();

        let dst = dir.path().join("dst");
        let failure = copy_path_no_replace(&source, &dst).unwrap_err();
        assert_eq!(
            failure.leftover(),
            Leftover::PartialDestination,
            "a copy that got past creating the destination owns what is there"
        );
        assert!(
            dst.is_dir(),
            "and what is there is the partial tree the caller clears away"
        );

        failure.discard_partial_destination(&dst).unwrap();
        assert!(!dst.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_swapped_under_the_copy_is_not_its_to_remove() {
        // A tree takes time to write, and the name can be taken back in that
        // time: another process puts its own entry where this call's partial
        // was. Deleting by path alone would take theirs — the identity read at
        // the claim is what keeps the cleanup on its own work.
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("page.txt"), "mine").unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(source.join("ipc")).unwrap();

        let dst = dir.path().join("dst");
        let failure = copy_path_no_replace(&source, &dst).unwrap_err();
        assert_eq!(failure.leftover(), Leftover::PartialDestination);

        // Stand in for that other process, building its directory *before* the
        // partial goes and renaming it into place. Not incidental: inode numbers
        // are reused, so a stand-in that removed the partial first and created a
        // directory at the same name could be handed the very number this copy
        // claimed — which is the hole the doc comment on
        // `discard_partial_destination` owns up to, and would make this test
        // pass or fail on the filesystem's mood.
        let theirs = dir.path().join("theirs");
        fs::create_dir(&theirs).unwrap();
        fs::write(theirs.join("theirs.txt"), "theirs").unwrap();
        fs::remove_dir_all(&dst).unwrap();
        fs::rename(&theirs, &dst).unwrap();

        failure.discard_partial_destination(&dst).unwrap();

        assert_eq!(
            fs::read_to_string(dst.join("theirs.txt")).unwrap(),
            "theirs",
            "the cleanup removed an entry it never created"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_cleanup_clears_a_nested_tree_without_following_a_link_out_of_it() {
        // The removal walks the tree itself rather than handing a path to
        // `remove_dir_all`, so what it does at each entry is this module's to
        // get right: descend into a directory, unlink anything else — and a
        // symbolic link is anything else. Following one would take a directory
        // the copy never wrote and the user never named.
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(source.join("ipc")).unwrap();

        let dst = dir.path().join("dst");
        let failure = copy_path_no_replace(&source, &dst).unwrap_err();
        assert_eq!(failure.leftover(), Leftover::PartialDestination);

        // Filling the partial out afterwards rather than arranging for the copy
        // to write it: what a failed copy leaves depends on the order
        // `read_dir` hands back, and the shape being tested here has to be the
        // same every run. Adding entries under `dst` leaves `dst` itself the
        // entry that was claimed.
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "keep").unwrap();
        fs::create_dir_all(dst.join("a/b/c")).unwrap();
        fs::write(dst.join("a/b/c/deep.txt"), "deep").unwrap();
        std::os::unix::fs::symlink(&outside, dst.join("a").join("link")).unwrap();

        failure.discard_partial_destination(&dst).unwrap();

        assert!(!dst.exists(), "the tree goes, however deep it runs");
        assert_eq!(
            fs::read_to_string(outside.join("keep.txt")).unwrap(),
            "keep",
            "the link was unlinked, not walked through"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_left_where_the_claim_was_is_not_the_claim() {
        // `O_NOFOLLOW` is what decides this. Without it the open would succeed
        // on the directory at the other end of the link, and the only thing
        // still standing between that directory and the removal would be its
        // identity.
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(source.join("ipc")).unwrap();

        let dst = dir.path().join("dst");
        let failure = copy_path_no_replace(&source, &dst).unwrap_err();
        assert_eq!(failure.leftover(), Leftover::PartialDestination);

        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("theirs.txt"), "theirs").unwrap();
        fs::remove_dir_all(&dst).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &dst).unwrap();

        failure.discard_partial_destination(&dst).unwrap();

        assert!(
            dst.symlink_metadata().is_ok(),
            "the link was never this copy's to remove"
        );
        assert_eq!(
            fs::read_to_string(elsewhere.join("theirs.txt")).unwrap(),
            "theirs",
            "and neither was what it points at"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_cleanup_removes_a_claimed_file_and_leaves_one_that_replaced_it() {
        // The file half of the removal, which a failed copy cannot be made to
        // produce on demand — `fill_file` fails on a read or a `chmod` of
        // entries this call itself just created, neither of which a test can
        // arrange — so it is driven directly. `claim_file` is the same call the
        // copy claims a destination with, and the identity comes off the handle
        // it returns, exactly as in `copy_path_no_replace`.
        let dir = tempdir().unwrap();
        let mine = dir.path().join("note.txt");
        let claimed = handle_identity(&claim_file(&mine).unwrap());
        assert!(claimed.is_some(), "the claim read its own handle back");
        discard_claimed(&mine, claimed).unwrap();
        assert!(!mine.exists());

        // The other side of it. Built elsewhere and renamed into place, rather
        // than created at the name after the first one goes: inode numbers are
        // reused, and a replacement handed the number this claim recorded would
        // make the assertion below depend on the filesystem's mood.
        let claimed = handle_identity(&claim_file(&mine).unwrap());
        let theirs = dir.path().join("theirs.txt");
        fs::write(&theirs, "theirs").unwrap();
        fs::remove_file(&mine).unwrap();
        fs::rename(&theirs, &mine).unwrap();

        discard_claimed(&mine, claimed).unwrap();

        assert_eq!(
            fs::read_to_string(&mine).unwrap(),
            "theirs",
            "the removal stopped at an entry it did not claim"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_named_through_a_link_is_still_cleared() {
        // `O_NOFOLLOW` belongs on the entry, not on the directory holding it,
        // which is why the parent is opened without it. Navigating into a
        // symbolically linked folder is an ordinary thing to do, and refusing
        // the open there would abandon the partial rather than clear it — the
        // regression this whole mechanism exists to avoid, traded for nothing:
        // following the link endangers nothing, because the identity check only
        // passes when the directory reached really does hold the claimed inode.
        let dir = tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(source.join("ipc")).unwrap();

        // Named through the link, so the parent this opens is `link`, not `real`.
        let dst = link.join("dst");
        let failure = copy_path_no_replace(&source, &dst).unwrap_err();
        assert_eq!(failure.leftover(), Leftover::PartialDestination);
        assert!(real.join("dst").is_dir(), "the partial landed in `real`");

        failure.discard_partial_destination(&dst).unwrap();

        assert!(
            !real.join("dst").exists(),
            "and was cleared through the link"
        );
        assert!(link.is_symlink(), "the link itself is not the target");
    }

    #[cfg(unix)]
    #[test]
    fn a_removal_that_lost_the_race_is_not_a_failure() {
        // The race itself cannot be staged: producing `ENOENT` at the `unlinkat`
        // needs another process to remove the entry in the two syscalls since
        // this call opened it, and every test above deliberately runs alone. But
        // what to do when it happens is a decision, not a race, so the decision
        // is pinned here rather than left to the one branch no test reaches.
        assert!(already_gone_is_fine(Ok(())).is_ok());
        assert!(
            already_gone_is_fine(Err(rustix::io::Errno::NOENT)).is_ok(),
            "gone is the outcome this call wanted; something else got there first"
        );
        // And `ENOTEMPTY` is not the same shape of news: an entry created inside
        // a directory after this call emptied it means the partial is still
        // standing, and reporting that as done would be untrue.
        let error = already_gone_is_fine(Err(rustix::io::Errno::NOTEMPTY)).unwrap_err();
        assert!(matches!(error, Error::Io(_)), "got: {error:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_claim_that_never_read_its_identity_removes_nothing() {
        // `None` is not "no check to run", it is "the check could not be made".
        // Failing to read back what was just created is itself a sign something
        // reached the destination in between, so it permits nothing.
        let dir = tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "mine").unwrap();
        discard_claimed(&path, None).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn copying_a_file_onto_itself_is_refused() {
        // `fs::copy` opens the destination truncating, so with the same path on
        // both sides the contents are gone before a byte is read.
        let dir = tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        fs::write(&file, "payload").unwrap();

        assert!(copy_path(&file, &file).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "payload");
    }

    #[test]
    fn destination_in_rejects_anything_but_a_plain_name() {
        let dir = tempdir().unwrap();
        for bad in ["../escape", "a/b", ".", "..", "", "/etc"] {
            assert!(
                destination_in(dir.path(), OsStr::new(bad)).is_err(),
                "allowed {bad:?}"
            );
        }
        assert_eq!(
            destination_in(dir.path(), OsStr::new("report.pdf")).unwrap(),
            dir.path().join("report.pdf")
        );
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_link_permanently_leaves_its_target_alone() {
        // `remove_dir_all` on a link to a directory would empty the directory it
        // points at; keying off `symlink_metadata` is what stops that.
        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("target-dir");
        fs::create_dir(&target_dir).unwrap();
        fs::write(target_dir.join("inside.txt"), "inside").unwrap();
        let target_file = dir.path().join("target.txt");
        fs::write(&target_file, "pointed at").unwrap();

        let dir_link = dir.path().join("dir-link");
        std::os::unix::fs::symlink(&target_dir, &dir_link).unwrap();
        let file_link = dir.path().join("file-link");
        std::os::unix::fs::symlink(&target_file, &file_link).unwrap();

        delete_permanent(&dir_link).unwrap();
        delete_permanent(&file_link).unwrap();

        assert!(fs::symlink_metadata(&dir_link).is_err());
        assert!(fs::symlink_metadata(&file_link).is_err());
        assert_eq!(
            fs::read_to_string(target_dir.join("inside.txt")).unwrap(),
            "inside"
        );
        assert_eq!(fs::read_to_string(&target_file).unwrap(), "pointed at");
    }

    #[test]
    fn move_path_within_volume_renames_and_removes_source() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("sub").join("a.txt");
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(&src, "payload").unwrap();

        let kind = move_path(&src, &dst).unwrap();
        assert_eq!(kind, MoveKind::Rename);
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "payload");
    }

    #[test]
    fn move_path_no_replace_moves_when_the_destination_is_free() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("b.txt");
        fs::write(&src, "payload").unwrap();

        // Which of the two it takes depends on whether this filesystem accepts
        // the no-replace flag, so only the outcome is asserted.
        move_path_no_replace(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "payload");
    }

    #[cfg(unix)]
    #[test]
    fn the_fallback_move_keeps_a_symlink_a_symlink_and_a_mode_a_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let target = dir.path().join("target.txt");
        fs::write(&target, "pointed at").unwrap();

        // A copy that followed the link would restore the target's bytes and
        // leave the link behind as a plain file.
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let moved_link = dir.path().join("moved-link.txt");
        copy_path_no_replace(&link, &moved_link).unwrap();
        assert!(fs::symlink_metadata(&moved_link).unwrap().is_symlink());
        assert_eq!(fs::read_link(&moved_link).unwrap(), target);

        // And the same for one nested inside a moved directory.
        let tree = dir.path().join("tree");
        fs::create_dir(&tree).unwrap();
        std::os::unix::fs::symlink(&target, tree.join("inner-link.txt")).unwrap();
        let moved_tree = dir.path().join("moved-tree");
        copy_path_no_replace(&tree, &moved_tree).unwrap();
        let inner = moved_tree.join("inner-link.txt");
        assert!(fs::symlink_metadata(&inner).unwrap().is_symlink());
        assert_eq!(fs::read_link(&inner).unwrap(), target);

        let script = dir.path().join("script.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let moved_script = dir.path().join("moved-script.sh");
        copy_path_no_replace(&script, &moved_script).unwrap();
        assert_eq!(
            fs::metadata(&moved_script).unwrap().permissions().mode() & 0o777,
            0o755,
            "an executable must not come back from the trash unexecutable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_fallback_move_keeps_a_private_directory_private_at_every_depth() {
        use std::os::unix::fs::PermissionsExt;

        // `create_dir_all` takes its mode from the umask, so without carrying
        // the source's across, a restored `0700` directory would come back
        // readable by anyone with an account on the machine.
        let dir = tempdir().unwrap();
        let outer = dir.path().join("secrets");
        let inner = outer.join("deeper");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("key.txt"), "private").unwrap();
        fs::set_permissions(&inner, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&outer, fs::Permissions::from_mode(0o750)).unwrap();

        let moved = dir.path().join("moved-secrets");
        copy_path_no_replace(&outer, &moved).unwrap();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&moved), 0o750);
        assert_eq!(mode(&moved.join("deeper")), 0o700);
        assert_eq!(
            fs::read_to_string(moved.join("deeper").join("key.txt")).unwrap(),
            "private",
            "the contents still have to arrive, mode applied last"
        );
    }

    #[test]
    fn move_path_no_replace_refuses_an_occupied_destination() {
        // The difference from `move_path` that restoring depends on: whatever
        // is already there survives, and the source is still where it was.
        let dir = tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("b.txt");
        fs::write(&src, "restored").unwrap();
        fs::write(&dst, "newer").unwrap();

        let failure = move_path_no_replace(&src, &dst).unwrap_err();

        assert_eq!(
            failure.leftover(),
            Leftover::Nothing,
            "the move never got the name, so what is there is not its to remove"
        );
        let error = failure.into_error();
        assert!(
            matches!(&error, Error::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&dst).unwrap(), "newer");
        assert_eq!(fs::read_to_string(&src).unwrap(), "restored");
    }

    #[test]
    fn move_path_no_replace_refuses_an_occupied_directory_destination() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("inside.txt"), "mine").unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("theirs.txt"), "theirs").unwrap();

        assert!(move_path_no_replace(&src, &dst).is_err());

        assert!(dst.join("theirs.txt").is_file());
        assert!(!dst.join("inside.txt").exists());
        assert!(src.join("inside.txt").is_file());
    }

    // The batch plan with the system calls replaced by counters: `on_disk` is
    // what exists, the batch takes all of it unless told to fail on a name,
    // and the single delete fails on the names in `stuck`.
    struct FakeTrash {
        on_disk: std::cell::RefCell<std::collections::HashSet<PathBuf>>,
        batch_calls: std::cell::Cell<usize>,
        single_calls: std::cell::RefCell<Vec<PathBuf>>,
    }

    impl FakeTrash {
        fn new(names: &[&str]) -> Self {
            Self {
                on_disk: std::cell::RefCell::new(names.iter().map(PathBuf::from).collect()),
                batch_calls: Default::default(),
                single_calls: Default::default(),
            }
        }

        fn run(&self, asked: &[&str], batch_fails_on: Option<&str>, stuck: &[&str]) -> Vec<PathBuf> {
            let asked: Vec<PathBuf> = asked.iter().map(PathBuf::from).collect();
            trash_in_one_batch(
                &asked,
                |path| self.on_disk.borrow().contains(path),
                |paths| {
                    self.batch_calls.set(self.batch_calls.get() + 1);
                    // Like IFileOperation: the others still go, and the
                    // failure does not say which one it was.
                    for path in paths {
                        if Some(path.as_path()) != batch_fails_on.map(Path::new) {
                            self.on_disk.borrow_mut().remove(path);
                        }
                    }
                    match batch_fails_on {
                        Some(_) => Err(Error::Other("some operations were aborted".into())),
                        None => Ok(()),
                    }
                },
                |path| {
                    self.single_calls.borrow_mut().push(path.to_path_buf());
                    if stuck.iter().any(|name| Path::new(name) == path) || !self.on_disk.borrow().contains(path) {
                        return Err(Error::Other(format!("cannot trash {}", path.display())));
                    }
                    self.on_disk.borrow_mut().remove(path);
                    Ok(())
                },
            )
            .into_iter()
            .map(|(path, _)| path)
            .collect()
        }
    }

    #[test]
    fn a_selection_goes_to_the_trash_in_one_call() {
        let trash = FakeTrash::new(&["a", "b", "c", "d"]);

        let failed = trash.run(&["a", "b", "c", "d"], None, &[]);

        assert!(failed.is_empty());
        assert_eq!(trash.batch_calls.get(), 1, "one system call for the whole selection");
        assert!(trash.single_calls.borrow().is_empty(), "{:?}", trash.single_calls.borrow());
        assert!(trash.on_disk.borrow().is_empty());
    }

    #[test]
    fn a_failed_batch_retries_only_what_is_left_and_names_the_file_that_failed() {
        let trash = FakeTrash::new(&["a", "b", "in use", "d"]);

        let failed = trash.run(&["a", "b", "in use", "d"], Some("in use"), &["in use"]);

        assert_eq!(failed, [PathBuf::from("in use")]);
        assert_eq!(
            *trash.single_calls.borrow(),
            [PathBuf::from("in use")],
            "what the batch already took is not tried again"
        );
    }

    #[test]
    fn a_path_already_gone_stays_out_of_the_batch_and_is_still_reported() {
        // The crate refuses the whole batch when one path cannot be opened; let
        // in, a file deleted by another program would keep the rest from going.
        let trash = FakeTrash::new(&["a", "b"]);

        let failed = trash.run(&["a", "gone", "b"], None, &[]);

        assert_eq!(failed, [PathBuf::from("gone")]);
        assert_eq!(trash.batch_calls.get(), 1);
        assert!(trash.on_disk.borrow().is_empty());
    }

    #[test]
    fn nothing_on_disk_makes_no_batch_call() {
        let trash = FakeTrash::new(&[]);

        let failed = trash.run(&["gone"], None, &[]);

        assert_eq!(failed, [PathBuf::from("gone")]);
        assert_eq!(trash.batch_calls.get(), 0);
    }

    #[test]
    fn trash_paths_moves_real_files_and_reports_the_missing_one() {
        let dir = tempdir().unwrap();
        let files: Vec<PathBuf> = (0..3).map(|n| dir.path().join(format!("lixo {n}.txt"))).collect();
        for file in &files {
            fs::write(file, "x").unwrap();
        }
        let folder = dir.path().join("pasta");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("dentro.txt"), "x").unwrap();
        let missing = dir.path().join("nunca existiu.txt");
        let mut asked = files.clone();
        asked.push(folder.clone());
        asked.push(missing.clone());

        let failed = trash_paths(&asked, None);

        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].0, missing);
        for path in files.iter().chain([&folder]) {
            assert!(!path.exists(), "{} is still there", path.display());
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_pasted_copy_keeps_the_modification_time() {
        // Explorer's copy keeps it, and so did `fs::copy`; the no-replace copy
        // wrote the file anew with `std::io::copy` and stamped it with the time
        // of the paste, so a folder of photos pasted elsewhere lost every date.
        let dir = tempdir().unwrap();
        let long_ago = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        let file = dir.path().join("foto.jpg");
        fs::write(&file, "payload").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(long_ago).unwrap();
        let tree = dir.path().join("album");
        fs::create_dir(&tree).unwrap();
        fs::copy(&file, tree.join("dentro.jpg")).unwrap();

        copy_path_no_replace(&file, &dir.path().join("copia.jpg")).unwrap();
        copy_path_no_replace(&tree, &dir.path().join("album copia")).unwrap();

        let modified = |path: PathBuf| fs::metadata(path).unwrap().modified().unwrap();
        assert_eq!(modified(dir.path().join("copia.jpg")), long_ago);
        assert_eq!(modified(dir.path().join("album copia").join("dentro.jpg")), long_ago);
        assert_eq!(fs::read_to_string(dir.path().join("copia.jpg")).unwrap(), "payload");
    }

    #[cfg(windows)]
    #[test]
    fn a_pasted_copy_keeps_a_read_only_file_read_only() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("contrato.pdf");
        fs::write(&file, "x").unwrap();
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).unwrap();
        let copy = dir.path().join("copia.pdf");

        copy_path_no_replace(&file, &copy).unwrap();

        assert!(fs::metadata(&copy).unwrap().permissions().readonly());
        // So the temp folder can be removed.
        for path in [&file, &copy] {
            let mut permissions = fs::metadata(path).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_source_another_program_holds_leaves_nothing_at_the_destination() {
        use std::os::windows::fs::OpenOptionsExt;
        // Opened with no sharing, the way a program holding a file exclusively
        // does: the copy fails reading it, and the failure has to say the
        // destination was never taken, or an overwrite rollback would go
        // looking for a half-copy to clear.
        let dir = tempdir().unwrap();
        let file = dir.path().join("em uso.mp4");
        fs::write(&file, "x").unwrap();
        let _held = fs::OpenOptions::new().read(true).share_mode(0).open(&file).unwrap();
        let copy = dir.path().join("copia.mp4");

        let failure = copy_path_no_replace(&file, &copy).unwrap_err();

        assert_eq!(failure.leftover(), Leftover::Nothing);
        assert!(!copy.exists());
        // ERROR_SHARING_VIOLATION, unwrapped from the HRESULT.
        assert!(matches!(&failure.into_error(), Error::Io(io) if io.raw_os_error() == Some(32)));
    }

    #[cfg(windows)]
    #[test]
    fn a_move_within_one_drive_is_a_rename_and_not_a_copy() {
        // Until 04/11/2026 Windows had no no-replace rename here, and Cut +
        // Paste of a folder copied every byte and deleted the source. A copy
        // writes the file anew, so its modification time becomes now; a rename
        // keeps it, which is what tells the two apart from outside.
        let dir = tempdir().unwrap();
        let src = dir.path().join("pasta");
        fs::create_dir(&src).unwrap();
        let file = src.join("foto.jpg");
        fs::write(&file, "payload").unwrap();
        let long_ago = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::options().write(true).open(&file).unwrap().set_modified(long_ago).unwrap();
        let dst = dir.path().join("movida");

        let kind = move_path_no_replace(&src, &dst).unwrap();

        assert_eq!(kind, MoveKind::Rename);
        assert!(!src.exists());
        let moved = dst.join("foto.jpg");
        assert_eq!(fs::read_to_string(&moved).unwrap(), "payload");
        assert_eq!(fs::metadata(&moved).unwrap().modified().unwrap(), long_ago);
    }

    #[cfg(windows)]
    #[test]
    fn a_folder_holding_a_junction_moves_within_one_drive() {
        // The copy fallback refuses links on Windows (`symlink_no_replace`), so
        // while every move went through it, a folder with a junction inside
        // (node_modules, a game's mods folder) could not be moved at all.
        // A junction needs no administrator, unlike a symbolic link.
        let dir = tempdir().unwrap();
        let target = dir.path().join("alvo");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("dentro.txt"), "theirs").unwrap();
        let src = dir.path().join("projeto");
        fs::create_dir(&src).unwrap();
        let junction = src.join("atalho");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        let dst = dir.path().join("projeto-movido");

        assert_eq!(move_path_no_replace(&src, &dst).unwrap(), MoveKind::Rename);

        let moved = dst.join("atalho");
        assert!(fs::symlink_metadata(&moved).unwrap().is_symlink(), "still a link, not a copy");
        assert_eq!(fs::read_to_string(moved.join("dentro.txt")).unwrap(), "theirs");
        assert_eq!(fs::read_to_string(target.join("dentro.txt")).unwrap(), "theirs");
    }

    #[cfg(windows)]
    #[test]
    fn the_no_replace_rename_reports_windows_codes_as_plain_io_errors() {
        // `is_cross_device` and every `AlreadyExists` check read the raw Win32
        // code; an HRESULT left wrapped would turn a cross-drive move into a
        // failure instead of a copy.
        let dir = tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("b.txt");
        fs::write(&src, "mine").unwrap();
        fs::write(&dst, "theirs").unwrap();

        let error = rename_no_replace(&src, &dst).unwrap().unwrap_err();

        // ERROR_ALREADY_EXISTS.
        assert_eq!(error.raw_os_error(), Some(183), "{error}");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        let missing = rename_no_replace(&dir.path().join("nada.txt"), &dir.path().join("c.txt"))
            .unwrap()
            .unwrap_err();
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound, "{missing}");
    }

    #[test]
    fn the_fallback_move_claims_the_destination_before_writing_to_it() {
        // Exercised directly: a test has one filesystem, so `rename` never
        // returns `EXDEV` and the copy path is otherwise unreachable here. It
        // is what runs wherever no no-replace rename exists, so it has to be as
        // refusing as the rename is.
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("top.txt"), "top").unwrap();
        let dst = dir.path().join("nested").join("dst");

        // A missing parent is created, the way `copy_path` does it.
        copy_path_no_replace(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("top.txt")).unwrap(), "top");

        assert!(copy_path_no_replace(&src, &dst).is_err());

        let file = dir.path().join("file.txt");
        fs::write(&file, "mine").unwrap();
        let taken = dir.path().join("taken.txt");
        fs::write(&taken, "theirs").unwrap();
        assert!(copy_path_no_replace(&file, &taken).is_err());
        assert_eq!(fs::read_to_string(&taken).unwrap(), "theirs");
    }

    #[test]
    fn a_rename_that_only_changes_case_collides_with_nothing() {
        let dir = tempdir().unwrap();
        let photo = dir.path().join("foto.jpg");
        fs::write(&photo, "mine").unwrap();
        let other = dir.path().join("outra.jpg");
        fs::write(&other, "theirs").unwrap();

        // On a case-insensitive filesystem `Foto.jpg` is taken — by the very
        // file being renamed. Where case matters it is simply free.
        assert!(!would_collide_with_another(&photo, &dir.path().join("Foto.jpg")));
        assert!(would_collide_with_another(&photo, &other), "another file is a collision");
        assert!(!would_collide_with_another(&photo, &dir.path().join("nova.jpg")));

        let renamed = rename_in_place(&photo, "Foto.jpg").unwrap();
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"Foto.jpg".to_string()), "{names:?}");
        assert_eq!(fs::read_to_string(renamed).unwrap(), "mine");
    }

    #[test]
    fn rename_in_place_changes_name_keeps_dir() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("old.txt");
        fs::write(&src, "x").unwrap();

        let dst = rename_in_place(&src, "new.txt").unwrap();
        assert_eq!(dst, dir.path().join("new.txt"));
        assert!(!src.exists());
        assert!(dst.exists());
    }

    #[test]
    fn create_dir_makes_a_new_directory() {
        let dir = tempdir().unwrap();
        let created = create_dir(dir.path(), "fresh").unwrap();
        assert_eq!(created, dir.path().join("fresh"));
        assert!(created.is_dir());
    }

    // Regression (04/11): a Delete on a pool thread a PDF preview had left in
    // the multithreaded apartment made the `trash` crate panic, which aborted
    // the app. The thread here is put in that state on purpose.
    #[cfg(windows)]
    #[test]
    #[allow(unsafe_code)]
    fn trash_works_from_a_thread_already_in_the_multithreaded_apartment() {
        #[link(name = "ole32")]
        unsafe extern "system" {
            fn CoInitializeEx(reserved: *const std::ffi::c_void, coinit: u32) -> i32;
        }
        const COINIT_MULTITHREADED: u32 = 0;
        let dir = tempdir().unwrap();
        let file = dir.path().join("nohrs-trash-mta-test.txt");
        fs::write(&file, "x").unwrap();
        let outcome = std::thread::spawn({
            let file = file.clone();
            move || {
                assert!(unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED) } >= 0);
                trash_path(&file, None)
            }
        })
        .join()
        .expect("trash_path panicked");
        outcome.unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn delete_permanent_removes_file_and_tree() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "x").unwrap();
        delete_permanent(&file).unwrap();
        assert!(!file.exists());

        let tree = dir.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("inner.txt"), "y").unwrap();
        delete_permanent(&tree).unwrap();
        assert!(!tree.exists());
    }

    #[test]
    fn would_conflict_detects_a_dangling_symlink() {
        let dir = tempdir().unwrap();
        let link = dir.path().join("broken");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("missing-target"), &link).unwrap();
        #[cfg(not(unix))]
        std::fs::write(&link, "x").unwrap();
        // `Path::exists()` would report `false` for a dangling symlink; the
        // entry is nonetheless occupied.
        assert!(would_conflict(&link));
    }

    #[test]
    fn rename_and_create_reject_path_traversal_names() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("f.txt");
        fs::write(&src, "x").unwrap();
        for bad in ["../escape", "a/b", ".", "..", ""] {
            assert!(
                rename_in_place(&src, bad).is_err(),
                "rename allowed {bad:?}"
            );
            assert!(
                create_dir(dir.path(), bad).is_err(),
                "create allowed {bad:?}"
            );
        }
        // A plain name is still accepted.
        assert!(create_dir(dir.path(), "ok-dir").is_ok());
    }

    #[test]
    fn is_cross_volume_false_within_same_dir() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("a.txt");
        fs::write(&src, "x").unwrap();
        assert!(!is_cross_volume(&src, dir.path()).unwrap());
    }

    // Pins the errnos the two entry opens produce for a symbolic link, because
    // the comment on `empty_dir`'s `NOTDIR | LOOP` arm once named the wrong one
    // and nothing here contradicted it. Which errno arrives is the kernel's
    // choice of check order, so it is measured rather than described.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused_by_both_entry_opens() {
        use rustix::fs::{Mode, OFlags, open};

        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let descending = open(
            &link,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect_err("empty_dir must not open a link as the directory it points at");

        // No `O_DIRECTORY`: the entry's type is what this open exists to find
        // out, so it cannot ask for one.
        let claiming = open(
            &link,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect_err("nor may open_claimed");

        // The portable part, and the one the match arm rests on: whatever the
        // platform returns is an errno that arm already handles.
        for errno in [descending, claiming] {
            assert!(
                matches!(errno, rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP),
                "a symlink at the entry produced an errno neither arm matches: {errno:?}"
            );
        }

        // Without `O_DIRECTORY` there is nothing to answer first, so the
        // refusal is `O_NOFOLLOW`'s own.
        assert_eq!(claiming, rustix::io::Errno::LOOP);

        // With it, both supported targets answer `O_DIRECTORY` first. Review
        // expected macOS to differ, XNU refusing the link in `namei` before the
        // directory check and so reporting `ELOOP`; the runner says otherwise.
        assert_eq!(descending, rustix::io::Errno::NOTDIR);
    }
}
