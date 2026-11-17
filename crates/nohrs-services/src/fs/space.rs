//! What a folder weighs, as a tree, and the squarified treemap that draws it.
//!
//! A folder's size is not listed anywhere: it is the sum of everything under
//! it. [`scan`] walks the tree once and reports that sum for every directory,
//! keeping the largest entries of each so the tree stays small enough to hold
//! in memory for a whole drive.
//!
//! [`layout`] then turns that tree into the mosaic of rectangles a disk-usage
//! map paints: Bruls, Huizing and van Wijk's squarified layout, which grows a
//! row of tiles while the worst aspect ratio in it keeps improving, instead of
//! the slivers a naive slice-and-dice leaves.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// How much of a tree a scan keeps, and how deep it looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceLimits {
    /// Entries held in the tree before the walk stops materialising any more.
    ///
    /// The sizes stay exact past the limit: the walk still descends, it just
    /// stops keeping the detail. Memory is the reason for the cap, not time.
    pub max_nodes: usize,
    /// Entries kept per directory; the smallest are summed into one tile so a
    /// folder of a hundred thousand files does not become a hundred thousand
    /// rectangles nobody can read.
    pub max_children: usize,
    /// Depth kept as entries, counting the scanned root as 0. Below it
    /// everything is summed into the deepest directory's own size.
    pub max_depth: u32,
}

impl Default for SpaceLimits {
    fn default() -> Self {
        Self {
            max_nodes: 60_000,
            max_children: 160,
            max_depth: 12,
        }
    }
}

/// One entry of a scanned tree, or one tile's worth of several entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceNode {
    /// The entry's name; empty for a tile that stands for merged entries.
    pub name: String,
    /// The entry's absolute path; empty for merged entries.
    pub path: String,
    /// Everything this entry holds, in bytes. Its own length for a file.
    pub bytes: u64,
    /// Files below this node, excluding directories.
    pub files: u64,
    /// Files below this node whose entries are hidden.
    pub hidden_files: u64,
    /// Newest last-write time below this node, in Unix seconds.
    pub modified: u64,
    /// Whether this entry itself is hidden.
    pub hidden: bool,
    /// Bytes below this node belonging to hidden entries.
    pub hidden_bytes: u64,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// How many files and folders this node stands for. One for a real entry,
    /// the number merged for an aggregated tile.
    pub count: u64,
    /// The largest children, largest first; empty for a file and for a
    /// directory whose contents were summed rather than kept.
    pub children: Vec<SpaceNode>,
}

impl SpaceNode {
    /// A single file, as the scanner records it.
    fn file(name: String, path: String, bytes: u64) -> Self {
        let hidden = name.starts_with('.');
        Self {
            name,
            path,
            bytes,
            files: 1,
            hidden_files: u64::from(hidden),
            modified: 0,
            hidden,
            hidden_bytes: if hidden { bytes } else { 0 },
            is_dir: false,
            count: 1,
            children: Vec::new(),
        }
    }

    /// The deepest node a list of child indices names, or `None` when one of
    /// them is out of range (a stale position after a rescan).
    pub fn node_at<'a>(&'a self, crumbs: &[usize]) -> Option<&'a SpaceNode> {
        let mut node = self;
        for index in crumbs {
            node = node.children.get(*index)?;
        }
        Some(node)
    }
}

/// Why a scan stopped without a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpaceError {
    /// The folder was navigated away from, or the map was closed.
    Cancelled,
    /// The root itself could not be read, with the reason the system gave.
    Root(String),
}

impl std::fmt::Display for SpaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("scan cancelled"),
            Self::Root(reason) => write!(formatter, "{reason}"),
        }
    }
}

impl std::error::Error for SpaceError {}

/// The counters a running scan keeps for the frame that shows its progress.
#[derive(Debug, Default)]
pub struct SpaceProgress {
    files: AtomicU64,
    directories: AtomicU64,
    bytes: AtomicU64,
    unreadable: AtomicU64,
    finished: AtomicBool,
}

/// A consistent reading of [`SpaceProgress`], for one frame's status line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaceSnapshot {
    /// Files met so far.
    pub files: u64,
    /// Directories met so far, the root included.
    pub directories: u64,
    /// Bytes of the files met so far.
    pub bytes: u64,
    /// Directories that could not be read.
    pub unreadable: u64,
}

impl SpaceProgress {
    /// A reading of the counters. The figures come from separate atomics, so
    /// they are close to a moment rather than exactly of one; the status line
    /// only needs them to move forward.
    pub fn snapshot(&self) -> SpaceSnapshot {
        SpaceSnapshot {
            files: self.files.load(Ordering::Relaxed),
            directories: self.directories.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            unreadable: self.unreadable.load(Ordering::Relaxed),
        }
    }

