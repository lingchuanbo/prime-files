//! `noh quick-launch` (`noh ql`): the terminal side of the Quick Launch window
//! (docs/QuickDock-技术实现方案与边界约定.md §17.1 C).
//!
//! The commands work on the same `quicklaunch.toml` the GUI does, resolved
//! through `nohrs_core::config::paths::quick_launch_file()` so that portable
//! mode selects the same file. Parsing, validation and rendering come from
//! `nohrs_core::config::quick_launch`; replacing the file goes through
//! `nohrs_services::fs::write_atomically`, the call the GUI saves with, so a
//! running GUI reloads either the old contents or the new ones and never a
//! half-written file (§5.2).
//!
//! `state.redb` is never opened: a running GUI holds it open (§17.1 C). A
//! `remove` therefore leaves its usage record behind, which the GUI drops on
//! its next load (§17.1 E).
//!
//! A file that does not parse is never written back. The commands that would
//! change it print the reason and exit with code 2: rewriting a repaired
//! document would silently drop whatever this build cannot represent (§4.2,
//! §17.1 C).

// `clippy.toml` bans the synchronous `std::fs` helpers so that blocking IO never
// reaches the GPUI foreground thread. `noh` is a one-shot process with no such
// thread: reading this file *is* the command. Writing still goes through
// `nohrs_services::fs::write_atomically`.
#![allow(clippy::disallowed_methods)]

use std::borrow::Cow;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use nohrs_core::config::quick_launch::{
    self as model, MAX_NAME_CHARS, MAX_SLOT, QuickLaunchConfig,
};
use nohrs_services::launch::{ExpandContext, Health, LaunchRegistry};
#[cfg(windows)]
use nohrs_services::launch::{LaunchMode, LaunchOutcome, LaunchRequest};

/// Exit code of a command that could not do what was asked.
const FAILURE: u8 = 1;

/// Exit code of a command refused because `quicklaunch.toml` does not parse, or
/// was written by a newer build (§17.1 C). The file is left exactly as it was.
const UNREADABLE_CONFIG: u8 = 2;

/// The file `noh quick-launch` was asked to act on.
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// List the groups and items in `quicklaunch.toml`.
    List(ListArgs),
    /// Add an existing file or folder to a group.
    Add(AddArgs),
    /// Remove the item in a group's slot.
    Remove(RemoveArgs),
    /// Start the item in a group's slot.
    Run(RunArgs),
    /// Check that every item points at something that exists.
    Check(CheckArgs),
}

/// Flags for `noh quick-launch list`.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct ListArgs {
    /// Print the file's contents as one JSON object instead of text.
    #[arg(long)]
    pub json: bool,
}

/// Operands and flags for `noh quick-launch add`.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct AddArgs {
    /// File or folder to add.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Group id to add to (default: the first group).
    #[arg(long, value_name = "ID")]
    pub group: Option<String>,

    /// Name shown on the tile (default: the file name without its extension).
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Slot to occupy (default: the first free one). An occupied slot is
    /// refused rather than taken over.
    #[arg(long, value_name = "N")]
    pub slot: Option<u32>,

    /// Arguments handed to an application.
    #[arg(long, value_name = "ARGS")]
    pub args: Option<String>,

    /// Start the item elevated.
    #[arg(long)]
    pub admin: bool,
}

/// Operands for `noh quick-launch remove`.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct RemoveArgs {
    /// Group id the item belongs to.
    #[arg(value_name = "GROUP")]
    pub group: String,

    /// Slot of the item within the group.
    #[arg(value_name = "SLOT")]
    pub slot: u32,
}

/// Operands and flags for `noh quick-launch run`.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct RunArgs {
    /// Group id the item belongs to.
    #[arg(value_name = "GROUP")]
    pub group: String,

    /// Slot of the item within the group.
    #[arg(value_name = "SLOT")]
    pub slot: u32,

    /// Start the item elevated, whatever the file says.
    #[arg(long)]
    pub admin: bool,
}

/// Flags for `noh quick-launch check`.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct CheckArgs {
    /// Print one JSON object describing every item instead of text.
    #[arg(long)]
    pub json: bool,
}

/// One `noh quick-launch` invocation: the file it works on and where its two
/// output streams go.
///
/// The path is injected rather than resolved here so that the tests can point
/// it at a temporary directory; `main` passes the real
/// `nohrs_core::config::paths::quick_launch_file()`.
pub struct Session<'a> {
    path: &'a Path,
    output: &'a mut dyn Write,
    errors: &'a mut dyn Write,
}

