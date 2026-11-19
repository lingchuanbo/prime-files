//! Resting the mouse on a sound file plays it (`ui.hover_audio`), the way
//! sound libraries let you audition a folder without opening each file.
//!
//! One player serves every pane and window: two sounds at once is never what
//! hovering means, and the next file simply replaces the one playing. The
//! player is Windows' Media Foundation engine (the preview's), made once on a
//! thread of its own and handed each new file with `SetSource`, so starting a
//! sound costs opening the file, not starting Media Foundation.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;

use gpui::{Context, Task, WeakEntity};
use nohrs_core::telemetry::LogErr;

use super::ExplorerPane;

enum Command {
    /// Path and volume, 0..=1.
    Play(String, f64),
    Stop,
}

/// What the pane remembers of the file under the mouse.
pub(crate) struct HoverAudio {
    path: String,
    // Waits out the delay, then starts the sound; dropping it cancels.
    _task: Task<()>,
}

fn player() -> Option<&'static Mutex<mpsc::Sender<Command>>> {
    static PLAYER: OnceLock<Option<Mutex<mpsc::Sender<Command>>>> = OnceLock::new();
    PLAYER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            match std::thread::Builder::new()
                .name("nohrs-hover-audio".into())
                .spawn(move || platform::run(&receiver))
            {
                Ok(_) => Some(Mutex::new(sender)),
                Err(error) => {
                    tracing::error!("could not start the hover player: {error}");
                    None
                }
            }
        })
        .as_ref()
}

fn send(command: Command) {
    let Some(player) = player() else {
        return;
    };
    match player.lock() {
        Ok(sender) => sender.send(command).log_err(),
        Err(error) => {
            tracing::error!("hover player lock poisoned: {error}");
            None
        }
    };
}

/// Stops whatever the mouse started, from any pane.
pub(crate) fn stop() {
    // Nothing to stop until something played; no thread for it either.
    if PLAYED.load(std::sync::atomic::Ordering::Relaxed) {
        send(Command::Stop);
    }
}

static PLAYED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// The file playing and how far along, 0..=1, written by the player thread
// while it plays: the listing colours the played part of the waveform from it.
static PROGRESS: Mutex<Option<(String, f32)>> = Mutex::new(None);

fn set_progress(progress: Option<(String, f32)>) {
    match PROGRESS.lock() {
        Ok(mut current) => *current = progress,
        Err(poisoned) => *poisoned.into_inner() = progress,
    }
}

/// How far the sound playing under the mouse is, 0..=1, if `path` is it.
pub(crate) fn progress_of(path: &str) -> Option<f32> {
    let current = PROGRESS.lock().ok()?;
    current
        .as_ref()
        .filter(|(playing, _)| playing == path)
        .map(|(_, fraction)| *fraction)
}

/// Whether a sound started by the mouse is playing, so the listing keeps
/// drawing frames to move its progress.
pub(crate) fn playing() -> bool {
    PROGRESS.lock().is_ok_and(|current| current.is_some())
}

/// A line along the bottom of an entry, as long as the part of its sound
/// already played; for entries whose icon is not a waveform to colour.
pub(crate) fn progress_strip(path: &str) -> Option<gpui::AnyElement> {
    use gpui::{IntoElement as _, ParentElement as _, Styled as _, div, px, relative, rgb};
    let fraction = progress_of(path)?;
    Some(
        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .h(px(2.0))
            .child(
                div()
                    .h_full()
                    .w(relative(fraction.clamp(0.0, 1.0)))
                    .bg(rgb(nohrs_ui::theme::theme::accent())),
            )
            .into_any_element(),
    )
}

impl ExplorerPane {
    /// The mouse entered (`hovered`) or left the entry for `path`.
    pub(crate) fn hover_audio_changed(
        &mut self,
        path: String,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        if hovered {
            if self
                .hover_audio
                .as_ref()
                .is_some_and(|hover| hover.path == path)
            {
                return;
            }
            // Leaving one sound for another stops the first at once, even if
            // the second never plays.
            if self.hover_audio.take().is_some() {
                stop();
            }
            if !self.hover_audio_enabled {
                return;
            }
            let delay = Duration::from_millis(u64::from(self.hover_audio_delay_ms));
            let volume = if self.media_muted {
                0.0
            } else {
                self.media_volume
            };
            let task = cx.spawn({
                let path = path.clone();
                async move |this: WeakEntity<Self>, cx| {
                    cx.background_executor().timer(delay).await;
                    this.update(cx, |pane, _cx| {
                        if pane
                            .hover_audio
                            .as_ref()
                            .is_some_and(|hover| hover.path == path)
                        {
                            PLAYED.store(true, std::sync::atomic::Ordering::Relaxed);
                            send(Command::Play(path, volume));
                        }
                    })
                    .log_err();
                }
            });
            self.hover_audio = Some(HoverAudio { path, _task: task });
        } else if self
            .hover_audio
            .as_ref()
            .is_some_and(|hover| hover.path == path)
        {
            self.end_hover_audio();
        }
    }

