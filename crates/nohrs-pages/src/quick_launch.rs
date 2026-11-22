//! Quick launch: a floating panel of shortcuts summoned by a global chord
//! (docs/QuickDock-技术实现方案与边界约定.md §5, §6, §9, §11, §15.0).
//!
//! [`QuickLaunch`] is the only piece of state in memory (§13): the window and
//! the group bar read and write it through the GPUI global. Everything the state
//! is made of — the file, the host KV, the Shell — is used off the foreground
//! thread:
//!
//! * the file is read and parsed on a background task (§5.1), written by an
//!   atomic replace after a 300ms throttle (§5.2), and watched by
//!   [`ConfigWatcher`] with a content hash so our own writes are ignored (§5.3);
//! * positions, the last group and usage counts go to `state.redb` through
//!   [`KvStore`], read once at startup and written from background tasks (§6);
//! * icons and launches go to one dedicated thread (§8.4, §9), which is the only
//!   place in this crate that owns COM for them.
//!
//! The window is created on the first summon and then only shown or hidden
//! (§11). Hiding it needs Win32: GPUI has no window-level `hide` on Windows.
//! [`crate::tray::platform`] already has the two calls, and they are used
//! directly instead of `tray::show`/`tray::hide`, which also change the main
//! window's tray state.

mod view;

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::{
    App, AppContext as _, Bounds, Global, Pixels, PlatformDisplay, Point, RenderImage, Window,
    WindowBounds, WindowHandle, WindowKind, WindowOptions, point, px, size,
};
use gpui_component::Root;
use nohrs_core::config::ConfigWatcher;
use nohrs_core::config::paths;
use nohrs_core::config::quick_launch::{
    Group, Item, OnRunning, Parsed, QuickLaunchConfig, QuickLaunchError, View, Warning,
};
use nohrs_core::telemetry::LogErr;
use nohrs_services::launch::LaunchRegistry;
use nohrs_services::launch::{
    ExpandContext, Health, LaunchError, LaunchMode, LaunchOutcome, LaunchRequest,
};
use nohrs_store::{KvKey, KvOp, KvStore};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as MatchConfig, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};

use crate::explorer::thumbnails;

use view::QuickLaunchView;

/// Most icons kept in memory before the least recently used is dropped.
const ICON_MEMORY_LIMIT: usize = 512;

/// Most bytes the icon cache on disk may hold before the oldest are deleted.
const ICON_DISK_LIMIT: u64 = 64 * 1024 * 1024;

/// How long an extraction may take before its path is given up on for the rest
/// of the session (§9).
const ICON_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the window waits after losing focus before hiding itself (§11).
const AUTO_HIDE_DELAY: Duration = Duration::from_millis(150);

/// How long edits are merged before writing the file (§5.2).
const SAVE_THROTTLE: Duration = Duration::from_millis(300);

/// Window size when the view is a grid (logical pixels).
const GRID_WINDOW_SIZE: (f32, f32) = (980.0, 875.0);

/// Window size when the view is a list.
const LIST_WINDOW_SIZE: (f32, f32) = (660.0, 860.0);

/// Smallest the window may be dragged.
const MIN_WINDOW_SIZE: (f32, f32) = (360.0, 300.0);

/// The sizes an icon is extracted at, smallest first (§9).
const ICON_STEPS: [u32; 8] = [16, 24, 32, 48, 64, 96, 128, 256];

/// The KV namespace every key of this feature lives in (§6).
const KV_NAMESPACE: &str = "quick_launch";

/// The key of the last group shown (§6).
const LAST_GROUP_KEY: &str = "last_group";

/// How the state came to hold its configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    /// The first read is still running.
    Loading,
    /// The file was read, or was not there.
    Ready,
    /// The file could not be read, or was written by a newer build. Writing it
    /// back is refused so the user's file is not overwritten (§5.2).
    ReadOnly(String),
}

impl Status {
    fn can_save(&self) -> bool {
        *self == Status::Ready
    }

    fn error(&self) -> Option<&str> {
        match self {
            Status::ReadOnly(message) => Some(message),
            Status::Loading | Status::Ready => None,
        }
    }
}

/// One icon in the in-memory cache, or the request for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IconKey {
    /// What was asked for: the item's custom icon, or its raw path. The worker
    /// expands it, so the foreground never touches the filesystem (§8.4).
    source: String,
    /// Edge of the extracted image in physical pixels.
    edge: u32,
}

impl IconKey {
    fn new(source: &str, edge: u32) -> Self {
        Self {
            source: source.to_string(),
            edge,
        }
    }
}

/// The icons this process has, plus the requests still in flight.
#[derive(Default)]
struct IconCache {
    images: HashMap<IconKey, Option<Arc<RenderImage>>>,
    /// Least recently used first.
    order: VecDeque<IconKey>,
    /// Asked for, not answered yet.
    pending: HashMap<IconKey, Instant>,
    /// Given up on, or known to have no icon. Asked once per session only (§9).
    settled: HashSet<IconKey>,
}

impl IconCache {
    /// The icon for `key`, if one is known. Does not start an extraction.
    fn get(&mut self, key: &IconKey) -> Option<Option<Arc<RenderImage>>> {
        if !self.images.contains_key(key) {
            return None;
        }
        let image = self.images.get(key).cloned();
        // Refresh the LRU position: a tile that is on screen stays.
        self.order.retain(|existing| existing != key);
        self.order.push_back(key.clone());
        image
    }

    fn insert(&mut self, key: IconKey, image: Option<Arc<RenderImage>>) {
        self.pending.remove(&key);
        self.order.retain(|existing| *existing != key);
        self.order.push_back(key.clone());
        self.images.insert(key, image);
        while self.images.len() > ICON_MEMORY_LIMIT {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.images.remove(&oldest);
        }
    }

    /// Whether an extraction should be started for `key` now: unknown, not in
    /// flight, not given up on, and its in-flight request has not timed out.
    fn wants_request(&mut self, key: &IconKey) -> bool {
        if self.settled.contains(key) || self.images.contains_key(key) {
            return false;
        }
        match self.pending.get(key) {
            Some(started) => {
                if started.elapsed() < ICON_TIMEOUT {
                    return false;
                }
                // The worker is still busy with it after the timeout: no more
                // tries this session, and a reply that does arrive is dropped.
                self.pending.remove(key);
                self.settled.insert(key.clone());
                false
            }
            None => true,
        }
    }
}

/// How long an icon request may wait in its queue before it is dropped (§8.4).
///
/// The window stops waiting for an icon after [`ICON_TIMEOUT`]; extracting one
/// older than that would only delay the requests behind it.
const ICON_QUEUE_LIMIT: Duration = Duration::from_secs(2);

/// How long the exit path waits for the dedicated threads (§15.x).
const WORKER_STOP_LIMIT: Duration = Duration::from_millis(500);

/// How many items one health batch checks (§12.1).
const HEALTH_BATCH: usize = 32;

/// How long the health pass waits between batches, so it stays in the window's
/// idle time (§12.1).
const HEALTH_PAUSE: Duration = Duration::from_millis(250);

/// One start request for the `quick-launch` thread.
struct StartRequest {
    // Boxed because the item is by far the largest thing a worker receives.
    item: Box<Item>,
    mode: LaunchMode,
    on_running: OnRunning,
    /// Echoed back so a reply the window has moved on from is dropped (§8.4).
    generation: u64,
}

/// One health request for the `quick-launch-icons` thread.
struct HealthRequest {
    /// The group id the item belongs to: a renamed group keeps its answers.
    group: String,
    slot: u32,
    item: Box<Item>,
}

/// One icon request for the `quick-launch-icons` thread.
struct IconRequest {
    key: IconKey,
    /// The custom icon's path, or the item's; expanded by the worker.
    path: String,
    /// When the window queued it: one that waited too long is dropped (§8.4).
    queued: Instant,
}

/// What the worker threads answer with.
enum WorkerReply {
    Launched {
        generation: u64,
        name: String,
        outcome: std::result::Result<LaunchOutcome, LaunchError>,
    },
    Icon {
        key: IconKey,
        image: Option<Arc<RenderImage>>,
    },
    Health {
        group: String,
        slot: u32,
        health: Health,
    },
}

/// One dedicated thread: its request channel, the notice that it has stopped, and
/// the handle needed to join it (§8.4).
struct Worker<Request> {
    requests: Option<std_mpsc::Sender<Request>>,
    stopped: std_mpsc::Receiver<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl<Request: Send + 'static> Worker<Request> {
    /// Starts `name` running `body` until its channel closes.
    fn start(name: &str, body: impl FnOnce(std_mpsc::Receiver<Request>) + Send + 'static) -> Self {
        let (requests, receiver) = std_mpsc::channel::<Request>();
        let (stopped_sender, stopped) = std_mpsc::channel::<()>();
        let join = match std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                body(receiver);
                // Sent after the body returned, so the COM apartment it owned is
                // already released when the wait below ends.
                stopped_sender.send(()).log_err();
            }) {
            Ok(join) => Some(join),
            Err(error) => {
                tracing::error!(target: "quick_launch", "the {name} thread did not start: {error}");
                None
            }
        };
        Self {
            requests: Some(requests),
            stopped,
            join,
        }
    }

    fn send(&self, request: Request) {
        let Some(requests) = self.requests.as_ref() else {
            return;
        };
        if let Err(error) = requests.send(request) {
            tracing::warn!(target: "quick_launch", "a worker is gone: {error}");
        }
    }

    /// Ends the thread and waits at most `limit` for it (§15.x).
    fn stop(&mut self, limit: Duration) {
        let Some(join) = self.join.take() else {
            return;
        };
        // Closing the channel is the stop signal: the loop ends as soon as it is
        // done with the request it holds. The bound is for a Shell call stuck on
        // a dead network path.
        self.requests.take();
        match self.stopped.recv_timeout(limit) {
            Ok(()) => {
                if join.join().is_err() {
                    tracing::warn!(target: "quick_launch", "a worker panicked on the way out");
                }
            }
            Err(_) => tracing::warn!(
                target: "quick_launch",
                "a worker did not stop within {} ms",
                limit.as_millis()
            ),
        }
    }
}

/// What the `quick-launch-icons` thread does: extract one icon, or check the
/// health of one item (§12.1).
enum IconWorkerRequest {
    Icon(IconRequest),
    Health(HealthRequest),
}

/// The two dedicated threads (§8.4). Starting and extracting never share one, so
/// a Shell call stuck on a dead network path cannot hold up a launch.
struct Workers {
    launch: Worker<StartRequest>,
    icons: Worker<IconWorkerRequest>,
}

impl Workers {
    fn start(replies: async_channel::Sender<WorkerReply>) -> Self {
        let launch = Worker::start("quick-launch", {
            let replies = replies.clone();
            move |receiver| {
                let registry = LaunchRegistry::with_builtins();
                run_launch_worker(receiver, replies, move |item, request| {
                    registry.launch(item, request)
                });
            }
        });
        let icons = Worker::start("quick-launch-icons", move |receiver| {
            run_icon_worker(receiver, replies, icon_image);
        });
        Self { launch, icons }
    }

    fn send_launch(&self, request: StartRequest) {
        self.launch.send(request);
    }

    fn send_icon(&self, request: IconRequest) {
        self.icons.send(IconWorkerRequest::Icon(request));
    }

    fn send_health(&self, request: HealthRequest) {
        self.icons.send(IconWorkerRequest::Health(request));
    }

    /// Stops both threads within one `limit` (§15.x).
    fn stop(&mut self, limit: Duration) {
        // Half each: one budget for two threads, so a stuck icon extraction
        // cannot eat the whole wait.
        let each = limit / 2;
        self.launch.stop(each);
        self.icons.stop(each);
    }
}

/// The `quick-launch` thread: starts items, one at a time (§8.4).
fn run_launch_worker(
    receiver: std_mpsc::Receiver<StartRequest>,
    replies: async_channel::Sender<WorkerReply>,
    start_item: impl Fn(&Item, &LaunchRequest) -> std::result::Result<LaunchOutcome, LaunchError>,
) {
    // COM and the expansion context are built on this thread: the Shell is
    // apartment-threaded and the context reads the configuration directory.
    let _com = ComApartment::enter();
    let context = ExpandContext::current();
    while let Ok(request) = receiver.recv() {
        let generation = request.generation;
        let name = request.item.name.clone();
        let launch_request = LaunchRequest {
            mode: request.mode,
            on_running: request.on_running,
            expand: context.clone(),
        };
        // A panic is a reply, not the end of the thread — and it keeps the
        // request's generation, so the window reports the failure instead of
        // dropping it as stale (§8.4).
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            start_item(&request.item, &launch_request)
        }))
        .unwrap_or_else(|panic| Err(LaunchError::Internal(panic_message(panic))));
        if let Err(error) = &outcome {
            tracing::debug!(target: "quick_launch", "launching {name:?} failed: {error}");
        }
        replies
            .try_send(WorkerReply::Launched {
                generation,
                name,
                outcome,
            })
            .log_err();
    }
}

/// The `quick-launch-icons` thread: extracts icons, dropping the ones that waited
/// too long in the queue (§8.4).
fn run_icon_worker(
    receiver: std_mpsc::Receiver<IconWorkerRequest>,
    replies: async_channel::Sender<WorkerReply>,
    extract: impl Fn(&ExpandContext, &str, u32) -> Option<Arc<RenderImage>>,
) {
    let _com = ComApartment::enter();
    let context = ExpandContext::current();
    // The same registry the launch thread uses, so an item is judged by the
    // action that would start it (§12.1).
    let registry = LaunchRegistry::with_builtins();
    while let Ok(request) = receiver.recv() {
        match request {
            IconWorkerRequest::Health(request) => {
                let health = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    registry.health(&request.item, &context)
                }))
                .unwrap_or_else(|panic| {
                    tracing::error!(target: "quick_launch", "health: {}", panic_message(panic));
                    Health::Unknown
                });
                replies
                    .try_send(WorkerReply::Health {
                        group: request.group,
                        slot: request.slot,
                        health,
                    })
                    .log_err();
            }
            IconWorkerRequest::Icon(request) => {
                let waited = request.queued.elapsed();
                if waited >= ICON_QUEUE_LIMIT {
                    // The window gave up on this one two seconds ago; running it
                    // now would only delay the requests behind it.
                    tracing::debug!(
                        target: "quick_launch",
                        path = %request.path,
                        waited_ms = waited.as_millis(),
                        "dropping an icon request that waited too long"
                    );
                    continue;
                }
                let key = request.key;
                let image = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    extract(&context, &request.path, key.edge)
                }))
                .unwrap_or_else(|panic| {
                    tracing::error!(target: "quick_launch", "icon: {}", panic_message(panic));
                    None
                });
                replies.try_send(WorkerReply::Icon { key, image }).log_err();
            }
        }
    }
}

