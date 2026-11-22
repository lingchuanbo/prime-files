//! A/B timing of the copy Paste does (`copy_path_no_replace`), alternating
//! the copy as it ran before 04/11/2026 (kept literally in `legacy`: claim the
//! name with `create_new`, then `std::io::copy`) with the current one.
//!
//! ```text
//! cargo run --release -p nohrs-services --example copy_ab -- [<destination folder>…]
//! ```
//!
//! The source trees are built under the system temp folder; each destination
//! folder given (the temp folder by default) gets the copies, which are removed
//! after each round. Before timing, each side's copy is compared with the
//! source file by file (relative path, size, first and last bytes), and the run
//! refuses to time a side that copies something else. It also says whether the
//! copy kept the files' modification time, which the old copy did not.

#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]
#![allow(missing_docs)]

use nohrs_services::fs::ops;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const ROUNDS: usize = 5;

mod legacy {
    //! `copy_path_no_replace` before the change, for a file and for a tree of
    //! files (`claim_file` + `fill_file`, `claim_dir` + `fill_dir` under
    //! `Existing::Refuse`). The destination-inside-source check is private
    //! and costs a few path comparisons; left out.
    use std::fs;
    use std::path::Path;

    pub fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
        if fs::symlink_metadata(from)?.is_dir() {
            fs::create_dir(to)?;
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                copy(&entry.path(), &to.join(entry.file_name()))?;
            }
            fs::set_permissions(to, fs::metadata(from)?.permissions())?;
            return Ok(());
        }
        let mut source = fs::File::open(from)?;
        let mut destination = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)?;
        std::io::copy(&mut source, &mut destination)?;
        fs::set_permissions(to, fs::metadata(from)?.permissions())?;
        Ok(())
    }
}

type Manifest = Vec<(PathBuf, u64, Vec<u8>)>;

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

fn manifest(root: &Path) -> Manifest {
    files_under(root)
        .into_iter()
        .map(|path| {
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
            (path.strip_prefix(root).unwrap().to_path_buf(), size, edges)
        })
        .collect()
}

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
    // A date far from now, so a copy that rewrites it shows.
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000))
        .unwrap();
}

fn photo_folder(root: &Path) {
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
    fs::create_dir_all(root).unwrap();
    write_file(&root.join("video.mp4"), 512 << 20, 7);
}

fn keeps_dates(source: &Path, copy: &Path) -> bool {
    files_under(source)
        .iter()
        .zip(files_under(copy))
        .all(|(from, to)| {
            fs::metadata(from).unwrap().modified().unwrap()
                == fs::metadata(to).unwrap().modified().unwrap()
        })
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn main() {
    let pid = std::process::id();
    let sources = std::env::temp_dir().join(format!("nohrs-copy-ab-src-{pid}"));
    let mut destinations: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if destinations.is_empty() {
        destinations.push(std::env::temp_dir());
    }

    type Build = fn(&Path);
    let cases: [(&str, Build); 2] = [
        ("2.000 arquivos de 64 KB em 20 pastas", photo_folder),
        ("1 arquivo de 512 MB", one_big_file),
    ];
    println!(
        "{ROUNDS} rodadas alternadas, mediana; origem em {}",
        sources.display()
    );
    for (index, (name, build)) in cases.iter().enumerate() {
        let source = sources.join(format!("caso-{index}"));
        build(&source);
        let original = manifest(&source);
        for destination in &destinations {
            let base = destination.join(format!("nohrs-copy-ab-dst-{pid}"));
            fs::create_dir_all(&base).unwrap();
            let old = base.join("antes");
            let new = base.join("depois");

            legacy::copy(&source, &old).unwrap();
            assert_eq!(
                manifest(&old),
                original,
                "{name}: the old copy changed the tree"
            );
            let old_dates = keeps_dates(&source, &old);
            fs::remove_dir_all(&old).unwrap();
            ops::copy_path_no_replace(&source, &new)
                .map_err(ops::ClaimFailure::into_error)
                .unwrap();
            assert_eq!(
                manifest(&new),
                original,
                "{name}: the new copy changed the tree"
            );
            let new_dates = keeps_dates(&source, &new);
            fs::remove_dir_all(&new).unwrap();

            let mut before = Vec::new();
            let mut after = Vec::new();
            for _ in 0..ROUNDS {
                let start = Instant::now();
                legacy::copy(&source, &old).unwrap();
                before.push(start.elapsed().as_secs_f64() * 1000.0);
                fs::remove_dir_all(&old).unwrap();
                let start = Instant::now();
                ops::copy_path_no_replace(&source, &new)
                    .map_err(ops::ClaimFailure::into_error)
                    .unwrap();
                after.push(start.elapsed().as_secs_f64() * 1000.0);
                fs::remove_dir_all(&new).unwrap();
            }
            println!(
                "{name:<38} -> {:<12} antes {:>9.1} ms   depois {:>9.1} ms   data mantida: antes {}, depois {}",
                destination.display().to_string(),
                median(before),
                median(after),
                if old_dates { "sim" } else { "não" },
                if new_dates { "sim" } else { "não" },
            );
            fs::remove_dir_all(&base).unwrap();
        }
    }
    fs::remove_dir_all(&sources).unwrap();
}
