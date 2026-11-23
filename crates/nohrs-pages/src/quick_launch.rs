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
    ActionKind, CommandSpec, Group, GroupSource, Item, MAX_GROUP_NESTING, MAX_GROUP_REFERENCES,
    OnRunning, Parsed, QuickLaunchConfig, QuickLaunchError, View, Warning, WindowSettings,
    command_spec, group_source, item_for_path,
};
use nohrs_core::telemetry::LogErr;
use nohrs_services::launch::LaunchRegistry;
use nohrs_services::launch::{
    ExpandContext, Health, KIND_COMMAND, LaunchError, LaunchMode, LaunchOutcome, LaunchRequest,
};
use nohrs_store::{KvKey, KvOp, KvStore};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as MatchConfig, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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

/// How long a `folder:` group's listing is reused before the directory is read
/// again (§17.2 G.2).
const FOLDER_REFRESH: Duration = Duration::from_secs(30);

/// How many entries one `folder:` group may show (§17.2 G.2).
const MAX_FOLDER_ITEMS: usize = 512;

/// The most entries a `recent`/`most_used` group shows, whatever the columns
/// (§17.2 G.3, D7).
const MAX_USAGE_ITEMS: usize = 32;

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

/// The name prefix of a command's trust record (§6, §17.2 D.1).
const TRUSTED_PREFIX: &str = "trusted_";

/// How many hex characters of the SHA-256 digest name a trust record (§17.2 D.1).
const TRUST_KEY_HEX_LENGTH: usize = 32;

/// The item `type` that bundles other items of its group (§17.2 E).
pub(crate) const KIND_GROUP: &str = "group";

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
    /// The group launch this request belongs to, when it is one (§17.2 E.1).
    ///
    /// A batch sends several requests that share one id, so their replies are
    /// counted together instead of each one closing the window and counting a
    /// use of its own.
    batch: Option<u64>,
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
        /// The batch the request belonged to, echoed as it was sent (§17.2 E.1).
        batch: Option<u64>,
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
        let batch = request.batch;
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
                batch,
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
fn icon_cache_file(file: &Path, edge: u32, index: Option<u32>) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    file.to_string_lossy().to_lowercase().hash(&mut hasher);
    // E1: the icon at an index is a different picture of the same file, so the
    // index is part of the key.
    index.hash(&mut hasher);
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
    {
        let cache = icon_cache_file(&program, edge, Some(index));
        if let Ok(bytes) = std::fs::read(&cache)
            && let Some(image) = decode_cached_icon(&bytes)
        {
            return Some(Arc::new(image));
        }
        let (pixels, width, height) =
            nohrs_services::launch::indexed_icon(&program.to_string_lossy(), index, edge)?;
        let image = thumbnails::render_image(width, height, pixels)?;
        let image = Arc::new(image);
        if let Some(bytes) = encode_cached_icon(&image) {
            write_icon_cache(&cache, &bytes);
        }
        return Some(image);
    }
    // A custom `.png` is the picture itself; anything else asks the Shell, which
    // knows `.ico`, programs and folders (§9).
    if file
        .extension()
        .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("png"))
    {
        return decode_png_icon(&file, edge).map(Arc::new);
    }
    let cache = icon_cache_file(&file, edge, None);
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

/// One item's use count and last use, as `state.redb` keeps it (§6).
///
/// The state holds these in memory too: the `recent` and `most_used` groups are
/// built from them without a store read (§17.2 G.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Usage {
    count: u32,
    last: u64,
}

/// What the store remembered: where the window was on each display, which
/// group was last shown, which commands the user trusted (§6), and how often
/// each item was used.
#[derive(Debug, Clone, Default, PartialEq)]
struct Restored {
    positions: HashMap<String, SavedBounds>,
    last_group: Option<String>,
    /// The hex part of every `trusted_*` key the store held (§17.2 D.1). A
    /// resident process adds the ones the user confirmed to this set without a
    /// store, so they last until the process ends (§17.2 D.5).
    trusted: HashSet<String>,
    /// Every `usage_*` record, by group id and slot (§6, §17.2 G.3).
    usage: HashMap<(String, u32), Usage>,
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
            if let Some(trusted) = name.strip_prefix(TRUSTED_PREFIX) {
                // Only the values this program writes count: a stray key with
                // another value is not a confirmation anyone made.
                if bytes.as_slice() == b"true" && trusted.len() == TRUST_KEY_HEX_LENGTH {
                    restored.trusted.insert(trusted.to_string());
                }
                continue;
            }
            if let Some(usage) = name.strip_prefix("usage_")
                && let Some((group, slot)) = usage.rsplit_once('_')
                && let Ok(slot) = slot.parse::<u32>()
                && let Ok(record) = serde_json::from_slice::<Usage>(&bytes)
            {
                restored.usage.insert((group.to_string(), slot), record);
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

/// The trust key of one command (§17.2 D.1): the first 32 hex characters of
/// SHA-256 over its shell, script, working directory and two flags.
///
/// SHA-256 rather than `DefaultHasher`: a collision here would let one script
/// inherit another one's trust. The working directory is hashed as written, not
/// expanded, so the key follows the file's text and any edit asks again.
fn command_trust_key(item: &Item, spec: &CommandSpec) -> String {
    let mut hasher = Sha256::new();
    let hidden = if spec.hidden { "1" } else { "0" };
    let keep_open = if spec.keep_open { "1" } else { "0" };
    for part in [
        spec.shell.as_str(),
        spec.script.as_str(),
        item.cwd.as_deref().unwrap_or_default(),
        hidden,
        keep_open,
    ] {
        hasher.update(part.as_bytes());
        // A NUL cannot occur in a TOML string, so it cannot join two fields
        // into the same input as another combination.
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex.truncate(TRUST_KEY_HEX_LENGTH);
    hex
}

/// Whether a command still needs its first-run confirmation (§17.2 D.2).
///
/// Answers `None` for anything that is not a command with a valid spec: there
/// is no trust key to compare, and the launch itself reports what is wrong.
fn command_needs_confirmation(
    quick_launch: &QuickLaunch,
    item: &Item,
) -> Option<(CommandSpec, String)> {
    if item.action_kind() != ActionKind::Command {
        return None;
    }
    let spec = command_spec(item).ok()?;
    let trust_key = command_trust_key(item, &spec);
    (!quick_launch.restored.trusted.contains(&trust_key)).then_some((spec, trust_key))
}

/// What launching one `group` item expands to (§17.2 E.1).
#[derive(Debug, Default, PartialEq)]
struct GroupPlan {
    /// The entries to start, in the order the references name them, with nested
    /// groups flattened into their own entries.
    items: Vec<Item>,
    /// How many entries were left out because the plan is already full.
    excess: usize,
}

/// Follows one `group` item's references inside its own group (§17.2 E.1).
///
/// Only ids of the same group are found, so a reference to another group is
/// ignored; an id that was already expanded is skipped, which is what breaks a
/// cycle the parser may have repaired but a hand-built state can still hold.
/// Nested groups are followed up to [`MAX_GROUP_NESTING`] levels, and the plan
/// stops at [`MAX_GROUP_REFERENCES`] entries, reporting the rest as `excess`.
fn expand_group(group_items: &[Item], group_item: &Item) -> GroupPlan {
    fn walk(
        group_items: &[Item],
        references: &[String],
        depth: usize,
        visited: &mut HashSet<String>,
        plan: &mut GroupPlan,
    ) {
        for reference in references {
            let Some(item) = group_items
                .iter()
                .find(|candidate| candidate.id.as_deref() == Some(reference.as_str()))
            else {
                continue;
            };
            if !visited.insert(reference.clone()) {
                continue;
            }
            if item.action_kind() == ActionKind::Group {
                if depth < MAX_GROUP_NESTING {
                    walk(group_items, &item.items, depth + 1, visited, plan);
                }
                continue;
            }
            if plan.items.len() < MAX_GROUP_REFERENCES {
                plan.items.push(item.clone());
            } else {
                plan.excess = plan.excess.saturating_add(1);
            }
        }
    }

    let mut plan = GroupPlan::default();
    let mut visited: HashSet<String> = HashSet::new();
    // The item that was clicked must not be expanded through one of its own
    // references: the parser drops that reference, but the runtime rule has to
    // hold for a state that was edited in memory too.
    if let Some(id) = &group_item.id {
        visited.insert(id.clone());
    }
    walk(group_items, &group_item.items, 0, &mut visited, &mut plan);
    plan
}

/// The health of a `group` item (§17.2 E.2): `Invalid` when no reference leads
/// to an entry, `Ok` otherwise. The entries keep their own health answers.
fn group_health(group_items: &[Item], group_item: &Item) -> Health {
    if expand_group(group_items, group_item).items.is_empty() {
        Health::Invalid(nohrs_core::t!("Este grupo não tem itens válidos.").to_string())
    } else {
        Health::Ok
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
/// Why a chord was refused, as the binary knows it (§10, M3 S3).
///
/// The settings page has its own words for the launcher conflict, so it must be
/// able to tell the kinds apart without reading the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyRefusal {
    /// The chord belongs to the search launcher, which was asked for it first.
    LauncherConflict,
    /// Anything else: the text does not parse, the OS refused it, and so on.
    Other(String),
}

impl std::fmt::Display for HotkeyRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LauncherConflict => formatter.write_str("the Launcher's own shortcut"),
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

/// A chord the binary registers, and how it is taken away again.
///
/// The error is not `anyhow::Error` because the binary does not depend on
/// `anyhow`, and this struct is built there.
#[derive(Clone, Copy)]
pub struct HotkeyHooks {
    /// Parses and registers `text`, returning an opaque id for
    /// [`HotkeyHooks::unregister`].
    pub register: fn(&mut App, &str) -> std::result::Result<u64, HotkeyRefusal>,
    /// Removes a chord registered by [`HotkeyHooks::register`].
    pub unregister: fn(&mut App, u64) -> std::result::Result<(), String>,
}

/// One `command` the user has asked to run but has not confirmed yet
/// (§17.2 D.3). It keeps everything the eventual launch needs, so the item may
/// change on disk while the dialog is open without changing what was shown.
struct PendingCommand {
    /// The group the item came from, recorded once it starts.
    group: String,
    /// The slot the item came from, recorded once it starts.
    slot: u32,
    /// The item itself: the launch uses this copy, not the one in the file.
    item: Item,
    /// What the dialog shows and what the trust key was built from.
    spec: CommandSpec,
    /// The key written when the user clicks "Run" (§17.2 D.1).
    trust_key: String,
    /// How the click asked for the launch, kept until the dialog answers.
    mode: LaunchMode,
}

/// What the window's command dialog draws (§17.2 D.3): a copy of the pending
/// command's fields, so the view does not hold the state borrow while it builds
/// the overlay.
pub(crate) struct CommandConfirmation {
    /// What the tile shows.
    pub(crate) name: String,
    /// The shell the script runs through.
    pub(crate) shell: &'static str,
    /// The working directory as written, when one is set.
    pub(crate) cwd: Option<String>,
    /// Whether the script runs without a console window.
    pub(crate) hidden: bool,
    /// The whole script, shown as written.
    pub(crate) script: String,
    /// Whether this process has a store, so the confirmation survives a restart
    /// (§17.2 D.5).
    pub(crate) remembered: bool,
}

/// One untrusted `command` of a group, waiting for the group's single
/// confirmation (§17.2 E.1).
struct PendingGroupCommand {
    /// The command itself: the launch uses this copy.
    item: Item,
    /// What the dialog shows and what the trust key was built from.
    spec: CommandSpec,
    /// The key written when the user clicks "Run" (§17.2 D.1).
    trust_key: String,
}

/// A `group` item whose untrusted commands are waiting for one confirmation
/// (§17.2 E.1). Cancelling it aborts the whole group.
struct PendingGroup {
    /// The group the item belongs to, by id: a renamed group keeps its usage.
    group: String,
    /// The slot of the group item, which is the only item usage is recorded on.
    slot: u32,
    /// What the group's tile shows, for the dialog's heading.
    name: String,
    /// The expanded entries, in launch order.
    plan: GroupPlan,
    /// Every command of the plan that is not trusted yet.
    commands: Vec<PendingGroupCommand>,
    /// How the click asked for the launch, kept until the dialog answers.
    kind: ClickKind,
}

/// What the window's group dialog draws (§17.2 E.1): every untrusted command of
/// the group, listed together.
pub(crate) struct GroupConfirmation {
    /// What the group's tile shows.
    pub(crate) name: String,
    /// The commands the "Run" button would trust and start.
    pub(crate) commands: Vec<GroupCommandConfirmation>,
    /// Whether this process has a store, so the confirmation survives a restart
    /// (§17.2 D.5).
    pub(crate) remembered: bool,
}

/// One command listed by the group dialog (§17.2 E.1).
pub(crate) struct GroupCommandConfirmation {
    /// What the command's tile shows.
    pub(crate) name: String,
    /// The shell the script runs through.
    pub(crate) shell: &'static str,
    /// The working directory as written, when one is set.
    pub(crate) cwd: Option<String>,
    /// Whether the script runs without a console window.
    pub(crate) hidden: bool,
    /// The whole script, shown as written.
    pub(crate) script: String,
}

/// A program that turned out to be already running, waiting for the user to
/// choose between switching to it and starting another instance (§17.2 F.2).
struct PendingAlreadyRunning {
    /// The group the item came from, recorded once a launch happens.
    group: String,
    /// The slot the item came from, recorded once a launch happens.
    slot: u32,
    /// The item itself: the chosen request uses this copy.
    item: Item,
    /// The running process the "switch to it" button asks for.
    process_id: u32,
}

/// What the window's already-running dialog draws (§17.2 F.2).
pub(crate) struct AlreadyRunningPrompt {
    /// What the tile shows.
    pub(crate) name: String,
}

/// The group launch in flight (§17.2 E.1): replies are counted until the whole
/// batch has answered, and only then is the footer set, the usage recorded and
/// the window hidden.
struct PendingLaunchBatch {
    /// The shared id the requests carried, and the generation they answer with.
    generation: u64,
    /// The group item usage is recorded on, once anything started.
    usage: (String, u32),
    /// How many requests were sent, for the partial-failure message.
    total: usize,
    /// How many answers are still to come.
    remaining: usize,
    /// The entries whose launch failed, by name, in reply order.
    failed: Vec<String>,
    /// The folders the entries asked to open, in reply order.
    folders: Vec<PathBuf>,
    /// Whether at least one entry started, activated or opened a folder.
    used: bool,
    /// Entries left out of the plan because it was already full.
    excess: usize,
}

/// What the state remembers about one `folder:` group's listing (§17.2 G.2).
#[derive(Debug, Default)]
struct FolderRefresh {
    /// When the directory was last asked for, if it ever was.
    last: Option<Instant>,
    /// Which read is the current one: a reply from an older one is dropped.
    request: u64,
}

/// Why the dynamic groups are being regenerated (§17.2 G.1–G.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshReason {
    /// The file was read: every dynamic group is built again.
    Loaded,
    /// The window is being shown: its folder listings may be read again.
    Shown,
    /// The window switched to this group index (§17.2 G.2).
    GroupSwitch(usize),
    /// An item was launched: the `recent`/`most_used` groups change (§17.2 G.3).
    Launched,
}

/// What one read of a `folder:` group's directory found (§17.2 G.2).
#[derive(Debug, Clone, PartialEq)]
enum FolderListing {
    /// The entries, in the order the group shows them.
    Items(Vec<Item>),
    /// There is no such directory.
    Missing,
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
    /// Why the last registration failed, when it did (§10, M3 S3).
    hotkey_refusal: Option<HotkeyRefusal>,
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
    /// A command waiting for the user's first-run confirmation (§17.2 D.3).
    pending_command: Option<PendingCommand>,
    /// A group waiting for the user's confirmation of its untrusted commands
    /// (§17.2 E.1).
    pending_group: Option<PendingGroup>,
    /// A program found running that the user has not answered about yet
    /// (§17.2 F.2).
    pending_already_running: Option<PendingAlreadyRunning>,
    /// A group launch whose replies are still being counted (§17.2 E.1).
    pending_batch: Option<PendingLaunchBatch>,
    /// A hide asked for while GPUI had the window out for an update. The 400 ms
    /// tick performs it, outside any update, rather than the request being lost.
    pending_hide: bool,
    /// Bumped whenever what the window draws changes, so the view rebuilds its
    /// item list only then instead of on every frame (S6).
    revision: u64,
    /// The item a launch was asked for; counted only once it started (S7).
    pending_usage: Option<(String, u32)>,
    /// The modes this session sent to the launcher, for the tests (§17.2 F.5).
    #[cfg(test)]
    sent_launches: Vec<LaunchMode>,
    /// What the user typed to filter the current group (§12.2, M2).
    filter: String,
    /// The health of each item, by group id and slot (§12.1, M2). Runtime state,
    /// never written to the file.
    health: HashMap<(String, u32), Health>,
    /// Whether the health pass has been scheduled for this session.
    health_started: bool,
    /// Which process this is (§15.1).
    mode: Mode,
    /// The single-item health checks this session asked for, for the tests (R2).
    #[cfg(test)]
    health_checks: Vec<(String, u32)>,
    /// The variables a path may contain (§7), built once: reading `config.toml` for them on the foreground is
    /// what S2 removed.
    expand: ExpandContext,
    /// The items still to check (§12.1).
    health_queue: VecDeque<(String, u32, Item)>,
    /// `(dynamic group id, slot)` → the `(group id, slot)` a copy shown in a
    /// `recent`/`most_used` group came from (§17.2 G.1).
    origins: HashMap<(String, u32), (String, u32)>,
    /// The `folder:` groups whose directory could not be read, by group id
    /// (§17.2 G.2).
    folder_missing: HashSet<String>,
    /// When each `folder:` group was last expanded, and which read is current
    /// (§17.2 G.2).
    folder_refresh: HashMap<String, FolderRefresh>,
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
    // G.4: a copy shown in a `recent`/`most_used` group answers with the health
    // of the entry it came from, where the check was recorded.
    let (id, slot) = state
        .origins
        .get(&(id.to_string(), slot))
        .map(|(origin_group, origin_slot)| (origin_group.as_str(), *origin_slot))
        .unwrap_or((id, slot));
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
        // The copies a `recent`/`most_used` group shows are checked through the
        // entries they came from, which their own group already queues (§17.2
        // G.4).
        .filter(|group| {
            !matches!(
                group_source(group),
                Some(GroupSource::Recent | GroupSource::MostUsed)
            )
        })
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
                let quick_launch = state_mut(cx)?;
                let mut sent = 0usize;
                while sent < HEALTH_BATCH {
                    // An empty queue ends the pass rather than the state being
                    // gone, so this one is a `break`.
                    let Some((group, slot, item)) = quick_launch.health_queue.pop_front() else {
                        break;
                    };
                    // A group's health is its references (§17.2 E.2), which is
                    // string work on the state already in memory: no Shell call
                    // is involved, so it can be answered here.
                    if item.action_kind() == ActionKind::Group {
                        let health = quick_launch
                            .config
                            .groups
                            .iter()
                            .find(|candidate| candidate.id == group)
                            .map(|candidate| group_health(&candidate.items, &item));
                        if let Some(health) = health {
                            quick_launch.health.insert((group, slot), health);
                            quick_launch.touch();
                        }
                        sent = sent.saturating_add(1);
                        continue;
                    }
                    let workers = quick_launch.workers.as_ref()?;
                    workers.send_health(HealthRequest {
                        group,
                        slot,
                        item: Box::new(item),
                    });
                    sent = sent.saturating_add(1);
                }
                (sent > 0).then_some(sent)
            });
            match batch {
                Ok(Some(_)) => {
                    cx.update(|cx| cx.refresh_windows()).log_err();
                    continue;
                }
                _ => return,
            }
        }
    })
    .detach();
}

/// Records one answer, so the view can grey the item (§12.1).
fn apply_health(cx: &mut App, group: String, slot: u32, health: Health) {
    tracing::debug!(
        target: "quick_launch",
        group = %group,
        slot,
        ?health,
        "health checked"
    );
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    quick_launch.health.insert((group, slot), health);
    quick_launch.touch();
    cx.refresh_windows();
}

/// Forgets the health answer of one item and asks for it again (§12.1, B3).
///
/// Called after anything that can change what an item points at, so a fixed
/// path stops being grey without a restart.
fn recheck_health(cx: &mut App, group: usize, slot: u32) {
    let Some(state) = state_mut(cx) else {
        return;
    };
    let Some(group_id) = state.config.groups.get(group).map(|group| group.id.clone()) else {
        return;
    };
    let item = state
        .config
        .groups
        .get(group)
        .and_then(|group| group.items.iter().find(|item| item.slot == slot))
        .cloned();
    state.health.remove(&(group_id.clone(), slot));
    #[cfg(test)]
    state.health_checks.push((group_id.clone(), slot));
    let Some(item) = item else {
        return;
    };
    // A group's references are checked against the state in memory (§17.2 E.2),
    // not on the worker thread: nothing outside the process is involved.
    if item.action_kind() == ActionKind::Group {
        let health = state
            .config
            .groups
            .iter()
            .find(|candidate| candidate.id == group_id)
            .map(|candidate| group_health(&candidate.items, &item));
        if let Some(health) = health {
            state.health.insert((group_id, slot), health);
            state.touch();
        }
        return;
    }
    if let Some(workers) = state.workers.as_ref() {
        workers.send_health(HealthRequest {
            group: group_id,
            slot,
            item: Box::new(item),
        });
    }
}

