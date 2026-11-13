#![allow(clippy::unwrap_used)]
use super::*;
use std::{
    fs,
    os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _},
    path::Path,
};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
    },
};

fn number(path: &Path) -> u64 {
    let file = fs::OpenOptions::new()
        .read(true)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
        .unwrap();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }.unwrap();
    (u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow)) & RECORD_NUMBER
}

fn advance(journal: &Journal, cursor: &mut Cursor, expected: u64) {
    for _ in 0..50 {
        let (next, records) = journal.changes(*cursor).unwrap();
        *cursor = next;
        if records.contains(&expected) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("journal did not report record {expected}");
}

fn names(journal: &Journal, id: u64) -> Vec<String> {
    let Some(mut bytes) = journal.record(id).unwrap() else {
        return vec![];
    };
    let mut table = crate::mft::Table::default();
    crate::mft::parse_record(&mut table, id, &mut bytes);
    table
        .names
        .iter()
        .map(|name| table.name(name).to_string())
        .collect()
}

struct Cleanup(std::path::PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("test cleanup: {error}");
        }
    }
}

#[test]
#[ignore = "requires an elevated token and an existing NTFS journal"]
fn real_journal_tracks_create_rename_hard_link_move_and_delete() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .canonicalize()
        .unwrap();
    let root = workspace.to_string_lossy().trim_start_matches(r"\\?\")[..2].to_string();
    let journal = Journal::open(&root).unwrap();
    let state = journal.state().unwrap();
    let mut cursor = Cursor {
        id: state.id,
        next: state.next,
    };
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let folder = workspace.join(format!("journal-test-{}-{suffix}", std::process::id()));
    fs::create_dir(&folder).unwrap();
    let _cleanup = Cleanup(folder.clone());
    let folder_id = number(&folder);
    let original = folder.join("original.txt");
    fs::write(&original, b"search update").unwrap();
    let file_id = number(&original);
    advance(&journal, &mut cursor, file_id);
    assert_eq!(vec!["original.txt"], names(&journal, file_id));
    let renamed = folder.join("renamed.txt");
    fs::rename(&original, &renamed).unwrap();
    advance(&journal, &mut cursor, file_id);
    assert_eq!(vec!["renamed.txt"], names(&journal, file_id));
    let link = folder.join("linked.txt");
    fs::hard_link(&renamed, &link).unwrap();
    advance(&journal, &mut cursor, file_id);
    let mut found = names(&journal, file_id);
    found.sort();
    assert_eq!(vec!["linked.txt", "renamed.txt"], found);
    let moved = folder.join("destination");
    fs::create_dir(&moved).unwrap();
    fs::rename(&renamed, moved.join("renamed.txt")).unwrap();
    advance(&journal, &mut cursor, file_id);
    assert_eq!(2, names(&journal, file_id).len());
    fs::remove_file(&link).unwrap();
    fs::remove_file(moved.join("renamed.txt")).unwrap();
    advance(&journal, &mut cursor, file_id);
    assert!(names(&journal, file_id).is_empty());
    assert!(
        names(&journal, folder_id)
            .iter()
            .any(|name| name.starts_with("journal-test-"))
    );
}