    /// Whether the walk is over, whatever the outcome.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
}

impl SpaceProgress {
    fn add_files(&self, count: u64, bytes: u64) {
        if count > 0 {
            self.files.fetch_add(count, Ordering::Relaxed);
        }
        if bytes > 0 {
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    fn add_directory(&self) {
        self.directories.fetch_add(1, Ordering::Relaxed);
    }

    fn add_unreadable(&self) {
        self.unreadable.fetch_add(1, Ordering::Relaxed);
    }

    /// Marks the measurement over. Called at the end of [`scan`], and by a
    /// caller whose tree came from somewhere else and has no walk to count.
    pub fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }
}

/// What stays the same for the whole walk.
struct ScanState<'a> {
    cancel: &'a AtomicBool,
    progress: &'a SpaceProgress,
    limits: SpaceLimits,
    /// Entries materialised so far, against [`SpaceLimits::max_nodes`].
    nodes: usize,
}

/// Measures everything under `root`, keeping the largest entries of each
/// directory.
///
/// Cancellation is cooperative: `cancel` is read between entries, so a scan
/// of a whole drive stops within a directory rather than at the end. The
/// progress counters are updated as the walk goes, for a status line.
pub fn scan(
    root: &str,
    cancel: &AtomicBool,
    progress: &SpaceProgress,
    limits: &SpaceLimits,
) -> Result<SpaceNode, SpaceError> {
    let path = Path::new(root);
    let metadata = std::fs::metadata(path).map_err(|error| SpaceError::Root(error.to_string()))?;
    let mut state = ScanState {
        cancel,
        progress,
        limits: *limits,
        nodes: 1,
    };
    let tree = if metadata.is_dir() {
        directory_node(root, path, 0, &mut state)
    } else {
        progress.add_files(1, metadata.len());
        SpaceNode {
            name: root.to_string(),
            path: root.to_string(),
            bytes: metadata.len(),
            files: 1,
            hidden_files: u64::from(is_hidden(root, &metadata)),
            modified: modified_seconds(&metadata),
            hidden: is_hidden(root, &metadata),
            hidden_bytes: if is_hidden(root, &metadata) {
                metadata.len()
            } else {
                0
            },
            is_dir: false,
            count: 1,
            children: Vec::new(),
        }
    };
    progress.finish();
    if cancel.load(Ordering::Relaxed) {
        return Err(SpaceError::Cancelled);
    }
    Ok(tree)
}

