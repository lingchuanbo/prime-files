//! Thumbnails for the grid and the small icons of the list: extracted on
//! worker threads, held per pane for the directory on screen, and handed to
//! GPUI as ready-made `RenderImage`s.

use gpui::{App, Context, RenderImage, Task};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::ExplorerPane;

/// Workers answering what is quick: icons, and thumbnails Windows already
/// holds in its cache (about 10 ms each, measured on a Google Drive folder).
const FAST_WORKERS: usize = 2;
/// Workers generating thumbnails the cache does not have yet. Each one reads
/// the file, which on Google Drive means fetching it over the network: 2 to
/// 4 s per video, mostly waiting. They used to share one queue with the icons,
/// so a folder of videos left even the list's icons waiting behind them for
/// about a minute; apart, a slow file holds up only other slow files.
const SLOW_WORKERS: usize = 4;

/// Pixels of thumbnails one pane keeps in memory, as RGBA bytes. Each one
/// also has a copy in the GPU's sprite atlas, so this caps both. Before the
/// cap a folder of 10,000 photos scrolled end to end in the grid kept every
/// tile: at the 256 px bucket that is 2.6 GB. A screen of the largest tiles
/// is about 100 of them, 26 MB, so this holds several screens of scrolling
/// back and forth before anything is read again.
/// The bytes of `Ready` thumbnails the whole process may hold (plan §2 O3).
///
/// One budget for every pane, every tab and the sidebar, instead of one per
/// `Thumbnails`: two windows can no longer add up to several times one pane's
/// worth. The figure is BoBO's (2026-11-23).
const SHARED_READY_BYTES_CAP: usize = 512 * 1024 * 1024;

/// What every [`Thumbnails`] in the process holds together, in bytes.
///
/// Bumped and lowered by the same paths that keep `Thumbnails::ready_bytes`, so a
/// cache that finds the budget over knows how much there is without walking the
/// other caches. Tests that set `ready_bytes` by hand do not touch it, so the
/// per-cache `cap` tests keep meaning what they did.
static READY_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Lowers the shared ledger by `bytes`, never below zero: the tests set
/// `ready_bytes` by hand, and those bytes were never added.
fn release_shared(bytes: usize) {
    let mut held = READY_BYTES.load(Ordering::Relaxed);
    loop {
        let next = held.saturating_sub(bytes);
        match READY_BYTES.compare_exchange_weak(held, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(current) => held = current,
        }
    }
}

/// How many times an answer that is not the real picture is asked for again,
/// and how long the first wait is; each wait is four times the one before
/// (15 s, 1 min, 4 min). A video whose thumbnail Windows could not make right
/// then (the file still syncing from Drive, a busy decoder) gets it on a later
/// look; a type that has no thumbnail at all costs three quick failures.
const RETRIES: u8 = 3;
const FIRST_RETRY: Duration = Duration::from_secs(15);

enum Entry {
    /// Asked for; `shown` is what the slot showed before, kept on screen
    /// while it is asked for again.
    Loading {
        #[allow(dead_code)]
        task: Task<()>,
        shown: Option<Arc<RenderImage>>,
    },
    Ready(Arc<RenderImage>),
    Failed,
}

impl Entry {
    /// The image this entry keeps in memory, whatever its state.
    fn held(&self) -> Option<&Arc<RenderImage>> {
        match self {
            Entry::Ready(image) => Some(image),
            Entry::Loading { shown, .. } => shown.as_ref(),
            Entry::Failed => None,
        }
    }
}

/// A worker's answer. `settled` is false when it is a stand-in — the type's
/// icon where the picture could not be made, or nothing — so the slot asks
/// again later instead of keeping it for good.
struct Answer {
    image: Option<Arc<RenderImage>>,
    settled: bool,
    /// True when the image is the icon every file of its class shares: it is not
    /// this path's own, so it is neither counted in the budget nor dropped when
    /// its row goes (the other rows use the same texture).
    shared: bool,
}

/// What is extracted for a file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    /// The picture itself for images and videos, the icon for anything else.
    Thumbnail,
    /// The file's icon, as Explorer's small-icon and details views show it:
    /// the program's own for an .exe, the associated app's for a document.
    Icon,
}

/// One kind and size asked for one path.
struct Slot {
    kind: Kind,
    size: u32,
    /// Whether the path is a folder, to ask the same way again.
    is_dir: bool,
    entry: Entry,
    /// The frame that last asked for it (`Thumbnails::frame`).
    last_seen: u64,
    /// When to ask again, for an answer that was a stand-in.
    retry_at: Option<Instant>,
    /// How many times it has been asked again.
    retries: u8,
    /// Whether what is held is the picture itself rather than a stand-in
    /// (the type's icon). The preview only takes the picture; past the
    /// retries a stand-in stays `Ready`, so `retry_at` cannot tell.
    settled: bool,
    /// Whether the icon held is the class-shared one (see [`Answer::shared`]).
    shared: bool,
}

/// Whether what `slot` holds is this path's own and so counts towards the budget:
/// every thumbnail, and the icons no class shares (folders, `.exe`, `.lnk`, and
/// the extensions whose icon differs per file). The class-shared icons stay out:
/// they live in `Service::class_icons` and are bounded by the extensions met.
fn counts_towards_budget(slot: &Slot) -> bool {
    match slot.kind {
        Kind::Thumbnail => true,
        Kind::Icon => !slot.shared,
    }
}

/// The Shell's picture of a file at one size, for the preview
/// ([`Thumbnails::picture`]).
pub enum Picture {
    /// The picture at that size.
    Shown(Arc<RenderImage>),
    /// Still being made; a smaller size of it, when one is held, to show
    /// meanwhile.
    Waiting(Option<Arc<RenderImage>>),
    /// The Shell has no picture of it — a format Windows has no codec for.
    Unavailable,
}

/// The thumbnails one pane has asked for, by path, then kind and size. The
/// path comes first so a lookup is one hash of the path the row already has,
/// and "a smaller size of the same picture" looks at that path's few slots
/// instead of every thumbnail held: that scan used to run for every tile still
/// loading, on every frame.
pub struct Thumbnails {
    directory: String,
    entries: HashMap<String, Vec<Slot>>,
    /// Counts [`Self::begin_frame`] calls; a slot not asked for during the
    /// last two left the screen.
    frame: u64,
    /// Bytes of the `Ready` thumbnails held (icons are not counted).
    ready_bytes: usize,
    /// How many slots are `Loading`, so a frame with none skips the sweep.
    loading: usize,
    /// The cache's own ceiling, in bytes: unbounded in production, where the
    /// shared budget is the limit, and small in the tests that reach it with
    /// 64×64 pixels.
    cap: usize,
    /// The ceiling the whole process shares, in bytes; production uses
    /// [`SHARED_READY_BYTES_CAP`], and a test lowers it to watch two caches
    /// compete for one budget.
    shared_cap: usize,
}

