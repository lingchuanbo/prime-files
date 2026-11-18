//! On-disk lifecycle for `config.toml`: writing the default file, backing up an
//! existing one before overwriting, resetting, and the (P1-minimal) migration
//! scaffold.
//!
//! Migration support is intentionally a skeleton: only `schema_version = 1`
//! exists today, so [`needs_migration`] is always `false`. The
//! peek-version → backup → overwrite shape is in place so future bumps slot in
//! without reworking callers (config.md §7).

use super::settings::{CURRENT_SCHEMA_VERSION, Config};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Ensure the parent directory of `path` exists.
fn ensure_parent_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Write the default `config.toml` (with `#:schema` header) to `path`, creating
/// parent directories as needed. Overwrites any existing file — callers that
/// must preserve the old contents should [`backup`] first.
pub fn write_default(path: &Path) -> io::Result<()> {
    ensure_parent_dir(path)?;
    // Config is loaded/written synchronously at startup, before the GPUI
    // foreground loop runs, so blocking I/O here cannot stall rendering.
    #[allow(clippy::disallowed_methods)]
    std::fs::write(path, Config::default_toml_template())
}

/// Create the config file with defaults only if it does not already exist.
/// Returns `true` if a new file was written.
pub fn ensure_exists(path: &Path) -> io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    write_default(path)?;
    Ok(true)
}

/// Copy `path` to a timestamped sibling `config.toml.bak-<unix-seconds>` so a
/// destructive operation is recoverable. Returns the backup path, or `None` if
/// the source does not exist.
pub fn backup(path: &Path) -> io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    let backup_path = path.with_file_name(format!("{file_name}.bak-{seconds}"));
    std::fs::copy(path, &backup_path)?;
    Ok(Some(backup_path))
}

/// Reset configuration to defaults, backing up any existing file first. Returns
/// the backup path (if one was made).
pub fn reset(path: &Path) -> io::Result<Option<PathBuf>> {
    let backup_path = backup(path)?;
    write_default(path)?;
    Ok(backup_path)
}

/// Set `key` in `[section]` of the file at `path` to `value` (already in TOML
/// form, e.g. `"dark"` with its quotes, or `true`), leaving every other line,
/// comment and blank as it was. A missing key is added at the end of its
/// section, and a missing section at the end of the file.
///
/// Line-based rather than a parse and re-serialize: the file is the user's,
/// with comments that document each value, and a round trip through the
/// parser would throw them away. Keys are only looked up inside a plain
/// `[section]` header, so a same-named key in another table is never touched.
///
/// Callers run this off the UI thread; the hot-reload watcher then applies
/// the change as if the user had edited the file.
#[allow(clippy::disallowed_methods)]
pub fn set_value(path: &Path, section: &str, key: &str, value: &str) -> io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Config::default_toml_template(),
        Err(error) => return Err(error),
    };
    ensure_parent_dir(path)?;
    std::fs::write(path, with_value(&text, section, key, value))
}

#[allow(clippy::disallowed_methods)]
/// Sets a value in a sparse TOML file without writing default settings.
pub fn set_value_sparse(path: &Path, section: &str, key: &str, value: &str) -> io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    ensure_parent_dir(path)?;
    std::fs::write(path, with_value(&text, section, key, value))
}

