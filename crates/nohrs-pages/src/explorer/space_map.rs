//! The space map: what a folder weighs, drawn as a treemap of its contents.
//!
//! The measurements come from a walk of the folder ([`nohrs_services::fs::space`]),
//! which runs off the UI thread and reports its progress to the status line
//! while it goes. The map lives beside the listing: it is opened from the
//! sidebar or the listing's "View" menu, replaces the listing while it is up,
//! and follows the pane when the user navigates to another folder.
//!
//! Inside the map, a click zooms into a directory — no second read, the scan
//! already has its contents — and the breadcrumb above the mosaic walks back
//! out. A file's tile shows where it is: clicking it opens its folder with the
//! file selected.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::{AppContext as _, Bounds, Context, Pixels, Task, WeakEntity};
use nohrs_core::telemetry::LogErr;
use nohrs_services::fs::space::{SpaceError, SpaceLimits, SpaceNode, SpaceProgress};

use super::ExplorerPane;
use super::places::{THIS_PC, THIS_PC_NAME};

/// How often the status line is repainted while a scan runs: often enough to
/// see the counts move, rarely enough that the walk keeps the cores.
const PROGRESS_TICK: Duration = Duration::from_millis(120);

/// How many levels below the folder the indexer is asked for: the mosaic
/// draws three, so one request fills the first view.
const INDEX_LEVELS: u16 = 3;

/// Tests that count bytes need the walk's apparent sizes, and a folder they
/// have just created may not be in the indexer's table yet: measurements from
/// the index can be turned off for them. Never set outside tests.
#[cfg(test)]
pub(crate) static INDEX_OFF: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// What measured the map's tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpaceSource {
    /// The indexer service's copy of the names, answered from its memory.
    Index,
    /// A walk of the folder itself.
    Filesystem,
}

/// The mosaic as last laid out, kept between frames: it is rebuilt only when
/// the tree, the zoomed-in directory or the painted area changes.
pub(crate) struct SpaceLayout {
    /// Parents before their own children; see `space::layout`.
    pub tiles: Rc<Vec<nohrs_services::fs::space::Tile>>,
    /// The scan this layout was built from.
    generation: u64,
    /// The directory inside the tree it draws.
    focus: Vec<usize>,
    /// The area it was laid out for.
    width: f32,
    height: f32,
}

/// An open space map, and the scan filling it.
pub(crate) struct SpaceMap {
    /// The folder being measured.
    pub(crate) root: String,
    /// The pane's location when the map was opened, or last followed it. A
    /// map opened on a folder picked in the listing measures that folder while
    /// the pane stays where it is; once the pane goes somewhere else, the map
    /// follows it instead.
    anchor: String,
    /// The scanned tree, once there is one.
    pub(crate) tree: Option<Rc<SpaceNode>>,
    /// What measured the tree: the indexer's memory, or a walk of the folder.
    pub(crate) source: Option<SpaceSource>,
    /// The directory the mosaic is looking at, as child indices from `tree`.
    pub(crate) focus: Vec<usize>,
    /// Counters of the running scan, read for the status line.
    pub(crate) progress: Arc<SpaceProgress>,
    /// Why the folder could not be measured, when it could not be.
    pub(crate) error: Option<String>,
    /// Where the mosaic was painted last frame, for hit-testing the pointer.
    pub(crate) viewport: Rc<Cell<Bounds<Pixels>>>,
    /// The mosaic as last laid out.
    pub(crate) layout: Option<SpaceLayout>,
    /// The tile under the pointer, as an index into `layout.tiles`.
    pub(crate) hover: Option<usize>,
    /// When the scan started, for the "taking a while" line.
    pub(crate) started: Instant,
    /// Bumped for every scan, so an answer to an older one is ignored.
    generation: u64,
    /// Read by the walk between entries: set when the map moves on.
    cancel: Arc<AtomicBool>,
    /// The tasks of the running scan and its progress ticker.
    tasks: Vec<Task<()>>,
    limits: SpaceLimits,
}

