//! The explorer's keyboard shortcuts, and the ones the person changed.
//!
//! Every command a shortcut can run is listed once in [`ACTIONS`], with the
//! keys it answers to out of the box. `[keybindings]` in `config.toml` maps a
//! command's id to other keys (`copy = "ctrl-c"`) or to none (`copy = ""`);
//! [`apply`] turns that into the keymap the explorer reads on every key
//! press. The settings page lists the same table, so a command added here
//! shows up there with nothing else to change.
//!
//! Keys are written as gpui spells them (`ctrl-shift-n`, `f5`, `alt-up`), so
//! the same text can become a gpui key binding for the commands the page
//! answers through its action system.

use std::collections::{BTreeMap, HashMap};
use std::sync::{OnceLock, RwLock};

/// A command a shortcut can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum Action {
    Copy,
    Cut,
    Paste,
    Rename,
    Refresh,
    Trash,
    DeletePermanently,
    NewFolder,
    GoUp,
    RecentFolders,
    DirectoryMenu,
    SelectAll,
    SearchSubfolders,
    SearchEverything,
    SearchIndex,
    FilterBar,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    NewTab,
    CloseTab,
    SplitVertical,
    SplitHorizontal,
    FocusPane1,
    FocusPane2,
    NextPane,
    PreviousPane,
    ToggleSidebar,
    TogglePreview,
    ToggleTerminal,
    OpenCommandPrompt,
    OpenPowerShell,
    OpenFileExplorer,
}

/// A command's place in the table: its id in `config.toml`, the group and
/// name the settings page shows (Portuguese keys for `i18n::tr`), and its
/// keys out of the box.
#[derive(Debug, Clone, Copy)]
pub struct ActionInfo {
    #[allow(missing_docs)]
    pub action: Action,
    /// The key under `[keybindings]`.
    pub id: &'static str,
    /// Group heading, a translation key.
    pub group: &'static str,
    /// What the command does, a translation key.
    pub label: &'static str,
    /// Keys it answers to when the config says nothing; the first is the one
    /// shown. Several spellings stand for one key on some layouts.
    pub defaults: &'static [&'static str],
}

const FILES: &str = crate::k!("Arquivos");
const SEARCH: &str = crate::k!("Seleção e busca");
const PANES: &str = crate::k!("Abas e painéis");
const VIEW: &str = crate::k!("Exibição");
const OPEN: &str = crate::k!("Abrir em outro programa");