impl<'a> Session<'a> {
    /// A session working on `path`, with results going to `output` and
    /// diagnostics to `errors`.
    pub fn new(path: &'a Path, output: &'a mut dyn Write, errors: &'a mut dyn Write) -> Self {
        Self {
            path,
            output,
            errors,
        }
    }

    /// Run `command`, returning the process exit code.
    pub fn run(mut self, command: &Command) -> io::Result<u8> {
        match command {
            Command::List(args) => self.list(args),
            Command::Add(args) => self.add(args),
            Command::Remove(args) => self.remove(args),
            Command::Run(args) => self.run_item(args),
            Command::Check(args) => self.check(args),
        }
    }

    fn list(&mut self, args: &ListArgs) -> io::Result<u8> {
        let parsed = match load(self.path) {
            Ok(parsed) => parsed,
            Err(error) => return self.report_load_error(&error),
        };
        self.report_warnings(&parsed)?;
        let config = config_or_default(&parsed);

        if args.json {
            let document = serde_json::json!({
                "path": self.path.display().to_string(),
                "groups": config
                    .groups
                    .iter()
                    .map(|group| {
                        serde_json::json!({
                            "id": group.id,
                            "items": group
                                .items
                                .iter()
                                .map(|item| {
                                    serde_json::json!({
                                        "slot": item.slot,
                                        "type": item.kind,
                                        "name": item.name,
                                        "path": item.path,
                                    })
                                })
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect::<Vec<_>>(),
            });
            let text = serde_json::to_string(&document).map_err(io::Error::other)?;
            writeln!(self.output, "{text}")?;
            return Ok(0);
        }

        // The line the user needs to know which file is in effect, before
        // anything it contains (§17.1 C).
        writeln!(self.output, "{}", self.path.display())?;
        for group in &config.groups {
            writeln!(self.output, "{}", group.id)?;
            for item in &group.items {
                writeln!(
                    self.output,
                    "  {}\t{}\t{}\t{}",
                    item.slot,
                    item.kind,
                    item.name,
                    item.path.as_deref().unwrap_or("-")
                )?;
            }
        }
        Ok(0)
    }

    fn add(&mut self, args: &AddArgs) -> io::Result<u8> {
        let parsed = match load(self.path) {
            Ok(parsed) => parsed,
            Err(error) => return self.report_load_error(&error),
        };
        self.report_warnings(&parsed)?;
        // Everything below changes this copy; the file is only replaced once
        // all the checks have passed, so a refusal leaves it untouched.
        let mut config = match parsed {
            Some(parsed) => parsed.config,
            None => QuickLaunchConfig::with_default_group(),
        };

        // Stored absolute: when the item is launched, a relative path is
        // resolved against the program folder (§7), which is not the folder the
        // user typed the command in.
        let path = match std::path::absolute(&args.path) {
            Ok(path) => path,
            Err(error) => {
                return self.fail(format_args!("{}: {error}", args.path.display()));
            }
        };
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => return self.fail(format_args!("{}: {error}", path.display())),
        };

        let group_index = match &args.group {
            Some(id) => match config.groups.iter().position(|group| &group.id == id) {
                Some(index) => index,
                None => return self.fail(format_args!("there is no group {id:?}")),
            },
            None if config.groups.is_empty() => {
                return self.fail("the file has no groups; create one in LumaFiles first");
            }
            None => 0,
        };

        let mut item = model::item_for_path(&path, 0, metadata.is_dir());
        if let Some(name) = &args.name {
            item.name = name.clone();
        }
        item.name = item.name.trim().to_string();
        if item.name.is_empty() {
            return self.fail("the item would have no name; pass --name");
        }
        if item.name.chars().count() > MAX_NAME_CHARS {
            item.name = item.name.chars().take(MAX_NAME_CHARS).collect();
            writeln!(
                self.errors,
                "warning: the name was cut to {MAX_NAME_CHARS} characters"
            )?;
        }
        if let Some(arguments) = &args.args {
            item.args = Some(arguments.clone());
        }
        item.admin = args.admin;

        let group = &config.groups[group_index];
        let slot = match args.slot {
            Some(slot) => {
                if slot > MAX_SLOT {
                    return self.fail(format_args!(
                        "slot {slot} is past the maximum of {MAX_SLOT}"
                    ));
                }
                if let Some(occupied) = group.items.iter().find(|item| item.slot == slot) {
                    return self.fail(format_args!(
                        "slot {slot} in group {:?} is already occupied by {:?}",
                        group.id, occupied.name
                    ));
                }
                slot
            }
            None => match first_free_slot(group) {
                Some(slot) => slot,
                None => return self.fail(format_args!("group {:?} has no free slot", group.id)),
            },
        };
        item.slot = slot;

        let name = item.name.clone();
        let group_id = group.id.clone();
        config.groups[group_index].items.push(item);
        if !self.store(&config)? {
            return Ok(FAILURE);
        }
        writeln!(
            self.output,
            "added {name:?} to group {group_id:?} in slot {slot}"
        )?;
        Ok(0)
    }

    fn remove(&mut self, args: &RemoveArgs) -> io::Result<u8> {
        let parsed = match load(self.path) {
            Ok(parsed) => parsed,
            Err(error) => return self.report_load_error(&error),
        };
        self.report_warnings(&parsed)?;
        let Some(parsed) = parsed else {
            return self.fail(format_args!(
                "there is no configuration file at {}",
                self.path.display()
            ));
        };
        let mut config = parsed.config;

        let Some(group_index) = config
            .groups
            .iter()
            .position(|group| group.id == args.group)
        else {
            return self.fail(format_args!("there is no group {:?}", args.group));
        };
        let Some(item_index) = config.groups[group_index]
            .items
            .iter()
            .position(|item| item.slot == args.slot)
        else {
            return self.fail(format_args!(
                "group {:?} has no item in slot {}",
                args.group, args.slot
            ));
        };
        let item = config.groups[group_index].items.remove(item_index);
        if !self.store(&config)? {
            return Ok(FAILURE);
        }
        writeln!(
            self.output,
            "removed {:?} from group {:?} in slot {}",
            item.name, args.group, args.slot
        )?;
        Ok(0)
    }

    #[cfg(windows)]
    fn run_item(&mut self, args: &RunArgs) -> io::Result<u8> {
        let item = match self.item_for(&args.group, args.slot)? {
            Ok(item) => item,
            Err(code) => return Ok(code),
        };
        let mode = if args.admin {
            LaunchMode::Admin
        } else {
            LaunchMode::Normal
        };
        let request = LaunchRequest {
            mode,
            on_running: item.on_running,
            expand: ExpandContext::current(),
        };
        let registry = LaunchRegistry::with_builtins();
        // A console process starts with no COM apartment and the Shell is COM
        // (§8.4); the guard releases it when this command ends.
        let _apartment = match com::Apartment::enter() {
            Ok(apartment) => apartment,
            Err(error) => return self.fail(format_args!("could not initialize COM: {error}")),
        };
        match registry.launch(&item.item, &request) {
            Ok(LaunchOutcome::OpenFolder(folder)) => {
                self.open_in_luma_files(&folder, &item.item.name)
            }
            Ok(LaunchOutcome::Started) => {
                writeln!(self.output, "started {:?}", item.item.name)?;
                Ok(0)
            }
            Ok(LaunchOutcome::Activated) => {
                writeln!(self.output, "activated {:?}", item.item.name)?;
                Ok(0)
            }
            // The user answered "no" to the UAC prompt: a decision, not a
            // failure (§8.2).
            Ok(LaunchOutcome::Cancelled) => {
                writeln!(self.output, "cancelled: the elevation prompt was declined")?;
                Ok(0)
            }
            Err(error) => self.fail(format_args!("{:?}: {error}", item.item.name)),
        }
    }

    #[cfg(not(windows))]
    fn run_item(&mut self, _args: &RunArgs) -> io::Result<u8> {
        writeln!(self.output, "quick-launch run is only supported on Windows")?;
        Ok(FAILURE)
    }

    /// Opens `folder` with the `LumaFiles.exe` beside `noh.exe`.
    ///
    /// The services layer hands a folder back instead of starting it (§8.1),
    /// and the only program that can open one as a tab is LumaFiles. The
    /// installer puts both executables in the same directory; a running
    /// instance takes the path from the new process, so the folder appears in
    /// the window the user already has open.
    #[cfg(windows)]
    fn open_in_luma_files(&mut self, folder: &Path, name: &str) -> io::Result<u8> {
        let executable = std::env::current_exe().ok().and_then(|executable| {
            executable
                .parent()
                .map(|directory| directory.join("LumaFiles.exe"))
        });
        let Some(executable) = executable.filter(|executable| executable.is_file()) else {
            return self
                .fail("LumaFiles.exe was not found next to noh.exe, so a folder cannot be opened");
        };
        if let Err(error) = std::process::Command::new(&executable).arg(folder).spawn() {
            return self.fail(format_args!(
                "could not start {}: {error}",
                executable.display()
            ));
        }
        writeln!(self.output, "opened {name:?} in LumaFiles")?;
        Ok(0)
    }

    fn check(&mut self, args: &CheckArgs) -> io::Result<u8> {
        let parsed = match load(self.path) {
            Ok(parsed) => parsed,
            Err(error) => return self.report_load_error(&error),
        };
        self.report_warnings(&parsed)?;
        let config = config_or_default(&parsed);
        let registry = LaunchRegistry::with_builtins();
        let context = ExpandContext::current();

        let mut healthy = true;
        let mut entries = Vec::new();
        for group in &config.groups {
            for item in &group.items {
                // The same judgement the window's health check makes, because
                // it is the same registry and the same action (§12.1).
                let health = registry.health(item, &context);
                if health != Health::Ok {
                    healthy = false;
                }
                if args.json {
                    entries.push(serde_json::json!({
                        "group": group.id,
                        "slot": item.slot,
                        "type": item.kind,
                        "name": item.name,
                        "path": item.path,
                        "health": health_name(&health),
                        "reason": health_reason(&health),
                    }));
                    continue;
                }
                write!(
                    self.output,
                    "{}\t{}/{}\t{}\t{}\t{}",
                    health_name(&health),
                    group.id,
                    item.slot,
                    item.kind,
                    item.name,
                    item.path.as_deref().unwrap_or("-")
                )?;
                if let Some(reason) = health_reason(&health) {
                    write!(self.output, "\t{reason}")?;
                }
                writeln!(self.output)?;
            }
        }

        if args.json {
            let document = serde_json::json!({
                "path": self.path.display().to_string(),
                "items": entries,
            });
            let text = serde_json::to_string(&document).map_err(io::Error::other)?;
            writeln!(self.output, "{text}")?;
        }
        Ok(if healthy { 0 } else { FAILURE })
    }

    /// The item in `group`/`slot`, or the exit code to stop with. The reason is
    /// printed before the code is returned.
    #[cfg(windows)]
    fn item_for(&mut self, group: &str, slot: u32) -> io::Result<Result<RunTarget, u8>> {
        let parsed = match load(self.path) {
            Ok(parsed) => parsed,
            Err(error) => return Ok(Err(self.report_load_error(&error)?)),
        };
        self.report_warnings(&parsed)?;
        let Some(parsed) = parsed else {
            return Ok(Err(self.fail(format_args!(
                "there is no configuration file at {}",
                self.path.display()
            ))?));
        };
        let config = parsed.config;
        let Some(found) = config.groups.iter().find(|candidate| candidate.id == group) else {
            return Ok(Err(self.fail(format_args!("there is no group {group:?}"))?));
        };
        let Some(item) = found.items.iter().find(|item| item.slot == slot) else {
            return Ok(Err(self.fail(format_args!(
                "group {group:?} has no item in slot {slot}"
            ))?));
        };
        Ok(Ok(RunTarget {
            item: item.clone(),
            on_running: config.window.on_running,
        }))
    }

    /// Renders `config` and replaces the file with it atomically. `false` means
    /// the write failed and the reason has been printed.
    fn store(&mut self, config: &QuickLaunchConfig) -> io::Result<bool> {
        let text = match model::to_toml(config) {
            Ok(text) => text,
            Err(error) => {
                writeln!(
                    self.errors,
                    "noh quick-launch: could not render the configuration: {error}"
                )?;
                return Ok(false);
            }
        };
        match nohrs_services::fs::write_atomically(self.path, &text) {
            Ok(_) => Ok(true),
            Err(error) => {
                writeln!(
                    self.errors,
                    "noh quick-launch: could not write {}: {error}",
                    self.path.display()
                )?;
                Ok(false)
            }
        }
    }

    fn report_load_error(&mut self, error: &LoadError) -> io::Result<u8> {
        writeln!(
            self.errors,
            "noh quick-launch: {}: {error}",
            self.path.display()
        )?;
        Ok(error.exit_code())
    }

    fn report_warnings(&mut self, parsed: &Option<model::Parsed>) -> io::Result<()> {
        if let Some(parsed) = parsed {
            for warning in &parsed.warnings {
                writeln!(self.errors, "warning: {warning}")?;
            }
        }
        Ok(())
    }

    fn fail(&mut self, message: impl fmt::Display) -> io::Result<u8> {
        writeln!(self.errors, "noh quick-launch: {message}")?;
        Ok(FAILURE)
    }
}

/// The item `run` resolved, with the window setting its launch needs.
#[cfg(windows)]
struct RunTarget {
    item: model::Item,
    on_running: model::OnRunning,
}

/// The file's model, or the one the GUI shows when there is no file at all
/// (§4.2). The borrowed case is the normal one; the owned one keeps `list` and
/// `check` meaningful before the first `add` creates the file.
fn config_or_default(parsed: &Option<model::Parsed>) -> Cow<'_, QuickLaunchConfig> {
    match parsed {
        Some(parsed) => Cow::Borrowed(&parsed.config),
        None => Cow::Owned(QuickLaunchConfig::with_default_group()),
    }
}

/// The lowest slot no item of `group` occupies.
fn first_free_slot(group: &model::Group) -> Option<u32> {
    (0..=MAX_SLOT).find(|slot| group.items.iter().all(|item| item.slot != *slot))
}

/// The one-word state [`Health`] reports, shared by the text and JSON output.
fn health_name(health: &Health) -> &'static str {
    match health {
        Health::Ok => "ok",
        Health::Missing => "missing",
        Health::Invalid(_) => "invalid",
        Health::Unknown => "unknown",
    }
}

/// Why `health` is not [`Health::Ok`], when it says why.
fn health_reason(health: &Health) -> Option<&str> {
    match health {
        Health::Invalid(reason) => Some(reason),
        _ => None,
    }
}

/// What one read of the file found.
enum LoadError {
    /// Reading failed for a reason other than the file not existing.
    Read(io::Error),
    /// The text is not a document this build understands (§4.2).
    Parse(model::QuickLaunchError),
}

impl LoadError {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Read(_) => FAILURE,
            // §17.1 C: a file that does not parse, or was written by a newer
            // build, must not be rewritten.
            Self::Parse(_) => UNREADABLE_CONFIG,
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "{error}"),
            Self::Parse(error) => write!(formatter, "{error}"),
        }
    }
}