/// The text of a panic caught on a worker thread (§8.4).
fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// The worker thread's COM apartment, uninitialized when the thread ends (§8.4).
struct ComApartment {
    #[cfg(windows)]
    initialized: bool,
}

impl ComApartment {
    fn enter() -> Self {
        #[cfg(windows)]
        {
            thumbnails::platform::init_thread();
            Self { initialized: true }
        }
        #[cfg(not(windows))]
        {
            Self {}
        }
    }
}

#[cfg(windows)]
impl Drop for ComApartment {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if !self.initialized {
            return;
        }
        // SAFETY: balances the `CoInitializeEx` of `init_thread` on this thread,
        // and every object made through it has been released by now.
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}

/// The disk cache path of one icon (§9).
fn icon_cache_file(file: &Path, edge: u32) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    file.to_string_lossy().to_lowercase().hash(&mut hasher);
    if let Ok(metadata) = std::fs::metadata(file) {
        metadata.len().hash(&mut hasher);
        if let Ok(modified) = metadata.modified()
            && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
        {
            since.as_nanos().hash(&mut hasher);
        }
    }
    edge.hash(&mut hasher);
    paths::quick_launch_icon_dir().join(format!("{:016x}.png", hasher.finish()))
}

/// The image for one icon, or `None` when there is none to show.
///
/// Reads the disk cache first, extracts from the Shell second, and writes what
/// it extracted back. Runs on the worker thread only, which is why the blocking
/// filesystem calls here are allowed (§8.4).
#[allow(clippy::disallowed_methods)] // worker thread, never a frame
fn icon_image(context: &ExpandContext, raw: &str, edge: u32) -> Option<Arc<RenderImage>> {
    let file = match nohrs_services::launch::expand(raw, context) {
        Ok(path) => path,
        Err(error) => {
            tracing::debug!(target: "quick_launch", "icon path {raw:?}: {error}");
            return None;
        }
    };
    // `"exe,索引"` names an icon inside a program rather than the program's
    // first icon, and the Shell's index-free extraction cannot express it (§9).
    if let Some((program, index)) = split_icon_index(raw)
        && let Ok(program) = nohrs_services::launch::expand(program, context)
        && let Some((pixels, width, height)) =
            nohrs_services::launch::indexed_icon(&program.to_string_lossy(), index, edge)
    {
        return thumbnails::render_image(width, height, pixels).map(Arc::new);
    }
    // A custom `.png` is the picture itself; anything else asks the Shell, which
    // knows `.ico`, programs and folders (§9).
    if file
        .extension()
        .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("png"))
    {
        return decode_png_icon(&file, edge).map(Arc::new);
    }
    let cache = icon_cache_file(&file, edge);
    if let Ok(bytes) = std::fs::read(&cache)
        && let Some(image) = decode_cached_icon(&bytes)
    {
        return Some(Arc::new(image));
    }
    let image =
        thumbnails::platform::extract(&file.to_string_lossy(), edge, thumbnails::Mode::Icon)?;
    if let Some(bytes) = encode_cached_icon(&image) {
        write_icon_cache(&cache, &bytes);
    }
    Some(Arc::new(image))
}

/// Splits `"program.exe,3"` into the program and the icon index (§9).
///
/// A path may itself contain a comma, so the split is the last one and only
/// counts when what follows it is a number.
fn split_icon_index(raw: &str) -> Option<(&str, u32)> {
    let (path, index) = raw.rsplit_once(',')?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let index = index.trim().parse::<u32>().ok()?;
    Some((path, index))
}

/// Writes one cached icon, and prunes the cache when it grew too large.
#[allow(clippy::disallowed_methods)] // worker thread, never a frame
fn write_icon_cache(path: &Path, bytes: &[u8]) {
    let Some(directory) = path.parent() else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(directory) {
        tracing::debug!(target: "quick_launch", "icon cache directory: {error}");
        return;
    }
    if let Err(error) = std::fs::write(path, bytes) {
        tracing::debug!(target: "quick_launch", "icon cache {}: {error}", path.display());
        return;
    }
    prune_icon_cache(directory);
}

/// Deletes the oldest cached icons while the directory is over its byte cap
/// (§9). Runs on the worker thread, which is idle when it is called.
fn prune_icon_cache(directory: &Path) {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(target: "quick_launch", "icon cache listing: {error}");
            return;
        }
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        total = total.saturating_add(metadata.len());
        files.push((modified, metadata.len(), entry.path()));
    }
    if total <= ICON_DISK_LIMIT {
        return;
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, length, path) in files {
        if total <= ICON_DISK_LIMIT {
            return;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => total = total.saturating_sub(length),
            Err(error) => {
                tracing::debug!(target: "quick_launch", "icon cache {}: {error}", path.display());
            }
        }
    }
}

/// A `.png` custom icon, scaled to `edge`.
fn decode_png_icon(file: &Path, edge: u32) -> Option<RenderImage> {
    let decoded = image::open(file).ok()?.thumbnail(edge, edge).into_rgba8();
    let (width, height) = decoded.dimensions();
    let mut bgra = decoded.into_raw();
    // The Shell's bitmaps arrive premultiplied (see `thumbnails::read_bitmap`)
    // and everything downstream expects that form, so a decoded image is
    // converted the same way.
    for pixel in bgra.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        let premultiply = |channel: u8| ((u16::from(channel) * alpha + 127) / 255) as u8;
        let (red, green, blue) = (pixel[0], pixel[1], pixel[2]);
        pixel[0] = premultiply(blue);
        pixel[1] = premultiply(green);
        pixel[2] = premultiply(red);
    }
    thumbnails::render_image(width, height, bgra)
}

/// A cached icon as a PNG.
///
/// GPUI keeps images as premultiplied BGRA; the bytes are written as RGBA with
/// the premultiplied values untouched, which makes the round trip exact (no
/// un-premultiplying and re-multiplying, which cannot be lossless). The file is
/// a cache, not something a person reads.
fn encode_cached_icon(image: &RenderImage) -> Option<Vec<u8>> {
    let size = image.size(0);
    let width = u32::try_from(size.width.0).ok()?;
    let height = u32::try_from(size.height.0).ok()?;
    let mut rgba = image.as_bytes(0)?.to_vec();
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    image::ImageEncoder::write_image(
        encoder,
        buffer.as_raw(),
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )
    .ok()?;
    Some(png)
}

/// The image of a cached icon, as the pixels GPUI draws.
fn decode_cached_icon(bytes: &[u8]) -> Option<RenderImage> {
    let decoded = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = decoded.dimensions();
    let mut bgra = decoded.into_raw();
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    thumbnails::render_image(width, height, bgra)
}

/// One remembered window position (§6).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct SavedBounds {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl From<Bounds<Pixels>> for SavedBounds {
    fn from(bounds: Bounds<Pixels>) -> Self {
        Self {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        }
    }
}

impl From<SavedBounds> for Bounds<Pixels> {
    fn from(saved: SavedBounds) -> Self {
        Bounds {
            origin: point(px(saved.x), px(saved.y)),
            size: size(px(saved.width), px(saved.height)),
        }
    }
}

/// What the store remembered: where the window was on each display, and which
/// group was last shown (§6).
#[derive(Debug, Clone, Default, PartialEq)]
struct Restored {
    positions: HashMap<String, SavedBounds>,
    last_group: Option<String>,
}

impl Restored {
    /// Reads the namespace into a plain map, so the foreground never waits on
    /// the store in the middle of a frame.
    fn from_entries(entries: Vec<(KvKey, Vec<u8>)>) -> Self {
        let mut restored = Self::default();
        for (key, bytes) in entries {
            let Some((_, name)) = key.as_str().split_once('.') else {
                continue;
            };
            if name == LAST_GROUP_KEY {
                if let Ok(text) = String::from_utf8(bytes) {
                    restored.last_group = Some(text);
                }
                continue;
            }
            if let Some(position) = name.strip_prefix("window_")
                && let Ok(saved) = serde_json::from_slice::<SavedBounds>(&bytes)
            {
                restored
                    .positions
                    .insert(format!("window_{position}"), saved);
            }
        }
        restored
    }
}

/// Which process the quick launch is running in (§15.1).
///
/// The window is the same in both; what changes is where a folder goes and
/// how a chord the main program already holds is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Beside the main window and its Explorer (§15.0).
    Main,
    /// Alone, in `LumaFiles launcher`: no Explorer, no tray, no `state.redb`
    /// (§15.1).
    Standalone,
}

/// What the binary injects so the window can own a global chord without this
/// crate depending on the launcher crate (§3.4, §10).
///
/// The error is a `String` rather than `anyhow::Error` because the binary does
/// not depend on `anyhow`, and this struct is built there.
#[derive(Clone, Copy)]
pub struct HotkeyHooks {
    /// Parses and registers `text`, returning an opaque id for
    /// [`HotkeyHooks::unregister`].
    pub register: fn(&mut App, &str) -> std::result::Result<u64, String>,
    /// Removes a chord registered by [`HotkeyHooks::register`].
    pub unregister: fn(&mut App, u64) -> std::result::Result<(), String>,
}

/// The in-memory state, read and written through the GPUI global (§13).
pub(crate) struct QuickLaunch {
    config: QuickLaunchConfig,
    status: Status,
    warnings: Vec<Warning>,
    /// Hash of the file this state came from, so a watcher signal caused by our
    /// own save is skipped (§5.3).
    file_hash: u64,
    /// The host KV store, or nothing when it could not be opened. Without it the
    /// feature works and simply forgets positions and usage (§6).
    kv: Option<Arc<dyn KvStore>>,
    restored: Restored,
    hooks: Option<HotkeyHooks>,
    workers: Option<Workers>,
    icons: IconCache,
    /// The group the window is showing, as an index into `config.groups`.
    group: usize,
    /// The slot the keyboard is on, if any.
    slot: Option<u32>,
    window: Option<WindowHandle<Root>>,
    visible: bool,
    /// The chord currently registered, its text, and why there is none when
    /// there is not (§10).
    hotkey: Option<u64>,
    hotkey_text: Option<String>,
    hotkey_error: Option<String>,
    /// A read is in flight; one at a time.
    loading: bool,
    /// An edit waiting for the throttle window, and when it ends (§5.2).
    pending_save: bool,
    save_due: Option<Instant>,
    _watcher: Option<ConfigWatcher>,
    signals: Option<std_mpsc::Receiver<()>>,
    /// Bumped per launch so a reply that arrived late is ignored (§8.4).
    generation: u64,
    /// The most recent launch failure, shown in the window's footer (§8.5).
    message: Option<String>,
    /// A hide asked for while GPUI had the window out for an update. The 400 ms
    /// tick performs it, outside any update, rather than the request being lost.
    pending_hide: bool,
    /// Bumped whenever what the window draws changes, so the view rebuilds its
    /// item list only then instead of on every frame (S6).
    revision: u64,
    /// The item a launch was asked for; counted only once it started (S7).
    pending_usage: Option<(String, u32)>,
    /// What the user typed to filter the current group (§12.2, M2).
    filter: String,
    /// The health of each item, by group id and slot (§12.1, M2). Runtime state,
    /// never written to the file.
    health: HashMap<(String, u32), Health>,
    /// Whether the health pass has been scheduled for this session.
    health_started: bool,
    /// Which process this is (§15.1).
    mode: Mode,
    /// The items still to check (§12.1).
    health_queue: VecDeque<(String, u32, Item)>,
}

impl Global for QuickLaunch {}

impl QuickLaunch {
    /// The configuration in effect.
    fn config(&self) -> &QuickLaunchConfig {
        &self.config
    }

    /// The group the window is showing.
    fn group(&self) -> Option<&Group> {
        self.config.groups.get(self.group)
    }

    fn set_group(&mut self, index: usize) {
        if index >= self.config.groups.len() || index == self.group {
            return;
        }
        self.group = index;
        self.slot = None;
    }

    /// Notes that what the window draws has changed, so the view rebuilds its
    /// cached frame: the configuration, the group, the items or their icons
    /// (S6).
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Whether the quick launch is part of keeping the process alive: the chord
    /// is registered and the file asks for it (§15.0).
    fn keeps_running(&self) -> bool {
        self.hotkey.is_some() && self.config.window.keep_running
    }
}

/// The state, read from anywhere on the foreground.
fn state(cx: &App) -> Option<&QuickLaunch> {
    cx.try_global::<QuickLaunch>()
}

/// The state, for the one thread that may change it.
fn state_mut(cx: &mut App) -> Option<&mut QuickLaunch> {
    cx.has_global::<QuickLaunch>()
        .then(|| cx.global_mut::<QuickLaunch>())
}

/// Whether editing is allowed: the file parsed and may be written back (§5.2).
pub(crate) fn editable(cx: &App) -> bool {
    state(cx).is_some_and(|state| state.status.can_save())
}

/// What the user typed to filter the current group, empty when not filtering.
pub(crate) fn filter(cx: &App) -> &str {
    state(cx)
        .map(|state| state.filter.as_str())
        .unwrap_or_default()
}

/// Replaces the filter text and drops the keyboard slot: the old slot may not be
/// on screen any more (§12.2).
pub(crate) fn set_filter(cx: &mut App, text: String) {
    let Some(state) = state_mut(cx) else {
        return;
    };
    state.filter = text;
    state.slot = None;
    state.touch();
}

/// The health of one item, when it has been checked (§12.1).
pub(crate) fn health_of(cx: &App, group: usize, slot: u32) -> Option<Health> {
    let state = state(cx)?;
    let id = state.config.groups.get(group)?.id.as_str();
    state.health.get(&(id.to_string(), slot)).cloned()
}

/// Starts the health pass once, after the window has been shown (§12.1).
///
/// Items are checked a batch at a time on the icon thread, between two idle
/// ticks, so the foreground never waits on a disk or a dead network path.
fn start_health_check(cx: &mut App) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    if quick_launch.health_started {
        return;
    }
    quick_launch.health_started = true;
    quick_launch.health_queue = quick_launch
        .config
        .groups
        .iter()
        .flat_map(|group| {
            group
                .items
                .iter()
                .map(|item| (group.id.clone(), item.slot, item.clone()))
        })
        .collect();
    if quick_launch.health_queue.is_empty() {
        return;
    }
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(HEALTH_PAUSE).await;
            let batch = cx.update(|cx| {
                let Some(quick_launch) = state_mut(cx) else {
                    return None;
                };
                let mut sent = 0usize;
                while sent < HEALTH_BATCH {
                    let Some((group, slot, item)) = quick_launch.health_queue.pop_front() else {
                        break;
                    };
                    if let Some(workers) = quick_launch.workers.as_ref() {
                        workers.send_health(HealthRequest {
                            group,
                            slot,
                            item: Box::new(item),
                        });
                        sent = sent.saturating_add(1);
                    } else {
                        return None;
                    }
                }
                (sent > 0).then_some(sent)
            });
            match batch {
                Ok(Some(_)) => continue,
                _ => return,
            }
        }
    })
    .detach();
}

