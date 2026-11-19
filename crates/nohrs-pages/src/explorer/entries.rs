use super::types::SortKey;
use nohrs_services::fs::listing::FileEntryDto;
use std::cmp::Ordering;

/// Where folders go in a sorted listing (File Pilot's "Cycle Folders Sort
/// Order").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FolderOrder {
    /// Folders first, then files.
    #[default]
    First,
    /// Files first, then folders.
    Last,
    /// Folders and files sorted together.
    Mixed,
}

impl FolderOrder {
    /// The next in File Pilot's cycle.
    pub fn next(self) -> Self {
        match self {
            Self::First => Self::Last,
            Self::Last => Self::Mixed,
            Self::Mixed => Self::First,
        }
    }
}

/// Sorts with folders first, the default order.
#[cfg(test)]
pub fn sort_entries(entries: &mut [FileEntryDto], key: SortKey, asc: bool) {
    sort_entries_with(entries, key, asc, FolderOrder::First);
}

/// Sorts by `key`, with folders placed as `folders` says. Names compare as
/// Explorer compares them (`2.txt` before `10.txt`), so a folder lists in the
/// order the user already knows from it.
pub fn sort_entries_with(
    entries: &mut [FileEntryDto],
    key: SortKey,
    asc: bool,
    folders: FolderOrder,
) {
    let grouping = |a: &FileEntryDto, b: &FileEntryDto| {
        let (is_dir_a, is_dir_b) = (a.kind == "dir", b.kind == "dir");
        match folders {
            FolderOrder::First => is_dir_b.cmp(&is_dir_a),
            FolderOrder::Last => is_dir_a.cmp(&is_dir_b),
            FolderOrder::Mixed => Ordering::Equal,
        }
    };
    let direction = |order: Ordering| if asc { order } else { order.reverse() };

    match key {
        SortKey::Size | SortKey::Modified => {
            let value = |entry: &FileEntryDto| match key {
                SortKey::Size => entry.size,
                _ => entry.modified,
            };
            let compare = |a: &FileEntryDto, b: &FileEntryDto| {
                grouping(a, b).then_with(|| direction(value(a).cmp(&value(b))))
            };
            if !entries.is_sorted_by(|a, b| compare(a, b) != Ordering::Greater) {
                entries.sort_by(compare);
            }
        }
        SortKey::Name | SortKey::Type => {
            // Each name is prepared once, not on every comparison: the old
            // `to_lowercase()` inside the comparator allocated two strings per
            // comparison, about 34 per entry in a folder of 35,000.
            let keys: Vec<SortName> = entries
                .iter()
                .map(|entry| match key {
                    SortKey::Name => SortName::new(&entry.name),
                    _ => SortName::new(&get_extension(&entry.name, &entry.kind)),
                })
                .collect();
            let compare = |a: usize, b: usize| {
                grouping(&entries[a], &entries[b]).then_with(|| direction(keys[a].cmp(&keys[b])))
            };
            // The listing is sorted again after every filter and on every
            // reload; most of those calls get it already in order, and a check
            // is one comparison per entry rather than a sort.
            if (1..entries.len()).all(|index| compare(index - 1, index) != Ordering::Greater) {
                return;
            }
            let mut order: Vec<usize> = (0..entries.len()).collect();
            order.sort_by(|&a, &b| compare(a, b));
            apply_order(entries, order);
        }
    }
}

/// Puts `entries[order[i]]` at `i` for every `i`, by swapping along the
/// permutation's cycles: no entry is cloned.
fn apply_order<T>(entries: &mut [T], mut order: Vec<usize>) {
    for start in 0..order.len() {
        let mut current = start;
        while order[current] != start {
            let next = order[current];
            entries.swap(current, next);
            order[current] = current;
            current = next;
        }
        order[current] = current;
    }
}

/// A name ready to compare, prepared once per entry.
///
/// On Windows it is the system's sort key for the name, the bytes
/// `CompareString` itself compares: case ignored and digit runs as numbers,
/// which is the order `StrCmpLogicalW` gives and Explorer shows. Calling
/// `StrCmpLogicalW` in the comparator gave that order too, at about 0.8 µs a
/// comparison: 35,000 names took 140 ms against 46 ms for the old lowercase
/// sort. With the key built once, each comparison is a byte compare
/// (`sort_timing` below; `names_sort_as_strcmplogicalw_does` holds the two to
/// the same order).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SortName(Vec<u8>);

