use std::fs;
use std::io;
use std::path::Path;

use super::paths;

/// Copies missing profile files into the selected storage root before startup.
/// Copies missing profile files into the selected storage root before startup.
pub fn prepare_storage() -> io::Result<()> {
    let portable = paths::portable_mode_enabled();
    let (config_source, config_target, roots) = if portable {
        (
            paths::config_dir(),
            paths::portable_config_dir(),
            vec![
                (paths::data_home().join("nohrs"), paths::data_dir()),
                (paths::cache_home().join("nohrs"), paths::cache_dir()),
                (paths::state_home().join("nohrs"), paths::state_dir()),
            ],
        )
    } else {
        (
            paths::portable_config_dir(),
            paths::config_dir(),
            vec![
                (paths::portable_config_dir().join("data"), paths::data_dir()),
                (
                    paths::portable_config_dir().join("cache"),
                    paths::cache_dir(),
                ),
                (
                    paths::portable_config_dir().join("state"),
                    paths::state_dir(),
                ),
            ],
        )
    };
    copy_missing_files(&config_source, &config_target)?;
    for (source, target) in roots {
        copy_missing_tree(&source, &target)?;
        fs::create_dir_all(target)?;
    }
    migrate_logs(portable)?;
    fs::create_dir_all(paths::runtime_dir())?;
    Ok(())
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
fn migrate_logs(portable: bool) -> io::Result<()> {
    let source = if portable {
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
    };
    fs::create_dir_all(paths::log_dir())?;
    let Ok(entries) = fs::read_dir(source) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str != "panic.log" && !nohrs_core_log_name(name_str) {
            continue;
        }
        let target = paths::log_dir().join(name);
        if !target.exists() && entry.file_type()?.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn nohrs_core_log_name(name: &str) -> bool {
    crate::telemetry::logging::is_log_file_name(name)
}

fn copy_missing_tree(source: &Path, target: &Path) -> io::Result<()> {
    if !source.is_dir() || source == target {
        return Ok(());
    }
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_missing_tree(&source_path, &target_path)?;
        } else if file_type.is_file() && !target_path.exists() {
            fs::copy(source_path, target_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

        copy_missing_tree(source.path(), target.path()).unwrap();

        assert_eq!(
            fs::read(target.path().join("nested/state.redb")).unwrap(),
            b"existing"
        );
        assert_eq!(fs::read(target.path().join("new-cache")).unwrap(), b"cache");
    }
}
