//! The sidebar's favorite folders ("Pastas favoritas") and the directory
//! menu (Ctrl+D), arranged the way Total Commander's directory menu is: a
//! tree the user makes up, of shortcuts and submenus they name themselves.
//! A submenu is the list's own structure, nothing on disk — the folders it
//! holds may live anywhere. Only the shortcuts live here: renaming,
//! removing or deleting a submenu never touches the folders themselves.
//!
//! One tree for the whole app, held as a GPUI global and saved to the same
//! redb store as the tags and the network places. Paths are compared the
//! way Windows reads them, without case or trailing separators, so the same
//! folder is never listed twice under different spellings.

use std::sync::Arc;

use gpui::{App, AppContext as _, BorrowAppContext as _, Global, Task};
use nohrs_core::telemetry::LogErr;
use nohrs_store::{KvKey, KvStore, kv_key};
use serde::{Deserialize, Serialize};

use super::same_path;

const FAVORITES_KEY: KvKey = kv_key!("explorer.favorites");

/// One entry of the favorites: either a shortcut to a folder, or a submenu
/// the user made up, which holds more entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FavoriteItem {
    pub id: u32,
    /// The name shown, as the user named it.
    pub name: String,
    /// For a shortcut: the folder it opens. A submenu has none.
    pub path: String,
    /// For a submenu: the entries inside, in the order the user keeps.
    #[serde(default)]
    pub children: Vec<FavoriteItem>,
}

impl FavoriteItem {
    /// Whether this entry is a submenu the user made up, not a shortcut.
    pub(crate) fn is_submenu(&self) -> bool {
        self.path.is_empty()
    }

    // A shortcut named after the folder it opens.
    fn link(id: u32, path: &str) -> Self {
        let name = std::path::Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        Self {
            id,
            name,
            path: path.to_string(),
            children: Vec::new(),
        }
    }

    // A submenu, empty for now.
    fn submenu(id: u32, name: String) -> Self {
        Self {
            id,
            name,
            path: String::new(),
            children: Vec::new(),
        }
    }
}

/// What the config file stores.
#[derive(Default, Serialize, Deserialize)]
struct Stored {
    items: Vec<FavoriteItem>,
    next_id: u32,
}

/// The flat shortcut list the first version saved, before submenus existed.
#[derive(Serialize, Deserialize)]
struct Legacy {
    path: String,
    name: String,
}

/// The favorites, kept for the whole app and saved with the tabs.
#[derive(Default)]
pub(crate) struct Favorites {
    items: Vec<FavoriteItem>,
    next_id: u32,
    store: Option<Arc<dyn KvStore>>,
    save: Option<Task<()>>,
}

impl Global for Favorites {}

impl Favorites {
    pub(crate) fn install(store: Option<Arc<dyn KvStore>>, cx: &mut App) {
        if cx.has_global::<Self>() {
            return;
        }
        let stored = store
            .as_ref()
            .and_then(|store| store.get(&FAVORITES_KEY).log_err().flatten());
        let mut favorites = Self {
            store,
            ..Default::default()
        };
        if let Some(bytes) = stored {
            match serde_json::from_slice::<Stored>(&bytes) {
                Ok(stored) => {
                    favorites.items = stored.items;
                    favorites.next_id = stored.next_id;
                }
                // The first version saved a flat list of shortcuts; they
                // become top-level entries with the names they had.
                Err(_) => {
                    if let Ok(legacy) = serde_json::from_slice::<Vec<Legacy>>(&bytes) {
                        for (index, favorite) in legacy.into_iter().enumerate() {
                            favorites.items.push(FavoriteItem {
                                id: index as u32 + 1,
                                name: favorite.name,
                                path: favorite.path,
                                children: Vec::new(),
                            });
                        }
                        favorites.next_id = favorites.items.len() as u32 + 1;
                    }
                }
            }
        }
        // Id 0 is reserved as "none", so the first entry is 1.
        favorites.next_id = favorites.next_id.max(1);
        cx.set_global(favorites);
    }