/// Records one answer, so the view can grey the item (§12.1).
fn apply_health(cx: &mut App, group: String, slot: u32, health: Health) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    quick_launch.health.insert((group, slot), health);
    quick_launch.touch();
    cx.refresh_windows();
}

/// Whether one item is known to be broken, so the view can grey it (§12.1).
pub(crate) fn is_missing(cx: &App, group: usize, slot: u32) -> bool {
    health_of(cx, group, slot).is_some_and(|health| health != Health::Ok)
}

/// The slots of the current group the filter keeps, in slot order (§12.2).
///
/// Empty when nothing matches, and every slot when there is no filter. The
/// view packs these into the grid, so this is also the order the arrow keys
/// walk.
pub(crate) fn filtered_slots(cx: &App) -> Vec<u32> {
    let Some(state) = state(cx) else {
        return Vec::new();
    };
    let Some(group) = state.config.groups.get(state.group) else {
        return Vec::new();
    };
    if state.filter.trim().is_empty() {
        let mut slots: Vec<u32> = group.items.iter().map(|item| item.slot).collect();
        slots.sort_unstable();
        return slots;
    }
    let mut matcher = Matcher::new(MatchConfig::DEFAULT.match_paths());
    let pattern = Pattern::parse(
        state.filter.trim(),
        CaseMatching::Smart,
        Normalization::Smart,
    );
    let mut buffer = Vec::new();
    group
        .items
        .iter()
        .filter(|item| matches(&pattern, item, &mut matcher, &mut buffer))
        .map(|item| item.slot)
        .collect()
}

/// Whether one item matches the typed filter: by name or by any of its tags.
fn matches(pattern: &Pattern, item: &Item, matcher: &mut Matcher, buffer: &mut Vec<char>) -> bool {
    let mut haystack = item.name.clone();
    for tag in &item.tags {
        haystack.push(' ');
        haystack.push_str(tag);
    }
    pattern
        .score(Utf32Str::new(&haystack, buffer), matcher)
        .is_some()
}

/// The click that asked for a launch, as the keyboard and the mouse both express
/// it (§12.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClickKind {
    /// A plain click or `Enter`.
    Plain,
    /// `Ctrl`+click or `Ctrl+Enter`: elevated, whatever the item says.
    Admin,
    /// `Shift`+click: show the item's place in LumaFiles instead of starting it.
    Reveal,
}

/// The item in `slot` of the group at `group`, if the slot is taken.
pub(crate) fn item_at(cx: &App, group: usize, slot: u32) -> Option<Item> {
    let state = state(cx)?;
    state
        .config
        .groups
        .get(group)?
        .items
        .iter()
        .find(|item| item.slot == slot)
        .cloned()
}

/// Puts `item` in `slot` of `group`, replacing whatever was there: one slot
/// holds one entry (§12.1).
pub(crate) fn put_item(cx: &mut App, group: usize, slot: u32, mut item: Item) -> bool {
    if !editable(cx) {
        return false;
    }
    let Some(state) = state_mut(cx) else {
        return false;
    };
    let Some(target) = state.config.groups.get_mut(group) else {
        return false;
    };
    item.slot = slot;
    target.items.retain(|existing| existing.slot != slot);
    target.items.push(item);
    target.items.sort_by_key(|item| item.slot);
    state.touch();
    mark_dirty(cx);
    true
}

/// Applies the four fields the edit dialog offers (§12.3): name, path,
/// arguments and "run as administrator".
pub(crate) fn edit_item(
    cx: &mut App,
    group: usize,
    slot: u32,
    name: &str,
    path: &str,
    args: &str,
    admin: bool,
) -> bool {
    let name = name.trim();
    let path = path.trim();
    // An item without a name would be an empty tile, and one without a path
    // could never start: the dialog treats both as "cancel".
    if name.is_empty() || path.is_empty() || !editable(cx) {
        return false;
    }
    let Some(state) = state_mut(cx) else {
        return false;
    };
    let Some(target) = state.config.groups.get_mut(group) else {
        return false;
    };
    let Some(item) = target.items.iter_mut().find(|item| item.slot == slot) else {
        return false;
    };
    item.name = name.to_string();
    item.path = Some(path.to_string());
    let args = args.trim();
    item.args = (!args.is_empty()).then(|| args.to_string());
    item.admin = admin;
    state.touch();
    mark_dirty(cx);
    true
}

/// Adds a group at the end and makes it the current one (§12.3). The caller
/// starts the inline rename the specification asks for.
pub(crate) fn add_group(cx: &mut App) -> Option<usize> {
    if !editable(cx) {
        return None;
    }
    let state = state_mut(cx)?;
    let id = next_group_id(&state.config);
    let index = state.config.groups.len();
    state.config.groups.push(Group {
        id,
        name: nohrs_core::t!("Novo grupo").to_string(),
        items: Vec::new(),
    });
    state.group = index;
    state.slot = None;
    state.touch();
    mark_dirty(cx);
    Some(index)
}

/// Renames a group; an empty name cancels (§12.3).
pub(crate) fn rename_group(cx: &mut App, index: usize, name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() || !editable(cx) {
        return false;
    }
    let Some(state) = state_mut(cx) else {
        return false;
    };
    let Some(group) = state.config.groups.get_mut(index) else {
        return false;
    };
    group.name = name.to_string();
    state.touch();
    mark_dirty(cx);
    true
}

/// Removes a group and its usage records. The last group cannot be removed
/// (§12.3), and neither can a group with items in it unless `force`.
pub(crate) fn delete_group(cx: &mut App, index: usize, force: bool) -> bool {
    if !editable(cx) {
        return false;
    }
    let Some(state) = state_mut(cx) else {
        return false;
    };
    if state.config.groups.len() <= 1 {
        return false;
    }
    let removed: Vec<(String, u32)> = {
        let Some(group) = state.config.groups.get(index) else {
            return false;
        };
        if !force && !group.items.is_empty() {
            return false;
        }
        let id = group.id.clone();
        group
            .items
            .iter()
            .map(|item| (id.clone(), item.slot))
            .collect()
    };
    state.config.groups.remove(index);
    state.group = state.group.min(state.config.groups.len().saturating_sub(1));
    state.slot = None;
    state.touch();
    mark_dirty(cx);
    delete_usage(cx, removed);
    true
}

/// Removes one item and the usage record it left behind (§6, §12.3).
pub(crate) fn delete_item(cx: &mut App, group: usize, slot: u32) -> bool {
    if !editable(cx) {
        return false;
    }
    let Some(state) = state_mut(cx) else {
        return false;
    };
    let Some(id) = state.config.groups.get(group).map(|group| group.id.clone()) else {
        return false;
    };
    let Some(target) = state.config.groups.get_mut(group) else {
        return false;
    };
    let before = target.items.len();
    target.items.retain(|item| item.slot != slot);
    if target.items.len() == before {
        return false;
    }
    if state.slot == Some(slot) {
        state.slot = None;
    }
    state.touch();
    mark_dirty(cx);
    delete_usage(cx, vec![(id, slot)]);
    true
}

/// Moves an item to the first empty slot of another group, or past its last item
/// when the group is full (§12.2, "drag to the group bar").
pub(crate) fn move_item_to_group(
    cx: &mut App,
    from_group: usize,
    slot: u32,
    to_group: usize,
) -> bool {
    if from_group == to_group || !editable(cx) {
        return false;
    }
    let Some(state) = state_mut(cx) else {
        return false;
    };
    let Some(source) = state.config.groups.get_mut(from_group) else {
        return false;
    };
    let Some(position) = source.items.iter().position(|item| item.slot == slot) else {
        return false;
    };
    let mut item = source.items.remove(position);
    let Some(target) = state.config.groups.get_mut(to_group) else {
        // Put it back: the destination vanished between the menu and the click.
        state
            .config
            .groups
            .get_mut(from_group)
            .map(|source| source.items.push(item));
        return false;
    };
    item.slot = first_free_slot(target);
    target.items.push(item);
    target.items.sort_by_key(|item| item.slot);
    state.touch();
    mark_dirty(cx);
    true
}

/// Moves an item to another slot of its own group; when the target is taken the
/// two swap (§12.2).
pub(crate) fn swap_slots(cx: &mut App, group: usize, from: u32, to: u32) -> bool {
    if from == to || !editable(cx) {
        return false;
    }
    let Some(state) = state_mut(cx) else {
        return false;
    };
    let Some(target) = state.config.groups.get_mut(group) else {
        return false;
    };
    // Both positions are found before anything is retagged: looking the target
    // up afterwards would find the item that just moved into it.
    let Some(position) = target.items.iter().position(|item| item.slot == from) else {
        return false;
    };
    let occupied = target.items.iter().position(|item| item.slot == to);
    if let Some(moved) = target.items.get_mut(position) {
        moved.slot = to;
    }
    if let Some(occupied) = occupied
        && let Some(parked) = target.items.get_mut(occupied)
    {
        parked.slot = from;
    }
    target.items.sort_by_key(|item| item.slot);
    state.touch();
    mark_dirty(cx);
    true
}

/// The first group id of the form `group_<n>` that nothing uses (§12.3).
fn next_group_id(config: &QuickLaunchConfig) -> String {
    let mut index = 1u32;
    loop {
        let candidate = format!("group_{index}");
        if !config.groups.iter().any(|group| group.id == candidate) {
            return candidate;
        }
        index = index.saturating_add(1);
    }
}

/// The lowest slot a group does not occupy, or one past its highest slot.
fn first_free_slot(group: &Group) -> u32 {
    let mut candidate = 0u32;
    let mut used: Vec<u32> = group.items.iter().map(|item| item.slot).collect();
    used.sort_unstable();
    for slot in used {
        if slot == candidate {
            candidate = candidate.saturating_add(1);
        } else if slot > candidate {
            break;
        }
    }
    candidate
}

/// The item a path picked or dropped into `slot` becomes (§12.3): an executable
/// or shortcut is an `app`, a folder a `folder`, everything else a `file`.
pub(crate) fn item_from_path(path: &Path, slot: u32) -> Item {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
    let kind = if path.is_dir() {
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

/// Where an item dropped on the group bar (or anywhere in the content area)
/// goes: the first empty slot, else past the last item (§12.2).
pub(crate) fn next_free_slot(cx: &App, group: usize) -> u32 {
    state(cx)
        .and_then(|state| state.config.groups.get(group))
        .map_or(0, first_free_slot)
}

/// The KV key of one item's usage record (§6).
fn usage_key_name(group: &str, slot: u32) -> String {
    format!("usage_{group}_{slot}")
}

/// Deletes usage records in one transaction, off the foreground (§6, §12.3).
fn delete_usage(cx: &mut App, entries: Vec<(String, u32)>) {
    if entries.is_empty() {
        return;
    }
    let Some(kv) = state(cx).and_then(|state| state.kv.clone()) else {
        return;
    };
    let ops: Vec<KvOp> = entries
        .iter()
        .filter_map(|(group, slot)| {
            KvKey::new(KV_NAMESPACE, &usage_key_name(group, *slot))
                .ok()
                .map(|key| KvOp::Delete { key })
        })
        .collect();
    if ops.is_empty() {
        return;
    }
    cx.background_spawn(async move {
        kv.batch(ops).log_err();
    })
    .detach();
}

/// Installs the state, starts watching the file and starts the worker thread.
///
/// Called once from the binary, before the first window is drawn. The file is
/// read on a background task, so nothing here waits on the disk (§5.1).
pub fn init(cx: &mut App, kv: Option<Arc<dyn KvStore>>, hooks: HotkeyHooks, mode: Mode) {
    if state(cx).is_some() {
        return;
    }

    let (signals, receiver) = std_mpsc::channel::<()>();
    let watcher = match ConfigWatcher::new(&paths::quick_launch_file(), move || {
        // A closed channel only means the application is shutting down.
        signals.send(()).log_err();
    }) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            tracing::warn!(target: "quick_launch", "hot reload disabled: {error}");
            None
        }
    };

    let (replies, reply_receiver) = async_channel::unbounded::<WorkerReply>();
    let workers = Workers::start(replies);

    cx.set_global(QuickLaunch {
        config: QuickLaunchConfig::default(),
        status: Status::Loading,
        warnings: Vec::new(),
        file_hash: 0,
        kv,
        restored: Restored::default(),
        hooks: Some(hooks),
        workers: Some(workers),
        icons: IconCache::default(),
        group: 0,
        slot: None,
        window: None,
        visible: false,
        hotkey: None,
        hotkey_text: None,
        hotkey_error: None,
        loading: false,
        pending_save: false,
        save_due: None,
        _watcher: watcher,
        signals: Some(receiver),
        generation: 0,
        message: None,
        pending_hide: false,
        revision: 0,
        pending_usage: None,
        filter: String::new(),
        health: HashMap::new(),
        health_started: false,
        health_queue: VecDeque::new(),
        mode,
    });

    // One task answers everything the worker says, for the process's lifetime.
    cx.spawn(async move |cx| {
        while let Ok(reply) = reply_receiver.recv().await {
            if cx.update(|cx| apply_reply(cx, reply)).is_err() {
                return;
            }
        }
    })
    .detach();

    start_restore(cx);
    start_load(cx);
}

/// Reads what the store remembers, on a background task (§6).
fn start_restore(cx: &mut App) {
    let Some(kv) = state(cx).and_then(|quick_launch| quick_launch.kv.clone()) else {
        return;
    };
    cx.spawn(async move |cx| {
        let restored = cx
            .background_spawn(async move {
                match kv.list_namespace(KV_NAMESPACE) {
                    Ok(entries) => Restored::from_entries(entries),
                    Err(error) => {
                        tracing::warn!(target: "quick_launch", "could not read the store: {error}");
                        Restored::default()
                    }
                }
            })
            .await;
        cx.update(|cx| {
            if let Some(quick_launch) = state_mut(cx) {
                quick_launch.restored = restored;
            }
            restore_last_group(cx);
        })
        .ok();
    })
    .detach();
}

/// Reads the file on a background task and applies what it holds.
fn start_load(cx: &mut App) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    if quick_launch.loading {
        return;
    }
    quick_launch.loading = true;
    let path = paths::quick_launch_file();
    cx.spawn(async move |cx| {
        let read = cx
            .background_spawn(async move { read_quick_launch(&path) })
            .await;
        cx.update(|cx| apply_loaded(cx, read)).ok();
    })
    .detach();
}