    /// Forgets the file under the mouse and silences it: a click (the preview
    /// takes over), leaving the folder, or the entry scrolling away.
    pub(crate) fn end_hover_audio(&mut self) {
        if self.hover_audio.take().is_some() {
            stop();
        }
    }

    /// Makes `element` (an entry of the listing) play its sound when the
    /// mouse rests on it, if it is a sound and the setting is on.
    pub(crate) fn hover_audio_target<E: gpui::StatefulInteractiveElement>(
        &self,
        element: E,
        item: &nohrs_services::fs::listing::FileEntryDto,
        cx: &mut Context<Self>,
    ) -> E {
        if !self.hover_audio_enabled
            || item.kind == "dir"
            || !super::waveforms::is_audio(&item.path)
        {
            return element;
        }
        let path = item.path.clone();
        element.on_hover(cx.listener(move |pane, hovered: &bool, _window, cx| {
            pane.hover_audio_changed(path.clone(), *hovered, cx);
        }))
    }
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod platform {
    //! The engine; `unsafe` stays here, as in `media.rs`.

    use super::{Command, set_progress};
    use std::sync::mpsc;
    use std::time::Duration;
    use windows::Win32::Media::MediaFoundation::{
        CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFMediaEngine, IMFMediaEngineClassFactory,
        IMFMediaEngineNotify, IMFMediaEngineNotify_Impl, MF_MEDIA_ENGINE_AUDIOONLY,
        MF_MEDIA_ENGINE_CALLBACK, MF_VERSION, MFCreateAttributes, MFSTARTUP_FULL, MFStartup,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::core::{BSTR, implement};

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

    pub fn run(commands: &mpsc::Receiver<Command>) {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .ok();
        if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            tracing::error!("hover player: Media Foundation did not start: {error}");
            return;
        }
        let engine = match create() {
            Ok(engine) => engine,
            Err(error) => {
                tracing::error!("hover player: no media engine: {error}");
                return;
            }
        };
        // Blocks between commands while nothing plays: the thread costs
        // nothing while nobody hovers. While a sound plays it wakes every
        // 40 ms to publish where it is. The app's end drops the sender.
        let mut current: Option<String> = None;
        loop {
            let received = if current.is_some() {
                match commands.recv_timeout(Duration::from_millis(40)) {
                    Ok(command) => Some(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match commands.recv() {
                    Ok(command) => Some(command),
                    Err(_) => break,
                }
            };
            let Some(mut command) = received else {
                if let Some(path) = &current {
                    let (position, duration, ended) = unsafe {
                        (
                            engine.GetCurrentTime(),
                            engine.GetDuration(),
                            engine.IsEnded().as_bool(),
                        )
                    };
                    if ended {
                        set_progress(None);
                        current = None;
                    } else if duration.is_finite() && duration > 0.0 {
                        set_progress(Some((
                            path.clone(),
                            (position / duration).clamp(0.0, 1.0) as f32,
                        )));
                    }
                }
                continue;
            };
            // Sweeping the mouse over a column of sounds queues a play and a
            // stop per file; only the last one still means anything.
            while let Ok(next) = commands.try_recv() {
                command = next;
            }
            match command {
                Command::Play(path, volume) => unsafe {
                    engine.Pause().ok();
                    engine.SetVolume(volume).ok();
                    if let Err(error) = engine.SetSource(&BSTR::from(path.as_str())) {
                        tracing::warn!("hover player could not open {path}: {error}");
                        set_progress(None);
                        current = None;
                        continue;
                    }
                    engine.Play().ok();
                    set_progress(Some((path.clone(), 0.0)));
                    current = Some(path);
                },
                Command::Stop => unsafe {
                    set_progress(None);
                    current = None;
                    engine.Pause().ok();
                    // Releases the file, so it can be renamed or deleted while
                    // the mouse is elsewhere.
                    engine.SetSource(&BSTR::new()).ok();
                },
            }
        }
        unsafe { engine.Shutdown() }.ok();
    }

    fn create() -> windows::core::Result<IMFMediaEngine> {
        unsafe {
            let factory: IMFMediaEngineClassFactory =
                CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attributes, 1)?;
            let attributes = attributes.ok_or_else(windows::core::Error::empty)?;
            let listener: IMFMediaEngineNotify = Listener.into();
            attributes.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &listener)?;
            factory.CreateInstance(MF_MEDIA_ENGINE_AUDIOONLY.0 as u32, &attributes)
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::Command;
    use std::sync::mpsc;

    pub fn run(commands: &mpsc::Receiver<Command>) {
        while commands.recv().is_ok() {}
    }
}