/// A directory's node, its largest children, and the bytes of everything in
/// it that is not kept as a child.
fn directory_node(name: &str, path: &Path, depth: u32, state: &mut ScanState<'_>) -> SpaceNode {
    state.progress.add_directory();
    let mut node = SpaceNode {
        name: name.to_string(),
        path: path.to_string_lossy().into_owned(),
        bytes: 0,
        files: 0,
        hidden_files: 0,
        modified: 0,
        hidden: path_hidden(path),
        hidden_bytes: 0,
        is_dir: true,
        count: 1,
        children: Vec::new(),
    };
    let reader = match std::fs::read_dir(path) {
        Ok(reader) => reader,
        Err(_) => {
            state.progress.add_unreadable();
            return node;
        }
    };
    let mut total = 0_u64;
    let mut entries = 0_u64;
    // File counts and sizes are batched per directory instead of per entry:
    // one read-modify-write cycle for the status line is enough per folder.
    let mut counted_files = 0_u64;
    let mut counted_bytes = 0_u64;
    for entry in reader.flatten() {
        if state.cancel.load(Ordering::Relaxed) {
            break;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        // Links and junctions are not followed: a link back up the tree would
        // count the same bytes twice, and Windows' junctions make that easy to
        // do by accident.
        if file_type.is_symlink() {
            continue;
        }
        let child_name = entry.file_name().to_string_lossy().into_owned();
        let child_path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let hidden = is_hidden(&child_name, &metadata);
        node.modified = node.modified.max(modified_seconds(&metadata));
        if file_type.is_dir() {
            entries += 1;
            if depth < state.limits.max_depth && state.nodes < state.limits.max_nodes {
                state.nodes += 1;
                let mut child = directory_node(&child_name, &child_path, depth + 1, state);
                child.hidden = hidden;
                if hidden {
                    child.hidden_bytes = child.bytes;
                    child.hidden_files = child.files;
                }
                total += child.bytes;
                node.files += child.files;
                node.hidden_files += if hidden {
                    child.files
                } else {
                    child.hidden_files
                };
                node.modified = node.modified.max(child.modified);
                node.hidden_bytes += child.hidden_bytes;
                node.children.push(child);
            } else {
                // Sizes have to stay exact even where the detail is dropped.
                let summary = sum_directory(&child_path, state);
                total += summary.bytes;
                node.files += summary.files;
                node.hidden_files += if hidden {
                    summary.files
                } else {
                    summary.hidden_files
                };
                node.modified = node.modified.max(summary.modified);
                node.hidden_bytes += if hidden {
                    summary.bytes
                } else {
                    summary.hidden_bytes
                };
            }
            continue;
        }
        let bytes = metadata.len();
        total += bytes;
        entries += 1;
        node.files += 1;
        node.hidden_files += u64::from(hidden);
        node.hidden_bytes += if hidden { bytes } else { 0 };
        counted_files += 1;
        counted_bytes += bytes;
        if depth < state.limits.max_depth && state.nodes < state.limits.max_nodes {
            state.nodes += 1;
            let mut child =
                SpaceNode::file(child_name, child_path.to_string_lossy().into_owned(), bytes);
            child.modified = modified_seconds(&metadata);
            child.hidden = hidden;
            child.hidden_files = u64::from(hidden);
            child.hidden_bytes = if hidden { bytes } else { 0 };
            node.children.push(child);
        }
    }
    state.progress.add_files(counted_files, counted_bytes);

    // Whatever is not a child of its own is still counted, in one tile: the
    // entries past the node or depth limit, and the smallest children a folder
    // is too full to draw. `total` was added to as the walk went, so the
    // difference against what is kept is exactly what the merged tile holds.
    node.children.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.name.cmp(&right.name))
    });
    let kept: u64 = node.children.iter().map(|child| child.bytes).sum();
    let mut merged_bytes = total.saturating_sub(kept);
    let mut merged_entries = entries.saturating_sub(node.children.len() as u64);
    let kept_files: u64 = node.children.iter().map(|child| child.files).sum();
    let kept_hidden_files: u64 = node.children.iter().map(|child| child.hidden_files).sum();
    let kept_hidden_bytes: u64 = node.children.iter().map(|child| child.hidden_bytes).sum();
    let mut merged_files = node.files.saturating_sub(kept_files);
    let mut merged_hidden_files = node.hidden_files.saturating_sub(kept_hidden_files);
    let mut merged_hidden_bytes = node.hidden_bytes.saturating_sub(kept_hidden_bytes);
    if node.children.len() > state.limits.max_children {
        let tail = node.children.split_off(state.limits.max_children);
        for child in tail {
            merged_bytes = merged_bytes.saturating_add(child.bytes);
            merged_entries = merged_entries.saturating_add(child.count);
            merged_files = merged_files.saturating_add(child.files);
            merged_hidden_files = merged_hidden_files.saturating_add(child.hidden_files);
            merged_hidden_bytes = merged_hidden_bytes.saturating_add(child.hidden_bytes);
        }
    }
    if merged_bytes > 0 || merged_entries > 0 {
        node.children.push(SpaceNode {
            // No name: the frame draws it as "the rest", with its size.
            name: String::new(),
            path: String::new(),
            bytes: merged_bytes,
            files: merged_files,
            hidden_files: merged_hidden_files,
            modified: node.modified,
            hidden: false,
            hidden_bytes: merged_hidden_bytes,
            is_dir: false,
            count: merged_entries,
            children: Vec::new(),
        });
    }
    node.bytes = node.children.iter().map(|child| child.bytes).sum();
    node.files = node.children.iter().map(|child| child.files).sum();
    node.hidden_files = node.children.iter().map(|child| child.hidden_files).sum();
    node.hidden_bytes = node.children.iter().map(|child| child.hidden_bytes).sum();
    node
}

/// The bytes under `path`, without keeping any of it: what a directory below
/// the depth or node limit still contributes to its parent's size.
#[derive(Default)]
struct SpaceSummary {
    bytes: u64,
    files: u64,
    hidden_files: u64,
    modified: u64,
    hidden_bytes: u64,
}

