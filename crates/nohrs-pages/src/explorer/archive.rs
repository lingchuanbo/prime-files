//! An archive browsed as if it were a folder.
//!
//! The address is `arc:<the file on disk>|<the path inside it>`, the part
//! after the bar empty for the archive itself (`arc:C:\a.zip`, then
//! `arc:C:\a.zip|docs`). A Windows path cannot hold a `|`, so one bar is
//! enough to tell the two halves apart — no escaping, and the address can be
//! kept in the tab history like any folder's.
//!
//! The entries come from the same reader the preview pane uses
//! ([`super::preview_listing::archive_entries`]): zip here, the rest through
//! 7-Zip or `tar.exe`. Nothing is decompressed to list a folder; opening a
//! file inside one is left to the preview, which asks for the entry itself.

use std::path::Path;

use nohrs_services::fs::listing::FileEntryDto;

use super::preview_listing::{hidden_command, seven_zip};

/// What an archive address starts with.
pub(crate) const PREFIX: &str = "arc:";

/// The address of `inner` inside `archive`; an empty `inner` is the archive's
/// own root.
pub(crate) fn location(archive: &str, inner: &str) -> String {
    if inner.is_empty() {
        format!("{PREFIX}{archive}")
    } else {
        format!("{PREFIX}{archive}|{}", inner.trim_matches('/'))
    }
}

/// The archive file and the path inside it, for an archive address; `None`
/// for any other path.
pub(crate) fn split(address: &str) -> Option<(&str, &str)> {
    let rest = address.strip_prefix(PREFIX)?;
    match rest.split_once('|') {
        Some((archive, inner)) => Some((archive, inner)),
        None => Some((rest, "")),
    }
}

/// Where "up" leads from an archive address: the folder holding the archive
/// from its root, the folder above it from anywhere inside.
pub(crate) fn parent(address: &str) -> Option<String> {
    let (archive, inner) = split(address)?;
    match inner.trim_matches('/').rsplit_once('/') {
        Some((parent, _)) => Some(location(archive, parent)),
        None => {
            if inner.trim_matches('/').is_empty() {
                Path::new(archive)
                    .parent()
                    .map(|parent| parent.to_string_lossy().into_owned())
            } else {
                Some(location(archive, ""))
            }
        }
    }
}

/// Whether `path` is a file this program can look inside, by its extension.
/// An archive address is not one (a zip inside a zip is a file to open later,
/// not an archive to walk into).
pub(crate) fn is_archive_file(path: &str) -> bool {
    if path.starts_with(PREFIX) {
        return false;
    }
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        extension.as_str(),
        "zip" | "rar" | "7z" | "tar" | "tgz" | "gz" | "xz" | "bz2" | "zst" | "cab" | "iso"
    )
}

/// The rows of the folder at `address`: the archive's root, or one folder
/// inside it. Read off the UI thread, like any other folder.
pub(crate) fn read_folder(address: &str) -> Result<Vec<FileEntryDto>, String> {
    let Some((archive, inner)) = split(address) else {
        return Err(format!("not an archive address: {address}"));
    };
    if !Path::new(archive).is_file() {
        return Err(format!("{archive} is not a file"));
    }
    let entries = super::preview_listing::archive_entries(Path::new(archive))?;
    let inner = inner.trim_matches('/');

    // The entries under the folder being listed, each as the first name on
    // its way to the entry: a folder when it carries a slash after that.
    let mut folders: Vec<String> = Vec::new();
    let mut files: Vec<(String, u64)> = Vec::new();
    for (name, is_dir, size) in entries {
        let path = name.replace('\\', "/");
        let path = path.trim_matches('/');
        if path.is_empty() {
            continue;
        }
        let rest = if inner.is_empty() {
            path
        } else {
            let Some(rest) = path.strip_prefix(inner) else { continue };
            // The folder's own line (`docs/`) is not one of its children.
            let Some(rest) = rest.strip_prefix('/') else { continue };
            rest
        };
        if rest.is_empty() {
            continue;
        }
        match rest.split_once('/') {
            Some((folder, _)) => {
                if !folders.iter().any(|kept| kept == folder) {
                    folders.push(folder.to_string());
                }
            }
            None => {
                if is_dir {
                    if !folders.iter().any(|kept| kept == rest) {
                        folders.push(rest.to_string());
                    }
                } else if !files.iter().any(|(kept, _)| kept == rest) {
                    files.push((rest.to_string(), size));
                }
            }
        }
    }

    folders.sort_by_key(|folder| folder.to_lowercase());
    files.sort_by_key(|(file, _)| file.to_lowercase());
    let mut rows: Vec<FileEntryDto> = folders
        .into_iter()
        .map(|folder| row(archive, inner, &folder, "dir", 0))
        .collect();
    rows.extend(files.into_iter().map(|(file, size)| row(archive, inner, &file, "file", size)));
    Ok(rows)
}

