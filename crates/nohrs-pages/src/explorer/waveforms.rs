//! Waveforms in place of the icon of sound files in the icons view, when
//! `ui.audio_waveforms` is on.
//!
//! What it costs, and where it goes (OTIMIZACAO.md):
//! - the waveform is an artifact, not a drawing made each frame from the
//!   sound (§3.5): `BARS` heights of one byte, decoded once per file version
//!   and kept in the redb store under path + size + modified time (§6.1), so
//!   opening the folder again reads 130 bytes instead of decoding the file;
//! - a cache read and a decode are different lanes (§5.3): one thread answers
//!   from the store, two decode. A folder whose waveforms are known fills at
//!   once even while long files are being decoded;
//! - newest request first, and what scrolled away is cancelled before it is
//!   decoded and stops at the next buffer if it already started (§5.3, §5.8);
//! - a long file is not decoded whole: a window is read at each bar's place
//!   (see `FULL_DECODE_LIMIT`);
//! - the tile paints the bars as quads on one canvas, no element per bar.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{Context, Task};
use nohrs_core::telemetry::LogErr;
use nohrs_store::{KvKey, KvOp, KvStore};

use super::ExplorerPane;

/// Heights kept per file. The largest tile is 256 px wide; at a bar of two
/// pixels and a gap of one that is 85 bars, so 128 leaves room for any zoom
/// and is still one small value in the store.
pub(crate) const BARS: usize = 128;

/// Up to this length a file is decoded whole; past it, one window per bar.
/// Measured 05/11/2026 on this machine (release, Media Foundation, three
/// rounds, `measure_decode`): decoding whole costs about 2-12 ms for a sound
/// effect, 75 ms for a 22 MB WAV and about 65 ms per minute of MP3 (4 MB:
/// 275 ms, 6 MB: 630 ms). The sampled read has a floor of its own — 35 ms for
/// a short WAV, 140 ms for a short MP3, 210 ms for the 4 MB one, where each
/// seek decodes from the previous frame — so it only wins past about three
/// minutes of MP3, and the whole read is the exact one. Four minutes.
const FULL_DECODE_LIMIT: Duration = Duration::from_secs(4 * 60);
/// How much is read at each bar's place in a sampled file: long enough to
/// cross a beat or a word, so the bar is not the silence between two.
const SAMPLE_WINDOW: Duration = Duration::from_millis(150);

/// Decoding threads. Decoding is CPU work, unlike the thumbnail workers'
/// waiting on the disk or the network; two leave the interface and the
/// thumbnails their share of the processor.
const DECODE_WORKERS: usize = 2;

/// Files remembered in the store before the oldest are forgotten: about 140
/// bytes each, 6 MB at the cap.
const STORED_CAP: usize = 40_000;
/// A stored waveform read after this long is written back with today's date,
/// so the ones in use survive the trimming without a write on every read.
const TOUCH_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);
/// New waveforms are written together: each redb commit syncs the disk, and
/// a folder of sound effects produces dozens a second.
const WRITE_EVERY: Duration = Duration::from_secs(2);
const WRITE_BATCH: usize = 64;

/// Waveforms in memory per pane before those off screen longest go. They
/// are small; this bounds a session that walks through many folders.
const KEPT_IN_MEMORY: usize = 20_000;

const NAMESPACE: &str = "waveform";
// Bumped when what is stored changes meaning, so an old value is not read
// as a new one (§6.1).
const FORMAT: u8 = 1;

const AUDIO_EXTENSIONS: [&str; 11] = [
    "wav", "mp3", "m4a", "aac", "flac", "wma", "aif", "aiff", "ogg", "opus", "mka",
];