fn sum_directory(path: &Path, state: &mut ScanState<'_>) -> SpaceSummary {
    state.progress.add_directory();
    let reader = match std::fs::read_dir(path) {
        Ok(reader) => reader,
        Err(_) => {
            state.progress.add_unreadable();
            return SpaceSummary::default();
        }
    };
    let mut summary = SpaceSummary::default();
    let mut files = 0_u64;
    let mut direct_bytes = 0_u64;
    for entry in reader.flatten() {
        if state.cancel.load(Ordering::Relaxed) {
            break;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let child = sum_directory(&entry.path(), state);
            let hidden = is_hidden(&entry.file_name().to_string_lossy(), &metadata);
            summary.bytes += child.bytes;
            summary.files += child.files;
            summary.hidden_files += if hidden {
                child.files
            } else {
                child.hidden_files
            };
            summary.hidden_bytes += if hidden {
                child.bytes
            } else {
                child.hidden_bytes
            };
            summary.modified = summary
                .modified
                .max(child.modified)
                .max(modified_seconds(&metadata));
            continue;
        }
        if let Ok(metadata) = entry.metadata() {
            let bytes = metadata.len();
            let hidden = is_hidden(&entry.file_name().to_string_lossy(), &metadata);
            summary.bytes += bytes;
            summary.files += 1;
            summary.hidden_files += u64::from(hidden);
            summary.hidden_bytes += if hidden { bytes } else { 0 };
            summary.modified = summary.modified.max(modified_seconds(&metadata));
            files += 1;
            direct_bytes += bytes;
        }
    }
    state.progress.add_files(files, direct_bytes);
    summary
}

fn modified_seconds(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs())
}

fn path_hidden(path: &Path) -> bool {
    let Some(name) = path.file_name().map(|name| name.to_string_lossy()) else {
        return false;
    };
    if name.starts_with('.') {
        return true;
    }
    std::fs::metadata(path)
        .ok()
        .is_some_and(|metadata| is_hidden(&name, &metadata))
}

fn is_hidden(name: &str, metadata: &std::fs::Metadata) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return metadata.file_attributes() & 0x2 != 0;
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

/// An axis-aligned rectangle in mosaic pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// A rectangle from its corner and size.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// The right edge.
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    /// The bottom edge.
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// Whether the point is inside the rectangle; its bottom and right edges
    /// are outside it, so neighbouring tiles never both claim a pixel.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// The rectangle shrunk by `padding` on every side, never past empty.
    #[must_use]
    pub fn inset(&self, padding: f32) -> Self {
        Self::new(
            self.x + padding,
            self.y + padding,
            (self.w - padding * 2.0).max(0.0),
            (self.h - padding * 2.0).max(0.0),
        )
    }
}

/// One rectangle of the mosaic.
#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    /// Where it is.
    pub rect: Rect,
    /// Child indices from the laid-out root: `[2]` is the root's third child,
    /// `[2, 0]` its first child's first child.
    pub crumbs: Vec<usize>,
    /// How deep below the laid-out root: 0 for its own children.
    pub depth: u32,
    /// The band a subdivided directory keeps for its own name. Its children
    /// are laid out below it, so no label ever sits on another tile.
    pub header: Option<Rect>,
}

/// How much of a tree to draw at once, and how finely.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutOptions {
    /// Levels drawn together; 1 draws only the root's children.
    pub max_depth: u32,
    /// Gap between siblings, and inset into their parent.
    pub padding: f32,
    /// Gap between the root's children: wider, so the first level of structure
    /// reads before the detail inside it.
    pub padding_outer: f32,
    /// Tiles smaller than this are dropped; they cannot be read or hit.
    pub min_tile: f32,
    /// Height of the band a top-level directory keeps for its name.
    pub header: f32,
    /// Height of the slimmer band a deeper directory keeps.
    pub header_inner: f32,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            max_depth: 3,
            padding: 1.5,
            padding_outer: 3.0,
            min_tile: 4.0,
            // Tall enough for the name a tile draws in it at the map's label
            // size: a band that clips its own text reads as a bug.
            header: 22.0,
            header_inner: 18.0,
        }
    }
}

/// Lays the children of `root` out inside `area`, and their children inside
/// them, down to [`LayoutOptions::max_depth`].
///
/// Tiles come out parents first: a parent precedes its own children, which is
/// what lets a view paint them in order and hit-test in reverse.
pub fn layout(root: &SpaceNode, area: Rect, options: &LayoutOptions) -> Vec<Tile> {
    layout_with_hidden(root, area, options, true, false)
}

/// Lays out visible bytes, optionally excluding hidden entries from each area.
pub fn layout_with_hidden(
    root: &SpaceNode,
    area: Rect,
    options: &LayoutOptions,
    include_hidden: bool,
    by_files: bool,
) -> Vec<Tile> {
    let mut tiles = Vec::new();
    let mut crumbs = Vec::new();
    place_children(
        root,
        area,
        options,
        include_hidden,
        by_files,
        0,
        &mut crumbs,
        &mut tiles,
    );
    tiles
}