impl SpaceMap {
    fn starting(root: String, anchor: String) -> Self {
        Self {
            root,
            anchor,
            tree: None,
            source: None,
            focus: Vec::new(),
            progress: Arc::new(SpaceProgress::default()),
            error: None,
            viewport: Rc::new(Cell::new(Bounds::default())),
            layout: None,
            hover: None,
            started: Instant::now(),
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            tasks: Vec::new(),
            limits: SpaceLimits::default(),
        }
    }

    /// The directory the mosaic draws: the tree's root, or the folder zoomed
    /// into. `None` before the scan has an answer.
    pub(crate) fn focus_node(&self) -> Option<&SpaceNode> {
        self.tree.as_deref()?.node_at(&self.focus)
    }

    /// Whether the scan is still running, for the "scanning" line.
    pub(crate) fn is_scanning(&self) -> bool {
        self.tree.is_none() && self.error.is_none()
    }

    /// The layout `generation` `focus` last produced for an area, if it is
    /// still the one to draw.
    fn cached(
        &self,
        generation: u64,
        focus: &[usize],
        width: f32,
        height: f32,
    ) -> Option<Rc<Vec<nohrs_services::fs::space::Tile>>> {
        let layout = self.layout.as_ref()?;
        (layout.generation == generation
            && layout.focus == focus
            && (layout.width - width).abs() < 0.5
            && (layout.height - height).abs() < 0.5)
            .then(|| Rc::clone(&layout.tiles))
    }
}

impl ExplorerPane {
    /// Whether `location` is something on disk that can be measured. Tag and
    /// archive locations and the Shell's namespaces are not: they list things
    /// from several folders rather than being folders themselves.
    pub(crate) fn measurable(location: &str) -> bool {
        !(location.starts_with(super::tags::TAG_PREFIX)
            || location.starts_with(super::archive::PREFIX)
            || location.starts_with("::"))
    }

    /// Whether the map is showing.
    pub(crate) fn space_map_open(&self) -> bool {
        self.space.is_some()
    }

    /// Opens the map over the listing, measuring the folder the pane is in.
    /// Nothing happens when it is already open.
    pub(crate) fn open_space_map(&mut self, cx: &mut Context<Self>) {
        if self.space.is_some() {
            return;
        }
        let cwd = self.cwd.clone();
        self.space = Some(SpaceMap::starting(cwd.clone(), cwd));
        self.start_space_scan(cx);
        cx.notify();
    }

    /// Opens the map on `folder`, picked in the listing, without leaving the
    /// folder the pane is in: the scan and the mosaic measure that folder, and
    /// the pane keeps its listing behind the map.
    pub(crate) fn open_space_map_on(&mut self, folder: String, cx: &mut Context<Self>) {
        if folder.eq_ignore_ascii_case(&self.cwd) {
            self.open_space_map(cx);
            return;
        }
        let anchor = self.cwd.clone();
        self.space = Some(SpaceMap::starting(folder, anchor));
        self.start_space_scan(cx);
        cx.notify();
    }

    /// Takes the map down, letting the listing show through again.
    pub(crate) fn close_space_map(&mut self, cx: &mut Context<Self>) {
        if let Some(map) = self.space.take() {
            // The walk stops at its next entry instead of finishing unseen.
            map.cancel.store(true, Ordering::Relaxed);
            cx.notify();
        }
    }

    /// Opens the map, or takes it down when it is already up.
    pub(crate) fn toggle_space_map(&mut self, cx: &mut Context<Self>) {
        if self.space.is_some() {
            self.close_space_map(cx);
        } else {
            self.open_space_map(cx);
        }
    }

    /// Called every frame with the map open: makes it follow the pane's
    /// navigation, and builds the drive mosaic of This PC once the sidebar
    /// knows the drives.
    pub(crate) fn tick_space_map(&mut self, cx: &mut Context<Self>) {
        let Some((root, anchor)) = self
            .space
            .as_ref()
            .map(|map| (map.root.clone(), map.anchor.clone()))
        else {
            return;
        };
        // A map opened on a folder picked in the listing stays on it while the
        // pane stays where it was; moving the pane is a new place to measure.
        if anchor == self.cwd {
            // This PC's mosaic is the drives' used space, which the shell
            // answers on its own thread: kept in step here, and left alone
            // while the answers have not changed.
            if root.eq_ignore_ascii_case(THIS_PC) {
                self.refresh_drives(cx);
            }
            return;
        }
        // Navigating to what cannot be measured — a tag's page, an archive —
        // takes the map down: there is nothing there to weigh.
        if !Self::measurable(&self.cwd) && !self.cwd.eq_ignore_ascii_case(THIS_PC) {
            self.close_space_map(cx);
            return;
        }
        let cwd = self.cwd.clone();
        self.space = Some(SpaceMap::starting(cwd.clone(), cwd));
        self.start_space_scan(cx);
        cx.notify();
    }

