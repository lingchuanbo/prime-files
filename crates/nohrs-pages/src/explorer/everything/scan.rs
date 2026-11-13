use std::ops::Range;
use std::sync::mpsc;

use super::*;

const SCAN_CHUNK: usize = 16_384;
const SAMPLE_SIZE: usize = 128;
const DENSE_DIVISOR: usize = 8;
type Match = (u32, u16, u32);
pub(super) type Hit = (u32, u16, usize, u32);

pub(super) fn search_index(
    index: Option<&Index>,
    query: &Query,
    fuzzy: bool,
    cancel: &Arc<AtomicBool>,
) -> Vec<Hit> {
    let Some(index) = index else {
        return Vec::new();
    };
    let query = Arc::new(query.clone());
    let (sender, receiver) = mpsc::channel();
    for (source, segment) in index.segments.iter().enumerate() {
        if !segment.in_scope(&query) {
            continue;
        }
        for start in (0..segment.entries.len()).step_by(SCAN_CHUNK) {
            if cancel.load(Ordering::Relaxed) {
                return Vec::new();
            }
            let segment = segment.clone();
            let query = query.clone();
            let cancel = cancel.clone();
            let sender = sender.clone();
            super::workers::submit(move || {
                let end = (start + SCAN_CHUNK).min(segment.entries.len());
                let found = segment.scan_range(start..end, &query, fuzzy, &cancel);
                // 新查询取消后接收端可以提前离开。
                if sender.send((source, found)).is_err() {
                    tracing::trace!("search results receiver closed");
                }
            });
        }
    }
    drop(sender);
    receiver
        .into_iter()
        .flat_map(|(source, found)| {
            found
                .into_iter()
                .map(move |(rank, length, position)| (rank, length, source, position))
        })
        .collect()
}

impl Segment {
    fn in_scope(&self, query: &Query) -> bool {
        !self.paths.is_empty()
            || query.scopes.is_empty()
            || query
                .scopes
                .iter()
                .any(|scope| scope.starts_with(&self.root().to_lowercase()))
    }

    pub(super) fn search(&self, query: &Query, fuzzy: bool, cancel: &AtomicBool) -> Vec<Match> {
        if !self.in_scope(query) {
            return Vec::new();
        }
        (0..self.entries.len())
            .step_by(SCAN_CHUNK)
            .flat_map(|start| {
                self.scan_range(
                    start..(start + SCAN_CHUNK).min(self.entries.len()),
                    query,
                    fuzzy,
                    cancel,
                )
            })
            .collect()
    }

    fn scan_range(
        &self,
        range: Range<usize>,
        query: &Query,
        fuzzy: bool,
        cancel: &AtomicBool,
    ) -> Vec<Match> {
        if cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let mut found = Vec::new();
        let mut check = |position: usize| {
            let entry = &self.entries[position];
            if let Some(rank) = query.rank(self, position as u32, entry, fuzzy) {
                found.push((rank, entry.name_len, position as u32));
            }
        };
        match self.anchor(&range, query, fuzzy) {
            Some(anchor) => {
                self.scan_arena(range.clone(), anchor, cancel, &mut check);
                if query.pinyin {
                    let start = self
                        .non_ascii
                        .partition_point(|&position| (position as usize) < range.start);
                    for &position in &self.non_ascii[start..] {
                        let position = position as usize;
                        if position >= range.end || cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        if !self.lower(&self.entries[position]).contains(anchor) {
                            check(position);
                        }
                    }
                }
            }
            None => {
                for position in range {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    check(position);
                }
            }
        }
        found
    }

    fn anchor<'a>(&self, range: &Range<usize>, query: &'a Query, fuzzy: bool) -> Option<&'a str> {
        if fuzzy {
            return None;
        }
        let sample = &self.entries[range.start..range.end.min(range.start + SAMPLE_SIZE)];
        let (anchor, matching) = query
            .terms
            .iter()
            .filter_map(|term| match term {
                Term::Name(name) if name.len() >= 3 => Some(name.as_str()),
                _ => None,
            })
            .map(|name| {
                let matching = sample
                    .iter()
                    .filter(|entry| self.lower(entry).contains(name))
                    .count();
                (name, matching)
            })
            .min_by_key(|(name, matching)| (*matching, std::cmp::Reverse(name.len())))?;
        // 短词或密集命中逐项扫描更便宜，避免每个命中都做二分定位。
        (matching * DENSE_DIVISOR <= sample.len()).then_some(anchor)
    }

    fn scan_arena(
        &self,
        range: Range<usize>,
        anchor: &str,
        cancel: &AtomicBool,
        mut check: impl FnMut(usize),
    ) {
        let start = range.start;
        let entries = &self.entries[range];
        let Some(first) = entries.first() else {
            return;
        };
        let last = entries.last().unwrap_or(first);
        let end = last.lower as usize + last.lower_len as usize;
        let mut cursor = first.lower as usize;
        while !cancel.load(Ordering::Relaxed) {
            let Some(relative) = self.lower[cursor..end].find(anchor) else {
                break;
            };
            let position = cursor + relative;
            let entry_index = entries.partition_point(|entry| entry.lower as usize <= position) - 1;
            let entry = &entries[entry_index];
            let name_end = entry.lower as usize + entry.lower_len as usize;
            if position + anchor.len() <= name_end {
                check(start + entry_index);
            }
            // 跨文件名的命中不成立；从下个名字重试，保留与它重叠的有效命中。
            cursor = name_end;
        }
    }
}

pub(super) fn best_prefix<T: Ord>(items: &mut [T], count: usize) -> &mut [T] {
    let count = count.min(items.len());
    if count < items.len() {
        items.select_nth_unstable(count);
    }
    let best = &mut items[..count];
    best.sort_unstable();
    best
}

pub(super) fn unique_targets(
    mut hits: Vec<Hit>,
    sources: &[(usize, &Segment)],
    cancel: &AtomicBool,
) -> Vec<(String, String, bool)> {
    let mut shown = std::collections::HashSet::new();
    let mut targets = Vec::new();
    let mut remaining = hits.as_mut_slice();
    while !remaining.is_empty() && targets.len() < MAX_RESULTS && !cancel.load(Ordering::Relaxed) {
        let count = remaining.len().min(MAX_RESULTS - targets.len());
        best_prefix(remaining, count);
        let (best, rest) = remaining.split_at_mut(count);
        for &mut (_, _, source, position) in best {
            let Some((_, segment)) = sources.iter().find(|(id, _)| *id == source) else {
                continue;
            };
            let Some(entry) = segment.entries.get(position as usize) else {
                continue;
            };
            let path = segment.path(position);
            if shown.insert(path.to_lowercase()) {
                targets.push((
                    segment.name(entry).to_string(),
                    path,
                    entry.flags & DIR != 0,
                ));
            }
        }
        // 重复来源去重后不足一屏时，继续取下一段，不能先截断再去重。
        remaining = rest;
    }
    targets
}
