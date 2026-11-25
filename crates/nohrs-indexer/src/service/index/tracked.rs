use super::{FALLBACK_REFRESH, Index, RETRY, UNMERGEABLE_REBUILD};
use crate::{
    mft, protocol,
    service::{
        self,
        journal::{Cursor, Journal},
    },
};
use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) struct Tracked {
    pub(super) root: String,
    policy: mft::Policy,
    table: mft::Table,
    pub(super) volume: Arc<protocol::Volume>,
    pub(super) journal: Option<(Journal, Cursor)>,
    built: Instant,
    pub(super) retry: Option<Instant>,
    /// How long to wait after a failure before reading the volume again.
    pub(super) retry_after: Duration,
}

impl Tracked {
    pub(super) fn build(root: &str, index: &Index) -> io::Result<Self> {
        let profile = service::ALLOWED
            .get()
            .and_then(|sid| service::profile_path(sid));
        let policy = service::policy_for(root, profile.as_deref());
        // 在全量读取前记住游标，之后回放扫描期间的变化。
        let journal = open_cursor(root);
        let mut table = mft::read_volume_interruptible(root, || index.generation().is_none())?;
        let mut journal = journal;
        let mut needs_retry = false;
        let mut slow_retry = false;
        if let Some((reader, cursor)) = journal.as_mut()
            && let Err(error) = catch_up(&mut table, reader, cursor)
        {
            service::log(&format!(
                "{root}: startup catch-up failed, taking fresh baseline: {error}"
            ));
            // A batch only a whole read answers will be refused again on the
            // next change to the same record, so the next attempt waits.
            slow_retry = error.kind() == io::ErrorKind::Unsupported;
            table = mft::read_volume_interruptible(root, || index.generation().is_none())?;
            journal = None;
            needs_retry = true;
        }
        let volume = Arc::new(mft::tree(&table, root, &policy));
        // Without the journal there is nothing to catch up on: the volume is
        // rebuilt from the drive every half hour, and a rebuild reads the
        // table again from the volume. Keeping it would hold a second copy of
        // every name on the drive, which is what a drive this size costs.
        let table = if journal.is_some() {
            table
        } else {
            mft::Table::default()
        };
        service::log(&format!(
            "{root}: {} names, {} journal tracking",
            volume.len(),
            if journal.is_some() { "with" } else { "without" }
        ));
        let retry = needs_retry.then(Instant::now);
        Ok(Self {
            root: root.into(),
            policy,
            table,
            volume,
            journal,
            built: Instant::now(),
            retry,
            retry_after: if slow_retry {
                UNMERGEABLE_REBUILD
            } else {
                RETRY
            },
        })
    }

    pub(super) fn poll(&mut self, index: &Index) -> io::Result<bool> {
        if let Some((reader, cursor)) = &mut self.journal {
            match catch_up(&mut self.table, reader, cursor) {
                Ok(false) => return Ok(false),
                Ok(true) => {}
                // A record keeping part of its attributes outside its record:
                // the journal cannot answer for it, so the volume is read
                // whole. Waiting longer here is what stops a volume whose
                // files keep changing from spending its time rebuilding.
                Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                    service::log(&format!("{}: {error}", self.root));
                    self.retry = Some(Instant::now());
                    self.retry_after = UNMERGEABLE_REBUILD;
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
            self.volume = Arc::new(mft::tree(&self.table, &self.root, &self.policy));
            Ok(true)
        } else if self.built.elapsed() >= FALLBACK_REFRESH {
            self.rebuild(index)
        } else {
            Ok(false)
        }
    }

    pub(super) fn rebuild(&mut self, index: &Index) -> io::Result<bool> {
        match Self::build(&self.root, index) {
            Ok(replacement) => {
                *self = replacement;
                Ok(true)
            }
            Err(error) => {
                self.retry = Some(Instant::now());
                Err(error)
            }
        }
    }
}

fn catch_up(table: &mut mft::Table, journal: &Journal, cursor: &mut Cursor) -> io::Result<bool> {
    let (next, numbers) = journal.changes(*cursor)?;
    let changed = if numbers.is_empty() {
        false
    } else {
        // The journal names records in the 48 bits a file reference carries;
        // one past the 32 bits a table holds is a corrupt reference, and the
        // only safe answer to it is a fresh baseline.
        let numbers = numbers
            .into_iter()
            .map(|number| {
                mft::record_of(number).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("MFT record {number} is past what a table numbers"),
                    )
                })
            })
            .collect::<io::Result<std::collections::BTreeSet<u32>>>()?;
        mft::updates::refresh(table, numbers, |number| journal.record(u64::from(number)))?
    };
    *cursor = next;
    Ok(changed)
}

fn open_cursor(root: &str) -> Option<(Journal, Cursor)> {
    match Journal::open(root).and_then(|journal| journal.state().map(|state| (journal, state))) {
        Ok((journal, state)) => Some((
            journal,
            Cursor {
                id: state.id,
                next: state.next,
            },
        )),
        Err(error) => {
            service::log(&format!(
                "{root}: journal unavailable, periodic rebuild: {error}"
            ));
            None
        }
    }
}
