//! The `quicklaunch.toml` model: parsing, validation, migration and writing.
//!
//! Pure logic, no file I/O (docs/QuickDock-技术实现方案与边界约定.md §4): the
//! caller decides where the file lives and when it is read or written, which is
//! what keeps the blocking filesystem work off the UI thread (§5.1).
//!
//! Every rule in §4.2 that repairs a document instead of rejecting it reports a
//! [`Warning`] rather than staying silent: the file is hand-written, so a value
//! that was quietly ignored is a value the author believes is in effect.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Version of the `quicklaunch.toml` schema this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Most groups a file may hold.
pub const MAX_GROUPS: usize = 64;

/// Most items a single group may hold.
pub const MAX_ITEMS_PER_GROUP: usize = 512;

/// Highest slot index an item may occupy.
pub const MAX_SLOT: u32 = 4095;

/// Most entries a `group` item may reference.
pub const MAX_GROUP_REFERENCES: usize = 16;

/// How deep `group` items may reference one another.
pub const MAX_GROUP_NESTING: usize = 8;

/// Longest group or item name, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Longest group or item id, in characters.
pub const MAX_ID_CHARS: usize = 32;

/// The summon chord a fresh file gets. It must not collide with the Launcher's
/// `Ctrl+Shift+Space` (§10).
pub const DEFAULT_HOTKEY: &str = "ctrl+alt+Space";

/// The id of the group a file with no groups is offered.
pub const DEFAULT_GROUP_ID: &str = "general";

/// The icon sizes an item may ask for, smallest first.
pub const ICON_SIZES: [u16; 6] = [16, 24, 32, 48, 64, 96];

/// How the window lays its items out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum View {
    /// A grid of icon tiles.
    #[default]
    Grid,
    /// A list of rows, one item per row.
    List,
}

/// What launching an application that is already running should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnRunning {
    /// Bring the existing window to the front.
    #[default]
    Activate,
    /// Start a second instance.
    New,
}

/// Window-level settings (`[window]` in the file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowSettings {
    /// The global summon chord, in `global-hotkey` syntax. Empty = not
    /// registered, so the window can only be summoned from inside LumaFiles.
    pub hotkey: String,
    /// Grid or list.
    pub view: View,
    /// Grid columns, clamped to `2..=16`.
    pub columns: u8,
    /// Grid cell edge in logical pixels, clamped to `64..=160`.
    pub cell_size: u16,
    /// Icon edge in logical pixels; any other value snaps to the nearest of
    /// [`ICON_SIZES`].
    pub icon_size: u16,
    /// Columns of the list view, clamped to `1..=3`.
    pub list_columns: u8,
    /// Hide the window when it loses focus.
    pub auto_hide: bool,
    /// Keep the window above ordinary windows.
    pub topmost: bool,
    /// Hide the window once an item has been launched.
    pub close_after_launch: bool,
    /// What to do when an application is already running.
    pub on_running: OnRunning,
    /// Keep the process alive after the main window is closed, so the summon
    /// chord still works (§15.0).
    pub keep_running: bool,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            hotkey: DEFAULT_HOTKEY.to_string(),
            view: View::Grid,
            columns: 8,
            cell_size: 96,
            icon_size: 48,
            list_columns: 2,
            auto_hide: true,
            topmost: false,
            close_after_launch: true,
            on_running: OnRunning::Activate,
            keep_running: true,
        }
    }
}

/// One named group of items (`[[group]]`).
//
// Not `Eq`: `Item::extra` is a `toml::Table`, whose values include floats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// Identifier used by `last_group` and by `group` item references:
    /// `^[a-z0-9_]{1,32}$`.
    pub id: String,
    /// What the group bar shows. Not translated: what the author typed is what
    /// is displayed (§14).
    pub name: String,
    /// The group's items (`[[group.item]]`).
    #[serde(default, rename = "item", skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<Item>,
}

/// One launchable entry (`[[group.item]]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    /// Position in the group's grid, `0..=4095`.
    pub slot: u32,
    /// `app` | `folder` | `file` | `url` | `uwp` | `command` | `group` |
    /// `plugin:<id>`. Unknown kinds are kept so that a file written by a newer
    /// build still round-trips.
    #[serde(rename = "type")]
    pub kind: String,
    /// What the tile shows. Not translated.
    pub name: String,
    /// Identifier a `group` item references. `^[a-z0-9_]{1,32}$`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Target path, possibly with `%ENV%` / `{portable}` / `{luma}` /
    /// `{config}` variables (§7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Command-line arguments handed to `ShellExecuteExW`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    /// Working directory; the executable's own folder when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Launch elevated (`runas`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub admin: bool,
    /// Custom icon: a `.ico` or `.png` path in this milestone (§9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Extra words the M2 filter also matches on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Item ids a `group` item launches, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// Free-form data for `command` / `plugin:*`; must be empty otherwise.
    #[serde(default, skip_serializing_if = "is_empty_table")]
    pub extra: toml::Table,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_empty_table(table: &toml::Table) -> bool {
    table.is_empty()
}

/// What an item's `type` means to the launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionKind {
    /// An executable or `.lnk` started through the Shell.
    App,
    /// A folder, opened as a new tab of LumaFiles by the pages layer.
    Folder,
    /// A document, opened with its associated program (M2).
    File,
    /// A `http` / `https` / `mailto` link.
    Url,
    /// A packaged (Store) application (M4).
    Uwp,
    /// A shell command (M4).
    Command,
    /// A bundle of other items of the same group (M4).
    Group,
    /// A plugin-provided action (never implemented here; the host is out of
    /// scope, §18).
    Plugin(String),
    /// A `type` this build does not know. The item is kept and displayed, and
    /// clicking it does nothing.
    Unknown(String),
}