impl Default for Thumbnails {
    fn default() -> Self {
        Self {
            directory: String::new(),
            entries: HashMap::new(),
            frame: 0,
            ready_bytes: 0,
            loading: 0,
            cap: usize::MAX,
            shared_cap: SHARED_READY_BYTES_CAP,
        }
    }
}

impl Drop for Thumbnails {
    fn drop(&mut self) {
        // A cache going away (a tab closed, a window closed) has nobody left to
        // release its bytes: without this the shared budget would stay spent for
        // the rest of the process. The images themselves are dropped with the
        // entries; the GPU atlas copies are the pane's business, as before.
        release_shared(self.ready_bytes);
        self.ready_bytes = 0;
    }
}

impl Thumbnails {
    /// Adds `bytes` of a newly held thumbnail to this cache and to the shared
    /// ledger.
    fn add_ready(&mut self, bytes: usize) {
        self.ready_bytes = self.ready_bytes.saturating_add(bytes);
        READY_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Lets `bytes` of held thumbnails go, in this cache and in the ledger. The
    /// ledger is clamped at zero: tests that set `ready_bytes` by hand never put
    /// those bytes in it.
    fn release_ready(&mut self, bytes: usize) {
        self.ready_bytes = self.ready_bytes.saturating_sub(bytes);
        release_shared(bytes);
    }

    /// Releases everything the last frame did not draw, keeping what the tab was
    /// showing (R1 of the memory review).
    ///
    /// A tab that is not in front never calls [`Self::begin_frame`], so without
    /// this its thumbnails stayed in the shared budget for good and the tab in
    /// front paid for them, evicting its own off-screen tiles every time it scrolled
    /// past them. A background tab now holds one screen, which is what makes it
    /// come back at once.
    pub fn trim_to_last_frame(&mut self, cx: &mut App) {
        let frame = self.frame;
        let mut released = 0;
        let mut cancelled = 0;
        for slots in self.entries.values_mut() {
            slots.retain(|slot| {
                if slot.last_seen == frame {
                    return true;
                }
                if matches!(slot.entry, Entry::Loading { .. }) {
                    cancelled += 1;
                }
                if counts_towards_budget(slot)
                    && let Some(image) = slot.entry.held()
                {
                    released += image_bytes(image);
                    cx.drop_image(image.clone(), None);
                }
                false
            });
        }
        self.entries.retain(|_, slots| !slots.is_empty());
        self.loading = self.loading.saturating_sub(cancelled);
        self.release_ready(released);
    }

    /// Forgets everything held for another directory. Dropping a `Loading`
    /// entry drops its task, which closes the reply channel, so a worker that
    /// has not reached the job yet skips it rather than decoding for nobody.
    pub fn show_directory(&mut self, directory: &str, cx: &mut App) {
        if self.directory == directory {
            return;
        }
        self.directory = directory.to_string();
        let mut released = 0;
        for (_, slots) in self.entries.drain() {
            for slot in slots {
                // The sprite atlas keeps its own copy of every image it has
                // drawn; without this each folder visited would stay in GPU
                // memory.
                if counts_towards_budget(&slot)
                    && let Some(image) = slot.entry.held()
                {
                    released += image_bytes(image);
                    cx.drop_image(image.clone(), None);
                }
            }
        }
        self.release_ready(released);
        self.ready_bytes = 0;
        self.loading = 0;
    }

    /// Starts a frame of the pane that draws from this cache. What the
    /// previous frame did not ask for is no longer on screen: if it is still
    /// waiting for a worker it is cancelled, so scrolling fast through a
    /// folder of videos does not leave the slow workers generating, for
    /// seconds each, tiles that went by (OTIMIZACAO.md §5.3). And once the
    /// thumbnails held pass [`SHARED_READY_BYTES_CAP`], the ones off screen longest
    /// go, down to three quarters of it so the next tile does not evict again.
    pub fn begin_frame(&mut self, cx: &mut App) {
        self.frame += 1;
        let frame = self.frame;
        let gone = |slot: &Slot| slot.last_seen + 1 < frame;

        if self.loading > 0 {
            let mut cancelled = 0;
            let mut released = 0;
            for slots in self.entries.values_mut() {
                slots.retain(|slot| {
                    let cancel = matches!(slot.entry, Entry::Loading { .. }) && gone(slot);
                    if cancel {
                        cancelled += 1;
                        if counts_towards_budget(slot)
                            && let Some(image) = slot.entry.held()
                        {
                            released += image_bytes(image);
                            cx.drop_image(image.clone(), None);
                        }
                    }
                    !cancel
                });
            }
            self.loading -= cancelled;
            self.release_ready(released);
            self.entries.retain(|_, slots| !slots.is_empty());
        }

        // Two ceilings: the cache's own (only the tests set it) and the budget
        // the whole process shares (§2 O3). Over the shared one, each cache lets
        // go of what it has off screen, oldest first; a cache with nothing off
        // screen waits for the next frame or for another cache's eviction, which
        // is the "if still over, next frame" the plan asks for instead of a
        // cross-cache LRU.
        let own_target = self.cap / 4 * 3;
        let shared_target = self.shared_cap / 4 * 3;
        let over_own = self.ready_bytes > self.cap;
        let over_shared = READY_BYTES.load(Ordering::Relaxed) > self.shared_cap;
        if over_own || over_shared {
            let held_before = READY_BYTES.load(Ordering::Relaxed);
            let mut candidates: Vec<(u64, String, Kind, u32)> = self
                .entries
                .iter()
                .flat_map(|(path, slots)| {
                    slots
                        .iter()
                        .filter(move |slot| {
                            counts_towards_budget(slot)
                                && matches!(slot.entry, Entry::Ready(_))
                                && gone(slot)
                        })
                        .map(move |slot| (slot.last_seen, path.clone(), slot.kind, slot.size))
                })
                .collect();
            candidates.sort_unstable_by_key(|candidate| candidate.0);
            for (_, path, kind, size) in candidates {
                if self.ready_bytes <= own_target
                    && READY_BYTES.load(Ordering::Relaxed) <= shared_target
                {
                    break;
                }
                let mut released = 0;
                let mut empty = false;
                if let Some(slots) = self.entries.get_mut(&path) {
                    if let Some(at) = slots
                        .iter()
                        .position(|slot| slot.kind == kind && slot.size == size)
                        && let Entry::Ready(image) = slots.swap_remove(at).entry
                    {
                        // Only counted slots are candidates, and those hold this
                        // path's own texture.
                        released = image_bytes(&image);
                        cx.drop_image(image, None);
                    }
                    empty = slots.is_empty();
                }
                self.release_ready(released);
                if empty {
                    self.entries.remove(&path);
                }
            }
            if over_shared {
                tracing::info!(
                    before = held_before,
                    after = READY_BYTES.load(Ordering::Relaxed),
                    cap = self.shared_cap,
                    "the shared thumbnail budget evicted"
                );
            }
        }
    }

    /// Forgets what is held for `path`, so the next frame asks again. For a
    /// file that changed on disk: the cache is by path, and without this a
    /// file overwritten in place kept the old picture for as long as the
    /// folder stayed open (OTIMIZACAO.md §6.1). Thumbnails are the file's own
    /// and their textures go; an icon may be one texture shared by every file
    /// of the type, so only the slot goes, as in `begin_frame`.
    pub fn forget(&mut self, path: &str, cx: &mut App) {
        let Some(slots) = self.entries.remove(path) else {
            return;
        };
        for slot in slots {
            if matches!(slot.entry, Entry::Loading { .. }) {
                self.loading -= 1;
            }
            if counts_towards_budget(&slot)
                && let Some(image) = slot.entry.held()
            {
                self.release_ready(image_bytes(image));
                cx.drop_image(image.clone(), None);
            }
        }
    }

    /// Whether anything is held or on its way for `path`.
    #[cfg(test)]
    pub(crate) fn holds(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    /// The thumbnail of `path` at `size`, asking for it if it was never asked
    /// for. While a larger size is on its way a smaller one already held is
    /// returned, so zooming in does not flash every tile back to an icon.
    pub fn get(
        &mut self,
        path: &str,
        size: u32,
        cx: &mut Context<ExplorerPane>,
    ) -> Option<Arc<RenderImage>> {
        self.fetch(
            path,
            Kind::Thumbnail,
            false,
            size,
            |pane| &mut pane.thumbnails,
            cx,
        )
    }

    /// The Shell's picture of `path` fitted in `size` pixels, for the
    /// preview: like [`Self::get`], but saying when the Shell has no picture,
    /// where `get` would hand over the type's icon in its place.
    pub fn picture(&mut self, path: &str, size: u32, cx: &mut Context<ExplorerPane>) -> Picture {
        let held = self.get(path, size, cx);
        self.picture_of(path, size, held)
    }

    // What `picture` answers, given what `get` handed over.
    fn picture_of(&self, path: &str, size: u32, held: Option<Arc<RenderImage>>) -> Picture {
        let Some(slot) = self.entries.get(path).and_then(|slots| {
            slots
                .iter()
                .find(|slot| slot.kind == Kind::Thumbnail && slot.size == size)
        }) else {
            return Picture::Waiting(held);
        };
        match &slot.entry {
            Entry::Ready(image) if slot.settled => Picture::Shown(image.clone()),
            Entry::Ready(_) | Entry::Failed => Picture::Unavailable,
            // A smaller size is shown meanwhile only if it is the picture: a
            // stand-in would flash the icon before the photo.
            Entry::Loading { .. } => Picture::Waiting(held.filter(|_| {
                self.entries[path]
                    .iter()
                    .any(|slot| slot.kind == Kind::Thumbnail && slot.size < size && slot.settled)
            })),
        }
    }

    /// The Shell icon of `path`, `size` pixels square, for a list row.
    pub fn icon(
        &mut self,
        path: &str,
        is_dir: bool,
        size: u32,
        cx: &mut Context<ExplorerPane>,
    ) -> Option<Arc<RenderImage>> {
        self.fetch(
            path,
            Kind::Icon,
            is_dir,
            size,
            |pane| &mut pane.thumbnails,
            cx,
        )
    }

    /// Like [`Self::icon`], for a cache held elsewhere in the pane: `slot`
    /// finds it again when the icon arrives.
    pub fn icon_in(
        &mut self,
        path: &str,
        is_dir: bool,
        size: u32,
        slot: fn(&mut ExplorerPane) -> &mut Thumbnails,
        cx: &mut Context<ExplorerPane>,
    ) -> Option<Arc<RenderImage>> {
        self.fetch(path, Kind::Icon, is_dir, size, slot, cx)
    }

    fn fetch(
        &mut self,
        path: &str,
        kind: Kind,
        is_dir: bool,
        size: u32,
        cache: fn(&mut ExplorerPane) -> &mut Thumbnails,
        cx: &mut Context<ExplorerPane>,
    ) -> Option<Arc<RenderImage>> {
        let frame = self.frame;
        if !self.entries.contains_key(path) {
            self.entries.insert(path.to_string(), Vec::new());
        }
        let slots = self.entries.get_mut(path)?;
        match slots
            .iter_mut()
            .find(|slot| slot.kind == kind && slot.size == size)
        {
            Some(slot) => {
                slot.last_seen = frame;
                if slot.retry_at.is_some_and(|at| Instant::now() >= at) {
                    slot.retry_at = None;
                    let shown = match std::mem::replace(&mut slot.entry, Entry::Failed) {
                        Entry::Ready(image) => Some(image),
                        _ => None,
                    };
                    slot.entry = Entry::Loading {
                        task: request(path, kind, slot.is_dir, size, cache, cx),
                        shown,
                    };
                    self.loading += 1;
                }
                match &slot.entry {
                    Entry::Ready(image) => return Some(image.clone()),
                    Entry::Failed => return None,
                    Entry::Loading {
                        shown: Some(image), ..
                    } => return Some(image.clone()),
                    Entry::Loading { shown: None, .. } => {}
                }
            }
            None => {
                slots.push(Slot {
                    kind,
                    size,
                    is_dir,
                    entry: Entry::Loading {
                        task: request(path, kind, is_dir, size, cache, cx),
                        shown: None,
                    },
                    last_seen: frame,
                    retry_at: None,
                    retries: 0,
                    settled: false,
                    shared: false,
                });
                self.loading += 1;
            }
        }
        slots
            .iter_mut()
            .filter(|slot| {
                slot.kind == kind && slot.size < size && matches!(slot.entry, Entry::Ready(_))
            })
            .max_by_key(|slot| slot.size)
            .and_then(|slot| {
                slot.last_seen = frame;
                match &slot.entry {
                    Entry::Ready(image) => Some(image.clone()),
                    _ => None,
                }
            })
    }

    fn arrived(&mut self, path: &str, kind: Kind, size: u32, answer: Answer, cx: &mut App) {
        // A reply for a slot no longer held belongs to a directory that was
        // left, or a tile that scrolled away, while it was in flight.
        let Some(slot) = self.entries.get_mut(path).and_then(|slots| {
            slots
                .iter_mut()
                .find(|slot| slot.kind == kind && slot.size == size)
        }) else {
            return;
        };
        let shown = match std::mem::replace(&mut slot.entry, Entry::Failed) {
            Entry::Loading { shown, .. } => shown,
            other => {
                slot.entry = other;
                return;
            }
        };
        self.loading -= 1;
        // A stand-in that comes back empty keeps the one already on screen.
        let image = answer.image.or_else(|| shown.clone());
        // An icon that every file of its class shares is not this path's own:
        // it is not counted, and its texture stays for the other rows.
        let counted = kind == Kind::Thumbnail || (kind == Kind::Icon && !answer.shared);
        let mut released = 0;
        let mut added = 0;
        if counted {
            if let Some(shown) = &shown {
                released = image_bytes(shown);
                if image
                    .as_ref()
                    .is_none_or(|image| !Arc::ptr_eq(image, shown))
                {
                    cx.drop_image(shown.clone(), None);
                }
            }
            if let Some(image) = &image {
                added = image_bytes(image);
            }
        }
        slot.settled = answer.settled && image.is_some();
        slot.shared = answer.shared && image.is_some();
        slot.entry = image.map_or(Entry::Failed, Entry::Ready);
        if !answer.settled && slot.retries < RETRIES {
            slot.retry_at = Some(Instant::now() + retry_delay(slot.retries));
            slot.retries += 1;
        }
        self.release_ready(released);
        self.add_ready(added);
    }
}

/// How long to wait before asking again for the `retries`-th time.
fn retry_delay(retries: u8) -> Duration {
    FIRST_RETRY * 4u32.pow(u32::from(retries))
}

/// Puts the job on a worker's queue, and the answer in the slot when it comes.
fn request(
    path: &str,
    kind: Kind,
    is_dir: bool,
    size: u32,
    cache: fn(&mut ExplorerPane) -> &mut Thumbnails,
    cx: &mut Context<ExplorerPane>,
) -> Task<()> {
    let reply = service().request(path.to_string(), kind, is_dir, size);
    let path = path.to_string();
    cx.spawn(async move |this, cx| {
        // A closed channel means the worker dropped the job: no answer, and
        // nothing settled.
        let answer = reply.recv().await.unwrap_or(Answer {
            image: None,
            settled: false,
            shared: false,
        });
        this.update(cx, |pane, cx| {
            cache(pane).arrived(&path, kind, size, answer, cx);
            cx.notify();
        })
        .ok();
    })
}

fn image_bytes(image: &RenderImage) -> usize {
    (0..image.frame_count())
        .map(|frame| {
            let size = image.size(frame);
            size.width.0.max(0) as usize * size.height.0.max(0) as usize * 4
        })
        .sum()
}

struct Job {
    path: String,
    kind: Kind,
    /// The extension when every file of it shows the same icon, so one
    /// extraction answers them all (`Service::class_icons`).
    class: Option<String>,
    size: u32,
    reply: async_channel::Sender<Answer>,
}

// A stack of jobs and the workers waiting on it.
#[derive(Default)]
struct Queue {
    jobs: Mutex<Vec<Job>>,
    available: Condvar,
}

impl Queue {
    fn push(&self, job: Job) {
        match self.jobs.lock() {
            Ok(mut jobs) => jobs.push(job),
            Err(error) => tracing::error!("thumbnail queue poisoned: {error}"),
        }
        self.available.notify_one();
    }

    // The next job someone still waits for, blocking until there is one;
    // `None` once the queue is unusable.
    fn next(&self) -> Option<Job> {
        let mut jobs = self.jobs.lock().ok()?;
        loop {
            // Newest first: the tiles asked for last are the ones the user
            // has scrolled to, while the oldest may be far off screen by now.
            while let Some(job) = jobs.pop() {
                if !job.reply.is_closed() {
                    return Some(job);
                }
            }
            jobs = self.available.wait(jobs).ok()?;
        }
    }
}

#[derive(Default)]
struct Service {
    fast: Queue,
    slow: Queue,
    /// Icons that depend only on the extension, by extension and size. A
    /// folder of 10,000 .wav asks the Shell once, not 10,000 times, and the
    /// rows share one image, so the GPU holds one texture for them
    /// (OTIMIZACAO.md §6.11). Bounded by the extensions met in a session.
    class_icons: Mutex<HashMap<(String, u32), Arc<RenderImage>>>,
    /// Whether an extension's icon is the same for every file of it.
    per_class: Mutex<HashMap<String, bool>>,
}

fn service() -> &'static Service {
    static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();
    SERVICE.get_or_init(|| {
        let service = Arc::new(Service::default());
        let workers = (0..FAST_WORKERS)
            .map(|index| (format!("nohrs-thumbnails-fast-{index}"), true))
            .chain((0..SLOW_WORKERS).map(|index| (format!("nohrs-thumbnails-{index}"), false)));
        for (name, fast) in workers {
            let service = service.clone();
            let spawned = std::thread::Builder::new().name(name).spawn(move || {
                if fast {
                    service.work_fast()
                } else {
                    service.work_slow()
                }
            });
            if let Err(error) = spawned {
                tracing::error!("could not start a thumbnail worker: {error}");
            }
        }
        service
    })
}

/// The extension of a file named `path`, lowercase, if it has one.
fn extension_of(path: &str) -> Option<String> {
    let name = path.rsplit(['\\', '/']).next()?;
    let (stem, extension) = name.rsplit_once('.')?;
    (!stem.is_empty() && !extension.is_empty()).then(|| extension.to_lowercase())
}

impl Service {
    fn request(
        &self,
        path: String,
        kind: Kind,
        is_dir: bool,
        size: u32,
    ) -> async_channel::Receiver<Answer> {
        let (reply, receiver) = async_channel::bounded(1);
        // Folders keep their own: Documents, Downloads and a folder with a
        // desktop.ini each show a different one.
        let class = (kind == Kind::Icon && !is_dir)
            .then(|| extension_of(&path))
            .flatten();
        // Every job starts on the fast queue; a thumbnail the cache cannot
        // answer moves on to the slow one from there.
        self.fast.push(Job {
            path,
            kind,
            class,
            size,
            reply,
        });
        receiver
    }