    /// The tree, in the order the user keeps.
    pub(crate) fn all(cx: &App) -> Vec<FavoriteItem> {
        cx.try_global::<Self>()
            .map(|favorites| favorites.items.clone())
            .unwrap_or_default()
    }

    /// The entry `id`, wherever it sits in the tree.
    pub(crate) fn get(id: u32, cx: &App) -> Option<FavoriteItem> {
        cx.try_global::<Self>().and_then(|favorites| find(&favorites.items, id))
    }

    /// Adds `path` as a shortcut under `parent` (`None` = the top level),
    /// when it is a folder on disk and not a favorite already. Returns
    /// whether it was added.
    pub(crate) fn add_link(parent: Option<u32>, path: &str, cx: &mut App) -> bool {
        let path = normalize(path);
        if path.is_empty() || !std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            return false;
        }
        if Self::holds(cx, &path) {
            return false;
        }
        Self::change(cx, move |favorites| {
            let id = favorites.next_id;
            let item = FavoriteItem::link(id, &path);
            let placed = match parent {
                Some(parent) => insert_into(&mut favorites.items, parent, item),
                None => {
                    favorites.items.push(item);
                    true
                }
            };
            if placed {
                favorites.next_id += 1;
            }
            placed
        })
        .unwrap_or(false)
    }

    /// Makes a new, empty submenu under `parent` (`None` = the top level)
    /// and returns its id; 0 when the parent is not there.
    pub(crate) fn add_submenu(parent: Option<u32>, name: String, cx: &mut App) -> u32 {
        Self::change(cx, move |favorites| {
            let id = favorites.next_id;
            let item = FavoriteItem::submenu(id, name);
            let placed = match parent {
                Some(parent) => insert_into(&mut favorites.items, parent, item),
                None => {
                    favorites.items.push(item);
                    true
                }
            };
            if placed {
                favorites.next_id += 1;
                id
            } else {
                0
            }
        })
        .unwrap_or(0)
    }

    /// Names entry `id` again. The folder itself is never touched.
    pub(crate) fn rename(id: u32, name: String, cx: &mut App) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        Self::change(cx, move |favorites| {
            if let Some(item) = item_mut(&mut favorites.items, id) {
                item.name = name;
            }
        });
    }

    /// Drops entry `id` — a shortcut, or a submenu with everything it
    /// holds. Only the shortcuts are taken out; the folders themselves are
    /// never touched.
    pub(crate) fn remove(id: u32, cx: &mut App) {
        Self::change(cx, move |favorites| {
            remove_from(&mut favorites.items, id);
        });
    }

    /// Moves entry `id` under `new_parent` (`None` = the top level), the
    /// way dragging it onto another entry or onto the section would.
    /// Refused when the target is the entry itself or sits inside its own
    /// subtree - a submenu cannot hold itself. Returns whether it moved.
    pub(crate) fn move_item(id: u32, new_parent: Option<u32>, cx: &mut App) -> bool {
        Self::change(cx, move |favorites| {
            let valid = match new_parent {
                None => true,
                Some(parent) => {
                    parent != id
                        && find(&favorites.items, parent).is_some_and(|target| target.is_submenu())
                        && !is_within(&favorites.items, id, parent)
                }
            };
            if !valid || parent_of(&favorites.items, id) == new_parent {
                return false;
            }
            let Some(item) = take_out(&mut favorites.items, id) else {
                return false;
            };
            match new_parent {
                Some(parent) => match item_mut(&mut favorites.items, parent) {
                    Some(submenu) => {
                        submenu.children.push(item);
                        true
                    }
                    // The parent vanished mid-move; the entry comes back
                    // at the top level rather than being lost.
                    None => {
                        favorites.items.push(item);
                        false
                    }
                },
                None => {
                    favorites.items.push(item);
                    true
                }
            }
        })
        .unwrap_or(false)
    }

    /// The shortcuts of the whole tree, flattened in menu order, for the
    /// views that have no tree (the sidebar's band).
    pub(crate) fn flat_links(cx: &App) -> Vec<FavoriteItem> {
        let mut links = Vec::new();
        collect_links(&Self::all(cx), &mut links);
        links
    }

    /// The submenus of the tree, depth first, each with the names that lead
    /// to it (`Trabalho / Projetos`): the places the directory menu offers
    /// for putting the folder on screen into.
    pub(crate) fn submenus(cx: &App) -> Vec<(u32, String)> {
        let mut found = Vec::new();
        collect_submenus(&Self::all(cx), "", &mut found);
        found
    }

    /// Whether some entry already opens `path`.
    fn holds(cx: &App, path: &str) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|favorites| holds_path(&favorites.items, path))
    }

    // Applies `edit` and saves shortly after. Hands back what `edit`
    // returned, or `None` when there is nothing to change.
    fn change<R>(cx: &mut App, edit: impl FnOnce(&mut Self) -> R) -> Option<R> {
        if !cx.has_global::<Self>() {
            return None;
        }
        let result = cx.update_global(|favorites: &mut Self, cx| {
            let result = edit(favorites);
            let stored = Stored {
                items: favorites.items.clone(),
                next_id: favorites.next_id,
            };
            let (Some(store), Ok(bytes)) = (favorites.store.clone(), serde_json::to_vec(&stored)) else {
                return result;
            };
            favorites.save = Some(cx.background_spawn(async move {
                store.put(&FAVORITES_KEY, &bytes).log_err();
            }));
            result
        });
        cx.refresh_windows();
        Some(result)
    }
}