/// Carries the health answer of one item to the place it moved to (§12.1, B3).
fn move_health(state: &mut QuickLaunch, from: (&str, u32), to: (&str, u32)) {
    let from_key = (from.0.to_string(), from.1);
    let to_key = (to.0.to_string(), to.1);
    // Both are taken out first: a swap exchanges them, and a move into a free
    // slot has nothing to bring back.
    let from_health = state.health.remove(&from_key);
    let to_health = state.health.remove(&to_key);
    if let Some(health) = from_health {
        state.health.insert(to_key, health);
    }
    if let Some(health) = to_health {
        state.health.insert(from_key, health);
    }
}

/// Drops every health answer of one group (§12.1, B3).
fn forget_group_health(state: &mut QuickLaunch, group_id: &str) {
    state.health.retain(|(id, _), _| id != group_id);
}
/// Whether the window is pinned (§11.1): the pin button's state and the
/// `topmost` the configuration already had.
pub(crate) fn is_locked(cx: &App) -> bool {
    state(cx).is_some_and(|quick_launch| quick_launch.config.window.topmost)
}

/// Flips the pin and returns its new state (§11.1).
///
/// A read-only file still toggles for this session; only a writable one marks
/// the state for the throttle, so the pin is kept on the next start too.
pub(crate) fn toggle_lock(cx: &mut App) -> bool {
    let Some(quick_launch) = state_mut(cx) else {
        return false;
    };
    quick_launch.config.window.topmost = !quick_launch.config.window.topmost;
    let locked = quick_launch.config.window.topmost;
    let save = quick_launch.status.can_save();
    quick_launch.touch();
    if save {
        mark_dirty(cx);
    }
    locked
}

/// Changes one field of the window settings (§13, §17.1 B).
///
/// Every control of the settings page's window group goes through here, so an
/// edit always touches the state, marks the file and tells the open window about
/// the two fields it keeps live: the pin and the summon chord.
pub(crate) fn set_window(cx: &mut App, change: impl FnOnce(&mut WindowSettings)) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let previous_topmost = quick_launch.config.window.topmost;
    let previous_hotkey = quick_launch.config.window.hotkey.clone();
    change(&mut quick_launch.config.window);
    let topmost = quick_launch.config.window.topmost;
    let hotkey = quick_launch.config.window.hotkey.clone();
    quick_launch.touch();
    mark_dirty(cx);
    if topmost != previous_topmost {
        sync_topmost(cx, topmost);
    }
    if hotkey != previous_hotkey {
        sync_hotkey(cx);
    }
}

/// Applies `topmost` to the open window, for a hot reload that changed it
/// (§11.1).
fn sync_topmost(cx: &mut App, topmost: bool) {
    let Some(handle) = live_window(cx) else {
        return;
    };
    handle
        .update(cx, |_root, window, _cx| set_topmost(window, topmost))
        .log_err();
    cx.refresh_windows();
}

/// Re-judges the kind of an edited item from its new path, off the foreground
/// (S4).
///
/// Only `app`, `file` and `folder` are re-judged: the other kinds are not built
/// from a path by this milestone, and a path that does not exist keeps its kind
/// so the health check can report it.
fn retype_after_edit(cx: &mut App, group: usize, slot: u32, previous_kind: &str) {
    if !matches!(previous_kind, "app" | "file" | "folder") {
        return;
    }
    let Some(item) = item_at(cx, group, slot) else {
        return;
    };
    let Some(path) = expanded_path(cx, &item) else {
        return;
    };
    let kind = previous_kind.to_string();
    cx.spawn(async move |cx| {
        let judged = cx
            .background_spawn(async move {
                let is_dir = path_is_directory(&path);
                path.is_file()
                    .then(|| {
                        nohrs_core::config::quick_launch::item_for_path(&path, slot, is_dir).kind
                    })
                    .or_else(|| is_dir.then(|| "folder".to_string()))
            })
            .await;
        let Some(judged) = judged else {
            return;
        };
        if judged == kind {
            return;
        }
        cx.update(|cx| {
            let Some(state) = state_mut(cx) else {
                return;
            };
            let Some(target) = state.config.groups.get_mut(group) else {
                return;
            };
            let Some(item) = target.items.iter_mut().find(|item| item.slot == slot) else {
                return;
            };
            item.kind = judged;
            state.touch();
            mark_dirty(cx);
            recheck_health(cx, group, slot);
            cx.refresh_windows();
        })
        .log_err();
    })
    .detach();
}

/// Deletes the usage records that name no item any more (§17.1 E2).
///
/// A `noh ql remove` or a hand edit leaves them behind, and the next item put in
/// that slot would inherit the count. Only the main process has the store: the
/// resident one runs without it.
///
/// Only `usage_*` keys are ever candidates (§17.2 D.8): a `trusted_*` record is
/// about a script, not a slot, and must survive the item being moved or deleted.
fn prune_usage(cx: &mut App) {
    let Some(quick_launch) = state(cx) else {
        return;
    };
    if quick_launch.mode != Mode::Main {
        return;
    }
    let Some(kv) = quick_launch.kv.clone() else {
        return;
    };
    let valid: std::collections::HashSet<(String, u32)> = quick_launch
        .config
        .groups
        .iter()
        // A dynamic group's entries are generated, so a record naming one is
        // not about an entry the file has: keeping it would keep a use that
        // belongs to nobody (§17.2 G.8).
        .filter(|group| !group_source(group).is_some_and(|source| source.is_dynamic()))
        .flat_map(|group| {
            group
                .items
                .iter()
                .map(move |item| (group.id.clone(), item.slot))
        })
        .collect();
    cx.background_spawn(async move {
        let entries = match kv.list_namespace(KV_NAMESPACE) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(target: "quick_launch", "could not list usage records: {error}");
                return;
            }
        };
        let prefix = format!("{KV_NAMESPACE}.");
        let stale: Vec<KvOp> = entries
            .into_iter()
            .filter_map(|(key, _)| {
                let name = key.as_str().strip_prefix(&prefix)?;
                let rest = name.strip_prefix("usage_")?;
                let (group, slot) = rest.rsplit_once('_')?;
                let slot = slot.parse::<u32>().ok()?;
                (!valid.contains(&(group.to_string(), slot))).then_some(KvOp::Delete { key })
            })
            .collect();
        if stale.is_empty() {
            return;
        }
        tracing::debug!(
            target: "quick_launch",
            keys = stale.len(),
            "deleting usage records of items that are gone"
        );
        if let Err(error) = kv.batch(stale) {
            tracing::warn!(target: "quick_launch", "could not delete stale usage records: {error}");
        }
    })
    .detach();
}

/// Whether the group at `index` may have its items edited (§17.2 G.5).
///
/// A dynamic group's entries are generated at runtime: they are not the user's
/// to add, move or delete. The group itself may still be renamed, reordered and
/// deleted.
pub(crate) fn group_editable(cx: &App, index: usize) -> bool {
    let Some(quick_launch) = state(cx) else {
        return false;
    };
    if !quick_launch.status.can_save() {
        return false;
    }
    quick_launch
        .config
        .groups
        .get(index)
        .is_some_and(|group| group.source.is_none())
}

/// What an empty dynamic group shows instead of "no items" (§17.2 G.2, G.3).
pub(crate) fn group_notice(cx: &App, index: usize) -> Option<&'static str> {
    let quick_launch = state(cx)?;
    let group = quick_launch.config.groups.get(index)?;
    match group_source(group)? {
        GroupSource::Folder(_) => quick_launch
            .folder_missing
            .contains(&group.id)
            .then(|| nohrs_core::t!("Pasta não encontrada")),
        GroupSource::Recent | GroupSource::MostUsed => (quick_launch.mode == Mode::Standalone)
            .then(|| nohrs_core::t!("Indisponível no modo residente")),
        GroupSource::Unknown(_) => None,
    }
}

/// Regenerates every dynamic group's items: what can be answered from memory
/// now, and the folder listings that are due (§17.2 G.1).
pub(crate) fn refresh_dynamic_groups(cx: &mut App) {
    refresh_dynamic_groups_for(cx, RefreshReason::Shown);
}

/// Regenerates the dynamic groups for `reason` (§17.2 G.1–G.3).
fn refresh_dynamic_groups_for(cx: &mut App, reason: RefreshReason) {
    let mut requests: Vec<(String, String, PathBuf, u64)> = Vec::new();
    {
        let Some(quick_launch) = state_mut(cx) else {
            return;
        };
        // The `recent`/`most_used` groups are copies of entries elsewhere, so
        // they are rebuilt from the use records held in memory (§17.2 G.3).
        if !matches!(reason, RefreshReason::GroupSwitch(_)) {
            let mode = quick_launch.mode;
            let usage_groups: Vec<(String, bool)> = quick_launch
                .config
                .groups
                .iter()
                .filter_map(|group| match group_source(group) {
                    Some(GroupSource::Recent) => Some((group.id.clone(), false)),
                    Some(GroupSource::MostUsed) => Some((group.id.clone(), true)),
                    _ => None,
                })
                .collect();
            for (group_id, most_used) in usage_groups {
                // A resident process has no store, so it has no use records to
                // show; the group says so instead (§17.2 G.3).
                let generated = if mode == Mode::Standalone {
                    Vec::new()
                } else {
                    usage_group_items(
                        &quick_launch.config,
                        &quick_launch.restored.usage,
                        most_used,
                    )
                };
                apply_usage_group(quick_launch, &group_id, generated);
            }
        }
        let now = Instant::now();
        for index in 0..quick_launch.config.groups.len() {
            if let RefreshReason::GroupSwitch(switched) = reason
                && switched != index
            {
                continue;
            }
            let (group_id, source, raw) = {
                let Some(group) = quick_launch.config.groups.get(index) else {
                    continue;
                };
                let Some(GroupSource::Folder(raw)) = group_source(group) else {
                    continue;
                };
                (
                    group.id.clone(),
                    group.source.clone().unwrap_or_default(),
                    raw,
                )
            };
            let request = {
                let refresh = quick_launch
                    .folder_refresh
                    .entry(group_id.clone())
                    .or_default();
                if refresh
                    .last
                    .is_some_and(|last| now.duration_since(last) < FOLDER_REFRESH)
                {
                    continue;
                }
                refresh.last = Some(now);
                refresh.request = refresh.request.wrapping_add(1);
                refresh.request
            };
            match nohrs_services::launch::expand(&raw, &quick_launch.expand) {
                Ok(path) => requests.push((group_id, source, path, request)),
                // A path whose variables cannot be resolved names no directory
                // this process can read either (§17.2 G.2).
                Err(error) => {
                    tracing::debug!(
                        target: "quick_launch",
                        "could not resolve the folder of {group_id:?}: {error}"
                    );
                    quick_launch.config.groups[index].items.clear();
                    quick_launch.folder_missing.insert(group_id);
                    quick_launch.touch();
                }
            }
        }
    }
    for (group_id, source, path, request) in requests {
        cx.spawn(async move |cx| {
            let listing = cx.background_spawn(async move { read_folder(&path) }).await;
            cx.update(|cx| apply_folder_listing(cx, &group_id, &source, request, listing))
                .ok();
        })
        .detach();
    }
    cx.refresh_windows();
}

/// The number of entries a `recent`/`most_used` group shows (§17.2 G.3, D7).
fn usage_group_limit(columns: u8) -> usize {
    usize::from(columns.max(1))
        .saturating_mul(2)
        .min(MAX_USAGE_ITEMS)
}

/// The copies a `recent`/`most_used` group shows, each with the entry it came
/// from, in the order the group displays them (§17.2 G.3).
///
/// Only entries the configuration still has are candidates, and a dynamic
/// group's own entries never are: they are copies of copies. `most_used` orders
/// by count and then by the latest use; `recent` orders by the latest use.
fn usage_group_items(
    config: &QuickLaunchConfig,
    usage: &HashMap<(String, u32), Usage>,
    most_used: bool,
) -> Vec<(Item, (String, u32))> {
    let mut candidates: Vec<(Item, (String, u32), Usage)> = Vec::new();
    for ((group_id, slot), record) in usage {
        let Some(group) = config.groups.iter().find(|group| group.id == *group_id) else {
            continue;
        };
        if group_source(group).is_some_and(|source| source.is_dynamic()) {
            continue;
        }
        let Some(item) = group.items.iter().find(|item| item.slot == *slot) else {
            continue;
        };
        candidates.push((item.clone(), (group.id.clone(), *slot), *record));
    }
    candidates.sort_by(
        |(_, (left_group, left_slot), left), (_, (right_group, right_slot), right)| {
            let order = if most_used {
                right
                    .count
                    .cmp(&left.count)
                    .then(right.last.cmp(&left.last))
            } else {
                right.last.cmp(&left.last)
            };
            // A stable order for records that tie, so the group does not reshuffle
            // between two frames.
            order
                .then(left_group.cmp(right_group))
                .then(left_slot.cmp(right_slot))
        },
    );
    candidates.truncate(usage_group_limit(config.window.columns));
    candidates
        .into_iter()
        .enumerate()
        .map(|(index, (mut item, origin, _))| {
            item.slot = u32::try_from(index).unwrap_or_default();
            (item, origin)
        })
        .collect()
}

/// Replaces one `recent`/`most_used` group's copies and records where each one
/// came from (§17.2 G.1).
fn apply_usage_group(
    quick_launch: &mut QuickLaunch,
    group_id: &str,
    generated: Vec<(Item, (String, u32))>,
) {
    let Some(group) = quick_launch
        .config
        .groups
        .iter_mut()
        .find(|group| group.id == group_id)
    else {
        return;
    };
    group.items = generated.iter().map(|(item, _)| item.clone()).collect();
    quick_launch.health.retain(|(id, _), _| id != group_id);
    quick_launch.origins.retain(|(id, _), _| id != group_id);
    for (item, origin) in generated {
        quick_launch
            .origins
            .insert((group_id.to_string(), item.slot), origin);
    }
    quick_launch.touch();
}

/// Reads one directory for a `folder:` group (§17.2 G.2).
///
/// Off the foreground: a directory on a dead network path must not hold a frame
/// (§8.4, §18). Only the entries directly in the directory are listed, sorted by
/// name without regard to case, hidden and system entries left out, at most
/// [`MAX_FOLDER_ITEMS`] of them, with slots from 0.
fn read_folder(path: &Path) -> FolderListing {
    let Ok(entries) = std::fs::read_dir(path) else {
        return FolderListing::Missing;
    };
    let mut found: Vec<(String, PathBuf, bool)> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(
                    target: "quick_launch",
                    "could not read an entry of {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        let entry_path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::debug!(
                    target: "quick_launch",
                    "could not read {}: {error}",
                    entry_path.display()
                );
                continue;
            }
        };
        if is_hidden_or_system(&entry_path, &metadata) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        found.push((name, entry_path, metadata.is_dir()));
    }
    // The path breaks a tie between two names that only differ in case, so the
    // same directory always produces the same order.
    found.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    found.truncate(MAX_FOLDER_ITEMS);
    FolderListing::Items(
        found
            .into_iter()
            .enumerate()
            .map(|(slot, (_, entry_path, is_dir))| {
                item_for_path(&entry_path, u32::try_from(slot).unwrap_or_default(), is_dir)
            })
            .collect(),
    )
}

/// Whether a directory entry is hidden or a system entry, which a `folder:`
/// group leaves out (§17.2 G.2).
#[cfg(windows)]
fn is_hidden_or_system(_path: &Path, metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    metadata.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

/// On this platform a hidden entry is one whose name starts with a dot.
#[cfg(not(windows))]
fn is_hidden_or_system(path: &Path, _metadata: &std::fs::Metadata) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

/// Puts one finished folder listing into the state (§17.2 G.2).
fn apply_folder_listing(
    cx: &mut App,
    group_id: &str,
    source: &str,
    request: u64,
    listing: FolderListing,
) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let Some(index) = quick_launch
        .config
        .groups
        .iter()
        .position(|group| group.id == group_id)
    else {
        return;
    };
    // The group may have been renamed, deleted or pointed somewhere else while
    // the directory was being read: only the read that is still current counts.
    if quick_launch.config.groups[index].source.as_deref() != Some(source) {
        return;
    }
    if quick_launch
        .folder_refresh
        .get(group_id)
        .is_none_or(|refresh| refresh.request != request)
    {
        return;
    }
    match listing {
        FolderListing::Items(items) => {
            quick_launch.config.groups[index].items = items;
            quick_launch.folder_missing.remove(group_id);
        }
        FolderListing::Missing => {
            quick_launch.config.groups[index].items.clear();
            quick_launch.folder_missing.insert(group_id.to_string());
        }
    }
    quick_launch.health.retain(|(id, _), _| id != group_id);
    quick_launch.origins.retain(|(id, _), _| id != group_id);
    quick_launch.touch();
    cx.refresh_windows();
}

