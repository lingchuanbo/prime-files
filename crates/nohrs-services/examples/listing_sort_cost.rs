//! What the case-insensitive sort inside `list_dir_sync` costs on a real
//! folder: the listing is read once, then the sort alone is timed on copies of
//! it in the order `read_dir` gave, median of 7.
//!
//! ```text
//! cargo run --release -p nohrs-services --example listing_sort_cost -- <dir>
//! ```

#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]
#![allow(missing_docs)]

use nohrs_services::fs::listing::{ListParams, list_dir_sync};
use std::time::Instant;

fn main() {
    let dir = std::env::args().nth(1).expect("a folder");
    let mut listed = list_dir_sync(ListParams {
        path: &dir,
        limit: usize::MAX,
        cursor: None,
    })
    .unwrap()
    .entries;
    // Back to the order `read_dir` hands them in, which is what the sort sees.
    let order: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let position: std::collections::HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    listed.sort_by_key(|entry| {
        position
            .get(entry.name.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });

    let mut samples = Vec::new();
    for _ in 0..7 {
        let mut entries = listed.clone();
        let start = Instant::now();
        entries.sort_by_cached_key(|entry| entry.name.to_lowercase());
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        std::hint::black_box(&entries);
    }
    samples.sort_by(f64::total_cmp);
    let whole = {
        let start = Instant::now();
        let listing = list_dir_sync(ListParams {
            path: &dir,
            limit: usize::MAX,
            cursor: None,
        })
        .unwrap();
        std::hint::black_box(&listing.entries);
        start.elapsed().as_secs_f64() * 1000.0
    };
    println!(
        "{dir}: {} itens, ordenação {:.1} ms (mediana), listagem inteira {:.1} ms",
        listed.len(),
        samples[samples.len() / 2],
        whole
    );
}
