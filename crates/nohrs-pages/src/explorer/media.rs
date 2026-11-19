//! Video and audio in the preview pane, played by Windows' own Media
//! Foundation engine (what the Films & TV app uses), so every format Windows
//! plays plays here, hardware decoding included.
//!
//! The engine runs on a thread of its own in frame-server mode: it decodes,
//! and each new video frame is copied into a bitmap the size of the preview
//! and handed to gpui as an image. No window of the engine's is put over
//! the app, so menus and dialogs still draw on top of the video.

use gpui::{Context, RenderImage, Task};
use std::sync::Arc;
use std::sync::mpsc;

use super::ExplorerPane;

/// Largest frame handed to the UI, in pixels. Frames are scaled down by the
/// engine to fit, so a 4K video costs what a 1080p one does.
const MAX_FRAME: (u32, u32) = (1280, 720);

/// What the preview's controls ask of the player.
pub(crate) enum Command {
    Play,
    Pause,
    /// Seconds from the start.
    Seek(f64),
    /// 0..=1.
    Volume(f64),
}

/// A video or audio file playing (or paused) in the preview.
pub(crate) struct MediaPreview {
    commands: mpsc::Sender<Command>,
    /// The frame on screen; `None` for audio and until the first arrives.
    pub frame: Option<Arc<RenderImage>>,
    /// Seconds.
    pub position: f64,
    /// Seconds; 0 until the file's length is known.
    pub duration: f64,
    pub playing: bool,
    pub has_video: bool,
    pub error: Option<String>,
    /// Loudest sample of each `PEAK_BLOCKS_PER_SECOND`th of a second of an
    /// audio file, 0..=1, filled from the start as the file is read. Empty
    /// for video. An `Arc` so a frame's paint can hold it without a copy.
    pub peaks: Arc<Vec<f32>>,
    /// Loudest of `peaks`, which the waveform is scaled to.
    pub peak_max: f32,
    _task: Task<()>,
}

/// Resolution of the waveform: a peak per 10 ms, fine enough for a bar per
/// pixel of a 3-minute song across a wide pane.
pub(crate) const PEAK_BLOCKS_PER_SECOND: usize = 100;

enum Update {
    Frame(RenderImage),
    State {
        position: f64,
        duration: f64,
        playing: bool,
        has_video: bool,
    },
    /// The next stretch of `MediaPreview::peaks`.
    Peaks(Vec<f32>),
    Failed(String),
}

