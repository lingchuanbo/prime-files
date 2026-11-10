//! What the preview pane lists for a folder (its contents) and for a zip
//! archive (the entries inside it), read off the UI thread.

use nohrs_core::{k, t, tf};
use nohrs_core::i18n;
use std::path::Path;

use super::entries::SortName;

/// Entries listed before the rest is only counted: a folder of tens of
/// thousands of files would otherwise build a list nobody scrolls through.
const MAX_ITEMS: usize = 2000;

/// One line of a listing preview.
#[derive(Clone, Debug)]
pub(crate) struct ListingItem {
    /// What the line shows: a name, or a path inside an archive.
    pub name: String,
    /// The item on disk, for a folder's own contents; `None` inside a zip.
    pub path: Option<String>,
    pub is_dir: bool,
    /// Size in bytes (uncompressed, inside a zip); `None` for folders.
    pub size: Option<u64>,
    /// Nesting inside a zip, for indenting; always 0 for a folder.
    pub depth: usize,
    /// Last change, Unix seconds, for a folder's own files (0 inside an
    /// archive): with the size, what tells a waveform is still the file's.
    pub modified: u64,
}

/// A folder's or an archive's contents, as the preview pane shows them.
#[derive(Clone, Debug)]
pub(crate) struct PreviewListing {
    pub items: Vec<ListingItem>,
    /// "12 pastas, 40 arquivos" and the like.
    pub summary: String,
    /// Entries left out past [`MAX_ITEMS`].
    pub hidden: usize,
}

/// The contents of `folder`: subfolders first, then files, each by name.
#[allow(clippy::disallowed_methods)]
pub(crate) fn read_folder(folder: &Path, show_hidden: bool) -> std::io::Result<PreviewListing> {
    let mut items: Vec<ListingItem> = std::fs::read_dir(folder)?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !show_hidden && is_hidden(&entry) {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            Some(ListingItem {
                path: Some(entry.path().to_string_lossy().into_owned()),
                is_dir: metadata.is_dir(),
                size: (!metadata.is_dir()).then_some(metadata.len()),
                depth: 0,
                modified: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |elapsed| elapsed.as_secs()),
                name,
            })
        })
        .collect();
    // Once per name, not per comparison, and the listing's own order: this
    // lowercased both names on every comparison and put `f10` before `f2`,
    // so a folder read in the preview disagreed with the same folder opened
    // (OTIMIZACAO.md §7.5).
    items.sort_by_cached_key(|item| (!item.is_dir, natural_key(&item.name)));
    let folders = items.iter().filter(|item| item.is_dir).count();
    let files = items.len() - folders;
    let summary = if items.is_empty() {
        t!("Pasta vazia").to_string()
    } else {
        format!("{} · {}", count_folders(folders), count_files(files))
    };
    Ok(cap(items, summary))
}

/// The entries of the zip archive at `path`, as a tree: each one under the
/// folder it sits in, indented by depth. Only the archive's directory is
/// read; nothing is decompressed.
#[allow(clippy::disallowed_methods)]
pub(crate) fn read_zip(path: &Path) -> Result<PreviewListing, String> {
    Ok(archive_tree(zip_entries(path)?))
}

/// The entries of the archive at `path`, flat — the name inside it, whether
/// it is a folder, and its uncompressed size — whichever format it is: zip is
/// read here, the rest by 7-Zip or `tar.exe` (see [`read_archive`]). What
/// browsing an archive as a folder lists, and what the preview draws as a
/// tree, both start from this.
pub(crate) fn archive_entries(path: &Path) -> Result<Vec<(String, bool, u64)>, String> {
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_lowercase();
    if extension == "zip" {
        zip_entries(path)
    } else {
        other_entries(path)
    }
}

#[allow(clippy::disallowed_methods)]
fn zip_entries(path: &Path) -> Result<Vec<(String, bool, u64)>, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|error| error.to_string())?;
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(index) else {
            continue;
        };
        entries.push((entry.name().to_string(), entry.is_dir(), entry.size()));
    }
    Ok(entries)
}

/// Archives the preview lists besides zip: what 7-Zip (or, without it, the
/// `tar.exe` Windows ships) can read the directory of.
pub(crate) fn is_other_archive(path: &str) -> bool {
    let extension = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        extension.as_str(),
        "rar" | "7z" | "tar" | "tgz" | "gz" | "xz" | "bz2" | "zst" | "cab" | "iso"
    )
}

/// The entries of a .rar, .7z or other archive [`is_other_archive`] names.
///
/// Neither format has a reader worth linking: RAR's only complete one is
/// the C++ unrar, whose licence forbids using it to write RAR. 7-Zip reads
/// both, RAR5 included, is on most machines that have such files, and
/// prints names as UTF-8 when asked to. Without it, `tar.exe` (libarchive,
/// in Windows since 10) reads both too, but writes names in the ANSI code
/// page, so a name outside it comes out with `?` in it.
pub(crate) fn read_archive(path: &Path) -> Result<PreviewListing, String> {
    Ok(archive_tree(other_entries(path)?))
}

