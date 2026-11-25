use super::{Table, compact, invalid, invalid_because};
use crate::mft::{
    self, ATTRIBUTE_LIST, END, Info, RECORD_NUMBER, apply_fixups, record_of, u16_at, u32_at, u64_at,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

type Raw = Option<Vec<u8>>;
const MAX_RECORD_GROWTH: u64 = 1_000_000;

struct Records<F> {
    cache: BTreeMap<u32, Raw>,
    read: F,
}

impl<F: FnMut(u32) -> io::Result<Raw>> Records<F> {
    fn get(&mut self, number: u32) -> io::Result<Raw> {
        if let Some(raw) = self.cache.get(&number) {
            return Ok(raw.clone());
        }
        // A read that fails is the reason a rebuild happened; the log can
        // only act on it if it says which record it was.
        let raw = (self.read)(number).map_err(|error| super::record_error(number, error))?;
        self.cache.insert(number, raw.clone());
        Ok(raw)
    }
}

struct Group {
    owner: u32,
    parsed: Table,
    extensions: Vec<u32>,
}

pub(crate) fn refresh(
    table: &mut Table,
    numbers: BTreeSet<u32>,
    read: impl FnMut(u32) -> io::Result<Raw>,
) -> io::Result<bool> {
    let mut records = Records {
        cache: BTreeMap::new(),
        read,
    };
    let mut owners = BTreeSet::new();
    let mut complex = false;
    for number in numbers {
        if u64::from(number) > table.records.len() as u64 + MAX_RECORD_GROWTH {
            return Err(super::record_error(
                number,
                invalid_because("it is past the table"),
            ));
        }
        if let Some(&owner) = table.extensions.get(&number) {
            owners.insert(owner);
        }
        complex |= table.extended.contains(&number);
        let raw = records.get(number)?;
        let owner = if let Some(raw) = &raw {
            let view = view(raw).map_err(|error| super::record_error(number, error))?;
            let base = u64_at(&view, 0x20).ok_or_else(invalid)? & RECORD_NUMBER;
            let listed = references(&view).map_err(|error| super::record_error(number, error))?;
            complex |= base != 0 || !listed.is_empty();
            if base == 0 {
                number
            } else {
                record_of(base).ok_or_else(|| {
                    super::record_error(
                        number,
                        invalid_because("its base record number does not fit"),
                    )
                })?
            }
        } else {
            number
        };
        owners.insert(owner);
    }
    if !complex {
        let changes = changed_records(table, records.cache)?;
        if changes.is_empty() {
            return Ok(false);
        }
        super::apply(table, changes)?;
        return Ok(true);
    }
    if owners
        .iter()
        .any(|&owner| u64::from(owner) > table.records.len() as u64 + MAX_RECORD_GROWTH)
    {
        return Err(invalid_because("a base record is past the table"));
    }
    // 完整准备所有 owner 后才修改主表，任一扩展记录缺失都保留整批旧数据。
    let groups = owners
        .iter()
        .map(|&owner| load(owner, &mut records).map_err(|error| super::record_error(owner, error)))
        .collect::<io::Result<Vec<_>>>()?;
    commit(table, groups, &owners);
    Ok(true)
}

fn changed_records(table: &Table, records: BTreeMap<u32, Raw>) -> io::Result<BTreeMap<u32, Raw>> {
    let mut old_names: BTreeMap<u32, Vec<(u32, &str)>> = BTreeMap::new();
    for name in &table.names {
        if records.contains_key(&name.record) {
            old_names
                .entry(name.record)
                .or_default()
                .push((name.parent, table.name(name)));
        }
    }
    let mut changed = BTreeMap::new();
    for (number, raw) in records {
        let mut parsed = Table::default();
        if let Some(bytes) = &raw {
            super::validate(bytes).map_err(|error| super::record_error(number, error))?;
            mft::parse_record(&mut parsed, 0, &mut bytes.clone());
        }
        let previous = table
            .records
            .get(number as usize)
            .copied()
            .unwrap_or_default();
        let current = parsed.records.first().copied().unwrap_or_default();
        let mut old = old_names.remove(&number).unwrap_or_default();
        let mut new: Vec<_> = parsed
            .names
            .iter()
            .map(|name| (name.parent, parsed.name(name)))
            .collect();
        old.sort_unstable();
        new.sort_unstable();
        if previous != current || old != new {
            changed.insert(number, raw);
        }
    }
    Ok(changed)
}

fn load(
    owner: u32,
    records: &mut Records<impl FnMut(u32) -> io::Result<Raw>>,
) -> io::Result<Group> {
    let mut group = Group {
        owner,
        parsed: Table::default(),
        extensions: vec![],
    };
    let Some(mut raw) = records.get(owner)? else {
        return Ok(group);
    };
    let base = view(&raw)?;
    if u64_at(&base, 0x20) != Some(0) {
        return Err(invalid_because("its base record is itself an extension"));
    }
    if u16_at(&base, 0x16).is_none_or(|flags| flags & 1 == 0) {
        return Err(invalid_because("its base record is not in use"));
    }
    let references = references(&base)?;
    mft::parse_record(&mut group.parsed, 0, &mut raw);
    let original = group
        .parsed
        .records
        .first()
        .copied()
        .ok_or_else(|| invalid_because("it holds no record of its own"))?;
    for (number, sequence) in references {
        if number == owner {
            check_sequence(&base, sequence)?;
            continue;
        }
        let raw = records
            .get(number)?
            .ok_or_else(|| invalid_because("one of its extension records is gone"))?;
        merge_extension(&mut group, raw, (&base, sequence))?;
        group.extensions.push(number);
    }
    if let Some(info) = group.parsed.records.first_mut() {
        info.in_use = original.in_use;
        info.dir = original.dir;
    }
    Ok(group)
}

fn merge_extension(group: &mut Group, mut raw: Vec<u8>, expected: (&[u8], u16)) -> io::Result<()> {
    let extension = view(&raw)?;
    check_sequence(&extension, expected.1)?;
    let reference = u64_at(&extension, 0x20)
        .ok_or_else(|| invalid_because("an extension record is cut short"))?;
    check_sequence(expected.0, (reference >> 48) as u16)?;
    if reference & RECORD_NUMBER != u64::from(group.owner) {
        return Err(invalid_because("an extension record names another owner"));
    }
    if u16_at(&extension, 0x16).is_none_or(|flags| flags & 1 == 0) {
        return Err(invalid_because("an extension record is not in use"));
    }
    if !references(&extension)?.is_empty() {
        return Err(super::unmergeable(
            "an extension record lists further extents",
        ));
    }
    // 临时表只用槽 0，避免按原始 FRN 分配数百万个空槽。
    raw[0x20..0x28].fill(0);
    mft::parse_record(&mut group.parsed, 0, &mut raw);
    Ok(())
}

fn check_sequence(record: &[u8], expected: u16) -> io::Result<()> {
    if u16_at(record, 0x10) != Some(expected) {
        return Err(invalid_because(
            "a record was reused (its sequence number moved)",
        ));
    }
    Ok(())
}

fn view(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut bytes = raw.to_vec();
    if bytes.get(..4) != Some(b"FILE") || !apply_fixups(&mut bytes) {
        return Err(invalid_because("it is not a whole FILE record"));
    }
    attributes(&bytes)?;
    Ok(bytes)
}

fn attributes(record: &[u8]) -> io::Result<Vec<&[u8]>> {
    let mut at = u16_at(record, 0x14)
        .ok_or_else(|| invalid_because("its attributes do not start in the record"))?
        as usize;
    let used = u32_at(record, 0x18)
        .ok_or_else(|| invalid_because("its attributes do not end in the record"))?
        as usize;
    if at < 0x30 || used > record.len() {
        return Err(invalid_because("its attributes overrun the record"));
    }
    let mut attributes = vec![];
    while at.checked_add(4).is_some_and(|end| end <= used) {
        if u32_at(record, at) == Some(END) {
            return Ok(attributes);
        }
        let length = u32_at(record, at + 4)
            .ok_or_else(|| invalid_because("an attribute length is cut short"))?
            as usize;
        if length < 16
            || !length.is_multiple_of(8)
            || at.checked_add(length).is_none_or(|end| end > used)
        {
            return Err(invalid_because("an attribute overruns the record"));
        }
        attributes.push(&record[at..at + length]);
        at += length;
    }
    Err(invalid_because("the record has no end marker"))
}

fn references(record: &[u8]) -> io::Result<BTreeMap<u32, u16>> {
    let mut references = BTreeMap::new();
    for attribute in attributes(record)? {
        if u32_at(attribute, 0) != Some(ATTRIBUTE_LIST) {
            continue;
        }
        if attribute[8] != 0 {
            return Err(super::unmergeable("its attribute list is not resident"));
        }
        let mut list = mft::resident_value(attribute)
            .ok_or_else(|| invalid_because("its attribute list is cut short"))?;
        while !list.is_empty() {
            let length = u16_at(list, 4)
                .ok_or_else(|| invalid_because("an attribute list entry is cut short"))?
                as usize;
            if length < 26 || length > list.len() {
                return Err(invalid_because("an attribute list entry is out of shape"));
            }
            let reference = u64_at(list, 16)
                .ok_or_else(|| invalid_because("an attribute list entry is cut short"))?;
            let number = record_of(reference).ok_or_else(|| {
                invalid_because("an attribute list names a record past what a table numbers")
            })?;
            let sequence = (reference >> 48) as u16;
            if references
                .insert(number, sequence)
                .is_some_and(|previous| previous != sequence)
            {
                return Err(invalid_because("its attribute list names one record twice"));
            }
            list = &list[length..];
        }
    }
    Ok(references)
}

fn commit(table: &mut Table, groups: Vec<Group>, owners: &BTreeSet<u32>) {
    table.names.retain(|name| !owners.contains(&name.record));
    compact(table);
    let obsolete = table
        .extensions
        .iter()
        .filter_map(|(&number, owner)| owners.contains(owner).then_some(number))
        .collect::<BTreeSet<_>>();
    table.extensions.retain(|_, owner| !owners.contains(owner));
    table
        .extended
        .retain(|number| !owners.contains(number) && !obsolete.contains(number));
    for group in groups {
        if let Some(info) = table.info(group.owner) {
            *info = group
                .parsed
                .records
                .first()
                .copied()
                .unwrap_or(Info::default());
        }
        let offset = table.text.len() as u32;
        std::sync::Arc::make_mut(&mut table.text).push_str(&group.parsed.text);
        for mut name in group.parsed.names {
            name.record = group.owner;
            name.start += offset;
            table.names.push(name);
        }
        for number in group.extensions {
            table.extensions.insert(number, group.owner);
            table.extended.insert(number);
            table.extended.insert(group.owner);
        }
    }
}