// One row of the archive's listing: where it is inside the archive, and the
// address that opens it.
fn row(archive: &str, inner: &str, name: &str, kind: &str, size: u64) -> FileEntryDto {
    let path = if inner.is_empty() {
        location(archive, name)
    } else {
        location(archive, &format!("{inner}/{name}"))
    };
    FileEntryDto {
        name: name.to_string(),
        path,
        kind: kind.to_string(),
        size,
        modified: 0,
    }
}

/// What the path bar shows for an archive address: the file's name, and the
/// path inside it when there is one (`pacote.zip › docs`).
pub(crate) fn display_name(address: &str) -> Option<String> {
    let (archive, inner) = split(address)?;
    let file = Path::new(archive)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| archive.to_string());
    let inner = inner.trim_matches('/');
    Some(if inner.is_empty() {
        file
    } else {
        format!("{file} › {}", inner.replace('/', " › "))
    })
}

/// Unpacks the entry `address` names into a folder of its own under the
/// system's temporary folder, and returns the file there: what the preview
/// and "open in another program" need, since neither can read inside an
/// archive. A file already unpacked from the same archive, newer than it, is
/// kept as it is — moving through a folder's files unpacks each once.
#[allow(clippy::disallowed_methods)]
pub(crate) fn extract(address: &str) -> Result<std::path::PathBuf, String> {
    let (archive, inner) = split(address).ok_or_else(|| format!("not an archive address: {address}"))?;
    let inner = inner.trim_matches('/');
    if inner.is_empty() {
        return Err("the archive itself has nothing to unpack".to_string());
    }
    let archive_path = Path::new(archive);
    let name = inner.rsplit('/').next().unwrap_or(inner);
    let folder = unpack_folder(archive_path, inner);
    let target = folder.join(name);
    // Fresh enough: unpacked from this very archive after it was written.
    if let (Ok(unpacked), Ok(archive_time)) =
        (std::fs::metadata(&target), std::fs::metadata(archive_path))
        && let (Ok(unpacked_time), Ok(archive_time)) = (unpacked.modified(), archive_time.modified())
        && unpacked_time >= archive_time
        && unpacked.len() > 0
    {
        return Ok(target);
    }
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let extension = archive_path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_lowercase();
    if extension == "zip" {
        extract_zip(archive_path, inner, &target)?;
    } else {
        extract_with_7z(archive_path, inner, &folder)?;
    }
    if !target.is_file() {
        return Err(format!("{name} was not found inside the archive"));
    }
    Ok(target)
}

// The folder one entry is unpacked into: one per archive and entry, named
// from the archive so two `docs/nota.txt` never meet, and stable across runs
// so a preview is unpacked once.
fn unpack_folder(archive: &Path, inner: &str) -> std::path::PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in archive.to_string_lossy().to_lowercase().bytes().chain([0u8]).chain(inner.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    let folder_name = archive
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string());
    std::env::temp_dir()
        .join("nohrs-archive")
        .join(format!("{folder_name}-{hash:016x}"))
}

#[allow(clippy::disallowed_methods)]
fn extract_zip(archive: &Path, inner: &str, target: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|error| error.to_string())?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|error| error.to_string())?;
    // Names inside a zip may be written with either separator.
    let name = if zip.file_names().any(|name| name == inner) {
        inner.to_string()
    } else {
        inner.replace('/', "\\")
    };
    let mut reader = zip.by_name(&name).map_err(|error| error.to_string())?;
    let mut out = std::fs::File::create(target).map_err(|error| error.to_string())?;
    std::io::copy(&mut reader, &mut out).map_err(|error| error.to_string())?;
    Ok(())
}

