use super::*;
use crate::{
    mft::{
        self,
        tests::{Record, table_of},
    },
    protocol,
};

fn fixture() -> Table {
    table_of(vec![
        (5, Record::new(3, 0).finish()),
        (30, Record::new(3, 0).name(5, "old", 1).finish()),
        (
            31,
            Record::new(1, 0)
                .name(30, "child.txt", 1)
                .resident_data(4)
                .finish(),
        ),
        (32, Record::new(3, 0).name(5, "target", 1).finish()),
    ])
}

fn hits(table: &Table) -> Vec<protocol::Hit> {
    let policy = mft::Policy {
        profiles: vec![],
        allowed: vec![],
    };
    let mut hits = vec![];
    mft::tree(table, "C:", &policy).search(&["txt".into()], 100, &mut hits);
    hits
}

#[test]
fn directory_moves_update_descendant_paths() {
    let mut table = fixture();
    let moved = Record::new(3, 0).name(32, "renamed", 1).finish();
    apply(&mut table, BTreeMap::from([(30, Some(moved))])).unwrap();
    assert_eq!(r"C:\target\renamed\child.txt", hits(&table)[0].path);
}

#[test]
fn deletes_and_record_reuse_replace_all_old_names() {
    let mut table = fixture();
    apply(&mut table, BTreeMap::from([(31, None)])).unwrap();
    assert!(hits(&table).is_empty());
    let replacement = Record::new(1, 0)
        .name(32, "new.txt", 1)
        .resident_data(9)
        .finish();
    apply(&mut table, BTreeMap::from([(31, Some(replacement))])).unwrap();
    assert_eq!(r"C:\target\new.txt", hits(&table)[0].path);
    assert_eq!(9, hits(&table)[0].size);
}

#[test]
fn refresh_keeps_current_hard_links_without_duplicates() {
    let mut table = fixture();
    for _ in 0..3 {
        let linked = Record::new(1, 0)
            .name(30, "a.txt", 1)
            .name(32, "b.txt", 1)
            .finish();
        apply(&mut table, BTreeMap::from([(31, Some(linked))])).unwrap();
    }
    let found = hits(&table);
    assert_eq!(2, found.len());
    assert!(found.iter().any(|hit| hit.path == r"C:\old\a.txt"));
    assert!(found.iter().any(|hit| hit.path == r"C:\target\b.txt"));
}

#[test]
fn malformed_record_does_not_partially_apply_batch() {
    let mut table = fixture();
    let before = hits(&table)[0].path.clone();
    assert!(
        apply(
            &mut table,
            BTreeMap::from([(30, None), (31, Some(vec![0; 1024]))])
        )
        .is_err()
    );
    assert_eq!(before, hits(&table)[0].path);
}

#[test]
fn unresolved_extension_changes_preserve_old_links() {
    let mut table = fixture();
    let mut extension = Record::new(1, 31).name(32, "linked.txt", 1).finish();
    mft::parse_record(&mut table, 40, &mut extension);
    for number in [31, 40] {
        assert!(apply(&mut table, BTreeMap::from([(number, None)])).is_err());
        assert_eq!(2, hits(&table).len());
    }
    let extension = Record::new(1, 31).name(32, "new.txt", 1).finish();
    assert!(apply(&mut table, BTreeMap::from([(41, Some(extension))])).is_err());
    assert_eq!(2, hits(&table).len());
}