fn place_children(
    node: &SpaceNode,
    area: Rect,
    options: &LayoutOptions,
    include_hidden: bool,
    by_files: bool,
    depth: u32,
    crumbs: &mut Vec<usize>,
    out: &mut Vec<Tile>,
) {
    if node.children.is_empty() || area.w <= 0.0 || area.h <= 0.0 {
        return;
    }
    // Ranked here rather than trusted from the scan: a hand-built tree must not
    // produce a bad layout.
    let mut order: Vec<usize> = (0..node.children.len())
        .filter(|index| visible_weight(&node.children[*index], include_hidden, by_files) > 0)
        .collect();
    order.sort_by(|left, right| {
        visible_weight(&node.children[*right], include_hidden, by_files)
            .cmp(&visible_weight(
                &node.children[*left],
                include_hidden,
                by_files,
            ))
            .then_with(|| node.children[*left].name.cmp(&node.children[*right].name))
    });
    if order.is_empty() {
        return;
    }
    let values: Vec<f64> = order
        .iter()
        .map(|index| visible_weight(&node.children[*index], include_hidden, by_files) as f64)
        .collect();
    let rects = squarify(&values, area);
    for (slot, rect) in rects.iter().enumerate() {
        let Some(index) = order.get(slot).copied() else {
            continue;
        };
        let rect = rect.inset(if depth == 0 {
            options.padding_outer
        } else {
            options.padding
        });
        if rect.w < options.min_tile || rect.h < options.min_tile {
            continue;
        }
        let child = &node.children[index];
        // A directory about to be subdivided keeps a band for its own name;
        // too small a tile stays whole instead, and zooming in gives it the
        // room back. A directory whose contents were summed rather than kept
        // has nothing to put below a band.
        let subdivided =
            child.is_dir && !child.children.is_empty() && depth + 1 < options.max_depth;
        let header = subdivided
            .then(|| header_band(rect, options, depth))
            .flatten();
        crumbs.push(index);
        out.push(Tile {
            rect,
            crumbs: crumbs.clone(),
            depth,
            header,
        });
        if let Some(header) = header {
            let body = Rect::new(
                rect.x,
                header.bottom(),
                rect.w,
                (rect.bottom() - header.bottom()).max(0.0),
            );
            place_children(
                child,
                body,
                options,
                include_hidden,
                by_files,
                depth + 1,
                crumbs,
                out,
            );
        }
        crumbs.pop();
    }
}

fn visible_bytes(node: &SpaceNode, include_hidden: bool) -> u64 {
    if include_hidden {
        node.bytes
    } else {
        node.bytes.saturating_sub(node.hidden_bytes)
    }
}

fn visible_weight(node: &SpaceNode, include_hidden: bool, by_files: bool) -> u64 {
    if by_files {
        let files = if include_hidden {
            node.files
        } else {
            node.files.saturating_sub(node.hidden_files)
        };
        if files > 0 {
            return files;
        }
    }
    visible_bytes(node, include_hidden)
}

/// The band a directory keeps for its name, or `None` when the tile is too
/// small to leave its children a usable area below it.
fn header_band(rect: Rect, options: &LayoutOptions, depth: u32) -> Option<Rect> {
    let height = if depth == 0 {
        options.header
    } else {
        options.header_inner
    };
    let body = rect.h - height;
    if rect.w < 44.0 || body < options.min_tile * 3.0 {
        return None;
    }
    Some(Rect::new(rect.x, rect.y, rect.w, height))
}

/// Splits `area` into one rectangle per value, proportional to it.
///
/// Returned in the order of `values`, not in layout order.
pub fn squarify(values: &[f64], area: Rect) -> Vec<Rect> {
    let mut rects = vec![Rect::default(); values.len()];
    let total: f64 = values.iter().filter(|value| **value > 0.0).sum();
    if total <= 0.0 || area.w <= 0.0 || area.h <= 0.0 {
        return rects;
    }
    let mut order: Vec<usize> = (0..values.len())
        .filter(|index| values[*index] > 0.0)
        .collect();
    order.sort_by(|left, right| values[*right].total_cmp(&values[*left]));
    let scale = f64::from(area.w) * f64::from(area.h) / total;
    let areas: Vec<f64> = order.iter().map(|index| values[*index] * scale).collect();

    let mut free = area;
    let mut start = 0;
    while start < areas.len() {
        let side = f64::from(free.w.min(free.h));
        let mut end = start + 1;
        let mut row_sum = areas[start];
        let mut row_worst = worst_ratio(&areas[start..end], row_sum, side);
        while end < areas.len() {
            let candidate_sum = row_sum + areas[end];
            let candidate_worst = worst_ratio(&areas[start..=end], candidate_sum, side);
            if candidate_worst > row_worst {
                break;
            }
            row_sum = candidate_sum;
            row_worst = candidate_worst;
            end += 1;
        }
        if free.w >= free.h {
            // A vertical strip on the left; its tiles stack top to bottom.
            let strip_w = ((row_sum / f64::from(free.h)) as f32).min(free.w);
            let mut y = free.y;
            for index in start..end {
                let height = if strip_w > 0.0 {
                    (areas[index] / f64::from(strip_w)) as f32
                } else {
                    0.0
                };
                let height = height.min(free.bottom() - y).max(0.0);
                rects[order[index]] = Rect::new(free.x, y, strip_w, height);
                y += height;
            }
            free.x += strip_w;
            free.w = (free.w - strip_w).max(0.0);
        } else {
            // A horizontal strip along the top; its tiles run left to right.
            let strip_h = ((row_sum / f64::from(free.w)) as f32).min(free.h);
            let mut x = free.x;
            for index in start..end {
                let width = if strip_h > 0.0 {
                    (areas[index] / f64::from(strip_h)) as f32
                } else {
                    0.0
                };
                let width = width.min(free.right() - x).max(0.0);
                rects[order[index]] = Rect::new(x, free.y, width, strip_h);
                x += width;
            }
            free.y += strip_h;
            free.h = (free.h - strip_h).max(0.0);
        }
        start = end;
    }
    rects
}