// The entry `id`, wherever it sits.
fn find(items: &[FavoriteItem], id: u32) -> Option<FavoriteItem> {
    for item in items {
        if item.id == id {
            return Some(item.clone());
        }
        if let Some(found) = find(&item.children, id) {
            return Some(found);
        }
    }
    None
}

// Takes entry `id` out of wherever it sits, returning it.
fn take_out(items: &mut Vec<FavoriteItem>, id: u32) -> Option<FavoriteItem> {
    let index = items.iter().position(|item| item.id == id);
    if let Some(index) = index {
        return Some(items.remove(index));
    }
    for item in items.iter_mut() {
        if let Some(found) = take_out(&mut item.children, id) {
            return Some(found);
        }
    }
    None
}

// The id of the submenu holding `id`; `None` when it sits at the top level
// (or is not in the tree at all).
fn parent_of(items: &[FavoriteItem], id: u32) -> Option<u32> {
    for item in items {
        if item.is_submenu() {
            if item.children.iter().any(|child| child.id == id) {
                return Some(item.id);
            }
            if let Some(found) = parent_of(&item.children, id) {
                return Some(found);
            }
        }
    }
    None
}

// Whether `candidate` sits inside the subtree of entry `root`.
fn is_within(items: &[FavoriteItem], root: u32, candidate: u32) -> bool {
    for item in items {
        if item.id == root {
            return subtree_has(item, candidate);
        }
        if item.is_submenu() && is_within(&item.children, root, candidate) {
            return true;
        }
    }
    false
}

fn subtree_has(item: &FavoriteItem, candidate: u32) -> bool {
    item.id == candidate || item.children.iter().any(|child| subtree_has(child, candidate))
}

// Inserts `item` inside submenu `parent`, wherever it sits. Returns whether
// the submenu was found.
fn insert_into(items: &mut [FavoriteItem], parent: u32, item: FavoriteItem) -> bool {
    if let Some(submenu) = item_mut(items, parent)
        && submenu.is_submenu()
    {
        submenu.children.push(item);
        return true;
    }
    false
}

// The entry `id`, wherever it sits.
fn item_mut(items: &mut [FavoriteItem], id: u32) -> Option<&mut FavoriteItem> {
    for entry in items {
        if entry.id == id {
            return Some(entry);
        }
        if let Some(found) = item_mut(&mut entry.children, id) {
            return Some(found);
        }
    }
    None
}

