//! Starting what a quick-launch item points at
//! (docs/QuickDock technical implementation plan and boundary conventions, §7, §8).
//!
//! Three things live here, all of them usable off the UI thread:
//!
//! * [`expand`] — the `%ENV%` / `{portable}` / `{luma}` / `{config}` variables
//!   of a stored path (§7).
//! * [`LaunchAction`] and [`LaunchRegistry`] — one implementation per `type`,
//!   looked up by name (§8.1).
//! * [`validate_url`] — the scheme whitelist, a pure function (§8.2).
//!
//! The Windows half is in [`windows`]; elsewhere the registry still knows the
//! kinds so that the interface draws the same list, but starting them reports
//! [`LaunchError::Unsupported`] (§8.1).
//!
//! Nothing here may run on the GPUI foreground thread: a Shell call can sit on
//! a network path or a sleeping disk (§8.4).

use std::collections::BTreeMap;
use std::path::PathBuf;

use nohrs_core::config::quick_launch::{Item, OnRunning};

#[cfg(windows)]
mod windows;

/// `type` of an application item.
pub const KIND_APP: &str = "app";
/// `type` of a folder item.
pub const KIND_FOLDER: &str = "folder";
/// `type` of a document item (M2).
pub const KIND_FILE: &str = "file";
/// `type` of a link item.
pub const KIND_URL: &str = "url";

/// The kind names this milestone implements.
pub const BUILTIN_KINDS: [&str; 4] = [KIND_APP, KIND_FOLDER, KIND_URL, KIND_FILE];

/// Why a stored path could not be turned into a real one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    /// The path was empty, or only whitespace.
    #[error("the path is empty")]
    Empty,
    /// A `%NAME%` or `{name}` the context cannot answer. Left unexpanded, the
    /// string would reach the Shell as a path nobody meant.
    #[error("the variable {name} is not defined")]
    Undefined {
        /// The variable's name, without its delimiters.
        name: String,
    },
    /// `{portable}` outside a portable installation.
    #[error("{{portable}} is only available in portable mode")]
    NotPortable,
}

/// What the variables of a stored path resolve against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandContext {
    /// The `userdata` folder beside the executable, when the installation is
    /// portable; `None` otherwise.
    pub portable_root: Option<PathBuf>,
    /// The directory the program's executable sits in (`{luma}`), which is also
    /// what a relative path is resolved against.
    pub program_dir: PathBuf,
    /// The active configuration directory (`{config}`).
    pub config_dir: PathBuf,
}

impl ExpandContext {
    /// The context of this process.
    ///
    /// Reads the same paths the rest of the application uses, so a portable
    /// installation expands `{portable}` to the folder it was started from.
    pub fn current() -> Self {
        let portable_root = nohrs_core::config::paths::portable_mode_enabled()
            .then(nohrs_core::config::paths::portable_config_dir);
        // The same folder `portable_config_dir` is built from; it is not a
        // getter the paths module offers, and `{luma}` is a services concern.
        let program_dir = std::env::current_exe()
            .ok()
            .and_then(|executable| executable.parent().map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        Self {
            portable_root,
            program_dir,
            config_dir: nohrs_core::config::paths::active_config_dir(),
        }
    }
}

/// Turns a stored path into one the Shell can be handed.
///
/// One level only: a variable whose value itself holds `%OTHER%` is not
/// expanded again, so a file cannot make the program resolve a path the author
/// never wrote. A relative path is resolved against [`ExpandContext::program_dir`].
pub fn expand(raw: &str, context: &ExpandContext) -> Result<PathBuf, ExpandError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ExpandError::Empty);
    }
    let characters: Vec<char> = trimmed.chars().collect();
    let mut expanded = String::with_capacity(trimmed.len() + 32);
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '%' => {
                let closing = characters[index + 1..].iter().position(|unit| *unit == '%');
                match closing {
                    // `%%` is not a variable; keep the characters as written.
                    Some(0) | None => {
                        expanded.push('%');
                        index += 1;
                    }
                    Some(length) => {
                        let name: String =
                            characters[index + 1..index + 1 + length].iter().collect();
                        let value = environment_variable(&name)
                            .ok_or_else(|| ExpandError::Undefined { name: name.clone() })?;
                        expanded.push_str(&value);
                        index += 1 + length + 1;
                    }
                }
            }
            '{' => {
                let closing = characters[index + 1..].iter().position(|unit| *unit == '}');
                match closing {
                    Some(0) | None => {
                        expanded.push('{');
                        index += 1;
                    }
                    Some(length) => {
                        let name: String =
                            characters[index + 1..index + 1 + length].iter().collect();
                        match name.as_str() {
                            "luma" => expanded.push_str(&context.program_dir.to_string_lossy()),
                            "config" => expanded.push_str(&context.config_dir.to_string_lossy()),
                            "portable" => {
                                let root = context
                                    .portable_root
                                    .as_ref()
                                    .ok_or(ExpandError::NotPortable)?;
                                expanded.push_str(&root.to_string_lossy());
                            }
                            _ => {
                                return Err(ExpandError::Undefined { name });
                            }
                        }
                        index += 1 + length + 1;
                    }
                }
            }
            character => {
                expanded.push(character);
                index += 1;
            }
        }
    }
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(context.program_dir.join(path))
}

