use super::{
    ATTRIBUTE_LIST, END, Info, RECORD_NUMBER, Table, apply_fixups, parse_record, u16_at, u32_at,
    u64_at,
};
use std::collections::BTreeMap;
use std::io;

mod resolve;
pub(crate) use resolve::refresh;

pub(crate) fn apply(table: &mut Table, changes: BTreeMap<u64, Option<Vec<u8>>>) -> io::Result<()> {
    for (&number, record) in &changes {
        if number > table.records.len() as u64 + 1_000_000 || table.extended.contains(&number) {
            return Err(record_error(number, invalid()));
        }
        if let Some(record) = record {
            validate(record).map_err(|error| record_error(number, error))?;
        }
    }
    // 先验证整批；扩展属性无法单独更新时，调用方保留旧快照并重建。
    table
        .names
        .retain(|name| !changes.contains_key(&name.record));
    compact(table);
    for (number, record) in changes {
        if let Some(info) = table.info(number) {
            *info = Info::default();
        }
        if let Some(mut record) = record {
            parse_record(table, number, &mut record);
        }
    }
    Ok(())
}

fn record_error(number: u64, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("MFT record {number}: {error}"))
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "MFT update needs a full rebuild",
    )
}

fn validate(raw: &[u8]) -> io::Result<()> {
    let mut record = raw.to_vec();
    if record.get(..4) != Some(b"FILE") || !apply_fixups(&mut record) {
        return Err(invalid());
    }
    if u64_at(&record, 0x20).is_none_or(|base| base & RECORD_NUMBER != 0) {
        return Err(invalid());
    }
    let mut at = u16_at(&record, 0x14).ok_or_else(invalid)? as usize;
    let used = u32_at(&record, 0x18).ok_or_else(invalid)? as usize;
    while at
        .checked_add(4)
        .is_some_and(|end| end <= used && end <= record.len())
    {
        let kind = u32_at(&record, at).ok_or_else(invalid)?;
        if kind == END {
            return Ok(());
        }
        let length = u32_at(&record, at + 4).ok_or_else(invalid)? as usize;
        if kind == ATTRIBUTE_LIST
            || length < 16
            || at
                .checked_add(length)
                .is_none_or(|end| end > used || end > record.len())
        {
            return Err(invalid());
        }
        at += length;
    }
    Err(invalid())
}

fn compact(table: &mut Table) {
    let mut text = String::with_capacity(table.text.len());
    for name in &mut table.names {
        let start = text.len() as u32;
        text.push_str(&table.text[name.start as usize..name.start as usize + name.len as usize]);
        name.start = start;
    }
    table.text = text;
}

#[cfg(test)]
#[path = "updates_tests.rs"]
mod tests;
