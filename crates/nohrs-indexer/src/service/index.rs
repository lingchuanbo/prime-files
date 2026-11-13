use super::journal::{Cursor, Journal};
use crate::{mft, protocol};
use std::{
    io,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

type Snapshot = Vec<Arc<protocol::Volume>>;

#[derive(Default)]
struct Published {
    volumes: Option<Arc<Snapshot>>,
    generation: u64,
    stopped: bool,
}

#[derive(Default)]
pub(super) struct Index {
    state: Mutex<Published>,
    changed: Condvar,
}

impl Index {
    pub fn snapshot(&self) -> io::Result<Arc<Snapshot>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(volumes) = &state.volumes {
                return Ok(volumes.clone());
            }
            if state.stopped {
                return Err(io::Error::other("indexer stopped"));
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub fn refresh(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.wrapping_add(1);
        self.changed.notify_all();
    }

    pub fn stop(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.stopped = true;
        self.changed.notify_all();
    }

    fn generation(&self) -> Option<u64> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (!state.stopped).then_some(state.generation)
    }

    fn publish(&self, volumes: Snapshot) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = state.volumes.replace(Arc::new(volumes));
        drop(state);
        self.changed.notify_all();
        // 释放数百万条旧名称可能耗时，不能占着搜索读取快照的锁。
        drop(previous);
    }

    fn pause(&self, generation: u64) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = self.changed.wait_timeout_while(state, POLL, |state| {
            !state.stopped && state.generation == generation
        });
        if result.is_err() {
            super::log("index worker wait poisoned");
        }
    }
}

const POLL: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(30);
const FALLBACK_REFRESH: Duration = Duration::from_secs(30 * 60);

struct Tracked {
    root: String,
    policy: mft::Policy,
    table: mft::Table,
    volume: Arc<protocol::Volume>,
    journal: Option<(Journal, Cursor)>,
    built: Instant,
    retry: Option<Instant>,
}

impl Tracked {
    fn build(root: &str, index: &Index) -> io::Result<Self> {
        let profile = super::ALLOWED
            .get()
            .and_then(|sid| super::profile_path(sid));
        let policy = super::policy_for(root, profile.as_deref());
        // 在全量读取前记住游标，之后回放扫描期间的变化。
        let journal = match Journal::open(root)
            .and_then(|journal| journal.state().map(|state| (journal, state)))
        {
            Ok((journal, state)) => Some((
                journal,
                Cursor {
                    id: state.id,
                    next: state.next,
                },
            )),
            Err(error) => {
                super::log(&format!(
                    "{root}: journal unavailable, periodic rebuild: {error}"
                ));
                None
            }
        };
        let mut table = mft::read_volume_interruptible(root, || index.generation().is_none())?;
        let mut journal = journal;
        let mut needs_retry = false;
        if let Some((reader, cursor)) = journal.as_mut() {
            if let Err(error) = catch_up(&mut table, reader, cursor) {
                super::log(&format!(
                    "{root}: startup catch-up failed, taking fresh baseline: {error}"
                ));
                table = mft::read_volume_interruptible(root, || index.generation().is_none())?;
                journal = None;
                needs_retry = true;
            }
        }
        let volume = Arc::new(mft::tree(&table, root, &policy));
        super::log(&format!(
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

    fn poll(&mut self, index: &Index) -> io::Result<bool> {
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

    fn rebuild(&mut self, index: &Index) -> io::Result<bool> {
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
    let mut changes = std::collections::BTreeMap::new();
    for number in numbers {
        changes.insert(number, journal.record(number)?);
    }
    let changed = !changes.is_empty();
    if changed {
        mft::updates::apply(table, changes)?;
    }
    *cursor = next;
    Ok(changed)
}

pub(super) fn run(index: &Index) {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    if let Err(error) =
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) }
    {
        super::log(&format!("could not lower index worker priority: {error}"));
    }
    let mut tracked: Vec<Tracked> = Vec::new();
    let mut applied_generation = None;
    let mut missing_retry = None;
    while let Some(generation) = index.generation() {
        let forced = applied_generation != Some(generation);
        let roots = super::ntfs_volumes();
        let before = tracked.len();
        tracked.retain(|volume| roots.contains(&volume.root));
        let mut changed = before != tracked.len();
        for volume in &mut tracked {
            if index.generation().is_none() {
                return;
            }
            changed |= update(volume, forced, index);
        }
        if forced || missing_retry.is_none_or(|last: Instant| last.elapsed() >= RETRY) {
            for root in roots
                .iter()
                .filter(|root| !tracked.iter().any(|volume| &volume.root == *root))
                .cloned()
                .collect::<Vec<_>>()
            {
                if index.generation().is_none() {
                    return;
                }
                match Tracked::build(&root, index) {
                    Ok(volume) => {
                        tracked.push(volume);
                        changed = true;
                    }
                    Err(error) => super::log(&format!("{root}: initial index failed: {error}")),
                }
            }
            missing_retry = Some(Instant::now());
        }
        if changed || applied_generation.is_none() {
            index.publish(tracked.iter().map(|volume| volume.volume.clone()).collect());
        }
        applied_generation = Some(generation);
        index.pause(generation);
    }
}

fn update(volume: &mut Tracked, forced: bool, index: &Index) -> bool {
    if !forced && volume.retry.is_some_and(|last| last.elapsed() < RETRY) {
        return false;
    }
    let result = if forced || volume.retry.is_some() {
        volume.rebuild(index)
    } else {
        volume.poll(index)
    };
    match result {
        Ok(changed) => changed,
        Err(error) => {
            super::log(&format!(
                "{}: update failed, retaining snapshot and scheduling rebuild: {error}",
                volume.root
            ));
            volume.retry = Some(Instant::now());
            false
        }
    }
}

#[cfg(test)]
#[path = "index_tests.rs"]
mod tests;