/// A `%NAME%` variable, matched without regard to case.
///
/// Windows' own lookup already ignores case; the scan is for the platforms
/// where it does not, so that one configuration file means the same thing
/// everywhere.
fn environment_variable(name: &str) -> Option<String> {
    if let Some(value) = std::env::var_os(name) {
        return Some(value.to_string_lossy().into_owned());
    }
    std::env::vars_os()
        .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.to_string_lossy().into_owned())
}

/// Rejects an address whose scheme could start anything but a browser or a
/// mail client (§8.2).
///
/// `file:` would open a local file without going through the `folder` / `file`
/// path (and without their rules), and `javascript:`, `vbscript:` and `ms-*:`
/// reach handlers that can run programs.
pub fn validate_url(raw: &str) -> Result<(), LaunchError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(LaunchError::Blocked("the address is empty".to_string()));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(LaunchError::Blocked(
            "the address holds a control character".to_string(),
        ));
    }
    let Some(colon) = trimmed.find(':') else {
        return Err(LaunchError::Blocked(
            "the address has no scheme".to_string(),
        ));
    };
    let scheme = trimmed[..colon].to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
        return Err(LaunchError::Blocked(format!(
            "the {scheme}: scheme is not allowed"
        )));
    }
    if trimmed[colon + 1..].trim().is_empty() {
        return Err(LaunchError::Blocked(
            "the address has nothing after its scheme".to_string(),
        ));
    }
    Ok(())
}

/// What a health check found, without opening anything (§8.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// The item points at something that exists.
    Ok,
    /// The item expands to a path that is not there.
    Missing,
    /// The item cannot be launched at all, and why.
    Invalid(String),
    /// No implementation knows this `type`.
    Unknown,
}

/// How a launch was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaunchMode {
    /// The ordinary click.
    #[default]
    Normal,
    /// Run elevated (`runas`).
    Admin,
    /// Start a new instance whatever the configuration says.
    ForceNew,
}

/// What a launch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// A new process was started.
    Started,
    /// An instance that was already running was brought to the front.
    Activated,
    /// The user answered "no" to the UAC prompt. Not an error: nothing is
    /// reported for a decision (§8.2).
    Cancelled,
    /// A folder the pages layer should open as a new tab of LumaFiles. The
    /// services layer cannot do it itself without depending on the interface.
    OpenFolder(PathBuf),
}

/// Everything a launch needs besides the item itself.
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    /// How the launch was asked for.
    pub mode: LaunchMode,
    /// What the configuration wants done when the target is already running.
    pub on_running: OnRunning,
    /// What the item's path variables resolve against.
    pub expand: ExpandContext,
}

/// Why a launch did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// The path the item names is not there.
    #[error("not found: {0}")]
    NotFound(PathBuf),
    /// A path variable could not be resolved.
    #[error("{0}")]
    Expand(#[from] ExpandError),
    /// This build cannot start this `type` on this platform.
    #[error("this type is not supported: {0}")]
    Unsupported(String),
    /// The address (or the item) was refused before anything was started.
    #[error("blocked: {0}")]
    Blocked(String),
    /// The system refused, with its own code and message.
    #[error("system error {code}: {message}")]
    Os {
        /// The Win32 error code.
        code: u32,
        /// The message the system gives for it.
        message: String,
    },
    /// Something unexpected, including a panic caught on the worker thread.
    #[error("internal error: {0}")]
    Internal(String),
}

/// One implementation per item `type`.
pub trait LaunchAction: Send + Sync + 'static {
    /// The `type` this action serves.
    fn kind(&self) -> &str;
    /// Whether the item points at something that exists. Looks at the path
    /// only: no opening, no network access.
    fn health(&self, item: &Item, context: &ExpandContext) -> Health;
    /// Starts the item.
    fn launch(&self, item: &Item, request: &LaunchRequest) -> Result<LaunchOutcome, LaunchError>;
}

