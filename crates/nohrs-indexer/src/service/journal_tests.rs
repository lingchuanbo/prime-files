use super::*;

fn event(major: u16, number: u64, usn: i64) -> Vec<u8> {
    let mut bytes = vec![0; 80];
    bytes[..4].copy_from_slice(&80u32.to_le_bytes());
    bytes[4..6].copy_from_slice(&major.to_le_bytes());
    bytes[8..16].copy_from_slice(&(number | (3 << 48)).to_le_bytes());
    let (at, length) = if major == 2 { (24, 56) } else { (40, 72) };
    bytes[at..at + 8].copy_from_slice(&usn.to_le_bytes());
    bytes[length..length + 2].copy_from_slice(&2u16.to_le_bytes());
    bytes[length + 2..length + 4].copy_from_slice(&76u16.to_le_bytes());
    bytes[76..78].copy_from_slice(&('a' as u16).to_le_bytes());
    bytes
}

fn response(events: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = 200i64.to_le_bytes().to_vec();
    for event in events {
        bytes.extend_from_slice(event);
    }
    bytes
}

#[test]
fn batches_deduplicate_records_and_accept_v2_and_ntfs_v3() {
    let bytes = response(&[event(2, 30, 100), event(3, 31, 110), event(2, 30, 120)]);
    let (next, records) = parse(&bytes, 100).unwrap();
    assert_eq!(200, next);
    assert_eq!(BTreeSet::from([30, 31]), records);
}

#[test]
fn malformed_and_unknown_records_require_rebuild() {
    let good = response(&[event(2, 30, 100)]);
    for length in 0..good.len() {
        if length == 8 {
            continue;
        }
        assert!(parse(&good[..length], 100).is_err(), "length {length}");
    }
    assert!(parse(&response(&[event(4, 30, 100)]), 100).is_err());
    let mut high_id = event(3, 30, 100);
    high_id[16] = 1;
    assert!(parse(&response(&[high_id]), 100).is_err());
}

#[test]
fn invalid_name_bounds_and_cursor_regressions_are_rejected() {
    let mut bad = event(2, 30, 100);
    bad[58..60].copy_from_slice(&79u16.to_le_bytes());
    assert!(parse(&response(&[bad]), 100).is_err());
    assert!(parse(&response(&[event(2, 30, 99)]), 100).is_err());
    assert!(parse(&response(&[event(2, 30, 201)]), 100).is_err());
    assert!(parse(&50i64.to_le_bytes(), 100).is_err());
}

#[test]
fn rollover_recreation_and_rewind_invalidate_cursor() {
    let state = State {
        id: 10,
        first: 100,
        next: 200,
    };
    assert!(state.accepts(Cursor { id: 10, next: 100 }));
    assert!(state.accepts(Cursor { id: 10, next: 200 }));
    assert!(!state.accepts(Cursor { id: 11, next: 150 }));
    assert!(!state.accepts(Cursor { id: 10, next: 99 }));
    assert!(!state.accepts(Cursor { id: 10, next: 201 }));
}

#[test]
fn kernel_records_are_accepted_without_weakening_raw_disk_fixup_checks() {
    let mut record = crate::mft::tests::Record::new(1, 0)
        .name(5, "kernel.txt", 1)
        .finish();
    assert!(crate::mft::apply_fixups(&mut record));
    prepare_record(&mut record).unwrap();
    let mut table = crate::mft::Table::default();
    crate::mft::parse_record(&mut table, 30, &mut record);
    assert_eq!(1, table.names.len());
    assert_eq!("kernel.txt", table.name(&table.names[0]));
}