// Takes entry `id` out, wherever it sits.
fn remove_from(items: &mut Vec<FavoriteItem>, id: u32) -> bool {
    let before = items.len();
    items.retain(|item| item.id != id);
    if items.len() != before {
        return true;
    }
    items.iter_mut().any(|item| remove_from(&mut item.children, id))
}

fn holds_path(items: &[FavoriteItem], path: &str) -> bool {
    items.iter().any(|item| {
        (!item.is_submenu() && same_path(&item.path, path)) || holds_path(&item.children, path)
    })
}

fn collect_links(items: &[FavoriteItem], out: &mut Vec<FavoriteItem>) {
    for item in items {
        if item.is_submenu() {
            collect_links(&item.children, out);
        } else {
            out.push(item.clone());
        }
    }
}

// The submenus, depth first, named by the path of names leading to each so
// two submenus with the same name can be told apart in a flat menu.
fn collect_submenus(items: &[FavoriteItem], prefix: &str, out: &mut Vec<(u32, String)>) {
    for item in items {
        if !item.is_submenu() {
            continue;
        }
        let label = if prefix.is_empty() {
            item.name.clone()
        } else {
            format!("{prefix} / {}", item.name)
        };
        out.push((item.id, label.clone()));
        collect_submenus(&item.children, &label, out);
    }
}

