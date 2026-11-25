//! A volume's master file table, read into a tree of names.
//!
//! Every file and folder on an NTFS volume is a record of the MFT (1 KB,
//! sometimes 4 KB), numbered from 0. A record holds attributes: the last
//! write in `$STANDARD_INFORMATION`, each name with the record number of its
//! folder in a `$FILE_NAME`, the size in the unnamed `$DATA`. Reading the
//! table front to back is a sequential read of a few GB at most, where a
//! walk opens every folder through the file system's filters one by one:
//! seconds instead of the minutes this machine's C: took cold.
//!
//! What is read here is the bytes on the volume, so nothing is trusted:
//! every offset is checked before it is used, and a record that does not
//! add up is skipped, not read wrong.

/// The root folder's record.
pub const ROOT: u32 = 5;
// Records below this are the file system's own ($MFT, $LogFile, $Extend…).
const FIRST_USER_RECORD: u32 = 24;
const RECORD_NUMBER: u64 = 0x0000_FFFF_FFFF_FFFF;
// 100 ns ticks from 1601 to 1970.
const EPOCH_TICKS: u64 = 116_444_736_000_000_000;

/// The record number in a file reference. NTFS holds at most 4 294 967 295
/// records on a volume, so the number always fits 32 bits and the rest of the
/// reference is the sequence number; `None` is a corrupt reference.
pub(crate) fn record_of(reference: u64) -> Option<u32> {
    u32::try_from(reference & RECORD_NUMBER).ok()
}

const STANDARD_INFORMATION: u32 = 0x10;
const ATTRIBUTE_LIST: u32 = 0x20;
const FILE_NAME: u32 = 0x30;
const DATA: u32 = 0x80;
const END: u32 = 0xFFFF_FFFF;
const IN_USE: u16 = 1;
const DIRECTORY: u16 = 2;
// The 8.3 alias of a name that has a long one: the same entry again.
const DOS_NAMESPACE: u8 = 2;

pub(crate) mod updates;

/// What one record says of itself.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Info {
    /// The record holds a live file or folder.
    pub in_use: bool,
    /// It is a folder.
    pub dir: bool,
    /// Bytes of its unnamed data stream.
    pub size: u64,
    /// Last write, whole seconds since 1970; 0 when unknown.
    pub modified: u32,
}

/// One name of a record: a file with hard links has several.
///
/// The numbers are the low 32 bits of the file references NTFS stores, which
/// is all a volume's record numbers ever are. 16 bytes instead of 24 is a
/// hundred megabytes on a drive with ten million names.
#[derive(Clone, Copy)]
pub struct Name {
    /// The record named.
    pub record: u32,
    /// The record of the folder it is in.
    pub parent: u32,
    start: u32,
    len: u16,
}

/// Every record of a volume, by number, and every name.
#[derive(Default)]
pub struct Table {
    /// Indexed by record number.
    pub records: Vec<Info>,
    /// In the order read.
    pub names: Vec<Name>,
    /// Every name's text, in the order read. A snapshot built off this table
    /// shares this buffer rather than copying the names out of it.
    text: std::sync::Arc<String>,
    extended: std::collections::HashSet<u32>,
    extensions: std::collections::HashMap<u32, u32>,
}

impl Table {
    fn info(&mut self, record: u32) -> Option<&mut Info> {
        let record = record as usize;
        // A record number far past the table read is a corrupt reference.
        if record >= self.records.len() {
            if record > self.records.len() + 1_000_000 {
                return None;
            }
            self.records.resize(record + 1, Info::default());
        }
        self.records.get_mut(record)
    }