    /// The mosaic for the map's root, laid out for `width` by `height`. The
    /// layout is kept between frames; only a new tree, another directory or a
    /// resize builds it again.
    pub(crate) fn space_tiles(
        &mut self,
        width: f32,
        height: f32,
    ) -> Option<Rc<Vec<nohrs_services::fs::space::Tile>>> {
        let map = self.space.as_ref()?;
        let generation = map.generation;
        let focus = map.focus.clone();
        if let Some(tiles) = map.cached(generation, &focus, width, height) {
            return Some(tiles);
        }
        let node = map.focus_node()?;
        let area = nohrs_services::fs::space::Rect::new(0.0, 0.0, width, height);
        let tiles = Rc::new(nohrs_services::fs::space::layout(
            node,
            area,
            &nohrs_services::fs::space::LayoutOptions::default(),
        ));
        if let Some(map) = self.space.as_mut() {
            map.layout = Some(SpaceLayout {
                tiles: Rc::clone(&tiles),
                generation,
                focus,
                width,
                height,
            });
        }
        Some(tiles)
    }

    /// Zooms the mosaic into the directory at `crumbs`, keeping the scan.
    pub(crate) fn space_zoom_to(&mut self, crumbs: Vec<usize>, cx: &mut Context<Self>) {
        let Some(map) = self.space.as_mut() else {
            return;
        };
        if map
            .tree
            .as_deref()
            .and_then(|tree| tree.node_at(&crumbs))
            .is_none()
        {
            return;
        }
        map.focus = crumbs;
        map.layout = None;
        map.hover = None;
        cx.notify();
    }

    /// Zooms out one directory, as the breadcrumb's parent does.
    pub(crate) fn space_zoom_out(&mut self, cx: &mut Context<Self>) {
        let Some(map) = self.space.as_mut() else {
            return;
        };
        if map.focus.pop().is_some() {
            map.layout = None;
            map.hover = None;
            cx.notify();
        }
    }

    /// The tile that was clicked: a directory is zoomed into (or measured
    /// again when the scan did not keep its contents), a file is shown in its
    /// folder with the listing.
    pub(crate) fn space_click(
        &mut self,
        crumbs: Vec<usize>,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some((drives, is_dir, has_children, path)) = self.space.as_ref().and_then(|map| {
            let node = map.tree.as_deref()?.node_at(&crumbs)?;
            Some((
                map.root.eq_ignore_ascii_case(THIS_PC),
                node.is_dir,
                !node.children.is_empty(),
                node.path.clone(),
            ))
        }) else {
            return;
        };
        if is_dir {
            if drives || !Self::measurable(&path) {
                // A drive in the This PC mosaic: open it, and the map follows
                // the pane and measures it.
                if !path.is_empty() {
                    self.change_dir(path, window, cx);
                }
                return;
            }
            if has_children {
                self.space_zoom_to(crumbs, cx);
                return;
            }
            // A directory whose contents the scan summed rather than kept:
            // measuring it on its own is what shows what is in it.
            self.change_dir(path, window, cx);
            return;
        }
        // A file: show it in the listing, in the folder that holds it.
        if path.is_empty() {
            return;
        }
        let folder = parent_of(&path).unwrap_or_else(|| self.cwd.clone());
        let same_folder = super::same_path(&folder, &self.cwd);
        self.close_space_map(cx);
        if same_folder {
            self.reveal(&path);
            cx.notify();
        } else {
            self.change_dir(folder, window, cx);
            self.reveal(&path);
        }
    }

    /// Measures the map's root again, from scratch.
    pub(crate) fn space_refresh(&mut self, cx: &mut Context<Self>) {
        if self.space.is_none() {
            return;
        }
        self.start_space_scan(cx);
        cx.notify();
    }

