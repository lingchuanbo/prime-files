//! A/B timing of a move within one volume — what Cut + Paste, a drop and the
//! overwrite's set-aside step do — alternating the move as it ran on Windows
//! before 04/11/2026 (kept literally in `legacy`) with the current
//! `move_path_no_replace`.
//!
//! Until then Windows had no no-replace rename here, so every move took the
//! copy-and-delete fallback even with source and destination on the same
//! drive: the bytes were written a second time, the drive needed room for a
//! second copy, and the time grew with the size of the tree instead of being
//! one rename.
//!
//! ```text
//! cargo run --release -p nohrs-services --example move_ab -- [<folder on the volume to test>]
//! ```
//!
//! The trees are built under the folder given (the system temp folder by
//! default) and removed at the end. Before timing, each side moves the tree once
//! and the result is compared with the original file by file (relative path,
//! size, first and last bytes); the run refuses to time a side whose move
//! answers something else.

#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]
#![allow(missing_docs)]

use nohrs_services::fs::ops;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const ROUNDS: usize = 5;

mod legacy {
    //! `move_path_no_replace` as it ran on Windows before the change: the
    //! no-replace rename answered `None`, so the copy and the delete always
    //! ran. The destination-inside-source check it also did is private and
    //! costs a few path comparisons; left out.
    use nohrs_services::fs::ops;
    use std::path::Path;

    pub fn move_no_replace(src: &Path, dst: &Path) -> nohrs_core::errors::Result<()> {
        ops::copy_path_no_replace(src, dst).map_err(ops::ClaimFailure::into_error)?;
        ops::delete_permanent(src)?;
        Ok(())
    }
}

type Manifest = Vec<(PathBuf, u64, Vec<u8>)>;

// Relative path, size, and the first and last 4 KB of every file: enough to
// tell a whole copy from a truncated or misplaced one without hashing gigabytes.
fn manifest(root: &Path) -> Manifest {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
                continue;
            }
            let mut file = fs::File::open(&path).unwrap();
            let size = file.metadata().unwrap().len();
            let mut edges = vec![0u8; size.min(4096) as usize];
            file.read_exact(&mut edges).unwrap();
            if size > 4096 {
                let mut tail = vec![0u8; 4096];
                file.seek(SeekFrom::End(-4096)).unwrap();
                file.read_exact(&mut tail).unwrap();
                edges.extend(tail);
            }
            out.push((path.strip_prefix(root).unwrap().to_path_buf(), size, edges));
        }
    }
    out.sort();
    out
}

// Bytes that differ from file to file, so a file landing under another name
// changes the manifest.
fn write_file(path: &Path, size: usize, seed: u64) {
    let mut file = fs::File::create(path).unwrap();
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut block = vec![0u8; 1 << 20];
    let mut left = size;
    while left > 0 {
        for byte in block.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        let take = left.min(block.len());
        file.write_all(&block[..take]).unwrap();
        left -= take;
    }
}

struct Case {
    name: &'static str,
    build: fn(&Path),
}

fn one_small_file(root: &Path) {
    fs::create_dir_all(root).unwrap();
    write_file(&root.join("nota.txt"), 1024, 1);
}

fn photo_folder(root: &Path) {
    // 2,000 files of 64 KB in 20 subfolders: a folder of photos or samples.
    for folder in 0..20u64 {
        let dir = root.join(format!("pasta {folder:02}"));
        fs::create_dir_all(&dir).unwrap();
        for file in 0..100u64 {
            write_file(
                &dir.join(format!("arquivo {file:03}.bin")),
                64 * 1024,
                folder * 1000 + file,
            );
        }
    }
}

fn one_big_file(root: &Path) {
    // A video-sized file.
    fs::create_dir_all(root).unwrap();
    write_file(&root.join("video.mp4"), 512 << 20, 7);
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn main() {
    let base = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("nohrs-move-ab-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();

    let cases = [
        Case {
            name: "1 arquivo de 1 KB",
            build: one_small_file,
        },
        Case {
            name: "2.000 arquivos de 64 KB em 20 pastas",
            build: photo_folder,
        },
        Case {
            name: "1 arquivo de 512 MB",
            build: one_big_file,
        },
    ];

    println!(
        "{} rodadas alternadas, mediana, em {}",
        ROUNDS,
        base.display()
    );
    for case in &cases {
        let here = base.join("a");
        let there = base.join("b");
        (case.build)(&here);
        let original = manifest(&here);

        // Each side moves the tree once and must hand back the same tree.
        legacy::move_no_replace(&here, &there).unwrap();
        assert!(
            !here.exists(),
            "{}: the old move left the source",
            case.name
        );
        assert_eq!(
            manifest(&there),
            original,
            "{}: the old move changed the tree",
            case.name
        );
        ops::move_path_no_replace(&there, &here)
            .map_err(ops::ClaimFailure::into_error)
            .unwrap();
        assert!(
            !there.exists(),
            "{}: the new move left the source",
            case.name
        );
        assert_eq!(
            manifest(&here),
            original,
            "{}: the new move changed the tree",
            case.name
        );

        let mut before = Vec::new();
        let mut after = Vec::new();
        for _ in 0..ROUNDS {
            let start = Instant::now();
            legacy::move_no_replace(&here, &there).unwrap();
            before.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            ops::move_path_no_replace(&there, &here)
                .map_err(ops::ClaimFailure::into_error)
                .unwrap();
            after.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        assert_eq!(
            manifest(&here),
            original,
            "{}: the tree changed over the rounds",
            case.name
        );
        println!(
            "{:<40} antes {:>10.2} ms   depois {:>8.3} ms",
            case.name,
            median(before),
            median(after)
        );
        fs::remove_dir_all(&here).unwrap();
    }
    fs::remove_dir_all(&base).unwrap();
}
