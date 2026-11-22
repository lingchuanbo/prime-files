//! A/B timing of moving a selection to the trash, alternating what the
//! explorer did before 04/11/2026 (kept literally in `legacy`: one
//! `trash_path` per item) with the current `trash_paths`, which hands the
//! whole selection to the system in one call.
//!
//! ```text
//! cargo run --release -p nohrs-services --example trash_ab
//! ```
//!
//! This uses the real Recycle Bin. The files are created in a folder of the
//! run's own under the system temp folder, and at the end the items whose
//! original location is that folder — and only those — are purged from the
//! Recycle Bin. Before timing, each side's result is checked: every file gone
//! and no failure reported; a side that answers otherwise is not timed.

#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]
#![allow(missing_docs)]

use nohrs_services::fs::ops;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const ROUNDS: usize = 3;

mod legacy {
    //! `trash_selection` before the change, on Windows, where the record is
    //! `None`: one `trash_path` per selected item, each one its own thread,
    //! its own COM apartment and its own `IFileOperation`.
    use nohrs_services::fs::ops;
    use std::path::PathBuf;

    pub fn trash(paths: &[PathBuf]) -> Vec<String> {
        let mut failed = Vec::new();
        for path in paths {
            if let Err(error) = ops::trash_path(path, None) {
                failed.push(format!("{}: {error}", path.display()));
            }
        }
        failed
    }
}

fn make_files(dir: &Path, count: usize) -> Vec<PathBuf> {
    fs::create_dir_all(dir).unwrap();
    (0..count)
        .map(|index| {
            let path = dir.join(format!("arquivo {index:04}.txt"));
            fs::write(&path, "x").unwrap();
            path
        })
        .collect()
}

fn check(side: &str, paths: &[PathBuf], failed: &[String]) {
    assert!(failed.is_empty(), "{side}: {failed:?}");
    let left = paths.iter().filter(|path| path.exists()).count();
    assert_eq!(left, 0, "{side}: {left} files were not trashed");
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn purge_ours(base: &Path) {
    let items = trash::os_limited::list().unwrap();
    let ours: Vec<_> = items
        .into_iter()
        .filter(|item| item.original_parent.starts_with(base))
        .collect();
    let count = ours.len();
    trash::os_limited::purge_all(ours).unwrap();
    println!("removidos da Lixeira: {count} itens criados por esta rodada");
}

fn main() {
    let base = std::env::temp_dir().join(format!("nohrs-trash-ab-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    println!(
        "{} rodadas alternadas, mediana, em {}",
        ROUNDS,
        base.display()
    );

    for count in [1, 10, 200, 1000] {
        let mut before = Vec::new();
        let mut after = Vec::new();
        for round in 0..ROUNDS {
            let paths = make_files(&base.join(format!("antes-{count}-{round}")), count);
            let start = Instant::now();
            let failed = legacy::trash(&paths);
            before.push(start.elapsed().as_secs_f64() * 1000.0);
            check("antes", &paths, &failed);

            let paths = make_files(&base.join(format!("depois-{count}-{round}")), count);
            let start = Instant::now();
            let failed: Vec<String> = ops::trash_paths(&paths, None)
                .into_iter()
                .map(|(path, error)| format!("{}: {error}", path.display()))
                .collect();
            after.push(start.elapsed().as_secs_f64() * 1000.0);
            check("depois", &paths, &failed);
        }
        println!(
            "{:>5} arquivos   antes {:>10.1} ms   depois {:>8.1} ms",
            count,
            median(before),
            median(after)
        );
    }

    purge_ours(&base);
    fs::remove_dir_all(&base).unwrap();
}