/// Every command, in the order the settings page lists them.
pub const ACTIONS: &[ActionInfo] = &[
    info(Action::Copy, "copy", FILES, crate::k!("Copiar"), &["ctrl-c"]),
    info(Action::Cut, "cut", FILES, crate::k!("Recortar"), &["ctrl-x"]),
    info(Action::Paste, "paste", FILES, crate::k!("Colar"), &["ctrl-v"]),
    info(Action::Rename, "rename", FILES, crate::k!("Renomear"), &["f2"]),
    info(Action::Refresh, "refresh", FILES, crate::k!("Atualizar a pasta"), &["f5"]),
    info(Action::Trash, "trash", FILES, crate::k!("Mandar para a Lixeira"), &["delete"]),
    info(Action::DeletePermanently, "delete_permanently", FILES, crate::k!("Excluir para sempre"), &["shift-delete"]),
    info(Action::NewFolder, "new_folder", FILES, crate::k!("Nova pasta"), &["ctrl-shift-n"]),
    info(Action::GoUp, "go_up", FILES, crate::k!("Subir para a pasta de cima"), &["alt-up"]),
    // The recent folders, under the header's arrow: the places visited last.
    info(Action::RecentFolders, "recent_folders", FILES, crate::k!("Pastas recentes"), &["alt-down"]),
    // The directory menu, as Total Commander's Ctrl+D: the favorite folders
    // in a popup, with the current folder added from there.
    info(Action::DirectoryMenu, "directory_menu", FILES, crate::k!("Menu de pastas favoritas"), &["ctrl-d"]),
    info(Action::SelectAll, "select_all", SEARCH, crate::k!("Selecionar tudo"), &["ctrl-a"]),
    info(Action::SearchSubfolders, "search_subfolders", SEARCH, crate::k!("Buscar nas subpastas"), &["ctrl-f"]),
    info(Action::SearchEverything, "search_everything", SEARCH, crate::k!("Buscar em todo o computador"), &["ctrl-e"]),
    info(Action::SearchIndex, "search_index", SEARCH, crate::k!("Buscar no índice"), &["ctrl-shift-f"]),
    info(Action::FilterBar, "filter_bar", SEARCH, crate::k!("Barra de filtros"), &["ctrl-shift-l"]),
    info(Action::NewTab, "new_tab", PANES, crate::k!("Nova aba"), &["ctrl-t"]),
    info(Action::CloseTab, "close_tab", PANES, crate::k!("Fechar aba"), &["ctrl-w"]),
    // One key that opens the split and, pressed again, closes it.
    info(Action::SplitVertical, "split_vertical", PANES, crate::k!("Dividir ou fechar lado a lado"), &["ctrl-\\"]),
    // Shift+\ comes as `|` on some layouts, with or without the shift.
    info(
        Action::SplitHorizontal,
        "split_horizontal",
        PANES,
        crate::k!("Dividir em cima e embaixo"),
        &["ctrl-shift-\\", "ctrl-|", "ctrl-shift-|"],
    ),
    info(Action::FocusPane1, "focus_pane_1", PANES, crate::k!("Ir para o painel 1"), &["ctrl-1"]),
    info(Action::FocusPane2, "focus_pane_2", PANES, crate::k!("Ir para o painel 2"), &["ctrl-2"]),
    info(Action::NextPane, "next_pane", PANES, crate::k!("Próximo painel"), &["ctrl-]"]),
    info(Action::PreviousPane, "previous_pane", PANES, crate::k!("Painel anterior"), &["ctrl-["]),
    info(Action::ToggleSidebar, "toggle_sidebar", PANES, crate::k!("Mostrar ou esconder a barra lateral"), &["ctrl-b"]),
    info(Action::TogglePreview, "toggle_preview", PANES, crate::k!("Mostrar ou esconder o visualizador"), &["alt-q"]),
    // Ctrl+' is VS Code's spelling on ABNT2, where ` is a dead key.
    info(
        Action::ToggleTerminal,
        "toggle_terminal",
        PANES,
        crate::k!("Mostrar ou esconder o terminal da aba"),
        &["ctrl-'", "ctrl-`"],
    ),
    // `+` because on most layouts it is Shift+= and some report it so.
    info(Action::ZoomIn, "zoom_in", VIEW, crate::k!("Aumentar o zoom"), &["ctrl-=", "ctrl-+", "ctrl-shift-="]),
    info(Action::ZoomOut, "zoom_out", VIEW, crate::k!("Diminuir o zoom"), &["ctrl--"]),
    info(Action::ZoomReset, "zoom_reset", VIEW, crate::k!("Zoom padrão"), &["ctrl-0"]),
    info(Action::OpenCommandPrompt, "open_command_prompt", OPEN, crate::k!("Abrir a pasta no Prompt de Comando"), &[]),
    info(Action::OpenPowerShell, "open_powershell", OPEN, crate::k!("Abrir a pasta no PowerShell"), &[]),
    info(Action::OpenFileExplorer, "open_file_explorer", OPEN, crate::k!("Abrir a pasta no Explorador do Windows"), &[]),
];

const fn info(
    action: Action,
    id: &'static str,
    group: &'static str,
    label: &'static str,
    defaults: &'static [&'static str],
) -> ActionInfo {
    ActionInfo { action, id, group, label, defaults }
}

/// The table row of `action`.
pub fn info_of(action: Action) -> &'static ActionInfo {
    // Every variant has a row; the test below holds the table to that.
    ACTIONS.iter().find(|info| info.action == action).unwrap_or(&ACTIONS[0])
}

/// A key with its modifiers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chord {
    /// Ctrl (or Cmd).
    pub ctrl: bool,
    #[allow(missing_docs)]
    pub alt: bool,
    #[allow(missing_docs)]
    pub shift: bool,
    /// gpui's name of the key, lowercase: `n`, `f5`, `delete`, `\`.
    pub key: String,
}

/// Keys that only modify others; a chord is never just one of these.
pub const MODIFIER_KEYS: &[&str] = &["control", "ctrl", "shift", "alt", "platform", "cmd", "win", "function", "fn", "capslock"];