/// The actions this build knows, by `type`.
pub struct LaunchRegistry {
    actions: BTreeMap<String, Box<dyn LaunchAction>>,
}

impl LaunchRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            actions: BTreeMap::new(),
        }
    }

    /// The registry with this milestone's kinds registered.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        let builtins: Vec<Box<dyn LaunchAction>> = vec![
            Box::new(FolderAction),
            url_action(),
            app_action(),
            file_action(),
        ];
        for action in builtins {
            // The kinds are distinct constants; a duplicate would be a mistake
            // in this file, and is reported rather than silently replacing.
            if let Err(error) = registry.register(action) {
                tracing::error!("quick launch: {error}");
            }
        }
        registry
    }

    /// Adds `action`, or reports that its `type` is already taken.
    pub fn register(&mut self, action: Box<dyn LaunchAction>) -> Result<(), LaunchError> {
        let kind = action.kind().to_string();
        if self.actions.contains_key(&kind) {
            return Err(LaunchError::Internal(format!(
                "the type {kind:?} is already registered"
            )));
        }
        self.actions.insert(kind, action);
        Ok(())
    }

    /// The action serving `kind`, if this build has one.
    pub fn get(&self, kind: &str) -> Option<&dyn LaunchAction> {
        self.actions.get(kind).map(Box::as_ref)
    }

    /// The health of `item`, or [`Health::Unknown`] when nothing serves it.
    pub fn health(&self, item: &Item, context: &ExpandContext) -> Health {
        match self.get(&item.kind) {
            Some(action) => action.health(item, context),
            None => Health::Unknown,
        }
    }

    /// Starts `item`, or reports why not.
    pub fn launch(
        &self,
        item: &Item,
        request: &LaunchRequest,
    ) -> Result<LaunchOutcome, LaunchError> {
        match self.get(&item.kind) {
            Some(action) => action.launch(item, request),
            None => Err(LaunchError::Unsupported(item.kind.clone())),
        }
    }
}

impl Default for LaunchRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// The path an item points at, expanded.
///
/// A kind that needs one and does not have it is refused here rather than
/// reaching the Shell with an empty string.
fn item_path(item: &Item, context: &ExpandContext) -> Result<PathBuf, LaunchError> {
    match item.path.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => Ok(expand(raw, context)?),
        _ => Err(LaunchError::Blocked(format!(
            "the {} item {:?} has no path",
            item.kind, item.name
        ))),
    }
}

/// The icon at `index` inside `path` (`"exe,索引"`, §9), as premultiplied BGRA
/// and its width and height.
///
/// `None` on a platform without the Shell, and for a path or index the Shell
/// cannot read.
pub fn indexed_icon(path: &str, index: u32, edge: u32) -> Option<(Vec<u8>, u32, u32)> {
    #[cfg(windows)]
    {
        windows::indexed_icon(path, index, edge)
    }
    #[cfg(not(windows))]
    {
        let (_, _, _) = (path, index, edge);
        None
    }
}

/// A folder, handed back to the pages layer to open as a tab (§8.1).
///
/// Platform-independent on purpose: it touches no system API, so the same code
/// answers on every platform where LumaFiles' Explorer exists.
struct FolderAction;

impl LaunchAction for FolderAction {
    fn kind(&self) -> &str {
        KIND_FOLDER
    }

    fn health(&self, item: &Item, context: &ExpandContext) -> Health {
        match item_path(item, context) {
            Ok(path) if path.is_dir() => Health::Ok,
            Ok(_) => Health::Missing,
            Err(LaunchError::Expand(error)) => Health::Invalid(error.to_string()),
            Err(error) => Health::Invalid(error.to_string()),
        }
    }

    fn launch(&self, item: &Item, request: &LaunchRequest) -> Result<LaunchOutcome, LaunchError> {
        let path = item_path(item, &request.expand)?;
        if !path.is_dir() {
            return Err(LaunchError::NotFound(path));
        }
        Ok(LaunchOutcome::OpenFolder(path))
    }
}

#[cfg(windows)]
fn app_action() -> Box<dyn LaunchAction> {
    Box::new(windows::AppAction)
}

#[cfg(not(windows))]
fn app_action() -> Box<dyn LaunchAction> {
    Box::new(UnsupportedAction { kind: KIND_APP })
}

#[cfg(windows)]
fn file_action() -> Box<dyn LaunchAction> {
    Box::new(windows::FileAction)
}

#[cfg(not(windows))]
fn file_action() -> Box<dyn LaunchAction> {
    Box::new(UnsupportedAction { kind: KIND_FILE })
}