#[test]
fn resolved_extensions_update_sizes_links_and_extension_deletion() {
    let mut table = fixture();
    let base = Record::new(1, 0)
        .name(30, "child.txt", 1)
        .attribute_list(&[31, 40])
        .finish();
    let extension = Record::new(1, 31)
        .name(32, "linked.txt", 1)
        .big_data(8192, &[0])
        .finish();
    let mut current = BTreeMap::from([(31, Some(base)), (40, Some(extension))]);
    refresh(&mut table, [31].into(), |number| {
        Ok(current.get(&number).cloned().flatten())
    })
    .unwrap();
    assert_eq!(2, hits(&table).len());
    assert!(hits(&table).iter().all(|hit| hit.size == 8192));
    current.insert(
        40,
        Some(
            Record::new(1, 31)
                .name(32, "renamed.txt", 1)
                .big_data(9000, &[0])
                .finish(),
        ),
    );
    refresh(&mut table, [40].into(), |number| {
        Ok(current.get(&number).cloned().flatten())
    })
    .unwrap();
    assert_eq!(2, hits(&table).len());
    assert!(
        hits(&table)
            .iter()
            .any(|hit| hit.path.ends_with("renamed.txt"))
    );
    assert!(hits(&table).iter().all(|hit| hit.size == 9000));
    current.insert(
        31,
        Some(
            Record::new(1, 0)
                .name(30, "child.txt", 1)
                .resident_data(7)
                .finish(),
        ),
    );
    current.insert(40, None);
    refresh(&mut table, [40].into(), |number| {
        Ok(current.get(&number).cloned().flatten())
    })
    .unwrap();
    assert_eq!(1, hits(&table).len());
    assert_eq!(7, hits(&table)[0].size);
    current.insert(31, None);
    refresh(&mut table, [31].into(), |number| {
        Ok(current.get(&number).cloned().flatten())
    })
    .unwrap();
    assert!(hits(&table).is_empty());
}

#[test]
fn missing_extension_keeps_entire_previous_batch() {
    let mut table = fixture();
    let listed = Record::new(1, 0)
        .name(30, "changed.txt", 1)
        .attribute_list(&[31, 40])
        .finish();
    let current = BTreeMap::from([(30, None), (31, Some(listed))]);
    assert!(
        refresh(&mut table, [30, 31].into(), |number| Ok(current
            .get(&number)
            .cloned()
            .flatten()))
        .is_err()
    );
    assert_eq!(r"C:\old\child.txt", hits(&table)[0].path);
}

#[test]
fn reused_owner_sequence_keeps_entire_previous_batch() {
    let mut table = fixture();
    let base = Record::new(1, 0).attribute_list(&[31, 40]).finish();
    let extension = Record::new(1, 31 | (1 << 48))
        .name(32, "wrong.txt", 1)
        .finish();
    let current = BTreeMap::from([(30, None), (31, Some(base)), (40, Some(extension))]);
    assert!(
        refresh(&mut table, [30, 40].into(), |number| {
            Ok(current.get(&number).cloned().flatten())
        })
        .is_err()
    );
    assert_eq!(r"C:\old\child.txt", hits(&table)[0].path);
}

#[test]
fn moving_into_another_profile_hides_descendants() {
    let mut table = fixture();
    let other = Record::new(3, 0).name(32, "other", 1).finish();
    let moved = Record::new(3, 0).name(33, "moved", 1).finish();
    apply(
        &mut table,
        BTreeMap::from([(30, Some(moved)), (33, Some(other))]),
    )
    .unwrap();
    let policy = mft::Policy {
        profiles: vec!["target".into()],
        allowed: vec!["own".into()],
    };
    let mut found = vec![];
    mft::tree(&table, "C:", &policy).search(&["txt".into()], 100, &mut found);
    assert!(found.is_empty());
}

#[test]
fn refresh_skips_identical_search_data_but_detects_metadata_and_deletion() {
    let mut table = fixture();
    let same = Record::new(1, 0)
        .name(30, "child.txt", 1)
        .resident_data(4)
        .finish();
    assert!(!refresh(&mut table, [31].into(), |_| Ok(Some(same.clone()))).unwrap());
    let changed = Record::new(1, 0)
        .name(30, "child.txt", 1)
        .resident_data(9)
        .finish();
    assert!(refresh(&mut table, [31].into(), |_| Ok(Some(changed.clone()))).unwrap());
    assert_eq!(9, hits(&table)[0].size);
    assert!(refresh(&mut table, [31].into(), |_| Ok(None)).unwrap());
    assert!(hits(&table).is_empty());
    assert!(!refresh(&mut table, [31].into(), |_| Ok(None)).unwrap());
}

#[test]
fn refresh_applies_changed_entries_in_a_mixed_batch() {
    let mut table = fixture();
    let same = Record::new(3, 0).name(5, "old", 1).finish();
    let renamed = Record::new(1, 0)
        .name(30, "renamed.txt", 1)
        .resident_data(4)
        .finish();
    let records = BTreeMap::from([(30, same), (31, renamed)]);
    assert!(
        refresh(&mut table, [30, 31].into(), |number| Ok(records
            .get(&number)
            .cloned()))
        .unwrap()
    );
    assert_eq!(r"C:\old\renamed.txt", hits(&table)[0].path);
}
