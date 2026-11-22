//! XDG-style path resolution for nohrs data locations.
//!
//! Per `docs/config.md` §1 we resolve `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` /
//! `$XDG_CACHE_HOME` directly, falling back to `~/.config`, `~/.local/share`,
//! and `~/.cache`. We deliberately do **not** use `dirs::config_dir()` and
//! friends: on macOS those return OS-native paths (`~/Library/...`) which
//! conflict with the dotfile layout nohrs uses on every platform. `dirs` is
//! used only to locate the home directory for the fallback.

use std::path::PathBuf;
use std::sync::OnceLock;

static ACTIVE_CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Application directory name used under each XDG base directory.
const APP_DIR: &str = "nohrs";

/// Resolve an XDG base directory: use the environment variable if it is set to
/// an absolute path (relative values are ignored per the XDG spec), otherwise
/// fall back to `~/<default_subdir>`. Falls back to the current directory if the
/// home directory cannot be determined, so callers always get a usable path.
fn xdg_base(env_var: &str, default_subdir: &str) -> PathBuf {
    if let Some(value) = std::env::var_os(env_var) {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return path;
        }
    }
    match dirs::home_dir() {
        Some(home) => home.join(default_subdir),
        None => PathBuf::from(default_subdir),
    }
}

/// `$XDG_CONFIG_HOME` or `~/.config`.
pub fn config_home() -> PathBuf {
    xdg_base("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_DATA_HOME` or `~/.local/share`.
pub fn data_home() -> PathBuf {
    xdg_base("XDG_DATA_HOME", ".local/share")
}

/// `$XDG_CACHE_HOME` or `~/.cache`.
pub fn cache_home() -> PathBuf {
    xdg_base("XDG_CACHE_HOME", ".cache")
}

/// `$XDG_STATE_HOME` or `~/.local/state`.
pub fn state_home() -> PathBuf {
    xdg_base("XDG_STATE_HOME", ".local/state")
}

/// The nohrs config directory (`.../nohrs`).
pub fn config_dir() -> PathBuf {
    config_home().join(APP_DIR)
}

/// Returns the `userdata` folder beside the executable.
pub fn portable_config_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
        .join("userdata")
}

/// Returns the active configuration directory, honoring the saved mode.
pub fn active_config_dir() -> PathBuf {
    ACTIVE_CONFIG_DIR.get_or_init(resolve_config_dir).clone()
}

/// Freezes the selected directory for this process and returns it.
pub fn freeze_active_config_dir() -> PathBuf {
    ACTIVE_CONFIG_DIR.get_or_init(resolve_config_dir).clone()
}

fn resolve_config_dir() -> PathBuf {
    if portable_mode_enabled() {
        portable_config_dir()
    } else {
        config_dir()
    }
}

fn portable_mode_from_configs(default: &std::path::Path, portable: &std::path::Path) -> bool {
    read_portable_mode(portable)
        .or_else(|| read_portable_mode(default))
        .unwrap_or(false)
}

fn read_portable_mode(path: &std::path::Path) -> Option<bool> {
    let contents = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&contents).ok()?;
    value.get("storage")?.get("portable_mode")?.as_bool()
}

/// The nohrs data directory (`.../nohrs`).
pub fn data_dir() -> PathBuf {
    if portable_mode_enabled() {
        portable_config_dir().join("data")
    } else {
        data_home().join(APP_DIR)
    }
}

/// Whether the saved profile selects portable storage.
pub fn portable_mode_enabled() -> bool {
    portable_mode_from_configs(
        &config_dir().join("config.toml"),
        &portable_config_dir().join("config.toml"),
    )
}
/// The nohrs cache directory (`.../nohrs`).
pub fn cache_dir() -> PathBuf {
    if portable_mode_enabled() {
        portable_config_dir().join("cache")
    } else {
        cache_home().join(APP_DIR)
    }
}

/// The nohrs state directory (`.../nohrs`).
pub fn state_dir() -> PathBuf {
    if portable_mode_enabled() {
        portable_config_dir().join("state")
    } else {
        state_home().join(APP_DIR)
    }
}

/// Directory holding rolling logs.
pub fn log_dir() -> PathBuf {
    if portable_mode_enabled() {
        return portable_config_dir().join("logs");
    }
    #[cfg(windows)]
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        return directory.to_path_buf();
    }
    state_dir().join("logs")
}

/// Directory for per-run coordination files such as the IPC endpoint.
pub fn runtime_dir() -> PathBuf {
    if portable_mode_enabled() {
        return portable_config_dir().join("runtime");
    }
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local).join(APP_DIR);
    }
    state_dir().join("runtime")
}

/// Migrates existing platform data into the selected profile before services open it.
pub fn prepare_storage() -> std::io::Result<()> {
    super::storage::prepare_storage()
}
/// Full path to `config.toml`.
pub fn config_file() -> PathBuf {
    active_config_dir().join("config.toml")
}

/// Returns the file containing appearance and color settings.
pub fn appearance_file() -> PathBuf {
    active_config_dir().join("appearance.toml")
}

/// Returns the file containing search settings.
pub fn search_file() -> PathBuf {
    active_config_dir().join("search.toml")
}

/// Full path to `quicklaunch.toml`, at the root of the active configuration
/// directory beside `config.toml`.
///
/// It is a file of its own rather than a `[quick_launch]` section: the launcher
/// rewrites it whenever an item is edited, and a separate file keeps the
/// hand-written comments in `config.toml` out of that path.
pub fn quick_launch_file() -> PathBuf {
    active_config_dir().join("quicklaunch.toml")
}

/// Where extracted quick-launch icons are cached.
///
/// Under the cache directory, not the data directory: the files are derived
/// from what is already on disk (`path` + size + mtime), so losing them costs
/// one re-extraction and nothing else.
pub fn quick_launch_icon_dir() -> PathBuf {
    cache_dir().join("quick_launch_icons")
}