impl Item {
    /// A minimal item of `kind` in `slot`: the fields every kind needs, with the
    /// rest left at their defaults.
    pub fn new(slot: u32, kind: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            slot,
            kind: kind.into(),
            name: name.into(),
            id: None,
            path: None,
            args: None,
            cwd: None,
            admin: false,
            icon: None,
            tags: Vec::new(),
            items: Vec::new(),
            extra: toml::Table::new(),
        }
    }

    /// What this item's `type` asks for.
    pub fn action_kind(&self) -> ActionKind {
        match self.kind.as_str() {
            "app" => ActionKind::App,
            "folder" => ActionKind::Folder,
            "file" => ActionKind::File,
            "url" => ActionKind::Url,
            "uwp" => ActionKind::Uwp,
            "command" => ActionKind::Command,
            "group" => ActionKind::Group,
            other => match other.strip_prefix("plugin:") {
                Some(id) if !id.is_empty() => ActionKind::Plugin(id.to_string()),
                _ => ActionKind::Unknown(other.to_string()),
            },
        }
    }

    /// Whether `extra` may carry data for this kind of item.
    fn carries_extra(&self) -> bool {
        matches!(
            self.action_kind(),
            ActionKind::Command | ActionKind::Plugin(_)
        )
    }
}

/// The item a path becomes (§12.3): a folder is a `folder`, an executable or
/// shortcut is an `app`, everything else a `file`.
///
/// `is_dir` comes from the caller: whether the path is a folder needs the disk,
/// and the window may not wait for a network path (M2 S1). Keeping the disk out of
/// this function is also what lets the CLI and the window agree on the rule.
pub fn item_for_path(path: &Path, slot: u32, is_dir: bool) -> Item {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
    let kind = if is_dir {
        "folder"
    } else if matches!(
        extension.as_deref(),
        Some("exe" | "lnk" | "com" | "bat" | "cmd")
    ) {
        "app"
    } else {
        "file"
    };
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .or_else(|| path.file_name().and_then(|name| name.to_str()))
        .unwrap_or_default()
        .to_string();
    let mut item = Item::new(slot, kind, name);
    item.path = Some(path.to_string_lossy().to_string());
    item
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuickLaunchConfig {
    /// Schema the file was written with.
    pub schema_version: u32,
    /// Window settings.
    pub window: WindowSettings,
    /// The groups, in the order the group bar shows them.
    #[serde(rename = "group", skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<Group>,
}

impl Default for QuickLaunchConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            window: WindowSettings::default(),
            groups: Vec::new(),
        }
    }
}

impl QuickLaunchConfig {
    /// The configuration to use when there is no file at all: one empty group
    /// named in the language in effect, which is not written to disk (§4.2).
    pub fn with_default_group() -> Self {
        Self {
            groups: vec![Group {
                id: DEFAULT_GROUP_ID.to_string(),
                name: crate::t!("Geral").to_string(),
                items: Vec::new(),
            }],
            ..Self::default()
        }
    }

    /// The group with this id, or the first group.
    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups
            .iter()
            .find(|group| group.id == id)
            .or_else(|| self.groups.first())
    }
}

/// One repaired-or-ignored detail of a document, in the author's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// `schema_version = 0` (or absent) was read as version 1.
    SchemaVersionZero,
    /// More groups than [`MAX_GROUPS`]; the rest were dropped.
    TooManyGroups {
        /// How many the file asked for.
        found: usize,
    },
    /// A group was dropped because its id is not `^[a-z0-9_]{1,32}$`.
    InvalidGroupId {
        /// The rejected id.
        id: String,
    },
    /// A second group used an id the first one already had.
    DuplicateGroupId {
        /// The repeated id.
        id: String,
    },
    /// A group was dropped because its name was empty.
    EmptyGroupName {
        /// The group's id.
        id: String,
    },
    /// A name longer than [`MAX_NAME_CHARS`] was cut.
    NameTruncated {
        /// The group or item the name belongs to.
        owner: String,
    },
    /// More items than [`MAX_ITEMS_PER_GROUP`]; the rest were dropped.
    TooManyItems {
        /// The group's id.
        group: String,
        /// How many it asked for.
        found: usize,
    },
    /// An item was dropped because its id is not `^[a-z0-9_]{1,32}$`.
    InvalidItemId {
        /// The group's id.
        group: String,
        /// The rejected id.
        id: String,
    },
    /// A second item of a group used an id the first one already had.
    DuplicateItemId {
        /// The group's id.
        group: String,
        /// The repeated id.
        id: String,
    },
    /// An item was dropped because its name was empty.
    EmptyItemName {
        /// The group's id.
        group: String,
        /// The item's slot.
        slot: u32,
    },
    /// A slot past [`MAX_SLOT`] was moved to the last valid one.
    SlotClamped {
        /// The group's id.
        group: String,
        /// The slot as written.
        found: u32,
    },
    /// A second item of a group used a slot the first one already had.
    DuplicateSlot {
        /// The group's id.
        group: String,
        /// The repeated slot.
        slot: u32,
    },
    /// A numeric setting outside its range was moved inside it.
    Clamped {
        /// The setting's path in the file, e.g. `window.columns`.
        field: &'static str,
        /// The value as written.
        found: i64,
        /// The value now in effect.
        used: i64,
    },
    /// `extra` was cleared because the item's kind does not accept it.
    ExtraCleared {
        /// The group's id.
        group: String,
        /// The item's slot.
        slot: u32,
    },
    /// A `group` item referenced something it cannot.
    ReferenceDropped {
        /// The group's id.
        group: String,
        /// The referencing item's id.
        item: String,
        /// The dropped reference.
        reference: String,
        /// Why it was dropped.
        reason: ReferenceProblem,
    },
}

/// Why a `group` item's reference was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceProblem {
    /// No item of the group has that id.
    Missing,
    /// The item referenced itself.
    Itself,
    /// Following the references would come back to the item.
    Cycle,
    /// Following the references would nest deeper than [`MAX_GROUP_NESTING`].
    TooDeep,
    /// The item already holds [`MAX_GROUP_REFERENCES`] valid references.
    TooMany,
}