/// The worst (largest) aspect ratio in a row of `areas` laid along `side`.
fn worst_ratio(areas: &[f64], row_sum: f64, side: f64) -> f64 {
    if row_sum <= 0.0 || side <= 0.0 {
        return f64::INFINITY;
    }
    let thickness = row_sum / side;
    areas.iter().fold(0.0_f64, |worst, area| {
        if *area <= 0.0 || thickness <= 0.0 {
            return worst;
        }
        let other = area / thickness;
        let ratio = (thickness / other).max(other / thickness);
        worst.max(ratio)
    })
}

/// The deepest tile containing a point.
///
/// Children are emitted after their parent and lie inside it, so the last
/// match in reverse order is the most specific one.
pub fn hit(tiles: &[Tile], x: f32, y: f32) -> Option<&Tile> {
    tiles.iter().rev().find(|tile| tile.rect.contains(x, y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file(name: &str, bytes: u64) -> SpaceNode {
        SpaceNode::file(name.to_string(), format!(r"C:\x\{name}"), bytes)
    }

    fn directory(name: &str, children: Vec<SpaceNode>) -> SpaceNode {
        let bytes = children.iter().map(|child| child.bytes).sum();
        let count = children.iter().map(|child| child.count).sum();
        SpaceNode {
            name: name.to_string(),
            path: format!(r"C:\x\{name}"),
            bytes,
            files: children.iter().map(|child| child.files).sum(),
            hidden_files: children.iter().map(|child| child.hidden_files).sum(),
            modified: children
                .iter()
                .map(|child| child.modified)
                .max()
                .unwrap_or(0),
            hidden: false,
            hidden_bytes: children.iter().map(|child| child.hidden_bytes).sum(),
            is_dir: true,
            count,
            children,
        }
    }

    fn area() -> Rect {
        Rect::new(0.0, 0.0, 800.0, 500.0)
    }

    #[test]
    fn hidden_entries_are_removed_from_the_layout_without_reindexing_nodes() {
        let root = directory(
            "root",
            vec![file("visible.txt", 60), file(".hidden.txt", 40)],
        );
        let all = layout_with_hidden(&root, area(), &LayoutOptions::default(), true, false);
        let visible = layout_with_hidden(&root, area(), &LayoutOptions::default(), false, false);
        assert_eq!(2, all.len());
        assert_eq!(1, visible.len());
        assert_eq!(
            Some("visible.txt"),
            root.node_at(&visible[0].crumbs)
                .map(|node| node.name.as_str())
        );
    }

    #[test]
    fn file_mode_sizes_tiles_by_file_count() {
        let many_files = directory(
            "many",
            vec![file("one", 1), file("two", 1), file("three", 1)],
        );
        let root = directory("root", vec![file("large.bin", 100), many_files]);
        let by_size = layout_with_hidden(&root, area(), &LayoutOptions::default(), true, false);
        let by_files = layout_with_hidden(&root, area(), &LayoutOptions::default(), true, true);
        assert_eq!(
            Some("large.bin"),
            root.node_at(&by_size[0].crumbs)
                .map(|node| node.name.as_str())
        );
        assert_eq!(
            Some("many"),
            root.node_at(&by_files[0].crumbs)
                .map(|node| node.name.as_str())
        );
    }

    #[test]
    fn squarify_fills_the_area_and_stays_inside_it() {
        let values = [40.0, 30.0, 20.0, 5.0, 3.0, 2.0];
        let rects = squarify(&values, area());
        let covered: f32 = rects.iter().map(|rect| rect.w * rect.h).sum();
        assert!(
            (covered - area().w * area().h).abs() < 1.0,
            "covered {covered}"
        );
        for rect in &rects {
            assert!(rect.x >= -0.01 && rect.y >= -0.01);
            assert!(rect.right() <= area().right() + 0.01);
            assert!(rect.bottom() <= area().bottom() + 0.01);
        }
    }

    #[test]
    fn squarify_keeps_aspect_ratios_readable() {
        // The canonical squarify example: 6, 6, 4, 3, 2, 2, 1 on a 6x4 canvas.
        let values = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let rects = squarify(&values, Rect::new(0.0, 0.0, 600.0, 400.0));
        for rect in &rects {
            let ratio = (rect.w / rect.h).max(rect.h / rect.w);
            assert!(ratio <= 4.0, "aspect ratio {ratio} in {rect:?}");
        }
    }

    #[test]
    fn squarify_survives_degenerate_input() {
        assert_eq!(squarify(&[], area()).len(), 0);
        assert!(
            squarify(&[0.0, 0.0], area())
                .iter()
                .all(|rect| rect.w * rect.h < f32::EPSILON)
        );
        assert!(squarify(&[1.0], Rect::new(0.0, 0.0, 0.0, 10.0))[0].w < f32::EPSILON);
    }

    #[test]
    fn layout_nests_children_inside_their_parent_and_keeps_a_header() {
        let root = directory(
            "root",
            vec![
                directory("big", vec![file("inside", 100), file("also", 50)]),
                file("small", 10),
            ],
        );
        let tiles = layout(&root, area(), &LayoutOptions::default());
        let crumbs: Vec<&[usize]> = tiles.iter().map(|tile| tile.crumbs.as_slice()).collect();
        assert_eq!(crumbs, vec![&[0][..], &[0, 0], &[0, 1], &[1]]);
        let parent = &tiles[0];
        let child = &tiles[1];
        assert_eq!(child.depth, 1);
        assert!(parent.rect.contains(child.rect.x + 0.5, child.rect.y + 0.5));
        let header = parent.header.expect("a subdivided directory names itself");
        assert!((header.y - parent.rect.y).abs() < f32::EPSILON);
        for tile in tiles.iter().filter(|tile| tile.crumbs.len() > 1) {
            assert!(tile.rect.y >= header.bottom() - f32::EPSILON);
        }
        // The band belongs to the parent when hit-tested.
        let centre = (header.x + header.w / 2.0, header.y + header.h / 2.0);
        assert_eq!(
            hit(&tiles, centre.0, centre.1).map(|tile| &tile.crumbs),
            Some(&vec![0])
        );
    }

    #[test]
    fn layout_depth_one_stops_at_direct_children() {
        let root = directory("root", vec![directory("big", vec![file("inside", 100)])]);
        let options = LayoutOptions {
            max_depth: 1,
            ..LayoutOptions::default()
        };
        let tiles = layout(&root, area(), &options);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].crumbs, vec![0]);
        assert!(tiles[0].header.is_none());
    }

    #[test]
    fn layout_skips_tiles_too_small_to_read() {
        let root = directory("root", vec![file("huge", 1_000_000), file("sliver", 1)]);
        let tiles = layout(&root, area(), &LayoutOptions::default());
        assert!(tiles.iter().all(|tile| tile.crumbs != vec![1]));
    }

    #[test]
    fn hit_returns_the_deepest_tile() {
        let root = directory(
            "root",
            vec![
                directory("big", vec![file("inside", 100)]),
                file("small", 1),
            ],
        );
        let tiles = layout(&root, area(), &LayoutOptions::default());
        let child = tiles
            .iter()
            .find(|tile| tile.crumbs == vec![0, 0])
            .expect("the child tile");
        let centre = (
            child.rect.x + child.rect.w / 2.0,
            child.rect.y + child.rect.h / 2.0,
        );
        assert_eq!(
            hit(&tiles, centre.0, centre.1).map(|tile| &tile.crumbs),
            Some(&vec![0, 0])
        );
        assert!(hit(&tiles, -50.0, -50.0).is_none());
    }

    #[test]
    fn node_at_walks_the_crumbs_and_rejects_stale_ones() {
        let root = directory(
            "root",
            vec![
                directory("big", vec![file("inside", 100)]),
                file("small", 1),
            ],
        );
        assert_eq!(
            root.node_at(&[]).map(|node| node.name.as_str()),
            Some("root")
        );
        assert_eq!(
            root.node_at(&[0, 0]).map(|node| node.name.as_str()),
            Some("inside")
        );
        assert!(root.node_at(&[2]).is_none());
        assert!(root.node_at(&[0, 9]).is_none());
    }

    /// A temporary tree: `folder/one.bin` of 3000 bytes, `folder/two.bin` of
    /// 2000, and `folder/nested/three.bin` of 5000, so every sum is known.
    #[allow(clippy::disallowed_methods)]
    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let nested = folder.join("nested");
        std::fs::create_dir(&nested).unwrap();
        for (path, size) in [
            (folder.join("one.bin"), 3000_usize),
            (folder.join("two.bin"), 2000),
            (nested.join("three.bin"), 5000),
        ] {
            let mut file = std::fs::File::create(path).unwrap();
            file.write_all(&vec![b'x'; size]).unwrap();
        }
        root
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn a_scan_sums_every_directory_and_finishes() {
        let root = fixture();
        let folder = root.path().join("folder");
        let cancel = AtomicBool::new(false);
        let progress = SpaceProgress::default();
        let tree = scan(
            folder.to_str().unwrap(),
            &cancel,
            &progress,
            &SpaceLimits::default(),
        )
        .unwrap();
        assert_eq!(tree.bytes, 10_000);
        assert!(progress.is_finished());
        assert_eq!(progress.snapshot().files, 3);
        assert_eq!(progress.snapshot().bytes, 10_000);
        let names: Vec<&str> = tree
            .children
            .iter()
            .map(|child| child.name.as_str())
            .collect();
        // Largest first, and the nested folder counts everything under it.
        assert_eq!(names, ["nested", "one.bin", "two.bin"]);
        let nested = tree.node_at(&[0]).unwrap();
        assert_eq!(nested.bytes, 5000);
        assert_eq!(nested.node_at(&[0]).map(|node| node.bytes), Some(5000));
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn a_scan_records_file_count_age_and_hidden_space() {
        let root = tempfile::tempdir().unwrap();
        for (name, size) in [("visible.bin", 60_usize), (".private.bin", 40)] {
            let mut file = std::fs::File::create(root.path().join(name)).unwrap();
            file.write_all(&vec![b'x'; size]).unwrap();
        }
        let tree = scan(
            root.path().to_str().unwrap(),
            &AtomicBool::new(false),
            &SpaceProgress::default(),
            &SpaceLimits::default(),
        )
        .unwrap();
        assert_eq!(2, tree.files);
        assert_eq!(1, tree.hidden_files);
        assert_eq!(40, tree.hidden_bytes);
        assert!(tree.modified > 0);
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn a_scan_sums_what_it_does_not_keep_into_one_tile() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..20 {
            let mut file =
                std::fs::File::create(root.path().join(format!("f{index:02}.bin"))).unwrap();
            file.write_all(&vec![b'x'; 100 + index]).unwrap();
        }
        let cancel = AtomicBool::new(false);
        let progress = SpaceProgress::default();
        let limits = SpaceLimits {
            max_children: 5,
            ..SpaceLimits::default()
        };
        let tree = scan(root.path().to_str().unwrap(), &cancel, &progress, &limits).unwrap();
        let total: u64 = (100..120).sum();
        assert_eq!(tree.bytes, total);
        assert_eq!(tree.children.len(), 6, "five kept and one merged");
        let merged = tree.children.last().expect("the merged tile");
        assert!(merged.name.is_empty());
        assert_eq!(merged.count, 15);
        assert_eq!(
            tree.children.iter().map(|child| child.bytes).sum::<u64>(),
            total
        );
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn a_cancelled_scan_stops_and_says_so() {
        let root = fixture();
        let cancel = AtomicBool::new(true);
        let progress = SpaceProgress::default();
        let outcome = scan(
            root.path().to_str().unwrap(),
            &cancel,
            &progress,
            &SpaceLimits::default(),
        );
        assert_eq!(outcome, Err(SpaceError::Cancelled));
        assert!(progress.is_finished());
    }

    #[test]
    fn an_unreadable_root_is_an_error_not_a_panic() {
        let cancel = AtomicBool::new(false);
        let progress = SpaceProgress::default();
        let outcome = scan(
            r"C:\nohrs-this-folder-does-not-exist",
            &cancel,
            &progress,
            &SpaceLimits::default(),
        );
        assert!(matches!(outcome, Err(SpaceError::Root(_))));
    }

    #[test]
    fn depth_limit_still_counts_deeper_bytes() {
        let mut deep = file("deep.bin", 700);
        for level in 0..4 {
            deep = directory(&format!("d{level}"), vec![deep]);
        }
        // The hand-built tree is already summed; the scan's depth limit is
        // covered by `a_scan_sums_what_it_does_not_keep_into_one_tile`.
        assert_eq!(deep.bytes, 700);
        assert_eq!(
            deep.node_at(&[0, 0, 0, 0]).map(|node| node.name.as_str()),
            Some("deep.bin")
        );
    }
}