/// Selects the split configuration file for a section and key.
pub fn config_file_for(section: &str, key: &str) -> PathBuf {
    let directory = active_config_dir();
    match (section, key) {
        ("theme", _) | ("space_map", _) | ("ui", "file_type_colors") => {
            directory.join("appearance.toml")
        }
        ("indexing", _) | ("search", _) => directory.join("search.toml"),
        _ => directory.join("config.toml"),
    }
}

#[cfg(test)]
// Rust 2024 made `std::env::{set_var, remove_var}` unsafe; these tests must
// mutate the process environment to exercise XDG path resolution.
#[allow(clippy::unwrap_used, unsafe_code)]
mod tests {
    use super::*;
    use crate::config::test_env::env_lock;

    #[test]
    fn portable_profile_is_detected_without_system_config() {
        let directory = tempfile::tempdir().unwrap();
        let system_config = directory.path().join("system/config.toml");
        let portable_config = directory.path().join("userdata/config.toml");
        std::fs::create_dir_all(portable_config.parent().unwrap()).unwrap();
        std::fs::write(&portable_config, "[storage]\nportable_mode = true\n").unwrap();
        std::fs::create_dir_all(system_config.parent().unwrap()).unwrap();
        std::fs::write(&system_config, "[storage]\nportable_mode = false\n").unwrap();

        assert!(portable_mode_from_configs(&system_config, &portable_config));
        std::fs::write(&portable_config, "[storage]\nportable_mode = false\n").unwrap();
        assert!(!portable_mode_from_configs(
            &system_config,
            &portable_config
        ));
    }

    #[test]
    fn config_file_lives_under_active_config_dir() {
        let _guard = env_lock();
        assert_eq!(config_file(), active_config_dir().join("config.toml"));
    }
    #[test]
    fn absolute_xdg_var_is_honoured() {
        let _guard = env_lock();
        // TODO: Audit that the environment access only happens in single-threaded code.
        // From `temp_dir` rather than a literal `/tmp/...`: on Windows that has no
        // drive, so it is not absolute and is rightly ignored.
        let absolute = std::env::temp_dir().join("xdg-test-abs");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &absolute) };
        let dir = config_dir();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(dir, absolute.join("nohrs"));
    }

    #[test]
    fn relative_xdg_var_is_ignored() {
        let _guard = env_lock();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "relative/path") };
        let home = config_home();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        // A relative value is ignored per the XDG spec, so we fall back to a home
        // (or cwd) based path, never the relative value itself.
        assert!(!home.ends_with("relative/path"));
    }

    #[test]
    fn each_home_honours_its_own_xdg_var() {
        let _guard = env_lock();
        // Absolute on every platform; see `absolute_xdg_var_is_honoured`.
        let temp = std::env::temp_dir();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", temp.join("cfg")) };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("XDG_DATA_HOME", temp.join("data")) };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("XDG_CACHE_HOME", temp.join("cache")) };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("XDG_STATE_HOME", temp.join("state")) };
        let (config, data, cache, state) = (config_home(), data_home(), cache_home(), state_home());
        for key in [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
        ] {
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { std::env::remove_var(key) };
        }
        assert_eq!(config, temp.join("cfg"));
        assert_eq!(data, temp.join("data"));
        assert_eq!(cache, temp.join("cache"));
        assert_eq!(state, temp.join("state"));
    }

    #[test]
    #[cfg(not(windows))]
    fn logs_live_under_the_state_directory() {
        let _guard = env_lock();
        if portable_mode_enabled() {
            assert_eq!(log_dir(), portable_config_dir().join("logs"));
        } else {
            assert_eq!(log_dir(), state_home().join("nohrs/logs"));
        }
    }

    #[test]
    #[cfg(windows)]
    fn logs_follow_the_selected_profile() {
        let executable_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let expected = if portable_mode_enabled() {
            executable_dir.join("userdata/logs")
        } else {
            executable_dir
        };
        assert_eq!(expected, log_dir());
    }
    #[test]
    fn app_dirs_follow_the_selected_profile() {
        let _guard = env_lock();
        assert!(config_dir().ends_with("nohrs"));
        if portable_mode_enabled() {
            let root = portable_config_dir();
            assert_eq!(config_file(), root.join("config.toml"));
            assert_eq!(data_dir(), root.join("data"));
            assert_eq!(cache_dir(), root.join("cache"));
            assert_eq!(state_dir(), root.join("state"));
        } else {
            assert!(data_dir().ends_with("nohrs"));
            assert!(cache_dir().ends_with("nohrs"));
            assert_eq!(config_file(), config_dir().join("config.toml"));
        }
    }

    #[test]
    fn the_quick_launch_file_sits_beside_config_toml() {
        let _guard = env_lock();
        assert_eq!(
            quick_launch_file(),
            active_config_dir().join("quicklaunch.toml")
        );
        // Portable mode relocates both files together, which is what makes a
        // copied `userdata` an installation.
        if portable_mode_enabled() {
            assert_eq!(
                quick_launch_file(),
                portable_config_dir().join("quicklaunch.toml")
            );
        }
    }

    #[test]
    fn quick_launch_icons_live_in_the_cache_directory() {
        let _guard = env_lock();
        assert_eq!(
            quick_launch_icon_dir(),
            cache_dir().join("quick_launch_icons")
        );
        if portable_mode_enabled() {
            assert_eq!(
                quick_launch_icon_dir(),
                portable_config_dir().join("cache/quick_launch_icons")
            );
        } else {
            assert!(quick_launch_icon_dir().ends_with("nohrs/quick_launch_icons"));
        }
    }
}
