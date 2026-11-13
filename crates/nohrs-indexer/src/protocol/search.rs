use std::ops::Range;

use super::*;

const MIN_PARALLEL_ENTRIES: usize = 32_768;
const SAMPLE_SIZE: usize = 128;
const DENSE_DIVISOR: usize = 8;

impl Volume {
    /// Adds matching names in table order, stopping at `limit`, including
    /// entries already in `out`. Terms must already be lowercased.
    pub fn search(&self, terms: &[String], limit: usize, out: &mut Vec<Hit>) {
        if terms.is_empty() || out.len() >= limit {
            return;
        }
        let remaining = limit - out.len();
        let threads = std::thread::available_parallelism()
            .map_or(4, usize::from)
            .clamp(1, 16);
        let chunk = self
            .entries
            .len()
            .div_ceil(threads)
            .max(MIN_PARALLEL_ENTRIES);
        let matched = if self.entries.len() <= chunk {
            self.matching(0..self.entries.len(), terms, remaining)
        } else {
            self.parallel_matching(terms, remaining, chunk)
        };
        for position in matched.into_iter().take(remaining) {
            let entry = &self.entries[position];
            out.push(Hit {
                path: self.path(position as u32),
                dir: entry.dir,
                size: entry.size,
                modified: entry.modified,
            });
        }
    }

    fn parallel_matching(&self, terms: &[String], limit: usize, chunk: usize) -> Vec<usize> {
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..self.entries.len())
                .step_by(chunk)
                .map(|start| {
                    scope.spawn(move || {
                        self.matching(start..(start + chunk).min(self.entries.len()), terms, limit)
                    })
                })
                .collect();
            // 分块天然保持表顺序，每块只保留至多 limit 条，无需收集全盘命中再排序。
            let chunks: Vec<_> = workers
                .into_iter()
                .map(|worker| match worker.join() {
                    Ok(found) => found,
                    Err(_) => {
                        eprintln!("index search worker panicked");
                        Vec::new()
                    }
                })
                .collect();
            chunks.into_iter().flatten().take(limit).collect()
        })
    }

    fn matching(&self, range: Range<usize>, terms: &[String], limit: usize) -> Vec<usize> {
        let mut found = Vec::new();
        let mut check = |position: usize| {
            let entry = &self.entries[position];
            if entry.parent != NO_PARENT
                && terms
                    .iter()
                    .all(|term| self.lower_of(entry).contains(term.as_str()))
            {
                found.push(position);
                return found.len() >= limit;
            }
            false
        };
        match self.anchor(&range, terms) {
            Some(anchor) => self.scan_arena(range, anchor, &mut check),
            None => {
                for position in range {
                    if check(position) {
                        break;
                    }
                }
            }
        }
        found
    }

    fn anchor<'a>(&self, range: &Range<usize>, terms: &'a [String]) -> Option<&'a str> {
        let sample = &self.entries[range.start..range.end.min(range.start + SAMPLE_SIZE)];
        let (anchor, matching) = terms
            .iter()
            .filter(|term| term.len() >= 3)
            .map(|term| {
                let matching = sample
                    .iter()
                    .filter(|entry| self.lower_of(entry).contains(term.as_str()))
                    .count();
                (term.as_str(), matching)
            })
            .min_by_key(|(term, matching)| (*matching, std::cmp::Reverse(term.len())))?;
        (matching * DENSE_DIVISOR <= sample.len()).then_some(anchor)
    }

    fn scan_arena(&self, range: Range<usize>, anchor: &str, mut check: impl FnMut(usize) -> bool) {
        let start = range.start;
        let entries = &self.entries[range];
        let Some(first) = entries.first() else {
            return;
        };
        let last = entries.last().unwrap_or(first);
        let end = last.lower_start as usize + last.lower_len as usize;
        let mut cursor = first.lower_start as usize;
        while let Some(relative) = self.lower[cursor..end].find(anchor) {
            let position = cursor + relative;
            let entry_index =
                entries.partition_point(|entry| entry.lower_start as usize <= position) - 1;
            let entry = &entries[entry_index];
            let name_end = entry.lower_start as usize + entry.lower_len as usize;
            if position + anchor.len() <= name_end && check(start + entry_index) {
                break;
            }
            // 跨名字的命中从下个名字重试，不能用不重叠迭代器跳过有效匹配。
            cursor = name_end;
        }
    }
}
