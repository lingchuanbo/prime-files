//! Progress and cancellation for the long filesystem operations: copy, move,
//! delete, and the archive tools the explorer runs.
//!
//! The work happens on a background thread while the window draws; a
//! [`TransferProgress`] is the small lock-free-ish board the two share. The
//! worker bumps the counters and checks [`TransferProgress::cancelled`] between
//! entries (and, for a large file, inside its copy loop); the dialog reads
//! [`TransferProgress::snapshot`] each frame and its Cancel button calls
//! [`TransferProgress::cancel`]. A cancelled operation reports
//! [`Error::Cancelled`](nohrs_core::errors::Error::Cancelled) so callers can
//! tell "the user stopped it" from "it went wrong".

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nohrs_core::errors::{Error, Result};

/// One operation's progress, shared between the worker and the window.
#[derive(Debug, Default)]
pub struct TransferProgress {
    cancelled: AtomicBool,
    /// Bytes finished so far.
    done: AtomicU64,
    /// Bytes expected in total; zero while it is not known (the bar runs
    /// indeterminate then).
    total: AtomicU64,
    /// Top-level items finished, out of the batch size.
    items_done: AtomicU64,
    items_total: AtomicU64,
    /// Files finished inside those items, for a folder copy.
    files_done: AtomicU64,
    /// What the worker is on now: the entry being copied, or an archive being
    /// unpacked.
    detail: Mutex<String>,
}

/// A copy of the counters for one frame of the dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferSnapshot {
    /// What the worker is on now.
    pub detail: String,
    /// Bytes finished.
    pub done: u64,
    /// Bytes expected, or zero while unknown.
    pub total: u64,
    /// Top-level items finished.
    pub items_done: u64,
    /// Top-level items in the batch.
    pub items_total: u64,
    /// Files finished inside those items.
    pub files_done: u64,
    /// The user asked for it to stop.
    pub cancelled: bool,
}

impl TransferProgress {
    /// A board for a batch of `items` top-level entries.
    pub fn new(items: u64) -> Arc<Self> {
        let progress = Self::default();
        progress.items_total.store(items, Ordering::Relaxed);
        Arc::new(progress)
    }

    /// Asks the worker to stop; it does so at its next check.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Whether the user asked for it to stop.
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// The error a cancelled operation ends with.
    pub fn cancelled_error(&self) -> Error {
        Error::Cancelled
    }

    /// Adds finished bytes to the bar.
    pub fn add_bytes(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Sets the expected bytes once they are known.
    pub fn set_total(&self, bytes: u64) {
        self.total.store(bytes, Ordering::Relaxed);
    }

    /// Adds to the expected bytes, for a batch counted as it goes.
    pub fn add_total(&self, bytes: u64) {
        self.total.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Notes the entry the worker just started.
    pub fn set_detail(&self, detail: impl Into<String>) {
        if let Ok(mut current) = self.detail.lock() {
            *current = detail.into();
        }
    }

    /// Counts one top-level item finished.
    pub fn item_done(&self) {
        self.items_done.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts `count` top-level items finished, for a batch done in chunks.
    pub fn items_added(&self, count: usize) {
        self.items_done
            .fetch_add(count as u64, Ordering::Relaxed);
    }

    /// Counts one file finished inside an item.
    pub fn file_done(&self) {
        self.files_done.fetch_add(1, Ordering::Relaxed);
    }

    /// What the dialog draws this frame.
    pub fn snapshot(&self) -> TransferSnapshot {
        TransferSnapshot {
            detail: self
                .detail
                .lock()
                .map(|detail| detail.clone())
                .unwrap_or_default(),
            done: self.done.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            items_done: self.items_done.load(Ordering::Relaxed),
            items_total: self.items_total.load(Ordering::Relaxed),
            files_done: self.files_done.load(Ordering::Relaxed),
            cancelled: self.cancelled(),
        }
    }
}

/// Fails with [`Error::Cancelled`] when the user asked the run to stop; called
/// between entries.
pub fn check(progress: Option<&TransferProgress>) -> Result<()> {
    match progress {
        Some(progress) if progress.cancelled() => Err(progress.cancelled_error()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_add_up_and_a_snapshot_copies_them() {
        let progress = TransferProgress::new(3);
        progress.set_total(100);
        progress.add_bytes(40);
        progress.set_detail("a.bin");
        progress.file_done();
        progress.item_done();
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.done, 40);
        assert_eq!(snapshot.total, 100);
        assert_eq!(snapshot.items_done, 1);
        assert_eq!(snapshot.items_total, 3);
        assert_eq!(snapshot.files_done, 1);
        assert_eq!(snapshot.detail, "a.bin");
        assert!(!snapshot.cancelled);
    }

    #[test]
    fn cancelling_is_visible_and_ends_the_run_with_the_cancelled_error() {
        let progress = TransferProgress::new(1);
        assert!(check(Some(&progress)).is_ok());
        progress.cancel();
        assert!(progress.cancelled());
        assert!(progress.snapshot().cancelled);
        assert!(matches!(
            check(Some(&progress)),
            Err(Error::Cancelled)
        ));
        assert!(check(None).is_ok());
    }
}