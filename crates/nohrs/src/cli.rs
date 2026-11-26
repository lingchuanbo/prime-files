//! Command-line interface: argument parsing plus the `nohrs config` subcommands
//! (config.md §8). With no subcommand the parsed global overrides are handed to
//! the GUI; with a `config` subcommand we run it and exit without opening a
//! window.

use clap::{Parser, Subcommand};
use nohrs_core::config::{self, ConfigOverride, ThemeMode};
use std::ffi::OsString;
use std::process::Command as ProcessCommand;

#[derive(Parser, Debug)]
#[command(
    name = "LumaFiles",
    version,
    about = "Launcher × Explorer file workspace",
    // A path and a subcommand are two different invocations; without this a
    // path named like a subcommand would be ambiguous.
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Override the theme mode for this run: system, light, or dark.
    #[arg(long, global = true, value_name = "MODE")]
    pub theme: Option<String>,

    /// Override the accent colour for this run (named colour or hex).
    #[arg(long, global = true, value_name = "COLOR")]
    pub accent: Option<String>,

    /// Start minimized, as at sign-in (Settings › Windows integration › Start
    /// with Windows). A running instance is left as it is.
    #[arg(long)]
    pub background: bool,

    /// A folder to open, or a file to open its folder with it selected. When
    /// Nohrs is already running it opens there, in a new tab.
    #[arg(value_name = "PATH", value_parser = parse_path)]
    pub path: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Open a folder, or select a file in its parent folder.
    Open {
        /// Folder or file to open.
        #[arg(value_parser = parse_path)]
        path: std::path::PathBuf,
    },
    /// Print the active explorer tab's folder; requires a running window.
    CurrentPath,
    /// Inspect or manage the user configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Open the launcher window on its own, without the explorer.
    Launcher,
    /// Open the quick launch panel, or hide it when it is already open.
    #[command(name = "quick_launcher")]
    QuickLauncher,
    /// Open the standalone search window, or hide it when it is already open.
    Search,
    /// Gives Listary's folders back to Explorer if they open here. Run by the
    /// uninstaller, which cannot edit Listary's JSON itself.
    #[command(hide = true)]
    ForgetListary,
}

/// The window `-quick_launcher` / `-search` asks for when it starts the program
/// instead of handing the request to an instance that is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupWindow {
    /// The quick launch panel.
    QuickLaunch,
    /// The standalone search window.
    Search,
}

impl Command {
    /// The window this command asks a *starting* program to open, if any.
    pub fn startup_window(&self) -> Option<StartupWindow> {
        match self {
            Self::QuickLauncher => Some(StartupWindow::QuickLaunch),
            Self::Search => Some(StartupWindow::Search),
            _ => None,
        }
    }
}

/// Rewrites the forms a script types — `-search`, `-quick_launcher`, and their
/// `--` and hyphen spellings — into the command names the parser knows.
///
/// `clap` reads `-search` as a cluster of short flags, so the spelling the
/// manual shows would otherwise be an argument error. A leading dash is how a
/// Windows program's switches are often written; a caller should not have to
/// know which layer reads it.
pub fn normalize_arguments(arguments: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    arguments
        .into_iter()
        .map(|argument| match argument.to_str() {
            Some(
                "-quick_launcher" | "--quick_launcher" | "-quick-launcher" | "--quick-launcher"
                | "quick_launcher",
            ) => OsString::from("quick_launcher"),
            Some("-search") | Some("--search") => OsString::from("search"),
            _ => argument,
        })
        .collect()
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the fully merged config (defaults < file < env < CLI) as JSON.
    Show,
    /// Open config.toml in $EDITOR (creating it with defaults if missing).
    Edit,
    /// Validate config.toml, reporting parse errors and warnings.
    Validate,
    /// Print the JSON Schema for config.toml to stdout.
    Schema,
    /// Print the full path to config.toml.
    Path,
    /// Reset config.toml to defaults, backing up any existing file first.
    Reset,
}

impl Cli {
    /// Build the CLI override layer from the global `--theme` / `--accent` flags.
    /// An invalid `--theme` value is warned about and ignored rather than
    /// aborting (it still leaves the file/env value in effect).
    pub fn overrides(&self) -> ConfigOverride {
        let mut over = ConfigOverride::default();
        if let Some(theme) = &self.theme {
            match ThemeMode::parse(theme.trim()) {
                Some(mode) => over.theme_mode = Some(mode),
                None => tracing::warn!("ignoring invalid --theme {theme:?}"),
            }
        }
        if let Some(accent) = &self.accent {
            over.theme_accent = Some(accent.clone());
        }
        over
    }
}