impl Chord {
    /// Reads `ctrl-shift-n`. `None` for an empty or modifier-only text.
    pub fn parse(text: &str) -> Option<Chord> {
        let text = text.trim().to_lowercase();
        if text.is_empty() {
            return None;
        }
        let mut chord = Chord { ctrl: false, alt: false, shift: false, key: String::new() };
        let mut rest = text.as_str();
        // `-` separates parts, but `ctrl--` is Ctrl and the minus key.
        loop {
            let Some((head, tail)) = rest.split_once('-') else { break };
            if tail.is_empty() {
                break;
            }
            match head {
                "ctrl" | "cmd" | "control" | "platform" => chord.ctrl = true,
                "alt" => chord.alt = true,
                "shift" => chord.shift = true,
                _ => break,
            }
            rest = tail;
        }
        if rest.is_empty() || MODIFIER_KEYS.contains(&rest) {
            return None;
        }
        chord.key = rest.to_string();
        Some(chord)
    }

    /// The `config.toml` and gpui spelling: `ctrl-shift-n`.
    pub fn to_config(&self) -> String {
        let mut text = String::new();
        if self.ctrl {
            text.push_str("ctrl-");
        }
        if self.alt {
            text.push_str("alt-");
        }
        if self.shift {
            text.push_str("shift-");
        }
        text.push_str(&self.key);
        text
    }

    /// How a person reads it: `Ctrl+Shift+N`, `Alt+↑`, `F5`.
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".into());
        }
        if self.alt {
            parts.push("Alt".into());
        }
        if self.shift {
            parts.push("Shift".into());
        }
        parts.push(key_name(&self.key));
        parts.join("+")
    }

    /// Whether a bare key with no Ctrl or Alt, which would type into the
    /// filter instead of running anything: letters, digits, symbols.
    pub fn is_typing_key(&self) -> bool {
        !self.ctrl && !self.alt && self.key.chars().count() == 1
    }
}