/// Copies the active profile to the selected location and persists the mode.
pub fn set_portable_mode(enabled: bool) -> io::Result<()> {
    let portable = super::paths::portable_config_dir();
    let default = super::paths::config_dir();
    let source = super::paths::active_config_dir();
    let target = if enabled { &portable } else { &default };
    std::fs::create_dir_all(target)?;
    if source != *target {
        for name in ["config.toml", "appearance.toml", "search.toml"] {
            let from = source.join(name);
            let to = target.join(name);
            if from.exists() && !to.exists() {
                std::fs::copy(from, to)?;
            }
        }
    }
    if enabled {
        set_value(
            &portable.join("config.toml"),
            "storage",
            "portable_mode",
            "true",
        )?;
        remove_value(&default.join("config.toml"), "storage", "portable_mode")
    } else {
        set_value(
            &default.join("config.toml"),
            "storage",
            "portable_mode",
            "false",
        )?;
        remove_value(&portable.join("config.toml"), "storage", "portable_mode")
    }
}
/// Removes `key` from `[section]` of the file at `path`, so the built-in
/// default applies again; every other line stays as it was. A missing file,
/// section or key is not an error: the default already applies.
#[allow(clippy::disallowed_methods)]
pub fn remove_value(path: &Path, section: &str, key: &str) -> io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let without = without_value(&text, section, key);
    if without != text {
        std::fs::write(path, without)?;
    }
    Ok(())
}