impl SortName {
    #[cfg(windows)]
    #[allow(unsafe_code)]
    pub(crate) fn new(name: &str) -> Self {
        use windows::Win32::Globalization::{
            LCMAP_SORTKEY, LCMapStringW, NORM_IGNORECASE, SORT_DIGITSASNUMBERS,
        };
        use windows::core::PWSTR;
        // LOCALE_USER_DEFAULT, the locale `StrCmpLogicalW` compares in.
        const USER_LOCALE: u32 = 0x0400;
        let flags = LCMAP_SORTKEY | NORM_IGNORECASE.0 | SORT_DIGITSASNUMBERS.0;
        let source: Vec<u16> = name.encode_utf16().collect();
        if source.is_empty() {
            return Self(Vec::new());
        }
        // For a sort key the destination is bytes, and both lengths are in
        // bytes; the first call asks how many.
        let needed = unsafe { LCMapStringW(USER_LOCALE, flags, &source, None, 0) };
        let Ok(length) = usize::try_from(needed) else {
            return Self(name.to_lowercase().into_bytes());
        };
        let mut key = vec![0u8; length];
        let written = unsafe {
            LCMapStringW(
                USER_LOCALE,
                flags,
                &source,
                Some(PWSTR(key.as_mut_ptr().cast())),
                needed,
            )
        };
        match usize::try_from(written) {
            Ok(written) if written > 0 => {
                key.truncate(written);
                Self(key)
            }
            _ => Self(name.to_lowercase().into_bytes()),
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn new(name: &str) -> Self {
        Self(name.to_lowercase().into_bytes())
    }
}

pub fn get_extension(name: &str, kind: &str) -> String {
    match kind {
        "dir" => "0_dir".to_string(),
        "file" => std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| "zzz_noext".to_string()),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: &str, size: u64) -> FileEntryDto {
        FileEntryDto {
            name: name.to_string(),
            path: format!("/{name}"),
            kind: kind.to_string(),
            size,
            modified: 0,
        }
    }

    #[test]
    fn sort_entries_keeps_directories_before_files() {
        let mut entries = vec![
            entry("b.txt", "file", 1),
            entry("alpha", "dir", 0),
            entry("a.txt", "file", 2),
        ];
        sort_entries(&mut entries, SortKey::Name, true);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "a.txt", "b.txt"]);
    }

    #[test]
    fn sort_entries_reverses_within_each_group_when_descending() {
        let mut entries = vec![
            entry("a.txt", "file", 1),
            entry("b.txt", "file", 2),
            entry("dir-a", "dir", 0),
        ];
        sort_entries(&mut entries, SortKey::Name, false);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        // Directories still sort ahead of files; ordering within files reverses.
        assert_eq!(names, ["dir-a", "b.txt", "a.txt"]);
    }

    #[test]
    fn get_extension_classifies_kinds() {
        assert_eq!(get_extension("anything", "dir"), "0_dir");
        assert_eq!(get_extension("Main.RS", "file"), "rs");
        assert_eq!(get_extension("README", "file"), "zzz_noext");
    }

    #[test]
    fn get_extension_edge_cases() {
        // Multi-dot names use the final component only.
        assert_eq!(get_extension("archive.tar.gz", "file"), "gz");
        assert_eq!(get_extension("", "file"), "zzz_noext");
        // Unknown kinds fall through to the kind string itself.
        assert_eq!(get_extension("link", "symlink"), "symlink");
    }

    #[test]
    fn sort_by_size_orders_within_groups_and_keeps_dirs_first() {
        let mut entries = vec![
            entry("big.bin", "file", 300),
            entry("small.bin", "file", 100),
            entry("zzz_dir", "dir", 0),
            entry("mid.bin", "file", 200),
        ];
        sort_entries(&mut entries, SortKey::Size, true);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["zzz_dir", "small.bin", "mid.bin", "big.bin"]);

        sort_entries(&mut entries, SortKey::Size, false);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        // Directories still lead; files reverse to largest-first.
        assert_eq!(names, ["zzz_dir", "big.bin", "mid.bin", "small.bin"]);
    }

    #[test]
    fn sort_by_modified_orders_by_timestamp() {
        let mut entries = vec![
            FileEntryDto {
                name: "old".into(),
                path: "/old".into(),
                kind: "file".into(),
                size: 0,
                modified: 100,
            },
            FileEntryDto {
                name: "new".into(),
                path: "/new".into(),
                kind: "file".into(),
                size: 0,
                modified: 900,
            },
        ];
        sort_entries(&mut entries, SortKey::Modified, true);
        assert_eq!(entries[0].name, "old");
        sort_entries(&mut entries, SortKey::Modified, false);
        assert_eq!(entries[0].name, "new");
    }

    #[test]
    fn folder_order_puts_folders_first_last_or_among_files() {
        let names = |entries: &[FileEntryDto]| -> Vec<String> {
            entries.iter().map(|entry| entry.name.clone()).collect()
        };
        let mut entries = vec![
            entry("b.txt", "file", 1),
            entry("a", "dir", 0),
            entry("c", "dir", 0),
        ];
        sort_entries_with(&mut entries, SortKey::Name, true, FolderOrder::First);
        assert_eq!(names(&entries), ["a", "c", "b.txt"]);
        sort_entries_with(&mut entries, SortKey::Name, true, FolderOrder::Last);
        assert_eq!(names(&entries), ["b.txt", "a", "c"]);
        sort_entries_with(&mut entries, SortKey::Name, true, FolderOrder::Mixed);
        assert_eq!(names(&entries), ["a", "b.txt", "c"]);
        assert_eq!(FolderOrder::Mixed.next(), FolderOrder::First);
    }

    #[test]
    fn names_sort_as_explorer_sorts_them() {
        let mut entries = vec![
            entry("f10.txt", "file", 0),
            entry("F2.txt", "file", 0),
            entry("f1.txt", "file", 0),
            entry("b", "dir", 0),
            entry("A", "dir", 0),
        ];
        sort_entries(&mut entries, SortKey::Name, true);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        #[cfg(windows)]
        assert_eq!(names, ["A", "b", "f1.txt", "F2.txt", "f10.txt"]);
        #[cfg(not(windows))]
        assert_eq!(names, ["A", "b", "f1.txt", "f10.txt", "F2.txt"]);
    }

    // The permutation is applied by swapping along cycles, which is easy to
    // get subtly wrong; every order here is checked against a plain sort.
    #[test]
    fn apply_order_matches_a_plain_sort_for_many_shapes() {
        let mut seed = 0x2545_f491_u64;
        for len in 0..200 {
            let values: Vec<u64> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed % 50
                })
                .collect();
            let mut order: Vec<usize> = (0..len).collect();
            order.sort_by_key(|&index| values[index]);
            let mut permuted = values.clone();
            apply_order(&mut permuted, order);
            let mut expected = values.clone();
            expected.sort();
            assert_eq!(permuted, expected, "length {len}");
        }
    }

    /// The keys must give the order Explorer gives. Each neighbour pair of
    /// the key order is checked with `StrCmpLogicalW` itself, over awkward
    /// names and over the real names of System32 and WinSxS\Manifests.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    #[test]
    fn names_sort_as_strcmplogicalw_does() {
        use windows::Win32::UI::Shell::StrCmpLogicalW;
        use windows::core::HSTRING;
        let logical =
            |a: &str, b: &str| unsafe { StrCmpLogicalW(&HSTRING::from(a), &HSTRING::from(b)) };

        let mut names: Vec<String> = [
            "a10",
            "a2",
            "A1",
            "a01",
            "a001",
            "b",
            "_x",
            "-y",
            "x 1",
            "x1",
            "x10",
            "ação",
            "acao",
            "Acao",
            "Zebra",
            "zebra",
            "01",
            "1",
            "001",
            "é",
            "e",
            "file(2)",
            "file (10)",
            "file (2)",
            "v1.10.txt",
            "v1.9.txt",
            "IMG_0010.JPG",
            "IMG_009.jpg",
            "~tmp",
            "#1",
            "Ünïcode",
            "日本",
            "1.5",
            "1,5",
            "99999999999999999999",
            "100000000000000000000",
        ]
        .iter()
        .map(|name| name.to_string())
        .collect();
        for folder in [r"C:\Windows\System32", r"C:\Windows\WinSxS\Manifests"] {
            if let Ok(read) = std::fs::read_dir(folder) {
                names.extend(
                    read.flatten()
                        .map(|entry| entry.file_name().to_string_lossy().into_owned()),
                );
            }
        }
        let mut keyed: Vec<(SortName, &str)> = names
            .iter()
            .map(|name| (SortName::new(name), name.as_str()))
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        let disagreements: Vec<(&str, &str)> = keyed
            .windows(2)
            .filter(|pair| logical(pair[0].1, pair[1].1) > 0)
            .map(|pair| (pair[0].1, pair[1].1))
            .take(10)
            .collect();
        assert!(
            disagreements.is_empty(),
            "out of Explorer's order: {disagreements:?}"
        );
    }

    /// Times sorting a large folder by name against the sort as it was before
    /// the 04/11/2026 pass (kept literally below). Run with
    /// `cargo test --release -p nohrs-pages sort_timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn sort_timing() {
        fn legacy(entries: &mut [FileEntryDto]) {
            entries.sort_by(|a, b| {
                let is_dir_a = a.kind == "dir";
                let is_dir_b = b.kind == "dir";
                match is_dir_b.cmp(&is_dir_a) {
                    Ordering::Equal => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    other => other,
                }
            });
        }
        let mut seed = 0x9e37_79b9_u64;
        let shuffled: Vec<FileEntryDto> = (0..35_000)
            .map(|index| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let kind = if index % 10 == 0 { "dir" } else { "file" };
                entry(
                    &format!("Amd64_Microsoft-Windows-{seed:x}_{index}.manifest"),
                    kind,
                    0,
                )
            })
            .collect();
        let median = |run: &dyn Fn(&mut Vec<FileEntryDto>)| {
            let mut samples: Vec<f64> = (0..7)
                .map(|_| {
                    let mut entries = shuffled.clone();
                    let start = std::time::Instant::now();
                    run(&mut entries);
                    start.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            samples.sort_by(f64::total_cmp);
            samples[3]
        };
        let before = median(&|entries| legacy(entries));
        let now = median(&|entries| sort_entries(entries, SortKey::Name, true));
        let mut sorted = shuffled.clone();
        sort_entries(&mut sorted, SortKey::Name, true);
        let again = median(&|entries| {
            entries.clone_from(&sorted);
            sort_entries(entries, SortKey::Name, true)
        });
        println!(
            "35,000 names: before {before:.1} ms, now {now:.1} ms, already sorted {again:.1} ms"
        );
    }
}
