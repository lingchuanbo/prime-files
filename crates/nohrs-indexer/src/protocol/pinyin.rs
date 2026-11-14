//! Pinyin initials for the service's fallback filename search.

use std::sync::atomic::{AtomicU8, Ordering};
use pinyin::ToPinyin;

const FIRST: u32 = 0x4E00;
const LAST: u32 = 0x9FFF;
static INITIALS: [AtomicU8; (LAST - FIRST + 1) as usize] =
    [const { AtomicU8::new(0) }; (LAST - FIRST + 1) as usize];

pub(super) fn initials(text: &str) -> Option<String> {
    if !text.chars().any(is_han) {
        return None;
    }
    let mut initials = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() {
            initials.push(ch.to_ascii_lowercase());
        } else if let Some(letter) = initial(ch) {
            initials.push(letter);
        }
    }
    Some(initials)
}

pub(super) fn full(text: &str) -> Option<String> {
    if !text.chars().any(|ch| ch.to_pinyin().is_some()) {
        return None;
    }
    let mut full = String::with_capacity(text.len());
    for ch in text.chars() {
        if let Some(pinyin) = ch.to_pinyin() {
            full.push_str(pinyin.plain());
        } else {
            full.extend(ch.to_lowercase());
        }
    }
    Some(full)
}

fn is_han(ch: char) -> bool {
    (FIRST..=LAST).contains(&(ch as u32))
}

fn initial(ch: char) -> Option<char> {
    if !is_han(ch) {
        return None;
    }
    let slot = &INITIALS[(ch as u32 - FIRST) as usize];
    let cached = slot.load(Ordering::Relaxed);
    if cached != 0 {
        return Some((b'a' + cached - 1) as char);
    }
    let letter = platform::initial(ch)?;
    slot.store(letter as u8 - b'a' + 1, Ordering::Relaxed);
    Some(letter)
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use std::ptr;

    const LOCALE: [u16; 6] = [
        b'z' as u16,
        b'h' as u16,
        b'-' as u16,
        b'C' as u16,
        b'N' as u16,
        0,
    ];
    const ANCHORS: [(char, char); 23] = [
        ('阿', 'a'),
        ('八', 'b'),
        ('擦', 'c'),
        ('搭', 'd'),
        ('蛾', 'e'),
        ('发', 'f'),
        ('嘎', 'g'),
        ('哈', 'h'),
        ('击', 'j'),
        ('喀', 'k'),
        ('拉', 'l'),
        ('妈', 'm'),
        ('拿', 'n'),
        ('哦', 'o'),
        ('啪', 'p'),
        ('七', 'q'),
        ('然', 'r'),
        ('撒', 's'),
        ('塌', 't'),
        ('挖', 'w'),
        ('昔', 'x'),
        ('压', 'y'),
        ('匝', 'z'),
    ];

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CompareStringEx(
            locale: *const u16,
            flags: u32,
            left: *const u16,
            left_len: i32,
            right: *const u16,
            right_len: i32,
            version: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
            parameter: isize,
        ) -> i32;
    }

    pub fn initial(ch: char) -> Option<char> {
        let mut found = ANCHORS[0].1;
        for (anchor, letter) in ANCHORS {
            if compare(ch, anchor) < 0 {
                break;
            }
            found = letter;
        }
        Some(found)
    }

    fn compare(left: char, right: char) -> i32 {
        let mut left_buf = [0u16; 2];
        let mut right_buf = [0u16; 2];
        let left_len = left.encode_utf16(&mut left_buf).len() as i32;
        let right_len = right.encode_utf16(&mut right_buf).len() as i32;
        // SAFETY: all pointers refer to initialized UTF-16 buffers for their
        // supplied lengths, and optional API arguments are null as documented.
        let result = unsafe {
            CompareStringEx(
                LOCALE.as_ptr(),
                0,
                left_buf.as_ptr(),
                left_len,
                right_buf.as_ptr(),
                right_len,
                ptr::null_mut(),
                ptr::null_mut(),
                0,
            )
        };
        result - 2
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn initial(_ch: char) -> Option<char> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn chinese_names_produce_expected_initials() {
        assert_eq!(initials("文件夹").as_deref(), Some("wjj"));
        assert_eq!(initials("工作报告.docx").as_deref(), Some("gzbg.docx"));
    }

    #[test]
    fn plain_ascii_names_need_no_conversion() {
        assert_eq!(initials("readme.txt"), None);
    }
}