    /// Starts the walk behind the map, replacing any running one. The walk
    /// itself runs on the background executor; the tree arrives through
    /// [`Self::finish_space_scan`], and a ticker repaints the progress line
    /// while it is on its way.
    fn start_space_scan(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.space.as_ref().map(|map| map.root.clone()) else {
            return;
        };
        if root.eq_ignore_ascii_case(THIS_PC) {
            // The whole computer is a mosaic of its drives, whose used space
            // the shell already answered: no walk of a drive is needed.
            self.reset_space_scan();
            self.refresh_drives(cx);
            return;
        }
        self.reset_space_scan();
        let Some(map) = self.space.as_mut() else {
            return;
        };
        let generation = map.generation;
        let limits = map.limits;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(SpaceProgress::default());
        map.cancel = Arc::clone(&cancel);
        map.progress = Arc::clone(&progress);

        let scan_root = root.clone();
        let scan_cancel = Arc::clone(&cancel);
        let scan_progress = Arc::clone(&progress);
        let scan = cx.background_spawn(async move {
            // The indexer service already holds every name and the size of
            // everything each folder holds: asking it is a scan of its memory,
            // where walking the folder itself takes minutes on a full drive.
            // Only a folder it cannot answer for is walked.
            if let Some(tree) = index_tree(&scan_root) {
                scan_progress.finish();
                return Ok((tree, SpaceSource::Index));
            }
            nohrs_services::fs::space::scan(&scan_root, &scan_cancel, &scan_progress, &limits)
                .map(|tree| (tree, SpaceSource::Filesystem))
        });
        let runner = cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let outcome = scan.await;
            this.update(cx, |pane, cx| {
                pane.finish_space_scan(generation, &root, outcome, cx)
            })
            .log_err();
        });
        let ticker = cx.spawn(async move |this: WeakEntity<Self>, cx| {
            loop {
                cx.background_executor().timer(PROGRESS_TICK).await;
                let alive = this
                    .update(cx, |pane, cx| {
                        let Some(map) = pane.space.as_ref() else {
                            return false;
                        };
                        if map.generation != generation {
                            return false;
                        }
                        cx.notify();
                        // The answer may land between two ticks; the ticker
                        // stops once it has.
                        !map.progress.is_finished()
                    })
                    .unwrap_or(false);
                if !alive {
                    return;
                }
            }
        });
        if let Some(map) = self.space.as_mut() {
            map.tasks.push(runner);
            map.tasks.push(ticker);
        }
    }

    /// Forgets the tree and starts a new generation, so answers to the walk
    /// that was running are ignored. The walk itself is started by the caller.
    fn reset_space_scan(&mut self) {
        let Some(map) = self.space.as_mut() else {
            return;
        };
        map.cancel.store(true, Ordering::Relaxed);
        map.tasks.clear();
        map.generation += 1;
        map.tree = None;
        map.source = None;
        map.error = None;
        map.focus.clear();
        map.layout = None;
        map.hover = None;
        map.started = Instant::now();
    }

    /// The scan's answer, for the scan that asked for it.
    fn finish_space_scan(
        &mut self,
        generation: u64,
        root: &str,
        outcome: Result<(SpaceNode, SpaceSource), SpaceError>,
        cx: &mut Context<Self>,
    ) {
        let Some(map) = self.space.as_mut() else {
            return;
        };
        // A later scan has taken over: this answer is about a folder nobody is
        // looking at any more.
        if map.generation != generation || map.root != root {
            return;
        }
        match outcome {
            Ok((tree, source)) => {
                map.tree = Some(Rc::new(tree));
                map.source = Some(source);
                map.error = None;
            }
            Err(SpaceError::Cancelled) => return,
            Err(SpaceError::Root(reason)) => map.error = Some(reason),
        }
        map.layout = None;
        cx.notify();
    }

    /// Builds the This PC mosaic out of the drives the sidebar knows, each
    /// sized by the space in use. Asked for on every frame while that mosaic
    /// is up: the drive readings arrive from a thread of their own, and this
    /// picks them up without repainting when they have not moved.
    fn refresh_drives(&mut self, cx: &mut Context<Self>) {
        // The drives come from the sidebar's own reading; waiting for it is
        // what the next frame is for (its arrival notifies).
        if self.sidebar.places.is_none() {
            self.ensure_places(cx);
            return;
        }
        let places = self.sidebar.places.clone().unwrap_or_default();
        let mut children: Vec<SpaceNode> = Vec::new();
        for drive in &places.drives {
            let Some(path) = drive.path.clone() else {
                continue;
            };
            let used = super::drives::space(&path, cx)
                .map(|space| space.total.saturating_sub(space.free))
                .unwrap_or(0);
            children.push(SpaceNode {
                name: drive.name.clone(),
                path,
                // A drive whose space the shell has not answered for yet is
                // still worth a tile to click on.
                bytes: used.max(1),
                is_dir: true,
                count: 1,
                children: Vec::new(),
            });
        }
        children.sort_by(|left, right| {
            right
                .bytes
                .cmp(&left.bytes)
                .then_with(|| left.name.cmp(&right.name))
        });
        let name = places
            .this_pc
            .as_ref()
            .map(|place| place.name.clone())
            .unwrap_or_else(|| nohrs_core::i18n::tr(THIS_PC_NAME).to_string());
        // Repainting is only worth it when a drive was added, taken away or
        // filled up since the last frame.
        let unchanged = self
            .space
            .as_ref()
            .and_then(|map| map.tree.as_deref())
            .is_some_and(|tree| {
                tree.name == name
                    && tree.children.len() == children.len()
                    && tree
                        .children
                        .iter()
                        .zip(&children)
                        .all(|(old, new)| old.path == new.path && old.bytes == new.bytes)
            });
        if unchanged {
            return;
        }
        let tree = SpaceNode {
            name,
            path: THIS_PC.to_string(),
            bytes: children.iter().map(|child| child.bytes).sum(),
            is_dir: true,
            count: children.len() as u64,
            children,
        };
        if let Some(map) = self.space.as_mut() {
            map.tree = Some(Rc::new(tree));
            map.layout = None;
        }
        cx.notify();
    }
}