    /// The icon shared by every file of `class`, if that is how the class
    /// works and it was already extracted. Asked on the worker, so the
    /// registry is read off the UI thread, once per extension.
    fn class_icon(&self, class: &str, size: u32) -> Option<Arc<RenderImage>> {
        let per_class = {
            let mut known = self.per_class.lock().ok()?;
            *known
                .entry(class.to_owned())
                .or_insert_with(|| platform::icon_is_per_class(class))
        };
        if !per_class {
            return None;
        }
        self.class_icons
            .lock()
            .ok()?
            .get(&(class.to_owned(), size))
            .cloned()
    }

    fn remember_class_icon(&self, class: &str, size: u32, image: &Arc<RenderImage>) -> bool {
        let per_class = self
            .per_class
            .lock()
            .ok()
            .and_then(|known| known.get(class).copied())
            .unwrap_or(false);
        if per_class && let Ok(mut icons) = self.class_icons.lock() {
            icons.insert((class.to_owned(), size), image.clone());
            return true;
        }
        false
    }

    fn work_fast(&self) {
        platform::init_thread();
        while let Some(job) = self.fast.next() {
            if let Some(class) = &job.class
                && let Some(image) = self.class_icon(class, job.size)
            {
                job.reply
                    .try_send(Answer {
                        image: Some(image),
                        settled: true,
                        shared: true,
                    })
                    .ok();
                continue;
            }
            let mode = match job.kind {
                Kind::Icon => Mode::Icon,
                Kind::Thumbnail => Mode::Cached,
            };
            match platform::extract(&job.path, job.size, mode) {
                // The pane may have moved on meanwhile; nothing is waiting then.
                Some(image) => {
                    let image = Arc::new(image);
                    // The first extraction of a class becomes the shared icon as
                    // well, so this row must not count or drop it either.
                    let shared = job
                        .class
                        .as_ref()
                        .is_some_and(|class| self.remember_class_icon(class, job.size, &image));
                    job.reply
                        .try_send(Answer {
                            image: Some(image),
                            settled: true,
                            shared,
                        })
                        .ok();
                }
                None if job.kind == Kind::Thumbnail => self.slow.push(job),
                None => {
                    job.reply
                        .try_send(Answer {
                            image: None,
                            settled: false,
                            shared: false,
                        })
                        .ok();
                }
            }
        }
    }