// 7-Zip unpacks one entry: `e` writes it flat into the folder, which is what
// the target path already expects. Without 7-Zip, `tar.exe` (libarchive, in
// Windows since 10) extracts the named member of the formats it reads.
fn extract_with_7z(archive: &Path, inner: &str, folder: &Path) -> Result<(), String> {
    if let Some(seven) = seven_zip() {
        let output = hidden_command(&seven)
            .arg("e")
            .arg("-y")
            .arg(format!("-o{}", folder.display()))
            .arg("-p")
            .arg("--")
            .arg(archive)
            .arg(inner)
            .output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            return Ok(());
        }
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let output = hidden_command(Path::new(r"C:\Windows\System32\tar.exe"))
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(folder)
        .arg(inner)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_split_at_their_one_bar() {
        assert_eq!(split("arc:C:\\a.zip"), Some(("C:\\a.zip", "")));
        assert_eq!(split("arc:C:\\a.zip|docs"), Some(("C:\\a.zip", "docs")));
        assert_eq!(split("arc:C:\\a.zip|docs/2026"), Some(("C:\\a.zip", "docs/2026")));
        assert_eq!(split("C:\\a.zip"), None);
    }

    #[test]
    fn up_from_an_archive_goes_through_its_own_folders_to_the_file() {
        // Inside a folder: its parent inside the archive.
        assert_eq!(parent("arc:C:\\a.zip|docs/2026").as_deref(), Some("arc:C:\\a.zip|docs"));
        // One folder deep: the archive's own root.
        assert_eq!(parent("arc:C:\\a.zip|docs").as_deref(), Some("arc:C:\\a.zip"));
        // At the root: the folder holding the archive.
        assert_eq!(parent("arc:C:\\a.zip").as_deref(), Some("C:\\"));
        assert_eq!(parent("arc:C:\\pasta\\a.zip").as_deref(), Some("C:\\pasta"));
    }

    #[test]
    fn a_file_is_an_archive_by_its_extension_and_an_address_never_is() {
        assert!(is_archive_file("C:\\a.zip"));
        assert!(is_archive_file("C:\\a.RAR"));
        assert!(is_archive_file("C:\\a.7z"));
        assert!(!is_archive_file("C:\\a.txt"));
        // A zip inside an archive is not walked into as a file system.
        assert!(!is_archive_file("arc:C:\\a.zip|inside.zip"));
    }

    #[test]
    fn the_path_bar_names_the_archive_and_where_it_is_inside() {
        assert_eq!(display_name("arc:C:\\pasta\\a.zip").as_deref(), Some("a.zip"));
        assert_eq!(
            display_name("arc:C:\\pasta\\a.zip|docs/2026").as_deref(),
            Some("a.zip › docs › 2026")
        );
        assert_eq!(display_name("C:\\a.zip"), None);
    }

    #[test]
    fn a_row_of_an_archive_nests_its_address() {
        let folder_row = row("C:\\a.zip", "", "docs", "dir", 0);
        assert_eq!(folder_row.path, "arc:C:\\a.zip|docs");
        assert_eq!(folder_row.kind, "dir");
        let file_row = row("C:\\a.zip", "docs", "nota.txt", "file", 12);
        assert_eq!(file_row.path, "arc:C:\\a.zip|docs/nota.txt");
        assert_eq!(file_row.size, 12);
    }

    #[test]
    fn an_entry_is_unpacked_where_the_preview_and_other_programs_can_read_it() {
        use std::io::Write as _;

        let folder = tempfile::tempdir().expect("temp folder");
        let path = folder.path().join("pacote.zip");
        {
            let file = std::fs::File::create(&path).expect("create");
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("docs/leiame.md", options).expect("entry");
            zip.write_all(b"# oi").expect("write");
            zip.finish().expect("finish");
        }
        let archive = path.to_string_lossy().into_owned();

        let address = location(&archive, "docs/leiame.md");
        let unpacked = extract(&address).expect("unpacked");
        assert!(unpacked.is_file(), "{unpacked:?}");
        assert_eq!(std::fs::read_to_string(&unpacked).expect("read"), "# oi");
        assert_eq!(unpacked.file_name().map(|name| name.to_string_lossy().into_owned()).as_deref(), Some("leiame.md"));

        // Asking again reuses the copy rather than unpacking over it.
        assert_eq!(extract(&address).expect("again"), unpacked);
        // The archive itself is not an entry.
        assert!(extract(&location(&archive, "")).is_err());
        // Neither is a path that names nothing inside it.
        assert!(extract(&location(&archive, "falta.txt")).is_err());
    }

    #[test]
    fn a_zip_reads_as_folders_and_files_the_way_a_folder_does() {
        use std::io::Write as _;

        let folder = tempfile::tempdir().expect("temp folder");
        let path = folder.path().join("pacote.zip");
        {
            let file = std::fs::File::create(&path).expect("create");
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("nota.txt", options).expect("entry");
            zip.write_all(b"oi").expect("write");
            zip.add_directory("docs/", options).expect("folder");
            zip.start_file("docs/leiame.md", options).expect("entry");
            zip.write_all(b"# oi").expect("write");
            zip.start_file("docs/mais/fundo.txt", options).expect("entry");
            zip.write_all(b"x").expect("write");
            zip.finish().expect("finish");
        }
        let archive = path.to_string_lossy().into_owned();

        let root = read_folder(&location(&archive, "")).expect("the archive's root");
        let names: Vec<&str> = root.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["docs", "nota.txt"], "folders first, as a listing shows them");

        let docs = read_folder(&location(&archive, "docs")).expect("a folder inside");
        let names: Vec<&str> = docs.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["mais", "leiame.md"]);
        assert_eq!(docs[1].path, location(&archive, "docs/leiame.md"));

        // Up from a folder inside leads back to the archive's root, and from
        // there to the folder holding it.
        let inner = location(&archive, "docs/mais");
        assert_eq!(parent(&inner).as_deref(), Some(location(&archive, "docs").as_str()));
        assert_eq!(parent(&location(&archive, "")).as_deref(), Some(folder.path().to_string_lossy().as_ref()));

        // A file that is not an archive is refused rather than listed empty.
        let text = folder.path().join("nota.txt");
        std::fs::write(&text, "x").expect("write");
        assert!(read_folder(&location(&text.to_string_lossy(), "")).is_err());
    }
}
