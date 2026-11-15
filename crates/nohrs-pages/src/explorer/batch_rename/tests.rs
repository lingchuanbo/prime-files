use super::{
    plan,
    rules::{CaseMode, Rules},
    transaction,
};
use nohrs_services::fs::listing::FileEntryDto;
use std::path::Path;

fn entry(path: &Path, directory: bool) -> FileEntryDto {
    FileEntryDto {
        path: path.to_string_lossy().into_owned(),
        name: path.file_name().unwrap().to_string_lossy().into_owned(),
        kind: if directory { "dir" } else { "file" }.into(),
        size: 0,
        modified: 0,
    }
}

#[test]
fn batch_rename_expands_each_files_own_ancestors_and_counter() {
    let item = entry(
        Path::new("root")
            .join("祖父")
            .join("父目录")
            .join("照片")
            .join("图像.jpg")
            .as_path(),
        false,
    );
    let rules = Rules {
        name: "[B2]_[B1]_[B]_[N]_[C]".into(),
        start: 7,
        step: 2,
        digits: 3,
        ..Rules::default()
    };
    assert_eq!(
        "祖父_父目录_照片_图像_011.jpg",
        rules.filename(&item, 2).unwrap()
    );
}

#[test]
fn batch_rename_pinyin_translates_stems_and_preserves_other_characters() {
    let rules = Rules {
        name: "[拼音]".into(),
        ..Rules::default()
    };
    for (original, directory, expected) in [
        ("中国.txt", false, "zhongguo.txt"),
        (
            "中国 Report_2026😀.JPG",
            false,
            "zhongguo Report_2026😀.JPG",
        ),
        ("Report_2026.txt", false, "Report_2026.txt"),
        ("中国", false, "zhongguo"),
        ("中国.v2", true, "zhongguo.v2"),
    ] {
        assert_eq!(
            expected,
            rules
                .filename(&entry(Path::new(original), directory), 0)
                .unwrap(),
            "{original}"
        );
    }
}

#[test]
fn batch_rename_pinyin_composes_with_tokens_replacement_and_case() {
    let rules = Rules {
        name: "[拼音]_[C]_[N]".into(),
        find: "guo".into(),
        replacement: "hua".into(),
        case: CaseMode::Upper,
        ..Rules::default()
    };
    assert_eq!(
        "ZHONGHUA_002_中国.TXT",
        rules
            .filename(&entry(Path::new("中国.txt"), false), 1)
            .unwrap()
    );
}

#[test]
fn batch_rename_defaults_preserve_extensions_dotfiles_and_dotted_directories() {
    for (name, directory) in [
        ("a.tar.gz", false),
        (".gitignore", false),
        ("notes", false),
        ("folder.v2", true),
    ] {
        assert_eq!(
            name,
            Rules::default()
                .filename(&entry(Path::new(name), directory), 0)
                .unwrap()
        );
    }
}

#[test]
fn batch_rename_unicode_ranges_replacement_and_case() {
    let rules = Rules {
        name: "[N1-2]_[N3-]".into(),
        find: "照片".into(),
        replacement: "photo".into(),
        case: CaseMode::Upper,
        ..Rules::default()
    };
    assert_eq!(
        "PHOTO_AB.JPG",
        rules
            .filename(&entry(Path::new("照片ab.jpg"), false), 0)
            .unwrap()
    );
}

#[test]
fn batch_rename_unknown_tokens_missing_ancestors_and_overflow_are_errors() {
    let item = entry(Path::new("a.txt"), false);
    for mask in ["[UNKNOWN]", "[B2]", "[N0-2]", "[N9-2]", "[N"] {
        let rules = Rules {
            name: mask.into(),
            ..Rules::default()
        };
        assert!(rules.filename(&item, 0).is_err(), "{mask}");
    }
    let rules = Rules {
        name: "[C]".into(),
        start: u64::MAX,
        step: 1,
        ..Rules::default()
    };
    assert!(rules.filename(&item, 1).is_err());
}

#[test]
fn batch_rename_rejects_invalid_windows_names() {
    for name in [
        "", ".", "..", "CON.txt", "nul", "COM1", "LPT9.jpg", "a/b", "a\\b", "a:b", "a?b", "a.",
        "a ", "a\n",
    ] {
        assert!(plan::validate_name(name).is_err(), "{name:?}");
    }
    assert!(plan::validate_name("正常的名称.webp").is_ok());
}

