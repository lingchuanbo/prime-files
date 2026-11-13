use std::{collections::BTreeSet, io};

mod volume;
pub(super) use volume::Journal;
const RECORD_NUMBER: u64 = 0x0000_FFFF_FFFF_FFFF;

#[derive(Clone, Copy)]
pub(super) struct Cursor {
    pub id: u64,
    pub next: i64,
}

pub(super) struct State {
    pub id: u64,
    pub first: i64,
    pub next: i64,
}

impl State {
    pub fn accepts(&self, cursor: Cursor) -> bool {
        self.id == cursor.id && (self.first..=self.next).contains(&cursor.next)
    }
}

pub(super) fn parse(bytes: &[u8], start: i64) -> io::Result<(i64, BTreeSet<u64>)> {
    let next = i64::from_le_bytes(field(bytes, 0)?);
    if next < start {
        return Err(invalid());
    }
    let mut records = BTreeSet::new();
    let mut at = 8;
    let mut previous = start;
    while at < bytes.len() {
        let length = u32::from_le_bytes(field(bytes, at)?) as usize;
        if length < 8 || !length.is_multiple_of(8) {
            return Err(invalid());
        }
        let record = bytes
            .get(at..at.checked_add(length).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        let (number, usn) = event(record)?;
        if usn < previous || usn >= next {
            return Err(invalid());
        }
        previous = usn;
        records.insert(number);
        at += length;
    }
    Ok((next, records))
}

fn event(record: &[u8]) -> io::Result<(u64, i64)> {
    let major = u16::from_le_bytes(field(record, 4)?);
    let (usn_at, name_at) = match major {
        2 => (24, 56),
        3 if u64::from_le_bytes(field(record, 16)?) == 0
            && u64::from_le_bytes(field(record, 32)?) == 0 =>
        {
            (40, 72)
        }
        _ => return Err(invalid()),
    };
    let length = u16::from_le_bytes(field(record, name_at)?) as usize;
    let offset = u16::from_le_bytes(field(record, name_at + 2)?) as usize;
    if offset < name_at + 4
        || !offset.is_multiple_of(2)
        || !length.is_multiple_of(2)
        || record.get(offset..offset + length).is_none()
    {
        return Err(invalid());
    }
    let number = u64::from_le_bytes(field(record, 8)?) & RECORD_NUMBER;
    Ok((number, i64::from_le_bytes(field(record, usn_at)?)))
}

fn field<const N: usize>(bytes: &[u8], at: usize) -> io::Result<[u8; N]> {
    bytes
        .get(at..at.checked_add(N).ok_or_else(invalid)?)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(invalid)
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or expired NTFS change journal",
    )
}

fn prepare_record(record: &mut [u8]) -> io::Result<()> {
    let offset = u16::from_le_bytes(field(record, 4)?) as usize;
    let count = u16::from_le_bytes(field(record, 6)?) as usize;
    if count < 2 || !record.len().is_multiple_of(count - 1) {
        return Err(invalid());
    }
    let stride = record.len() / (count - 1);
    if stride < 512 || offset < 8 || offset + count * 2 > stride - 2 {
        return Err(invalid());
    }
    let check: [u8; 2] = field(record, offset)?;
    // FSCTL 返回的是内核已修复的记录；重建保护标记后复用严格的磁盘解析器。
    for sector in 1..count {
        let end = sector * stride;
        let real: [u8; 2] = field(record, end - 2)?;
        record[offset + sector * 2..offset + sector * 2 + 2].copy_from_slice(&real);
        record[end - 2..end].copy_from_slice(&check);
    }
    Ok(())
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "journal_live_tests.rs"]
mod live_tests;