#[cfg(windows)]
fn url_action() -> Box<dyn LaunchAction> {
    Box::new(windows::UrlAction)
}

#[cfg(not(windows))]
fn url_action() -> Box<dyn LaunchAction> {
    Box::new(UnsupportedAction { kind: KIND_URL })
}

/// A kind this build cannot start on this platform.
///
/// Registered all the same, so the item list is the same everywhere and the
/// interface can say "not supported here" instead of "unknown type".
#[cfg(not(windows))]
struct UnsupportedAction {
    kind: &'static str,
}

#[cfg(not(windows))]
impl LaunchAction for UnsupportedAction {
    fn kind(&self) -> &str {
        self.kind
    }

    fn health(&self, _item: &Item, _context: &ExpandContext) -> Health {
        Health::Invalid(format!("{} is only implemented on Windows", self.kind))
    }

    fn launch(&self, _item: &Item, _request: &LaunchRequest) -> Result<LaunchOutcome, LaunchError> {
        Err(LaunchError::Unsupported(self.kind.to_string()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
mod tests {
    use super::*;
    use nohrs_core::config::quick_launch::Item;

    fn context() -> ExpandContext {
        ExpandContext {
            portable_root: Some(PathBuf::from("P:\\userdata")),
            program_dir: PathBuf::from("C:\\Apps\\Luma"),
            config_dir: PathBuf::from("C:\\Users\\me\\.config\\nohrs"),
        }
    }

    fn item(kind: &str, path: &str) -> Item {
        let mut entry = Item::new(0, kind, "Item");
        entry.path = Some(path.to_string());
        entry
    }

    fn request() -> LaunchRequest {
        LaunchRequest {
            mode: LaunchMode::Normal,
            on_running: OnRunning::Activate,
            expand: context(),
        }
    }

    fn expanded(raw: &str) -> PathBuf {
        expand(raw, &context()).expect("the path should expand")
    }

    #[test]
    fn a_plain_path_is_kept_as_written() {
        assert_eq!(
            expanded("C:\\Windows\\notepad.exe"),
            PathBuf::from("C:\\Windows\\notepad.exe")
        );
    }

    #[test]
    fn the_named_folders_expand() {
        assert_eq!(
            expanded("{luma}\\tools"),
            PathBuf::from("C:\\Apps\\Luma\\tools")
        );
        assert_eq!(
            expanded("{config}/config.toml"),
            PathBuf::from("C:\\Users\\me\\.config\\nohrs/config.toml")
        );
        assert_eq!(
            expanded("{portable}\\notes.txt"),
            PathBuf::from("P:\\userdata\\notes.txt")
        );
    }

    #[test]
    fn an_environment_variable_expands_without_regard_to_case() {
        let name = "NOHRS_QUICK_LAUNCH_TEST_VARIABLE";
        // Rust 2024 made `set_var` unsafe; this test needs the variable set.
        unsafe { std::env::set_var(name, "C:\\temp") };
        let upper = expand("%NOHRS_QUICK_LAUNCH_TEST_VARIABLE%\\x", &context());
        let lower = expand("%nohrs_quick_launch_test_variable%\\x", &context());
        unsafe { std::env::remove_var(name) };
        assert_eq!(upper.expect("set variable"), PathBuf::from("C:\\temp\\x"));
        assert_eq!(lower.expect("set variable"), PathBuf::from("C:\\temp\\x"));
    }

    #[test]
    fn an_undefined_variable_is_an_error_not_a_literal() {
        let error =
            expand("%NOHRS_QUICK_LAUNCH_SURELY_UNDEFINED%\\x", &context()).expect_err("undefined");
        assert_eq!(
            error,
            ExpandError::Undefined {
                name: "NOHRS_QUICK_LAUNCH_SURELY_UNDEFINED".to_string()
            }
        );

        let error = expand("{unknown}\\x", &context()).expect_err("undefined");
        assert_eq!(
            error,
            ExpandError::Undefined {
                name: "unknown".to_string()
            }
        );
    }

    #[test]
    fn portable_outside_a_portable_installation_is_an_error() {
        let context = ExpandContext {
            portable_root: None,
            ..context()
        };
        let error = expand("{portable}\\x", &context).expect_err("not portable");
        assert_eq!(error, ExpandError::NotPortable);
    }

    #[test]
    fn only_one_level_is_expanded() {
        let name = "NOHRS_QUICK_LAUNCH_INDIRECT";
        unsafe { std::env::set_var(name, "%NOHRS_QUICK_LAUNCH_SURELY_UNDEFINED%") };
        let result = expand("%NOHRS_QUICK_LAUNCH_INDIRECT%\\x", &context());
        unsafe { std::env::remove_var(name) };
        let path = result.expect("the outer variable is defined");
        assert!(
            path.to_string_lossy().contains('%'),
            "the inner variable should be left as written: {path:?}"
        );
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_program_folder() {
        assert_eq!(
            expanded("tools\\run.cmd"),
            PathBuf::from("C:\\Apps\\Luma\\tools\\run.cmd")
        );
        assert_eq!(expanded("./a/b"), PathBuf::from("C:\\Apps\\Luma\\a/b"));
    }

    #[test]
    fn an_empty_path_is_an_error() {
        assert_eq!(expand("   ", &context()), Err(ExpandError::Empty));
    }

    #[test]
    fn stray_delimiters_are_kept() {
        assert_eq!(expanded("100%%"), PathBuf::from("C:\\Apps\\Luma\\100%%"));
        assert_eq!(expanded("a{b"), PathBuf::from("C:\\Apps\\Luma\\a{b"));
    }

    #[test]
    fn the_url_scheme_whitelist_lets_the_three_through() {
        for url in [
            "http://example.com",
            "https://example.com/a?b=c",
            "mailto:someone@example.com",
            "HTTPS://EXAMPLE.COM",
            "  https://example.com  ",
        ] {
            assert!(validate_url(url).is_ok(), "{url} should be allowed");
        }
    }

    #[test]
    fn every_other_scheme_is_blocked() {
        for url in [
            "file:///C:/Windows/System32/cmd.exe",
            "javascript:alert(1)",
            "vbscript:msgbox(1)",
            "ms-settings:privacy",
            "ftp://example.com",
            "C:\\Windows\\notepad.exe",
            "example.com",
            "https:",
            "   ",
            "http://example.com/\u{0}",
        ] {
            let error = validate_url(url).expect_err("should be blocked");
            assert!(
                matches!(error, LaunchError::Blocked(_)),
                "{url} gave {error:?}"
            );
        }
    }

    #[test]
    fn the_registry_knows_the_builtin_kinds() {
        let registry = LaunchRegistry::with_builtins();
        for kind in BUILTIN_KINDS {
            assert!(registry.get(kind).is_some(), "{kind} should be registered");
        }
        assert!(registry.get("uwp").is_none());
        assert_eq!(
            registry.health(&item("uwp", "{config}"), &context()),
            Health::Unknown
        );
    }

    #[test]
    fn registering_a_kind_twice_is_an_error() {
        let mut registry = LaunchRegistry::new();
        registry
            .register(Box::new(FolderAction))
            .expect("the first registration should work");
        let error = registry
            .register(Box::new(FolderAction))
            .expect_err("the second should not");
        assert!(error.to_string().contains("already registered"), "{error}");
    }

    #[test]
    fn a_folder_item_asks_the_pages_layer_to_open_it() {
        // An existing folder with an absolute path, so the test context's
        // `{luma}` (which is not a real directory) plays no part.
        let folder = std::env::current_dir().expect("the test has a working directory");
        let mut entry = item(KIND_FOLDER, &folder.to_string_lossy());
        entry.name = "Pasta".to_string();
        let outcome = LaunchRegistry::with_builtins()
            .launch(&entry, &request())
            .expect("a folder that exists should launch");
        assert_eq!(outcome, LaunchOutcome::OpenFolder(folder));
    }

    #[test]
    fn a_folder_item_that_is_not_there_names_the_path() {
        let entry = item(KIND_FOLDER, "{luma}\\nao-existe");
        let error = LaunchRegistry::with_builtins()
            .launch(&entry, &request())
            .expect_err("a missing folder should fail");
        assert_eq!(
            error,
            LaunchError::NotFound(PathBuf::from("C:\\Apps\\Luma\\nao-existe"))
        );
        assert_eq!(
            LaunchRegistry::with_builtins().health(&entry, &context()),
            Health::Missing
        );
    }

    #[test]
    fn an_item_without_a_path_is_refused_before_the_shell_sees_it() {
        let mut entry = item(KIND_APP, "x");
        entry.path = None;
        let error = LaunchRegistry::with_builtins()
            .launch(&entry, &request())
            .expect_err("no path");
        assert!(matches!(error, LaunchError::Blocked(_)), "{error:?}");
    }

    #[test]
    fn an_unsupported_type_says_which_one() {
        let entry = item("hologram", "{luma}");
        let error = LaunchRegistry::with_builtins()
            .launch(&entry, &request())
            .expect_err("nothing serves this");
        assert_eq!(error, LaunchError::Unsupported("hologram".to_string()));
    }
}