    fn work_slow(&self) {
        platform::init_thread();
        while let Some(job) = self.slow.next() {
            let answer = match platform::extract(&job.path, job.size, Mode::Generate) {
                Some(image) => Answer {
                    image: Some(Arc::new(image)),
                    settled: true,
                    shared: false,
                },
                // No picture: the type's icon stands in, and the slot asks
                // again later, in case it was the moment and not the file.
                None => Answer {
                    image: platform::extract(&job.path, job.size, Mode::Icon).map(Arc::new),
                    settled: false,
                    shared: false,
                },
            };
            job.reply.try_send(answer).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(edge: u32) -> Arc<RenderImage> {
        Arc::new(render_image(edge, edge, vec![0; (edge * edge * 4) as usize]).unwrap())
    }

    fn slot(kind: Kind, size: u32, entry: Entry, last_seen: u64) -> Slot {
        Slot {
            kind,
            size,
            is_dir: false,
            entry,
            last_seen,
            retry_at: None,
            retries: 0,
            settled: false,
            shared: false,
        }
    }

    /// A cache with its two ceilings set by the test. `Thumbnails` has a `Drop`,
    /// so a struct update cannot move one out of `Default::default()`.
    fn cache_with_caps(cap: usize, shared_cap: usize) -> Thumbnails {
        let mut cache = Thumbnails::default();
        cache.cap = cap;
        cache.shared_cap = shared_cap;
        cache
    }

    fn loading() -> Entry {
        Entry::Loading {
            task: Task::ready(()),
            shown: None,
        }
    }

    #[gpui::test]
    fn a_paths_own_icon_counts_and_a_shared_one_does_not(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut cache = Thumbnails::default();
            // A folder's icon (or an .exe's) belongs to that path alone.
            cache
                .entries
                .insert("own".into(), vec![slot(Kind::Icon, 16, loading(), 0)]);
            cache.loading += 1;
            cache.arrived(
                "own",
                Kind::Icon,
                16,
                Answer {
                    image: Some(image(16)),
                    settled: true,
                    shared: false,
                },
                cx,
            );
            assert_eq!(cache.ready_bytes, 16 * 16 * 4);
            // One that every .txt shares is not this path's own: it lives in the
            // service's class map and would be counted once per file otherwise.
            cache
                .entries
                .insert("shared".into(), vec![slot(Kind::Icon, 16, loading(), 0)]);
            cache.loading += 1;
            cache.arrived(
                "shared",
                Kind::Icon,
                16,
                Answer {
                    image: Some(image(16)),
                    settled: true,
                    shared: true,
                },
                cx,
            );
            assert_eq!(
                cache.ready_bytes,
                16 * 16 * 4,
                "the class-shared icon must not count"
            );
            assert!(cache.entries["shared"][0].shared);
        });
    }

