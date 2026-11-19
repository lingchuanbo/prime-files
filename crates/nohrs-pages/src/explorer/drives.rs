//! How full each drive is: the cards above This PC's listing and the small
//! bars beside the drives in the sidebar, as Explorer shows under "Devices
//! and drives".
//!
//! Asking Windows is cheap for a local disk and can hang for seconds on a
//! network drive that went away, so the answers are read on a thread of
//! their own, kept app-wide, and read again at most every [`REFRESH_AFTER`]
//! while something on screen shows them.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Global, Task};
use nohrs_core::telemetry::LogErr;

const REFRESH_AFTER: Duration = Duration::from_secs(5);

/// Space on one drive, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Space {
    pub free: u64,
    pub total: u64,
}

impl Space {
    /// Used share, 0..=1.
    pub fn used_fraction(self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (self.total.saturating_sub(self.free) as f64 / self.total as f64) as f32
    }

    /// Explorer turns the bar red past 90% full.
    pub fn nearly_full(self) -> bool {
        self.used_fraction() >= 0.9
    }

    /// "197 GB livres de 931 GB".
    pub fn describe(self) -> String {
        use nohrs_ui::components::file_list::human_bytes;
        nohrs_core::tf!(
            "{free} livres de {total}",
            free = human_bytes(self.free),
            total = human_bytes(self.total)
        )
    }
}

#[derive(Default)]
struct Spaces {
    by_root: HashMap<String, Space>,
    read_at: Option<Instant>,
    reading: bool,
    _task: Option<Task<()>>,
}

impl Global for Spaces {}

/// `C:\` for any spelling of a drive's root (`c:`, `C:\`, `c:/`); `None` for
/// anything that is not one.
pub(crate) fn drive_root(path: &str) -> Option<String> {
    let mut chars = path.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    if chars.next() != Some(':') {
        return None;
    }
    match chars.as_str() {
        "" | "\\" | "/" => Some(format!("{}:\\", letter.to_ascii_uppercase())),
        _ => None,
    }
}

/// The space on the drive whose root is `path`, if known; asks Windows again
/// in the background when the last answer is old. The window repaints when
/// the answer arrives.
pub(crate) fn space(path: &str, cx: &mut App) -> Option<Space> {
    let root = drive_root(path)?;
    refresh_if_stale(cx);
    cx.try_global::<Spaces>()?.by_root.get(&root).copied()
}

fn refresh_if_stale(cx: &mut App) {
    let spaces = cx.default_global::<Spaces>();
    let fresh = spaces
        .read_at
        .is_some_and(|at| at.elapsed() < REFRESH_AFTER);
    if fresh || spaces.reading {
        return;
    }
    spaces.reading = true;
    let read = cx.background_spawn(async { platform::read_all() });
    let task = cx.spawn(async move |cx| {
        let by_root = read.await;
        cx.update(|cx| {
            let spaces = cx.global_mut::<Spaces>();
            spaces.reading = false;
            spaces.read_at = Some(Instant::now());
            if spaces.by_root != by_root {
                spaces.by_root = by_root;
                cx.refresh_windows();
            }
        })
        .log_err();
    });
    cx.global_mut::<Spaces>()._task = Some(task);
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod platform {
    use super::Space;
    use std::collections::HashMap;
    use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetLogicalDrives};
    use windows::core::HSTRING;

    pub fn read_all() -> HashMap<String, Space> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|bit| mask & (1 << bit) != 0)
            .filter_map(|bit| {
                let root = format!("{}:\\", char::from(b'A' + bit));
                let mut free = 0u64;
                let mut total = 0u64;
                // The free space this user may fill (quotas count), as
                // Explorer reports it. An empty card reader fails here and is
                // left out.
                unsafe {
                    GetDiskFreeSpaceExW(
                        &HSTRING::from(root.as_str()),
                        Some(&mut free),
                        Some(&mut total),
                        None,
                    )
                }
                .ok()?;
                (total > 0).then_some((root, Space { free, total }))
            })
            .collect()
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::Space;
    use std::collections::HashMap;

    pub fn read_all() -> HashMap<String, Space> {
        HashMap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bare_drive_is_a_root() {
        assert_eq!(drive_root("c:").as_deref(), Some("C:\\"));
        assert_eq!(drive_root("D:\\").as_deref(), Some("D:\\"));
        assert_eq!(drive_root("C:\\Users"), None);
        assert_eq!(drive_root("\\\\server\\share"), None);
    }

    #[test]
    fn the_bar_turns_red_past_ninety_percent() {
        let space = Space {
            free: 50,
            total: 1000,
        };
        assert!(space.nearly_full());
        assert!((space.used_fraction() - 0.95).abs() < 1e-6);
        assert!(
            !Space {
                free: 500,
                total: 1000
            }
            .nearly_full()
        );
        assert_eq!(Space { free: 0, total: 0 }.used_fraction(), 0.0);
    }
}
