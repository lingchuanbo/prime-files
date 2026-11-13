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