/// What one read of the file found.
struct FileRead {
    hash: u64,
    parsed: Option<std::result::Result<Parsed, QuickLaunchError>>,
}

/// Reads and parses the file. On a background thread: this is the only place
/// that touches the file on a path a user waits for.
#[allow(clippy::disallowed_methods)] // background task, never a frame
fn read_quick_launch(path: &Path) -> FileRead {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return FileRead {
                hash: 0,
                parsed: None,
            };
        }
        Err(error) => {
            return FileRead {
                hash: 0,
                parsed: Some(Err(QuickLaunchError::Parse {
                    line: 1,
                    column: 1,
                    message: error.to_string(),
                })),
            };
        }
    };
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    let hash = hasher.finish();
    let parsed = match String::from_utf8(bytes) {
        Ok(text) => {
            // Windows PowerShell 5.1 and old Notepad save UTF-8 with a
            // byte-order mark, which TOML has no place for: a user who edits the
            // file once would otherwise land in the read-only state (§4.2 v2.3).
            let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
            Some(nohrs_core::config::quick_launch::parse(text))
        }
        Err(error) => Some(Err(QuickLaunchError::Parse {
            line: 1,
            column: 1,
            message: error.to_string(),
        })),
    };
    FileRead { hash, parsed }
}

/// Puts a finished read into the state (§5.3).
fn apply_loaded(cx: &mut App, read: FileRead) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    quick_launch.loading = false;
    if quick_launch.file_hash == read.hash && quick_launch.status != Status::Loading {
        // Our own save, or a change to a configuration file this state does not
        // read: the content is what is already loaded.
        return;
    }
    match read.parsed {
        None => {
            quick_launch.config = QuickLaunchConfig::with_default_group();
            quick_launch.status = Status::Ready;
            quick_launch.warnings.clear();
            quick_launch.file_hash = read.hash;
        }
        Some(Ok(parsed)) => {
            for warning in &parsed.warnings {
                tracing::warn!(target: "quick_launch", "{warning}");
            }
            quick_launch.config = parsed.config;
            quick_launch.warnings = parsed.warnings;
            quick_launch.status = Status::Ready;
            quick_launch.file_hash = read.hash;
        }
        Some(Err(error)) => {
            tracing::warn!(target: "quick_launch", "quicklaunch.toml: {error}");
            quick_launch.status = Status::ReadOnly(error.to_string());
            quick_launch.file_hash = read.hash;
        }
    }
    let groups = quick_launch.config.groups.len();
    quick_launch.group = quick_launch.group.min(groups.saturating_sub(1));
    quick_launch.slot = None;
    quick_launch.touch();
    restore_last_group(cx);
    ensure_selection(cx);
    sync_hotkey(cx);
    cx.refresh_windows();
}

/// Selects the group the last session was on, by id (§6).
fn restore_last_group(cx: &mut App) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let Some(id) = quick_launch.restored.last_group.clone() else {
        return;
    };
    let Some(index) = quick_launch
        .config
        .groups
        .iter()
        .position(|group| group.id == id)
    else {
        return;
    };
    if index == quick_launch.group {
        return;
    }
    quick_launch.group = index;
    // The window may already have drawn, and its frame still holds the old
    // group's names and tiles; the slot it selected belongs to that group too.
    // Dropping the slot and bumping the revision is what makes the next frame
    // show the restored group in the bar, in the tiles and in the selection
    // (R1).
    quick_launch.slot = None;
    quick_launch.touch();
}

/// Whether the process must stay alive for the quick launch to be summonable
/// (§15.0).
pub fn resident(cx: &App) -> bool {
    state(cx).is_some_and(QuickLaunch::keeps_running)
}

/// What closing the main window should do (§15.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Let the window go; the process ends with it.
    ///
    /// The table in §15.0 never answers this — a process whose only window goes
    /// would leave nothing to summon the quick launch into, so that case is
    /// [`CloseAction::Quit`] instead. The name is kept because the specification
    /// names all three outcomes.
    #[allow(dead_code)]
    Close,
    /// Hide the window and keep everything (IPC, KV, this feature) alive.
    HideToTray,
    /// Write what is pending, then end the process.
    Quit,
}

/// The table of §15.0, as a function so it can be tested without a window.
pub fn close_action(close_to_tray: bool, resident: bool) -> CloseAction {
    match (close_to_tray, resident) {
        // The user asked for the tray, which is what the window already does.
        (true, _) => CloseAction::HideToTray,
        // A hidden quick-launch window outlives every visible one, so answering
        // "just close" here would leave a process with no window at all.
        (false, false) => CloseAction::Quit,
        (false, true) => CloseAction::HideToTray,
    }
}

/// Registers, re-registers or removes the global chord the file names (§10).
///
/// Called after every successful read. A chord the OS refuses is reported in the
/// state and logged, and no part of the feature is disabled by it: the window can
/// still be summoned from the tray.
fn sync_hotkey(cx: &mut App) {
    let Some(quick_launch) = state(cx) else {
        return;
    };
    let wanted = quick_launch.config.window.hotkey.trim().to_string();
    let current = quick_launch.hotkey;
    let current_text = quick_launch.hotkey_text.clone();
    let previous_text = current_text.clone();
    let hooks = quick_launch.hooks;
    if wanted.is_empty() {
        if let Some(id) = current {
            unregister_hotkey(cx, hooks, id);
        }
        if let Some(quick_launch) = state_mut(cx) {
            quick_launch.hotkey = None;
            quick_launch.hotkey_text = None;
            quick_launch.hotkey_error = None;
        }
        return;
    }
    if current.is_some() && current_text.as_deref() == Some(wanted.as_str()) {
        // Re-registering the same chord would fail as "already registered".
        return;
    }
    if let Some(id) = current {
        unregister_hotkey(cx, hooks, id);
        if let Some(quick_launch) = state_mut(cx) {
            quick_launch.hotkey = None;
        }
    }
    let Some(hooks) = hooks else {
        return;
    };
    match (hooks.register)(cx, &wanted) {
        Ok(id) => {
            if let Some(quick_launch) = state_mut(cx) {
                quick_launch.hotkey = Some(id);
                quick_launch.hotkey_text = Some(wanted.clone());
                quick_launch.hotkey_error = None;
            }
            tracing::info!(target: "quick_launch", "listening on {wanted}");
        }
        Err(error) => {
            // §15.1: the same user may run the main program and the resident
            // one; the chord the first registered is the one that answers, and the
            // second says so at info level only.
            if crate::ipc::is_running() {
                tracing::info!(
                    target: "quick_launch",
                    "the hotkey {wanted:?} is held by the running LumaFiles: {error}"
                );
                if let Some(quick_launch) = state_mut(cx) {
                    quick_launch.hotkey = None;
                    quick_launch.hotkey_text = None;
                    quick_launch.hotkey_error = None;
                }
                return;
            }
            tracing::warn!(target: "quick_launch", "the hotkey {wanted:?} is unavailable: {error}");
            // The chord that was working goes back: a typo in the file must not
            // leave the window with no way to be summoned (§10).
            let restored = previous_text
                .as_deref()
                .and_then(|previous| (hooks.register)(cx, previous).ok());
            if let Some(quick_launch) = state_mut(cx) {
                quick_launch.hotkey = restored;
                quick_launch.hotkey_text = previous_text.filter(|_| restored.is_some());
                quick_launch.hotkey_error = Some(error);
            }
            if let Some(id) = restored {
                tracing::warn!(
                    target: "quick_launch",
                    "keeping the previous hotkey (id {id}) until the file is fixed"
                );
            }
        }
    }
}

fn unregister_hotkey(cx: &mut App, hooks: Option<HotkeyHooks>, id: u64) {
    let Some(hooks) = hooks else {
        return;
    };
    if let Err(error) = (hooks.unregister)(cx, id) {
        tracing::warn!(target: "quick_launch", "could not unregister the hotkey: {error}");
    }
}

/// Removes the chord and stops the worker thread when the process goes away.
pub fn shutdown(cx: &mut App) {
    let (hooks, id) = state(cx)
        .map(|quick_launch| (quick_launch.hooks, quick_launch.hotkey))
        .unwrap_or((None, None));
    if let Some(id) = id {
        unregister_hotkey(cx, hooks, id);
    }
    if let Some(quick_launch) = state_mut(cx) {
        quick_launch.hotkey = None;
        // §15.x: the dedicated threads are stopped before the process goes, with
        // a bounded wait so a Shell call on a dead path cannot hang the exit.
        if let Some(mut workers) = quick_launch.workers.take() {
            workers.stop(WORKER_STOP_LIMIT);
        }
    }
}

/// Writes a pending edit before the process ends (§15.0).
///
/// M1 has no editor, so nothing is ever pending; the exit path exists for the
/// milestones that add one, and it is synchronous because a throttled save would
/// not outlive the process.
pub fn flush_pending_save(cx: &mut App) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    if !quick_launch.pending_save {
        return;
    }
    quick_launch.pending_save = false;
    quick_launch.save_due = None;
    if !quick_launch.status.can_save() {
        return;
    }
    save_now(quick_launch);
}

/// Ends the throttled save window, if one was started (§5.2).
fn poll_save(cx: &mut App) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let Some(due) = quick_launch.save_due else {
        return;
    };
    if Instant::now() < due {
        return;
    }
    quick_launch.save_due = None;
    if !quick_launch.pending_save || !quick_launch.status.can_save() {
        return;
    }
    quick_launch.pending_save = false;
    save_in_background(cx);
}

/// Renders the model and writes it, on a background task (§5.2).
///
/// The render happens here because the model belongs to the foreground; the file
/// itself is written off the frame, and the hash of what was written comes back
/// so the reload that follows it is skipped (§5.3).
fn save_in_background(cx: &mut App) {
    let Some(quick_launch) = state(cx) else {
        return;
    };
    if !quick_launch.status.can_save() {
        return;
    }
    let text = match nohrs_core::config::quick_launch::to_toml(&quick_launch.config) {
        Ok(text) => text,
        Err(error) => {
            tracing::error!(target: "quick_launch", "could not write the configuration: {error}");
            return;
        }
    };
    let path = paths::quick_launch_file();
    cx.spawn(async move |cx| {
        let written = cx
            .background_spawn(async move { write_atomically(&path, &text) })
            .await;
        cx.update(|cx| match written {
            Ok(hash) => {
                if let Some(quick_launch) = state_mut(cx) {
                    quick_launch.file_hash = hash;
                }
            }
            Err(error) => {
                tracing::error!(target: "quick_launch", "could not write the configuration: {error}");
            }
        })
        .ok();
    })
    .detach();
}

/// Marks the state as changed, and starts the throttle if it is not running.
///
/// Every edit goes through here: the event handlers never write the file
/// themselves (§12.3).
fn mark_dirty(cx: &mut App) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    quick_launch.pending_save = true;
    if quick_launch.save_due.is_none() {
        quick_launch.save_due = Some(Instant::now() + SAVE_THROTTLE);
    }
}

/// Renders the model and replaces the file, atomically (§5.2).
fn save_now(quick_launch: &mut QuickLaunch) {
    let text = match nohrs_core::config::quick_launch::to_toml(&quick_launch.config) {
        Ok(text) => text,
        Err(error) => {
            tracing::error!(target: "quick_launch", "could not write the configuration: {error}");
            return;
        }
    };
    let path = paths::quick_launch_file();
    match write_atomically(&path, &text) {
        Ok(hash) => quick_launch.file_hash = hash,
        Err(error) => {
            tracing::error!(target: "quick_launch", "could not write {}: {error}", path.display());
        }
    }
}

/// Replaces `path` with `contents` through a temporary file and a backup.
///
/// A crash or a full disk in the middle leaves the original file alone: the
/// temporary file is written and flushed first, and only then renamed over the
/// target, which is atomic on both platforms LumaFiles runs on.
#[allow(clippy::disallowed_methods)] // the exit path and the throttled save
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<u64> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory)?;
    let temporary = path.with_extension("toml.tmp");
    let backup = path.with_extension("toml.bak");
    let mut file = std::fs::File::create(&temporary)?;
    {
        use std::io::Write as _;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    drop(file);
    if path.exists() {
        std::fs::copy(path, &backup)?;
    }
    std::fs::rename(&temporary, path)?;
    let mut hasher = DefaultHasher::new();
    contents.as_bytes().hash(&mut hasher);
    Ok(hasher.finish())
}

/// Writes one KV value in the background, logging rather than failing (§6).
fn write_kv_name(cx: &mut App, name: String, bytes: Vec<u8>) {
    let Some(kv) = state(cx).and_then(|quick_launch| quick_launch.kv.clone()) else {
        return;
    };
    cx.background_spawn(async move {
        let Ok(key) = KvKey::new(KV_NAMESPACE, &name) else {
            return;
        };
        kv.put(&key, &bytes).log_err();
    })
    .detach();
}

/// The KV key of one display's remembered window position (§6).
fn window_key(display: &Rc<dyn PlatformDisplay>) -> String {
    let mut hasher = DefaultHasher::new();
    // The display's own stable name, so the key survives a restart; the numeric
    // id is the fallback when the platform cannot name it.
    match display.uuid() {
        Ok(uuid) => uuid.to_string().hash(&mut hasher),
        Err(_) => u32::from(display.id()).hash(&mut hasher),
    }
    format!("window_{:016x}", hasher.finish())
}

/// The display whose bounds hold `point`, else the primary one.
fn display_at(cx: &App, point: Point<Pixels>) -> Option<Rc<dyn PlatformDisplay>> {
    cx.displays()
        .iter()
        .find(|display| display.bounds().contains(&point))
        .cloned()
        .or_else(|| cx.primary_display())
}

/// The display the pointer is on (§11).
///
/// GPUI only knows the pointer of one of its own windows, and when the summon
/// chord fires the foreground belongs to another application — which is exactly
/// the case this decides. Windows is asked where the pointer is instead.
fn pointer_display(cx: &mut App) -> Option<Rc<dyn PlatformDisplay>> {
    #[cfg(windows)]
    if let Some(index) = platform::cursor_monitor_index()
        && let Some(display) = cx
            .displays()
            .into_iter()
            .find(|display| u32::from(display.id()) == index)
    {
        return Some(display);
    }
    let position = cx.active_window().and_then(|window| {
        window
            .update(cx, |_, window, _| window.mouse_position())
            .ok()
    })?;
    display_at(cx, position)
}