impl std::fmt::Display for Warning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Warning::SchemaVersionZero => {
                write!(formatter, "schema_version = 0, read as {SCHEMA_VERSION}")
            }
            Warning::TooManyGroups { found } => write!(
                formatter,
                "{found} groups, only the first {MAX_GROUPS} are kept"
            ),
            Warning::InvalidGroupId { id } => {
                write!(
                    formatter,
                    "group {id:?} skipped: id must be ^[a-z0-9_]{{1,32}}$"
                )
            }
            Warning::DuplicateGroupId { id } => {
                write!(formatter, "group {id:?} repeated, the later one skipped")
            }
            Warning::EmptyGroupName { id } => {
                write!(formatter, "group {id:?} skipped: its name is empty")
            }
            Warning::NameTruncated { owner } => write!(
                formatter,
                "{owner}: name longer than {MAX_NAME_CHARS} characters, cut"
            ),
            Warning::TooManyItems { group, found } => write!(
                formatter,
                "group {group:?}: {found} items, only the first {MAX_ITEMS_PER_GROUP} are kept"
            ),
            Warning::InvalidItemId { group, id } => write!(
                formatter,
                "group {group:?}: item {id:?} skipped: id must be ^[a-z0-9_]{{1,32}}$"
            ),
            Warning::DuplicateItemId { group, id } => write!(
                formatter,
                "group {group:?}: item id {id:?} repeated, the later one skipped"
            ),
            Warning::EmptyItemName { group, slot } => write!(
                formatter,
                "group {group:?}: item in slot {slot} skipped: its name is empty"
            ),
            Warning::SlotClamped { group, found } => write!(
                formatter,
                "group {group:?}: slot {found} moved to {MAX_SLOT}"
            ),
            Warning::DuplicateSlot { group, slot } => write!(
                formatter,
                "group {group:?}: slot {slot} repeated, the later item skipped"
            ),
            Warning::Clamped { field, found, used } => {
                write!(formatter, "{field} = {found} out of range, using {used}")
            }
            Warning::ExtraCleared { group, slot } => write!(
                formatter,
                "group {group:?}: item in slot {slot} has extra data its type does not take, cleared"
            ),
            Warning::ReferenceDropped {
                group,
                item,
                reference,
                reason,
            } => {
                let reason = match reason {
                    ReferenceProblem::Missing => "no item has that id",
                    ReferenceProblem::Itself => "an item cannot reference itself",
                    ReferenceProblem::Cycle => "the references would form a cycle",
                    ReferenceProblem::TooDeep => "the references would nest too deeply",
                    ReferenceProblem::TooMany => "too many references",
                };
                write!(
                    formatter,
                    "group {group:?}: item {item:?} no longer references {reference:?}: {reason}"
                )
            }
        }
    }
}

/// Why a document could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum QuickLaunchError {
    /// The file is not valid TOML, or does not match the schema.
    #[error("line {line}, column {column}: {message}")]
    Parse {
        /// 1-based line of the offending text.
        line: usize,
        /// 1-based character column of the offending text.
        column: usize,
        /// What the parser said.
        message: String,
    },
    /// The file was written by a newer build. The caller must not write it
    /// back: doing so would throw away fields this build cannot represent.
    #[error("the file was written by a newer version (schema {found})")]
    UnsupportedVersion {
        /// The version the file declares.
        found: u32,
    },
    /// The model could not be rendered back to TOML.
    #[error("could not write the file: {message}")]
    Serialize {
        /// What the serializer said.
        message: String,
    },
}

/// A parsed file: the configuration with every repair applied, plus what was
/// repaired.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    /// The configuration after validation.
    pub config: QuickLaunchConfig,
    /// Everything the validation changed or dropped, in file order.
    pub warnings: Vec<Warning>,
}

/// Parses `text` and repairs it, reporting what was repaired.
///
/// A file written by a newer build is the one thing that is refused rather than
/// repaired: this build cannot know what the fields mean, and writing the file
/// back would delete them (§4.2).
pub fn parse(text: &str) -> Result<Parsed, QuickLaunchError> {
    let mut config: QuickLaunchConfig =
        toml::from_str(text).map_err(|error| parse_error(text, &error))?;
    let mut warnings = Vec::new();
    if config.schema_version == 0 {
        config.schema_version = SCHEMA_VERSION;
        warnings.push(Warning::SchemaVersionZero);
    }
    if config.schema_version > SCHEMA_VERSION {
        return Err(QuickLaunchError::UnsupportedVersion {
            found: config.schema_version,
        });
    }
    let config = migrate(config);
    let config = validate(config, &mut warnings);
    Ok(Parsed { config, warnings })
}

/// Renders `config` back to TOML.
///
/// Comments the author wrote are not preserved; the settings page warns about
/// that before offering to rewrite the file (§5.2).
pub fn to_toml(config: &QuickLaunchConfig) -> Result<String, QuickLaunchError> {
    toml::to_string(config).map_err(|error| QuickLaunchError::Serialize {
        message: error.to_string(),
    })
}

/// Brings an older schema up to [`SCHEMA_VERSION`].
///
/// Only version 1 exists so far, so this is the identity. It stays a separate
/// step so that a future version has an obvious place to migrate in.
pub fn migrate(config: QuickLaunchConfig) -> QuickLaunchConfig {
    config
}

/// Turns a `toml` error into a line and column of `text`.
fn parse_error(text: &str, error: &toml::de::Error) -> QuickLaunchError {
    let offset = error.span().map_or(0, |span| span.start).min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = before[line_start..].chars().count() + 1;
    QuickLaunchError::Parse {
        line,
        column,
        message: error.message().to_string(),
    }
}

/// Applies every repair in §4.2, in the order the document is read.
fn validate(mut config: QuickLaunchConfig, warnings: &mut Vec<Warning>) -> QuickLaunchConfig {
    validate_window(&mut config.window, warnings);

    if config.groups.len() > MAX_GROUPS {
        warnings.push(Warning::TooManyGroups {
            found: config.groups.len(),
        });
        config.groups.truncate(MAX_GROUPS);
    }

    let mut seen_groups: Vec<String> = Vec::with_capacity(config.groups.len());
    let mut groups = Vec::with_capacity(config.groups.len());
    for mut group in config.groups.drain(..) {
        if !is_valid_id(&group.id) {
            warnings.push(Warning::InvalidGroupId { id: group.id });
            continue;
        }
        if seen_groups.contains(&group.id) {
            warnings.push(Warning::DuplicateGroupId { id: group.id });
            continue;
        }
        group.name = group.name.trim().to_string();
        if group.name.is_empty() {
            warnings.push(Warning::EmptyGroupName { id: group.id });
            continue;
        }
        truncate_name(&mut group.name, &format!("group {:?}", group.id), warnings);
        seen_groups.push(group.id.clone());
        validate_items(&mut group, warnings);
        groups.push(group);
    }
    config.groups = groups;
    config
}