/// The map's tree from the indexer, when it can answer for the folder: the
/// folder and [`INDEX_LEVELS`] below it, each with the size of everything
/// under it, in the time a pipe message takes.
fn index_tree(root: &str) -> Option<SpaceNode> {
    #[cfg(test)]
    if INDEX_OFF.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let entries = super::everything::mft::subtree(root, INDEX_LEVELS)?;
    tree_from_index(root, entries)
}

/// Turns the indexer's flat answer into the tree the mosaic draws. The
/// entries arrive parents first, so one forward pass gives each its path and
/// one backward pass puts every child under its parent.
fn tree_from_index(
    root: &str,
    entries: Vec<nohrs_indexer::protocol::TreeNode>,
) -> Option<SpaceNode> {
    let first = entries.first()?;
    if first.parent != nohrs_indexer::protocol::NO_PARENT {
        return None;
    }
    let mut paths: Vec<String> = Vec::with_capacity(entries.len());
    let mut nodes: Vec<Option<SpaceNode>> = Vec::with_capacity(entries.len());
    for entry in &entries {
        let path = if entry.parent == nohrs_indexer::protocol::NO_PARENT {
            root.to_string()
        } else {
            let mut path = paths.get(entry.parent as usize)?.clone();
            if !path.ends_with(['\\', '/']) {
                path.push('\\');
            }
            path.push_str(&entry.name);
            path
        };
        paths.push(path.clone());
        nodes.push(Some(SpaceNode {
            name: entry.name.clone(),
            path,
            bytes: entry.size,
            is_dir: entry.dir,
            count: 1,
            children: Vec::new(),
        }));
    }
    for index in (1..entries.len()).rev() {
        let Some(node) = nodes.get_mut(index).and_then(Option::take) else {
            continue;
        };
        let parent = entries.get(index).map_or(0, |entry| entry.parent as usize);
        if let Some(slot) = nodes.get_mut(parent).and_then(Option::as_mut) {
            slot.children.push(node);
        }
    }
    nodes.into_iter().next().flatten()
}