fn key_name(key: &str) -> String {
    match key {
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "escape" => "Esc".into(),
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        "space" => "Espaço".into(),
        _ if key.chars().count() == 1 => key.to_uppercase(),
        _ => {
            let mut chars = key.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

fn keymap() -> &'static RwLock<HashMap<Action, Vec<Chord>>> {
    static KEYMAP: OnceLock<RwLock<HashMap<Action, Vec<Chord>>>> = OnceLock::new();
    KEYMAP.get_or_init(|| RwLock::new(build(&BTreeMap::new())))
}

/// The keymap `[keybindings]` describes: each command's own keys, or its
/// defaults when the config does not name it. An unreadable value keeps the
/// defaults rather than leave the command with nothing.
pub fn build(bindings: &BTreeMap<String, String>) -> HashMap<Action, Vec<Chord>> {
    ACTIONS
        .iter()
        .map(|info| {
            let chords = match bindings.get(info.id) {
                Some(value) if value.trim().is_empty() => Vec::new(),
                Some(value) => match Chord::parse(value) {
                    Some(chord) => vec![chord],
                    None => defaults(info),
                },
                None => defaults(info),
            };
            (info.action, chords)
        })
        .collect()
}

fn defaults(info: &ActionInfo) -> Vec<Chord> {
    info.defaults.iter().filter_map(|text| Chord::parse(text)).collect()
}

/// Makes `[keybindings]` the keymap. Returns whether anything changed, so
/// the caller knows to rebind and repaint.
pub fn apply(bindings: &BTreeMap<String, String>) -> bool {
    let next = build(bindings);
    let mut current = match keymap().write() {
        Ok(current) => current,
        Err(poisoned) => poisoned.into_inner(),
    };
    if *current == next {
        return false;
    }
    *current = next;
    true
}

/// The keys `action` answers to now.
pub fn chords(action: Action) -> Vec<Chord> {
    let current = match keymap().read() {
        Ok(current) => current,
        Err(poisoned) => poisoned.into_inner(),
    };
    current.get(&action).cloned().unwrap_or_default()
}

/// The keys `action` answers to, as shown beside it ("Ctrl+C"); `None` when
/// it has none.
pub fn display(action: Action) -> Option<String> {
    chords(action).first().map(Chord::display)
}

/// `label` followed by the action's keys in parentheses, when it has any.
pub fn with_keys(label: &str, action: Action) -> String {
    match display(action) {
        Some(keys) => format!("{label} ({keys})"),
        None => label.to_string(),
    }
}

/// The command a key press runs. A symbol typed with Shift (`+`, `|`) also
/// matches a chord written without it, since layouts disagree on whether
/// the Shift is reported.
pub fn resolve(ctrl: bool, alt: bool, shift: bool, key: &str) -> Option<Action> {
    let key = key.to_lowercase();
    let current = match keymap().read() {
        Ok(current) => current,
        Err(poisoned) => poisoned.into_inner(),
    };
    let find = |shift: bool| {
        ACTIONS.iter().map(|info| info.action).find(|action| {
            current.get(action).is_some_and(|chords| {
                chords.iter().any(|chord| chord.ctrl == ctrl && chord.alt == alt && chord.shift == shift && chord.key == key)
            })
        })
    };
    find(shift).or_else(|| {
        let symbol = key.chars().count() == 1 && !key.chars().all(char::is_alphanumeric);
        if shift && symbol { find(false) } else { None }
    })
}

/// The other commands already answering to `chord` in `bindings`'s keymap.
pub fn conflicts(bindings: &BTreeMap<String, String>, chord: &Chord, except: Action) -> Vec<Action> {
    let map = build(bindings);
    ACTIONS
        .iter()
        .map(|info| info.action)
        .filter(|action| *action != except)
        .filter(|action| map.get(action).is_some_and(|chords| chords.contains(chord)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_one_row_and_its_defaults_parse() {
        let mut ids = std::collections::HashSet::new();
        for info in ACTIONS {
            assert!(ids.insert(info.id), "duplicated id {}", info.id);
            assert_eq!(info_of(info.action).id, info.id);
            for text in info.defaults {
                let chord = Chord::parse(text).unwrap_or_else(|| panic!("bad default {text}"));
                assert_eq!(&chord.to_config(), text, "defaults are written in canonical order");
            }
        }
        // No two commands share a default key.
        let map = build(&BTreeMap::new());
        for info in ACTIONS {
            for chord in &map[&info.action] {
                assert!(conflicts(&BTreeMap::new(), chord, info.action).is_empty(), "{} shares {chord:?}", info.id);
            }
        }
    }

    #[test]
    fn chords_read_and_print_both_ways() {
        let chord = Chord::parse("Ctrl-Shift-N").unwrap();
        assert_eq!(chord.to_config(), "ctrl-shift-n");
        assert_eq!(chord.display(), "Ctrl+Shift+N");
        assert_eq!(Chord::parse("ctrl--").unwrap().key, "-");
        assert_eq!(Chord::parse("alt-up").unwrap().display(), "Alt+↑");
        assert_eq!(Chord::parse("f5").unwrap().display(), "F5");
        assert!(Chord::parse("ctrl-shift").is_none());
        assert!(Chord::parse("").is_none());
        assert!(Chord::parse("k").unwrap().is_typing_key());
        assert!(!Chord::parse("f2").unwrap().is_typing_key());
    }

    #[test]
    fn the_config_replaces_or_removes_keys() {
        let mut bindings = BTreeMap::new();
        bindings.insert("copy".to_string(), "ctrl-shift-c".to_string());
        bindings.insert("cut".to_string(), String::new());
        bindings.insert("paste".to_string(), "ctrl-shift".to_string());
        let map = build(&bindings);
        assert_eq!(map[&Action::Copy], vec![Chord::parse("ctrl-shift-c").unwrap()]);
        assert!(map[&Action::Cut].is_empty());
        assert_eq!(map[&Action::Paste], vec![Chord::parse("ctrl-v").unwrap()], "unreadable keeps the default");
        let conflicting = conflicts(&bindings, &Chord::parse("ctrl-v").unwrap(), Action::Copy);
        assert_eq!(conflicting, vec![Action::Paste]);
    }

    #[test]
    fn a_key_press_finds_its_command() {
        apply(&BTreeMap::new());
        assert_eq!(resolve(true, false, false, "C"), Some(Action::Copy));
        assert_eq!(resolve(false, false, true, "delete"), Some(Action::DeletePermanently));
        assert_eq!(resolve(false, false, false, "delete"), Some(Action::Trash));
        // `+` with the Shift the layout needs for it.
        assert_eq!(resolve(true, false, true, "+"), Some(Action::ZoomIn));
        assert_eq!(resolve(true, false, true, "c"), None);
    }
}