/// Run a `config` subcommand. Returns the process exit code.
pub fn run_config_command(action: &ConfigAction, cli: &Cli) -> i32 {
    let path = config::paths::config_file();
    match action {
        ConfigAction::Path => {
            println!("{}", path.display());
            0
        }
        ConfigAction::Schema => match config::json_schema_string() {
            Ok(schema) => {
                println!("{schema}");
                0
            }
            Err(error) => {
                eprintln!("failed to generate schema: {error}");
                1
            }
        },
        ConfigAction::Show => {
            let (mut merged, diagnostics) = config::load_from_path(&path);
            config::report_diagnostics(&diagnostics);
            merged.apply_override(&ConfigOverride::from_env());
            merged.apply_override(&cli.overrides());
            match merged.to_json() {
                Ok(json) => {
                    println!("{json}");
                    0
                }
                Err(error) => {
                    eprintln!("failed to serialize config: {error}");
                    1
                }
            }
        }
        ConfigAction::Validate => {
            let (_, diagnostics) = config::load_from_path(&path);
            let error = config::report_diagnostics(&diagnostics);
            if let Some(message) = error {
                eprintln!("invalid config ({}): {message}", path.display());
                1
            } else {
                let warnings = diagnostics.len();
                if warnings == 0 {
                    println!("{}: OK", path.display());
                } else {
                    println!(
                        "{}: OK with {warnings} warning(s) (see logs)",
                        path.display()
                    );
                }
                0
            }
        }
        ConfigAction::Reset => match config::reset(&path) {
            Ok(Some(backup)) => {
                println!(
                    "reset {}; backed up to {}",
                    path.display(),
                    backup.display()
                );
                0
            }
            Ok(None) => {
                println!("wrote default config to {}", path.display());
                0
            }
            Err(error) => {
                eprintln!("failed to reset config: {error}");
                1
            }
        },
        ConfigAction::Edit => edit_config(&path),
    }
}

fn edit_config(path: &std::path::Path) -> i32 {
    if let Err(error) = config::ensure_exists(path) {
        eprintln!("failed to create {}: {error}", path.display());
        return 1;
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    // `$EDITOR` often carries arguments (e.g. `code --wait`), so split on
    // whitespace rather than treating the whole string as a program path.
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    match ProcessCommand::new(program).args(parts).arg(path).status() {
        // A signal-terminated child reports no exit code; surface that as a
        // failure rather than masking it as success.
        Ok(status) if status.success() => 0,
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("failed to launch editor {editor:?}: {error}");
            1
        }
    }
}

/// Reads the `PATH` argument, repairing a quoted drive root.
///
/// Windows hands a folder over as `LumaFiles.exe "%1"`, and for a drive that is
/// `"C:\"`: by the rules every program parses its command line with, the
/// backslash escapes the closing quote, and the argument arrives as `C:"`.
/// A quote can never be part of a Windows path, so a trailing one is that
/// backslash.
fn parse_path(argument: &str) -> Result<std::path::PathBuf, std::convert::Infallible> {
    Ok(match argument.strip_suffix('"') {
        Some(root) => std::path::PathBuf::from(format!("{root}\\")),
        None => std::path::PathBuf::from(argument),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drive_root_whose_quote_was_escaped_is_repaired() {
        let cli = Cli::parse_from(["nohrs", "C:\""]);
        assert_eq!(cli.path, Some(std::path::PathBuf::from("C:\\")));
        let cli = Cli::parse_from(["nohrs", r"C:\Users"]);
        assert_eq!(cli.path, Some(std::path::PathBuf::from(r"C:\Users")));
    }

    #[test]
    fn external_commands_parse_without_breaking_path_arguments() {
        let cli = Cli::parse_from(["nohrs", "open", r"C:\My Files"]);
        assert!(
            matches!(cli.command, Some(Command::Open { path }) if path == std::path::Path::new(r"C:\My Files"))
        );
        let cli = Cli::parse_from(["nohrs", "current-path"]);
        assert!(matches!(cli.command, Some(Command::CurrentPath)));
        assert!(Cli::try_parse_from(["nohrs", "open"]).is_err());
    }

    #[test]
    fn the_dash_forms_of_the_window_commands_parse() {
        for argument in [
            "-quick_launcher",
            "--quick_launcher",
            "-quick-launcher",
            "--quick-launcher",
        ] {
            let args = normalize_arguments(["LumaFiles".into(), argument.into()]);
            let cli = Cli::parse_from(args);
            assert!(
                matches!(cli.command, Some(Command::QuickLauncher)),
                "{argument} should open the quick launch panel"
            );
            assert_eq!(
                cli.command.as_ref().and_then(Command::startup_window),
                Some(StartupWindow::QuickLaunch)
            );
        }
        for argument in ["-search", "--search"] {
            let args = normalize_arguments(["LumaFiles".into(), argument.into()]);
            let cli = Cli::parse_from(args);
            assert!(
                matches!(cli.command, Some(Command::Search)),
                "{argument} should open the standalone search"
            );
            assert_eq!(
                cli.command.as_ref().and_then(Command::startup_window),
                Some(StartupWindow::Search)
            );
        }
    }
}