fn extension(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Whether `path` is a sound-only file, which the preview draws as a waveform.
pub(crate) fn is_audio(path: &str) -> bool {
    matches!(
        extension(path).as_str(),
        "mp3" | "wav" | "m4a" | "aac" | "flac" | "wma" | "ogg" | "opus"
    )
}

/// Whether the preview plays `path` as video or audio.
pub(crate) fn is_media(path: &str) -> bool {
    is_audio(path)
        || matches!(
            extension(path).as_str(),
            "mp4"
                | "m4v"
                | "mov"
                | "wmv"
                | "avi"
                | "mkv"
                | "webm"
                | "mpg"
                | "mpeg"
                | "3gp"
                | "ts"
                | "mts"
        )
}

/// The volume a player is told to play at.
fn effective_volume(volume: f64, muted: bool) -> f64 {
    if muted { 0.0 } else { volume.clamp(0.0, 1.0) }
}

impl MediaPreview {
    /// Sends `command` to the player.
    pub(crate) fn send(&self, command: Command) {
        // The player thread only ends when this preview is dropped.
        self.commands.send(command).ok();
    }
}

impl ExplorerPane {
    /// Opens `path` in the preview's player, paused on its first frame.
    pub(crate) fn open_media_preview(&mut self, path: String, cx: &mut Context<Self>) {
        let (commands, command_receiver) = mpsc::channel();
        let (sender, receiver) = async_channel::unbounded();
        // Queued before the engine exists; the player applies it as it starts.
        commands
            .send(Command::Volume(effective_volume(
                self.media_volume,
                self.media_muted,
            )))
            .ok();
        if is_audio(&path) {
            // Its own reader on its own thread: playback starts at once and
            // the waveform fills in beside it. Both end with the preview,
            // when the receiver goes and their sends start failing.
            let (path, sender) = (path.clone(), sender.clone());
            let spawned = std::thread::Builder::new()
                .name("nohrs-waveform".into())
                .spawn(move || platform::waveform(&path, &sender));
            if let Err(error) = spawned {
                tracing::warn!("could not start reading the waveform: {error}");
            }
        }
        let spawned = std::thread::Builder::new()
            .name("nohrs-media".into())
            .spawn(move || platform::run(&path, &command_receiver, &sender));
        if let Err(error) = spawned {
            tracing::error!("could not start the media player: {error}");
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            while let Ok(update) = receiver.recv().await {
                let alive = this.update(cx, |pane, cx| {
                    let Some(media) = pane.preview_media.as_mut() else {
                        return false;
                    };
                    match update {
                        Update::Frame(frame) => {
                            // The atlas keeps a copy of every image drawn;
                            // without this each frame would stay in it.
                            if let Some(old) = media.frame.replace(Arc::new(frame)) {
                                cx.drop_image(old, None);
                            }
                        }
                        Update::State {
                            position,
                            duration,
                            playing,
                            has_video,
                        } => {
                            media.position = position;
                            media.duration = duration;
                            media.playing = playing;
                            media.has_video = has_video;
                        }
                        Update::Peaks(stretch) => {
                            media.peak_max =
                                stretch.iter().fold(media.peak_max, |max, &p| max.max(p));
                            Arc::make_mut(&mut media.peaks).extend(stretch);
                        }
                        Update::Failed(error) => media.error = Some(error),
                    }
                    cx.notify();
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        });
        let media = MediaPreview {
            commands,
            frame: None,
            position: 0.0,
            duration: 0.0,
            playing: false,
            has_video: false,
            error: None,
            peaks: Arc::default(),
            peak_max: 0.0,
            _task: task,
        };
        // The engine takes Play before the file is open and starts once it
        // is, so autoplay needs no wait for the first state.
        if self.preview_autoplay {
            media.send(Command::Play);
        }
        self.preview_media = Some(MediaPreview {
            playing: self.preview_autoplay,
            ..media
        });
        cx.notify();
    }

    /// Stops the player and frees its last frame, when the preview moves on.
    pub(crate) fn close_media_preview(&mut self, cx: &mut gpui::App) {
        if let Some(media) = self.preview_media.take()
            && let Some(frame) = media.frame
        {
            cx.drop_image(frame, None);
        }
    }

    /// Plays or pauses the previewed media.
    pub(crate) fn toggle_media(&mut self, cx: &mut Context<Self>) {
        if let Some(media) = self.preview_media.as_mut() {
            media.playing = !media.playing;
            media.send(if media.playing {
                Command::Play
            } else {
                Command::Pause
            });
            cx.notify();
        }
    }

    /// Jumps the previewed media to `fraction` (0..=1) of its length.
    pub(crate) fn seek_media(&mut self, fraction: f64, cx: &mut Context<Self>) {
        if let Some(media) = self.preview_media.as_mut()
            && media.duration > 0.0
        {
            media.position = fraction.clamp(0.0, 1.0) * media.duration;
            media.send(Command::Seek(media.position));
            cx.notify();
        }
    }

    /// Mutes or unmutes the previewed media.
    pub(crate) fn toggle_media_mute(&mut self, cx: &mut Context<Self>) {
        self.media_muted = !self.media_muted;
        // Unmuting a slider left at zero would still be silent.
        if !self.media_muted && self.media_volume <= 0.0 {
            self.media_volume = 0.5;
        }
        self.send_media_volume(cx);
    }

    /// Sets the volume to `volume` (0..=1), unmuting.
    pub(crate) fn set_media_volume(&mut self, volume: f64, cx: &mut Context<Self>) {
        self.media_volume = volume.clamp(0.0, 1.0);
        self.media_muted = false;
        self.send_media_volume(cx);
    }

    fn send_media_volume(&mut self, cx: &mut Context<Self>) {
        if let Some(media) = self.preview_media.as_ref() {
            media.send(Command::Volume(effective_volume(
                self.media_volume,
                self.media_muted,
            )));
        }
        cx.notify();
    }
}

/// Heights (0..=1) of `bars` waveform bars spread over `duration` seconds,
/// each the loudest peak in its stretch. `None` for a stretch not read yet.
pub(crate) fn waveform_bars(
    peaks: &[f32],
    peak_max: f32,
    duration: f64,
    bars: usize,
) -> Vec<Option<f32>> {
    // Until the engine knows the length, what has been read fills the width.
    let blocks = if duration > 0.0 {
        (duration * PEAK_BLOCKS_PER_SECOND as f64).ceil() as usize
    } else {
        peaks.len()
    };
    if bars == 0 || blocks == 0 {
        return Vec::new();
    }
    let scale = if peak_max > 0.0 { 1.0 / peak_max } else { 0.0 };
    (0..bars)
        .map(|bar| {
            let start = bar * blocks / bars;
            let end = ((bar + 1) * blocks / bars).max(start + 1);
            let stretch = peaks
                .get(start..end.min(peaks.len()))
                .filter(|s| !s.is_empty())?;
            Some((stretch.iter().fold(0.0f32, |max, &p| max.max(p)) * scale).min(1.0))
        })
        .collect()
}

/// `1:05` or `1:02:05`.
pub(crate) fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! The Media Foundation engine. `unsafe` is confined to this module: none
    //! of these interfaces has a safe binding.

    use super::{Command, MAX_FRAME, PEAK_BLOCKS_PER_SECOND, Update};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmap, IWICImagingFactory,
        WICBitmapCacheOnLoad, WICBitmapLockRead, WICRect,
    };
    use windows::Win32::Media::MediaFoundation::{
        CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFMediaEngine, IMFMediaEngineClassFactory,
        IMFMediaEngineNotify, IMFMediaEngineNotify_Impl, IMFSample, MF_MEDIA_ENGINE_CALLBACK,
        MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, MF_MT_AUDIO_NUM_CHANNELS,
        MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
        MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
        MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFARGB, MFAudioFormat_Float, MFCreateAttributes,
        MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio, MFSTARTUP_FULL,
        MFShutdown, MFStartup,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::core::{BSTR, HSTRING, implement};

    // DXGI_FORMAT_B8G8R8A8_UNORM, the frame format asked of the engine: the
    // byte order gpui's images use, so frames need no swizzling.
    const B8G8R8A8: u32 = 87;

    // The engine insists on a listener; the player polls its state instead,
    // which keeps everything on this one thread.
    #[implement(IMFMediaEngineNotify)]
    struct Listener;

    impl IMFMediaEngineNotify_Impl for Listener_Impl {
        fn EventNotify(
            &self,
            _event: u32,
            _param1: usize,
            _param2: u32,
        ) -> windows::core::Result<()> {
            Ok(())
        }
    }

    pub fn run(
        path: &str,
        commands: &mpsc::Receiver<Command>,
        updates: &async_channel::Sender<Update>,
    ) {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .ok();
        if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            updates
                .send_blocking(Update::Failed(error.message().to_string()))
                .ok();
            return;
        }
        if let Err(error) = play(path, commands, updates) {
            updates
                .send_blocking(Update::Failed(error.message().to_string()))
                .ok();
        }
        unsafe { MFShutdown() }.ok();
    }

    fn play(
        path: &str,
        commands: &mpsc::Receiver<Command>,
        updates: &async_channel::Sender<Update>,
    ) -> windows::core::Result<()> {
        let engine = unsafe {
            let factory: IMFMediaEngineClassFactory =
                CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attributes, 2)?;
            let attributes = attributes.ok_or_else(windows::core::Error::empty)?;
            let listener: IMFMediaEngineNotify = Listener.into();
            attributes.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &listener)?;
            attributes.SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, B8G8R8A8)?;
            let engine: IMFMediaEngine = factory.CreateInstance(0, &attributes)?;
            engine.SetSource(&BSTR::from(path))?;
            engine
        };
        let wic: IWICImagingFactory =
            unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };
        let mut target: Option<(IWICBitmap, u32, u32)> = None;
        let mut last_frame = i64::MIN;
        let mut last_state = Instant::now() - Duration::from_secs(1);
        let result = loop {
            loop {
                match commands.try_recv() {
                    Ok(Command::Play) => unsafe {
                        // From the end, play starts over, as players do.
                        if engine.IsEnded().as_bool() {
                            engine.SetCurrentTime(0.0).ok();
                        }
                        engine.Play().ok();
                    },
                    Ok(Command::Pause) => unsafe { engine.Pause() }.unwrap_or(()),
                    Ok(Command::Seek(seconds)) => {
                        unsafe { engine.SetCurrentTime(seconds) }.unwrap_or(())
                    }
                    Ok(Command::Volume(volume)) => {
                        unsafe { engine.SetVolume(volume) }.unwrap_or(())
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    // The preview was closed.
                    Err(mpsc::TryRecvError::Disconnected) => {
                        unsafe { engine.Shutdown() }.ok();
                        return Ok(());
                    }
                }
            }
            if let Ok(error) = unsafe { engine.GetError() }
                && unsafe { error.GetErrorCode() } != 0
            {
                break Err(windows::core::Error::new(
                    // 0.61 hands the extended code back as a `Result`, the
                    // failure HRESULT riding in its `Err`.
                    windows::core::HRESULT::from(unsafe { error.GetExtendedErrorCode() }),
                    nohrs_core::t!("Windows não consegue reproduzir este arquivo"),
                ));
            }
            let has_video = unsafe { engine.HasVideo() }.as_bool();
            if has_video
                && let Ok(time) = unsafe { engine.OnVideoStreamTick() }
                && time != last_frame
                && let Some(frame) = grab(&engine, &wic, &mut target)
            {
                last_frame = time;
                if updates.send_blocking(Update::Frame(frame)).is_err() {
                    break Ok(());
                }
            }
            if last_state.elapsed() >= Duration::from_millis(100) {
                last_state = Instant::now();
                let state = unsafe {
                    Update::State {
                        position: engine.GetCurrentTime(),
                        duration: engine.GetDuration().max(0.0),
                        playing: !engine.IsPaused().as_bool() && !engine.IsEnded().as_bool(),
                        has_video,
                    }
                };
                if updates.send_blocking(state).is_err() {
                    break Ok(());
                }
            }
            // About one vsync: frames are not missed and the thread idles.
            std::thread::sleep(Duration::from_millis(8));
        };
        unsafe { engine.Shutdown() }.ok();
        result
    }

    /// Reads `path`'s sound from start to end and sends its peaks, a stretch
    /// at a time, until the file ends or the preview is closed.
    pub fn waveform(path: &str, updates: &async_channel::Sender<Update>) {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .ok();
        if unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.is_err() {
            return;
        }
        // No waveform is not worth an error over the player: it just stays empty.
        if let Err(error) = read_peaks(path, updates) {
            tracing::debug!("no waveform for {path}: {error}");
        }
        unsafe { MFShutdown() }.ok();
    }

    fn read_peaks(
        path: &str,
        updates: &async_channel::Sender<Update>,
    ) -> windows::core::Result<()> {
        let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let reader = unsafe {
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path), None)?;
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(stream, true)?;
            // Decoded to float PCM, whatever the file holds: one sample
            // format to take peaks of.
            let wanted = MFCreateMediaType()?;
            wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
            reader.SetCurrentMediaType(stream, None, &wanted)?;
            reader
        };
        let format = unsafe { reader.GetCurrentMediaType(stream)? };
        let channels = unsafe { format.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)? }.max(1) as usize;
        let rate = unsafe { format.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)? }.max(1) as usize;
        // Every channel's samples of a block count toward its one peak.
        let per_block = (rate / PEAK_BLOCKS_PER_SECOND).max(1) * channels;
        let (mut stretch, mut peak, mut filled) = (Vec::new(), 0.0f32, 0usize);
        let mut last_sent = Instant::now();
        loop {
            let mut flags = 0u32;
            let mut sample: Option<IMFSample> = None;
            unsafe {
                reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))?
            };
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            let Some(sample) = sample else { continue };
            let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
            let (mut data, mut length) = (std::ptr::null_mut::<u8>(), 0u32);
            unsafe { buffer.Lock(&mut data, None, Some(&mut length))? };
            if !data.is_null() {
                // Float PCM, as asked; Media Foundation buffers are aligned
                // for it. Valid until the `Unlock` below.
                let samples =
                    unsafe { std::slice::from_raw_parts(data as *const f32, length as usize / 4) };
                for &value in samples {
                    peak = peak.max(value.abs());
                    filled += 1;
                    if filled == per_block {
                        stretch.push(peak);
                        (peak, filled) = (0.0, 0);
                    }
                }
            }
            unsafe { buffer.Unlock()? };
            // A few stretches a second: the waveform visibly fills in
            // without a message per decoded buffer.
            if last_sent.elapsed() >= Duration::from_millis(200) && !stretch.is_empty() {
                last_sent = Instant::now();
                if updates
                    .send_blocking(Update::Peaks(std::mem::take(&mut stretch)))
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
        if filled > 0 {
            stretch.push(peak);
        }
        updates.send_blocking(Update::Peaks(stretch)).ok();
        Ok(())
    }

    // Copies the engine's current frame into a bitmap no larger than
    // `MAX_FRAME`, keeping the aspect ratio, and returns it as an image.
    fn grab(
        engine: &IMFMediaEngine,
        wic: &IWICImagingFactory,
        target: &mut Option<(IWICBitmap, u32, u32)>,
    ) -> Option<gpui::RenderImage> {
        let (mut width, mut height) = (0u32, 0u32);
        unsafe { engine.GetNativeVideoSize(Some(&mut width), Some(&mut height)) }.ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        let scale = (MAX_FRAME.0 as f64 / width as f64)
            .min(MAX_FRAME.1 as f64 / height as f64)
            .min(1.0);
        let (width, height) = (
            ((width as f64 * scale) as u32).max(1),
            ((height as f64 * scale) as u32).max(1),
        );
        if !matches!(target, Some((_, w, h)) if *w == width && *h == height) {
            let bitmap = unsafe {
                wic.CreateBitmap(
                    width,
                    height,
                    &GUID_WICPixelFormat32bppPBGRA,
                    WICBitmapCacheOnLoad,
                )
            }
            .ok()?;
            *target = Some((bitmap, width, height));
        }
        let (bitmap, _, _) = target.as_ref()?;
        let rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let black = MFARGB {
            rgbBlue: 0,
            rgbGreen: 0,
            rgbRed: 0,
            rgbAlpha: 255,
        };
        unsafe { engine.TransferVideoFrame(bitmap, None, &rect, Some(&black)) }.ok()?;
        let lock = unsafe {
            bitmap.Lock(
                &WICRect {
                    X: 0,
                    Y: 0,
                    Width: width as i32,
                    Height: height as i32,
                },
                WICBitmapLockRead.0 as u32,
            )
        }
        .ok()?;
        let stride = unsafe { lock.GetStride() }.ok()? as usize;
        let (mut size, mut data) = (0u32, std::ptr::null_mut::<u8>());
        unsafe { lock.GetDataPointer(&mut size, &mut data) }.ok()?;
        if data.is_null() {
            return None;
        }
        // The lock's buffer, valid until `lock` drops at the end of this function.
        let buffer = unsafe { std::slice::from_raw_parts(data, size as usize) };
        let row = width as usize * 4;
        let mut bgra = Vec::with_capacity(row * height as usize);
        for line in 0..height as usize {
            bgra.extend_from_slice(buffer.get(line * stride..line * stride + row)?);
        }
        crate::explorer::thumbnails::render_image(width, height, bgra)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Command, Update};
    use std::sync::mpsc;

    pub fn run(
        _path: &str,
        _commands: &mpsc::Receiver<Command>,
        updates: &async_channel::Sender<Update>,
    ) {
        updates
            .send_blocking(Update::Failed(
                "Playback is only available on Windows".into(),
            ))
            .ok();
    }

    pub fn waveform(_path: &str, _updates: &async_channel::Sender<Update>) {}
}