fn validate_window(window: &mut WindowSettings, warnings: &mut Vec<Warning>) {
    let mut clamp = |field: &'static str, value: i64, low: i64, high: i64| -> i64 {
        let used = value.clamp(low, high);
        if used != value {
            warnings.push(Warning::Clamped {
                field,
                found: value,
                used,
            });
        }
        used
    };

    let columns = clamp("window.columns", i64::from(window.columns), 2, 16);
    window.columns = u8::try_from(columns).unwrap_or(2);
    let cell_size = clamp("window.cell_size", i64::from(window.cell_size), 64, 160);
    window.cell_size = u16::try_from(cell_size).unwrap_or(96);
    let list_columns = clamp("window.list_columns", i64::from(window.list_columns), 1, 3);
    window.list_columns = u8::try_from(list_columns).unwrap_or(1);
    let icon_size = i64::from(window.icon_size);
    let snapped = nearest_icon_size(icon_size);
    if i64::from(snapped) != icon_size {
        warnings.push(Warning::Clamped {
            field: "window.icon_size",
            found: icon_size,
            used: i64::from(snapped),
        });
    }
    window.icon_size = snapped;
}

/// The entry of [`ICON_SIZES`] closest to `value`; ties go to the smaller one.
fn nearest_icon_size(value: i64) -> u16 {
    let mut best = ICON_SIZES[0];
    let mut best_distance = i64::MAX;
    for candidate in ICON_SIZES {
        let distance = (i64::from(candidate) - value).abs();
        if distance < best_distance {
            best = candidate;
            best_distance = distance;
        }
    }
    best
}

fn validate_items(group: &mut Group, warnings: &mut Vec<Warning>) {
    if group.items.len() > MAX_ITEMS_PER_GROUP {
        warnings.push(Warning::TooManyItems {
            group: group.id.clone(),
            found: group.items.len(),
        });
        group.items.truncate(MAX_ITEMS_PER_GROUP);
    }

    let mut seen_slots: Vec<u32> = Vec::with_capacity(group.items.len());
    let mut seen_ids: Vec<String> = Vec::with_capacity(group.items.len());
    let mut items = Vec::with_capacity(group.items.len());
    for mut item in group.items.drain(..) {
        if item.slot > MAX_SLOT {
            warnings.push(Warning::SlotClamped {
                group: group.id.clone(),
                found: item.slot,
            });
            item.slot = MAX_SLOT;
        }
        if seen_slots.contains(&item.slot) {
            warnings.push(Warning::DuplicateSlot {
                group: group.id.clone(),
                slot: item.slot,
            });
            continue;
        }
        if let Some(id) = &item.id {
            if !is_valid_id(id) {
                warnings.push(Warning::InvalidItemId {
                    group: group.id.clone(),
                    id: id.clone(),
                });
                continue;
            }
            if seen_ids.contains(id) {
                warnings.push(Warning::DuplicateItemId {
                    group: group.id.clone(),
                    id: id.clone(),
                });
                continue;
            }
        }
        item.name = item.name.trim().to_string();
        if item.name.is_empty() {
            warnings.push(Warning::EmptyItemName {
                group: group.id.clone(),
                slot: item.slot,
            });
            continue;
        }
        truncate_name(
            &mut item.name,
            &format!("group {:?} slot {}", group.id, item.slot),
            warnings,
        );
        if !item.carries_extra() && !item.extra.is_empty() {
            warnings.push(Warning::ExtraCleared {
                group: group.id.clone(),
                slot: item.slot,
            });
            item.extra = toml::Table::new();
        }
        seen_slots.push(item.slot);
        if let Some(id) = &item.id {
            seen_ids.push(id.clone());
        }
        items.push(item);
    }

    prune_references(&mut items, &group.id, warnings);
    group.items = items;
}

/// Removes the references a `group` item cannot have (§4.2).
///
/// Runs to a fixed point because dropping a reference can turn another one from
/// "part of a cycle" into a valid edge, and the document has to be left in a
/// state that loads the same way next time.
fn prune_references(items: &mut [Item], group: &str, warnings: &mut Vec<Warning>) {
    // One reference is dropped per round and the graph is rebuilt before the
    // next one is judged: a cycle makes every edge around it look invalid at
    // once, and dropping all of them would lose references that one break would
    // have saved.
    loop {
        let edges = reference_map(items);
        let mut changed = false;
        let mut restart = false;
        for item in items.iter_mut() {
            if item.action_kind() != ActionKind::Group {
                continue;
            }
            let own_id = item.id.clone();
            let mut kept: Vec<String> = Vec::with_capacity(item.items.len());
            for reference in std::mem::take(&mut item.items) {
                if kept.len() >= MAX_GROUP_REFERENCES {
                    changed = true;
                    drop_reference(
                        warnings,
                        group,
                        &own_id,
                        reference,
                        ReferenceProblem::TooMany,
                    );
                    continue;
                }
                let problem = match &own_id {
                    None => Some(ReferenceProblem::Missing),
                    Some(id) if *id == reference => Some(ReferenceProblem::Itself),
                    Some(_) if !edges.contains_key(&reference) => Some(ReferenceProblem::Missing),
                    Some(_) => match reference_depth(&edges, &reference, 1) {
                        None => Some(ReferenceProblem::Cycle),
                        Some(depth) if depth > MAX_GROUP_NESTING => Some(ReferenceProblem::TooDeep),
                        Some(_) => None,
                    },
                };
                match problem {
                    Some(reason) => {
                        drop_reference(warnings, group, &own_id, reference, reason);
                        changed = true;
                        restart = true;
                        break;
                    }
                    None => kept.push(reference),
                }
            }
            item.items = kept;
            if restart {
                break;
            }
        }
        if !changed {
            return;
        }
    }
}

/// Records that one reference of a `group` item was dropped.
fn drop_reference(
    warnings: &mut Vec<Warning>,
    group: &str,
    item: &Option<String>,
    reference: String,
    reason: ReferenceProblem,
) {
    warnings.push(Warning::ReferenceDropped {
        group: group.to_string(),
        item: item.clone().unwrap_or_else(|| "-".to_string()),
        reference,
        reason,
    });
}