/// The entry a use of `group`/`slot` is recorded on: the copy in a
/// `recent`/`most_used` group belongs to the entry it was copied from
/// (§17.2 G.4).
fn usage_target(cx: &App, group: &str, slot: u32) -> (String, u32) {
    state(cx)
        .and_then(|quick_launch| {
            quick_launch
                .origins
                .get(&(group.to_string(), slot))
                .cloned()
        })
        .unwrap_or_else(|| (group.to_string(), slot))
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
#[cfg_attr(not(test), allow(dead_code))] // M3's settings page replaces items too
pub(crate) fn put_item(cx: &mut App, group: usize, slot: u32, mut item: Item) -> bool {
    if !group_editable(cx, group) {
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
    recheck_health(cx, group, slot);
    true
}

/// Everything the settings page's item form can change (§17.1 B).
#[derive(Clone)]
pub(crate) struct ItemEdit {
    /// The `type` the form chose. A form that cannot offer the item's own type
    /// keeps it, so the settings page never rewrites a kind it cannot build.
    pub kind: String,
    /// What the tile shows.
    pub name: String,
    /// The target, possibly with the variables of §7.
    pub path: String,
    /// Command-line arguments, empty for none.
    pub args: String,
    /// Working directory, empty for the executable's own folder.
    pub cwd: String,
    /// Start elevated (`runas`).
    pub admin: bool,
    /// Custom icon, empty for the program's own.
    pub icon: String,
    /// Extra words the filter matches on.
    pub tags: Vec<String>,
    /// The `command` fields the settings form offers (§17.2 A.3, D.6). `None`
    /// keeps the item's `extra` as it is, which is what the window's edit dialog
    /// and every other kind of item need.
    pub command: Option<CommandEdit>,
    /// The slots a `group` item launches, in start order (§17.2 E.3). `None`
    /// leaves the item's references as they are, which is what every other kind
    /// of item wants; the settings form fills it when the kind is `group`.
    pub group_items: Option<Vec<u32>>,
}

/// What the settings page's form knows about a `command` item (§17.2 A.3).
#[derive(Clone)]
pub(crate) struct CommandEdit {
    /// One of `cmd`, `pwsh`, `powershell`, `python`.
    pub shell: String,
    /// The script, passed to the shell as written.
    pub script: String,
    /// Run without a console window.
    pub hidden: bool,
    /// Keep the console open after the script ends.
    pub keep_open: bool,
}

/// Writes the ids a saved `group` item refers to (§17.2 E.3), giving an id to
/// every item that has none: the group item itself, then the selected entries in
/// start order.
///
/// An id is `item_<slot>`; when another item of the group already holds that id,
/// `_2`, `_3`… is appended until it is free. All of it happens in the caller's
/// single write, so the references cannot be saved half-assigned.
fn group_selection_ids(items: &mut [Item], own_slot: u32, selected: &[u32]) -> Vec<String> {
    fn assign(items: &mut [Item], slot: u32) -> Option<String> {
        let index = items.iter().position(|item| item.slot == slot)?;
        if let Some(id) = items.get(index).and_then(|item| item.id.clone()) {
            return Some(id);
        }
        let mut candidate = format!("item_{slot}");
        let mut suffix = 1u32;
        while items
            .iter()
            .any(|item| item.id.as_deref() == Some(candidate.as_str()))
        {
            suffix = suffix.saturating_add(1);
            candidate = format!("item_{slot}_{suffix}");
        }
        items.get_mut(index)?.id = Some(candidate.clone());
        Some(candidate)
    }

    assign(items, own_slot);
    selected
        .iter()
        .filter_map(|slot| assign(items, *slot))
        .collect()
}

/// Applies the item form of the settings page (§17.1 B), with the rules of the
/// window's edit dialog: no empty name or path, no edits while the file is
/// read-only, a fresh health check and a kind judged again from a new path.
///
/// A `command` saved through the form is trusted at once (D5, §17.2 D.6): the
/// form is the one path where the user read and wrote the script themselves.
pub(crate) fn update_item(cx: &mut App, group: usize, slot: u32, edit: &ItemEdit) -> bool {
    let name = edit.name.trim();
    let path = edit.path.trim();
    // A `group` has no path and a `command` keeps its target in `extra`; the
    // rest of the form still needs one.
    let is_group = edit.kind == KIND_GROUP;
    let command_extra = match (edit.kind.as_str(), edit.command.as_ref()) {
        (KIND_COMMAND, Some(command)) => {
            let mut probe = Item::new(0, KIND_COMMAND, "command");
            probe
                .extra
                .insert("shell".to_string(), command.shell.clone().into());
            probe
                .extra
                .insert("script".to_string(), command.script.clone().into());
            if command.hidden {
                probe.extra.insert("hidden".to_string(), true.into());
            }
            if command.keep_open {
                probe.extra.insert("keep_open".to_string(), true.into());
            }
            if command_spec(&probe).is_err() {
                return false;
            }
            Some(probe.extra)
        }
        _ => None,
    };
    let is_command = command_extra.is_some();
    // An item without a name would be an empty tile, and one without a path
    // could never start: the form treats both as "cancel".
    if name.is_empty()
        || (!is_command && !is_group && path.is_empty())
        || !group_editable(cx, group)
    {
        return false;
    }
    let cwd = edit.cwd.trim();
    // The key is built from what the form is saving, before the state borrow.
    let trust_key = command_extra.as_ref().and_then(|extra| {
        let mut probe = Item::new(slot, KIND_COMMAND, name);
        probe.extra = extra.clone();
        probe.cwd = (!cwd.is_empty()).then(|| cwd.to_string());
        command_spec(&probe)
            .ok()
            .map(|spec| command_trust_key(&probe, &spec))
    });
    let Some(state) = state_mut(cx) else {
        return false;
    };
    let Some(target) = state.config.groups.get_mut(group) else {
        return false;
    };
    // A `group`'s references, and the ids they need, are settled before the
    // item itself is borrowed: this is the one write that may touch several
    // items of the group (§17.2 E.3).
    let references = is_group.then(|| {
        group_selection_ids(
            &mut target.items,
            slot,
            edit.group_items.as_deref().unwrap_or_default(),
        )
    });
    let Some(item) = target.items.iter_mut().find(|item| item.slot == slot) else {
        return false;
    };
    // Only the kinds the form offers may be introduced; every other kind
    // (group, plugin:…) is kept as it is, since this milestone cannot build
    // those and changing one would change what the entry means.
    if !matches!(
        edit.kind.as_str(),
        "app" | "folder" | "file" | "url" | "uwp" | KIND_COMMAND | KIND_GROUP
    ) && edit.kind != item.kind
    {
        return false;
    }
    let previous_kind = item.kind.clone();
    let previous_path = item.path.clone();
    item.kind = edit.kind.clone();
    item.name = name.to_string();
    item.path = (!is_command && !is_group).then(|| path.to_string());
    let args = edit.args.trim();
    item.args = (!is_group && !args.is_empty()).then(|| args.to_string());
    item.cwd = (!is_group && !cwd.is_empty()).then(|| cwd.to_string());
    let icon = edit.icon.trim();
    item.icon = (!icon.is_empty()).then(|| icon.to_string());
    item.admin = if is_command || is_group {
        false
    } else {
        edit.admin
    };
    item.items = references.unwrap_or_default();
    item.tags = edit
        .tags
        .iter()
        .map(|tag| tag.trim())
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect();
    match command_extra {
        Some(extra) => item.extra = extra,
        // A command changed into another kind has no use for its fields, and
        // the file only keeps `extra` for `command` and `plugin:*` (§4.2).
        None if previous_kind == KIND_COMMAND => item.extra.clear(),
        None => {}
    }
    let path_changed = item.path != previous_path;
    state.touch();
    mark_dirty(cx);
    recheck_health(cx, group, slot);
    // S4: a path that now points at a folder (or a program) changes the kind, so
    // the item keeps working after the edit. An unchanged path keeps the kind
    // the form chose.
    if path_changed && !is_command && !is_group {
        retype_after_edit(cx, group, slot, &previous_kind);
    }
    if let Some(trust_key) = trust_key {
        remember_trust(cx, trust_key);
    }
    true
}

/// Applies the four fields the window's edit dialog offers (§12.3): name, path,
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
    if !group_editable(cx, group) {
        return false;
    }
    // The dialog does not offer the other fields, so they keep what the item
    // already has; a missing item is refused by `update_item` too.
    let Some(item) = item_at(cx, group, slot) else {
        return false;
    };
    let previous_kind = item.kind.clone();
    let path_changed = item.path.as_deref().unwrap_or_default().trim() != path.trim();
    let edited = update_item(
        cx,
        group,
        slot,
        &ItemEdit {
            kind: item.kind,
            name: name.to_string(),
            path: path.to_string(),
            args: args.to_string(),
            cwd: item.cwd.unwrap_or_default(),
            admin,
            icon: item.icon.unwrap_or_default(),
            tags: item.tags,
            command: None,
            group_items: None,
        },
    );
    // The dialog judges the kind after every edit, as it did before the form
    // existed; the form only asks for that when its path changed, so a kind
    // chosen by hand with the path kept is not overruled from the disk.
    if edited && !path_changed {
        retype_after_edit(cx, group, slot, &previous_kind);
    }
    edited
}

/// Renames one item without touching anything else (§17.2 E.4).
///
/// The window's edit dialog offers only the name for a `group` item: its path,
/// arguments and working directory do not exist, and its contents are chosen in
/// the settings page.
pub(crate) fn rename_item(cx: &mut App, group: usize, slot: u32, name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() || !editable(cx) {
        return false;
    }
    let Some(state) = state_mut(cx) else {
        return false;
    };
    let Some(item) = state
        .config
        .groups
        .get_mut(group)
        .and_then(|target| target.items.iter_mut().find(|item| item.slot == slot))
    else {
        return false;
    };
    if item.name == name {
        return true;
    }
    item.name = name.to_string();
    state.touch();
    mark_dirty(cx);
    true
}

/// Adds a group at the end and makes it the current one (§12.3). The caller
/// starts the inline rename the specification asks for.
pub(crate) fn add_group(cx: &mut App) -> Option<usize> {
    add_group_with_source(cx, None)
}

/// Adds a group whose items are generated from `source` instead of edited
/// (§17.2 G.6): `folder:<path>`, `recent` or `most_used`.
pub(crate) fn add_group_with_source(cx: &mut App, source: Option<String>) -> Option<usize> {
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
        source,
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

/// Moves a group to another place in the bar (§17.1 B), by swapping it with its
/// neighbour: the settings page's "up" and "down" buttons.
///
/// Health answers and usage records are kept by group id, so a move needs no
/// migration; the window follows the swap when it was showing one of the two.
pub(crate) fn move_group(cx: &mut App, from: usize, to: usize) -> bool {
    if from == to || !editable(cx) {
        return false;
    }
    let Some(quick_launch) = state_mut(cx) else {
        return false;
    };
    if from >= quick_launch.config.groups.len() || to >= quick_launch.config.groups.len() {
        return false;
    }
    quick_launch.config.groups.swap(from, to);
    if quick_launch.group == from {
        quick_launch.group = to;
    } else if quick_launch.group == to {
        quick_launch.group = from;
    }
    // The slot belonged to the group that just left this index.
    quick_launch.slot = None;
    quick_launch.touch();
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
    let (id, removed): (String, Vec<(String, u32)>) = {
        let Some(group) = state.config.groups.get(index) else {
            return false;
        };
        if !force && !group.items.is_empty() {
            return false;
        }
        let id = group.id.clone();
        let removed = group
            .items
            .iter()
            .map(|item| (id.clone(), item.slot))
            .collect();
        (id, removed)
    };
    forget_group_health(state, &id);
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
    if !group_editable(cx, group) {
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
    state.health.remove(&(id.clone(), slot));
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
    if from_group == to_group || !group_editable(cx, from_group) || !group_editable(cx, to_group) {
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
        if let Some(source) = state.config.groups.get_mut(from_group) {
            source.items.push(item);
        }
        return false;
    };
    let new_slot = first_free_slot(target);
    item.slot = new_slot;
    target.items.push(item);
    target.items.sort_by_key(|item| item.slot);
    // B3 and S3: the answer and the usage record follow the item into its new
    // group and slot. The source is the `slot` the caller named, not the one the
    // item landed on (R1): with the two confused, the move took the records of
    // whatever sat at `new_slot` in the source group.
    let from_id = state
        .config
        .groups
        .get(from_group)
        .map(|group| group.id.clone());
    let to_id = state
        .config
        .groups
        .get(to_group)
        .map(|group| group.id.clone());
    if let (Some(from_id), Some(to_id)) = (from_id.clone(), to_id.clone()) {
        move_health(state, (&from_id, slot), (&to_id, new_slot));
    }
    state.touch();
    mark_dirty(cx);
    if let (Some(from_id), Some(to_id)) = (from_id, to_id) {
        move_usage(cx, (from_id, slot), (to_id, new_slot));
    }
    true
}

/// Moves an item to another slot of its own group; when the target is taken the
/// two swap (§12.2).
pub(crate) fn swap_slots(cx: &mut App, group: usize, from: u32, to: u32) -> bool {
    if from == to || !group_editable(cx, group) {
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
    // B3 and S3: the answers and the usage records belong to the items, not to
    // the slots.
    let group_id = state.config.groups.get(group).map(|group| group.id.clone());
    if let Some(group_id) = group_id.as_deref() {
        move_health(state, (group_id, from), (group_id, to));
    }
    state.touch();
    mark_dirty(cx);
    if let Some(group_id) = group_id {
        swap_usage(cx, group_id, from, to);
    }
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

/// Puts `items` in the group's next free slots, from `start` on, in order
/// (§12.2). Nothing already there is replaced: a drop onto a taken
/// cell takes the first free slot after it.
///
/// This is the only way a drop or a file picker adds entries; `put_item` is
/// for edits, where replacing is what the user asked for.
pub(crate) fn place_items(cx: &mut App, group: usize, start: u32, items: Vec<Item>) -> bool {
    if !group_editable(cx, group) || items.is_empty() {
        return false;
    }
    let Some(state) = state_mut(cx) else {
        return false;
    };
    let Some(target) = state.config.groups.get_mut(group) else {
        return false;
    };
    let mut cursor = start;
    let mut placed = Vec::with_capacity(items.len());
    for mut item in items {
        let slot = next_free_slot_from(target, cursor);
        item.slot = slot;
        target.items.push(item);
        placed.push(slot);
        cursor = slot.saturating_add(1);
    }
    target.items.sort_by_key(|item| item.slot);
    state.touch();
    mark_dirty(cx);
    // R2: a new item is checked at once, on the same path an edit takes, so a
    // path that is already broken is grey before the window is drawn again.
    for slot in placed {
        recheck_health(cx, group, slot);
    }
    true
}

/// The lowest free slot of `group` at or after `start`.
fn next_free_slot_from(group: &Group, start: u32) -> u32 {
    let mut used: Vec<u32> = group
        .items
        .iter()
        .map(|item| item.slot)
        .filter(|slot| *slot >= start)
        .collect();
    used.sort_unstable();
    let mut candidate = start;
    for slot in used {
        if slot == candidate {
            candidate = candidate.saturating_add(1);
        } else if slot > candidate {
            break;
        }
    }
    candidate
}
/// The lowest slot a group does not occupy, or one past its highest slot (the
/// drop target of the group bar, §12.2).
fn first_free_slot(group: &Group) -> u32 {
    next_free_slot_from(group, 0)
}

/// Where an item dropped on the group bar (or anywhere in the content area)
/// goes: the first empty slot, else past the last item (§12.2).
pub(crate) fn next_free_slot(cx: &App, group: usize) -> u32 {
    state(cx)
        .and_then(|state| state.config.groups.get(group))
        .map_or(0, first_free_slot)
}

/// Whether `path` is a folder, read off the foreground (S1).
///
/// A path the disk cannot answer for is treated as a file, which is what the
/// item then is: something the Shell has to be asked to open.
pub(crate) fn path_is_directory(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Moves one item's usage record to the place it went, in one transaction
/// (S3): the old key is removed only when the new one is written.
fn move_usage(cx: &mut App, from: (String, u32), to: (String, u32)) {
    // The in-memory records the use groups are built from follow the item too
    // (§17.2 G.3).
    if let Some(quick_launch) = state_mut(cx)
        && let Some(record) = quick_launch.restored.usage.remove(&from)
    {
        quick_launch.restored.usage.insert(to.clone(), record);
    }
    let Some(kv) = state(cx).and_then(|state| state.kv.clone()) else {
        return;
    };
    cx.background_spawn(async move {
        let (Ok(from_key), Ok(to_key)) = (
            KvKey::new(KV_NAMESPACE, &usage_key_name(&from.0, from.1)),
            KvKey::new(KV_NAMESPACE, &usage_key_name(&to.0, to.1)),
        ) else {
            return;
        };
        let Some(value) = kv.get(&from_key).log_err().flatten() else {
            return;
        };
        kv.batch(vec![
            KvOp::Put { key: to_key, value },
            KvOp::Delete { key: from_key },
        ])
        .log_err();
    })
    .detach();
}

/// Swaps the usage records of two slots of one group, in one transaction (S3).
fn swap_usage(cx: &mut App, group_id: String, first: u32, second: u32) {
    // The in-memory records the use groups are built from swap as well
    // (§17.2 G.3).
    if let Some(quick_launch) = state_mut(cx) {
        let first_key = (group_id.clone(), first);
        let second_key = (group_id.clone(), second);
        let first_record = quick_launch.restored.usage.remove(&first_key);
        let second_record = quick_launch.restored.usage.remove(&second_key);
        if let Some(record) = second_record {
            quick_launch.restored.usage.insert(first_key, record);
        }
        if let Some(record) = first_record {
            quick_launch.restored.usage.insert(second_key, record);
        }
    }
    let Some(kv) = state(cx).and_then(|state| state.kv.clone()) else {
        return;
    };
    cx.background_spawn(async move {
        let (Ok(first_key), Ok(second_key)) = (
            KvKey::new(KV_NAMESPACE, &usage_key_name(&group_id, first)),
            KvKey::new(KV_NAMESPACE, &usage_key_name(&group_id, second)),
        ) else {
            return;
        };
        let first_value = kv.get(&first_key).log_err().flatten();
        let second_value = kv.get(&second_key).log_err().flatten();
        let mut ops = Vec::new();
        match second_value {
            Some(value) => ops.push(KvOp::Put {
                key: first_key.clone(),
                value,
            }),
            None => ops.push(KvOp::Delete {
                key: first_key.clone(),
            }),
        }
        match first_value {
            Some(value) => ops.push(KvOp::Put {
                key: second_key,
                value,
            }),
            None => ops.push(KvOp::Delete { key: second_key }),
        }
        kv.batch(ops).log_err();
    })
    .detach();
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
    // The in-memory records the use groups are built from go with them
    // (§17.2 G.3).
    if let Some(quick_launch) = state_mut(cx) {
        for (group, slot) in &entries {
            quick_launch.restored.usage.remove(&(group.clone(), *slot));
        }
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
    tracing::debug!(
        target: "quick_launch",
        keys = ops.len(),
        "deleting the usage records of removed items"
    );
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
        hotkey_refusal: None,
        loading: false,
        pending_save: false,
        save_due: None,
        _watcher: watcher,
        signals: Some(receiver),
        generation: 0,
        message: None,
        pending_command: None,
        pending_group: None,
        pending_already_running: None,
        pending_batch: None,
        pending_hide: false,
        revision: 0,
        pending_usage: None,
        #[cfg(test)]
        sent_launches: Vec::new(),
        filter: String::new(),
        health: HashMap::new(),
        health_started: false,
        health_queue: VecDeque::new(),
        origins: HashMap::new(),
        folder_missing: HashSet::new(),
        folder_refresh: HashMap::new(),
        mode,
        #[cfg(test)]
        health_checks: Vec::new(),
        expand: ExpandContext::current(),
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
            // The use records the `recent`/`most_used` groups are made of were
            // not in memory before this read (§17.2 G.3).
            refresh_dynamic_groups_for(cx, RefreshReason::Loaded);
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
    // Only a file that parsed describes the items the usage records belong to: a
    // missing file loads a default configuration and a broken one keeps the old
    // one, and pruning against either would delete counts that are still wanted
    // (B1).
    let mut parsed_ok = false;
    match read.parsed {
        None => {
            quick_launch.config = QuickLaunchConfig::with_default_group();
            quick_launch.status = Status::Ready;
            quick_launch.warnings.clear();
            quick_launch.file_hash = read.hash;
        }
        Some(Ok(parsed)) => {
            parsed_ok = true;
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
    // The generated items of the configuration that was just replaced describe
    // entries that may not be in the new one at all (§17.2 G.1).
    quick_launch.origins.clear();
    quick_launch.folder_missing.clear();
    quick_launch.folder_refresh.clear();
    let topmost = quick_launch.config.window.topmost;
    quick_launch.touch();
    // §11.1: a `topmost` changed in the file reaches the window that is already
    // open; the pin button reads the same field.
    sync_topmost(cx, topmost);
    restore_last_group(cx);
    // G.1: every dynamic group is generated again after a successful read.
    refresh_dynamic_groups_for(cx, RefreshReason::Loaded);
    ensure_selection(cx);
    sync_hotkey(cx);
    cx.refresh_windows();
    // E2: records of items the file no longer has go now, so the next occupant of
    // a slot does not inherit them — but only when this read is the file (B1).
    if parsed_ok {
        prune_usage(cx);
    }
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
            quick_launch.hotkey_refusal = None;
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
                quick_launch.hotkey_refusal = None;
            }
            tracing::info!(target: "quick_launch", "listening on {wanted}");
        }
        Err(refusal) => {
            let error = refusal.to_string();
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
                    quick_launch.hotkey_refusal = None;
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
                quick_launch.hotkey_refusal = Some(refusal);
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

/// The configuration as the file holds it (§17.2 G.1): a dynamic group's items
/// are generated at runtime, so the copy that is serialized has none of them.
///
/// `to_toml` clears them again (§17.2 A.2); the rule is applied here as well so
/// that nothing this module serializes depends on that second line of defence.
fn config_for_saving(quick_launch: &QuickLaunch) -> QuickLaunchConfig {
    let mut config = quick_launch.config.clone();
    for group in &mut config.groups {
        if group_source(group).is_some_and(|source| source.is_dynamic()) {
            group.items.clear();
        }
    }
    config
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
    let text = match nohrs_core::config::quick_launch::to_toml(&config_for_saving(quick_launch)) {
        Ok(text) => text,
        Err(error) => {
            tracing::error!(target: "quick_launch", "could not write the configuration: {error}");
            return;
        }
    };
    let path = paths::quick_launch_file();
    cx.spawn(async move |cx| {
        let written = cx
            .background_spawn(async move { nohrs_services::fs::write_atomically(&path, &text) })
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
    let text = match nohrs_core::config::quick_launch::to_toml(&config_for_saving(quick_launch)) {
        Ok(text) => text,
        Err(error) => {
            tracing::error!(target: "quick_launch", "could not write the configuration: {error}");
            return;
        }
    };
    let path = paths::quick_launch_file();
    match nohrs_services::fs::write_atomically(&path, &text) {
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
/// The count is kept in memory as well as in the store: the `recent` and
/// `most_used` groups are built from the copy in memory, including the use that
/// just happened (§17.2 G.3).
fn record_usage(cx: &mut App, group: &str, slot: u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let usage = quick_launch
        .restored
        .usage
        .entry((group.to_string(), slot))
        .or_default();
    usage.count = usage.count.saturating_add(1);
    usage.last = now;
    let usage = *usage;
    let Some(kv) = quick_launch.kv.clone() else {
        return;
    };
    let name = usage_key_name(group, slot);
    cx.background_spawn(async move {
        let Ok(key) = KvKey::new(KV_NAMESPACE, &name) else {
            return;
        };
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
            // G.2, G.3: a folder group is read again at most every 30 s, and
            // the `recent`/`most_used` groups follow the use records.
            refresh_dynamic_groups_for(cx, RefreshReason::Shown);
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
    // G.2, G.3: the window opens on freshly generated dynamic groups.
    refresh_dynamic_groups_for(cx, RefreshReason::Shown);
    ensure_selection(cx);
    // §12.1: the items are checked once the window has been shown, a batch at a
    // time, on the icon thread.
    start_health_check(cx);
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
pub(crate) fn set_topmost(window: &Window, topmost: bool) {
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
            batch,
            name,
            outcome,
        } => {
            let Some(quick_launch) = state(cx) else {
                return;
            };
            if generation != quick_launch.generation {
                return;
            }
            if let Some(batch_id) = batch {
                apply_batch_reply(cx, batch_id, &name, outcome);
                return;
            }
            let close_after_launch = quick_launch.config.window.close_after_launch;
            let visible = quick_launch.visible;
            let started = counts_as_usage(&outcome);
            let folder = match outcome {
                Ok(LaunchOutcome::AlreadyRunning { process_id }) => {
                    // §17.2 F.2: ask before switching; nothing is counted yet.
                    ask_about_running_program(cx, process_id, visible);
                    return;
                }
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
                // G.3: the groups made of use records follow the launch that
                // just happened.
                refresh_dynamic_groups_for(cx, RefreshReason::Launched);
            }
            if let Some(path) = folder {
                open_in_explorer(cx, path);
            }
            if hides_after_action(close_after_launch, is_locked(cx)) {
                hide(cx);
            }
        }
    }
}

/// §17.2 F.2: a program is already running and the configuration asks first.
///
/// The dialog belongs to the quick-launch window, so a launch from the
/// Launcher's `@` shows the window before it appears. Nothing is recorded: the
/// use counts only after the user chooses, and a cancel records nothing.
fn ask_about_running_program(cx: &mut App, process_id: u32, visible: bool) {
    let Some((group, slot)) = state(cx).and_then(|quick_launch| quick_launch.pending_usage.clone())
    else {
        tracing::debug!(
            target: "quick_launch",
            process = process_id,
            "a running program answered without a pending item"
        );
        return;
    };
    let item = state(cx).and_then(|quick_launch| {
        quick_launch
            .config
            .groups
            .iter()
            .find(|candidate| candidate.id == group)
            .and_then(|candidate| candidate.items.iter().find(|item| item.slot == slot))
            .cloned()
    });
    let Some(item) = item else {
        tracing::debug!(
            target: "quick_launch",
            process = process_id,
            "the item behind the running program is gone"
        );
        return;
    };
    if !visible && let Err(error) = show(cx) {
        tracing::warn!(target: "quick_launch", "could not show the window: {error}");
    }
    if let Some(quick_launch) = state_mut(cx) {
        quick_launch.message = None;
        quick_launch.pending_already_running = Some(PendingAlreadyRunning {
            group,
            slot,
            item,
            process_id,
        });
        quick_launch.touch();
    }
    cx.refresh_windows();
}

/// "Switch to it": sends the launch again, naming the process to activate
/// (§17.2 F.2).
pub(crate) fn switch_to_running_program(cx: &mut App) {
    let Some(pending) = take_pending_already_running(cx) else {
        return;
    };
    send_launch(
        cx,
        pending.item,
        pending.group,
        pending.slot,
        LaunchMode::Activate(pending.process_id),
    );
}

/// "Start another": sends the launch again, forcing a new instance (§17.2 F.2).
pub(crate) fn start_another_instance(cx: &mut App) {
    let Some(pending) = take_pending_already_running(cx) else {
        return;
    };
    send_launch(
        cx,
        pending.item,
        pending.group,
        pending.slot,
        LaunchMode::ForceNew,
    );
}

/// "Cancel": starts nothing and records no use (§17.2 F.2).
pub(crate) fn cancel_already_running(cx: &mut App) {
    if take_pending_already_running(cx).is_none() {
        return;
    }
    if let Some(quick_launch) = state_mut(cx) {
        // The launch never happened, so it must not be counted afterwards.
        quick_launch.pending_usage = None;
        quick_launch.touch();
    }
    cx.refresh_windows();
}

/// Takes the pending running-program dialog, if there is one, and tells the
/// window it is gone.
fn take_pending_already_running(cx: &mut App) -> Option<PendingAlreadyRunning> {
    let pending = {
        let quick_launch = state_mut(cx)?;
        let pending = quick_launch.pending_already_running.take()?;
        quick_launch.touch();
        pending
    };
    cx.refresh_windows();
    Some(pending)
}

/// Counts one group entry's answer, and finishes the batch when it was the last
/// (§17.2 E.1).
///
/// Nothing is recorded per entry: the group item is the one that was used, and
/// the footer, the folders and the hide wait for the whole batch (§17.2 E.1).
fn apply_batch_reply(
    cx: &mut App,
    batch_id: u64,
    name: &str,
    outcome: std::result::Result<LaunchOutcome, LaunchError>,
) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let Some(batch) = quick_launch.pending_batch.as_mut() else {
        return;
    };
    if batch.generation != batch_id {
        return;
    }
    match &outcome {
        Ok(LaunchOutcome::Cancelled) => {}
        // D9: a group starts `ask` entries as `activate`, so a group entry has
        // no dialog to open; nothing is counted for it.
        Ok(LaunchOutcome::AlreadyRunning { .. }) => {
            tracing::debug!(
                target: "quick_launch",
                "a group entry answered with a running process"
            );
        }
        Ok(LaunchOutcome::OpenFolder(path)) => {
            batch.used = true;
            batch.folders.push(path.clone());
        }
        Ok(LaunchOutcome::Started | LaunchOutcome::Activated) => batch.used = true,
        Err(error) => {
            tracing::warn!(
                target: "quick_launch",
                "could not launch {name:?} from a group: {error}"
            );
            batch.failed.push(name.to_string());
        }
    }
    batch.remaining = batch.remaining.saturating_sub(1);
    if batch.remaining > 0 {
        return;
    }
    let Some(batch) = state_mut(cx).and_then(|quick_launch| quick_launch.pending_batch.take())
    else {
        return;
    };
    finish_batch(cx, batch);
}

/// What happens once every entry of a group has answered (§17.2 E.1): one
/// footer line when anything failed, the folders one by one, one usage record on
/// the group item, and one decision about hiding the window.
fn finish_batch(cx: &mut App, batch: PendingLaunchBatch) {
    if batch.used {
        record_usage(cx, &batch.usage.0, batch.usage.1);
        // G.3: the groups made of use records follow the launch that just
        // happened.
        refresh_dynamic_groups_for(cx, RefreshReason::Launched);
    }
    for folder in batch.folders {
        open_in_explorer(cx, folder);
    }
    let message = if !batch.failed.is_empty() {
        Some(nohrs_core::tf!(
            "{failed} de {total} itens não iniciaram: {names}",
            failed = batch.failed.len(),
            total = batch.total,
            names = batch.failed.join(", ")
        ))
    } else if batch.excess > 0 {
        Some(nohrs_core::tf!(
            "{skipped} itens do grupo excedem o limite de {limit} e não serão iniciados.",
            skipped = batch.excess,
            limit = MAX_GROUP_REFERENCES
        ))
    } else {
        None
    };
    let close_after_launch =
        state(cx).is_some_and(|quick_launch| quick_launch.config.window.close_after_launch);
    if let Some(quick_launch) = state_mut(cx) {
        quick_launch.message = message;
    }
    // The window is hidden once, after the whole batch (§17.2 E.1), and only
    // when something actually started — the same rule as one item (§8.5).
    if batch.used && hides_after_action(close_after_launch, is_locked(cx)) {
        hide(cx);
    }
    cx.refresh_windows();
}

/// Whether what just happened hides the window (§11.1): a pinned window stays
/// for the launch and the reveal, and only `Esc` and the chord hide it.
fn hides_after_action(close_after_launch: bool, locked: bool) -> bool {
    close_after_launch && !locked
}

/// Whether one outcome counts as a use (§6): starting, activating and opening a
/// folder all do; a decision or a failure does not.
fn counts_as_usage(outcome: &std::result::Result<LaunchOutcome, LaunchError>) -> bool {
    matches!(
        outcome,
        Ok(LaunchOutcome::Started | LaunchOutcome::Activated | LaunchOutcome::OpenFolder(_))
    )
}

/// Shows a path in LumaFiles: a folder to open, or a file to select in its
/// folder (§12.2, B2).
///
/// One function for the folder items and for `Shift`+click, because both have
/// the same two cases: the Explorer is in this process but its window may be
/// hidden in the tray, or there is no Explorer here at all (§15.1).
pub(crate) fn open_in_explorer(cx: &mut App, path: PathBuf) {
    match state(cx).map_or(Mode::Main, |quick_launch| quick_launch.mode) {
        Mode::Main => open_in_main_window(path),
        Mode::Standalone => open_without_explorer(cx, path),
    }
}

/// The Explorer is here: show its window first, then hand it the path.
fn open_in_main_window(path: PathBuf) {
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

/// No Explorer here (§15.1): the main program takes it through the IPC,
/// and when it is not running a copy of this executable is started with the
/// path, which opens or selects it the way the shell does.
///
/// The spawn happens on a background task: a process call on the foreground is
/// what §18 forbids.
fn open_without_explorer(cx: &mut App, path: PathBuf) {
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
///
/// A `command` whose trust key is not in the restored set stops here: the window
/// shows its confirmation dialog instead, and only that dialog's "Run" calls
/// [`confirm_pending_command`] (§17.2 D.3).
///
/// A `group` is expanded and started here instead of being sent to the registry
/// (§17.2 E.1).
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
    if item.action_kind() == ActionKind::Group {
        let group_items = quick_launch
            .config
            .groups
            .iter()
            .find(|candidate| candidate.id == group_id)
            .map(|candidate| candidate.items.clone())
            .unwrap_or_default();
        let visible = quick_launch.visible;
        activate_group(cx, &group_id, &group_items, slot, &item, kind, visible);
        return;
    }
    let mode = launch_mode(&item, kind);
    let confirmation = command_needs_confirmation(quick_launch, &item);
    let visible = quick_launch.visible;
    if let Some((spec, trust_key)) = confirmation {
        // The dialog lives in the quick-launch window, so a summon from the
        // Launcher's `@` shows it first (§17.2 D.4).
        if !visible && let Err(error) = show(cx) {
            tracing::warn!(target: "quick_launch", "could not show the window: {error}");
        }
        if let Some(quick_launch) = state_mut(cx) {
            quick_launch.message = None;
            quick_launch.pending_command = Some(PendingCommand {
                group: group_id,
                slot,
                item,
                spec,
                trust_key,
                mode,
            });
            quick_launch.touch();
        }
        cx.refresh_windows();
        return;
    }
    send_launch(cx, item, group_id, slot, mode);
}

/// Expands and starts one `group` item (§17.2 E.1).
///
/// The whole group is refused when one of its commands still needs its first-run
/// confirmation and the user cancels: the commands are collected into one
/// overlay rather than one per command, and nothing is sent before it answers.
fn activate_group(
    cx: &mut App,
    group_id: &str,
    group_items: &[Item],
    slot: u32,
    group_item: &Item,
    kind: ClickKind,
    visible: bool,
) {
    let plan = expand_group(group_items, group_item);
    if plan.items.is_empty() {
        if let Some(quick_launch) = state_mut(cx) {
            quick_launch.message =
                Some(nohrs_core::t!("Este grupo não tem itens válidos.").to_string());
            quick_launch.touch();
        }
        cx.refresh_windows();
        return;
    }
    let commands: Vec<PendingGroupCommand> = {
        let Some(quick_launch) = state(cx) else {
            return;
        };
        plan.items
            .iter()
            .filter_map(|child| {
                command_needs_confirmation(quick_launch, child).map(|(spec, trust_key)| {
                    PendingGroupCommand {
                        item: child.clone(),
                        spec,
                        trust_key,
                    }
                })
            })
            .collect()
    };
    if commands.is_empty() {
        send_group_launch(cx, group_id.to_string(), slot, plan, kind);
        return;
    }
    // The dialog lives in the quick-launch window, so a summon from the
    // Launcher's `@` shows it first (§17.2 D.4).
    if !visible && let Err(error) = show(cx) {
        tracing::warn!(target: "quick_launch", "could not show the window: {error}");
    }
    if let Some(quick_launch) = state_mut(cx) {
        quick_launch.message = None;
        quick_launch.pending_group = Some(PendingGroup {
            group: group_id.to_string(),
            slot,
            name: group_item.name.clone(),
            plan,
            commands,
            kind,
        });
        quick_launch.touch();
    }
    cx.refresh_windows();
}

/// Sends one start request per entry of a group, all under one generation and
/// batch id (§17.2 E.1).
fn send_group_launch(cx: &mut App, group: String, slot: u32, plan: GroupPlan, kind: ClickKind) {
    // G.4: a use of a copy in a `recent`/`most_used` group belongs to the entry
    // it was copied from.
    let usage = usage_target(cx, &group, slot);
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let generation = quick_launch.generation.wrapping_add(1);
    quick_launch.generation = generation;
    // D9: `ask` is not asked per entry inside a group; it behaves as `activate`.
    let on_running = group_on_running(quick_launch.config.window.on_running);
    let total = plan.items.len();
    quick_launch.message = None;
    quick_launch.pending_usage = None;
    quick_launch.pending_batch = Some(PendingLaunchBatch {
        generation,
        usage,
        total,
        remaining: total,
        failed: Vec::new(),
        folders: Vec::new(),
        used: false,
        excess: plan.excess,
    });
    if let Some(workers) = quick_launch.workers.as_ref() {
        for item in plan.items {
            let mode = launch_mode(&item, kind);
            workers.send_launch(StartRequest {
                item: Box::new(item),
                mode,
                on_running,
                generation,
                batch: Some(generation),
            });
        }
    }
    cx.refresh_windows();
}

/// The launch mode one click asks for (§8.1): `Ctrl` or an item marked `admin`.
fn launch_mode(item: &Item, kind: ClickKind) -> LaunchMode {
    if kind == ClickKind::Admin || item.admin {
        LaunchMode::Admin
    } else {
        LaunchMode::Normal
    }
}

/// D9 (§17.2 F.3): a group does not ask per entry, so `ask` behaves as
/// `activate` inside one.
fn group_on_running(on_running: OnRunning) -> OnRunning {
    match on_running {
        OnRunning::Ask => OnRunning::Activate,
        other => other,
    }
}

/// Hands one start request to the `quick-launch` thread (§8.1, §8.4).
fn send_launch(cx: &mut App, item: Item, group: String, slot: u32, mode: LaunchMode) {
    // G.4: a use of a copy in a `recent`/`most_used` group belongs to the entry
    // it was copied from.
    let usage = usage_target(cx, &group, slot);
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let on_running = quick_launch.config.window.on_running;
    let generation = quick_launch.generation.wrapping_add(1);
    quick_launch.generation = generation;
    #[cfg(test)]
    quick_launch.sent_launches.push(mode);
    quick_launch.message = None;
    quick_launch.pending_usage = Some(usage);
    // A single launch replaces whatever group was in flight: its replies are
    // already stale, because the generation moved on.
    quick_launch.pending_batch = None;
    if let Some(workers) = quick_launch.workers.as_ref() {
        workers.send_launch(StartRequest {
            item: Box::new(item),
            mode,
            on_running,
            generation,
            batch: None,
        });
    }
    cx.refresh_windows();
}

/// Whether a command dialog is waiting for an answer (§17.2 D.3), or a group
/// dialog is (§17.2 E.1), or the running-program dialog is (§17.2 F.2).
pub(crate) fn command_waiting(cx: &App) -> bool {
    state(cx).is_some_and(|quick_launch| {
        quick_launch.pending_command.is_some()
            || quick_launch.pending_group.is_some()
            || quick_launch.pending_already_running.is_some()
    })
}

/// What the window's command dialog shows, if one is waiting (§17.2 D.3).
pub(crate) fn command_confirmation(cx: &App) -> Option<CommandConfirmation> {
    let quick_launch = state(cx)?;
    let pending = quick_launch.pending_command.as_ref()?;
    Some(CommandConfirmation {
        name: pending.item.name.clone(),
        shell: pending.spec.shell.as_str(),
        cwd: pending.item.cwd.clone(),
        hidden: pending.spec.hidden,
        script: pending.spec.script.clone(),
        remembered: quick_launch.kv.is_some(),
    })
}

/// What the window's running-program dialog shows, if one is waiting (§17.2 F.2).
pub(crate) fn already_running_prompt(cx: &App) -> Option<AlreadyRunningPrompt> {
    let quick_launch = state(cx)?;
    let pending = quick_launch.pending_already_running.as_ref()?;
    Some(AlreadyRunningPrompt {
        name: pending.item.name.clone(),
    })
}

/// What the window's group dialog shows, if one is waiting (§17.2 E.1).
pub(crate) fn group_confirmation(cx: &App) -> Option<GroupConfirmation> {
    let quick_launch = state(cx)?;
    let pending = quick_launch.pending_group.as_ref()?;
    Some(GroupConfirmation {
        name: pending.name.clone(),
        commands: pending
            .commands
            .iter()
            .map(|command| GroupCommandConfirmation {
                name: command.item.name.clone(),
                shell: command.spec.shell.as_str(),
                cwd: command.item.cwd.clone(),
                hidden: command.spec.hidden,
                script: command.spec.script.clone(),
            })
            .collect(),
        remembered: quick_launch.kv.is_some(),
    })
}

/// "Run": writes the trust key, then starts the pending command (§17.2 D.3).
///
/// The key reaches the store before the start request, so a launch that works
/// once does not ask again even if the process ends right after it.
pub(crate) fn confirm_pending_command(cx: &mut App) {
    let Some(pending) = state_mut(cx).and_then(|quick_launch| quick_launch.pending_command.take())
    else {
        return;
    };
    let kv = {
        let Some(quick_launch) = state_mut(cx) else {
            return;
        };
        remember_trust_in_memory(quick_launch, &pending.trust_key);
        quick_launch.touch();
        quick_launch.kv.clone()
    };
    cx.refresh_windows();
    let PendingCommand {
        group,
        slot,
        item,
        trust_key,
        mode,
        ..
    } = pending;
    cx.spawn(async move |cx| {
        if let Some(kv) = kv {
            let key_name = format!("{TRUSTED_PREFIX}{trust_key}");
            let written = cx
                .background_spawn(async move {
                    let Ok(key) = KvKey::new(KV_NAMESPACE, &key_name) else {
                        return false;
                    };
                    kv.put(&key, b"true").is_ok()
                })
                .await;
            if !written {
                tracing::warn!(
                    target: "quick_launch",
                    "could not remember a command's trust in the store"
                );
            }
        }
        cx.update(|cx| send_launch(cx, item, group, slot, mode))
            .ok();
    })
    .detach();
}

/// "Cancel": forget the pending command, start nothing, report nothing
/// (§17.2 D.3). A pending group is the same decision for its whole batch
/// (§17.2 E.1), and so is the running-program dialog (§17.2 F.2).
pub(crate) fn cancel_pending_command(cx: &mut App) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let cancelled = quick_launch.pending_command.take().is_some();
    let cancelled_group = quick_launch.pending_group.take().is_some();
    let cancelled_running = quick_launch.pending_already_running.take().is_some();
    if !cancelled && !cancelled_group && !cancelled_running {
        return;
    }
    if cancelled_running {
        // A cancelled launch never happened, so it must not be counted.
        quick_launch.pending_usage = None;
    }
    quick_launch.touch();
    cx.refresh_windows();
}

/// "Run" for a group (§17.2 E.1): writes every trust key, then sends the whole
/// batch. Cancelling instead drops the pending group untouched, so the whole
/// group is refused.
pub(crate) fn confirm_pending_group(cx: &mut App) {
    let Some(pending) = state_mut(cx).and_then(|quick_launch| quick_launch.pending_group.take())
    else {
        return;
    };
    let kv = {
        let Some(quick_launch) = state_mut(cx) else {
            return;
        };
        for command in &pending.commands {
            remember_trust_in_memory(quick_launch, &command.trust_key);
        }
        quick_launch.touch();
        quick_launch.kv.clone()
    };
    cx.refresh_windows();
    let PendingGroup {
        group,
        slot,
        plan,
        commands,
        kind,
        ..
    } = pending;
    let key_names: Vec<String> = commands
        .iter()
        .map(|command| format!("{TRUSTED_PREFIX}{}", command.trust_key))
        .collect();
    cx.spawn(async move |cx| {
        if let Some(kv) = kv {
            let written = cx
                .background_spawn(async move {
                    let mut written = true;
                    for key_name in key_names {
                        let Ok(key) = KvKey::new(KV_NAMESPACE, &key_name) else {
                            written = false;
                            continue;
                        };
                        if kv.put(&key, b"true").is_err() {
                            written = false;
                        }
                    }
                    written
                })
                .await;
            if !written {
                tracing::warn!(
                    target: "quick_launch",
                    "could not remember the group's commands' trust in the store"
                );
            }
        }
        cx.update(|cx| send_group_launch(cx, group, slot, plan, kind))
            .ok();
    })
    .detach();
}

/// Adds one trust key to the in-memory set and, when there is a store, writes it
/// down (§17.2 D.1, D.5).
fn remember_trust(cx: &mut App, trust_key: String) {
    let Some(kv) = state_mut(cx).and_then(|quick_launch| {
        remember_trust_in_memory(quick_launch, &trust_key);
        quick_launch.kv.clone()
    }) else {
        // A resident process has no store: the set above is the whole record.
        return;
    };
    let key_name = format!("{TRUSTED_PREFIX}{trust_key}");
    cx.background_spawn(async move {
        let Ok(key) = KvKey::new(KV_NAMESPACE, &key_name) else {
            return;
        };
        kv.put(&key, b"true").log_err();
    })
    .detach();
}

/// Adds one trust key to the set the foreground consults, without touching the
/// store: the caller decides whether the key is written.
fn remember_trust_in_memory(quick_launch: &mut QuickLaunch, trust_key: &str) {
    quick_launch.restored.trusted.insert(trust_key.to_string());
}

/// Whether the command at `group`/`slot` is trusted, as the settings form shows
/// it (§17.2 D.7). `None` when the item is gone or its `extra` is invalid.
pub(crate) fn command_trust(cx: &App, group: &str, slot: u32) -> Option<bool> {
    let quick_launch = state(cx)?;
    let item = quick_launch
        .config
        .groups
        .iter()
        .find(|candidate| candidate.id == group)?
        .items
        .iter()
        .find(|item| item.slot == slot)?;
    let spec = command_spec(item).ok()?;
    let trust_key = command_trust_key(item, &spec);
    Some(quick_launch.restored.trusted.contains(&trust_key))
}

/// Removes the trust record of the command at `group`/`slot` (§17.2 D.7).
///
/// The foreground set is updated at once; the store is written on a background
/// task, like every other KV write of this module.
pub(crate) fn revoke_command_trust(cx: &mut App, group: &str, slot: u32) -> bool {
    let Some(item) = state(cx)
        .and_then(|quick_launch| {
            quick_launch
                .config
                .groups
                .iter()
                .find(|candidate| candidate.id == group)
        })
        .and_then(|candidate| candidate.items.iter().find(|item| item.slot == slot))
        .cloned()
    else {
        return false;
    };
    let Ok(spec) = command_spec(&item) else {
        return false;
    };
    let trust_key = command_trust_key(&item, &spec);
    let kv = {
        let Some(quick_launch) = state_mut(cx) else {
            return false;
        };
        quick_launch.restored.trusted.remove(&trust_key);
        quick_launch.kv.clone()
    };
    if let Some(kv) = kv {
        let key_name = format!("{TRUSTED_PREFIX}{trust_key}");
        cx.background_spawn(async move {
            let Ok(key) = KvKey::new(KV_NAMESPACE, &key_name) else {
                return;
            };
            kv.delete(&key).log_err();
        })
        .detach();
    }
    true
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
    // G.2: a `folder:` group reads its directory again when it is switched to,
    // if the last read is older than 30 s.
    refresh_dynamic_groups_for(cx, RefreshReason::GroupSwitch(index));
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
pub(crate) fn reveal_item(cx: &mut App, slot: u32) -> bool {
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
    open_in_explorer(cx, path);
    true
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

/// One quick-launch entry as the launcher's `@` search needs it (M3 D).
///
/// A plain data copy: the launcher crate cannot name this module's types, so the
/// binary maps these into its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherEntry {
    /// The group id activation needs.
    pub group: String,
    /// The item's slot in that group.
    pub slot: u32,
    /// What the row shows.
    pub name: String,
    /// Words the query also matches.
    pub tags: Vec<String>,
    /// The item's `type`.
    pub kind: String,
    /// The item's path as written.
    pub path: String,
}

/// Every entry, cloned from memory for the launcher (M3 D).
///
/// Runs on the foreground and reads nothing but the state, which is what makes it
/// safe from a keystroke handler: the matching itself happens on the background.
pub fn launcher_snapshot(cx: &App) -> Vec<LauncherEntry> {
    let Some(quick_launch) = state(cx) else {
        return Vec::new();
    };
    quick_launch
        .config
        .groups
        .iter()
        // G.7: the copies a `recent`/`most_used` group shows are already in the
        // snapshot through their own group; listing them again would duplicate
        // every row.
        .filter(|group| {
            !matches!(
                group_source(group),
                Some(GroupSource::Recent | GroupSource::MostUsed)
            )
        })
        .flat_map(|group| {
            group.items.iter().map(move |item| LauncherEntry {
                group: group.id.clone(),
                slot: item.slot,
                name: item.name.clone(),
                tags: item.tags.clone(),
                kind: item.kind.clone(),
                path: item.path.clone().unwrap_or_default(),
            })
        })
        .collect()
}

/// Starts the entry at `group`/`slot`, or shows its place when `reveal` (M3 D).
///
/// The launcher names an entry by its group id rather than its position, because
/// the file may have been reordered since the rows were built; a group that is gone
/// does nothing at all.
pub fn activate_by_id(cx: &mut App, group: &str, slot: u32, reveal: bool) {
    let Some(quick_launch) = state_mut(cx) else {
        return;
    };
    let Some(index) = quick_launch
        .config
        .groups
        .iter()
        .position(|candidate| candidate.id == group)
    else {
        return;
    };
    quick_launch.group = index;
    quick_launch.slot = Some(slot);
    quick_launch.touch();
    if reveal {
        reveal_item(cx, slot);
    } else {
        activate_with(cx, slot, ClickKind::Plain);
    }
    cx.refresh_windows();
}

/// One item's path with the variables of §7 resolved, using the context built at init (S2).
fn expanded_path(cx: &App, item: &Item) -> Option<PathBuf> {
    let state = state(cx)?;
    let raw = item.path.as_deref()?.trim();
    nohrs_services::launch::expand(raw, &state.expand).ok()
}

/// The message the footer shows, if the last launch failed.
fn message(cx: &App) -> Option<&str> {
    state(cx).and_then(|quick_launch| quick_launch.message.as_deref())
}

/// The read-only banner's text, if the file must not be written back (§5.2).
pub(crate) fn read_only_reason(cx: &App) -> Option<&str> {
    state(cx).and_then(|quick_launch| quick_launch.status.error())
}

/// The window settings as the settings page shows them (§13); the defaults
/// before the first read has landed.
pub(crate) fn window_settings(cx: &App) -> WindowSettings {
    state(cx)
        .map(|quick_launch| quick_launch.config.window.clone())
        .unwrap_or_default()
}

/// A copy of the groups, for the settings page (§13).
///
/// The page and the window read the same state; the copy only lasts for the
/// frame being built.
pub(crate) fn groups(cx: &App) -> Vec<Group> {
    state(cx)
        .map(|quick_launch| quick_launch.config.groups.clone())
        .unwrap_or_default()
}

/// The chord the OS is actually holding, if one is registered (§10).
pub(crate) fn registered_hotkey(cx: &App) -> Option<&str> {
    let quick_launch = state(cx)?;
    quick_launch.hotkey?;
    quick_launch.hotkey_text.as_deref()
}

/// Why the last registration failed, if it did (§10).
pub(crate) fn hotkey_error(cx: &App) -> Option<&str> {
    state(cx).and_then(|quick_launch| quick_launch.hotkey_error.as_deref())
}

/// Whether the last registration failed because the chord belongs to the
/// Launcher (§10, M3 S3).
///
/// The binary refuses that chord before the OS is asked and says so with the same
/// enum; the settings page shows the user's own words for it, so it asks for the
/// kind rather than reading the message.
pub(crate) fn hotkey_conflicts_with_launcher(cx: &App) -> bool {
    state(cx).is_some_and(|quick_launch| {
        quick_launch.hotkey_refusal == Some(HotkeyRefusal::LauncherConflict)
    })
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
///
/// `GetCursorPos` and `MonitorFromPoint` are the only way to learn where the
/// pointer is while no LumaFiles window has the foreground.
#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {

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
            source: None,
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
                cache.get(&key);
            }
            grid_cells_for_timing(&items);
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

    /// §9, E1: the same program at two icon indexes gets two cache files.
    #[test]
    fn an_indexed_icon_has_its_own_cache_file() {
        let program = Path::new("C:/Windows/System32/shell32.dll");
        let plain = icon_cache_file(program, 48, None);
        let third = icon_cache_file(program, 48, Some(3));
        let fourth = icon_cache_file(program, 48, Some(4));
        assert_ne!(third, fourth, "each index is its own picture");
        assert_ne!(plain, third, "no index is not the same as an index");
        // The size still separates the files, and the same question answers the
        // same file: the lookup has to be stable for the cache to work.
        assert_eq!(third, icon_cache_file(program, 48, Some(3)));
        assert_ne!(third, icon_cache_file(program, 32, Some(3)));
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
        let config = QuickLaunchConfig {
            groups: vec![
                Group {
                    id: "group_1".to_string(),
                    name: "Um".to_string(),
                    items: Vec::new(),
                    source: None,
                },
                Group {
                    id: "group_3".to_string(),
                    name: "Tres".to_string(),
                    items: Vec::new(),
                    source: None,
                },
            ],
            ..QuickLaunchConfig::default()
        };
        assert_eq!(next_group_id(&config), "group_2");
    }

    /// §12.2, B1: a drop walks over taken slots instead of replacing what is there.
    #[gpui::test]
    fn placing_items_never_overwrites_a_slot(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.id = "general".to_string();
            group.items = vec![Item::new(3, "app", "Tres"), Item::new(4, "app", "Quatro")];
            install(cx, vec![group]);
            let placed = vec![
                Item::new(0, "file", "A"),
                Item::new(0, "file", "B"),
                Item::new(0, "file", "C"),
            ];
            assert!(place_items(cx, 0, 2, placed));
            let slots: Vec<u32> = state(cx)
                .map(|state| {
                    state.config.groups[0]
                        .items
                        .iter()
                        .map(|item| item.slot)
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(slots, vec![2, 3, 4, 5, 6]);
            let name = |cx: &gpui::App, slot: u32| item_at(cx, 0, slot).map(|item| item.name);
            assert_eq!(name(cx, 2), Some("A".to_string()));
            assert_eq!(name(cx, 3), Some("Tres".to_string()), "the old item stays");
            assert_eq!(
                name(cx, 4),
                Some("Quatro".to_string()),
                "the old item stays"
            );
            assert_eq!(name(cx, 5), Some("B".to_string()));
            assert_eq!(name(cx, 6), Some("C".to_string()));
            // A single drop onto a taken cell goes after it.
            assert!(place_items(cx, 0, 3, vec![Item::new(0, "file", "D")]));
            assert_eq!(
                item_at(cx, 0, 7).map(|item| item.name),
                Some("D".to_string())
            );
            assert_eq!(name(cx, 3), Some("Tres".to_string()));
        });
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

    /// A store that keeps its keys in memory, for the records S3 migrates.
    #[derive(Clone, Default)]
    struct TestStore(std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>>);

    impl KvStore for TestStore {
        fn get(&self, key: &KvKey) -> nohrs_store::Result<Option<Vec<u8>>> {
            Ok(self
                .0
                .lock()
                .ok()
                .and_then(|map| map.get(key.as_str()).cloned()))
        }

        fn put(&self, key: &KvKey, value: &[u8]) -> nohrs_store::Result<()> {
            if let Ok(mut map) = self.0.lock() {
                map.insert(key.as_str().to_string(), value.to_vec());
            }
            Ok(())
        }

        fn delete(&self, key: &KvKey) -> nohrs_store::Result<()> {
            if let Ok(mut map) = self.0.lock() {
                map.remove(key.as_str());
            }
            Ok(())
        }

        fn list_namespace(&self, namespace: &str) -> nohrs_store::Result<Vec<(KvKey, Vec<u8>)>> {
            let prefix = format!("{namespace}.");
            let mut entries = Vec::new();
            if let Ok(map) = self.0.lock() {
                for (key, value) in map.iter() {
                    let Some(name) = key.strip_prefix(&prefix) else {
                        continue;
                    };
                    if let Ok(key) = KvKey::new(namespace, name) {
                        entries.push((key, value.clone()));
                    }
                }
            }
            Ok(entries)
        }

        fn batch(&self, ops: Vec<KvOp>) -> nohrs_store::Result<()> {
            if let Ok(mut map) = self.0.lock() {
                for op in ops {
                    match op {
                        KvOp::Put { key, value } => {
                            map.insert(key.as_str().to_string(), value);
                        }
                        KvOp::Delete { key } => {
                            map.remove(key.as_str());
                        }
                    }
                }
            }
            Ok(())
        }
    }

    /// §12.1, R2: every item a drop or the picker places asks for a check of its own.
    #[gpui::test]
    fn a_placed_item_is_checked_at_once(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.id = "general".to_string();
            group.items = vec![Item::new(0, "app", "A")];
            install(cx, vec![group]);
            let placed = vec![Item::new(0, "file", "B"), Item::new(0, "file", "C")];
            assert!(place_items(cx, 0, 1, placed));
            let checks = state(cx)
                .map(|state| state.health_checks.clone())
                .unwrap_or_default();
            assert_eq!(
                checks,
                vec![("general".to_string(), 1), ("general".to_string(), 2)],
                "one check per placed item, on the slots it landed in"
            );
            // Nothing is placed when the file is read-only, and no check is asked.
            if let Some(state) = state_mut(cx) {
                state.status = Status::ReadOnly("schema_version 99".to_string());
                state.health_checks.clear();
            }
            assert!(!place_items(cx, 0, 4, vec![Item::new(0, "file", "D")]));
            assert!(
                state(cx).is_some_and(|state| state.health_checks.is_empty()),
                "a refused place asks for nothing"
            );
        });
    }

    /// §12.2, R1: moving an item takes its own health and usage with it, and leaves
    /// every other slot's records alone — including the one it lands on in the
    /// destination group.
    #[gpui::test]
    fn a_move_takes_its_own_records_not_the_destination_slot_s(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            let mut source = group_of(0);
            source.id = "a".to_string();
            source.items = vec![Item::new(0, "app", "X"), Item::new(5, "app", "Y")];
            let mut target = group_of(0);
            target.id = "b".to_string();
            install(cx, vec![source, target]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            apply_health(cx, "a".to_string(), 0, Health::Missing);
            apply_health(cx, "a".to_string(), 5, Health::Ok);
            let x = KvKey::new(KV_NAMESPACE, "usage_a_0").expect("valid key");
            let y = KvKey::new(KV_NAMESPACE, "usage_a_5").expect("valid key");
            store.put(&x, b"x").expect("the test store");
            store.put(&y, b"y").expect("the test store");
            // Y (slot 5) moves to the empty group B, whose free slot 0 is also
            // where X's records would be found by a shadowed source slot.
            assert!(move_item_to_group(cx, 0, 5, 1));
        });
        cx.run_until_parked();
        // X keeps both records.
        let health = cx.update(|cx| (health_of(cx, 0, 0), health_of(cx, 0, 5)));
        assert_eq!(health.0, Some(Health::Missing), "X keeps its health answer");
        assert_eq!(health.1, None, "nothing is left at the old slot");
        let moved = cx.update(|cx| health_of(cx, 1, 0));
        assert_eq!(moved, Some(Health::Ok), "Y took its own answer along");
        let x = KvKey::new(KV_NAMESPACE, "usage_a_0").expect("valid key");
        assert_eq!(store.get(&x).expect("the test store"), Some(b"x".to_vec()));
        let y = KvKey::new(KV_NAMESPACE, "usage_a_5").expect("valid key");
        assert_eq!(store.get(&y).expect("the test store"), None);
        let moved_usage = KvKey::new(KV_NAMESPACE, "usage_b_0").expect("valid key");
        assert_eq!(
            store.get(&moved_usage).expect("the test store"),
            Some(b"y".to_vec())
        );
    }

    /// §6, B1: a missing file must not delete any usage record.
    #[gpui::test]
    fn a_missing_file_keeps_the_usage_records(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            install(cx, vec![group_of(2)]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            let record = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("valid key");
            store.put(&record, b"a").expect("the test store");
            apply_loaded(
                cx,
                FileRead {
                    hash: 99,
                    parsed: None,
                },
            );
        });
        cx.run_until_parked();
        let record = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("valid key");
        assert_eq!(
            store.get(&record).expect("the test store"),
            Some(b"a".to_vec())
        );
    }

    /// §6, B1: a file that does not parse must not delete any usage record either.
    #[gpui::test]
    fn a_broken_file_keeps_the_usage_records(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            install(cx, vec![group_of(2)]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            let record = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("valid key");
            store.put(&record, b"a").expect("the test store");
            apply_loaded(
                cx,
                FileRead {
                    hash: 77,
                    parsed: Some(Err(QuickLaunchError::Parse {
                        line: 1,
                        column: 1,
                        message: "quebrado".to_string(),
                    })),
                },
            );
        });
        cx.run_until_parked();
        let record = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("valid key");
        assert_eq!(
            store.get(&record).expect("the test store"),
            Some(b"a".to_vec())
        );
    }

    /// §6, E2: a load deletes the usage records that name no item, and only those.
    #[gpui::test]
    fn a_load_deletes_the_usage_records_of_items_that_are_gone(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            let mut group = group_of(2);
            group.id = "general".to_string();
            install(cx, vec![group]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            let kept = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("valid key");
            let gone = KvKey::new(KV_NAMESPACE, "usage_general_9").expect("valid key");
            let other = KvKey::new(KV_NAMESPACE, "last_group").expect("valid key");
            store.put(&kept, b"a").expect("the test store");
            store.put(&gone, b"b").expect("the test store");
            store.put(&other, b"c").expect("the test store");
            apply_loaded(
                cx,
                FileRead {
                    hash: 4321,
                    parsed: Some(Ok(Parsed {
                        config: QuickLaunchConfig {
                            groups: vec![group_of(2)],
                            ..QuickLaunchConfig::default()
                        },
                        warnings: Vec::new(),
                    })),
                },
            );
        });
        // The prune runs on a background task; like the other worker tests, wait
        // for it rather than assuming the first tick has run it.
        let gone = KvKey::new(KV_NAMESPACE, "usage_general_9").expect("valid key");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            if store.get(&gone).expect("the test store").is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let kept = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("valid key");
        let other = KvKey::new(KV_NAMESPACE, "last_group").expect("valid key");
        assert_eq!(
            store.get(&kept).expect("the test store"),
            Some(b"a".to_vec())
        );
        assert_eq!(store.get(&gone).expect("the test store"), None);
        assert_eq!(
            store.get(&other).expect("the test store"),
            Some(b"c".to_vec()),
            "only usage records are touched"
        );
    }

    /// §6, S3: a moved item takes its usage record with it, in one transaction.
    #[gpui::test]
    fn a_move_takes_the_usage_record_along(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            let mut source = group_of(0);
            source.id = "general".to_string();
            source.items = vec![Item::new(0, "app", "A")];
            let mut target = group_of(0);
            target.id = "dev".to_string();
            install(cx, vec![source, target]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            let key = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("valid key");
            store.put(&key, b"count").expect("the test store");
            assert!(move_item_to_group(cx, 0, 0, 1));
        });
        cx.run_until_parked();
        let moved = KvKey::new(KV_NAMESPACE, "usage_dev_0").expect("valid key");
        assert_eq!(
            store.get(&moved).expect("the test store"),
            Some(b"count".to_vec())
        );
        let old = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("valid key");
        assert_eq!(store.get(&old).expect("the test store"), None);
    }

    /// §6, S3: a swap exchanges the two usage records.
    #[gpui::test]
    fn a_swap_exchanges_the_usage_records(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            let mut group = group_of(0);
            group.id = "general".to_string();
            group.items = vec![Item::new(0, "app", "A"), Item::new(2, "app", "B")];
            install(cx, vec![group]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            let first = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("valid key");
            let second = KvKey::new(KV_NAMESPACE, "usage_general_2").expect("valid key");
            store.put(&first, b"um").expect("the test store");
            store.put(&second, b"dois").expect("the test store");
            assert!(swap_slots(cx, 0, 0, 2));
        });
        cx.run_until_parked();
        let first = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("valid key");
        let second = KvKey::new(KV_NAMESPACE, "usage_general_2").expect("valid key");
        assert_eq!(
            store.get(&first).expect("the test store"),
            Some(b"dois".to_vec())
        );
        assert_eq!(
            store.get(&second).expect("the test store"),
            Some(b"um".to_vec())
        );
    }

    /// §11.1: the pin flips `topmost`, marks the state for the throttle, and the window
    /// keeps its place until Esc or the chord.
    #[gpui::test]
    fn the_pin_flips_topmost_and_asks_for_a_save(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(1)]);
            assert!(!is_locked(cx));
            assert!(toggle_lock(cx));
            assert!(is_locked(cx));
            assert!(
                state(cx).is_some_and(|state| state.pending_save),
                "a writable file saves the pin through the throttle"
            );
            assert!(!toggle_lock(cx));
            assert!(!is_locked(cx));
        });
    }

    /// §11.1: a read-only file still pins the window, but never asks for a save.
    #[gpui::test]
    fn a_read_only_file_pins_without_saving(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(1)]);
            if let Some(state) = state_mut(cx) {
                state.status = Status::ReadOnly("schema_version 99".to_string());
            }
            assert!(toggle_lock(cx));
            assert!(is_locked(cx), "the pin works for this session");
            assert!(
                state(cx).is_some_and(|state| !state.pending_save),
                "nothing is written for a file that cannot be written"
            );
        });
    }

    /// §11.1: a pinned window stays after a launch and after a reveal; `Esc` and the
    /// chord are the ways out.
    #[test]
    fn a_pinned_window_does_not_hide_after_an_action() {
        assert!(hides_after_action(true, false));
        assert!(!hides_after_action(true, true));
        assert!(!hides_after_action(false, false));
    }

    /// §11.1: a hot reload that changes `topmost` updates the state the button reads.
    #[gpui::test]
    fn a_reload_changes_the_pin(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(1)]);
            assert!(!is_locked(cx));
            let mut config = QuickLaunchConfig::default();
            config.window.topmost = true;
            apply_loaded(
                cx,
                FileRead {
                    hash: 12345,
                    parsed: Some(Ok(Parsed {
                        config,
                        warnings: Vec::new(),
                    })),
                },
            );
            assert!(is_locked(cx), "the button follows the file");
        });
    }

    /// §12.3, S4: a path edited to a folder turns the item into a folder, and one that
    /// points nowhere keeps the kind it had.
    #[gpui::test]
    fn an_edited_path_judges_the_kind_again(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("Uma pasta");
        std::fs::create_dir(&folder).unwrap();
        let folder_text = folder.to_string_lossy().to_string();
        // A program that is already there: the sandbox refuses to create an .exe.
        let program_text = "C:\\Windows\\System32\\cmd.exe".to_string();
        cx.update(|cx| {
            let mut group = group_of(0);
            group.id = "general".to_string();
            let mut item = Item::new(0, "app", "A");
            item.path = Some(program_text.clone());
            group.items = vec![item];
            install(cx, vec![group]);
            // A program edited to a folder becomes a folder.
            assert!(edit_item(cx, 0, 0, "A", &folder_text, "", false));
        });
        cx.run_until_parked();
        let kind = cx.update(|cx| item_at(cx, 0, 0).map(|item| item.kind));
        assert_eq!(kind, Some("folder".to_string()));
        cx.update(|cx| {
            // A folder edited back to a program becomes an app again.
            assert!(edit_item(cx, 0, 0, "A", &program_text, "", false));
        });
        cx.run_until_parked();
        let kind = cx.update(|cx| item_at(cx, 0, 0).map(|item| item.kind));
        assert_eq!(kind, Some("app".to_string()));
        cx.update(|cx| {
            // A path that is not there keeps the kind: the health check reports it.
            assert!(edit_item(
                cx,
                0,
                0,
                "A",
                "C:\\nao\\existe\\nada.exe",
                "",
                false
            ));
        });
        cx.run_until_parked();
        let kind = cx.update(|cx| item_at(cx, 0, 0).map(|item| item.kind));
        assert_eq!(kind, Some("app".to_string()));
    }

    /// §7, S2: the context is state, not something the foreground reads a file for.
    #[gpui::test]
    fn the_expansion_context_is_built_once(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(0)]);
            let stored = state(cx).map(|state| state.expand.program_dir.clone());
            assert_eq!(
                stored,
                Some(ExpandContext::current().program_dir),
                "the state holds the context every path expansion uses"
            );
            let mut item = Item::new(0, "folder", "Pasta");
            item.path = Some("{luma}".to_string());
            let expanded = expanded_path(cx, &item);
            assert_eq!(expanded, stored, "{{luma}} expands from the stored context");
        });
    }

    /// §12.1, B3: an edit forgets the old answer and asks for a new one.
    #[gpui::test]
    fn an_edit_forgets_the_health_of_that_item(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(2);
            group.id = "general".to_string();
            install(cx, vec![group]);
            apply_health(cx, "general".to_string(), 0, Health::Missing);
            assert!(is_missing(cx, 0, 0));
            assert!(edit_item(
                cx,
                0,
                0,
                "Novo",
                "C:\\Windows\\System32\\notepad.exe",
                "",
                false
            ));
            assert_eq!(
                health_of(cx, 0, 0),
                None,
                "the grey mark is gone until the icon thread answers"
            );
            // A second item keeps its answer: only the edited slot is cleared.
            apply_health(cx, "general".to_string(), 1, Health::Missing);
            assert!(put_item(cx, 0, 1, Item::new(1, "app", "Outro")));
            assert_eq!(health_of(cx, 0, 1), None);
        });
    }

    /// §12.1, B3: swapping two slots swaps their answers with them.
    #[gpui::test]
    fn a_swap_takes_the_health_along(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.id = "general".to_string();
            group.items = vec![Item::new(0, "app", "A"), Item::new(2, "app", "B")];
            install(cx, vec![group]);
            apply_health(cx, "general".to_string(), 0, Health::Missing);
            apply_health(cx, "general".to_string(), 2, Health::Ok);
            assert!(swap_slots(cx, 0, 0, 2));
            assert_eq!(health_of(cx, 0, 2), Some(Health::Missing));
            assert_eq!(health_of(cx, 0, 0), Some(Health::Ok));
            // A move into a free slot carries the answer but leaves nothing
            // behind.
            assert!(swap_slots(cx, 0, 0, 5));
            assert_eq!(health_of(cx, 0, 5), Some(Health::Ok));
            assert_eq!(health_of(cx, 0, 0), None);
        });
    }

    /// §12.1, B3: an item moved to another group takes its answer there.
    #[gpui::test]
    fn a_move_takes_the_health_to_the_new_group(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut source = group_of(0);
            source.id = "general".to_string();
            source.items = vec![Item::new(0, "app", "A")];
            let mut target = group_of(0);
            target.id = "dev".to_string();
            install(cx, vec![source, target]);
            apply_health(cx, "general".to_string(), 0, Health::Missing);
            assert!(move_item_to_group(cx, 0, 0, 1));
            assert_eq!(health_of(cx, 0, 0), None, "nothing left behind");
            assert_eq!(health_of(cx, 1, 0), Some(Health::Missing));
        });
    }

    /// §12.1, B3: deleting an item or a group drops its answers.
    #[gpui::test]
    fn deletes_drop_the_health(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(2);
            group.id = "general".to_string();
            let mut second = group_of(1);
            second.id = "dev".to_string();
            install(cx, vec![group, second]);
            apply_health(cx, "general".to_string(), 0, Health::Missing);
            apply_health(cx, "general".to_string(), 1, Health::Missing);
            apply_health(cx, "dev".to_string(), 0, Health::Missing);
            assert!(delete_item(cx, 0, 0));
            assert_eq!(health_of(cx, 0, 0), None);
            assert_eq!(health_of(cx, 0, 1), Some(Health::Missing));
            assert!(delete_group(cx, 0, true));
            assert_eq!(health_of(cx, 0, 1), None, "the whole group's answers go");
            // The group left keeps its own answer, now under index 0.
            assert_eq!(health_of(cx, 0, 0), Some(Health::Missing));
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
            assert!(!update_item(
                cx,
                0,
                0,
                &ItemEdit {
                    kind: "app".to_string(),
                    name: "Nome".to_string(),
                    path: "C:\\a.exe".to_string(),
                    args: String::new(),
                    cwd: String::new(),
                    admin: false,
                    icon: String::new(),
                    tags: Vec::new(),
                    command: None,
                    group_items: None,
                }
            ));
            assert!(!move_group(cx, 0, 1));
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

    /// §17.1 B: the settings page's form changes the fields the window's dialog
    /// does not offer, and only lets the kinds of the form be introduced.
    #[gpui::test]
    fn a_settings_page_item_form_covers_the_other_fields(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(0)]);
            assert!(put_item(cx, 0, 2, Item::new(2, "app", "Antigo")));
            let edit = ItemEdit {
                kind: "folder".to_string(),
                name: "  Nova  ".to_string(),
                path: " C:\\Pasta ".to_string(),
                args: " /a ".to_string(),
                cwd: "C:\\Trabalho".to_string(),
                admin: true,
                icon: "C:\\icone.png".to_string(),
                tags: vec![" um ".to_string(), String::new(), "dois".to_string()],
                command: None,
                group_items: None,
            };
            assert!(update_item(cx, 0, 2, &edit));
            let item = item_at(cx, 0, 2).expect("the item is still there");
            assert_eq!(item.kind, "folder");
            assert_eq!(item.name, "Nova");
            assert_eq!(item.path.as_deref(), Some("C:\\Pasta"));
            assert_eq!(item.args.as_deref(), Some("/a"));
            assert_eq!(item.cwd.as_deref(), Some("C:\\Trabalho"));
            assert_eq!(item.icon.as_deref(), Some("C:\\icone.png"));
            assert!(item.admin);
            assert_eq!(item.tags, vec!["um".to_string(), "dois".to_string()]);

            // A name or a path left empty is "cancel", as in the dialog.
            let mut empty = edit.clone();
            empty.name = "   ".to_string();
            assert!(!update_item(cx, 0, 2, &empty));
            empty.name = "Nova".to_string();
            empty.path = String::new();
            assert!(!update_item(cx, 0, 2, &empty));

            // A command comes with its own fields and no path, and its trust
            // record is written as it is saved (D5, §17.2 D.6).
            let command = ItemEdit {
                kind: KIND_COMMAND.to_string(),
                name: "Comando".to_string(),
                path: String::new(),
                args: String::new(),
                admin: true,
                command: Some(CommandEdit {
                    shell: "cmd".to_string(),
                    script: "echo ok".to_string(),
                    hidden: true,
                    keep_open: false,
                }),
                ..edit.clone()
            };
            assert!(update_item(cx, 0, 2, &command));
            let item = item_at(cx, 0, 2).expect("the item is still there");
            assert_eq!(item.kind, KIND_COMMAND);
            assert_eq!(item.path, None, "a command has no path");
            assert_eq!(
                item.extra.get("shell").and_then(|value| value.as_str()),
                Some("cmd")
            );
            assert_eq!(
                item.extra.get("script").and_then(|value| value.as_str()),
                Some("echo ok")
            );
            assert_eq!(
                item.extra.get("hidden").and_then(|value| value.as_bool()),
                Some(true)
            );
            assert!(!item.admin, "a command cannot run elevated (D4)");
            let spec = command_spec(&item).expect("a valid command");
            let trust = command_trust_key(&item, &spec);
            assert!(
                state(cx).is_some_and(|state| state.restored.trusted.contains(&trust)),
                "saving through the form trusts the new key"
            );

            let broken = ItemEdit {
                command: Some(CommandEdit {
                    shell: "bash".to_string(),
                    script: String::new(),
                    hidden: false,
                    keep_open: false,
                }),
                ..command.clone()
            };
            assert!(!update_item(cx, 0, 2, &broken));

            // A kind the form cannot build is kept, never introduced.
            let introduced = ItemEdit {
                kind: "plugin:x".to_string(),
                ..command.clone()
            };
            assert!(!update_item(cx, 0, 2, &introduced));

            // Going back to an ordinary kind drops the command's `extra`.
            let back = ItemEdit {
                kind: "app".to_string(),
                path: "C:\\a.exe".to_string(),
                command: None,
                ..edit.clone()
            };
            assert!(update_item(cx, 0, 2, &back));
            let item = item_at(cx, 0, 2).expect("the item is still there");
            assert_eq!(item.kind, "app");
            assert!(item.extra.is_empty(), "the command fields go with the kind");
        });
    }

    /// §17.1 B: a group moves by swapping with its neighbour, and the window
    /// keeps showing the group it was showing.
    #[gpui::test]
    fn a_settings_page_move_reorders_groups(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut first = group_of(0);
            first.id = "first".to_string();
            first.name = "Primeiro".to_string();
            let mut second = group_of(0);
            second.id = "second".to_string();
            second.name = "Segundo".to_string();
            let mut third = group_of(0);
            third.id = "third".to_string();
            third.name = "Terceiro".to_string();
            install(cx, vec![first, second, third]);
            assert!(move_group(cx, 0, 1));
            assert_eq!(
                state(cx).map(|state| state.config.groups[1].name.clone()),
                Some("Primeiro".to_string())
            );
            assert_eq!(
                state(cx).map(|state| state.group),
                Some(1),
                "the window follows the group it was showing"
            );
            assert!(!move_group(cx, 1, 1), "a move onto itself is nothing");
            assert!(!move_group(cx, 0, 9), "past the end");
            assert_eq!(
                state(cx).map(|state| state.config.groups.len()),
                Some(3),
                "a refused move changes nothing"
            );
        });
    }

    /// §17.1 B: every window setting of the page goes through `set_window`,
    /// which touches and marks the file.
    #[gpui::test]
    fn the_settings_page_window_changes_are_marked_for_saving(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install(cx, vec![group_of(0)]);
            set_window(cx, |window| window.view = View::List);
            assert_eq!(
                state(cx).map(|state| state.config.window.view),
                Some(View::List)
            );
            let (pending, revision) = state(cx)
                .map(|state| (state.pending_save, state.revision))
                .expect("the state is installed");
            assert!(pending, "an edit asks for a save");
            set_window(cx, |window| window.hotkey = "ctrl+alt+Space".to_string());
            assert!(state(cx).is_some_and(|state| state.revision > revision));
            set_window(cx, |window| window.topmost = true);
            assert!(state(cx).is_some_and(|state| state.config.window.topmost));
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
            batch: None,
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
            batch: None,
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
            // A key of another milestone, or one too short to be a trust key: skipped,
            // not an error.
            (
                KvKey::new(KV_NAMESPACE, "trusted_00ff").unwrap(),
                b"true".to_vec(),
            ),
            // A trust record this build wrote: the hex part is restored.
            (
                KvKey::new(KV_NAMESPACE, "trusted_0123456789abcdef0123456789abcdef").unwrap(),
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
        assert_eq!(
            restored.trusted.iter().collect::<Vec<_>>(),
            vec!["0123456789abcdef0123456789abcdef"],
            "a key that is not 32 hex characters is not a trust record"
        );
    }

    #[test]
    fn an_icon_cache_file_name_is_a_hash_that_changes_with_the_edge() {
        let directory = tempfile::tempdir().unwrap();
        // Named `.txt` rather than `.exe`: the hash only needs a file that
        // exists, and a test that writes an executable into the temporary
        // directory gets refused by the on-access scanner on some machines.
        let file = directory.path().join("programa.txt");
        std::fs::write(&file, b"nao e um exe de verdade").unwrap();
        let small = icon_cache_file(&file, 32, None);
        let large = icon_cache_file(&file, 64, None);
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

    /// A `command` item with a valid spec, for the confirmation tests (§17.2 D).
    fn command_item(slot: u32, script: &str) -> Item {
        let mut item = Item::new(slot, KIND_COMMAND, "Script");
        item.extra.insert("shell".to_string(), "cmd".into());
        item.extra.insert("script".to_string(), script.into());
        item
    }

    /// §17.2 D.1: the same command always gets the same key, and any field that
    /// changes what runs changes it too, so a changed script is confirmed again.
    #[test]
    fn a_command_trust_key_is_stable_and_follows_every_field() {
        let item = command_item(0, "echo ok");
        let spec = command_spec(&item).expect("a valid command");
        let key = command_trust_key(&item, &spec);
        assert_eq!(key.len(), TRUST_KEY_HEX_LENGTH);
        assert!(
            key.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "lowercase hex, as the KV name rules ask: {key}"
        );
        assert!(KvKey::new(KV_NAMESPACE, &format!("{TRUSTED_PREFIX}{key}")).is_ok());
        assert_eq!(
            command_trust_key(&item, &command_spec(&item).expect("a valid command")),
            key,
            "the same command answers the same key"
        );
        for field in [
            {
                let mut changed = item.clone();
                changed
                    .extra
                    .insert("script".to_string(), "echo ok!".into());
                changed
            },
            {
                let mut changed = item.clone();
                changed.extra.insert("shell".to_string(), "pwsh".into());
                changed
            },
            {
                let mut changed = item.clone();
                changed.cwd = Some("C:\\Temp".to_string());
                changed
            },
            {
                let mut changed = item.clone();
                changed.extra.insert("hidden".to_string(), true.into());
                changed
            },
            {
                let mut changed = item.clone();
                changed.extra.insert("keep_open".to_string(), true.into());
                changed
            },
        ] {
            let spec = command_spec(&field).expect("a valid command");
            assert_ne!(
                command_trust_key(&field, &spec),
                key,
                "a changed {field:?} needs its own confirmation"
            );
        }
    }

    /// §17.2 D.2: an untrusted command opens the dialog instead of a start request.
    ///
    /// The dialog belongs to the window, so the hidden-window path of D.4 calls
    /// `show`; a headless test window cannot be made visible, so the state is
    /// marked visible here and the dialog itself is what the test checks.
    #[gpui::test]
    fn an_untrusted_command_opens_the_confirmation_instead_of_launching(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![command_item(0, "echo ok")];
            install(cx, vec![group]);
            if let Some(state) = state_mut(cx) {
                state.visible = true;
            }
            activate_with(cx, 0, ClickKind::Plain);
            let state = state(cx).expect("the state is installed");
            assert!(state.pending_command.is_some(), "the dialog is waiting");
            assert_eq!(state.generation, 0, "nothing was sent to the launcher");
            assert_eq!(state.pending_usage, None, "no launch was counted");
            let confirmation = command_confirmation(cx).expect("the dialog has content");
            assert_eq!(confirmation.name, "Script");
            assert_eq!(confirmation.shell, "cmd");
            assert_eq!(confirmation.script, "echo ok");
            assert!(!confirmation.hidden);
            assert!(
                !confirmation.remembered,
                "the sample has no store, so the note is shown"
            );
        });
    }

    /// §17.2 D.3: "Run" writes the key and only then asks the launcher to start
    /// the command.
    #[gpui::test]
    fn confirming_a_command_writes_the_key_and_launches(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        let mut trust_key = String::new();
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![command_item(0, "echo ok")];
            install(cx, vec![group]);
            if let Some(state) = state_mut(cx) {
                state.visible = true;
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            activate_with(cx, 0, ClickKind::Plain);
            let item = item_at(cx, 0, 0).expect("the item is there");
            trust_key = command_trust_key(&item, &command_spec(&item).expect("a valid command"));
            confirm_pending_command(cx);
            let state = state(cx).expect("the state is installed");
            assert!(
                state.pending_command.is_none(),
                "the dialog closes when Run is clicked"
            );
            assert!(
                state.restored.trusted.contains(&trust_key),
                "the trust is in memory before the store answers"
            );
        });
        cx.run_until_parked();
        let key = KvKey::new(KV_NAMESPACE, &format!("{TRUSTED_PREFIX}{trust_key}"))
            .expect("a valid trust key");
        assert_eq!(
            store.get(&key).expect("the test store"),
            Some(b"true".to_vec()),
            "the key reaches the store"
        );
        let sent =
            cx.update(|cx| state(cx).map(|state| (state.generation, state.pending_usage.clone())));
        assert_eq!(
            sent,
            Some((1, Some(("general".to_string(), 0)))),
            "one launch request follows the write"
        );
    }

    /// §17.2 D.9: a script edited after its first run is a different command and
    /// must be confirmed again; the unchanged one runs without asking.
    #[gpui::test]
    fn a_changed_script_asks_for_confirmation_again(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![command_item(0, "echo ok")];
            install(cx, vec![group]);
            if let Some(state) = state_mut(cx) {
                state.visible = true;
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            activate_with(cx, 0, ClickKind::Plain);
            confirm_pending_command(cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            // The same script is trusted now.
            activate_with(cx, 0, ClickKind::Plain);
            assert!(
                state(cx).is_some_and(|state| state.pending_command.is_none()),
                "the confirmed script runs without asking"
            );
            // A hand edit changes the script behind the state's back.
            if let Some(state) = state_mut(cx) {
                if let Some(item) = state.config.groups[0].items.first_mut() {
                    item.extra.insert("script".to_string(), "echo outro".into());
                }
                state.touch();
            }
            activate_with(cx, 0, ClickKind::Plain);
            let state = state(cx).expect("the state is installed");
            assert!(
                state.pending_command.is_some(),
                "the changed script asks for confirmation again"
            );
        });
    }

    /// §17.2 D.6 (D5): saving a command through the settings form trusts the key
    /// it just wrote.
    #[gpui::test]
    fn saving_a_command_through_the_form_writes_its_trust_record(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            install(cx, vec![group_of(0)]);
            assert!(put_item(cx, 0, 1, Item::new(1, "app", "Antigo")));
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            let edit = ItemEdit {
                kind: KIND_COMMAND.to_string(),
                name: "Comando".to_string(),
                path: String::new(),
                args: String::new(),
                cwd: String::new(),
                admin: false,
                icon: String::new(),
                tags: Vec::new(),
                command: Some(CommandEdit {
                    shell: "cmd".to_string(),
                    script: "echo ok".to_string(),
                    hidden: false,
                    keep_open: false,
                }),
                group_items: None,
            };
            assert!(update_item(cx, 0, 1, &edit));
        });
        cx.run_until_parked();
        let key_name = cx.update(|cx| {
            let item = item_at(cx, 0, 1).expect("the item is there");
            let spec = command_spec(&item).expect("a valid command");
            format!("{TRUSTED_PREFIX}{}", command_trust_key(&item, &spec))
        });
        let key = KvKey::new(KV_NAMESPACE, &key_name).expect("a valid trust key");
        assert_eq!(
            store.get(&key).expect("the test store"),
            Some(b"true".to_vec())
        );
        assert_eq!(cx.update(|cx| command_trust(cx, "general", 1)), Some(true));
    }

    /// §17.2 D.7: revoking removes the record from memory and from the store.
    #[gpui::test]
    fn revoking_a_trust_removes_the_record(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        let item = command_item(0, "echo ok");
        let spec = command_spec(&item).expect("a valid command");
        let trust_key = command_trust_key(&item, &spec);
        let key = KvKey::new(KV_NAMESPACE, &format!("{TRUSTED_PREFIX}{trust_key}"))
            .expect("a valid trust key");
        store.put(&key, b"true").expect("the test store");
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![item];
            install(cx, vec![group]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
                state.restored.trusted.insert(trust_key.clone());
            }
            assert_eq!(command_trust(cx, "general", 0), Some(true));
            assert!(revoke_command_trust(cx, "general", 0));
            assert_eq!(command_trust(cx, "general", 0), Some(false));
        });
        cx.run_until_parked();
        assert_eq!(store.get(&key).expect("the test store"), None);
    }

    /// §17.2 D.5: without a store the trust is kept in memory and forgotten when
    /// the process ends.
    #[gpui::test]
    fn a_resident_process_keeps_its_trust_in_memory_only(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![command_item(0, "echo ok")];
            install(cx, vec![group]);
            if let Some(state) = state_mut(cx) {
                state.visible = true;
                state.mode = Mode::Standalone;
            }
            activate_with(cx, 0, ClickKind::Plain);
            assert!(
                state(cx).is_some_and(|state| state.pending_command.is_some()),
                "the first run still asks"
            );
            confirm_pending_command(cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let installed = state(cx).expect("the state is installed");
            assert_eq!(installed.mode, Mode::Standalone);
            assert!(installed.kv.is_none(), "the resident process has no store");
            assert_eq!(installed.generation, 1, "the command did start");
            assert!(
                !command_confirmation(cx).is_some_and(|confirmation| confirmation.remembered),
                "the dialog would say the trust is not stored"
            );
            // Within this process the confirmation holds.
            activate_with(cx, 0, ClickKind::Plain);
            let installed = state(cx).expect("the state is installed");
            assert!(
                installed.pending_command.is_none(),
                "the same process runs it without asking again"
            );
            assert_eq!(installed.generation, 2);
        });
    }

    /// §17.2 D.8: pruning usage records must never delete a trust record.
    #[gpui::test]
    fn pruning_usage_leaves_the_trust_records_alone(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        let trusted_name = format!("{TRUSTED_PREFIX}0123456789abcdef0123456789abcdef");
        let kept = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("a valid key");
        let stale = KvKey::new(KV_NAMESPACE, "usage_general_9").expect("a valid key");
        let trusted = KvKey::new(KV_NAMESPACE, &trusted_name).expect("a valid key");
        store.put(&kept, b"a").expect("the test store");
        store.put(&stale, b"b").expect("the test store");
        store.put(&trusted, b"true").expect("the test store");
        cx.update(|cx| {
            install(cx, vec![group_of(1)]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            prune_usage(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            store.get(&kept).expect("the test store"),
            Some(b"a".to_vec()),
            "a live usage record stays"
        );
        assert_eq!(
            store.get(&stale).expect("the test store"),
            None,
            "the stale usage record goes"
        );
        assert_eq!(
            store.get(&trusted).expect("the test store"),
            Some(b"true".to_vec()),
            "a trust record is not a usage record and stays"
        );
    }

    /// A leaf item that can be referenced by a `group` item (§17.2 E).
    fn group_leaf(slot: u32, id: &str) -> Item {
        let mut item = Item::new(slot, "app", id);
        item.id = Some(id.to_string());
        item
    }

    /// A `group` item with `references`, in start order.
    fn group_bundle(slot: u32, id: &str, references: &[&str]) -> Item {
        let mut item = Item::new(slot, KIND_GROUP, id);
        item.id = Some(id.to_string());
        item.items = references
            .iter()
            .map(|reference| reference.to_string())
            .collect();
        item
    }

    /// §17.2 E.1: references start in order, a nested group is flattened where it
    /// appears, and an id that was already expanded is skipped.
    #[test]
    fn group_references_start_in_order_and_flatten_nesting() {
        let items = vec![
            group_leaf(0, "a"),
            group_leaf(1, "b"),
            group_leaf(2, "c"),
            group_leaf(3, "d"),
            group_bundle(4, "inner", &["c", "d"]),
            group_bundle(5, "outer", &["b", "inner", "a", "a"]),
        ];
        let plan = expand_group(&items, &items[5]);
        let names: Vec<&str> = plan.items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["b", "c", "d", "a"]);
        assert_eq!(plan.excess, 0, "nothing was left out");
    }

    /// §17.2 E.1: a cycle in memory is broken by the visited set instead of
    /// recursing forever; the parse repairs one, but the runtime must not hang.
    #[test]
    fn group_references_break_a_cycle() {
        let items = vec![
            group_leaf(0, "x"),
            group_bundle(1, "a", &["b"]),
            group_bundle(2, "b", &["a", "x"]),
        ];
        let plan = expand_group(&items, &items[1]);
        let names: Vec<&str> = plan.items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["x"]);
    }

    /// §17.2 E.1: a plan stops at sixteen entries and reports the rest instead of
    /// starting them.
    #[test]
    fn group_references_stop_at_the_entry_limit() {
        let mut items: Vec<Item> = (0..20)
            .map(|slot| Item {
                slot,
                ..group_leaf(slot, &format!("i{slot}"))
            })
            .collect();
        let references: Vec<String> = items.iter().filter_map(|item| item.id.clone()).collect();
        let reference_text: Vec<&str> = references.iter().map(String::as_str).collect();
        items.push(group_bundle(20, "all", &reference_text));
        let plan = expand_group(&items, &items[20]);
        assert_eq!(plan.items.len(), MAX_GROUP_REFERENCES);
        assert_eq!(plan.excess, 20 - MAX_GROUP_REFERENCES);
        assert_eq!(
            plan.items.first().map(|item| item.name.as_str()),
            Some("i0"),
            "the first references start"
        );
    }

    /// §17.2 E.1: a reference that names another group's item is not found in
    /// this group, so it is ignored.
    #[test]
    fn group_references_ignore_other_groups() {
        let group = vec![group_leaf(0, "a")];
        let mut bundle = group_bundle(1, "outer", &["a", "foreign"]);
        bundle.id = Some("outer".to_string());
        let plan = expand_group(&group, &bundle);
        assert_eq!(
            plan.items
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(plan.excess, 0, "a missing reference is not an overflow");
    }

    /// §17.2 E.1: nesting is followed up to [`MAX_GROUP_NESTING`] levels.
    #[test]
    fn group_references_stop_at_the_nesting_limit() {
        let build = |links: usize| {
            let mut items = vec![group_leaf(0, "leaf")];
            let mut references = vec!["leaf".to_string()];
            for index in 0..links {
                let id = format!("n{index}");
                let reference_text: Vec<&str> = references.iter().map(String::as_str).collect();
                items.push(group_bundle(
                    u32::try_from(index + 1).unwrap_or(1),
                    &id,
                    &reference_text,
                ));
                references = vec![id];
            }
            let last = references.first().cloned().unwrap_or_default();
            let outer_reference: Vec<&str> = vec![last.as_str()];
            items.push(group_bundle(
                u32::try_from(links + 1).unwrap_or(1),
                "outer",
                &outer_reference,
            ));
            items
        };
        let allowed = build(MAX_GROUP_NESTING);
        let plan = expand_group(&allowed, allowed.last().expect("the outer bundle"));
        assert_eq!(
            plan.items
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["leaf"],
            "eight nested groups still reach the leaf"
        );
        let too_deep = build(MAX_GROUP_NESTING + 1);
        let plan = expand_group(&too_deep, too_deep.last().expect("the outer bundle"));
        assert!(plan.items.is_empty(), "the ninth level is not followed");
    }

    /// §17.2 E.2: a group with no valid reference is invalid; one with a
    /// reference is ok, and its entries keep their own answers.
    #[test]
    fn a_group_is_invalid_only_without_a_valid_reference() {
        let items = vec![group_leaf(0, "a")];
        assert_eq!(
            group_health(&items, &group_bundle(1, "empty", &[])),
            Health::Invalid(nohrs_core::t!("Este grupo não tem itens válidos.").to_string())
        );
        assert_eq!(
            group_health(&items, &group_bundle(2, "ghost", &["ghost"])),
            Health::Invalid(nohrs_core::t!("Este grupo não tem itens válidos.").to_string())
        );
        assert_eq!(
            group_health(&items, &group_bundle(3, "real", &["a"])),
            Health::Ok
        );
    }

    /// D9: inside a group `ask` behaves as `activate`.
    #[test]
    fn asking_inside_a_group_becomes_activating() {
        assert_eq!(group_on_running(OnRunning::Ask), OnRunning::Activate);
        assert_eq!(group_on_running(OnRunning::New), OnRunning::New);
        assert_eq!(group_on_running(OnRunning::Activate), OnRunning::Activate);
    }

    /// §17.2 E.1: untrusted commands of the whole plan share one dialog, and
    /// cancelling it starts nothing.
    #[gpui::test]
    fn untrusted_group_commands_share_one_confirmation(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![
                group_leaf(1, "a"),
                command_item(2, "echo um"),
                command_item(3, "echo dois"),
                group_bundle(4, "inner", &["c"]),
                group_bundle(0, "bundle", &["a", "b", "inner"]),
            ];
            group.items[1].id = Some("b".to_string());
            group.items[2].id = Some("c".to_string());
            group.items[2].name = "Dois".to_string();
            group.items[1].name = "Um".to_string();
            install(cx, vec![group]);
            if let Some(state) = state_mut(cx) {
                state.visible = true;
            }
            activate_with(cx, 0, ClickKind::Plain);
            let installed = state(cx).expect("the state is installed");
            assert!(
                installed.pending_command.is_none(),
                "a group does not use the single-command dialog"
            );
            let pending = installed
                .pending_group
                .as_ref()
                .expect("the group dialog is waiting");
            assert_eq!(pending.commands.len(), 2, "one entry per untrusted command");
            assert_eq!(installed.generation, 0, "nothing was sent to the launcher");
            let confirmation = group_confirmation(cx).expect("the dialog has content");
            assert_eq!(confirmation.name, "bundle");
            let names: Vec<&str> = confirmation
                .commands
                .iter()
                .map(|command| command.name.as_str())
                .collect();
            assert_eq!(names, vec!["Um", "Dois"]);
            assert_eq!(confirmation.commands[0].script, "echo um");
            assert_eq!(confirmation.commands[1].script, "echo dois");
            cancel_pending_command(cx);
            let state = state(cx).expect("the state is installed");
            assert!(state.pending_group.is_none(), "cancel closes the dialog");
            assert!(
                state.pending_batch.is_none(),
                "cancel refuses the whole group"
            );
            assert_eq!(state.generation, 0, "nothing started");
        });
        // "Run" trusts every command and only then sends the whole batch.
        cx.update(|cx| {
            activate_with(cx, 0, ClickKind::Plain);
            confirm_pending_group(cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let state = state(cx).expect("the state is installed");
            assert!(state.pending_group.is_none(), "Run closes the dialog");
            assert_eq!(state.restored.trusted.len(), 2, "both keys are remembered");
            let batch = state
                .pending_batch
                .as_ref()
                .expect("the whole group is in flight");
            assert_eq!(batch.total, 3, "the non-command entry starts too");
            assert_eq!(state.generation, 1, "one batch, one generation");
        });
    }

    /// §17.2 E.1: once every entry has answered, one footer line names the ones
    /// that did not start, and only then is the batch finished.
    #[gpui::test]
    fn a_group_reports_partial_failures_in_one_line(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![
                group_leaf(1, "a"),
                group_leaf(2, "b"),
                group_leaf(3, "c"),
                group_bundle(0, "bundle", &["a", "b", "c"]),
            ];
            group.items[1].name = "A".to_string();
            group.items[2].name = "B".to_string();
            group.items[3].name = "C".to_string();
            group.items[0].name = "Pacote".to_string();
            install(cx, vec![group]);
            activate_with(cx, 0, ClickKind::Plain);
            let installed = state(cx).expect("the state is installed");
            let batch = installed
                .pending_batch
                .as_ref()
                .expect("the batch is in flight");
            assert_eq!(batch.total, 3);
            assert_eq!(
                installed.message, None,
                "nothing is reported before the end"
            );
            let generation = installed.generation;
            apply_reply(
                cx,
                WorkerReply::Launched {
                    generation,
                    batch: Some(generation),
                    name: "A".to_string(),
                    outcome: Ok(LaunchOutcome::Started),
                },
            );
            apply_reply(
                cx,
                WorkerReply::Launched {
                    generation,
                    batch: Some(generation),
                    name: "B".to_string(),
                    outcome: Err(LaunchError::NotFound(PathBuf::from("C:\\nao\\existe"))),
                },
            );
            let after_one = state(cx).expect("the state is installed");
            assert!(
                after_one.pending_batch.is_some(),
                "one answer is still missing"
            );
            assert_eq!(after_one.message, None, "no line before the batch ends");
            apply_reply(
                cx,
                WorkerReply::Launched {
                    generation,
                    batch: Some(generation),
                    name: "C".to_string(),
                    outcome: Err(LaunchError::NotFound(PathBuf::from("C:\\nao\\existe"))),
                },
            );
            let after_all = state(cx).expect("the state is installed");
            assert!(after_all.pending_batch.is_none(), "the batch is finished");
            assert_eq!(
                after_all.message.as_deref(),
                Some(
                    nohrs_core::tf!(
                        "{failed} de {total} itens não iniciaram: {names}",
                        failed = 2,
                        total = 3,
                        names = "B, C"
                    )
                    .as_str()
                )
            );
        });
    }

    /// §17.2 E.1: entries past the limit are not started, and the footer says how
    /// many were left out.
    #[gpui::test]
    fn a_group_reports_entries_past_the_limit(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            for slot in 1..=18u32 {
                let id = format!("i{slot}");
                let mut item = group_leaf(slot, &id);
                item.name = format!("Item {slot}");
                group.items.push(item);
            }
            let references: Vec<String> = (1..=18u32).map(|slot| format!("i{slot}")).collect();
            let reference_text: Vec<&str> = references.iter().map(String::as_str).collect();
            group.items.push(group_bundle(0, "bundle", &reference_text));
            install(cx, vec![group]);
            activate_with(cx, 0, ClickKind::Plain);
            let generation = state(cx).map(|state| state.generation).unwrap_or(0);
            let total = state(cx)
                .and_then(|state| state.pending_batch.as_ref().map(|batch| batch.total))
                .unwrap_or(0);
            assert_eq!(total, MAX_GROUP_REFERENCES);
            for slot in 1..=u32::try_from(MAX_GROUP_REFERENCES).unwrap_or(16) {
                apply_reply(
                    cx,
                    WorkerReply::Launched {
                        generation,
                        batch: Some(generation),
                        name: format!("Item {slot}"),
                        outcome: Ok(LaunchOutcome::Started),
                    },
                );
            }
            let state = state(cx).expect("the state is installed");
            assert_eq!(
                state.message.as_deref(),
                Some(
                    nohrs_core::tf!(
                        "{skipped} itens do grupo excedem o limite de {limit} e não serão iniciados.",
                        skipped = 2,
                        limit = MAX_GROUP_REFERENCES
                    )
                    .as_str()
                )
            );
        });
    }

    /// §17.2 E.1: the use is recorded on the group item only, never on the
    /// entries it started.
    #[gpui::test]
    fn a_group_records_usage_only_on_itself(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![
                group_leaf(1, "a"),
                group_leaf(2, "b"),
                group_bundle(0, "bundle", &["a", "b"]),
            ];
            install(cx, vec![group]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            activate_with(cx, 0, ClickKind::Plain);
            let generation = state(cx).map(|state| state.generation).unwrap_or(0);
            for name in ["a", "b"] {
                apply_reply(
                    cx,
                    WorkerReply::Launched {
                        generation,
                        batch: Some(generation),
                        name: name.to_string(),
                        outcome: Ok(LaunchOutcome::Started),
                    },
                );
            }
            assert!(
                state(cx).is_some_and(|state| state.pending_usage.is_none()),
                "a batch never uses the single item's pending record"
            );
        });
        cx.run_until_parked();
        let usage = |slot: u32| {
            let key = KvKey::new(KV_NAMESPACE, &format!("usage_general_{slot}"))
                .expect("a valid usage key");
            store.get(&key).expect("the test store")
        };
        let group_usage = usage(0).expect("the group counted one use");
        assert!(
            String::from_utf8_lossy(&group_usage).contains("\"count\":1"),
            "the record is a usage count: {group_usage:?}"
        );
        assert!(usage(1).is_none(), "the first entry is not counted");
        assert!(usage(2).is_none(), "the second entry is not counted");
    }

    /// §17.2 E.3: saving a group gives ids to the items that had none, avoids a
    /// collision by appending `_2`, and writes the references in pick order in
    /// the same edit.
    #[gpui::test]
    fn saving_a_group_assigns_ids_without_collisions(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut group = group_of(0);
            group.items = vec![
                {
                    let mut item = Item::new(0, KIND_GROUP, "Pacote");
                    item.items = vec!["item_1".to_string()];
                    item
                },
                Item::new(1, "app", "Um"),
                {
                    let mut item = Item::new(2, "app", "Dois");
                    item.id = Some("item_1".to_string());
                    item
                },
                Item::new(3, "app", "Tres"),
            ];
            install(cx, vec![group]);
            let edit = ItemEdit {
                kind: KIND_GROUP.to_string(),
                name: "Pacote".to_string(),
                path: String::new(),
                args: String::new(),
                cwd: String::new(),
                admin: false,
                icon: String::new(),
                tags: Vec::new(),
                command: None,
                group_items: Some(vec![2, 1, 3]),
            };
            let revision = state(cx).map(|state| state.revision).unwrap_or(0);
            assert!(update_item(cx, 0, 0, &edit));
            let item = item_at(cx, 0, 0).expect("the group is still there");
            assert_eq!(
                item.id.as_deref(),
                Some("item_0"),
                "the group gets an id too"
            );
            assert_eq!(
                item.items,
                vec![
                    "item_1".to_string(),
                    "item_1_2".to_string(),
                    "item_3".to_string()
                ],
                "pick order, and the taken id gets a suffix"
            );
            assert_eq!(
                item_at(cx, 0, 1).and_then(|item| item.id),
                Some("item_1_2".to_string())
            );
            assert_eq!(
                item_at(cx, 0, 2).and_then(|item| item.id),
                Some("item_1".to_string()),
                "an existing id is kept"
            );
            assert_eq!(
                item_at(cx, 0, 3).and_then(|item| item.id),
                Some("item_3".to_string())
            );
            assert!(
                state(cx).is_some_and(|state| state.pending_save),
                "the whole assignment is one throttled write"
            );
            assert!(
                state(cx).map(|state| state.revision).unwrap_or(0) > revision,
                "the window is told what changed"
            );
            // Saving the same selection again is stable: no new ids, no `_3`.
            assert!(update_item(cx, 0, 0, &edit));
            assert_eq!(
                item_at(cx, 0, 0).map(|item| item.items),
                Some(vec![
                    "item_1".to_string(),
                    "item_1_2".to_string(),
                    "item_3".to_string()
                ])
            );
        });
    }

    /// Installs an `app` item, sends its launch, and answers it with
    /// `AlreadyRunning`, leaving the three-way dialog waiting (§17.2 F.2).
    fn install_running_program(cx: &mut gpui::App, process_id: u32) {
        let mut group = group_of(0);
        group.items = vec![Item::new(0, "app", "Programa")];
        install(cx, vec![group]);
        if let Some(state) = state_mut(cx) {
            state.visible = true;
            state.config.window.on_running = OnRunning::Ask;
        }
        activate_with(cx, 0, ClickKind::Plain);
        let generation = state(cx).map(|state| state.generation).unwrap_or(0);
        apply_reply(
            cx,
            WorkerReply::Launched {
                generation,
                batch: None,
                name: "Programa".to_string(),
                outcome: Ok(LaunchOutcome::AlreadyRunning { process_id }),
            },
        );
    }

    /// §17.2 F.2: a running program that the configuration asks about opens the
    /// dialog instead of counting a use.
    #[gpui::test]
    fn a_running_program_opens_the_three_way_dialog(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install_running_program(cx, 4242);
            let installed = state(cx).expect("the state is installed");
            let pending = installed
                .pending_already_running
                .as_ref()
                .expect("the dialog is waiting");
            assert_eq!(pending.process_id, 4242);
            assert_eq!(
                installed.pending_usage,
                Some(("general".to_string(), 0)),
                "nothing is counted before the user chooses"
            );
            assert_eq!(
                installed.message, None,
                "a running program is not a failure"
            );
            assert_eq!(
                installed.sent_launches,
                vec![LaunchMode::Normal],
                "nothing was sent after the answer"
            );
            let prompt = already_running_prompt(cx).expect("the dialog has content");
            assert_eq!(prompt.name, "Programa");
        });
    }

    /// §17.2 F.2 and F.5: "switch to it" sends the launch again for that process,
    /// and the use counts once the reply says it was activated.
    #[gpui::test]
    fn switching_to_the_running_program_asks_for_its_process(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            install_running_program(cx, 4242);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            switch_to_running_program(cx);
            let installed = state(cx).expect("the state is installed");
            assert!(
                installed.pending_already_running.is_none(),
                "the dialog closes"
            );
            assert_eq!(
                installed.sent_launches.last(),
                Some(&LaunchMode::Activate(4242))
            );
            assert_eq!(
                installed.pending_usage,
                Some(("general".to_string(), 0)),
                "the choice is counted when it starts"
            );
            let generation = installed.generation;
            apply_reply(
                cx,
                WorkerReply::Launched {
                    generation,
                    batch: None,
                    name: "Programa".to_string(),
                    outcome: Ok(LaunchOutcome::Activated),
                },
            );
            assert!(
                state(cx).is_some_and(|state| state.pending_usage.is_none()),
                "an answered launch is counted once"
            );
        });
        cx.run_until_parked();
        let key = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("a valid usage key");
        assert!(
            store.get(&key).expect("the test store").is_some(),
            "the use is recorded after the choice"
        );
    }

    /// §17.2 F.2 and F.5: "start another" sends a forced new instance.
    #[gpui::test]
    fn starting_another_instance_asks_for_a_new_one(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            install_running_program(cx, 4242);
            start_another_instance(cx);
            let state = state(cx).expect("the state is installed");
            assert!(state.pending_already_running.is_none(), "the dialog closes");
            assert_eq!(state.sent_launches.last(), Some(&LaunchMode::ForceNew));
        });
    }

    /// §17.2 F.2 and F.5: cancelling starts nothing and records no use.
    #[gpui::test]
    fn cancelling_the_running_program_records_no_use(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            install_running_program(cx, 4242);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            cancel_already_running(cx);
            let state = state(cx).expect("the state is installed");
            assert!(state.pending_already_running.is_none(), "the dialog closes");
            assert_eq!(
                state.pending_usage, None,
                "a cancelled launch must not be counted later"
            );
            assert_eq!(
                state.sent_launches,
                vec![LaunchMode::Normal],
                "cancel sends no request"
            );
            assert_eq!(state.generation, 1, "and changes no generation");
        });
        cx.run_until_parked();
        let key = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("a valid usage key");
        assert_eq!(
            store.get(&key).expect("the test store"),
            None,
            "cancel records no use"
        );
    }

    fn sample_state() -> QuickLaunch {
        QuickLaunch {
            config: QuickLaunchConfig {
                groups: vec![Group {
                    id: "general".to_string(),
                    name: "Geral".to_string(),
                    items: Vec::new(),
                    source: None,
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
            hotkey_refusal: None,
            loading: false,
            pending_save: false,
            save_due: None,
            _watcher: None,
            signals: None,
            generation: 0,
            message: None,
            pending_command: None,
            pending_group: None,
            pending_already_running: None,
            pending_batch: None,
            pending_hide: false,
            revision: 0,
            pending_usage: None,
            #[cfg(test)]
            sent_launches: Vec::new(),
            filter: String::new(),
            health: HashMap::new(),
            health_started: false,
            health_queue: VecDeque::new(),
            origins: HashMap::new(),
            folder_missing: HashSet::new(),
            folder_refresh: HashMap::new(),
            mode: Mode::Main,
            #[cfg(test)]
            health_checks: Vec::new(),
            expand: ExpandContext::current(),
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
            source: None,
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

    /// One group whose items are generated from `source` (§17.2 G).
    fn dynamic_group(id: &str, source: &str) -> Group {
        Group {
            id: id.to_string(),
            name: id.to_string(),
            items: Vec::new(),
            source: Some(source.to_string()),
        }
    }

    /// A state holding `groups` and the use records `(group, slot, count, last)`.
    fn usage_state(groups: Vec<Group>, usage: Vec<((&str, u32), u32, u64)>) -> QuickLaunch {
        let mut state = sample_state();
        state.config.groups = groups;
        for ((group, slot), count, last) in usage {
            state
                .restored
                .usage
                .insert((group.to_string(), slot), Usage { count, last });
        }
        state
    }

    /// The settings form's fields for one item, as a test edit.
    fn item_edit(path: &str) -> ItemEdit {
        ItemEdit {
            kind: "app".to_string(),
            name: "Outro".to_string(),
            path: path.to_string(),
            args: String::new(),
            cwd: String::new(),
            admin: false,
            icon: String::new(),
            tags: Vec::new(),
            command: None,
            group_items: None,
        }
    }

    /// §17.2 G.2: a `folder:` group lists its directory, sorted by name without
    /// regard to case, with hidden entries out and slots from 0.
    #[test]
    fn a_folder_group_lists_its_directory_in_order() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("banana.txt"), b"x").unwrap();
        std::fs::write(directory.path().join("Apple.txt"), b"x").unwrap();
        std::fs::write(directory.path().join("cherry.txt"), b"x").unwrap();
        std::fs::create_dir(directory.path().join("Zulu")).unwrap();
        // A hidden entry is the one the listing must leave out.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .attributes(FILE_ATTRIBUTE_HIDDEN)
                .open(directory.path().join("hidden.txt"))
                .unwrap();
        }
        #[cfg(not(windows))]
        std::fs::write(directory.path().join(".hidden.txt"), b"x").unwrap();

        let FolderListing::Items(items) = read_folder(directory.path()) else {
            panic!("the directory exists");
        };
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Apple", "banana", "cherry", "Zulu"],
            "the names are sorted without regard to case"
        );
        let slots: Vec<u32> = items.iter().map(|item| item.slot).collect();
        assert_eq!(slots, vec![0, 1, 2, 3], "the slots run from 0");
        let kinds: Vec<&str> = items.iter().map(|item| item.kind.as_str()).collect();
        assert_eq!(kinds, vec!["file", "file", "file", "folder"]);
    }

    /// §17.2 G.2: at most 512 entries are kept, after the sort.
    #[test]
    fn a_folder_group_stops_at_512_entries() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..(MAX_FOLDER_ITEMS + 5) {
            std::fs::write(directory.path().join(format!("item_{index:04}.txt")), b"x").unwrap();
        }
        let FolderListing::Items(items) = read_folder(directory.path()) else {
            panic!("the directory exists");
        };
        assert_eq!(items.len(), MAX_FOLDER_ITEMS);
        assert_eq!(items.first().map(|item| item.slot), Some(0));
        assert_eq!(items.last().map(|item| item.slot), Some(511));
        assert_eq!(
            items.first().map(|item| item.name.as_str()),
            Some("item_0000"),
            "the first of the sorted names stay"
        );
    }

    /// §17.2 G.2: a directory that is not there has no entries at all.
    #[test]
    fn a_folder_group_without_its_directory_has_no_entries() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            read_folder(&directory.path().join("nao_existe")),
            FolderListing::Missing
        );
    }

    /// §17.2 G.3: `recent` orders by the latest use, `most_used` by the count
    /// and then by the latest use; both copy the entry with slots from 0.
    #[test]
    fn the_use_groups_order_their_copies() {
        let mut general = group_of(0);
        general.items = vec![
            Item::new(0, "app", "A"),
            Item::new(1, "app", "B"),
            Item::new(2, "app", "C"),
        ];
        let state = usage_state(
            vec![
                general,
                dynamic_group("recent", "recent"),
                dynamic_group("most", "most_used"),
            ],
            vec![
                (("general", 0), 9, 5),
                (("general", 1), 5, 30),
                (("general", 2), 5, 20),
            ],
        );
        let recent: Vec<(String, u32)> =
            usage_group_items(&state.config, &state.restored.usage, false)
                .into_iter()
                .map(|(item, _)| (item.name, item.slot))
                .collect();
        assert_eq!(
            recent,
            vec![
                ("B".to_string(), 0),
                ("C".to_string(), 1),
                ("A".to_string(), 2)
            ],
            "the latest use comes first"
        );
        let most: Vec<String> = usage_group_items(&state.config, &state.restored.usage, true)
            .into_iter()
            .map(|(item, _)| item.name)
            .collect();
        assert_eq!(
            most,
            vec!["A".to_string(), "B".to_string(), "C".to_string()],
            "the count first, then the latest use"
        );
        let origins: Vec<(String, u32)> =
            usage_group_items(&state.config, &state.restored.usage, false)
                .into_iter()
                .map(|(_, origin)| origin)
                .collect();
        assert_eq!(
            origins,
            vec![
                ("general".to_string(), 1),
                ("general".to_string(), 2),
                ("general".to_string(), 0)
            ],
            "each copy carries where it came from"
        );
    }

    /// §17.2 G.3: at most `columns × 2` copies, never more than 32, and no copy
    /// of an entry the configuration no longer has (or of a dynamic group).
    #[test]
    fn the_use_groups_stop_at_the_limit_and_skip_gone_entries() {
        let mut general = group_of(0);
        general.items = (0..20)
            .map(|slot| Item::new(slot, "app", format!("Item {slot}")))
            .collect();
        let mut recent = dynamic_group("recent", "recent");
        // The group's own generated entry is not a candidate: its record would
        // name a copy, not an entry of the file.
        recent.items = vec![Item::new(0, "app", "Copia")];
        let mut usage = Vec::new();
        for slot in 0..20 {
            usage.push((("general", slot), 1, 100 + u64::from(slot)));
        }
        // An entry that is gone from the file, a group that is gone, and a
        // dynamic group's own record.
        usage.push((("general", 99), 50, 999));
        usage.push((("ghost", 0), 50, 999));
        usage.push((("recent", 0), 50, 999));
        let mut state = usage_state(vec![general, recent], usage);

        let items = usage_group_items(&state.config, &state.restored.usage, false);
        assert_eq!(items.len(), 16, "8 columns allow 8 × 2 copies");
        assert_eq!(items[0].0.name, "Item 19", "the newest entry comes first");
        assert!(
            items
                .iter()
                .all(|(_, origin)| origin.0 == "general" && origin.1 < 20),
            "no copy of a gone entry or of the dynamic group itself"
        );

        state.config.window.columns = 16;
        let items = usage_group_items(&state.config, &state.restored.usage, false);
        assert_eq!(items.len(), 20, "a wide window takes every remaining entry");
        let mut crowded = state;
        crowded.config.groups[0].items = (0..40u32)
            .map(|slot| Item::new(slot, "app", format!("Item {slot}")))
            .collect();
        crowded.restored.usage = (0..40u32)
            .map(|slot| {
                (
                    ("general".to_string(), slot),
                    Usage {
                        count: 1,
                        last: u64::from(slot),
                    },
                )
            })
            .collect();
        assert_eq!(
            usage_group_items(&crowded.config, &crowded.restored.usage, false).len(),
            MAX_USAGE_ITEMS,
            "no group shows more than 32 copies"
        );
    }

    /// §17.2 G.4: launching a copy records the use on the entry it came from.
    #[gpui::test]
    fn launching_a_copy_counts_the_original(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            let mut general = group_of(0);
            general.items = vec![Item::new(5, "app", "A")];
            let mut recent = dynamic_group("recent", "recent");
            recent.items = vec![Item::new(0, "app", "A")];
            install(cx, vec![general, recent]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
                state
                    .origins
                    .insert(("recent".to_string(), 0), ("general".to_string(), 5));
            }
            let item = item_at(cx, 1, 0).expect("the copy");
            send_launch(cx, item, "recent".to_string(), 0, LaunchMode::Normal);
            let pending = state(cx).and_then(|state| state.pending_usage.clone());
            assert_eq!(
                pending,
                Some(("general".to_string(), 5)),
                "the use belongs to the original entry"
            );
            if let Some(state) = state_mut(cx) {
                state.pending_usage = None;
            }
            record_usage(cx, "general", 5);
        });
        cx.run_until_parked();
        let key = KvKey::new(KV_NAMESPACE, "usage_general_5").expect("a valid usage key");
        let written = store.get(&key).expect("the test store").expect("a record");
        let usage: Usage = serde_json::from_slice(&written).expect("a usage record");
        assert_eq!(usage.count, 1, "one use of the original");
        assert!(usage.last > 0, "the use has a time");
        let copy = KvKey::new(KV_NAMESPACE, "usage_recent_0").expect("a valid usage key");
        assert_eq!(
            store.get(&copy).expect("the test store"),
            None,
            "the copy gets no record of its own"
        );
    }

    /// §17.2 G.1: the copy that is serialized leaves a dynamic group's generated
    /// entries out while the state keeps them.
    #[test]
    fn saving_leaves_generated_items_out() {
        let mut state = sample_state();
        state.config.groups[0].items = vec![Item::new(0, "app", "Escrito")];
        let mut folder = dynamic_group("tools", "folder:C:\\Tools");
        folder.items = vec![Item::new(0, "app", "Gerado")];
        state.config.groups.push(folder);

        let copy = config_for_saving(&state);
        let text = nohrs_core::config::quick_launch::to_toml(&copy).expect("the file renders");
        assert!(text.contains("name = \"Escrito\""), "{text}");
        assert!(!text.contains("Gerado"), "a generated entry is not written");
        let written = nohrs_core::config::quick_launch::parse(&text).expect("the file parses back");
        let folder = written
            .config
            .groups
            .iter()
            .find(|group| group.id == "tools")
            .expect("the dynamic group is in the file");
        assert_eq!(
            folder.source.as_deref(),
            Some("folder:C:\\Tools"),
            "the group keeps its source"
        );
        assert!(folder.items.is_empty(), "and no generated entries");
        assert_eq!(
            state.config.groups[1].items.len(),
            1,
            "the state keeps the entries it generated"
        );
    }

    /// §17.2 G.5: every edit entry refuses a dynamic group, while the group
    /// itself can still be renamed, reordered and deleted.
    #[gpui::test]
    fn every_edit_entry_refuses_a_dynamic_group(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut general = group_of(0);
            general.items = vec![Item::new(0, "app", "A"), Item::new(1, "app", "B")];
            let mut folder = dynamic_group("tools", "folder:C:\\Tools");
            folder.items = vec![Item::new(0, "app", "Gerado")];
            let mut recent = dynamic_group("recent", "recent");
            recent.items = vec![Item::new(0, "app", "Copia")];
            install(cx, vec![general, folder, recent]);

            for index in [1, 2] {
                assert!(!place_items(
                    cx,
                    index,
                    0,
                    vec![Item::new(0, "file", "Novo")]
                ));
                assert!(!put_item(cx, index, 0, Item::new(0, "file", "Novo")));
                assert!(!edit_item(cx, index, 0, "Outro", "C:\\x.exe", "", false));
                assert!(!update_item(cx, index, 0, &item_edit("C:\\x.exe")));
                assert!(!swap_slots(cx, index, 0, 1));
                assert!(!delete_item(cx, index, 0));
                assert!(
                    !move_item_to_group(cx, index, 0, 0),
                    "out of a dynamic group"
                );
                assert!(!move_item_to_group(cx, 0, 0, index), "into a dynamic group");
            }
            // The ordinary group stays editable.
            assert!(place_items(cx, 0, 2, vec![Item::new(0, "file", "Novo")]));

            // The group itself is not one of its entries: it can be renamed,
            // moved and deleted like any other.
            assert!(rename_group(cx, 1, "Ferramentas 2"));
            assert!(move_group(cx, 1, 2));
            assert!(delete_group(cx, 2, true));
        });
    }

    /// §17.2 G.7: the snapshot carries the folder group's generated entries and
    /// leaves the copies of the use groups out.
    #[gpui::test]
    fn the_launcher_snapshot_keeps_folder_groups_and_drops_use_groups(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let mut general = group_of(0);
            general.items = vec![Item::new(0, "app", "Escrito")];
            let mut folder = dynamic_group("tools", "folder:C:\\Tools");
            folder.items = vec![Item::new(0, "app", "Gerado")];
            let mut recent = dynamic_group("recent", "recent");
            recent.items = vec![Item::new(0, "app", "Copia")];
            let mut most = dynamic_group("most", "most_used");
            most.items = vec![Item::new(0, "app", "Copia2")];
            install(cx, vec![general, folder, recent, most]);

            let snapshot = launcher_snapshot(cx);
            let names: Vec<&str> = snapshot.iter().map(|entry| entry.name.as_str()).collect();
            assert_eq!(names, vec!["Escrito", "Gerado"]);
            assert_eq!(snapshot[1].group, "tools", "the folder group is in");
        });
    }

    /// §17.2 G.8: a dynamic group's records name no entry of the file, so the
    /// cleanup drops them and keeps the ordinary groups' records.
    #[gpui::test]
    fn pruning_usage_ignores_dynamic_groups(cx: &mut gpui::TestAppContext) {
        let store = TestStore::default();
        cx.update(|cx| {
            let mut general = group_of(0);
            general.items = vec![Item::new(0, "app", "Escrito")];
            let mut folder = dynamic_group("tools", "folder:C:\\Tools");
            folder.items = vec![Item::new(0, "app", "Gerado")];
            install(cx, vec![general, folder, dynamic_group("recent", "recent")]);
            if let Some(state) = state_mut(cx) {
                state.kv = Some(std::sync::Arc::new(store.clone()));
            }
            for name in ["usage_general_0", "usage_tools_0", "usage_recent_0"] {
                let key = KvKey::new(KV_NAMESPACE, name).expect("a valid key");
                store.put(&key, b"{}").expect("the test store");
            }
            prune_usage(cx);
        });
        cx.run_until_parked();
        let general = KvKey::new(KV_NAMESPACE, "usage_general_0").expect("a valid key");
        assert!(
            store.get(&general).expect("the test store").is_some(),
            "an entry of the file keeps its record"
        );
        for name in ["usage_tools_0", "usage_recent_0"] {
            let key = KvKey::new(KV_NAMESPACE, name).expect("a valid key");
            assert_eq!(
                store.get(&key).expect("the test store"),
                None,
                "a dynamic group's record goes"
            );
        }
    }

    /// §17.2 G.1, G.2: the refresh generates a folder group's entries in the
    /// background, reuses them for 30 s, and says when the directory is gone.
    #[gpui::test]
    fn refreshing_generates_folder_groups(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("beta.txt"), b"x").unwrap();
        std::fs::write(directory.path().join("Alfa.txt"), b"x").unwrap();
        let existing = format!("folder:{}", directory.path().to_string_lossy());
        let missing = format!(
            "folder:{}",
            directory.path().join("nao_existe").to_string_lossy()
        );
        cx.update(|cx| {
            install(
                cx,
                vec![
                    dynamic_group("tools", &existing),
                    dynamic_group("gone", &missing),
                ],
            );
            refresh_dynamic_groups(cx);
            refresh_dynamic_groups(cx);
        });
        cx.run_until_parked();
        let (names, slots, requests, notice) = cx.update(|cx| {
            let state = state(cx).expect("the state is installed");
            let items = state
                .config
                .groups
                .first()
                .map(|group| group.items.clone())
                .unwrap_or_default();
            (
                items
                    .iter()
                    .map(|item| item.name.clone())
                    .collect::<Vec<_>>(),
                items.iter().map(|item| item.slot).collect::<Vec<_>>(),
                state
                    .folder_refresh
                    .get("tools")
                    .map(|refresh| refresh.request)
                    .unwrap_or_default(),
                group_notice(cx, 1).map(str::to_string),
            )
        });
        assert_eq!(names, vec!["Alfa".to_string(), "beta".to_string()]);
        assert_eq!(slots, vec![0, 1], "the slots run from 0");
        assert_eq!(
            requests, 1,
            "the second refresh within 30 s reuses the listing"
        );
        assert_eq!(
            notice,
            Some(nohrs_core::t!("Pasta não encontrada").to_string())
        );
    }

    /// §17.2 G.3: the resident process has no store, so its use groups have
    /// nothing to show and say why.
    #[gpui::test]
    fn the_resident_process_has_no_use_groups(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut recent = dynamic_group("recent", "recent");
            recent.items = vec![Item::new(0, "app", "Copia")];
            install(cx, vec![recent]);
            if let Some(state) = state_mut(cx) {
                state.mode = Mode::Standalone;
            }
            refresh_dynamic_groups(cx);
            let state = state(cx).expect("the state is installed");
            assert!(
                state.config.groups[0].items.is_empty(),
                "a resident process has no use records to copy"
            );
            assert_eq!(
                group_notice(cx, 0),
                Some(nohrs_core::t!("Indisponível no modo residente"))
            );
        });
    }
}