// The entries of a .rar, .7z or other archive [`is_other_archive`] names,
// read by 7-Zip, or `tar.exe` without it.
fn other_entries(path: &Path) -> Result<Vec<(String, bool, u64)>, String> {
    match seven_zip() {
        Some(seven) => {
            // `-p` with no password: an encrypted archive fails at once
            // instead of waiting for a password on a console nobody sees.
            let output = hidden_command(&seven)
                .args(["l", "-slt", "-sccUTF-8", "-p", "--"])
                .arg(path)
                .output()
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
            Ok(parse_seven_zip(&String::from_utf8_lossy(&output.stdout)))
        }
        None => {
            let output = hidden_command(Path::new(r"C:\Windows\System32\tar.exe"))
                .arg("-tvf")
                .arg(path)
                .output()
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(ansi_to_string(&output.stderr).trim().to_string());
            }
            Ok(parse_tar_listing(&ansi_to_string(&output.stdout)))
        }
    }
}

// 7-Zip's console program, where its installer puts it or on the PATH.
pub(crate) fn seven_zip() -> Option<std::path::PathBuf> {
    let installed = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|dir| Path::new(&dir).join("7-Zip").join("7z.exe"));
    let on_path = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).map(|dir| dir.join("7z.exe")).collect::<Vec<_>>())
        .unwrap_or_default();
    installed.chain(on_path).find(|candidate| candidate.is_file())
}

// A console program run without flashing a console window over the app.
pub(crate) fn hidden_command(program: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.stdin(std::process::Stdio::null());
    command
}

/// The entries of `7z l -slt`: blocks of `Key = value` lines, one per entry,
/// after a `----------` line that ends the archive's own block.
fn parse_seven_zip(text: &str) -> Vec<(String, bool, u64)> {
    let Some((_, entries)) = text.split_once("\n----------") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut current: Option<(String, bool, u64)> = None;
    for line in entries.lines() {
        let line = line.trim_end_matches('\r');
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        match (key, current.as_mut()) {
            ("Path", _) => {
                out.extend(current.take());
                current = Some((value.to_string(), false, 0));
            }
            ("Size", Some(entry)) => entry.2 = value.parse().unwrap_or(0),
            ("Folder", Some(entry)) => entry.1 |= value == "+",
            // `D` among a RAR's or a 7z's attributes marks a folder.
            ("Attributes", Some(entry)) => {
                entry.1 |= value.split_whitespace().next().is_some_and(|attrs| attrs.contains('D'));
            }
            _ => {}
        }
    }
    out.extend(current);
    out
}

/// The entries of `tar -tvf`, an `ls -l` line each: mode, links, owner,
/// group, size, three fields of date, then the name, which may hold spaces.
fn parse_tar_listing(text: &str) -> Vec<(String, bool, u64)> {
    text.lines()
        .filter_map(|line| {
            let mut rest = line.trim_end_matches('\r');
            let mut fields = Vec::with_capacity(8);
            for _ in 0..8 {
                rest = rest.trim_start();
                let end = rest.find(char::is_whitespace)?;
                fields.push(&rest[..end]);
                rest = &rest[end..];
            }
            let name = rest.strip_prefix(' ').unwrap_or(rest).to_string();
            let is_dir = fields[0].starts_with('d') || name.ends_with('/');
            Some((name, is_dir, fields[4].parse().unwrap_or(0)))
        })
        .collect()
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn ansi_to_string(bytes: &[u8]) -> String {
    use windows::Win32::Globalization::{CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
    if bytes.is_empty() {
        return String::new();
    }
    // SAFETY: the first call only measures; the second writes into a buffer
    // of exactly the length the first returned.
    unsafe {
        let len = MultiByteToWideChar(CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None);
        let mut wide = vec![0u16; len.max(0) as usize];
        MultiByteToWideChar(CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, Some(&mut wide));
        String::from_utf16_lossy(&wide)
    }
}

#[cfg(not(windows))]
fn ansi_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// An archive's entries (`path`, folder?, size) as a tree, folders first at
/// every level and each entry indented under the folder it sits in.
fn archive_tree(entries: Vec<(String, bool, u64)>) -> PreviewListing {
    let mut paths: std::collections::BTreeMap<String, (bool, Option<u64>)> = std::collections::BTreeMap::new();
    let mut total: u64 = 0;
    for (name, is_dir, size) in entries {
        let name = name.replace('\\', "/");
        let trimmed = name.trim_end_matches('/').to_string();
        if trimmed.is_empty() {
            continue;
        }
        // Archives often list only files; their folders are implied.
        let mut parent = String::new();
        for part in trimmed.split('/').collect::<Vec<_>>().iter().rev().skip(1).rev() {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(part);
            paths.entry(parent.clone()).or_insert((true, None));
        }
        if !is_dir {
            total += size;
        }
        paths.insert(trimmed, (is_dir, (!is_dir).then_some(size)));
    }
    // Folders before files at every level, the way the listing orders them.
    let mut keys: Vec<(&String, &(bool, Option<u64>))> = paths.iter().collect();
    keys.sort_by_cached_key(|(path, (is_dir, _))| sort_key(path, *is_dir, &paths));
    let items: Vec<ListingItem> = keys
        .into_iter()
        .map(|(path, (is_dir, size))| ListingItem {
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            path: None,
            is_dir: *is_dir,
            size: *size,
            depth: path.matches('/').count(),
            modified: 0,
        })
        .collect();
    let folders = items.iter().filter(|item| item.is_dir).count();
    let files = items.len() - folders;
    let summary = tf!(
        "{folders} · {files} · {size} descompactado",
        folders = count_folders(folders),
        files = count_files(files),
        size = human_size(total)
    );
    cap(items, summary)
}

// The key that sorts an archive entry into tree order with folders first at each
// level: every ancestor component tagged as a folder, then the entry itself.
fn sort_key(
    path: &str,
    is_dir: bool,
    paths: &std::collections::BTreeMap<String, (bool, Option<u64>)>,
) -> Vec<(bool, SortName)> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut key = Vec::with_capacity(parts.len());
    let mut prefix = String::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            prefix.push('/');
        }
        prefix.push_str(part);
        let dir = if index + 1 == parts.len() {
            is_dir
        } else {
            paths.get(&prefix).is_none_or(|(dir, _)| *dir)
        };
        // `false` sorts first: folders before files.
        key.push((!dir, natural_key(part)));
    }
    key
}