/// The folder that holds `path`, by its last separator.
fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let cut = trimmed.rfind(['\\', '/'])?;
    let parent = &trimmed[..cut];
    // `C:` without the separator is the current folder on that drive, not its
    // root; keeping the separator is what makes it the root.
    Some(if parent.ends_with(':') {
        format!("{parent}\\")
    } else if parent.is_empty() {
        trimmed[..=cut].to_string()
    } else {
        parent.to_string()
    })
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::{parent_of, tree_from_index};
    use nohrs_indexer::protocol::{NO_PARENT, TreeNode};

    fn entry(parent: u32, name: &str, dir: bool, size: u64) -> TreeNode {
        TreeNode {
            parent,
            name: name.to_string(),
            dir,
            size,
            modified: 0,
        }
    }

    #[test]
    fn a_files_folder_keeps_its_drive_root() {
        assert_eq!(
            parent_of(r"C:\Users\BoBO\file.txt").as_deref(),
            Some(r"C:\Users\BoBO")
        );
        assert_eq!(parent_of(r"C:\file.txt").as_deref(), Some(r"C:\"));
        assert_eq!(
            parent_of("C:/Users/BoBO/file.txt").as_deref(),
            Some("C:/Users/BoBO")
        );
        assert_eq!(parent_of("file.txt"), None);
    }

    #[test]
    fn the_index_turns_into_the_tree_the_mosaic_draws() {
        // The service's own answer for C:\Work: Work (800), two files, a
        // folder holding one, in table order, sizes already summed.
        let entries = vec![
            entry(NO_PARENT, "Work", true, 800),
            entry(0, "big.bin", false, 500),
            entry(0, "Projetos", true, 300),
            entry(2, "notas.txt", false, 300),
        ];
        let tree = tree_from_index(r"C:\Work", entries).expect("a tree");
        assert_eq!(
            (tree.name.as_str(), tree.path.as_str(), tree.bytes),
            ("Work", r"C:\Work", 800)
        );
        assert_eq!(tree.children.len(), 2);
        let big = tree
            .children
            .iter()
            .find(|child| child.name == "big.bin")
            .expect("the file");
        assert_eq!(
            (big.path.as_str(), big.bytes, big.is_dir),
            (r"C:\Work\big.bin", 500, false)
        );
        let folder = tree
            .children
            .iter()
            .find(|child| child.name == "Projetos")
            .expect("the folder");
        assert_eq!(
            (folder.path.as_str(), folder.bytes, folder.is_dir),
            (r"C:\Work\Projetos", 300, true)
        );
        let note = folder.children.first().expect("the file in it");
        assert_eq!(
            (note.path.as_str(), note.bytes),
            (r"C:\Work\Projetos\notas.txt", 300)
        );
        // The crumbs the map zooms by address the same nodes. The children are
        // kept newest-first by the assembly; the mosaic sorts them anyway, so
        // the position is what the answer's own order gives.
        let folder_at = tree
            .children
            .iter()
            .position(|child| child.name == "Projetos")
            .expect("the folder's place");
        assert_eq!(
            tree.node_at(&[folder_at, 0]).map(|node| node.name.as_str()),
            Some("notas.txt")
        );
    }

    #[test]
    fn an_index_answer_out_of_shape_is_refused() {
        assert_eq!(tree_from_index("C:\\", Vec::new()), None);
        let orphan = vec![entry(7, "orphan", false, 1)];
        assert_eq!(tree_from_index("C:\\", orphan), None);
        // A parent past the end of the answer cannot be placed either.
        let missing = vec![
            entry(NO_PARENT, "root", true, 1),
            entry(9, "lost", false, 1),
        ];
        assert_eq!(tree_from_index("C:\\", missing), None);
    }

    #[test]
    fn a_drive_root_tree_names_itself_after_the_drive() {
        let entries = vec![
            entry(NO_PARENT, "C:", true, 1000),
            entry(0, "Windows", true, 900),
        ];
        let tree = tree_from_index("C:\\", entries).expect("a tree");
        assert_eq!(tree.name, "C:");
        assert_eq!(tree.path, "C:\\");
        assert_eq!(tree.children[0].path, r"C:\Windows");
        assert_eq!(tree.children[0].bytes, 900);
    }
}