    /// The text of `name`.
    pub fn name(&self, name: &Name) -> &str {
        self.text
            .get(name.start as usize..name.start as usize + name.len as usize)
            .unwrap_or("")
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn seconds(ticks: u64) -> u32 {
    if ticks <= EPOCH_TICKS {
        return 0;
    }
    ((ticks - EPOCH_TICKS) / 10_000_000).min(u64::from(u32::MAX)) as u32
}

/// Undoes a record's update sequence: NTFS writes a check value over the
/// last two bytes of each sector and keeps the real bytes in an array at
/// the record's head, so a torn write shows. False when the record is torn
/// or malformed.
pub fn apply_fixups(record: &mut [u8]) -> bool {
    let (Some(offset), Some(count)) = (u16_at(record, 4), u16_at(record, 6)) else {
        return false;
    };
    let (offset, count) = (offset as usize, count as usize);
    if count < 2 {
        return false;
    }
    let stride = record.len() / (count - 1);
    let Some(check) = u16_at(record, offset) else {
        return false;
    };
    for sector in 1..count {
        let end = sector * stride;
        let (Some(found), Some(real)) = (
            u16_at(record, end.wrapping_sub(2)),
            u16_at(record, offset + sector * 2),
        ) else {
            return false;
        };
        if end < 2 || found != check {
            return false;
        }
        record[end - 2..end].copy_from_slice(&real.to_le_bytes());
    }
    true
}

// The value of a resident attribute.
fn resident_value(attribute: &[u8]) -> Option<&[u8]> {
    let length = u32_at(attribute, 0x10)? as usize;
    let offset = u16_at(attribute, 0x14)? as usize;
    attribute.get(offset..offset.checked_add(length)?)
}

/// Reads record `number` into `table`. `record` is the raw record, fixed up
/// here; one that is not a live file record is left out.
pub fn parse_record(table: &mut Table, number: u32, record: &mut [u8]) {
    if record.get(..4) != Some(b"FILE") || !apply_fixups(record) {
        return;
    }
    let Some(flags) = u16_at(record, 0x16) else {
        return;
    };
    if flags & IN_USE == 0 {
        return;
    }
    let base = u64_at(record, 0x20).unwrap_or(0) & RECORD_NUMBER;
    // An extension record carries attributes of its base record that did
    // not fit there.
    let owner = if base == 0 {
        number
    } else {
        let Some(owner) = record_of(base) else {
            return;
        };
        owner
    };
    if base != 0 {
        table.extended.insert(owner);
        table.extended.insert(number);
        table.extensions.insert(number, owner);
    }
    if base == 0
        && let Some(info) = table.info(owner)
    {
        info.in_use = true;
        info.dir = flags & DIRECTORY != 0;
    }
    let mut at = u16_at(record, 0x14).map_or(usize::MAX, usize::from);
    let used = u32_at(record, 0x18).map_or(0, |used| (used as usize).min(record.len()));
    while at.checked_add(16).is_some_and(|end| end <= used) {
        let (Some(kind), Some(length)) = (u32_at(record, at), u32_at(record, at + 4)) else {
            break;
        };
        let length = length as usize;
        if kind == END || length < 16 || at + length > used {
            break;
        }
        let attribute = &record[at..at + length];
        let resident = attribute[8] == 0;
        let named = attribute[9] != 0;
        match kind {
            ATTRIBUTE_LIST => {
                table.extended.insert(owner);
            }
            STANDARD_INFORMATION if resident => {
                if let Some(modified) = resident_value(attribute).and_then(|value| u64_at(value, 8))
                    && let Some(info) = table.info(owner)
                {
                    info.modified = seconds(modified);
                }
            }
            FILE_NAME if resident => {
                if let Some(value) = resident_value(attribute) {
                    add_name(table, owner, value);
                }
            }
            DATA if !named => {
                // A non-resident stream in several extents has its sizes in
                // the first one only.
                let size = if resident {
                    u32_at(attribute, 0x10).map(u64::from)
                } else if u64_at(attribute, 0x10) == Some(0) {
                    u64_at(attribute, 0x30)
                } else {
                    None
                };
                if let Some(size) = size
                    && let Some(info) = table.info(owner)
                {
                    info.size = size;
                }
            }
            _ => {}
        }
        at += length;
    }
}

fn add_name(table: &mut Table, record: u32, value: &[u8]) {
    let (Some(parent), Some(&length), Some(&namespace)) =
        (u64_at(value, 0), value.get(0x40), value.get(0x41))
    else {
        return;
    };
    if namespace == DOS_NAMESPACE {
        return;
    }
    let Some(parent) = record_of(parent) else {
        return;
    };
    let Some(units) = value.get(0x42..0x42 + length as usize * 2) else {
        return;
    };
    // NTFS names are at most 255 UTF-16 units, and a longer one is not a
    // name. The buffer keeps the conversion off the heap, once per name read.
    let mut buffer = [0u16; 256];
    let Some(slots) = buffer.get_mut(..units.len() / 2) else {
        return;
    };
    for (slot, pair) in slots.iter_mut().zip(units.as_chunks::<2>().0) {
        *slot = u16::from_le_bytes(*pair);
    }
    let name = String::from_utf16_lossy(slots);
    let start = table.text.len() as u32;
    std::sync::Arc::make_mut(&mut table.text).push_str(&name);
    table.names.push(Name {
        record,
        parent,
        start,
        len: name.len() as u16,
    });
}

/// The extents of a non-resident attribute, as (first cluster, clusters);
/// a sparse run has no first cluster.
pub fn data_runs(attribute: &[u8]) -> Option<Vec<(Option<u64>, u64)>> {
    let mut at = u16_at(attribute, 0x20)? as usize;
    let mut runs = Vec::new();
    let mut cluster: i64 = 0;
    loop {
        let header = *attribute.get(at)?;
        if header == 0 {
            return Some(runs);
        }
        let (length_size, offset_size) = ((header & 0x0F) as usize, (header >> 4) as usize);
        if length_size == 0 || length_size > 8 || offset_size > 8 {
            return None;
        }
        let length_bytes = attribute.get(at + 1..at + 1 + length_size)?;
        let mut length = [0u8; 8];
        length[..length_size].copy_from_slice(length_bytes);
        let length = u64::from_le_bytes(length);
        let start = if offset_size == 0 {
            None
        } else {
            let offset_bytes =
                attribute.get(at + 1 + length_size..at + 1 + length_size + offset_size)?;
            // Signed, relative to the previous run's start.
            let negative = offset_bytes.last().is_some_and(|byte| byte & 0x80 != 0);
            let mut offset = if negative { [0xFFu8; 8] } else { [0u8; 8] };
            offset[..offset_size].copy_from_slice(offset_bytes);
            cluster = cluster.checked_add(i64::from_le_bytes(offset))?;
            Some(u64::try_from(cluster).ok()?)
        };
        runs.push((start, length));
        at += 1 + length_size + offset_size;
    }
}

/// One extent: its first cluster when it starts one, and how many clusters.
type Extent = (Option<u64>, u64);

/// The `$MFT`'s own extents and whether the record sends part of its attributes
/// elsewhere (then the runs may be partial).
type MftRuns = (Vec<Extent>, bool);

/// The `$MFT`'s own extents, read from its record 0.
pub fn mft_runs(record: &mut [u8]) -> Option<MftRuns> {
    if record.get(..4) != Some(b"FILE") || !apply_fixups(record) {
        return None;
    }
    let mut at = u16_at(record, 0x14)? as usize;
    let used = (u32_at(record, 0x18)? as usize).min(record.len());
    let mut listed = false;
    let mut runs = None;
    while at + 16 <= used {
        let kind = u32_at(record, at)?;
        let length = u32_at(record, at + 4)? as usize;
        if kind == END || length < 16 || at + length > used {
            break;
        }
        let attribute = &record[at..at + length];
        if kind == ATTRIBUTE_LIST {
            listed = true;
        }
        if kind == DATA
            && attribute[8] != 0
            && attribute[9] == 0
            && u64_at(attribute, 0x10) == Some(0)
        {
            runs = data_runs(attribute);
        }
        at += length;
    }
    Some((runs?, listed))
}

/// How deep into a volume names are listed, beyond what the file system
/// itself keeps.
pub struct Policy {
    /// Lowercased folder names from the root to the folder holding user
    /// profiles (`["users"]`), on this volume; empty when it is not here.
    pub profiles: Vec<String>,
    /// Lowercased profile folders listed in full there (the asking user's,
    /// and Public). The others are named, not listed: the app's own walk
    /// is refused at their door too.
    pub allowed: Vec<String>,
}

// Folders whose contents nobody searches for, as the app's walk skips them.
fn skipped(lowered: &str) -> bool {
    matches!(
        lowered,
        "$recycle.bin" | "system volume information" | "$winreagent"
    )
}

/// The volume's names as a tree from its root, parents first.
pub fn tree(table: &Table, root: &str, policy: &Policy) -> crate::protocol::Volume {
    let count = table.records.len();
    if !table
        .records
        .get(ROOT as usize)
        .is_some_and(|info| info.in_use && info.dir)
    {
        return crate::protocol::Volume::new(root);
    }
    // Children by folder, without a vector per folder: count, then fill. The
    // names and their bytes are counted here too, so the volume can be given
    // exactly the room it needs instead of growing by doubling.
    let mut first = vec![0u32; count + 1];
    let live = |name: &Name| {
        name.record >= FIRST_USER_RECORD
            && table
                .records
                .get(name.record as usize)
                .is_some_and(|info| info.in_use)
            && table
                .records
                .get(name.parent as usize)
                .is_some_and(|info| info.in_use && info.dir)
    };
    let mut entries = 0usize;
    let mut name_bytes = 0usize;
    for name in table.names.iter().filter(|name| live(name)) {
        first[name.parent as usize] += 1;
        entries += 1;
        name_bytes += crate::protocol::stored_name_len(table.name(name));
    }
    let mut running = 0u32;
    for slot in first.iter_mut() {
        let here = *slot;
        *slot = running;
        running += here;
    }
    let mut children = vec![0u32; running as usize];
    let mut next = first.clone();
    for (index, name) in table.names.iter().enumerate() {
        if live(name) {
            let slot = &mut next[name.parent as usize];
            children[*slot as usize] = index as u32;
            *slot += 1;
        }
    }
    // The fill counts are spent; on a big drive that is another table-sized
    // vector to hand back before the tree itself is built.
    drop(next);

    let mut volume = crate::protocol::Volume::sharing(root, std::sync::Arc::clone(&table.text));
    volume.reserve(entries, name_bytes);
    let mut visited = vec![false; count];
    // (record, its index in the volume, how many of `policy.profiles` its
    // path has matched so far, if it is on that path).
    let mut queue = std::collections::VecDeque::from([(ROOT, 0u32, Some(0usize))]);
    if let Some(seen) = visited.get_mut(ROOT as usize) {
        *seen = true;
    }
    while let Some((record, index, matched)) = queue.pop_front() {
        let at_profiles = !policy.profiles.is_empty() && matched == Some(policy.profiles.len());
        let range = first[record as usize] as usize..first[record as usize + 1] as usize;
        for &child in &children[range] {
            let name = &table.names[child as usize];
            let Some(info) = table.records.get(name.record as usize) else {
                continue;
            };
            let text = table.name(name);
            let child_index = volume.push_shared(
                index,
                name.start,
                name.len,
                info.dir,
                if info.dir { 0 } else { info.size },
                info.modified,
            );
            if !info.dir || visited[name.record as usize] {
                continue;
            }
            let lowered = text.to_lowercase();
            if skipped(&lowered) || (at_profiles && !policy.allowed.contains(&lowered)) {
                continue;
            }
            visited[name.record as usize] = true;
            let matched = matched
                .filter(|matched| policy.profiles.get(*matched) == Some(&lowered))
                .map(|matched| matched + 1);
            queue.push_back((name.record, child_index, matched));
        }
    }
    volume.shrink();
    volume
}

#[cfg(windows)]
pub use volume::read_volume;
#[cfg(windows)]
pub(crate) use volume::read_volume_interruptible;

#[cfg(windows)]
#[allow(unsafe_code)]
mod volume {
    use std::io;
    use std::os::windows::fs::{FileExt as _, OpenOptionsExt as _};
    use std::os::windows::io::AsRawHandle as _;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};

    use super::{Table, mft_runs, parse_record};

    // Big reads keep the drive streaming; the table is a few GB at most.
    const CHUNK: u64 = 8 * 1024 * 1024;

    /// Reads every record of the NTFS volume `root` ("C:"). Needs
    /// administrator rights: it opens the volume itself.
    pub fn read_volume(root: &str) -> io::Result<Table> {
        read_volume_interruptible(root, || false)
    }

    pub(crate) fn read_volume_interruptible(
        root: &str,
        cancelled: impl Fn() -> bool,
    ) -> io::Result<Table> {
        let volume = std::fs::OpenOptions::new()
            .read(true)
            // Share everything: the volume is in use by everyone else.
            .share_mode(7)
            .open(format!(r"\\.\{root}"))?;
        let mut data = NTFS_VOLUME_DATA_BUFFER::default();
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                HANDLE(volume.as_raw_handle()),
                FSCTL_GET_NTFS_VOLUME_DATA,
                None,
                0,
                Some(std::ptr::from_mut(&mut data).cast()),
                std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                Some(&mut returned),
                None,
            )
        }
        .map_err(io::Error::from)?;
        let cluster = u64::from(data.BytesPerCluster);
        let record_size = data.BytesPerFileRecordSegment as usize;
        let sector = (data.BytesPerSector as usize).max(512);
        let valid = u64::try_from(data.MftValidDataLength).unwrap_or(0);
        let start = u64::try_from(data.MftStartLcn).unwrap_or(0) * cluster;
        if cluster == 0 || !(256..=65_536).contains(&record_size) || valid == 0 {
            return Err(io::Error::other(format!(
                "{root}: unexpected NTFS geometry"
            )));
        }