/// Which item ids of a group can be referenced.
fn reference_map(items: &[Item]) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut edges = std::collections::BTreeMap::new();
    for item in items {
        if let Some(id) = &item.id {
            let targets = if item.action_kind() == ActionKind::Group {
                item.items.clone()
            } else {
                Vec::new()
            };
            edges.insert(id.clone(), targets);
        }
    }
    edges
}

/// How deep the chain from `id` goes, or `None` when it loops.
///
/// `depth` is the number of hops already taken, so the caller passing 1 bounds
/// the result by how many references may follow one another.
fn reference_depth(
    edges: &std::collections::BTreeMap<String, Vec<String>>,
    id: &str,
    depth: usize,
) -> Option<usize> {
    if depth > MAX_GROUP_NESTING + 1 {
        return None;
    }
    let Some(targets) = edges.get(id) else {
        return Some(depth);
    };
    let mut deepest = depth;
    for target in targets {
        if target == id {
            return None;
        }
        let found = reference_depth(edges, target, depth + 1)?;
        deepest = deepest.max(found);
    }
    Some(deepest)
}

/// `^[a-z0-9_]{1,32}$`.
fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_ID_CHARS
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn truncate_name(name: &mut String, owner: &str, warnings: &mut Vec<Warning>) {
    if name.chars().count() <= MAX_NAME_CHARS {
        return;
    }
    *name = name.chars().take(MAX_NAME_CHARS).collect();
    warnings.push(Warning::NameTruncated {
        owner: owner.to_string(),
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    /// §12.3: the kind of an item follows the metadata and the extension.
    #[test]
    fn an_item_takes_its_kind_from_the_path_and_the_metadata() {
        let program = item_for_path(Path::new("C:/x/programa.exe"), 4, false);
        assert_eq!(program.kind, "app");
        assert_eq!(program.name, "programa");
        assert_eq!(program.slot, 4);
        assert_eq!(
            item_for_path(Path::new("C:/x/atalho.lnk"), 0, false).kind,
            "app"
        );
        assert_eq!(
            item_for_path(Path::new("C:/x/notas.txt"), 0, false).kind,
            "file"
        );
        assert_eq!(
            item_for_path(Path::new("C:/x/notas.txt"), 0, false).name,
            "notas"
        );
        // The flag wins: something without an extension is a folder when the
        // caller says so.
        assert_eq!(
            item_for_path(Path::new("C:/x/Pasta"), 0, true).kind,
            "folder"
        );
        assert_eq!(
            item_for_path(Path::new("C:/x/Pasta"), 0, false).kind,
            "file"
        );
    }

    fn parse_ok(text: &str) -> Parsed {
        parse(text).expect("the document should parse")
    }

    fn item(slot: u32, kind: &str, name: &str) -> Item {
        Item::new(slot, kind, name)
    }

    #[test]
    fn an_empty_file_parses_to_the_defaults() {
        let parsed = parse_ok("");
        assert_eq!(
            parsed.config,
            QuickLaunchConfig {
                schema_version: SCHEMA_VERSION,
                window: WindowSettings::default(),
                groups: Vec::new(),
            }
        );
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn the_default_group_is_named_in_the_current_language() {
        let config = QuickLaunchConfig::with_default_group();
        assert_eq!(config.groups.len(), 1);
        assert_eq!(config.groups[0].id, DEFAULT_GROUP_ID);
        assert_eq!(config.groups[0].name, crate::t!("Geral"));
        assert!(config.groups[0].items.is_empty());
    }

    #[test]
    fn the_window_defaults_match_the_specification() {
        let window = WindowSettings::default();
        assert_eq!(window.hotkey, "ctrl+alt+Space");
        assert_eq!(window.view, View::Grid);
        assert_eq!(window.columns, 8);
        assert_eq!(window.cell_size, 96);
        assert_eq!(window.icon_size, 48);
        assert_eq!(window.list_columns, 2);
        assert!(window.auto_hide);
        assert!(!window.topmost);
        assert!(window.close_after_launch);
        assert_eq!(window.on_running, OnRunning::Activate);
        assert!(window.keep_running);
    }

    #[test]
    fn a_document_round_trips_through_toml() {
        let text = r#"
schema_version = 1

[window]
hotkey = "ctrl+alt+x"
view = "list"
columns = 4
cell_size = 80
icon_size = 32
list_columns = 3
auto_hide = false
topmost = true
close_after_launch = false
on_running = "new"
keep_running = false

[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "app"
name = "Editor"
path = "C:\\Windows\\notepad.exe"
args = "/A"
cwd = "C:\\Windows"
admin = true
icon = "C:\\icons\\editor.ico"
tags = ["texto", "notas"]

[[group.item]]
slot = 1
type = "folder"
name = "Projetos"
path = "{luma}\\projetos"

[[group]]
id = "dev"
name = "Dev"

[[group.item]]
slot = 0
type = "plugin:acme"
name = "Acme"
extra = { key = "value" }
"#;
        let parsed = parse_ok(text);
        assert_eq!(parsed.warnings, Vec::new(), "{:?}", parsed.warnings);
        let written = to_toml(&parsed.config).expect("the model should serialize");
        let reparsed = parse_ok(&written);
        assert_eq!(reparsed.config, parsed.config, "written as:\n{written}");
    }

    #[test]
    fn a_group_item_round_trips_its_references() {
        let mut group = Group {
            id: "general".to_string(),
            name: "Geral".to_string(),
            items: vec![item(0, "app", "A"), item(1, "app", "B")],
        };
        group.items[0].id = Some("a".to_string());
        group.items[1].id = Some("b".to_string());
        let mut bundle = item(2, "group", "Pacote");
        bundle.id = Some("bundle".to_string());
        bundle.items = vec!["a".to_string(), "b".to_string()];
        group.items.push(bundle);
        let config = QuickLaunchConfig {
            groups: vec![group],
            ..QuickLaunchConfig::default()
        };
        let written = to_toml(&config).expect("the model should serialize");
        let parsed = parse_ok(&written);
        assert_eq!(parsed.config, config, "written as:\n{written}");
    }

    #[test]
    fn a_syntax_error_reports_its_line_and_column() {
        let error = parse("schema_version = = 1").expect_err("this is not TOML");
        match error {
            QuickLaunchError::Parse {
                line,
                column,
                message,
            } => {
                assert_eq!(line, 1);
                assert!(column > 1, "column {column} should point into the line");
                assert!(!message.is_empty());
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_field_is_a_parse_error() {
        let error = parse("[window]\ncolour = \"red\"\n").expect_err("unknown field");
        assert!(
            matches!(error, QuickLaunchError::Parse { line: 2, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_newer_schema_is_refused_not_repaired() {
        let error = parse("schema_version = 99\n").expect_err("newer schema");
        match error {
            QuickLaunchError::UnsupportedVersion { found } => assert_eq!(found, 99),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn schema_version_zero_is_read_as_version_one() {
        let parsed = parse_ok("schema_version = 0\n");
        assert_eq!(parsed.config.schema_version, SCHEMA_VERSION);
        assert_eq!(parsed.warnings, vec![Warning::SchemaVersionZero]);
    }

    #[test]
    fn numeric_settings_are_clamped_into_range() {
        let parsed = parse_ok(
            r#"
[window]
columns = 40
cell_size = 10
icon_size = 50
list_columns = 0
"#,
        );
        let window = &parsed.config.window;
        assert_eq!(window.columns, 16);
        assert_eq!(window.cell_size, 64);
        assert_eq!(window.icon_size, 48);
        assert_eq!(window.list_columns, 1);
        assert_eq!(parsed.warnings.len(), 4, "{:?}", parsed.warnings);
        assert!(parsed.warnings.contains(&Warning::Clamped {
            field: "window.columns",
            found: 40,
            used: 16
        }));
    }

    #[test]
    fn an_icon_size_snaps_to_the_nearest_supported_one() {
        assert_eq!(nearest_icon_size(1), 16);
        assert_eq!(nearest_icon_size(20), 16);
        assert_eq!(nearest_icon_size(21), 24);
        assert_eq!(nearest_icon_size(48), 48);
        assert_eq!(nearest_icon_size(200), 96);
        assert_eq!(nearest_icon_size(-5), 16);
    }

    #[test]
    fn a_bad_group_id_drops_the_whole_group() {
        let parsed = parse_ok(
            r#"
[[group]]
id = "Nine"
name = "Nove"

[[group]]
id = "ok"
name = "Certo"
"#,
        );
        assert_eq!(parsed.config.groups.len(), 1);
        assert_eq!(parsed.config.groups[0].id, "ok");
        assert_eq!(
            parsed.warnings,
            vec![Warning::InvalidGroupId {
                id: "Nine".to_string()
            }]
        );
    }

    #[test]
    fn ids_are_lowercase_ascii_digits_and_underscore() {
        assert!(is_valid_id("a"));
        assert!(is_valid_id("general_2"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("Upper"));
        assert!(!is_valid_id("with space"));
        assert!(!is_valid_id("dash-ed"));
        assert!(!is_valid_id(&"a".repeat(MAX_ID_CHARS + 1)));
        assert!(is_valid_id(&"a".repeat(MAX_ID_CHARS)));
    }

    #[test]
    fn a_duplicate_group_id_keeps_the_first() {
        let parsed = parse_ok(
            r#"
[[group]]
id = "general"
name = "Um"

[[group]]
id = "general"
name = "Dois"
"#,
        );
        assert_eq!(parsed.config.groups.len(), 1);
        assert_eq!(parsed.config.groups[0].name, "Um");
        assert_eq!(
            parsed.warnings,
            vec![Warning::DuplicateGroupId {
                id: "general".to_string()
            }]
        );
    }

    #[test]
    fn an_empty_group_name_drops_the_group() {
        let parsed = parse_ok("[[group]]\nid = \"general\"\nname = \"   \"\n");
        assert!(parsed.config.groups.is_empty());
        assert_eq!(
            parsed.warnings,
            vec![Warning::EmptyGroupName {
                id: "general".to_string()
            }]
        );
    }

    #[test]
    fn a_long_name_is_cut_and_reported() {
        let long = "n".repeat(MAX_NAME_CHARS + 5);
        let parsed = parse_ok(&format!("[[group]]\nid = \"general\"\nname = \"{long}\"\n"));
        assert_eq!(parsed.config.groups[0].name.chars().count(), MAX_NAME_CHARS);
        assert!(matches!(
            parsed.warnings.first(),
            Some(Warning::NameTruncated { .. })
        ));
    }

    #[test]
    fn a_duplicate_slot_keeps_the_first_item() {
        let parsed = parse_ok(
            r#"
[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 3
type = "app"
name = "Primeiro"

[[group.item]]
slot = 3
type = "app"
name = "Segundo"
"#,
        );
        assert_eq!(parsed.config.groups[0].items.len(), 1);
        assert_eq!(parsed.config.groups[0].items[0].name, "Primeiro");
        assert_eq!(
            parsed.warnings,
            vec![Warning::DuplicateSlot {
                group: "general".to_string(),
                slot: 3
            }]
        );
    }

    #[test]
    fn a_slot_past_the_limit_is_moved_to_the_last_one() {
        let parsed = parse_ok(
            r#"
[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 9999
type = "app"
name = "Tarde"
"#,
        );
        assert_eq!(parsed.config.groups[0].items[0].slot, MAX_SLOT);
        assert_eq!(
            parsed.warnings,
            vec![Warning::SlotClamped {
                group: "general".to_string(),
                found: 9999
            }]
        );
    }

    #[test]
    fn a_duplicate_item_id_keeps_the_first_item() {
        let parsed = parse_ok(
            r#"
[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "app"
name = "Um"
id = "x"

[[group.item]]
slot = 1
type = "app"
name = "Dois"
id = "x"
"#,
        );
        assert_eq!(parsed.config.groups[0].items.len(), 1);
        assert_eq!(
            parsed.warnings,
            vec![Warning::DuplicateItemId {
                group: "general".to_string(),
                id: "x".to_string()
            }]
        );
    }

    #[test]
    fn an_invalid_item_id_drops_the_item() {
        let parsed = parse_ok(
            r#"
[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "app"
name = "Um"
id = "Bad"
"#,
        );
        assert!(parsed.config.groups[0].items.is_empty());
        assert_eq!(
            parsed.warnings,
            vec![Warning::InvalidItemId {
                group: "general".to_string(),
                id: "Bad".to_string()
            }]
        );
    }

    #[test]
    fn an_empty_item_name_drops_the_item() {
        let parsed = parse_ok(
            r#"
[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "app"
name = "  "
"#,
        );
        assert!(parsed.config.groups[0].items.is_empty());
        assert_eq!(
            parsed.warnings,
            vec![Warning::EmptyItemName {
                group: "general".to_string(),
                slot: 0
            }]
        );
    }

    #[test]
    fn the_limits_truncate_and_report() {
        let mut text = String::from("schema_version = 1\n");
        for index in 0..(MAX_GROUPS + 1) {
            text.push_str(&format!(
                "[[group]]\nid = \"g{index}\"\nname = \"G{index}\"\n"
            ));
        }
        let parsed = parse_ok(&text);
        assert_eq!(parsed.config.groups.len(), MAX_GROUPS);
        assert_eq!(
            parsed.warnings,
            vec![Warning::TooManyGroups {
                found: MAX_GROUPS + 1
            }]
        );

        let mut text = String::from("[[group]]\nid = \"general\"\nname = \"Geral\"\n");
        for index in 0..(MAX_ITEMS_PER_GROUP + 1) {
            text.push_str(&format!(
                "[[group.item]]\nslot = {index}\ntype = \"app\"\nname = \"I{index}\"\n"
            ));
        }
        let parsed = parse_ok(&text);
        assert_eq!(parsed.config.groups[0].items.len(), MAX_ITEMS_PER_GROUP);
        assert_eq!(
            parsed.warnings,
            vec![Warning::TooManyItems {
                group: "general".to_string(),
                found: MAX_ITEMS_PER_GROUP + 1
            }]
        );
    }

    #[test]
    fn an_unknown_kind_is_kept_and_written_back() {
        let text = r#"
[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "hologram"
name = "Futuro"
"#;
        let parsed = parse_ok(text);
        assert_eq!(
            parsed.config.groups[0].items[0].action_kind(),
            ActionKind::Unknown("hologram".to_string())
        );
        let written = to_toml(&parsed.config).expect("serialize");
        assert!(written.contains("hologram"), "{written}");
        assert_eq!(parse_ok(&written).config, parsed.config);
    }

    #[test]
    fn the_known_kinds_are_named() {
        for (kind, expected) in [
            ("app", ActionKind::App),
            ("folder", ActionKind::Folder),
            ("file", ActionKind::File),
            ("url", ActionKind::Url),
            ("uwp", ActionKind::Uwp),
            ("command", ActionKind::Command),
            ("group", ActionKind::Group),
            ("plugin:acme", ActionKind::Plugin("acme".to_string())),
            ("plugin:", ActionKind::Unknown("plugin:".to_string())),
            ("timer", ActionKind::Unknown("timer".to_string())),
        ] {
            let mut entry = item(0, kind, "x");
            entry.kind = kind.to_string();
            assert_eq!(entry.action_kind(), expected, "{kind}");
        }
    }

    #[test]
    fn extra_data_is_cleared_for_kinds_that_do_not_take_it() {
        let parsed = parse_ok(
            r#"
[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "app"
name = "Um"
extra = { key = "value" }
"#,
        );
        assert!(parsed.config.groups[0].items[0].extra.is_empty());
        assert_eq!(
            parsed.warnings,
            vec![Warning::ExtraCleared {
                group: "general".to_string(),
                slot: 0
            }]
        );
    }

    #[test]
    fn extra_data_survives_on_a_command_item() {
        let parsed = parse_ok(
            r#"
[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "command"
name = "Um"
extra = { shell = "cmd" }
"#,
        );
        assert_eq!(
            parsed.config.groups[0].items[0]
                .extra
                .get("shell")
                .and_then(|value| value.as_str()),
            Some("cmd")
        );
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    }

    /// A group whose items are named after their ids, so a reference can be
    /// read in the warnings.
    fn group_with_bundles(references: &[(&str, &str, Vec<&str>)]) -> String {
        let mut text = String::from("[[group]]\nid = \"general\"\nname = \"Geral\"\n");
        for (index, (id, _name, refs)) in references.iter().enumerate() {
            text.push_str(&format!(
                "\n[[group.item]]\nslot = {index}\ntype = \"group\"\nname = \"{id}\"\nid = \"{id}\"\n"
            ));
            if !refs.is_empty() {
                let list = refs
                    .iter()
                    .map(|reference| format!("\"{reference}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                text.push_str(&format!("items = [{list}]\n"));
            }
        }
        text
    }

    #[test]
    fn a_reference_to_a_missing_item_is_dropped() {
        let parsed = parse_ok(&group_with_bundles(&[("a", "A", vec!["ghost"])]));
        assert!(parsed.config.groups[0].items[0].items.is_empty());
        assert_eq!(
            parsed.warnings,
            vec![Warning::ReferenceDropped {
                group: "general".to_string(),
                item: "a".to_string(),
                reference: "ghost".to_string(),
                reason: ReferenceProblem::Missing,
            }]
        );
    }

    #[test]
    fn a_self_reference_is_dropped() {
        let parsed = parse_ok(&group_with_bundles(&[("a", "A", vec!["a"])]));
        assert!(parsed.config.groups[0].items[0].items.is_empty());
        assert!(matches!(
            parsed.warnings.first(),
            Some(Warning::ReferenceDropped {
                reason: ReferenceProblem::Itself,
                ..
            })
        ));
    }

    #[test]
    fn a_cycle_is_broken() {
        let parsed = parse_ok(&group_with_bundles(&[
            ("a", "A", vec!["b"]),
            ("b", "B", vec!["a"]),
        ]));
        let items = &parsed.config.groups[0].items;
        // One of the two edges survives; the one that closes the loop does not.
        assert_eq!(items.iter().map(|item| item.items.len()).sum::<usize>(), 1);
        assert!(parsed.warnings.iter().any(|warning| matches!(
            warning,
            Warning::ReferenceDropped {
                reason: ReferenceProblem::Cycle,
                ..
            }
        )));
    }

    #[test]
    fn a_chain_that_nests_too_deep_is_cut() {
        let mut references = Vec::new();
        let ids: Vec<String> = (0..(MAX_GROUP_NESTING + 2))
            .map(|index| format!("n{index}"))
            .collect();
        for (index, id) in ids.iter().enumerate() {
            let next = ids.get(index + 1).cloned();
            references.push((id.clone(), next));
        }
        let mut text = String::from("[[group]]\nid = \"general\"\nname = \"Geral\"\n");
        for (index, (id, next)) in references.iter().enumerate() {
            text.push_str(&format!(
                "\n[[group.item]]\nslot = {index}\ntype = \"group\"\nname = \"{id}\"\nid = \"{id}\"\n"
            ));
            if let Some(next) = next {
                text.push_str(&format!("items = [\"{next}\"]\n"));
            }
        }
        let parsed = parse_ok(&text);
        assert!(parsed.warnings.iter().any(|warning| matches!(
            warning,
            Warning::ReferenceDropped {
                reason: ReferenceProblem::TooDeep,
                ..
            }
        )));
        // The result must load the same way again.
        let written = to_toml(&parsed.config).expect("serialize");
        assert_eq!(parse_ok(&written).config, parsed.config);
    }

    #[test]
    fn no_more_than_sixteen_references_survive() {
        let mut text = String::from("[[group]]\nid = \"general\"\nname = \"Geral\"\n");
        let count = MAX_GROUP_REFERENCES + 3;
        for index in 0..count {
            text.push_str(&format!(
                "\n[[group.item]]\nslot = {index}\ntype = \"app\"\nname = \"I{index}\"\nid = \"i{index}\"\n"
            ));
        }
        let list = (0..count)
            .map(|index| format!("\"i{index}\""))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            "\n[[group.item]]\nslot = {count}\ntype = \"group\"\nname = \"Tudo\"\nid = \"all\"\nitems = [{list}]\n"
        ));
        let parsed = parse_ok(&text);
        let bundle = parsed.config.groups[0]
            .items
            .iter()
            .find(|item| item.id.as_deref() == Some("all"))
            .expect("the bundle survives");
        assert_eq!(bundle.items.len(), MAX_GROUP_REFERENCES);
        assert_eq!(
            parsed
                .warnings
                .iter()
                .filter(|warning| matches!(
                    warning,
                    Warning::ReferenceDropped {
                        reason: ReferenceProblem::TooMany,
                        ..
                    }
                ))
                .count(),
            3
        );
    }

    #[test]
    fn a_group_item_without_an_id_cannot_reference_anything() {
        let parsed = parse_ok(
            r#"
[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "app"
name = "Um"
id = "x"

[[group.item]]
slot = 1
type = "group"
name = "Pacote"
items = ["x"]
"#,
        );
        assert!(parsed.config.groups[0].items[1].items.is_empty());
        assert!(matches!(
            parsed.warnings.first(),
            Some(Warning::ReferenceDropped {
                reason: ReferenceProblem::Missing,
                ..
            })
        ));
    }

    #[test]
    fn a_group_is_found_by_id_or_falls_back_to_the_first() {
        let config = QuickLaunchConfig {
            groups: vec![
                Group {
                    id: "a".to_string(),
                    name: "A".to_string(),
                    items: Vec::new(),
                },
                Group {
                    id: "b".to_string(),
                    name: "B".to_string(),
                    items: Vec::new(),
                },
            ],
            ..QuickLaunchConfig::default()
        };
        assert_eq!(
            config.group("b").map(|group| group.name.as_str()),
            Some("B")
        );
        assert_eq!(
            config.group("ghost").map(|group| group.name.as_str()),
            Some("A")
        );
    }

    /// §16.2: `parse` of a 500-item document, median of 50 runs.
    ///
    /// Run with `cargo test --release -p nohrs-core
    /// parse_of_500_items_is_cheap -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing, run by hand for the PR"]
    fn parse_of_500_items_is_cheap() {
        let mut text =
            String::from("schema_version = 1\n\n[[group]]\nid = \"general\"\nname = \"Geral\"\n");
        for slot in 0..500u32 {
            text.push_str(&format!(
                "\n[[group.item]]\nslot = {slot}\ntype = \"app\"\nname = \"Programa {slot}\"\npath = \"C:\\\\Windows\\\\notepad.exe\"\ntags = [\"a\", \"b\"]\n"
            ));
        }
        let mut timings = Vec::new();
        for _ in 0..50 {
            let started = std::time::Instant::now();
            let parsed = parse(&text).expect("the document parses");
            timings.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(parsed.config.groups[0].items.len(), 500);
        }
        timings.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
        let median = timings[timings.len() / 2];
        println!("parse, 500 items: median {median:.3} ms");
        assert!(median < 5.0, "{median} ms is over the §16.2 budget");
    }

    #[test]
    fn migrate_leaves_version_one_alone() {
        let config = QuickLaunchConfig::with_default_group();
        assert_eq!(migrate(config.clone()), config);
    }

    #[test]
    fn every_warning_renders_a_readable_line() {
        let warnings = vec![
            Warning::SchemaVersionZero,
            Warning::TooManyGroups { found: 70 },
            Warning::InvalidGroupId {
                id: "Bad".to_string(),
            },
            Warning::DuplicateGroupId {
                id: "a".to_string(),
            },
            Warning::EmptyGroupName {
                id: "a".to_string(),
            },
            Warning::NameTruncated {
                owner: "group \"a\"".to_string(),
            },
            Warning::TooManyItems {
                group: "a".to_string(),
                found: 600,
            },
            Warning::InvalidItemId {
                group: "a".to_string(),
                id: "B".to_string(),
            },
            Warning::DuplicateItemId {
                group: "a".to_string(),
                id: "b".to_string(),
            },
            Warning::EmptyItemName {
                group: "a".to_string(),
                slot: 0,
            },
            Warning::SlotClamped {
                group: "a".to_string(),
                found: 9999,
            },
            Warning::DuplicateSlot {
                group: "a".to_string(),
                slot: 1,
            },
            Warning::Clamped {
                field: "window.columns",
                found: 40,
                used: 16,
            },
            Warning::ExtraCleared {
                group: "a".to_string(),
                slot: 2,
            },
            Warning::ReferenceDropped {
                group: "a".to_string(),
                item: "b".to_string(),
                reference: "c".to_string(),
                reason: ReferenceProblem::Cycle,
            },
        ];
        for warning in warnings {
            let line = warning.to_string();
            assert!(!line.is_empty());
            assert!(!line.contains("{error}"), "{line}");
        }
    }
}
