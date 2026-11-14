//! Pinyin initials, so a name can be typed the way a Chinese speaker reads
//! it: `wjj` finds 文件夹, `gz` finds 工作. Only the initials are matched, not
//! the whole syllable (`wjj` yes, `wenjianjia` no), which is what Total
//! Commander's "use pinyin" search does and what people type.
//!
//! The initial of a character comes from Windows' own zh-CN collation, which
//! sorts Han characters the way a pinyin dictionary does: the initials run in
//! alphabetical order through it, so comparing a character against one anchor
//! per initial tells which initial it belongs to. The answer is kept per
//! character in a lock-free table: a folder is filtered on every keystroke,
//! and each Han character should cost the comparison once.

use std::sync::atomic::{AtomicU8, Ordering};
use pinyin::ToPinyin;

/// First and last Han character the table covers: the CJK Unified Ideographs
/// block, which is where a file name's characters come from.
const FIRST: u32 = 0x4E00;
const LAST: u32 = 0x9FFF;

/// The initial of each Han character from `FIRST` to `LAST`, as a letter
/// number plus one; 0 means "not worked out yet". Atomic, so the filter and
/// the subfolder search can fill it from different threads without a lock.
static INITIALS: [AtomicU8; (LAST - FIRST + 1) as usize] = [const { AtomicU8::new(0) }; (LAST - FIRST + 1) as usize];

/// Whether `ch` is a Han character the table has an initial for.
pub(crate) fn is_han(ch: char) -> bool {
    (FIRST..=LAST).contains(&(ch as u32))
}

/// The initial of `ch`'s pinyin, lowercase (`文` -> `w`). `None` for anything
/// pinyin has no initial for: Latin letters and digits have none of their own
/// here, and kana, Cyrillic, symbols and rare Han outside the table have no
/// reading this can find.
pub(crate) fn initial(ch: char) -> Option<char> {
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

/// How `text` is typed in initials: its ASCII letters and digits as they are,
/// each Han character as the initial of its pinyin, and nothing for the rest
/// (`文件夹 (2026)` -> `wjj(2026)`). Lowercase, since the filter ignores case.
/// `None` when there is no Han character to translate, which is most names:
/// the caller then has nothing new to try and no string to build.
pub(crate) fn initials(text: &str) -> Option<String> {
    if !text.chars().any(is_han) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push(ch.to_ascii_lowercase());
        } else if let Some(letter) = initial(ch) {
            out.push(letter);
        }
    }
    Some(out)
}

/// The full, tone-free pinyin of a name that contains Han characters.
/// Latin text and punctuation stay in place so mixed names can match too.
pub(crate) fn full(text: &str) -> Option<String> {
    if !text.chars().any(|ch| ch.to_pinyin().is_some()) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if let Some(pinyin) = ch.to_pinyin() {
            out.push_str(pinyin.plain());
        } else {
            out.extend(ch.to_lowercase());
        }
    }
    Some(out)
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! Windows' zh-CN collation has no safe binding.

    use windows::Win32::Globalization::{COMPARE_STRING_FLAGS, CompareStringEx};
    use windows::core::w;

    /// One character per pinyin initial, in the order the initials run in a
    /// pinyin-sorted dictionary: `a` first, `z` last, with no `i`, `u` or `v`
    /// (no syllable starts with them). Each is a common character whose
    /// reading starts with that initial and with nothing else.
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

    /// Which initial `ch` belongs to: the last anchor it sorts at or after.
    /// A character sorting before every anchor (not pinyin at all, or a
    /// reading newer Windows sorts elsewhere) keeps the first one, so it is
    /// still findable rather than dropped.
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

    // How the zh-CN dictionary orders two characters: negative when `a`
    // comes first, as `Ordering` reads.
    fn compare(a: char, b: char) -> i32 {
        let mut left = [0u16; 2];
        let mut right = [0u16; 2];
        let left_len = a.encode_utf16(&mut left).len();
        let right_len = b.encode_utf16(&mut right).len();
        // CompareStringEx answers 1 (less), 2 (equal) or 3 (greater); 0 is an
        // error, which reads as "not less" and keeps the search permissive.
        let result = unsafe {
            CompareStringEx(
                w!("zh-CN"),
                COMPARE_STRING_FLAGS(0),
                &left[..left_len],
                &right[..right_len],
                None,
                None,
                None,
            )
        };
        result.0 as i32 - 2
    }
}

#[cfg(not(windows))]
mod platform {
    /// No pinyin without Windows' collation: initials match nothing, and the
    /// rest of the filter works as it did.
    pub fn initial(_ch: char) -> Option<char> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn han_characters_give_the_initial_they_are_read_with() {
        assert_eq!(initial('文'), Some('w'));
        assert_eq!(initial('件'), Some('j'));
        assert_eq!(initial('夹'), Some('j'));
        assert_eq!(initial('工'), Some('g'));
        assert_eq!(initial('作'), Some('z'));
        assert_eq!(initial('一'), Some('y'));
        assert_eq!(initial('七'), Some('q'));
        assert_eq!(initial('中'), Some('z'));
        assert_eq!(initial('阿'), Some('a'));
        assert_eq!(initial('额'), Some('e'));
        assert_eq!(initial('欧'), Some('o'));
    }

    #[cfg(windows)]
    #[test]
    fn a_name_is_typed_in_its_initials_with_the_rest_left_alone() {
        // Han characters become initials; letters, digits and symbols stay.
        assert_eq!(initials("文件夹").as_deref(), Some("wjj"));
        assert_eq!(initials("工作报告.docx").as_deref(), Some("gzbg.docx"));
        assert_eq!(initials("项目A报告").as_deref(), Some("xmabg"));
        // Nothing to translate: no string is built at all.
        assert_eq!(initials("plain.txt"), None);
        assert_eq!(initials(""), None);
    }

    #[test]
    fn only_the_han_block_has_initials() {
        assert!(!is_han('a'));
        assert!(is_han('文'));
        assert_eq!(initial('a'), None);
        assert_eq!(initial('。'), None);
        assert_eq!(initial('ア'), None);
    }
}