        let mut first = vec![0u8; record_size.max(sector)];
        volume.seek_read(&mut first, start)?;
        let (runs, listed) = mft_runs(&mut first[..record_size])
            .ok_or_else(|| io::Error::other(format!("{root}: the $MFT record is unreadable")))?;
        let covered: u64 = runs.iter().map(|(_, clusters)| clusters * cluster).sum();
        if covered < valid && listed {
            return Err(io::Error::other(format!(
                "{root}: the $MFT is too fragmented to read whole"
            )));
        }

        // NTFS numbers a volume's records in 32 bits, so a table longer than
        // that is refused rather than indexed with wrapped numbers.
        let Ok(records) = u32::try_from(valid / record_size as u64) else {
            return Err(io::Error::other(format!(
                "{root}: the $MFT is too large to index"
            )));
        };

        let mut table = Table::default();
        table.records.reserve(records as usize);
        // One name a record is the usual case; hard links add more, which the
        // buffers take as they come before being trimmed at the end.
        table.names.reserve(records as usize);
        let mut buffer = vec![0u8; CHUNK as usize];
        let mut done = 0u64;
        'runs: for (lcn, clusters) in runs {
            let mut offset = lcn.map(|lcn| lcn * cluster);
            let mut remaining = clusters * cluster;
            while remaining > 0 {
                if cancelled() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "index rebuild stopped",
                    ));
                }
                if done >= valid {
                    break 'runs;
                }
                let length = remaining
                    .min(CHUNK)
                    .min(valid - done)
                    .next_multiple_of(cluster)
                    .min(remaining);
                let chunk = &mut buffer[..length as usize];
                match offset {
                    Some(at) => volume.seek_read(chunk, at).map(|_| ())?,
                    None => chunk.fill(0),
                }
                for (index, record) in chunk.chunks_exact_mut(record_size).enumerate() {
                    let number = done / record_size as u64 + index as u64;
                    // Within `records`, which the geometry above held to 32 bits.
                    parse_record(&mut table, number as u32, record);
                }
                done += length;
                remaining -= length;
                offset = offset.map(|at| at + length);
            }
        }
        // The table is kept for the service's whole run, so it gives back the
        // room it grew through; the text buffer doubles as names are added.
        table.records.shrink_to_fit();
        table.names.shrink_to_fit();
        std::sync::Arc::make_mut(&mut table.text).shrink_to_fit();
        Ok(table)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;

    const RECORD: usize = 1024;

    #[test]
    fn a_record_costs_16_bytes_of_info_and_16_of_name() {
        // The table is kept for the service's whole run, one of each per file
        // the drive holds.
        assert_eq!(16, std::mem::size_of::<Info>());
        assert_eq!(16, std::mem::size_of::<Name>());
    }

    #[test]
    fn a_file_reference_gives_its_number_and_never_a_wrapped_one() {
        assert_eq!(Some(31), record_of(31 | (3 << 48)));
        assert_eq!(Some(u32::MAX), record_of(u64::from(u32::MAX)));
        // Past the most records NTFS puts on one volume: not a record here.
        assert_eq!(None, record_of(0x1_0000_0000));
        assert_eq!(None, record_of(RECORD_NUMBER));
    }

    #[test]
    fn an_extension_record_past_what_a_table_numbers_is_left_out() {
        let mut table = Table::default();
        let mut extension = Record::new(IN_USE, 0x1_0000_0000)
            .name(31, "nowhere.txt", 3)
            .finish();
        parse_record(&mut table, 60, &mut extension);
        assert!(table.names.is_empty());
        assert!(table.extensions.is_empty());
    }

    #[test]
    fn a_tree_shares_the_tables_names_instead_of_copying_them() {
        // A snapshot off a drive must not hold a second copy of every name:
        // it reads them out of the table's own buffer.
        let table = sample();
        let policy = Policy {
            profiles: Vec::new(),
            allowed: Vec::new(),
        };
        let volume = tree(&table, "C:", &policy);
        assert!(volume.shares_names(&table.text));
    }

    #[test]
    fn a_tree_reserves_the_room_it_needs_instead_of_doubling_it() {
        // Two thousand names under one folder: enough that growing by
        // doubling would show, small enough to build in a test.
        let mut records = vec![
            (
                ROOT,
                Record::new(IN_USE | DIRECTORY, 0)
                    .name(ROOT, ".", 3)
                    .finish(),
            ),
            (
                30,
                Record::new(IN_USE | DIRECTORY, 0)
                    .name(ROOT, "Users", 3)
                    .finish(),
            ),
        ];
        for index in 0..2_000u32 {
            let name = format!("file-{index:05}.txt");
            records.push((
                31 + index,
                Record::new(IN_USE, 0).name(30, &name, 1).finish(),
            ));
        }
        let table = table_of(records);
        let policy = Policy {
            profiles: Vec::new(),
            allowed: Vec::new(),
        };
        let volume = tree(&table, "C:", &policy);
        assert_eq!(2_002, volume.len());
        for (capacity, len) in volume.room() {
            assert!(capacity >= len, "reserved {capacity} for {len}");
            assert!(capacity < len + 64, "left {capacity} for {len}");
        }
    }

    // A record as NTFS lays it out, update sequence included.
    pub(crate) struct Record {
        bytes: Vec<u8>,
        at: usize,
    }

    impl Record {
        pub(crate) fn new(flags: u16, base: u64) -> Self {
            let mut bytes = vec![0u8; RECORD];
            bytes[..4].copy_from_slice(b"FILE");
            bytes[4..6].copy_from_slice(&0x30u16.to_le_bytes());
            bytes[6..8].copy_from_slice(&3u16.to_le_bytes());
            bytes[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
            bytes[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
            bytes[0x20..0x28].copy_from_slice(&base.to_le_bytes());
            Self { bytes, at: 0x38 }
        }

        fn attribute(
            mut self,
            kind: u32,
            resident: bool,
            header: &[(usize, &[u8])],
            value: &[u8],
        ) -> Self {
            let value_offset = 0x18usize;
            let body = if resident {
                value_offset + value.len()
            } else {
                0x48 + value.len()
            };
            let length = body.next_multiple_of(8);
            let at = self.at;
            self.bytes[at..at + 4].copy_from_slice(&kind.to_le_bytes());
            self.bytes[at + 4..at + 8].copy_from_slice(&(length as u32).to_le_bytes());
            self.bytes[at + 8] = u8::from(!resident);
            if resident {
                self.bytes[at + 0x10..at + 0x14]
                    .copy_from_slice(&(value.len() as u32).to_le_bytes());
                self.bytes[at + 0x14..at + 0x16]
                    .copy_from_slice(&(value_offset as u16).to_le_bytes());
                self.bytes[at + value_offset..at + value_offset + value.len()]
                    .copy_from_slice(value);
            } else {
                self.bytes[at + 0x20..at + 0x22].copy_from_slice(&0x48u16.to_le_bytes());
                self.bytes[at + 0x48..at + 0x48 + value.len()].copy_from_slice(value);
            }
            for (offset, bytes) in header {
                self.bytes[at + offset..at + offset + bytes.len()].copy_from_slice(bytes);
            }
            self.at += length;
            self
        }

        pub(crate) fn modified(self, seconds: u64) -> Self {
            let mut value = vec![0u8; 0x48];
            value[8..16].copy_from_slice(&(seconds * 10_000_000 + EPOCH_TICKS).to_le_bytes());
            self.attribute(STANDARD_INFORMATION, true, &[], &value)
        }

        pub(crate) fn name(self, parent: u32, name: &str, namespace: u8) -> Self {
            let units: Vec<u16> = name.encode_utf16().collect();
            let mut value = vec![0u8; 0x42 + units.len() * 2];
            value[..8].copy_from_slice(&(u64::from(parent) | 0x0001_0000_0000_0000).to_le_bytes());
            value[0x40] = units.len() as u8;
            value[0x41] = namespace;
            for (index, unit) in units.iter().enumerate() {
                value[0x42 + index * 2..0x44 + index * 2].copy_from_slice(&unit.to_le_bytes());
            }
            self.attribute(FILE_NAME, true, &[], &value)
        }

        pub(crate) fn resident_data(self, size: usize) -> Self {
            self.attribute(DATA, true, &[], &vec![7u8; size])
        }

        pub(crate) fn attribute_list(self, records: &[u64]) -> Self {
            let mut value = vec![0u8; records.len() * 32];
            for (entry, number) in value.as_chunks_mut::<32>().0.iter_mut().zip(records) {
                entry[..4].copy_from_slice(&DATA.to_le_bytes());
                entry[4..6].copy_from_slice(&32u16.to_le_bytes());
                entry[16..24].copy_from_slice(&number.to_le_bytes());
            }
            self.attribute(ATTRIBUTE_LIST, true, &[], &value)
        }

        pub(crate) fn big_data(self, size: u64, runs: &[u8]) -> Self {
            let header: Vec<(usize, Vec<u8>)> = vec![(0x30, size.to_le_bytes().to_vec())];
            let header: Vec<(usize, &[u8])> = header
                .iter()
                .map(|(at, bytes)| (*at, bytes.as_slice()))
                .collect();
            self.attribute(DATA, false, &header, runs)
        }

        // Ends the attributes and writes the update sequence over each
        // sector's last two bytes, as NTFS does on disk.
        pub(crate) fn finish(mut self) -> Vec<u8> {
            let at = self.at;
            self.bytes[at..at + 4].copy_from_slice(&END.to_le_bytes());
            let used = at + 8;
            self.bytes[0x18..0x1C].copy_from_slice(&(used as u32).to_le_bytes());
            let check = 0x0A0Bu16;
            self.bytes[0x30..0x32].copy_from_slice(&check.to_le_bytes());
            for sector in 1..3usize {
                let end = sector * 512;
                let real = [self.bytes[end - 2], self.bytes[end - 1]];
                self.bytes[0x30 + sector * 2..0x32 + sector * 2].copy_from_slice(&real);
                self.bytes[end - 2..end].copy_from_slice(&check.to_le_bytes());
            }
            self.bytes
        }
    }

    pub(crate) fn table_of(records: Vec<(u32, Vec<u8>)>) -> Table {
        let mut table = Table::default();
        for (number, mut record) in records {
            parse_record(&mut table, number, &mut record);
        }
        table
    }

    fn sample() -> Table {
        table_of(vec![
            (
                ROOT,
                Record::new(IN_USE | DIRECTORY, 0)
                    .modified(100)
                    .name(ROOT, ".", 3)
                    .finish(),
            ),
            (
                11,
                Record::new(IN_USE | DIRECTORY, 0)
                    .name(ROOT, "$Extend", 3)
                    .finish(),
            ),
            (
                30,
                Record::new(IN_USE | DIRECTORY, 0)
                    .modified(200)
                    .name(ROOT, "Users", 3)
                    .finish(),
            ),
            (
                31,
                Record::new(IN_USE | DIRECTORY, 0)
                    .name(30, "ana", 1)
                    .name(30, "ANA~1", 2)
                    .finish(),
            ),
            (
                32,
                Record::new(IN_USE | DIRECTORY, 0)
                    .name(30, "outra", 1)
                    .finish(),
            ),
            (
                33,
                Record::new(IN_USE, 0)
                    .modified(300)
                    .name(31, "Bonk.wav", 1)
                    .big_data(5_000_000, &[0x11, 0x10, 0x20, 0])
                    .finish(),
            ),
            (
                34,
                Record::new(IN_USE, 0)
                    .name(32, "segredo.txt", 3)
                    .resident_data(12)
                    .finish(),
            ),
            (
                35,
                Record::new(IN_USE, 0).name(31, "apagado.txt", 3).finish(),
            ),
            (
                36,
                Record::new(IN_USE | DIRECTORY, 0)
                    .name(ROOT, "$Recycle.Bin", 3)
                    .finish(),
            ),
            (37, Record::new(IN_USE, 0).name(36, "lixo.txt", 3).finish()),
            // An extension record naming 38 a second time (a hard link).
            (
                38,
                Record::new(IN_USE, 0)
                    .name(31, "link a.txt", 3)
                    .resident_data(5)
                    .finish(),
            ),
            (
                39,
                Record::new(IN_USE, 38).name(ROOT, "link b.txt", 3).finish(),
            ),
            (40, Record::new(0, 0).name(31, "livre.txt", 3).finish()),
        ])
    }

    fn paths(volume: &crate::protocol::Volume) -> Vec<String> {
        let mut bytes = Vec::new();
        crate::protocol::write(&mut bytes, std::slice::from_ref(volume)).unwrap();
        struct Paths(Vec<String>);
        impl crate::protocol::Sink for Paths {
            fn volume(&mut self, _root: &str) {}
            fn entry(&mut self, parent: u32, name: &str, dir: bool, size: u64, modified: u32) {
                let path = match self.0.get(parent as usize) {
                    Some(parent) => {
                        format!("{}\\{name}", parent.split(' ').next().unwrap_or_default())
                    }
                    None => name.to_string(),
                };
                self.0.push(format!("{path} {dir} {size} {modified}"));
            }
        }
        let mut seen = Paths(Vec::new());
        crate::protocol::read(&mut bytes.as_slice(), &mut seen).unwrap();
        seen.0
    }

    #[test]
    fn records_become_a_tree_with_sizes_dates_and_hard_links() {
        let table = sample();
        let policy = Policy {
            profiles: vec!["users".into()],
            allowed: vec!["ana".into(), "public".into()],
        };
        let mut found = paths(&tree(&table, "C:", &policy));
        found.sort();
        assert_eq!(
            found,
            [
                "C: true 0 0",
                "C:\\$Recycle.Bin true 0 0",
                "C:\\Users true 0 200",
                "C:\\Users\\ana true 0 0",
                "C:\\Users\\ana\\Bonk.wav false 5000000 300",
                "C:\\Users\\ana\\apagado.txt false 0 0",
                "C:\\Users\\ana\\link a.txt false 5 0",
                "C:\\Users\\outra true 0 0",
                "C:\\link b.txt false 5 0",
            ]
        );
    }

    #[test]
    fn a_torn_record_is_left_out() {
        let mut record = Record::new(IN_USE, 0).name(ROOT, "rasgado.txt", 3).finish();
        record[1022] ^= 0xFF;
        let mut table = Table::default();
        parse_record(&mut table, 50, &mut record);
        assert!(table.names.is_empty());
        let mut garbage = vec![0xAAu8; RECORD];
        parse_record(&mut table, 51, &mut garbage);
        garbage[..4].copy_from_slice(b"FILE");
        parse_record(&mut table, 52, &mut garbage);
        assert!(table.names.is_empty());
    }

    #[test]
    fn data_runs_are_relative_and_may_be_sparse() {
        // 0x10 clusters at 0x20, then 0x08 clusters 0x10 before that, then a
        // sparse run of 4.
        let mut attribute = vec![0u8; 0x48];
        attribute[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        attribute.truncate(0x40);
        attribute.extend_from_slice(&[0x11, 0x10, 0x20, 0x11, 0x08, 0xF0, 0x01, 0x04, 0x00]);
        assert_eq!(
            data_runs(&attribute),
            Some(vec![(Some(0x20), 0x10), (Some(0x10), 0x08), (None, 4)])
        );
        attribute.truncate(attribute.len() - 1);
        assert_eq!(data_runs(&attribute), None);
    }
}
