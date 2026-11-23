//! The atomic file write the configuration files use (§5.2).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

/// Replaces `path` with `contents`, atomically, and returns the content hash.
///
/// The file is written beside itself as `.toml.tmp`, flushed to the disk, the
/// previous version is copied to `.toml.bak`, and only then is the temporary file
/// renamed over the original: a crash or a full disk leaves the old file in place.
/// The hash lets the caller recognize its own write when the file watcher reports
/// it (§5.3).
///
/// Runs on the caller's thread, which is never the foreground one (§18): the write
/// is deliberately blocking, which is why the lint is silenced here.
#[allow(clippy::disallowed_methods)]
pub fn write_atomically(path: &Path, contents: &str) -> std::io::Result<u64> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory)?;
    let temporary = path.with_extension("toml.tmp");
    let backup = path.with_extension("toml.bak");
    let mut file = std::fs::File::create(&temporary)?;
    {
        use std::io::Write as _;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    drop(file);
    if path.exists() {
        std::fs::copy(path, &backup)?;
    }
    std::fs::rename(&temporary, path)?;
    let mut hasher = DefaultHasher::new();
    contents.as_bytes().hash(&mut hasher);
    Ok(hasher.finish())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn the_atomic_write_replaces_the_file_and_keeps_a_backup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quicklaunch.toml");
        write_atomically(&path, "schema_version = 1\n").unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, "schema_version = 1\n");

        let hash = write_atomically(&path, "schema_version = 1\n\n[[group]]\n").unwrap();
        assert_ne!(hash, 0);
        let second = std::fs::read_to_string(&path).unwrap();
        assert!(second.contains("[[group]]"), "{second}");
        let backup = directory.path().join("quicklaunch.toml.bak");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), first);
        assert!(!directory.path().join("quicklaunch.toml.tmp").exists());
    }

    #[test]
    fn a_failed_write_leaves_the_original_file_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quicklaunch.toml");
        write_atomically(&path, "schema_version = 1\n").unwrap();
        // A directory where the temporary file belongs makes the write fail, which
        // is the "disk full / read-only folder" case without changing any
        // attribute of the test directory.
        std::fs::create_dir_all(directory.path().join("quicklaunch.toml.tmp")).unwrap();
        assert!(write_atomically(&path, "schema_version = 99\n").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "schema_version = 1\n",
            "the original file must not be touched"
        );
    }

    #[test]
    fn the_hash_is_the_content() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("a.toml");
        let second = directory.path().join("b.toml");
        let one = write_atomically(&first, "x = 1\n").unwrap();
        let two = write_atomically(&second, "x = 1\n").unwrap();
        assert_eq!(one, two, "the same content hashes the same, wherever it is");
        let three = write_atomically(&first, "x = 2\n").unwrap();
        assert_ne!(one, three);
    }
}
