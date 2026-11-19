#![allow(clippy::disallowed_methods)]
use super::{
    plan,
    rules::Rules,
    scan::{self, Progress},
    transaction,
};
use std::{collections::HashSet, fs};

#[test]
fn selected_files_drive_continuous_or_per_directory_counters_and_undo() {
    let root = tempfile::tempdir().unwrap();
    for directory in ["A", "B"] {
        fs::create_dir(root.path().join(directory)).unwrap();
        for name in ["a.png", "b.png"] {
            fs::write(root.path().join(directory).join(name), name).unwrap();
        }
    }
    let entries = scan::scan(
        &root.path().to_string_lossy(),
        true,
        "png",
        &Progress::default(),
    )
    .unwrap()
    .entries;
    let rules = Rules {
        name: "[B]_[C]".into(),
        ..Rules::default()
    };
    let excluded = HashSet::from([entries[0].path.clone()]);
    let rows = plan::preview_selected(&entries, &rules, &excluded, false);
    assert!(!rows[0].selected);
    assert_eq!("A_001.png", rows[1].name);
    assert_eq!("B_002.png", rows[2].name);
    let rows = plan::preview_selected(&entries, &rules, &excluded, true);
    assert_eq!("B_001.png", rows[2].name);
    assert_eq!("B_002.png", rows[3].name);
    assert!(rows.iter().all(|row| row.error.is_none()));
    let changes: Vec<_> = rows
        .iter()
        .filter(|row| row.selected && row.changed())
        .map(plan::Preview::change)
        .collect();
    let undo = transaction::execute(&changes).unwrap();
    assert!(root.path().join("A/a.png").exists());
    assert!(root.path().join("B/B_001.png").exists());
    transaction::undo(&undo).unwrap();
    assert!(
        entries
            .iter()
            .all(|entry| std::path::Path::new(&entry.path).exists())
    );
}

#[test]
fn list_rename_maps_lines_in_order_and_preserves_extensions() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a.png", "b.jpg"] {
        fs::write(root.path().join(name), "safe").unwrap();
    }
    let entries = scan::scan(
        &root.path().to_string_lossy(),
        false,
        "",
        &Progress::default(),
    )
    .unwrap()
    .entries;
    let rows = plan::preview_list(
        &entries,
        &["first".into(), "second".into()],
        plan::ListApply::Name,
        &HashSet::new(),
    );
    assert_eq!("first.png", rows[0].name);
    assert_eq!("second.jpg", rows[1].name);
    assert!(rows.iter().all(|row| row.error.is_none()));
}

#[test]
fn list_rename_rejects_mismatched_line_counts() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.txt"), "safe").unwrap();
    let entries = scan::scan(
        &root.path().to_string_lossy(),
        false,
        "",
        &Progress::default(),
    )
    .unwrap()
    .entries;
    let rows = plan::preview_list(
        &entries,
        &["one".into(), "extra".into()],
        plan::ListApply::FullName,
        &HashSet::new(),
    );
    assert!(rows[0].error.as_deref().unwrap().contains("数量必须一致"));
}
#[test]
fn deselecting_conflicting_rows_releases_conflicts_but_existing_files_still_block() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a.png", "b.png"] {
        fs::write(root.path().join(name), "safe").unwrap();
    }
    let entries = scan::scan(
        &root.path().to_string_lossy(),
        false,
        "png",
        &Progress::default(),
    )
    .unwrap()
    .entries;
    let rules = Rules {
        name: "same".into(),
        ..Rules::default()
    };
    assert!(
        plan::preview(&entries, &rules)
            .iter()
            .all(|row| row.error.is_some())
    );
    let excluded = HashSet::from([entries[0].path.clone()]);
    assert!(
        plan::preview_selected(&entries, &rules, &excluded, false)
            .iter()
            .all(|row| row.error.is_none())
    );
    let rules = Rules {
        name: "a".into(),
        ..Rules::default()
    };
    assert!(
        plan::preview_selected(&entries, &rules, &excluded, false)[1]
            .error
            .is_some()
    );
}