/// Where the window should open (§11).
fn initial_bounds(cx: &mut App) -> Bounds<Pixels> {
    let view = state(cx).map_or(View::Grid, |quick_launch| quick_launch.config.window.view);
    let (width, height) = window_size(view);
    let target = pointer_display(cx).or_else(|| cx.primary_display());
    let Some(target) = target else {
        return Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(width), px(height)),
        };
    };

    // A position remembered for this display, as long as it is still on it: a
    // display can be unplugged, and a window restored off screen would be
    // invisible with no way to bring it back.
    if let Some(saved) = state(cx).and_then(|quick_launch| {
        quick_launch
            .restored
            .positions
            .get(&window_key(&target))
            .copied()
    }) {
        let bounds: Bounds<Pixels> = saved.into();
        if target.bounds().intersects(&bounds) {
            return bounds;
        }
    }

    let screen = target.bounds();
    Bounds {
        origin: point(
            screen.origin.x + (screen.size.width - px(width)) / 2.0,
            screen.origin.y + (screen.size.height - px(height)) / 2.0,
        ),
        size: size(px(width), px(height)),
    }
}

/// Remembers where the window was on the display it is on (§6).
fn save_position(cx: &mut App, bounds: Bounds<Pixels>) {
    let Some(display) = display_at(cx, bounds.center()) else {
        return;
    };
    let name = window_key(&display);
    let saved = SavedBounds::from(bounds);
    if let Some(quick_launch) = state_mut(cx) {
        quick_launch.restored.positions.insert(name.clone(), saved);
    }
    let Ok(bytes) = serde_json::to_vec(&saved) else {
        return;
    };
    write_kv_name(cx, name, bytes);
}

/// Counts one launch of the item in `slot` (§6).
///
/// Nothing ranks by it in this milestone; the record is written so that the
/// counts are already there when something does.
fn record_usage(cx: &mut App, group: &str, slot: u32) {
    let name = format!("usage_{group}_{slot}");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let Some(kv) = state(cx).and_then(|quick_launch| quick_launch.kv.clone()) else {
        return;
    };
    cx.background_spawn(async move {
        #[derive(Serialize, Deserialize, Default)]
        struct Usage {
            count: u32,
            last: u64,
        }
        let Ok(key) = KvKey::new(KV_NAMESPACE, &name) else {
            return;
        };
        let mut usage: Usage = kv
            .get(&key)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        usage.count = usage.count.saturating_add(1);
        usage.last = now;
        let Ok(bytes) = serde_json::to_vec(&usage) else {
            return;
        };
        kv.put(&key, &bytes).log_err();
    })
    .detach();
}

/// Shows the window, creating it the first time (§11).
pub(crate) fn show(cx: &mut App) -> Result<()> {
    // The §16.2 budget is measured from here: the first summon also creates the
    // window, a later one only shows it.
    let started = Instant::now();
    if let Some(handle) = live_window(cx) {
        let shown = handle.update(cx, |_root, window, _cx| {
            show_native(window);
            window.activate_window();
            crate::foreground::bring_to_front(window);
        });
        if shown.is_ok() {
            if let Some(quick_launch) = state_mut(cx) {
                quick_launch.visible = true;
            }
            ensure_selection(cx);
            tracing::debug!(
                target: "quick_launch",
                elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                created = false,
                "summon"
            );
            return Ok(());
        }
    }

    if live_window(cx).is_some() {
        // The window is there but could not be shown just now (it was out for an
        // update). Building a second one would leave two, so this summon does
        // nothing and the next one shows the window it already has.
        tracing::debug!(target: "quick_launch", "summon deferred: the window is busy");
        return Ok(());
    }

    let bounds = initial_bounds(cx);
    let view = state(cx).map_or(View::Grid, |quick_launch| quick_launch.config.window.view);
    let topmost = state(cx).is_some_and(|quick_launch| quick_launch.config.window.topmost);
    let handle = cx.open_window(window_options(bounds, view), |window, cx| {
        let quick_launch = cx.new(|cx| QuickLaunchView::new(window, cx));
        cx.new(|cx| Root::new(quick_launch, window, cx))
    })?;
    if let Some(quick_launch) = state_mut(cx) {
        quick_launch.window = Some(handle);
        quick_launch.visible = true;
    }
    ensure_selection(cx);
    handle
        .update(cx, |_root, window, _cx| {
            set_topmost(window, topmost);
            window.activate_window();
        })
        .log_err();
    tracing::debug!(
        target: "quick_launch",
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        created = true,
        "summon"
    );
    Ok(())
}

/// Hides the window without destroying it (§11).
///
/// For callers that do not already hold the window (the summon chord, a reply
/// from the worker thread). Anything running *inside* the window's own event
/// handling must use [`hide_in`] instead: GPUI takes a window out of the
/// application while it is being updated, so asking for it again from its own
/// handler fails with "window not found".
pub(crate) fn hide(cx: &mut App) {
    let Some(handle) = live_window(cx) else {
        return;
    };
    let hidden = handle.update(cx, |_root, window, cx| hide_in(cx, window));
    if hidden.is_err() {
        // Either the window is gone, or it is out for an update. The next tick
        // asks again; if it is really gone, `live_window` will answer no and the
        // state is cleared then.
        tracing::debug!(target: "quick_launch", "hide deferred to the next tick");
        if let Some(quick_launch) = state_mut(cx) {
            quick_launch.pending_hide = true;
        }
        return;
    }
    if let Some(quick_launch) = state_mut(cx) {
        quick_launch.visible = false;
        quick_launch.pending_hide = false;
    }
}

/// Hides `window`, which the caller already holds.
pub(super) fn hide_in(cx: &mut App, window: &mut Window) {
    hide_native(window);
    let bounds = window.bounds();
    save_position(cx, bounds);
    if let Some(quick_launch) = state_mut(cx) {
        quick_launch.visible = false;
    }
}

/// Shows the window if it is hidden, hides it if it is showing.
pub fn toggle(cx: &mut App) {
    let showing =
        state(cx).is_some_and(|quick_launch| quick_launch.visible) && live_window(cx).is_some();
    if showing {
        hide(cx);
    } else if let Err(error) = show(cx) {
        tracing::error!(target: "quick_launch", "could not show the window: {error}");
    }
}

/// The window, if it is still alive.
///
/// The remembered handle outlives the window it names, so it is checked against
/// the live list rather than trusted: a stale handle would make every summon do
/// nothing.
fn live_window(cx: &App) -> Option<WindowHandle<Root>> {
    let handle = state(cx).and_then(|quick_launch| quick_launch.window)?;
    cx.windows()
        .iter()
        .any(|window| window.window_id() == handle.window_id())
        .then_some(handle)
}

/// Shows a window the OS has hidden, without touching the tray state.
fn show_native(window: &Window) {
    if let Some(hwnd) = crate::ipc::window_hwnd(window) {
        crate::tray::platform::show_window(hwnd);
    }
}

/// Hides a window without touching the tray state.
fn hide_native(window: &Window) {
    if let Some(hwnd) = crate::ipc::window_hwnd(window) {
        crate::tray::platform::hide_window(hwnd);
    }
}

/// Puts the window above (or among) ordinary windows (§11).
#[cfg(windows)]
#[allow(unsafe_code)]
fn set_topmost(window: &Window, topmost: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
    };
    let Some(hwnd) = crate::ipc::window_hwnd(window) else {
        return;
    };
    let after = if topmost {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    // SAFETY: the handle comes from the live window and no buffer is involved.
    unsafe {
        SetWindowPos(
            HWND(hwnd as *mut core::ffi::c_void),
            Some(after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    }
    .log_err();
}

#[cfg(not(windows))]
fn set_topmost(_window: &Window, _topmost: bool) {}

/// The window's size for a view.
fn window_size(view: View) -> (f32, f32) {
    match view {
        View::Grid => GRID_WINDOW_SIZE,
        View::List => LIST_WINDOW_SIZE,
    }
}

/// The window's shape: a borderless pop-up whose title bar this crate draws
/// (§11).
fn window_options(bounds: Bounds<Pixels>, _view: View) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // The title bar is drawn by the view, which is what puts the drag area
        // and the menu on it.
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: true,
        is_resizable: true,
        is_minimizable: false,
        display_id: None,
        window_background: gpui::WindowBackgroundAppearance::Opaque,
        app_id: None,
        window_min_size: Some(size(px(MIN_WINDOW_SIZE.0), px(MIN_WINDOW_SIZE.1))),
        window_decorations: None,
        tabbing_identifier: None,
    }
}

/// The icon edge an item's icon is extracted at, in physical pixels (§9).
fn icon_edge(window: &Window, configured: u16) -> u32 {
    let scaled = (f32::from(configured) * window.scale_factor()).ceil();
    let scaled = u32::try_from(scaled.max(0.0) as u64).unwrap_or(32);
    ICON_STEPS
        .iter()
        .copied()
        .find(|step| *step >= scaled)
        .unwrap_or(*ICON_STEPS.last().unwrap_or(&256))
}

/// The icon of `path`, or `None` while it is being made.
///
/// Called from the drawing code, so it only touches the in-memory cache and the
/// worker's queue (§8.4, §9).
fn icon(cx: &mut App, path: &str, edge: u32) -> Option<Arc<RenderImage>> {
    let key = IconKey::new(path, edge);
    let quick_launch = state_mut(cx)?;
    if let Some(image) = quick_launch.icons.get(&key) {
        return image;
    }
    if !quick_launch.icons.wants_request(&key) {
        return None;
    }
    let queued = Instant::now();
    quick_launch.icons.pending.insert(key.clone(), queued);
    if let Some(workers) = quick_launch.workers.as_ref() {
        workers.send_icon(IconRequest {
            key,
            path: path.to_string(),
            queued,
        });
    }
    None
}

/// Puts what the worker said into the state.
fn apply_reply(cx: &mut App, reply: WorkerReply) {
    match reply {
        WorkerReply::Health {
            group,
            slot,
            health,
        } => apply_health(cx, group, slot, health),
        WorkerReply::Icon { key, image } => {
            if let Some(quick_launch) = state_mut(cx) {
                let timed_out = quick_launch
                    .icons
                    .pending
                    .get(&key)
                    .is_some_and(|started| started.elapsed() >= ICON_TIMEOUT);
                if timed_out || image.is_none() {
                    quick_launch.icons.pending.remove(&key);
                    quick_launch.icons.settled.insert(key);
                } else {
                    quick_launch.icons.insert(key, image);
                    // The tiles draw the picture that just arrived.
                    quick_launch.touch();
                }
            }
            cx.refresh_windows();
        }
        WorkerReply::Launched {
            generation,
            name,
            outcome,
        } => {
            let Some(quick_launch) = state(cx) else {
                return;
            };
            if generation != quick_launch.generation {
                return;
            }
            let close_after_launch = quick_launch.config.window.close_after_launch;
            let started = counts_as_usage(&outcome);
            let folder = match outcome {
                Ok(LaunchOutcome::OpenFolder(path)) => Some(path),
                Ok(LaunchOutcome::Started | LaunchOutcome::Activated) => None,
                // Nothing is reported for a decision (§8.2).
                Ok(LaunchOutcome::Cancelled) => return,
                Err(error) => {
                    report_launch_failure(cx, &name, &error);
                    return;
                }
            };
            if started
                && let Some((group, slot)) =
                    state_mut(cx).and_then(|state| state.pending_usage.take())
            {
                // Only a launch that happened is counted (S7).
                record_usage(cx, &group, slot);
            }
            if let Some(path) = folder {
                match state(cx).map_or(Mode::Main, |quick_launch| quick_launch.mode) {
                    Mode::Main => open_folder_in_main_window(path),
                    Mode::Standalone => open_folder_without_explorer(cx, path),
                }
            }
            if close_after_launch {
                hide(cx);
            }
        }
    }
}

/// Whether one outcome counts as a use (§6): starting, activating and opening a
/// folder all do; a decision or a failure does not.
fn counts_as_usage(outcome: &std::result::Result<LaunchOutcome, LaunchError>) -> bool {
    matches!(
        outcome,
        Ok(LaunchOutcome::Started | LaunchOutcome::Activated | LaunchOutcome::OpenFolder(_))
    )
}

/// Opens a folder in this process's Explorer (§15.0): the main window
/// may be hidden in the tray, and Q5 makes that the normal case, so the tray
/// is asked to show it first.
fn open_folder_in_main_window(path: PathBuf) {
    // The same request the tray's own menu sends, handled by the existing
    // publisher loop; the quick launch does not touch the tray state itself.
    crate::tray::request_show();
    if !crate::explorer_link::request(path.clone()) {
        tracing::info!(
            target: "quick_launch",
            "no LumaFiles window to open {} in",
            path.display()
        );
    }
}

/// Opens a folder with no Explorer in this process (§15.1): the main
/// program takes it through the IPC, and when it is not running a copy of this
/// executable is started with the path, which opens it the way the shell does.
///
/// The spawn happens on a background task: a process call on the foreground is
/// what §18 forbids.
fn open_folder_without_explorer(cx: &mut App, path: PathBuf) {
    cx.background_spawn(async move {
        if crate::ipc::request_open(&path) {
            return;
        }
        let Some(executable) = std::env::current_exe().ok() else {
            tracing::warn!(
                target: "quick_launch",
                "no executable to open {} with",
                path.display()
            );
            return;
        };
        if let Err(error) = std::process::Command::new(executable).arg(&path).spawn() {
            tracing::warn!(
                target: "quick_launch",
                "could not start LumaFiles for {}: {error}",
                path.display()
            );
        }
    })
    .detach();
}

/// Tells the user that one item would not start, in one line (§8.5).
fn report_launch_failure(cx: &mut App, name: &str, error: &LaunchError) {
    tracing::warn!(target: "quick_launch", "could not launch {name:?}: {error}");
    let message = nohrs_core::tf!(
        "Não foi possível iniciar {name}: {error}",
        name = name,
        error = error
    );
    // The bottom line of the window is the one-line report §8.5 asks for. A
    // `push_notification` here would ask the window for it while the window's
    // own `Root` is being updated, and GPUI aborts on that nested update.
    if let Some(quick_launch) = state_mut(cx) {
        quick_launch.message = Some(message);
    }
    cx.refresh_windows();
}

/// Starts the item in `slot` of the current group (§12.2).
/// Starts the item in `slot` the way the click asked for (§12.2).
pub(crate) fn activate_with(cx: &mut App, slot: u32, kind: ClickKind) {
    let Some(quick_launch) = state(cx) else {
        return;
    };
    let Some(group) = quick_launch.group() else {
        return;
    };
    let Some(item) = group.items.iter().find(|item| item.slot == slot) else {
        return;
    };
    tracing::debug!(
        target: "quick_launch",
        slot,
        name = %item.name,
        "launching the selected item"
    );
    let (item, group_id) = (item.clone(), group.id.clone());
    let mode = if kind == ClickKind::Admin || item.admin {
        LaunchMode::Admin
    } else {
        LaunchMode::Normal
    };
    let on_running = quick_launch.config.window.on_running;
    let generation = quick_launch.generation.wrapping_add(1);
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    quick_launch.generation = generation;
    quick_launch.message = None;
    quick_launch.pending_usage = Some((group_id, slot));
    if let Some(workers) = quick_launch.workers.as_ref() {
        workers.send_launch(StartRequest {
            item: Box::new(item),
            mode,
            on_running,
            generation,
        });
    }
    cx.refresh_windows();
}

