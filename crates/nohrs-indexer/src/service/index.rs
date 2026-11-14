use crate::protocol;

mod tracked;
use std::{
    io,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};
use tracked::Tracked;

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
            changed |= add_missing(&mut tracked, &roots, index);
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

fn add_missing(tracked: &mut Vec<Tracked>, roots: &[String], index: &Index) -> bool {
    let mut changed = false;
    for root in roots
        .iter()
        .filter(|root| !tracked.iter().any(|volume| &volume.root == *root))
        .cloned()
        .collect::<Vec<_>>()
    {
        if index.generation().is_none() {
            return changed;
        }
        match Tracked::build(&root, index) {
            Ok(volume) => {
                tracked.push(volume);
                changed = true;
            }
            Err(error) => super::log(&format!("{root}: initial index failed: {error}")),
        }
    }
    changed
}
