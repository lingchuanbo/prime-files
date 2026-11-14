#![allow(unsafe_code)]
use super::{Cursor, RECORD_NUMBER, State, field, invalid, parse, prepare_record};
use std::os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io,
};
use windows::Win32::{
    Foundation::HANDLE,
    System::{
        IO::DeviceIoControl,
        Ioctl::{
            FSCTL_GET_NTFS_FILE_RECORD, FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL,
            FSCTL_READ_USN_JOURNAL,
        },
    },
};

const BUFFER_SIZE: usize = 1024 * 1024;
const MAX_BATCHES: usize = 256;
const MAX_CHANGES: usize = 32_768;

pub(in crate::service) struct Journal {
    file: File,
    record_size: usize,
}

impl Journal {
    pub fn open(root: &str) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(7)
            .open(format!(r"\\.\{root}"))?;
        let mut geometry = [0u8; 96];
        let returned = ioctl(&file, FSCTL_GET_NTFS_VOLUME_DATA, &[], &mut geometry)?;
        let size = u32::from_le_bytes(field(&geometry[..returned], 48)?) as usize;
        if !(256..=65_536).contains(&size) {
            return Err(invalid());
        }
        Ok(Self {
            file,
            record_size: size,
        })
    }

    pub fn state(&self) -> io::Result<State> {
        let mut bytes = [0u8; 80];
        let returned = ioctl(&self.file, FSCTL_QUERY_USN_JOURNAL, &[], &mut bytes)?;
        let bytes = &bytes[..returned];
        let first = i64::from_le_bytes(field(bytes, 8)?).max(i64::from_le_bytes(field(bytes, 24)?));
        let next = i64::from_le_bytes(field(bytes, 16)?);
        if first < 0 || next < first {
            return Err(invalid());
        }
        Ok(State {
            id: u64::from_le_bytes(field(bytes, 0)?),
            first,
            next,
        })
    }

    pub fn changes(&self, cursor: Cursor) -> io::Result<(Cursor, BTreeSet<u64>)> {
        let state = self.state()?;
        if !state.accepts(cursor) {
            return Err(invalid());
        }
        let mut input = [0u8; 48];
        input[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        input[32..40].copy_from_slice(&cursor.id.to_le_bytes());
        input[40..42].copy_from_slice(&2u16.to_le_bytes());
        input[42..44].copy_from_slice(&3u16.to_le_bytes());
        // BytesToWaitFor=0：没有变化立即返回，停机不依赖新文件事件。
        let mut next = cursor.next;
        let mut changes = BTreeSet::new();
        if next >= state.next {
            return Ok((cursor, changes));
        }
        let mut buffer = vec![0; BUFFER_SIZE];
        for _ in 0..MAX_BATCHES {
            if next >= state.next {
                return Ok((Cursor { next, ..cursor }, changes));
            }
            input[..8].copy_from_slice(&next.to_le_bytes());
            let returned = ioctl(&self.file, FSCTL_READ_USN_JOURNAL, &input, &mut buffer)?;
            let (after, records) = parse(&buffer[..returned], next)?;
            if after <= next {
                return Err(invalid());
            }
            changes.extend(records);
            if changes.len() > MAX_CHANGES {
                return Err(invalid());
            }
            next = after;
        }
        Err(invalid())
    }

    pub fn record(&self, number: u64) -> io::Result<Option<Vec<u8>>> {
        let mut bytes = vec![0; 12 + self.record_size];
        let returned = ioctl(
            &self.file,
            FSCTL_GET_NTFS_FILE_RECORD,
            &number.to_le_bytes(),
            &mut bytes,
        )?;
        let bytes = &bytes[..returned];
        let found = u64::from_le_bytes(field(bytes, 0)?) & RECORD_NUMBER;
        // 此 API 对空闲槽返回前一个有效记录；不能将它误认为请求的文件。
        if found != number {
            return Ok(None);
        }
        let length = u32::from_le_bytes(field(bytes, 8)?) as usize;
        if length < 48 {
            return Err(invalid());
        }
        let record = bytes.get(12..12 + length).ok_or_else(invalid)?;
        let mut record = record.to_vec();
        prepare_record(&mut record)?;
        Ok(Some(record))
    }
}

fn ioctl(file: &File, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
    let mut returned = 0;
    // 所有指针对应存活的切片，句柄由 File 持有，调用是同步的。
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            code,
            Some(input.as_ptr().cast()),
            input.len() as u32,
            Some(output.as_mut_ptr().cast()),
            output.len() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(io::Error::from)?;
    if returned as usize > output.len() {
        return Err(invalid());
    }
    Ok(returned as usize)
}