/// The path as kept: without surrounding blanks and trailing separators,
/// except that a drive root keeps its backslash (`C:\`), so `C:\Users\` and
/// `C:\Users` are one favorite and `Q:\` stays a root.
fn normalize(path: &str) -> String {
    let trimmed = path.trim().trim_end_matches(['\\', '/']);
    if trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized_with_their_root_kept() {
        assert_eq!(normalize(r"C:\Users\Bobo\"), r"C:\Users\Bobo");
        assert_eq!(normalize(r"C:\Users\Bobo"), r"C:\Users\Bobo");
        assert_eq!(normalize("Q:\\"), "Q:\\");
        assert_eq!(normalize(" Q:\\ "), "Q:\\");
        assert_eq!(normalize(r"\\nas\edicao\"), r"\\nas\edicao");
        assert_eq!(normalize(""), "");
    }

    #[gpui::test]
    fn submenus_are_listed_depth_first_with_the_names_leading_to_them(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            Favorites::install(None, cx);
            let work = Favorites::add_submenu(None, "Trabalho".into(), cx);
            let projects = Favorites::add_submenu(Some(work), "Projetos".into(), cx);
            let personal = Favorites::add_submenu(None, "Pessoal".into(), cx);
            assert_eq!(
                Favorites::submenus(cx),
                vec![
                    (work, "Trabalho".to_string()),
                    (projects, "Trabalho / Projetos".to_string()),
                    (personal, "Pessoal".to_string()),
                ],
                "the directory menu can name every place to put the folder into"
            );
        });
    }

    #[gpui::test]
    fn entries_move_between_levels_without_cycles(cx: &mut gpui::TestAppContext) {
        let folder = tempfile::tempdir().expect("temp folder");
        let one = folder.path().join("one");
        let two = folder.path().join("two");
        std::fs::create_dir(&one).expect("dir");
        std::fs::create_dir(&two).expect("dir");
        cx.update(|cx| {
            Favorites::install(None, cx);
            assert!(Favorites::add_link(None, one.to_str().expect("utf-8"), cx));
            assert!(Favorites::add_link(None, two.to_str().expect("utf-8"), cx));
            let submenu = Favorites::add_submenu(None, "Group".into(), cx);
            assert_ne!(submenu, 0);
            // The first shortcut joins the submenu.
            assert!(Favorites::move_item(1, Some(submenu), cx));
            let tree = Favorites::all(cx);
            assert_eq!(tree.len(), 2, "one shortcut now lives inside the submenu");
            assert_eq!(tree[1].children.len(), 1);
            // Already there: nothing changes.
            assert!(!Favorites::move_item(1, Some(submenu), cx));
            // A submenu cannot take itself, nor hold its own subtree.
            assert!(!Favorites::move_item(submenu, Some(submenu), cx));
            let inner = Favorites::add_submenu(Some(submenu), "Inner".into(), cx);
            assert!(!Favorites::move_item(submenu, Some(inner), cx));
            // Back to the top level it goes; the inner submenu stays.
            assert!(Favorites::move_item(1, None, cx));
            let tree = Favorites::all(cx);
            assert_eq!(tree.len(), 3);
            assert!(tree[1].children.iter().all(|item| item.is_submenu()));
        });
    }

    #[gpui::test]
    fn favorites_are_shortcuts_only(cx: &mut gpui::TestAppContext) {
        let folder = tempfile::tempdir().expect("temp folder");
        let work = folder.path().join("work");
        std::fs::create_dir(&work).expect("folder");
        let file = folder.path().join("note.txt");
        std::fs::write(&file, "1").expect("write");
        let work = work.to_string_lossy().into_owned();
        let file = file.to_string_lossy().into_owned();
        cx.update(|cx| {
            Favorites::install(None, cx);
            // Only folders on disk are added; files are left out.
            assert!(!Favorites::add_link(None, &file, cx));
            assert!(Favorites::add_link(None, &work, cx));
            // The same folder under another spelling is one favorite.
            assert!(!Favorites::add_link(None, &format!("{work}\\"), cx));
            assert!(!Favorites::add_link(None, &work.to_uppercase(), cx));
            assert_eq!(Favorites::all(cx).len(), 1);
            // Removing takes the shortcut out and leaves the folder alone.
            Favorites::remove(Favorites::all(cx)[0].id, cx);
            assert!(Favorites::all(cx).is_empty());
            assert!(std::fs::metadata(&work).is_ok_and(|meta| meta.is_dir()));
        });
    }

    #[gpui::test]
    fn submenus_hold_their_own_and_follow_a_rename(cx: &mut gpui::TestAppContext) {
        let folder = tempfile::tempdir().expect("temp folder");
        let one = folder.path().join("one");
        let two = folder.path().join("two");
        std::fs::create_dir(&one).expect("folder");
        std::fs::create_dir(&two).expect("folder");
        let one = one.to_string_lossy().into_owned();
        let two = two.to_string_lossy().into_owned();
        cx.update(|cx| {
            Favorites::install(None, cx);
            let submenu = Favorites::add_submenu(None, "Trabalho".into(), cx);
            assert!(submenu > 0);
            assert!(Favorites::add_link(Some(submenu), &one, cx));
            // The same folder cannot come in twice, wherever it sits.
            assert!(!Favorites::add_link(None, &one, cx));
            assert!(Favorites::add_link(Some(submenu), &two, cx));
            let tree = Favorites::all(cx);
            assert_eq!(tree.len(), 1, "the submenu is the only top entry");
            assert!(tree[0].is_submenu());
            assert_eq!(tree[0].children.len(), 2);
            assert_eq!(tree[0].children[0].path, one);
            // A shortcut is named after its folder until renamed.
            Favorites::rename(tree[0].children[0].id, "Um".into(), cx);
            assert_eq!(Favorites::get(tree[0].children[0].id, cx).expect("item").name, "Um");
            Favorites::rename(submenu, "Job".into(), cx);
            assert_eq!(Favorites::get(submenu, cx).expect("submenu").name, "Job");
            // Deleting the submenu takes its shortcuts out, folders stay.
            Favorites::remove(submenu, cx);
            assert!(Favorites::all(cx).is_empty());
            assert!(std::fs::metadata(&one).is_ok_and(|meta| meta.is_dir()));
            assert!(std::fs::metadata(&two).is_ok_and(|meta| meta.is_dir()));
        });
    }
}