fn cap(mut items: Vec<ListingItem>, summary: String) -> PreviewListing {
    let hidden = items.len().saturating_sub(MAX_ITEMS);
    items.truncate(MAX_ITEMS);
    PreviewListing { items, summary, hidden }
}

// The order the listing sorts names in, Explorer's.
fn natural_key(name: &str) -> SortName {
    SortName::new(name)
}

fn count_folders(n: usize) -> String {
    i18n::plural(n, k!("{n} pasta"), k!("{n} pastas"))
}

fn count_files(n: usize) -> String {
    i18n::plural(n, k!("{n} arquivo"), k!("{n} arquivos"))
}

/// `1.5 MB`, `320 KB`: the size as the listing shows it.
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(windows)]
fn is_hidden(entry: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    entry
        .metadata()
        .is_ok_and(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
        || entry.file_name().to_string_lossy().starts_with('.')
}

#[cfg(not(windows))]
fn is_hidden(entry: &std::fs::DirEntry) -> bool {
    entry.file_name().to_string_lossy().starts_with('.')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_folder_in_the_preview_is_in_the_listing_order() {
        // Numbers as numbers, as the listing (and Explorer) sorts: the
        // preview used to put `f10` before `f2`.
        let dir = tempfile::tempdir().unwrap();
        for name in ["f10.txt", "f2.txt", "F1.txt"] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        let listing = read_folder(dir.path(), false).unwrap();
        let names: Vec<_> = listing.items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["F1.txt", "f2.txt", "f10.txt"]);
    }

    #[test]
    fn a_folder_lists_subfolders_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        std::fs::create_dir(dir.path().join("Zeta")).unwrap();
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        let listing = read_folder(dir.path(), false).unwrap();
        let names: Vec<_> = listing.items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["beta", "Zeta", "a.txt"]);
        assert_eq!(listing.summary, "2 pastas · 1 arquivo");
    }

    #[test]
    fn a_zip_lists_its_tree_with_implied_folders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pack.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file("readme.txt", options).unwrap();
        writer.write_all(b"hi").unwrap();
        writer.start_file("src/main.rs", options).unwrap();
        writer.write_all(b"fn main() {}").unwrap();
        writer.finish().unwrap();

        let listing = read_zip(&path).unwrap();
        let lines: Vec<_> = listing.items.iter().map(|item| (item.depth, item.name.as_str(), item.is_dir)).collect();
        assert_eq!(lines, [(0, "src", true), (1, "main.rs", false), (0, "readme.txt", false)]);
        assert!(listing.summary.starts_with("1 pasta · 2 arquivos"));
    }

    #[test]
    fn seven_zip_blocks_become_entries() {
        let text = "Listing archive: x.rar\n\n--\nPath = x.rar\nType = Rar5\n\n----------\nPath = docs\\a b.txt\nFolder = -\nSize = 12\nAttributes = A\n\nPath = docs\nFolder = +\nSize = 0\nAttributes = D\n\n";
        let entries = parse_seven_zip(text);
        assert_eq!(entries, [("docs\\a b.txt".to_string(), false, 12), ("docs".to_string(), true, 0)]);
        let listing = archive_tree(entries);
        let lines: Vec<_> = listing.items.iter().map(|item| (item.depth, item.name.as_str())).collect();
        assert_eq!(lines, [(0, "docs"), (1, "a b.txt")]);
    }

    #[test]
    fn tar_lines_keep_spaces_in_names() {
        let text = "-rw-rw-rw-  0 0      0           3 nov  04 11:11 pasta/ação teste.txt\r\ndrwxr-xr-x  0 0      0           0 nov  04 11:11 pasta/\r\n";
        assert_eq!(
            parse_tar_listing(text),
            [("pasta/ação teste.txt".to_string(), false, 3), ("pasta/".to_string(), true, 0)]
        );
    }
}