    #[gpui::test]
    fn a_tab_going_background_keeps_only_what_its_last_frame_drew(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut cache = Thumbnails::default();
            cache.frame = 10;
            // One tile drawn in the last frame, one the frame before, and one still
            // loading that the scroll left behind.
            for (path, seen, entry) in [
                ("now", 10, loading()),
                ("before", 9, loading()),
                ("scrolled", 2, loading()),
            ] {
                cache
                    .entries
                    .insert(path.into(), vec![slot(Kind::Thumbnail, 64, entry, seen)]);
                cache.loading += 1;
                cache.arrived(
                    path,
                    Kind::Thumbnail,
                    64,
                    Answer {
                        image: Some(image(64)),
                        settled: true,
                        shared: false,
                    },
                    cx,
                );
            }

            cache.trim_to_last_frame(cx);

            assert_eq!(cache.entries.len(), 1, "only the last frame's tile stays");
            assert!(cache.entries.contains_key("now"));
            assert_eq!(cache.ready_bytes, 64 * 64 * 4);
            assert_eq!(cache.loading, 0, "the pending tile was cancelled");
        });
    }

    #[gpui::test]
    fn the_shared_budget_caps_every_cache_together(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            // Two caches of 48 KB each against one 64 KB budget: together they are
            // over, and neither is over on its own.
            let shared = 64 * 1024;
            let mut first = cache_with_caps(usize::MAX, shared);
            let mut second = cache_with_caps(usize::MAX, shared);
            for (cache, name) in [(&mut first, "first"), (&mut second, "second")] {
                for index in 0..3 {
                    let path = format!("{name}-{index}");
                    cache
                        .entries
                        .insert(path.clone(), vec![slot(Kind::Thumbnail, 64, loading(), 0)]);
                    cache.loading += 1;
                    cache.arrived(
                        &path,
                        Kind::Thumbnail,
                        64,
                        Answer {
                            image: Some(image(64)),
                            settled: true,
                            shared: false,
                        },
                        cx,
                    );
                }
            }
            let together =
                |first: &Thumbnails, second: &Thumbnails| first.ready_bytes + second.ready_bytes;
            assert_eq!(together(&first, &second), 6 * 64 * 64 * 4);
            // What was added above is what the frame before this one did not ask
            // for, which is what makes it evictable.
            first.frame = 10;
            second.frame = 10;

            // Each frame lets go of what its own cache has off screen until the
            // shared figure is back under three quarters of the budget.
            first.begin_frame(cx);
            second.begin_frame(cx);
            assert!(
                together(&first, &second) <= shared,
                "the two caches still hold {} bytes against a shared budget of {shared}",
                together(&first, &second)
            );
        });
    }

    #[gpui::test]
    fn what_left_the_screen_is_cancelled_and_the_oldest_thumbnails_go_past_the_cap(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let mut cache = cache_with_caps(3 * 64 * 64 * 4, SHARED_READY_BYTES_CAP);
            cache.frame = 10;
            // Seen in the frame that just ended, the one before it, and long
            // ago.
            for (path, seen) in [("now", 10), ("before", 9), ("old", 2), ("older", 1)] {
                cache.entries.insert(
                    path.into(),
                    vec![
                        slot(Kind::Thumbnail, 64, Entry::Ready(image(64)), seen),
                        slot(Kind::Thumbnail, 256, loading(), seen),
                    ],
                );
                cache.ready_bytes += 64 * 64 * 4;
                cache.loading += 1;
            }
            // Icons shared by a whole extension are not this path's own: never
            // counted, never evicted.
            let mut class_icon = slot(Kind::Icon, 16, Entry::Ready(image(16)), 0);
            class_icon.shared = true;
            cache.entries.insert("icon".into(), vec![class_icon]);

            cache.begin_frame(cx);

            let held = |cache: &Thumbnails, path: &str| -> Vec<(u32, bool)> {
                cache.entries.get(path).map_or_else(Vec::new, |slots| {
                    slots
                        .iter()
                        .map(|slot| (slot.size, matches!(slot.entry, Entry::Ready(_))))
                        .collect()
                })
            };
            // On screen in the frame that ended: nothing touched.
            assert_eq!(held(&cache, "now"), [(64, true), (256, false)]);
            // Off screen: the pending request is cancelled. Four thumbnails
            // against a cap of three: the least recently seen go first, until
            // no more than three quarters of the cap are left.
            assert_eq!(held(&cache, "before"), [(64, true)]);
            assert_eq!(held(&cache, "old"), []);
            assert_eq!(held(&cache, "older"), []);
            assert_eq!(held(&cache, "icon"), [(16, true)]);
            assert_eq!(cache.loading, 1);
            assert_eq!(cache.ready_bytes, 2 * 64 * 64 * 4);
        });
    }

    // A stand-in (the type's icon where the picture could not be made) is
    // shown but not kept for good: the slot asks again later, a bounded number
    // of times, and the real picture replaces it when it comes.
    // The preview draws a fitted image from the Shell's picture and falls back
    // to decoding the file only when the Shell has none, so the type's icon
    // in its place has to read as "none" — also once the retries are spent and
    // it stays `Ready` for good — and never be what is shown meanwhile.
    #[gpui::test]
    fn the_preview_takes_the_picture_and_never_the_stand_in(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut cache = Thumbnails::default();
            cache.entries.insert("foto.webp".into(), vec![slot(Kind::Thumbnail, 1024, loading(), 0)]);
            cache.loading = 1;
            assert!(matches!(cache.picture_of("foto.webp", 1024, None), Picture::Waiting(None)));

            let icon = image(64);
            cache.arrived(
                "foto.webp",
                Kind::Thumbnail,
                1024,
                Answer {
                    image: Some(icon.clone()),
                    settled: false,
                    shared: false,
                },
                cx,
            );
            assert!(matches!(cache.picture_of("foto.webp", 1024, Some(icon.clone())), Picture::Unavailable));
            let slot = &mut cache.entries.get_mut("foto.webp").unwrap()[0];
            slot.retry_at = None;
            slot.retries = RETRIES;
            assert!(
                matches!(cache.picture_of("foto.webp", 1024, Some(icon.clone())), Picture::Unavailable),
                "past the retries the icon stays Ready, and is still not the picture"
            );

            let photo = image(64);
            cache.entries.insert("foto.jpg".into(), vec![slot_of(Kind::Thumbnail, 1024, loading())]);
            cache.loading += 1;
            cache.arrived(
                "foto.jpg",
                Kind::Thumbnail,
                1024,
                Answer {
                    image: Some(photo.clone()),
                    settled: true,
                    shared: false,
                },
                cx,
            );
            assert!(matches!(cache.picture_of("foto.jpg", 1024, None), Picture::Shown(shown) if Arc::ptr_eq(&shown, &photo)));

            // While a bigger size is made, a smaller picture of the same file
            // shows; a smaller stand-in does not.
            let tile = image(64);
            let mut small = slot_of(Kind::Thumbnail, 256, Entry::Ready(tile.clone()));
            small.settled = true;
            cache.entries.insert("grande.jpg".into(), vec![small, slot_of(Kind::Thumbnail, 2048, loading())]);
            assert!(matches!(
                cache.picture_of("grande.jpg", 2048, Some(tile.clone())),
                Picture::Waiting(Some(shown)) if Arc::ptr_eq(&shown, &tile)
            ));
            cache.entries.get_mut("grande.jpg").unwrap()[0].settled = false;
            assert!(matches!(cache.picture_of("grande.jpg", 2048, Some(tile)), Picture::Waiting(None)));
        });
    }

    fn slot_of(kind: Kind, size: u32, entry: Entry) -> Slot {
        slot(kind, size, entry, 0)
    }

    #[gpui::test]
    fn a_stand_in_is_asked_for_again_and_the_picture_replaces_it(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut cache = Thumbnails::default();
            cache.entries.insert(
                "v.mp4".into(),
                vec![slot(Kind::Thumbnail, 64, loading(), 0)],
            );
            cache.loading = 1;

            let cone = image(64);
            cache.arrived(
                "v.mp4",
                Kind::Thumbnail,
                64,
                Answer {
                    image: Some(cone.clone()),
                    settled: false,
                    shared: false,
                },
                cx,
            );
            let held = &cache.entries["v.mp4"][0];
            assert!(matches!(&held.entry, Entry::Ready(shown) if Arc::ptr_eq(shown, &cone)));
            assert!(
                held.retry_at.is_some(),
                "a stand-in must be asked for again"
            );
            assert_eq!(
                (held.retries, cache.loading, cache.ready_bytes),
                (1, 0, 64 * 64 * 4)
            );

            // Asked again: the cone stays on screen meanwhile.
            let slot = &mut cache.entries.get_mut("v.mp4").unwrap()[0];
            slot.retry_at = None;
            slot.entry = Entry::Loading {
                task: Task::ready(()),
                shown: Some(cone.clone()),
            };
            cache.loading = 1;

            let frame = image(64);
            cache.arrived(
                "v.mp4",
                Kind::Thumbnail,
                64,
                Answer {
                    image: Some(frame.clone()),
                    settled: true,
                    shared: false,
                },
                cx,
            );
            let held = &cache.entries["v.mp4"][0];
            assert!(matches!(&held.entry, Entry::Ready(shown) if Arc::ptr_eq(shown, &frame)));
            assert!(held.retry_at.is_none(), "the real picture is final");
            assert_eq!((cache.loading, cache.ready_bytes), (0, 64 * 64 * 4));

            // The retries run out: a type with no picture stops asking.
            let slot = &mut cache.entries.get_mut("v.mp4").unwrap()[0];
            slot.retries = RETRIES;
            slot.entry = loading();
            cache.loading = 1;
            cache.ready_bytes = 0;
            cache.arrived(
                "v.mp4",
                Kind::Thumbnail,
                64,
                Answer {
                    image: Some(cone),
                    settled: false,
                    shared: false,
                },
                cx,
            );
            assert!(cache.entries["v.mp4"][0].retry_at.is_none());
            assert_eq!(retry_delay(0), FIRST_RETRY);
            assert_eq!(retry_delay(2), FIRST_RETRY * 16);
        });
    }

    #[test]
    fn extension_of_reads_the_last_dot_of_the_name_only() {
        assert_eq!(extension_of(r"C:\a.b\Song.WAV").as_deref(), Some("wav"));
        assert_eq!(extension_of(r"C:\a.b\README"), None);
        assert_eq!(extension_of(r"C:\x\.gitignore"), None);
        assert_eq!(extension_of(r"C:\x\archive.tar.gz").as_deref(), Some("gz"));
    }

    /// Asks the Shell for each file in `NOHRS_PROBE` (separated by `|`) in
    /// every mode and says whether the answer is the file's icon.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn probe_shell_answers() {
        platform::init_thread();
        let paths = std::env::var("NOHRS_PROBE").unwrap_or_default();
        for path in paths.split('|').filter(|path| !path.is_empty()) {
            let pixels = |image: Option<RenderImage>| {
                image.map(|image| {
                    let size = image.size(0);
                    (
                        size.width.0,
                        size.height.0,
                        image.as_bytes(0).map(<[u8]>::to_vec).unwrap_or_default(),
                    )
                })
            };
            let icon = pixels(platform::extract(path, 256, Mode::Icon));
            for mode in [Mode::Cached, Mode::Generate] {
                let answer = pixels(platform::extract(path, 256, mode));
                let is_icon = answer.is_some() && answer == icon;
                println!(
                    "{path} {mode:?}: {} is_icon={is_icon}",
                    answer
                        .as_ref()
                        .map_or("none".to_string(), |(w, h, _)| format!("{w}x{h}"))
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn icons_are_shared_only_where_the_registry_says_so() {
        assert!(platform::icon_is_per_class("txt"));
        assert!(!platform::icon_is_per_class("exe"));
        assert!(!platform::icon_is_per_class("lnk"));
        assert!(!platform::icon_is_per_class("ico"));
    }
}

/// What [`platform::extract`] is asked for.
///
/// Visible to the crate because the quick-launch window extracts its own icons
/// with [`platform::extract`] rather than growing a second Shell extractor
/// (docs/QuickDock-技术实现方案与边界约定.md §9).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// The file's icon.
    Icon,
    /// The thumbnail, only if Windows already has it cached.
    Cached,
    /// The thumbnail, generated from the file if need be.
    Generate,
}

#[cfg(windows)]
pub(crate) use platform::read_bitmap;

pub(crate) fn render_image(width: u32, height: u32, bgra: Vec<u8>) -> Option<RenderImage> {
    let buffer = image::RgbaImage::from_raw(width, height, bgra)?;
    Some(RenderImage::new(vec![image::Frame::new(buffer)]))
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) mod platform {
    //! The Shell's thumbnail extractor. `unsafe` is confined to this module:
    //! COM and GDI have no safe bindings, and the alternative — decoding every
    //! format ourselves — would still leave video frames and program icons out.

    use super::Mode;
    use gpui::RenderImage;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
        DeleteObject, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ,
    };
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CLASSES_ROOT, KEY_READ, RegCloseKey, RegOpenKeyExW,
    };
    use windows::Win32::UI::Shell::{
        ASSOCF_NOTRUNCATE, ASSOCSTR, ASSOCSTR_DEFAULTICON, ASSOCSTR_PROGID, AssocQueryStringW,
    };
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY, SIIGBF_INCACHEONLY,
        SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY,
    };
    use windows::core::{HSTRING, PCWSTR, PWSTR};

    pub fn init_thread() {
        // Some thumbnail handlers are apartment-threaded, so each worker is its
        // own STA, as Explorer's are. The thread lives for the whole run, so it
        // is never uninitialized.
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result.is_err() {
            tracing::warn!("COM init for thumbnails failed: {result:?}");
        }
    }

    pub fn extract(path: &str, size: u32, mode: Mode) -> Option<RenderImage> {
        let edge = i32::try_from(size).ok()?;
        // A list row wants the icon even for a photo, as Explorer's small
        // icons do: a 16px picture of the photo reads as a smudge.
        //
        // The thumbnail modes ask for the picture only. Without
        // `SIIGBF_THUMBNAILONLY` the Shell answers the type's icon, as a
        // success, whenever it has no picture — and `SIIGBF_INCACHEONLY` has
        // none for every video Explorer never showed. Tiles took the VLC cone
        // as the thumbnail and never asked again (probed 04/11/2026 with
        // `probe_shell_answers`: a real .mp4, Cached = the icon at 256x256,
        // Generate = the 256x144 frame). The icon comes from `Mode::Icon`
        // instead, marked as a stand-in.
        let flags = match mode {
            Mode::Icon => SIIGBF_ICONONLY,
            // Fails at once when the cache has nothing, rather than reading
            // the file; any size scales from what is cached.
            Mode::Cached => SIIGBF_RESIZETOFIT | SIIGBF_INCACHEONLY | SIIGBF_THUMBNAILONLY,
            Mode::Generate => SIIGBF_RESIZETOFIT | SIIGBF_THUMBNAILONLY,
        };
        let bitmap = unsafe {
            let factory: IShellItemImageFactory =
                SHCreateItemFromParsingName(&HSTRING::from(path), None).ok()?;
            factory.GetImage(SIZE { cx: edge, cy: edge }, flags).ok()?
        };

        let image = read_bitmap(bitmap);
        unsafe { DeleteObject(HGDIOBJ(bitmap.0)) }.ok().ok();
        image
    }

    /// Whether every file with this `extension` shows the same icon, as
    /// Salamander decides it (`CAssociations`, OTIMIZACAO.md §6.11): no when
    /// the registered icon is the file itself (`%1`, as for .ico) or an icon
    /// handler computes it per file (.lnk, .url), and no for the types known
    /// to carry their own. When the registry gives no icon at all the answer
    /// is per file too: asking the Shell for each costs time, a wrong shared
    /// icon would be a wrong picture. (An extension nothing is registered for
    /// does get one, the generic icon every such file shows.)
    pub fn icon_is_per_class(extension: &str) -> bool {
        const PER_FILE: [&str; 13] = [
            "exe",
            "lnk",
            "url",
            "ico",
            "cur",
            "ani",
            "scr",
            "cpl",
            "msc",
            "appref-ms",
            "website",
            "library-ms",
            "search-ms",
        ];
        if PER_FILE.contains(&extension) {
            return false;
        }
        let dotted = format!(".{extension}");
        let Some(default_icon) = query(ASSOCSTR_DEFAULTICON, &dotted) else {
            return false;
        };
        if default_icon.is_empty() || default_icon.contains("%1") {
            return false;
        }
        let mut handler_keys = vec![
            format!("{dotted}\\shellex\\IconHandler"),
            format!("SystemFileAssociations\\{dotted}\\shellex\\IconHandler"),
        ];
        if let Some(program) = query(ASSOCSTR_PROGID, &dotted) {
            handler_keys.push(format!("{program}\\shellex\\IconHandler"));
        }
        !handler_keys.iter().any(|key| key_exists(key))
    }

    fn query(what: ASSOCSTR, association: &str) -> Option<String> {
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        unsafe {
            AssocQueryStringW(
                ASSOCF_NOTRUNCATE,
                what,
                &HSTRING::from(association),
                PCWSTR::null(),
                Some(PWSTR(buffer.as_mut_ptr())),
                &mut length,
            )
        }
        .ok()
        .ok()?;
        let end = buffer
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..end]))
    }

    fn key_exists(subkey: &str) -> bool {
        let mut key = HKEY::default();
        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_CLASSES_ROOT,
                &HSTRING::from(subkey),
                None,
                KEY_READ,
                &mut key,
            )
        };
        if opened != ERROR_SUCCESS {
            return false;
        }
        let closed = unsafe { RegCloseKey(key) };
        if closed != ERROR_SUCCESS {
            tracing::warn!("could not close HKCR\\{subkey}: {closed:?}");
        }
        true
    }

    /// The pixels of a 32-bit `bitmap`, which stays the caller's to free.
    /// Also reads the bitmaps the Shell puts beside its menu entries.
    pub fn read_bitmap(bitmap: HBITMAP) -> Option<RenderImage> {
        let pixels = unsafe {
            let mut info = BITMAP::default();
            let filled = GetObjectW(
                HGDIOBJ(bitmap.0),
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut info as *mut BITMAP).cast()),
            );
            if filled == 0 || info.bmWidth <= 0 || info.bmHeight <= 0 {
                None
            } else {
                let (width, height) = (info.bmWidth, info.bmHeight);
                let mut header = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: width,
                        // Negative height asks for rows top-down, the order
                        // `RenderImage` stores them in.
                        biHeight: -height,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut buffer = vec![0u8; width as usize * height as usize * 4];
                let device = CreateCompatibleDC(None);
                let lines = GetDIBits(
                    device,
                    bitmap,
                    0,
                    height as u32,
                    Some(buffer.as_mut_ptr().cast()),
                    &mut header,
                    DIB_RGB_COLORS,
                );
                DeleteDC(device).ok().ok();
                (lines == height).then_some((width as u32, height as u32, buffer))
            }
        }?;

        let (width, height, mut bgra) = pixels;
        // Photo and video thumbnails come back as 32-bit pixels whose alpha
        // byte was never written. Read as written they would be fully
        // transparent, so a bitmap with no alpha anywhere is opaque. Icons do
        // carry alpha, already premultiplied, the form GPUI keeps images in. (A
        // program with only a small icon arrives framed in a dark square; that
        // is the Shell's own rendering, the same one Explorer shows.)
        if bgra.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0) {
            for pixel in bgra.as_chunks_mut::<4>().0 {
                pixel[3] = 0xFF;
            }
        }
        super::render_image(width, height, bgra)
    }
}

