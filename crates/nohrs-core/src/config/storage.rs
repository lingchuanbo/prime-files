use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::paths;

/// Copies missing profile files into the selected storage root before startup.
/// Copies missing profile files into the selected storage root before startup.
pub fn prepare_storage() -> io::Result<()> {
    let portable = paths::portable_mode_enabled();
    let (config_source, config_target) = if portable {
        (paths::config_dir(), paths::portable_config_dir())
    } else {
        (paths::portable_config_dir(), paths::config_dir())
    };
    copy_missing_files(&config_source, &config_target)?;
    for (source, target, skip) in migration_roots(portable) {
        copy_missing_tree(&source, &target, skip)?;
        fs::create_dir_all(target)?;
    }
    migrate_panic_log(portable)?;
    fs::create_dir_all(paths::runtime_dir())?;
    Ok(())
}

/// What a profile migration copies, and what it leaves behind.
///
/// The cache is not copied: `everything.idx` and `quick_launch_icons` are rebuilt,
/// and on the machine this was measured on the index alone was 258 MB. Nor is the
/// state's `logs` directory (752 MB there); [`migrate_panic_log`] carries the crash
/// record over instead, which is the only part of it worth keeping.
fn migration_roots(portable: bool) -> Vec<(PathBuf, PathBuf, &'static [&'static str])> {
    if portable {
        vec![
            (paths::data_home().join("nohrs"), paths::data_dir(), &[][..]),
            (
                paths::state_home().join("nohrs"),
                paths::state_dir(),
                &["logs"][..],
            ),
        ]
    } else {
        vec![
            (
                paths::portable_config_dir().join("data"),
                paths::data_dir(),
                &[][..],
            ),
            (
                paths::portable_config_dir().join("state"),
                paths::state_dir(),
                &["logs"][..],
            ),
        ]
    }
}

fn copy_missing_files(source: &Path, target: &Path) -> io::Result<()> {
    if !source.is_dir() || source == target {
        return Ok(());
    }
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let destination = target.join(entry.file_name());
            if !destination.exists() {
                fs::copy(entry.path(), destination)?;
            }
        }
    }
    Ok(())
}
/// Where the other profile keeps its logs, if it is reachable.
fn other_log_directory(portable: bool) -> PathBuf {
    if portable {
        #[cfg(windows)]
        {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(Path::to_path_buf))
                .unwrap_or_default()
        }
        #[cfg(not(windows))]
        {
            paths::state_home().join("nohrs").join("logs")
        }
    } else {
        paths::portable_config_dir().join("logs")
    }
}

/// Carries the crash record of the other profile over, and nothing else: the
/// rolling files are history, not state, and used to be copied by the hundred of
/// megabytes.
fn migrate_panic_log(portable: bool) -> io::Result<()> {
    copy_panic_log(&other_log_directory(portable), &paths::log_dir())
}

fn copy_panic_log(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    let from = source.join("panic.log");
    let to = target.join("panic.log");
    if !to.exists() && from.is_file() {
        fs::copy(&from, &to)?;
    }
    Ok(())
}

fn copy_missing_tree(source: &Path, target: &Path, skip: &[&str]) -> io::Result<()> {
    if !source.is_dir() || source == target {
        return Ok(());
    }
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip.iter().any(|skipped| name.to_str() == Some(skipped)) {
            continue;
        }
        let source_path = entry.path();
        let target_path = target.join(&name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_missing_tree(&source_path, &target_path, skip)?;
        } else if file_type.is_file() && !target_path.exists() {
            fs::copy(source_path, target_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
// Tests build fixture files; there is no UI thread here.
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn copies_missing_files_recursively_without_overwriting() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("nested")).unwrap();
        fs::create_dir_all(target.path().join("nested")).unwrap();
        fs::write(source.path().join("nested/state.redb"), b"new").unwrap();
        fs::write(target.path().join("nested/state.redb"), b"existing").unwrap();
        fs::write(source.path().join("new-cache"), b"cache").unwrap();

        copy_missing_tree(source.path(), target.path(), &[]).unwrap();

        assert_eq!(
            fs::read(target.path().join("nested/state.redb")).unwrap(),
            b"existing"
        );
        assert_eq!(fs::read(target.path().join("new-cache")).unwrap(), b"cache");
    }

    #[test]
    fn the_state_copy_leaves_the_logs_directory_behind() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("logs")).unwrap();
        fs::write(
            source.path().join("logs/nohrs.log.2026-01-01"),
            vec![b'x'; 4096],
        )
        .unwrap();
        fs::write(source.path().join("batch-rename.json"), b"state").unwrap();

        copy_missing_tree(source.path(), target.path(), &["logs"]).unwrap();

        assert!(
            !target.path().join("logs").exists(),
            "the log history is not state and is not copied"
        );
        assert_eq!(
            fs::read(target.path().join("batch-rename.json")).unwrap(),
            b"state"
        );
    }

    #[test]
    fn only_the_panic_log_is_migrated() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::write(source.path().join("panic.log"), b"crash").unwrap();
        fs::write(source.path().join("nohrs.log.2026-01-01"), b"history").unwrap();

        copy_panic_log(source.path(), target.path()).unwrap();

        assert_eq!(fs::read(target.path().join("panic.log")).unwrap(), b"crash");
        assert!(!target.path().join("nohrs.log.2026-01-01").exists());
    }

    #[test]
    fn the_cache_is_not_part_of_the_migration() {
        for portable in [true, false] {
            let targets: Vec<PathBuf> = migration_roots(portable)
                .into_iter()
                .map(|(_, target, _)| target)
                .collect();
            assert!(
                targets.iter().all(
                    |target| target.file_name().and_then(|name| name.to_str()) != Some("cache")
                ),
                "the cache is rebuilt, not migrated: {targets:?}"
            );
        }
    }
}