/// Reads and parses the file. `None` means there is none, which the GUI and
/// this CLI both treat as the default group (§4.2).
fn load(path: &Path) -> Result<Option<model::Parsed>, LoadError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(LoadError::Read(error)),
    };
    let text = String::from_utf8(bytes).map_err(|error| {
        LoadError::Parse(model::QuickLaunchError::Parse {
            line: 1,
            column: 1,
            message: error.to_string(),
        })
    })?;
    // Windows PowerShell 5.1 and old Notepad save UTF-8 with a byte-order mark,
    // which TOML has no place for. The GUI strips it before parsing (§4.2 v2.3)
    // and a file the CLI refuses to read would otherwise stay uneditable from
    // here.
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    model::parse(text).map(Some).map_err(LoadError::Parse)
}

/// The STA apartment the one-shot `run` command puts the current thread in.
#[cfg(windows)]
mod com {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    use windows::core::Error;

    /// An apartment entered on this thread, left when it is dropped.
    pub(super) struct Apartment {
        initialized: bool,
    }

    impl Apartment {
        /// Enters an STA apartment.
        #[allow(unsafe_code)]
        pub(super) fn enter() -> Result<Self, Error> {
            // SAFETY: no arguments outlive the call, and a successful one is
            // balanced by `CoUninitialize` in `drop`.
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
                .ok()
                .map(|()| Self { initialized: true })
        }
    }

    impl Drop for Apartment {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            if !self.initialized {
                return;
            }
            // SAFETY: balances the successful `CoInitializeEx` in `enter`; every
            // object made through the apartment has been released by now.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn group(slots: &[u32]) -> model::Group {
        model::Group {
            id: "general".to_string(),
            name: "Geral".to_string(),
            items: slots
                .iter()
                .map(|slot| model::Item::new(*slot, "app", "Item"))
                .collect(),
        }
    }

    #[test]
    fn the_first_free_slot_is_the_lowest_unoccupied_one() {
        assert_eq!(first_free_slot(&group(&[])), Some(0));
        assert_eq!(first_free_slot(&group(&[0])), Some(1));
        assert_eq!(first_free_slot(&group(&[0, 2])), Some(1));
        assert_eq!(first_free_slot(&group(&[1, 0])), Some(2));
    }

    #[test]
    fn a_group_with_every_slot_taken_has_no_free_one() {
        let every_slot: Vec<u32> = (0..=MAX_SLOT).collect();
        assert_eq!(first_free_slot(&group(&every_slot)), None);
    }
}