#[cfg(test)]
mod tests {
    use super::{
        PEAK_BLOCKS_PER_SECOND, clock, effective_volume, is_audio, is_media, waveform_bars,
    };

    #[test]
    fn bars_take_the_loudest_peak_of_their_stretch_scaled_to_the_file() {
        // Two seconds known, one read: the second half has nothing to draw.
        let mut peaks = vec![0.1; PEAK_BLOCKS_PER_SECOND];
        peaks[10] = 0.5;
        let bars = waveform_bars(&peaks, 0.5, 2.0, 4);
        assert_eq!(bars, [Some(1.0), Some(0.2), None, None]);
        // With no length yet, what has been read fills every bar.
        assert!(
            waveform_bars(&peaks, 0.5, 0.0, 4)
                .iter()
                .all(Option::is_some)
        );
        assert!(waveform_bars(&[], 0.0, 0.0, 4).is_empty());
    }

    #[test]
    fn muting_silences_without_forgetting_the_volume() {
        assert_eq!(effective_volume(0.7, false), 0.7);
        assert_eq!(effective_volume(0.7, true), 0.0);
        assert!(is_audio("a.MP3") && !is_audio("a.mp4") && is_media("a.mp4"));
    }

    #[test]
    fn clock_reads_like_a_player() {
        assert_eq!(clock(5.4), "0:05");
        assert_eq!(clock(65.0), "1:05");
        assert_eq!(clock(3725.0), "1:02:05");
    }

    #[test]
    fn media_is_recognized_by_extension() {
        assert!(is_media("C:\\v\\clip.MP4"));
        assert!(is_media("song.flac"));
        assert!(!is_media("notes.txt"));
    }
}
