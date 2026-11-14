use super::{FALLBACK_REFRESH, Index};
use crate::{
    mft, protocol,
    service::{
        self,
        journal::{Cursor, Journal},
    },
};
use std::{io, sync::Arc, time::Instant};

pub(super) struct Tracked {
    pub(super) root: String,
    policy: mft::Policy,
    table: mft::Table,
    pub(super) volume: Arc<protocol::Volume>,
    pub(super) journal: Option<(Journal, Cursor)>,
    built: Instant,
    pub(super) retry: Option<Instant>,
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
        if let Some((reader, cursor)) = journal.as_mut()
            && let Err(error) = catch_up(&mut table, reader, cursor)
        {
            service::log(&format!(
                "{root}: startup catch-up failed, taking fresh baseline: {error}"
            ));
            table = mft::read_volume_interruptible(root, || index.generation().is_none())?;
            journal = None;
            needs_retry = true;
        }
        let volume = Arc::new(mft::tree(&table, root, &policy));
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
        })
    }

    pub(super) fn poll(&mut self, index: &Index) -> io::Result<bool> {
        if let Some((reader, cursor)) = &mut self.journal {
            if !catch_up(&mut self.table, reader, cursor)? {
                return Ok(false);
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
    let changed = !numbers.is_empty();
    if changed {
        mft::updates::refresh(table, numbers, |number| journal.record(number))?;
    }
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
