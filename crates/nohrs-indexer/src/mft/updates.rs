use super::{
    ATTRIBUTE_LIST, END, Info, RECORD_NUMBER, Table, apply_fixups, parse_record, u16_at, u32_at,
    u64_at,
};
use std::collections::BTreeMap;
use std::io;

mod resolve;
pub(crate) use resolve::refresh;

pub(crate) fn apply(table: &mut Table, changes: BTreeMap<u32, Option<Vec<u8>>>) -> io::Result<()> {
    for (&number, record) in &changes {
        if u64::from(number) > table.records.len() as u64 + 1_000_000 {
            return Err(record_error(
                number,
                invalid_because("it is past the table"),
            ));
        }
        if table.extended.contains(&number) {
            return Err(record_error(
                number,
                invalid_because("its attributes live in extension records"),
            ));
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

fn record_error(number: u32, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("MFT record {number}: {error}"))
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "MFT update needs a full rebuild",
    )
}

/// The same refusal, saying which check made it: a rebuild costs a whole
/// MFT read, and the log is the only place that can say why one happened.
fn invalid_because(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("MFT update needs a full rebuild: {what}"),
    )
}

/// A batch the journal alone cannot answer: a record keeps part of its
/// attributes outside the record, in a buffer only a read of the volume
/// gathers. A rebuild is the only repair, and the same record will refuse
/// every batch it appears in, so the caller is told to rebuild on a slower
/// schedule rather than at once.
fn unmergeable(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("MFT update needs a full rebuild: {what}"),
    )
}

fn validate(raw: &[u8]) -> io::Result<()> {
    let mut record = raw.to_vec();
    if record.get(..4) != Some(b"FILE") || !apply_fixups(&mut record) {
        return Err(invalid_because("it is not a whole FILE record"));
    }
    if u64_at(&record, 0x20).is_none_or(|base| base & RECORD_NUMBER != 0) {
        return Err(invalid_because("it is an extension record"));
    }
    let mut at = u16_at(&record, 0x14)
        .ok_or_else(|| invalid_because("its attributes do not start in the record"))?
        as usize;
    let used = u32_at(&record, 0x18)
        .ok_or_else(|| invalid_because("its attributes do not end in the record"))?
        as usize;
    while at
        .checked_add(4)
        .is_some_and(|end| end <= used && end <= record.len())
    {
        let kind = u32_at(&record, at)
            .ok_or_else(|| invalid_because("an attribute header is cut short"))?;
        if kind == END {
            return Ok(());
        }
        let length = u32_at(&record, at + 4)
            .ok_or_else(|| invalid_because("an attribute length is cut short"))?
            as usize;
        if kind == ATTRIBUTE_LIST {
            return Err(invalid_because("its attributes are listed elsewhere"));
        }
        if length < 16
            || at
                .checked_add(length)
                .is_none_or(|end| end > used || end > record.len())
        {
            return Err(invalid_because("an attribute overruns the record"));
        }
        at += length;
    }
    Err(invalid_because("the record has no end marker"))
}

fn compact(table: &mut Table) {
    let mut text = String::with_capacity(table.text.len());
    for name in &mut table.names {
        let start = text.len() as u32;
        text.push_str(
            &table.text.as_str()[name.start as usize..name.start as usize + name.len as usize],
        );
        name.start = start;
    }
    // A fresh buffer, not an edit of the old one: the snapshot published so
    // far holds that older buffer, and starting a new one is what lets the
    // names this batch adds be written without copying the whole table.
    table.text = std::sync::Arc::new(text);
}

#[cfg(test)]
#[path = "updates_tests.rs"]
mod tests;