#[cfg(not(windows))]
pub(crate) mod platform {
    //! Elsewhere the formats `image` decodes here (PNG and JPEG) get a scaled
    //! copy; anything else keeps its icon.

    use super::Mode;
    use gpui::RenderImage;

    pub fn init_thread() {}

    /// Icons are not extracted here, so there is nothing to share.
    pub fn icon_is_per_class(_extension: &str) -> bool {
        false
    }

    pub fn extract(path: &str, size: u32, mode: Mode) -> Option<RenderImage> {
        // No cache to ask here: everything is generated on the slow queue.
        if mode != Mode::Generate {
            return None;
        }
        let decoded = image::open(path).ok()?.thumbnail(size, size).into_rgba8();
        let (width, height) = decoded.dimensions();
        let mut bgra = decoded.into_raw();
        for pixel in bgra.as_chunks_mut::<4>().0 {
            let alpha = u16::from(pixel[3]);
            let premultiply = |channel: u8| ((u16::from(channel) * alpha + 127) / 255) as u8;
            let (red, green, blue) = (pixel[0], pixel[1], pixel[2]);
            pixel[0] = premultiply(blue);
            pixel[1] = premultiply(green);
            pixel[2] = premultiply(red);
        }
        super::render_image(width, height, bgra)
    }
}