/// Whether the icons view draws a waveform for `path`.
pub(crate) fn is_audio(path: &str) -> bool {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        AUDIO_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

enum Entry {
    Loading(#[allow(dead_code)] Task<()>),
    Ready(Arc<[u8]>),
    /// Not a sound Windows can decode; the tile keeps the icon.
    Failed,
}

struct Slot {
    entry: Entry,
    size: u64,
    modified: u64,
    last_seen: u64,
}

/// The waveforms one pane has asked for, by path.
#[derive(Default)]
pub struct Waveforms {
    directory: String,
    entries: HashMap<String, Slot>,
    frame: u64,
    loading: usize,
}

impl Waveforms {
    /// Forgets the waveforms of another directory; dropping a request closes
    /// its reply, and the worker skips it.
    pub fn show_directory(&mut self, directory: &str) {
        if self.directory != directory {
            self.directory = directory.to_string();
            self.entries.clear();
            self.loading = 0;
        }
    }

    /// Starts a frame: a request the last frame did not draw scrolled away
    /// and is cancelled.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
        let frame = self.frame;
        if self.loading > 0 {
            let before = self.entries.len();
            self.entries.retain(|_, slot| {
                !(matches!(slot.entry, Entry::Loading(_)) && slot.last_seen + 1 < frame)
            });
            self.loading -= before - self.entries.len();
        }
        if self.entries.len() > KEPT_IN_MEMORY {
            let mut seen: Vec<u64> = self.entries.values().map(|slot| slot.last_seen).collect();
            let keep_from = seen.len() - KEPT_IN_MEMORY / 4 * 3;
            let (_, threshold, _) = seen.select_nth_unstable(keep_from);
            let threshold = *threshold;
            self.entries.retain(|_, slot| {
                matches!(slot.entry, Entry::Loading(_)) || slot.last_seen >= threshold
            });
        }
    }

    /// The bars of `path`, or `None` while they are on their way or when the
    /// file cannot be decoded; the tile shows the icon meanwhile.
    pub fn get(
        &mut self,
        path: &str,
        size: u64,
        modified: u64,
        cx: &mut Context<ExplorerPane>,
    ) -> Option<Arc<[u8]>> {
        let frame = self.frame;
        if let Some(slot) = self.entries.get_mut(path) {
            if slot.size == size && slot.modified == modified {
                slot.last_seen = frame;
                return match &slot.entry {
                    Entry::Ready(bars) => Some(bars.clone()),
                    Entry::Loading(_) | Entry::Failed => None,
                };
            }
            // The file changed on disk while the folder was open.
            if matches!(slot.entry, Entry::Loading(_)) {
                self.loading -= 1;
            }
        }
        let task = request(path, size, modified, cx);
        self.entries.insert(
            path.to_string(),
            Slot {
                entry: Entry::Loading(task),
                size,
                modified,
                last_seen: frame,
            },
        );
        self.loading += 1;
        None
    }

    fn arrived(&mut self, path: &str, modified: u64, bars: Option<Arc<[u8]>>) {
        let Some(slot) = self.entries.get_mut(path) else {
            return;
        };
        if slot.modified != modified || !matches!(slot.entry, Entry::Loading(_)) {
            return;
        }
        self.loading -= 1;
        slot.entry = bars.map_or(Entry::Failed, Entry::Ready);
    }
}

fn request(path: &str, size: u64, modified: u64, cx: &mut Context<ExplorerPane>) -> Task<()> {
    let (reply, answer) = async_channel::bounded(1);
    service().ask(Job {
        path: path.to_string(),
        size,
        modified,
        reply,
    });
    let path = path.to_string();
    cx.spawn(async move |this, cx| {
        // A closed channel: the worker dropped the job, which counts as no
        // waveform for now; the next visit asks again.
        let bars = answer.recv().await.ok().flatten();
        this.update(cx, |pane, cx| {
            pane.waveforms.arrived(&path, modified, bars);
            cx.notify();
        })
        .log_err();
    })
}

struct Job {
    path: String,
    size: u64,
    modified: u64,
    reply: async_channel::Sender<Option<Arc<[u8]>>>,
}

// A stack of jobs, newest on top, and the threads waiting on it.
#[derive(Default)]
struct Lane {
    jobs: Mutex<Vec<Job>>,
    available: Condvar,
}

impl Lane {
    fn push(&self, job: Job) {
        match self.jobs.lock() {
            Ok(mut jobs) => jobs.push(job),
            Err(error) => tracing::error!("waveform queue poisoned: {error}"),
        }
        self.available.notify_one();
    }

    fn next(&self) -> Option<Job> {
        let mut jobs = self.jobs.lock().ok()?;
        loop {
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
    stored: Lane,
    decode: Lane,
    store: OnceLock<Arc<dyn KvStore>>,
    writes: Mutex<Vec<KvOp>>,
    written: Condvar,
}

fn service() -> &'static Arc<Service> {
    static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();
    SERVICE.get_or_init(|| {
        let service = Arc::new(Service::default());
        let mut threads: Vec<(String, fn(&Service))> = vec![
            ("nohrs-waveforms-stored".to_string(), Service::answer_stored),
            ("nohrs-waveforms-writer".to_string(), Service::write_batches),
        ];
        for index in 0..DECODE_WORKERS {
            threads.push((format!("nohrs-waveforms-{index}"), Service::decode));
        }
        for (name, work) in threads {
            let service = service.clone();
            if let Err(error) = std::thread::Builder::new()
                .name(name)
                .spawn(move || work(&service))
            {
                tracing::error!("could not start a waveform thread: {error}");
            }
        }
        service
    })
}

/// Gives the waveform cache its store, and trims it if it passed the cap.
/// Without a store waveforms still work, decoded again in each session.
pub(crate) fn install_store(store: Option<Arc<dyn KvStore>>) {
    let Some(store) = store else {
        return;
    };
    if service().store.set(store.clone()).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("nohrs-waveforms-trim".into())
        .spawn(move || trim(store.as_ref()));
    if let Err(error) = spawned {
        tracing::error!("could not start the waveform trim: {error}");
    }
}

// Down to nine tenths of the cap, oldest first, when the cap is passed.
fn trim(store: &dyn KvStore) {
    let Some(stored) = store.list_namespace(NAMESPACE).log_err() else {
        return;
    };
    if stored.len() <= STORED_CAP {
        return;
    }
    let mut dated: Vec<(u64, KvKey)> = stored
        .into_iter()
        .map(|(key, value)| (decode_value(&value).map_or(0, |value| value.written), key))
        .collect();
    dated.sort_unstable_by_key(|(written, _)| *written);
    let excess = dated.len() - STORED_CAP / 10 * 9;
    let deletes = dated
        .into_iter()
        .take(excess)
        .map(|(_, key)| KvOp::Delete { key })
        .collect();
    store.batch(deletes).log_err();
}

impl Service {
    fn ask(&self, job: Job) {
        if self.store.get().is_some() {
            self.stored.push(job);
        } else {
            self.decode.push(job);
        }
    }

    // The fast lane: answers what the store has, passes the rest on.
    fn answer_stored(&self) {
        while let Some(job) = self.stored.next() {
            let Some(store) = self.store.get() else {
                self.decode.push(job);
                continue;
            };
            let key = cache_key(&job.path, job.size, job.modified);
            let stored = key
                .as_ref()
                .and_then(|key| store.get(key).log_err().flatten());
            match stored.as_deref().and_then(decode_value) {
                Some(value) => {
                    if now_seconds().saturating_sub(value.written) > TOUCH_AFTER.as_secs()
                        && let Some(key) = key
                    {
                        self.queue_write(key, encode_value(value.bars.as_deref()));
                    }
                    if job.reply.try_send(value.bars).is_err() {
                        tracing::trace!("waveform answered after the tile left");
                    }
                }
                None => self.decode.push(job),
            }
        }
    }

    fn decode(&self) {
        platform::init_thread();
        while let Some(job) = self.decode.next() {
            let started = Instant::now();
            let reply = job.reply.clone();
            let cancelled = move || reply.is_closed();
            let bars = match platform::peaks(&job.path, &cancelled) {
                Ok(Some(peaks)) => Some(Arc::<[u8]>::from(peaks)),
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!("no waveform for {}: {error}", job.path);
                    None
                }
            };
            tracing::trace!("waveform of {} in {:?}", job.path, started.elapsed());
            if let Some(key) = cache_key(&job.path, job.size, job.modified) {
                self.queue_write(key, encode_value(bars.as_deref()));
            }
            if job.reply.try_send(bars).is_err() {
                tracing::trace!("waveform decoded after the tile left");
            }
        }
    }

    fn queue_write(&self, key: KvKey, value: Vec<u8>) {
        if self.store.get().is_none() {
            return;
        }
        if let Ok(mut writes) = self.writes.lock() {
            writes.push(KvOp::Put { key, value });
            if writes.len() >= WRITE_BATCH {
                self.written.notify_one();
            }
        }
    }

    fn write_batches(&self) {
        let Ok(mut writes) = self.writes.lock() else {
            return;
        };
        loop {
            let Ok((guard, _)) = self.written.wait_timeout(writes, WRITE_EVERY) else {
                return;
            };
            writes = guard;
            if writes.is_empty() {
                continue;
            }
            let batch = std::mem::take(&mut *writes);
            // Not holding the lock through the commit: the workers keep
            // queueing while the disk syncs.
            drop(writes);
            if let Some(store) = self.store.get() {
                store.batch(batch).log_err();
            }
            let Ok(guard) = self.writes.lock() else {
                return;
            };
            writes = guard;
        }
    }
}

struct Stored {
    written: u64,
    /// `None` for a file that could not be decoded: remembered too, so it is
    /// not decoded again on every visit until it changes.
    bars: Option<Arc<[u8]>>,
}

fn encode_value(bars: Option<&[u8]>) -> Vec<u8> {
    let bars = bars.unwrap_or_default();
    let mut value = Vec::with_capacity(9 + bars.len());
    value.push(FORMAT);
    value.extend_from_slice(&now_seconds().to_le_bytes());
    value.extend_from_slice(bars);
    value
}

fn decode_value(value: &[u8]) -> Option<Stored> {
    let (&format, rest) = value.split_first()?;
    if format != FORMAT || rest.len() < 8 {
        return None;
    }
    let (written, bars) = rest.split_at(8);
    let written = u64::from_le_bytes(written.try_into().ok()?);
    let bars = match bars.len() {
        0 => None,
        BARS => Some(Arc::from(bars)),
        _ => return None,
    };
    Some(Stored { written, bars })
}

// FNV-1a, not the standard hasher: the key has to come out the same in every
// build and every run, which `DefaultHasher` does not promise.
fn cache_key(path: &str, size: u64, modified: u64) -> Option<KvKey> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    // Windows paths are not case-sensitive: one waveform for both spellings.
    feed(path.to_lowercase().as_bytes());
    feed(&size.to_le_bytes());
    feed(&modified.to_le_bytes());
    KvKey::new(NAMESPACE, &format!("f{hash:016x}")).log_err()
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Peaks per bucket, scaled so the loudest is 255.
fn normalize(peaks: &[f32]) -> Vec<u8> {
    let loudest = peaks.iter().fold(0.0f32, |max, &peak| max.max(peak));
    let scale = if loudest > 0.0 { 255.0 / loudest } else { 0.0 };
    peaks
        .iter()
        .map(|&peak| (peak * scale).round().clamp(0.0, 255.0) as u8)
        .collect()
}

/// `BARS` buckets from any number of block peaks, each the loudest of its
/// stretch.
fn bucket(blocks: &[f32]) -> Vec<f32> {
    if blocks.is_empty() {
        return vec![0.0; BARS];
    }
    (0..BARS)
        .map(|bar| {
            let start = bar * blocks.len() / BARS;
            let end = ((bar + 1) * blocks.len() / BARS)
                .max(start + 1)
                .min(blocks.len());
            blocks.get(start..end).map_or(0.0, |stretch| {
                stretch.iter().fold(0.0f32, |max, &peak| max.max(peak))
            })
        })
        .collect()
}

/// The heights `bars` drawn `count` wide: each drawn bar is the loudest of
/// the stored ones it covers, so a narrow tile keeps the peaks.
pub(crate) fn fit(bars: &[u8], count: usize) -> Vec<u8> {
    if count == 0 || bars.is_empty() {
        return Vec::new();
    }
    (0..count)
        .map(|index| {
            let start = index * bars.len() / count;
            let end = ((index + 1) * bars.len() / count)
                .max(start + 1)
                .min(bars.len());
            bars.get(start..end)
                .and_then(|stretch| stretch.iter().copied().max())
                .unwrap_or(0)
        })
        .collect()
}

// Bar and gap of a tile's waveform, in pixels.
const WAVEFORM_BAR: f32 = 2.0;
const WAVEFORM_GAP: f32 = 1.0;

/// A sound file's waveform in an icon's square, for the icons view and the
/// preview's folder grid: the bars are quads painted on one canvas, so a
/// screen of tiles adds no element per bar to the layout.
/// `played` (0..=1), when the sound is playing under the mouse, paints the
/// bars already heard in the accent colour, as the preview's timeline does.
pub(crate) fn canvas(
    bars: Arc<[u8]>,
    icon: f32,
    selected: bool,
    played: Option<f32>,
) -> gpui::AnyElement {
    use gpui::{Bounds, Hsla, IntoElement as _, Styled as _, fill, point, px, rgb, size};
    use nohrs_ui::theme::theme;
    let (color, heard): (Hsla, Hsla) = match played {
        Some(_) => (rgb(theme::muted()).into(), rgb(theme::accent()).into()),
        None => {
            let color: Hsla = rgb(if selected {
                theme::accent()
            } else {
                theme::fg_secondary()
            })
            .into();
            (color, color)
        }
    };
    let played = played.unwrap_or(0.0);
    gpui::canvas(
        |_bounds, _window, _cx| {},
        move |bounds, (), window, _cx| {
            let width = f32::from(bounds.size.width) * 0.9;
            let height = f32::from(bounds.size.height) * 0.55;
            let count = ((width + WAVEFORM_GAP) / (WAVEFORM_BAR + WAVEFORM_GAP))
                .floor()
                .max(1.0) as usize;
            let fitted = fit(&bars, count);
            let drawn = count as f32 * (WAVEFORM_BAR + WAVEFORM_GAP) - WAVEFORM_GAP;
            let left = f32::from(bounds.origin.x) + (f32::from(bounds.size.width) - drawn) / 2.0;
            let middle = f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.0;
            for (index, &bar) in fitted.iter().enumerate() {
                // Silence still draws a line, so the tile never looks empty.
                let bar_height = (f32::from(bar) / 255.0 * height).max(1.0);
                let x = left + index as f32 * (WAVEFORM_BAR + WAVEFORM_GAP);
                let done = (index as f32 + 0.5) / count as f32 <= played;
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(x), px(middle - bar_height / 2.0)),
                        size(px(WAVEFORM_BAR), px(bar_height)),
                    ),
                    if done { heard } else { color },
                ));
            }
        },
    )
    .size(px(icon))
    .into_any_element()
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use super::{BARS, FULL_DECODE_LIMIT, SAMPLE_WINDOW, bucket, normalize};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::{GUID, HSTRING};

    /// Media Foundation for the life of a decoding thread: started once, not
    /// per file.
    pub fn init_thread() {
        // SAFETY: plain initialization calls on the thread that will use
        // Media Foundation; a failure surfaces as an error on the first file.
        unsafe {
            if let Err(error) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
                tracing::debug!("waveform thread COM: {error}");
            }
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_LITE) {
                tracing::error!("waveform thread could not start Media Foundation: {error}");
            }
        }
    }

    /// The file's `BARS` heights, or `None` when `cancelled` said stop.
    pub fn peaks(
        path: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> windows::core::Result<Option<Vec<u8>>> {
        peaks_read(path, cancelled, None)
    }

    /// `peaks`, with the way of reading forced (`Some(true)` = whole), for
    /// the measurement in the tests.
    pub fn peaks_read(
        path: &str,
        cancelled: &dyn Fn() -> bool,
        whole_file: Option<bool>,
    ) -> windows::core::Result<Option<Vec<u8>>> {
        let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        // SAFETY: COM calls on objects this function owns; buffers are read
        // only between their Lock and Unlock.
        unsafe {
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path), None)?;
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(stream, true)?;
            let wanted = MFCreateMediaType()?;
            wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
            reader.SetCurrentMediaType(stream, None, &wanted)?;
            let format = reader.GetCurrentMediaType(stream)?;
            let channels = format.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)?.max(1) as usize;
            let rate = format.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?.max(1) as usize;
            let duration = reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
                .ok()
                .and_then(|value| u64::try_from(&value).ok())
                .unwrap_or(0);
            let limit = FULL_DECODE_LIMIT.as_nanos() as u64 / 100;
            let peaks = if !whole_file.unwrap_or(duration <= limit) {
                sampled(&reader, stream, duration, channels, rate, cancelled)?
            } else {
                whole(&reader, stream, channels, rate, cancelled)?
            };
            Ok(peaks.map(|peaks| normalize(&peaks)))
        }
    }

    // Every sample, a peak per 10 ms block, then bucketed.
    unsafe fn whole(
        reader: &IMFSourceReader,
        stream: u32,
        channels: usize,
        rate: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> windows::core::Result<Option<Vec<f32>>> {
        let per_block = (rate / 100).max(1) * channels;
        let (mut blocks, mut peak, mut filled) = (Vec::new(), 0.0f32, 0usize);
        loop {
            if cancelled() {
                return Ok(None);
            }
            let more = unsafe {
                read(reader, stream, &mut |samples| {
                    for value in samples {
                        peak = peak.max(value.abs());
                        filled += 1;
                        if filled == per_block {
                            blocks.push(peak);
                            (peak, filled) = (0.0, 0);
                        }
                    }
                })?
            };
            if !more {
                break;
            }
        }
        if filled > 0 {
            blocks.push(peak);
        }
        Ok(Some(bucket(&blocks)))
    }

    // One window at the middle of each bar's stretch.
    unsafe fn sampled(
        reader: &IMFSourceReader,
        stream: u32,
        duration: u64,
        channels: usize,
        rate: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> windows::core::Result<Option<Vec<f32>>> {
        let window = (rate as u128 * SAMPLE_WINDOW.as_millis() / 1000) as usize * channels;
        let mut peaks = Vec::with_capacity(BARS);
        for bar in 0..BARS as u64 {
            if cancelled() {
                return Ok(None);
            }
            let at = duration * (2 * bar + 1) / (2 * BARS as u64);
            let position = PROPVARIANT::from(at as i64);
            unsafe { reader.SetCurrentPosition(&GUID::zeroed(), &position)? };
            let (mut peak, mut taken) = (0.0f32, 0usize);
            while taken < window {
                let more = unsafe {
                    read(reader, stream, &mut |samples| {
                        for value in samples.iter().take(window - taken) {
                            peak = peak.max(value.abs());
                        }
                        taken += samples.len();
                    })?
                };
                if !more {
                    break;
                }
            }
            peaks.push(peak);
        }
        Ok(Some(peaks))
    }

    // Hands the next buffer of float samples to `take`, read in place;
    // false at the end of the stream.
    unsafe fn read(
        reader: &IMFSourceReader,
        stream: u32,
        take: &mut dyn FnMut(&[f32]),
    ) -> windows::core::Result<bool> {
        loop {
            let mut flags = 0u32;
            let mut sample: Option<IMFSample> = None;
            unsafe {
                reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))?
            };
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(false);
            }
            let Some(sample) = sample else { continue };
            let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
            let (mut data, mut length) = (std::ptr::null_mut::<u8>(), 0u32);
            unsafe { buffer.Lock(&mut data, None, Some(&mut length))? };
            if !data.is_null() {
                // Float PCM, as asked; valid until the Unlock below.
                take(unsafe {
                    std::slice::from_raw_parts(data as *const f32, length as usize / 4)
                });
            }
            unsafe { buffer.Unlock()? };
            return Ok(true);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn init_thread() {}

    pub fn peaks(_path: &str, _cancelled: &dyn Fn() -> bool) -> std::io::Result<Option<Vec<u8>>> {
        Err(std::io::Error::other(
            "waveforms are only decoded on Windows",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_values_round_trip_and_reject_other_formats() {
        let bars: Vec<u8> = (0..BARS as u32).map(|index| index as u8).collect();
        let value = encode_value(Some(&bars));
        let stored = decode_value(&value).expect("readable");
        assert_eq!(stored.bars.as_deref(), Some(bars.as_slice()));
        let failed = decode_value(&encode_value(None)).expect("readable");
        assert!(
            failed.bars.is_none(),
            "an undecodable file is remembered as such"
        );
        let mut other = value.clone();
        other[0] = FORMAT + 1;
        assert!(
            decode_value(&other).is_none(),
            "another format is never read as this one"
        );
        assert!(
            decode_value(&value[..20]).is_none(),
            "a truncated value is not read"
        );
    }

    #[test]
    fn the_key_changes_with_the_file_and_not_with_the_case() {
        let key = cache_key(r"C:\Sons\a.wav", 10, 20);
        assert_eq!(key, cache_key(r"c:\sons\A.WAV", 10, 20));
        assert_ne!(key, cache_key(r"C:\Sons\a.wav", 11, 20));
        assert_ne!(key, cache_key(r"C:\Sons\a.wav", 10, 21));
        assert_eq!(
            key.map(|key| key.namespace().to_string()),
            Some(NAMESPACE.to_string())
        );
    }

    #[test]
    fn fitting_keeps_the_loudest_of_each_stretch() {
        let mut bars = vec![0u8; BARS];
        bars[5] = 200;
        let fitted = fit(&bars, 16);
        assert_eq!(fitted.len(), 16);
        assert_eq!(fitted[0], 200, "a peak survives narrowing");
        assert_eq!(fitted.iter().filter(|&&bar| bar > 0).count(), 1);
    }

    #[test]
    fn buckets_cover_every_block_and_normalize_to_full_scale() {
        let blocks: Vec<f32> = (0..1000)
            .map(|index| if index == 999 { 0.5 } else { 0.1 })
            .collect();
        let buckets = bucket(&blocks);
        assert_eq!(buckets.len(), BARS);
        let heights = normalize(&buckets);
        assert_eq!(
            heights[BARS - 1],
            255,
            "the last block lands in the last bar"
        );
        assert_eq!(heights[0], 51);
        assert_eq!(normalize(&[0.0, 0.0]), vec![0, 0], "silence stays flat");
    }

    /// Decode times of the files in `NOHRS_WAVEFORM_PROBE` (separated by
    /// `|`), whole and sampled, alternating, for `FULL_DECODE_LIMIT`.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn measure_decode() {
        let Ok(paths) = std::env::var("NOHRS_WAVEFORM_PROBE") else {
            return;
        };
        platform::init_thread();
        let never = || false;
        for path in paths.split('|') {
            for round in 0..3 {
                for whole in [true, false] {
                    let started = Instant::now();
                    let result = platform::peaks_read(path, &never, Some(whole));
                    let bars = result
                        .as_ref()
                        .ok()
                        .and_then(|bars| bars.as_ref())
                        .map_or(0, Vec::len);
                    println!(
                        "{path}	round {round}	{}	{:?}	{bars} bars",
                        if whole { "whole" } else { "sampled" },
                        started.elapsed()
                    );
                }
            }
        }
    }

    #[test]
    fn audio_is_told_by_extension() {
        assert!(is_audio(r"C:\a\b.WAV"));
        assert!(is_audio("x.flac"));
        assert!(!is_audio("song.mp4"));
        assert!(!is_audio("noextension"));
    }
}