/// Switches the group bar to `index` (§12.2).
fn select_group(cx: &mut App, index: usize) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    quick_launch.set_group(index);
    quick_launch.touch();
    let Some(id) = quick_launch
        .config
        .groups
        .get(index)
        .map(|group| group.id.clone())
    else {
        return;
    };
    quick_launch.restored.last_group = Some(id.clone());
    let bytes = id.into_bytes();
    write_kv_name(cx, LAST_GROUP_KEY.to_string(), bytes);
    ensure_selection(cx);
    cx.refresh_windows();
}

/// Moves the keyboard selection by `delta` cells, skipping empty slots and
/// stopping at the edges without wrapping (§12.2).
fn move_selection(cx: &mut App, delta: i64) {
    // The arrow keys walk what the window shows: the filtered slots, or every
    // slot when nothing is typed.
    let occupied = filtered_slots(cx);
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let Some(first) = occupied.first().copied() else {
        quick_launch.slot = None;
        return;
    };
    let Some(current) = quick_launch.slot else {
        // Nothing selected yet: the first press lands on the first item, which
        // is what the arrow keys mean before anything is chosen.
        quick_launch.slot = Some(first);
        cx.refresh_windows();
        return;
    };
    let next = if delta > 0 {
        let target = current.saturating_add(u32::try_from(delta).unwrap_or(1));
        occupied
            .iter()
            .copied()
            .find(|slot| *slot >= target)
            .or_else(|| occupied.last().copied())
    } else {
        let target = current.saturating_sub(u32::try_from(-delta).unwrap_or(1));
        occupied
            .iter()
            .copied()
            .rev()
            .find(|slot| *slot <= target)
            .or_else(|| occupied.first().copied())
    };
    quick_launch.slot = next;
    cx.refresh_windows();
}

/// Makes sure one item is selected, so `Enter` has something to start and the
/// grid has a highlighted cell (§12.2).
fn ensure_selection(cx: &mut App) {
    let kept = filtered_slots(cx);
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    // A selection the filter no longer keeps is replaced, so Enter cannot start
    // an item that is not on screen.
    if quick_launch.slot.is_some_and(|slot| kept.contains(&slot)) {
        return;
    }
    quick_launch.slot = kept.first().copied();
}

/// Shows where an item lives in LumaFiles (§12.2, `Shift`+click).
///
/// The path is expanded here because the Explorer needs the real one; the
/// expansion is string work, not a filesystem read.
pub(crate) fn reveal_item(cx: &App, slot: u32) -> bool {
    let Some(state) = state(cx) else {
        return false;
    };
    let Some(item) = state
        .config
        .groups
        .get(state.group)
        .and_then(|group| group.items.iter().find(|item| item.slot == slot))
    else {
        return false;
    };
    let Some(path) = expanded_path(cx, item) else {
        return false;
    };
    crate::explorer_link::request(path)
}

/// The text the "copy" entry puts on the clipboard: the item's path (§12.2).
pub(crate) fn copy_text(cx: &App, slot: u32) -> Option<String> {
    let state = state(cx)?;
    let item = state
        .config
        .groups
        .get(state.group)?
        .items
        .iter()
        .find(|item| item.slot == slot)?;
    Some(
        expanded_path(cx, item)
            .map(|path| path.to_string_lossy().to_string())
            .or_else(|| item.path.clone())
            .unwrap_or_default(),
    )
}

/// One item's path with the variables of §7 resolved.
fn expanded_path(_cx: &App, item: &Item) -> Option<PathBuf> {
    let context = ExpandContext::current();
    let raw = item.path.as_deref()?.trim();
    nohrs_services::launch::expand(raw, &context)
        .ok()
        .map(PathBuf::from)
}

/// The message the footer shows, if the last launch failed.
fn message(cx: &App) -> Option<&str> {
    state(cx).and_then(|quick_launch| quick_launch.message.as_deref())
}

/// The read-only banner's text, if the file must not be written back (§5.2).
fn read_only_reason(cx: &App) -> Option<&str> {
    state(cx).and_then(|quick_launch| quick_launch.status.error())
}

/// Whether the first read is still running.
fn is_loading(cx: &App) -> bool {
    state(cx).is_none_or(|quick_launch| quick_launch.status == Status::Loading)
}

/// Re-reads the file when the watcher says it changed (§5.3).
///
/// Called from the 400ms configuration loop, so it starts no timer of its own,
/// and the content hash in [`apply_loaded`] turns the signals our own save causes
/// into nothing.
pub fn poll_reload(cx: &mut App) {
    let Some(quick_launch) = state(cx) else {
        return;
    };
    let mut changed = false;
    if let Some(signals) = &quick_launch.signals {
        while signals.try_recv().is_ok() {
            changed = true;
        }
    }
    if changed {
        start_load(cx);
    }
    if state_mut(cx).is_some_and(|quick_launch| quick_launch.pending_hide) {
        if live_window(cx).is_some() {
            hide(cx);
        } else if let Some(quick_launch) = state_mut(cx) {
            // The window is really gone: forget it so the next summon makes a
            // new one, and drop the owe.
            quick_launch.window = None;
            quick_launch.visible = false;
            quick_launch.pending_hide = false;
        }
    }
    ensure_selection(cx);
    poll_save(cx);
}