#[test]
fn batch_rename_preview_rejects_duplicates_and_existing_destinations() {
    let directory = tempfile::tempdir().unwrap();
    let items: Vec<_> = ["a.txt", "b.txt"]
        .into_iter()
        .map(|name| {
            let path = directory.path().join(name);
            std::fs::write(&path, name).unwrap();
            entry(&path, false)
        })
        .collect();
    let rules = Rules {
        name: "same".into(),
        ..Rules::default()
    };
    let rows = plan::preview(&items, &rules);
    assert!(rows.iter().all(|row| row.error.is_some()));
    std::fs::write(directory.path().join("taken.txt"), "external").unwrap();
    let rules = Rules {
        name: "taken".into(),
        ..Rules::default()
    };
    assert!(plan::preview(&items[..1], &rules)[0].error.is_some());
}

#[test]
fn batch_rename_swap_and_undo_preserve_contents() {
    let directory = tempfile::tempdir().unwrap();
    let a = directory.path().join("a.txt");
    let b = directory.path().join("b.txt");
    std::fs::write(&a, "AAA").unwrap();
    std::fs::write(&b, "BBB").unwrap();
    let changes = vec![(a.clone(), b.clone()), (b.clone(), a.clone())];
    let undo = transaction::execute(&changes).unwrap();
    assert_eq!("BBB", std::fs::read_to_string(&a).unwrap());
    assert_eq!("AAA", std::fs::read_to_string(&b).unwrap());
    transaction::undo(&undo).unwrap();
    assert_eq!("AAA", std::fs::read_to_string(&a).unwrap());
    assert_eq!("BBB", std::fs::read_to_string(&b).unwrap());
    assert_eq!(2, std::fs::read_dir(directory.path()).unwrap().count());
}

#[test]
fn batch_rename_case_only_rename_and_directory_undo() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("Folder.v2");
    let after = directory.path().join("folder.V2");
    std::fs::create_dir(&before).unwrap();
    std::fs::write(before.join("child.txt"), "safe").unwrap();
    let undo = transaction::execute(&[(before.clone(), after.clone())]).unwrap();
    assert_eq!(
        "safe",
        std::fs::read_to_string(after.join("child.txt")).unwrap()
    );
    transaction::undo(&undo).unwrap();
    assert_eq!(
        "safe",
        std::fs::read_to_string(before.join("child.txt")).unwrap()
    );
}

#[test]
fn batch_rename_undo_refuses_modified_content_and_occupied_original() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.txt");
    let after = directory.path().join("after.txt");
    std::fs::write(&before, "safe").unwrap();
    let undo = transaction::execute(&[(before.clone(), after.clone())]).unwrap();
    std::fs::write(&before, "external").unwrap();
    assert!(transaction::undo(&undo).is_err());
    assert_eq!("external", std::fs::read_to_string(&before).unwrap());
    std::fs::write(&after, "changed content").unwrap();
    assert!(transaction::undo(&undo).is_err());
}

#[test]
fn batch_rename_execution_rechecks_collisions_without_touching_sources() {
    let directory = tempfile::tempdir().unwrap();
    let before = directory.path().join("before.txt");
    let after = directory.path().join("after.txt");
    std::fs::write(&before, "safe").unwrap();
    std::fs::write(&after, "external").unwrap();
    assert!(transaction::execute(&[(before.clone(), after.clone())]).is_err());
    assert_eq!("safe", std::fs::read_to_string(&before).unwrap());
    assert_eq!("external", std::fs::read_to_string(&after).unwrap());
}

#[test]
fn batch_rename_rejects_parent_and_child_in_one_batch() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("parent");
    std::fs::create_dir(&parent).unwrap();
    let child = parent.join("child");
    std::fs::write(&child, "safe").unwrap();
    assert!(
        transaction::execute(&[
            (parent.clone(), directory.path().join("new")),
            (child, parent.join("new-child"))
        ])
        .is_err()
    );
    assert_eq!(
        "safe",
        std::fs::read_to_string(parent.join("child")).unwrap()
    );
}