/// The text of [`remove_value`], without the file.
fn without_value(text: &str, section: &str, key: &str) -> String {
    let header = format!("[{section}]");
    let mut in_section = false;
    let mut kept = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            in_section = line.trim() == header;
        } else if in_section
            && trimmed
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        {
            continue;
        }
        kept.push(line);
    }
    let mut out = kept.join(
        "
",
    );
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// The text of [`set_value`], without the file.
fn with_value(text: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let is_header = |line: &str| line.trim_start().starts_with('[');
    let Some(start) = lines.iter().position(|line| line.trim() == header) else {
        if lines.last().is_some_and(|line| !line.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(header);
        lines.push(format!("{key} = {value}"));
        return lines.join(
            "
",
        ) + "
";
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| is_header(line))
        .map_or(lines.len(), |offset| start + 1 + offset);
    let found = (start + 1..end).find(|&ix| {
        let line = lines[ix].trim_start();
        line.strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    });
    match found {
        Some(ix) => {
            // Keep the trailing comment, which documents the allowed values,
            // with the spacing that aligns it.
            let comment = comment_start(&lines[ix])
                .map(|at| lines[ix][at..].to_string())
                .unwrap_or_default();
            let indent: String = lines[ix]
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();
            lines[ix] = format!("{indent}{key} = {value}{comment}");
        }
        None => {
            // After the section's last non-blank line, not after the blank
            // that separates it from the next one.
            let mut at = end;
            while at > start + 1 && lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            lines.insert(at, format!("{key} = {value}"));
        }
    }
    lines.join(
        "
",
    ) + "
"
}

/// Where the trailing comment of a `key = value` line begins, counting the
/// whitespace before the `#`. A `#` inside a string (an accent like
/// `"#ff8800"`) is part of the value.
fn comment_start(line: &str) -> Option<usize> {
    let mut in_string = false;
    let mut escaped = false;
    for (at, character) in line.char_indices() {
        match character {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return Some(line[..at].trim_end().len()),
            _ => {}
        }
    }
    None
}

/// Whether a file declaring `version` requires migration before this build can
/// use it. P1 only knows v1, so this is always `false`; kept as the hook for
/// future schema bumps.
pub fn needs_migration(version: u64) -> bool {
    version < CURRENT_SCHEMA_VERSION
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn without_value_drops_only_that_key_in_that_section() {
        let text =
            "[keybindings]\ncopy = \"ctrl-shift-c\"\ncopy_path = \"ctrl-p\"\n\n[ui]\ncopy = 1\n";
        let out = without_value(text, "keybindings", "copy");
        assert_eq!(
            out,
            "[keybindings]\ncopy_path = \"ctrl-p\"\n\n[ui]\ncopy = 1\n"
        );
        assert_eq!(without_value(&out, "keybindings", "copy"), out);
    }

    #[test]
    fn with_value_replaces_in_its_section_and_keeps_the_comment() {
        let text = "[theme]
mode = \"system\"   # \"system\" | \"dark\"

[indexing]
mode = \"auto\"
";
        let out = with_value(text, "theme", "mode", "\"dark\"");
        assert_eq!(
            out,
            "[theme]
mode = \"dark\"   # \"system\" | \"dark\"

[indexing]
mode = \"auto\"
"
        );
    }

    #[test]
    fn with_value_adds_missing_keys_and_sections() {
        let text = "[ui]
default_sort = \"name\"

[explorer]
";
        let out = with_value(text, "ui", "show_hidden", "true");
        assert_eq!(
            out,
            "[ui]
default_sort = \"name\"
show_hidden = true

[explorer]
"
        );
        let out = with_value(&out, "theme", "mode", "\"light\"");
        assert!(out.ends_with(
            "[explorer]

[theme]
mode = \"light\"
"
        ));
        // A `#` inside a value is not a comment.
        let out = with_value(
            "[theme]
accent = \"#fff\" # hex
",
            "theme",
            "accent",
            "\"#000\"",
        );
        assert_eq!(
            out,
            "[theme]
accent = \"#000\" # hex
"
        );
        // A key that only shares a prefix is not the key.
        let out = with_value(
            "[ui]
show_hidden_x = 1
",
            "ui",
            "show_hidden",
            "true",
        );
        assert_eq!(
            out,
            "[ui]
show_hidden_x = 1
show_hidden = true
"
        );
    }

    #[test]
    fn set_value_round_trips_through_the_loader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_default(&path).unwrap();
        set_value(&path, "theme", "mode", "\"light\"").unwrap();
        let (config, diagnostics) = crate::config::load_from_path(&path);
        assert!(diagnostics.is_empty());
        assert_eq!(config.theme.mode, crate::config::ThemeMode::Light);
    }
    use tempfile::tempdir;

    #[test]
    fn write_default_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        write_default(&path).unwrap();
        assert!(path.exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("#:schema"));
    }

    #[test]
    fn ensure_exists_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(ensure_exists(&path).unwrap());
        std::fs::write(&path, "schema_version = 1\n").unwrap();
        // Second call must not overwrite the user's edits.
        assert!(!ensure_exists(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "schema_version = 1\n"
        );
    }

    #[test]
    fn reset_backs_up_then_restores_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "schema_version = 1\nmangled").unwrap();
        let backup_path = reset(&path).unwrap().unwrap();
        assert!(backup_path.exists());
        assert_eq!(
            std::fs::read_to_string(&backup_path).unwrap(),
            "schema_version = 1\nmangled"
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("#:schema")
        );
    }

    #[test]
    fn backup_of_missing_file_is_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(backup(&path).unwrap().is_none());
    }

    #[test]
    fn write_default_creates_deep_parents_and_overwrites() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a/b/c/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "stale").unwrap();
        write_default(&path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("#:schema"));
        assert!(!contents.contains("stale"));
    }

    #[test]
    fn ensure_exists_reports_creation_then_no_op() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(ensure_exists(&path).unwrap(), "first call creates the file");
        assert!(
            !ensure_exists(&path).unwrap(),
            "second call leaves it untouched"
        );
    }

    #[test]
    fn backup_preserves_original_contents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "schema_version = 1\naccent = \"teal\"").unwrap();
        let backup_path = backup(&path).unwrap().expect("backup created");
        assert_eq!(
            std::fs::read_to_string(&backup_path).unwrap(),
            "schema_version = 1\naccent = \"teal\""
        );
        // The original is left in place by `backup`.
        assert!(path.exists());
    }

    #[test]
    fn reset_without_existing_file_writes_default_and_no_backup() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let backup_path = reset(&path).unwrap();
        assert!(backup_path.is_none());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("#:schema")
        );
    }

    #[test]
    fn needs_migration_only_for_older_versions() {
        assert!(needs_migration(0));
        assert!(!needs_migration(CURRENT_SCHEMA_VERSION));
        assert!(!needs_migration(CURRENT_SCHEMA_VERSION + 1));
        assert!(!needs_migration(u64::MAX));
    }
}
