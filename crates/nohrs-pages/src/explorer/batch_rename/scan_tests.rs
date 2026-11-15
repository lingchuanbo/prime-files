#![allow(clippy::disallowed_methods)]
use super::*;
use std::{fs, sync::atomic::Ordering};

fn fixture() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("旅行/深层")).unwrap();
    for name in [
        "封面.PNG",
        "说明.txt",
        "旅行/天空.png",
        "旅行/海边.jpg",
        "旅行/深层/图标.webp",
    ] {
        fs::write(directory.path().join(name), "safe").unwrap();
    }
    directory
}

#[test]
fn directory_scan_filters_formats_and_only_recurses_when_enabled() {
    let directory = fixture();
    let root = directory.path().to_string_lossy();
    let direct = scan(&root, false, "png", &Progress::default()).unwrap();
    assert_eq!(1, direct.entries.len());
    assert_eq!("封面.PNG", direct.entries[0].name);
    let recursive = scan(&root, true, "*.png;.webp", &Progress::default()).unwrap();
    assert_eq!(3, recursive.entries.len());
    assert!(recursive.entries.iter().all(|entry| entry.kind == "file"));
    assert!(recursive.issues.is_empty());
}

#[test]
fn overlapping_roots_are_deduplicated_and_results_are_stable() {
    let directory = fixture();
    let roots = format!(
        "{} | {} | {}",
        directory.path().display(),
        directory.path().join("旅行").display(),
        directory.path().display()
    );
    let progress = Progress::default();
    let result = scan(&roots, true, "", &progress).unwrap();
    assert_eq!(5, result.entries.len());
    assert_eq!(5, progress.found.load(Ordering::Relaxed));
    let again = scan(&roots, true, "*.*", &Progress::default()).unwrap();
    assert_eq!(
        result
            .entries
            .iter()
            .map(|entry| &entry.path)
            .collect::<Vec<_>>(),
        again
            .entries
            .iter()
            .map(|entry| &entry.path)
            .collect::<Vec<_>>()
    );
}

#[test]
fn invalid_format_and_missing_roots_are_not_silently_treated_as_all_files() {
    let directory = fixture();
    for format in [";", "png,jpg", "../png", "*.png;;*.jpg", "*png"] {
        assert!(
            scan(
                &directory.path().to_string_lossy(),
                true,
                format,
                &Progress::default()
            )
            .is_err(),
            "{format}"
        );
    }
    assert!(scan("", true, "", &Progress::default()).is_err());
    let missing = scan(
        &directory.path().join("missing").to_string_lossy(),
        true,
        "",
        &Progress::default(),
    )
    .unwrap();
    assert_eq!(1, missing.issues.len());
    assert!(missing.entries.is_empty());
}

#[test]
fn cancelled_scan_never_produces_an_executable_partial_result() {
    let directory = fixture();
    let progress = Progress::default();
    progress.cancel.store(true, Ordering::Relaxed);
    assert!(scan(&directory.path().to_string_lossy(), true, "", &progress).is_err());
}

#[test]
fn relative_labels_use_only_requested_roots_including_overlapping_roots() {
    let directory = fixture();
    let root = directory.path();
    let roots = format!("{} | {}", root.display(), root.join("旅行").display());
    let result = scan(&roots, true, "", &Progress::default()).unwrap();
    assert_eq!(vec![fs::canonicalize(root).unwrap()], result.roots);
}

#[cfg(windows)]
#[test]
fn explicitly_selected_junction_is_skipped_without_following_its_target() {
    let directory = fixture();
    let link = directory.path().join("junction");
    let status = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&link)
        .arg(directory.path().join("旅行"))
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let result = scan(&link.to_string_lossy(), true, "", &Progress::default()).unwrap();
    assert!(result.entries.is_empty());
    assert!(result.issues.iter().any(|issue| issue.contains("重解析点")));
}