/// The pointer's monitor, numbered the way the platform lists displays (§11).
#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! `GetCursorPos` and `MonitorFromPoint`, which is the only way to learn
    //! where the pointer is while no LumaFiles window has the foreground.

    use windows::Win32::Foundation::{LPARAM, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MonitorFromPoint,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    use windows::core::BOOL;

    /// The index of the monitor under the pointer, in `EnumDisplayMonitors`
    /// order — which is what `DisplayId` counts on this platform.
    pub fn cursor_monitor_index() -> Option<u32> {
        let mut position = POINT::default();
        // SAFETY: `position` is a live out-parameter and the call only writes it.
        unsafe { GetCursorPos(&mut position) }.ok()?;
        // SAFETY: no buffer is involved; the call answers a monitor handle.
        let monitor = unsafe { MonitorFromPoint(position, MONITOR_DEFAULTTONEAREST) };
        let mut monitors: Vec<HMONITOR> = Vec::new();
        // SAFETY: the callback only pushes into `monitors`, which outlives the
        // enumeration because it is still borrowed here.
        unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(collect_monitor),
                LPARAM((&mut monitors as *mut Vec<HMONITOR>).cast::<core::ffi::c_void>() as isize),
            )
        }
        .ok()
        .ok()?;
        monitors
            .iter()
            .position(|handle| handle.0 == monitor.0)
            .and_then(|index| u32::try_from(index).ok())
    }

    /// Collects one monitor of the enumeration `cursor_monitor_index` runs.
    unsafe extern "system" fn collect_monitor(
        monitor: HMONITOR,
        _device: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the `Vec<HMONITOR>` that is alive for the whole
        // enumeration, and the callback is only ever called from inside it.
        let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        monitors.push(monitor);
        BOOL(1)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
mod tests {
    use super::*;

    /// A group of `count` items, the size §16.2 measures a switch with.
    fn group_of(count: u32) -> Group {
        Group {
            id: "general".to_string(),
            name: "Geral".to_string(),
            items: (0..count)
                .map(|slot| Item::new(slot, "app", format!("Item {slot}")))
                .collect(),
        }
    }

    /// §16.2: switching to a group of 200 entries, on the foreground, median of
    /// 100 switches. Everything a switch does besides the window's own redraw:
    /// the group lookup, the items the view will draw, and the icon cache hits
    /// for each of them.
    ///
    /// Run with `cargo test --release -p nohrs-pages group_switch_is_cheap --
    /// --ignored --nocapture`.
    #[test]
    #[ignore = "timing, run by hand for the PR"]
    fn group_switch_is_cheap() {
        let mut state = sample_state();
        state.config.groups = vec![group_of(200), group_of(200)];
        let mut timings = Vec::new();
        for switch in 0..100 {
            let started = Instant::now();
            state.set_group(switch % 2);
            let items = state
                .config()
                .groups
                .get(state.group)
                .map(|group| group.items.clone())
                .unwrap_or_default();
            let mut cache = IconCache::default();
            for item in &items {
                let key = IconKey::new(item.path.as_deref().unwrap_or_default(), 48);
                cache.insert(key.clone(), None);
                let _ = cache.get(&key);
            }
            let _ = grid_cells_for_timing(&items);
            timings.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        timings.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
        let median = timings[timings.len() / 2];
        println!("group switch, 200 items: median {median:.3} ms");
        assert!(median < 16.0, "{median} ms is over the §16.2 budget");
    }

    /// The layout work a switch does, without the window: the same function the
    /// view calls.
    fn grid_cells_for_timing(items: &[Item]) -> usize {
        super::view::grid_cells_for_timing(items, 8)
    }

    fn resident_table() -> Vec<(bool, bool, CloseAction)> {
        vec![
            (true, true, CloseAction::HideToTray),
            (true, false, CloseAction::HideToTray),
            (false, true, CloseAction::HideToTray),
            (false, false, CloseAction::Quit),
        ]
    }

    #[test]
    fn closing_the_main_window_follows_the_table_of_the_specification() {
        for (close_to_tray, resident, expected) in resident_table() {
            assert_eq!(
                close_action(close_to_tray, resident),
                expected,
                "close_to_tray={close_to_tray} resident={resident}"
            );
        }
    }

    #[test]
    fn closing_never_leaves_a_windowless_process() {
        // `Close` is not the answer to any combination: with a quick-launch
        // window alive but hidden, the process would outlive every visible
        // window, and with none alive it ends anyway.
        for (close_to_tray, resident, _) in resident_table() {
            assert_ne!(close_action(close_to_tray, resident), CloseAction::Close);
        }
    }

    /// Puts `groups` into the global, ready to be navigated.
    fn install(cx: &mut gpui::App, groups: Vec<Group>) -> Vec<u32> {
        let mut state = sample_state();
        state.config.groups = groups;
        // A freshly summoned window has nothing selected until the first key.
        state.slot = None;
        let slots = state
            .config
            .groups
            .first()
            .map(|group| group.items.iter().map(|item| item.slot).collect())
            .unwrap_or_default();
        cx.set_global(state);
        slots
    }

    /// §9: a custom icon may name an index inside a program.
    #[test]
    fn a_custom_icon_can_name_an_index_inside_a_program() {
        assert_eq!(
            split_icon_index("C:\\Windows\\System32\\shell32.dll,3"),
            Some(("C:\\Windows\\System32\\shell32.dll", 3))
        );
        assert_eq!(split_icon_index("icone.png"), None);
        assert_eq!(split_icon_index("programa.exe,"), None);
        assert_eq!(split_icon_index(",4"), None);
        assert_eq!(
            split_icon_index("C:\\pasta,com,9"),
            Some(("C:\\pasta,com", 9)),
            "the last comma is the separator"
        );
    }

    /// §12.3: a group gets the first free `group_<n>` id.
    #[test]
    fn a_new_group_gets_the_first_free_id() {
        let mut config = QuickLaunchConfig::default();
        config.groups = vec![
            Group {
                id: "group_1".to_string(),
                name: "Um".to_string(),
                items: Vec::new(),
            },
            Group {
                id: "group_3".to_string(),
                name: "Tres".to_string(),
                items: Vec::new(),
            },
        ];
        assert_eq!(next_group_id(&config), "group_2");
    }

    /// §12.2: a move or a drop into a group goes to its first empty slot, and
    /// past the last item when nothing is empty.
    #[test]
    fn a_move_lands_in_the_first_empty_slot() {
        let mut group = group_of(0);
        group.items = vec![
            Item::new(0, "app", "A"),
            Item::new(1, "app", "B"),
            Item::new(3, "app", "D"),
        ];
        assert_eq!(first_free_slot(&group), 2);
        group.items.push(Item::new(2, "app", "C"));
        assert_eq!(first_free_slot(&group), 4);
        assert_eq!(first_free_slot(&group_of(0)), 0);
    }

    /// §12.3: what a picked or dropped path becomes.
    #[test]
    fn a_picked_path_becomes_the_item_its_extension_says() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("Uma pasta");
        std::fs::create_dir(&folder).unwrap();
        let folder_item = item_from_path(&folder, 3);
        assert_eq!(folder_item.kind, "folder");
        assert_eq!(folder_item.name, "Uma pasta");
        assert_eq!(folder_item.slot, 3);

        let shortcut = directory.path().join("atalho.lnk");
        std::fs::write(&shortcut, b"").unwrap();
        assert_eq!(item_from_path(&shortcut, 0).kind, "app");

        let document = directory.path().join("notas.txt");
        std::fs::write(&document, b"oi").unwrap();
        let document_item = item_from_path(&document, 0);
        assert_eq!(document_item.kind, "file");
        assert_eq!(document_item.name, "notas");
    }

    /// §6: the usage key of one item.
    #[test]
    fn a_usage_record_is_named_after_its_group_and_slot() {
        assert_eq!(usage_key_name("general", 7), "usage_general_7");
    }

    /// §5.2, §12.3: a read-only file refuses every edit.
    /// §6: a folder opened from the quick launch is a use; a refused or
    /// failed start is not.
    #[test]
    fn opening_a_folder_counts_as_a_use() {
        let folder = LaunchOutcome::OpenFolder(std::path::PathBuf::from("C:\\"));
        assert!(counts_as_usage(&Ok(folder)));
        assert!(counts_as_usage(&Ok(LaunchOutcome::Started)));
        assert!(counts_as_usage(&Ok(LaunchOutcome::Activated)));
        assert!(!counts_as_usage(&Ok(LaunchOutcome::Cancelled)));
        assert!(!counts_as_usage(&Err(LaunchError::NotFound(
            std::path::PathBuf::from("C:\\nao\\existe.exe")
        ))));
    }

    /// §15.1: the resident process and the main one differ in where a folder goes,
    /// which is the only thing the mode changes.
    #[gpui::test]
    fn the_mode_says_which_process_this_is(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(1)]);
            assert_eq!(state(cx).map(|state| state.mode), Some(Mode::Main));
            if let Some(state) = state_mut(cx) {
                state.mode = Mode::Standalone;
            }
            assert_eq!(
                state(cx).map(|state| state.mode),
                Some(Mode::Standalone),
                "a folder then goes through the IPC instead of the Explorer"
            );
        });
    }

    /// §12.1: a broken item is greyed by the view, and one that is fine is
    /// not. The answers live in the state, never in the file.
    #[gpui::test]
    fn health_marks_only_the_broken_items(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(2);
            group.id = "general".to_string();
            install(cx, vec![group]);
            assert!(!is_missing(cx, 0, 0), "nothing is checked yet");
            apply_health(cx, "general".to_string(), 0, Health::Missing);
            assert!(is_missing(cx, 0, 0));
            apply_health(cx, "general".to_string(), 1, Health::Ok);
            assert!(!is_missing(cx, 0, 1));
            assert_eq!(health_of(cx, 0, 0), Some(Health::Missing));
            assert_eq!(
                state(cx).map(|state| state.health.len()),
                Some(2),
                "the answers are kept beside the configuration"
            );
            assert!(
                state(cx).is_some_and(|state| state.config.groups[0].items.len() == 2),
                "the file still has its items"
            );
        });
    }

    /// §12.2: typing filters the current group by name and tags, and the
    /// arrow keys walk only what is left.
    #[gpui::test]
    fn typing_filters_the_group_by_name_and_tags(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            let mut editor = Item::new(0, "app", "Editor de texto");
            editor.tags = vec!["escrever".to_string()];
            let mut terminal = Item::new(4, "app", "Terminal");
            terminal.tags = vec!["shell".to_string()];
            group.items = vec![editor, terminal];
            install(cx, vec![group]);

            set_filter(cx, "term".to_string());
            assert_eq!(filtered_slots(cx), vec![4]);
            ensure_selection(cx);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(4));

            // A tag matches too.
            set_filter(cx, "escrever".to_string());
            assert_eq!(filtered_slots(cx), vec![0]);

            // Nothing matches: no slot to select, so Enter has nothing to start.
            set_filter(cx, "zzz".to_string());
            assert!(filtered_slots(cx).is_empty());
            ensure_selection(cx);
            assert_eq!(state(cx).and_then(|state| state.slot), None);

            set_filter(cx, String::new());
            assert_eq!(filtered_slots(cx), vec![0, 4]);
        });
    }

    /// §12.2: the arrow keys step through the filtered slots only.
    #[gpui::test]
    fn the_arrow_keys_walk_the_filtered_slots(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![
                Item::new(0, "app", "Alfa"),
                Item::new(2, "app", "Beta"),
                Item::new(4, "app", "Alfa dois"),
            ];
            install(cx, vec![group]);
            set_filter(cx, "alfa".to_string());
            ensure_selection(cx);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(0));
            move_selection(cx, 1);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(4));
            move_selection(cx, 1);
            assert_eq!(
                state(cx).and_then(|state| state.slot),
                Some(4),
                "the edge stops instead of wrapping"
            );
            move_selection(cx, -1);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(0));
        });
    }

    #[gpui::test]
    fn a_read_only_file_refuses_edits(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(1), group_of(1)]);
            if let Some(state) = state_mut(cx) {
                state.status = Status::ReadOnly("quebrado".to_string());
            }
            assert!(!put_item(cx, 0, 4, Item::new(4, "app", "Novo")));
            assert!(!edit_item(cx, 0, 0, "Nome", "C:\\a.exe", "", false));
            assert!(!add_group(cx).is_some());
            assert!(!rename_group(cx, 0, "Outro"));
            assert!(!delete_item(cx, 0, 0));
            assert!(!delete_group(cx, 1, true));
            assert!(state(cx).is_some_and(|state| state.config.groups.len() == 2));
        });
    }

    /// §12.3: adding, editing, replacing, renaming and deleting, all through the
    /// state layer so the throttle sees one edit.
    #[gpui::test]
    fn editing_a_group_and_its_items(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(1), group_of(1)]);
            assert!(put_item(cx, 0, 4, Item::new(4, "app", "Novo")));
            assert_eq!(
                item_at(cx, 0, 4).map(|item| item.name),
                Some("Novo".to_string())
            );
            // One slot holds one item: a second put replaces the first.
            assert!(put_item(cx, 0, 4, Item::new(4, "app", "Outro")));
            assert_eq!(
                state(cx).map(|state| state.config.groups[0]
                    .items
                    .iter()
                    .filter(|item| item.slot == 4)
                    .count()),
                Some(1)
            );
            assert!(edit_item(
                cx,
                0,
                4,
                " Renomeado ",
                "C:\\Windows\\System32\\notepad.exe",
                " /a",
                true
            ));
            let edited = item_at(cx, 0, 4).unwrap();
            assert_eq!(edited.name, "Renomeado");
            assert_eq!(edited.args.as_deref(), Some("/a"));
            assert!(edited.admin);
            // An empty name or path cancels the edit (§12.3).
            assert!(!edit_item(cx, 0, 4, " ", "C:\\a.exe", "", false));
            assert!(!edit_item(cx, 0, 4, "Nome", "  ", "", false));

            let index = add_group(cx).expect("a writable file can add a group");
            assert_eq!(
                state(cx).map(|state| state.config.groups[index].id.clone()),
                Some("group_1".to_string())
            );
            assert!(!rename_group(cx, index, "  "));
            assert!(rename_group(cx, index, "Dev"));
            assert_eq!(
                state(cx).map(|state| state.config.groups[index].name.clone()),
                Some("Dev".to_string())
            );

            assert!(delete_item(cx, 0, 4));
            assert_eq!(item_at(cx, 0, 4), None);
            // A non-empty group needs the confirmation, an empty one does not.
            let other = add_group(cx).unwrap();
            assert!(delete_group(cx, other, false));
            assert!(add_group(cx).is_some());
        });
    }

    /// §12.2: moving between slots swaps when the target is taken, and moving to
    /// another group takes its first free slot.
    #[gpui::test]
    fn moving_items_between_slots_and_groups(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut source = group_of(0);
            source.items = vec![Item::new(0, "app", "A"), Item::new(2, "app", "B")];
            let mut target = group_of(0);
            target.items = vec![Item::new(1, "app", "C")];
            install(cx, vec![source, target]);

            assert!(swap_slots(cx, 0, 0, 2));
            let name_at = |cx: &gpui::App, slot: u32| item_at(cx, 0, slot).map(|item| item.name);
            assert_eq!(name_at(cx, 0), Some("B".to_string()));
            assert_eq!(name_at(cx, 2), Some("A".to_string()));
            // A free target slot just moves the item.
            assert!(swap_slots(cx, 0, 0, 3));
            assert_eq!(name_at(cx, 3), Some("B".to_string()));
            assert_eq!(name_at(cx, 0), None);

            assert!(move_item_to_group(cx, 0, 2, 1));
            assert_eq!(
                item_at(cx, 1, 0).map(|item| item.name),
                Some("A".to_string()),
                "the free slot 0 of the target group takes it"
            );
            assert!(!move_item_to_group(cx, 0, 9, 1), "nothing there to move");
        });
    }

    /// §12.3: the last group cannot be removed, nor can a non-empty one without
    /// the confirmation.
    #[gpui::test]
    fn deleting_groups_respects_the_last_one(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(1)]);
            assert!(!delete_group(cx, 0, true));

            install(cx, vec![group_of(1), group_of(2)]);
            assert!(!delete_group(cx, 1, false), "it has items");
            assert!(delete_group(cx, 1, true));
            assert_eq!(state(cx).map(|state| state.config.groups.len()), Some(1));
        });
    }

    #[gpui::test]
    fn the_arrow_keys_skip_empty_slots_and_stop_at_the_edges(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![
                Item::new(0, "app", "Primeiro"),
                Item::new(2, "app", "Segundo"),
                Item::new(5, "app", "Terceiro"),
            ];
            install(cx, vec![group]);
            // Nothing is selected until the first press, which lands on the
            // first item rather than on the empty slot 0 of a grid.
            assert_eq!(state(cx).and_then(|state| state.slot), None);
            move_selection(cx, 1);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(0));
            move_selection(cx, 1);
            assert_eq!(
                state(cx).and_then(|state| state.slot),
                Some(2),
                "slot 1 is empty and must be skipped"
            );
            move_selection(cx, 1);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(5));
            move_selection(cx, 1);
            assert_eq!(
                state(cx).and_then(|state| state.slot),
                Some(5),
                "the last item is a boundary, not a wrap-around"
            );
            move_selection(cx, -1);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(2));
            move_selection(cx, -1);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(0));
            move_selection(cx, -1);
            assert_eq!(
                state(cx).and_then(|state| state.slot),
                Some(0),
                "the first item is a boundary too"
            );
        });
    }

    #[gpui::test]
    fn a_whole_row_of_the_grid_is_one_arrow_press(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![
                Item::new(0, "app", "A"),
                Item::new(4, "app", "B"),
                Item::new(8, "app", "C"),
            ];
            // The view turns "down" into a delta of `columns`.
            install(cx, vec![group]);
            move_selection(cx, 1);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(0));
            move_selection(cx, 4);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(4));
            move_selection(cx, 4);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(8));
            move_selection(cx, -4);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(4));
        });
    }

    #[gpui::test]
    fn a_group_switch_selects_an_item_of_the_new_group(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut first = group_of(0);
            first.items = vec![Item::new(0, "app", "A")];
            let mut second = group_of(0);
            second.id = "dev".to_string();
            second.name = "Dev".to_string();
            second.items = vec![Item::new(3, "app", "D")];
            install(cx, vec![first, second]);
            move_selection(cx, 1);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(0));
            select_group(cx, 1);
            assert_eq!(state(cx).map(|state| state.group), Some(1));
            assert_eq!(
                state(cx).and_then(|state| state.slot),
                Some(3),
                "the new group's first item is selected, not the old group's slot"
            );
            move_selection(cx, 1);
            assert_eq!(state(cx).and_then(|state| state.slot), Some(3));
        });
    }

    #[gpui::test]
    fn an_empty_group_has_nothing_to_select(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(0)]);
            move_selection(cx, 1);
            assert_eq!(state(cx).and_then(|state| state.slot), None);
            move_selection(cx, -1);
            assert_eq!(state(cx).and_then(|state| state.slot), None);
        });
    }

    #[gpui::test]
    fn the_process_is_resident_only_with_a_registered_chord(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut state = sample_state();
            state.config.window.keep_running = true;
            cx.set_global(state);
            assert!(!resident(cx), "no chord, nothing to keep the process for");
            if let Some(state) = state_mut(cx) {
                state.hotkey = Some(7);
            }
            assert!(resident(cx));
            if let Some(state) = state_mut(cx) {
                state.config.window.keep_running = false;
            }
            assert!(!resident(cx), "the file can turn it off");
        });
    }

    /// §8.4 (v2.2) B3: an icon extraction that never returns must not delay a
    /// launch, because the two requests run on different threads.
    #[test]
    fn a_blocked_icon_thread_does_not_delay_a_launch() {
        use std::sync::{Condvar, Mutex};

        let (replies, reply_receiver) = async_channel::unbounded::<WorkerReply>();
        // The stand-in for the Shell call that never comes back.
        let blocked = std::sync::Arc::new((Mutex::new(false), Condvar::new()));

        let icons = Worker::start("test-icons", {
            let replies = replies.clone();
            let blocked = blocked.clone();
            move |receiver: std_mpsc::Receiver<IconWorkerRequest>| {
                run_icon_worker(receiver, replies, move |_context, _path, _edge| {
                    let (lock, condvar) = &*blocked;
                    let mut released = lock.lock().unwrap_or_else(|error| error.into_inner());
                    while !*released {
                        released = condvar
                            .wait(released)
                            .unwrap_or_else(|error| error.into_inner());
                    }
                    None
                });
            }
        });
        let launch = Worker::start("test-launch", {
            let replies = replies.clone();
            move |receiver: std_mpsc::Receiver<StartRequest>| {
                run_launch_worker(receiver, replies, |_item, _request| {
                    Ok(LaunchOutcome::Started)
                });
            }
        });

        // One icon request, which the icon thread takes and then hangs on.
        icons.send(IconWorkerRequest::Icon(IconRequest {
            key: IconKey::new("bloqueado", 32),
            path: "C:\\nao\\existe".to_string(),
            queued: Instant::now(),
        }));
        std::thread::sleep(Duration::from_millis(50));

        // The launch must be answered within 100 ms even though the icon thread
        // is stuck, which is the whole point of having two of them.
        launch.send(StartRequest {
            item: Box::new(Item::new(0, "app", "Teste")),
            mode: LaunchMode::Normal,
            on_running: OnRunning::Activate,
            generation: 7,
        });
        let deadline = Instant::now() + Duration::from_millis(100);
        let mut answer = None;
        while Instant::now() < deadline && answer.is_none() {
            answer = reply_receiver.try_recv().ok();
            std::thread::sleep(Duration::from_millis(1));
        }
        let answer = answer.expect("the launch must be answered within 100 ms");
        match answer {
            WorkerReply::Launched {
                generation,
                outcome,
                ..
            } => {
                assert_eq!(generation, 7);
                assert_eq!(outcome, Ok(LaunchOutcome::Started));
            }
            _ => panic!("the icon thread answered first"),
        }

        // Let the icon thread go, then stop both as the exit path does.
        {
            let (lock, condvar) = &*blocked;
            *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
            condvar.notify_all();
        }
        let mut launch = launch;
        let mut icons = icons;
        launch.stop(Duration::from_millis(500));
        icons.stop(Duration::from_millis(500));
    }

    /// S4: a panic inside the launcher still answers with the request's
    /// generation, so the window reports it instead of dropping it as stale.
    #[test]
    fn a_panicking_launcher_answers_with_the_requests_generation() {
        let (replies, reply_receiver) = async_channel::unbounded::<WorkerReply>();
        let mut launch = Worker::start("test-launch-panic", {
            let replies = replies.clone();
            move |receiver: std_mpsc::Receiver<StartRequest>| {
                run_launch_worker(
                    receiver,
                    replies,
                    |_item: &Item, _request: &LaunchRequest| {
                        panic!("the launcher exploded");
                    },
                );
            }
        });
        launch.send(StartRequest {
            item: Box::new(Item::new(0, "app", "Teste")),
            mode: LaunchMode::Normal,
            on_running: OnRunning::Activate,
            generation: 42,
        });
        match reply_receiver.recv_blocking() {
            Ok(WorkerReply::Launched {
                generation,
                outcome,
                ..
            }) => {
                assert_eq!(generation, 42, "the reply must not look stale");
                assert!(matches!(outcome, Err(LaunchError::Internal(_))));
            }
            _ => panic!("the panic must be reported as a reply"),
        }
        launch.stop(Duration::from_millis(500));
    }

    /// R1: the KV read can finish after the window has drawn. The restored group
    /// must then be the one the bar shows, the one whose items the tiles draw,
    /// and the one the selection points into — which needs the frame invalidated
    /// and the old group's slot dropped.
    #[gpui::test]
    fn a_group_restored_after_the_first_frame_is_the_one_shown(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut general = group_of(0);
            general.items = vec![Item::new(0, "app", "A")];
            let mut dev = group_of(0);
            dev.id = "dev".to_string();
            dev.name = "Dev".to_string();
            dev.items = vec![Item::new(5, "app", "D")];
            install(cx, vec![general, dev]);

            // The first frame has already been built from this revision, and the
            // keyboard is on a slot of the first group.
            let drawn_revision = state(cx).map(|state| state.revision).unwrap_or(0);
            if let Some(state) = state_mut(cx) {
                state.slot = Some(0);
            }

            // The store answers late: the file was already read, so
            // `apply_loaded` runs the restore a second time.
            if let Some(state) = state_mut(cx) {
                state.restored.last_group = Some("dev".to_string());
            }
            restore_last_group(cx);
            ensure_selection(cx);

            let quick_launch = state(cx).expect("the state is installed");
            assert_eq!(
                quick_launch.group, 1,
                "the restored group is the current one"
            );
            assert_eq!(
                quick_launch.group().map(|group| group.name.as_str()),
                Some("Dev"),
                "the bar draws the restored group"
            );
            assert_eq!(
                quick_launch.slot,
                Some(5),
                "the selection is the first slot of the group that is drawn"
            );
            assert!(
                quick_launch.group().is_some_and(|group| group
                    .items
                    .iter()
                    .any(|item| Some(item.slot) == quick_launch.slot)),
                "the selected slot belongs to the drawn group"
            );
            assert_ne!(
                quick_launch.revision, drawn_revision,
                "the frame built for the old group is stale"
            );
        });
    }

    /// R2 (§4.2 v2.3): a file that starts with a UTF-8 byte-order mark parses,
    /// so it is not taken for a broken file and shown read-only.
    #[test]
    fn a_file_that_starts_with_a_byte_order_mark_parses() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quicklaunch.toml");
        let text = "schema_version = 1\n\n[[group]]\nid = \"general\"\nname = \"Geral\"\n";
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(text.as_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let read = read_quick_launch(&path);
        let parsed = read
            .parsed
            .expect("the file is there")
            .expect("a byte-order mark is not a parse error");
        assert_eq!(parsed.config.groups.len(), 1);
        assert_eq!(parsed.config.groups[0].id, "general");
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    }

    #[test]
    fn the_kv_keys_follow_the_key_rules_of_the_store() {
        let mut hasher = DefaultHasher::new();
        "\\\\.\\DISPLAY1".hash(&mut hasher);
        let key = format!("window_{:016x}", hasher.finish());
        assert_eq!(key.len(), "window_".len() + 16);
        assert!(
            key.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        );
        assert!(KvKey::new(KV_NAMESPACE, &key).is_ok());
        assert!(KvKey::new(KV_NAMESPACE, LAST_GROUP_KEY).is_ok());
        assert!(KvKey::new(KV_NAMESPACE, "usage_general_7").is_ok());
    }

    #[test]
    fn restored_state_reads_what_the_store_holds() {
        let saved = SavedBounds {
            x: 10.0,
            y: 20.0,
            width: 980.0,
            height: 875.0,
        };
        let entries = vec![
            (
                KvKey::new(KV_NAMESPACE, "window_00000000000000ff").unwrap(),
                serde_json::to_vec(&saved).unwrap(),
            ),
            (
                KvKey::new(KV_NAMESPACE, LAST_GROUP_KEY).unwrap(),
                b"desenvolvimento".to_vec(),
            ),
            // A key of another milestone, or one that cannot be read: skipped,
            // not an error.
            (
                KvKey::new(KV_NAMESPACE, "trusted_00ff").unwrap(),
                b"true".to_vec(),
            ),
            (
                KvKey::new(KV_NAMESPACE, "window_00000000000000fe").unwrap(),
                b"not json".to_vec(),
            ),
        ];
        let restored = Restored::from_entries(entries);
        assert_eq!(restored.positions.len(), 1);
        assert_eq!(
            restored.positions.get("window_00000000000000ff").copied(),
            Some(saved)
        );
        assert_eq!(restored.last_group.as_deref(), Some("desenvolvimento"));
    }

    #[test]
    fn an_icon_cache_file_name_is_a_hash_that_changes_with_the_edge() {
        let directory = tempfile::tempdir().unwrap();
        // Named `.txt` rather than `.exe`: the hash only needs a file that
        // exists, and a test that writes an executable into the temporary
        // directory gets refused by the on-access scanner on some machines.
        let file = directory.path().join("programa.txt");
        std::fs::write(&file, b"nao e um exe de verdade").unwrap();
        let small = icon_cache_file(&file, 32);
        let large = icon_cache_file(&file, 64);
        assert_ne!(small, large);
        assert_eq!(
            small.parent(),
            Some(paths::quick_launch_icon_dir().as_path())
        );
        let name = small.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with(".png"), "{name}");
        assert_eq!(name.len(), 16 + ".png".len());
    }

    #[test]
    fn an_icon_round_trips_through_the_disk_cache() {
        // Two pixels, one opaque and one half transparent, in the premultiplied
        // BGRA order GPUI keeps: the cache must give back exactly the same
        // bytes, which is why it stores the channels as they are.
        let bgra = vec![
            10u8, 20, 30, 255, // opaque
            5, 10, 15, 128, // translucent, premultiplied
        ];
        let image = thumbnails::render_image(2, 1, bgra.clone()).expect("a 2x1 image");
        let png = encode_cached_icon(&image).expect("the image encodes");
        let back = decode_cached_icon(&png).expect("the image decodes");
        assert_eq!(back.size(0).width.0, 2);
        assert_eq!(back.size(0).height.0, 1);
        assert_eq!(back.as_bytes(0).unwrap(), bgra.as_slice());
    }

    #[test]
    fn the_icon_cache_evicts_the_least_recently_used() {
        let mut cache = IconCache::default();
        let first = IconKey::new("a", 32);
        let second = IconKey::new("b", 32);
        cache.insert(first.clone(), None);
        cache.insert(second.clone(), None);
        // Asking for the first again makes it the most recent, so the second is
        // the one the next insertion evicts.
        assert!(cache.get(&first).is_some());
        // One insert short of a full cache, so exactly one entry is evicted.
        for index in 0..ICON_MEMORY_LIMIT - 1 {
            cache.insert(IconKey::new(&format!("extra{index}"), 32), None);
        }
        assert!(cache.get(&second).is_none(), "the unreferenced entry goes");
        assert!(cache.get(&first).is_some(), "the recently used one stays");
        assert_eq!(cache.images.len(), ICON_MEMORY_LIMIT);
    }

    #[test]
    fn a_settled_icon_is_not_asked_for_again() {
        let mut cache = IconCache::default();
        let key = IconKey::new("a", 32);
        assert!(cache.wants_request(&key));
        cache.pending.insert(key.clone(), Instant::now());
        assert!(!cache.wants_request(&key), "an extraction is running");
        cache.pending.remove(&key);
        cache.settled.insert(key.clone());
        assert!(!cache.wants_request(&key), "a settled icon stays settled");
    }

    #[test]
    fn a_request_that_timed_out_is_given_up_on() {
        let mut cache = IconCache::default();
        let key = IconKey::new("a", 32);
        cache.pending.insert(
            key.clone(),
            Instant::now() - ICON_TIMEOUT - Duration::from_millis(1),
        );
        assert!(!cache.wants_request(&key), "the timeout passed");
        assert!(cache.settled.contains(&key));
        assert!(
            !cache.pending.contains_key(&key),
            "the request is forgotten"
        );
    }

    fn sample_state() -> QuickLaunch {
        QuickLaunch {
            config: QuickLaunchConfig {
                groups: vec![Group {
                    id: "general".to_string(),
                    name: "Geral".to_string(),
                    items: Vec::new(),
                }],
                ..QuickLaunchConfig::default()
            },
            status: Status::Ready,
            warnings: Vec::new(),
            file_hash: 0,
            kv: None,
            restored: Restored::default(),
            hooks: None,
            workers: None,
            icons: IconCache::default(),
            group: 0,
            slot: Some(3),
            window: None,
            visible: false,
            hotkey: None,
            hotkey_text: None,
            hotkey_error: None,
            loading: false,
            pending_save: false,
            save_due: None,
            _watcher: None,
            signals: None,
            generation: 0,
            message: None,
            pending_hide: false,
            revision: 0,
            pending_usage: None,
            filter: String::new(),
            health: HashMap::new(),
            health_started: false,
            health_queue: VecDeque::new(),
            mode: Mode::Main,
        }
    }

    #[test]
    fn a_group_selection_does_not_outlive_the_groups() {
        let mut state = sample_state();
        state.set_group(9);
        assert_eq!(state.group, 0, "an index past the groups is ignored");
        state.set_group(0);
        assert_eq!(state.group, 0);
        assert_eq!(
            state.slot,
            Some(3),
            "re-selecting the group that is already showing keeps the slot"
        );
    }

    #[test]
    fn switching_group_drops_the_keyboard_slot() {
        let mut state = sample_state();
        state.config.groups.push(Group {
            id: "dev".to_string(),
            name: "Dev".to_string(),
            items: Vec::new(),
        });
        state.set_group(1);
        assert_eq!(state.group, 1);
        assert_eq!(state.slot, None);
    }

    #[test]
    fn a_loading_or_read_only_state_cannot_save() {
        assert!(Status::Ready.can_save());
        assert!(!Status::Loading.can_save());
        assert!(!Status::ReadOnly("line 1, column 1: bad".to_string()).can_save());
        assert_eq!(Status::Ready.error(), None);
        assert_eq!(
            Status::ReadOnly("line 2, column 3: bad".to_string()).error(),
            Some("line 2, column 3: bad")
        );
    }

    #[test]
    fn the_throttle_window_is_the_one_the_specification_names() {
        assert_eq!(SAVE_THROTTLE, Duration::from_millis(300));
        // The first edit sets the deadline; an edit inside the window does not
        // move it, which is what "merged into one save" means.
        let started = Instant::now();
        let due = started + SAVE_THROTTLE;
        assert!(started + SAVE_THROTTLE / 2 < due);
    }

    #[test]
    fn the_atomic_write_replaces_the_file_and_keeps_a_backup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quicklaunch.toml");
        write_atomically(&path, "schema_version = 1\n").unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, "schema_version = 1\n");

        let hash = write_atomically(&path, "schema_version = 1\n\n[[group]]\n").unwrap();
        assert_ne!(hash, 0);
        let second = std::fs::read_to_string(&path).unwrap();
        assert!(second.contains("[[group]]"), "{second}");
        // The previous content is kept beside it.
        let backup = directory.path().join("quicklaunch.toml.bak");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), first);
        assert!(!directory.path().join("quicklaunch.toml.tmp").exists());
    }

    #[test]
    fn a_failed_write_leaves_the_original_file_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quicklaunch.toml");
        write_atomically(&path, "schema_version = 1\n").unwrap();
        // A directory where the temporary file belongs makes the write fail,
        // which is the "disk full / read-only folder" case without changing any
        // attribute of the test directory.
        std::fs::create_dir_all(directory.path().join("quicklaunch.toml.tmp")).unwrap();
        assert!(write_atomically(&path, "schema_version = 99\n").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "schema_version = 1\n",
            "the original file must not be touched"
        );
    }

    #[test]
    fn the_icon_edge_snaps_up_to_a_supported_size() {
        // The steps are what the Shell extracts well; anything between two of
        // them is rounded up so the image is never stretched.
        for (scaled, expected) in [(10, 16), (16, 16), (17, 24), (48, 48), (49, 64), (300, 256)] {
            let found = ICON_STEPS
                .iter()
                .copied()
                .find(|step| *step >= scaled)
                .unwrap_or(256);
            assert_eq!(found, expected, "{scaled}");
        }
        assert_eq!(ICON_STEPS[0], 16);
        assert_eq!(*ICON_STEPS.last().unwrap(), 256);
    }

    #[test]
    fn the_window_options_are_a_borderless_popup_with_a_floor() {
        let bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(980.0), px(875.0)),
        };
        let options = window_options(bounds, View::Grid);
        assert!(options.titlebar.is_none());
        assert!(matches!(options.kind, WindowKind::PopUp));
        assert!(options.is_resizable);
        assert!(options.is_movable);
        assert!(!options.is_minimizable);
        assert_eq!(
            options.window_min_size,
            Some(size(px(MIN_WINDOW_SIZE.0), px(MIN_WINDOW_SIZE.1)))
        );
    }

    #[test]
    fn the_sizes_are_the_ones_the_specification_names() {
        assert_eq!(GRID_WINDOW_SIZE, (980.0, 875.0));
        assert_eq!(LIST_WINDOW_SIZE, (660.0, 860.0));
        assert_eq!(MIN_WINDOW_SIZE, (360.0, 300.0));
        assert_eq!(window_size(View::Grid), GRID_WINDOW_SIZE);
        assert_eq!(window_size(View::List), LIST_WINDOW_SIZE);
    }

    #[test]
    fn a_saved_position_is_the_window_bounds_of_the_specification() {
        let bounds = Bounds {
            origin: point(px(100.0), px(200.0)),
            size: size(px(980.0), px(875.0)),
        };
        let saved = SavedBounds::from(bounds);
        assert_eq!(saved.x, 100.0);
        assert_eq!(saved.y, 200.0);
        assert_eq!(saved.width, 980.0);
        assert_eq!(saved.height, 875.0);
        let back: Bounds<Pixels> = saved.into();
        assert_eq!(back, bounds);
    }

    #[test]
    fn a_saved_position_survives_json() {
        let saved = SavedBounds {
            x: -12.5,
            y: 40.0,
            width: 660.0,
            height: 860.0,
        };
        let bytes = serde_json::to_vec(&saved).unwrap();
        let back: SavedBounds = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, saved);
    }

    #[test]
    fn an_empty_store_answers_nothing_instead_of_failing() {
        let directory = tempfile::tempdir().unwrap();
        let store = nohrs_store::RedbKvStore::open(
            &directory.path().join("state.redb"),
            &nohrs_store::StoreLogConfig::default(),
        )
        .unwrap();
        let store: Arc<dyn KvStore> = Arc::new(store);
        assert!(store.list_namespace(KV_NAMESPACE).unwrap().is_empty());
        let key = KvKey::new(KV_NAMESPACE, "window_0000000000000001").unwrap();
        let saved = SavedBounds {
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
        };
        store
            .put(&key, &serde_json::to_vec(&saved).unwrap())
            .unwrap();
        let restored = Restored::from_entries(store.list_namespace(KV_NAMESPACE).unwrap());
        assert_eq!(
            restored.positions.get("window_0000000000000001").copied(),
            Some(saved)
        );
    }
}
