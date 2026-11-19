//! "Group by": the listing split into labelled runs (kind, date, size), as
//! Explorer and File Pilot do.
//!
//! Grouping only reorders `filtered_entries` so each group is contiguous and
//! records where each run starts. Everything addressed by entry index —
//! selection, keyboard movement, rename, activation — keeps working unchanged;
//! only the views insert a header row before each run.

use nohrs_core::i18n;
use nohrs_core::{k, t};
use nohrs_services::fs::listing::FileEntryDto;
use time::{Date, Duration, OffsetDateTime, UtcOffset};

use super::filter::KindFilter;

/// What the listing is grouped by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroupBy {
    /// One flat listing.
    #[default]
    None,
    /// Folders, images, videos, audio, documents, archives, programs.
    Kind,
    /// Today, yesterday, this week… newest first.
    Modified,
    /// Exact modification time, newest first, without section headers.
    ModifiedFlat,
    /// Empty, tiny, small… smallest first.
    Size,
}

/// One labelled run of `filtered_entries`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// The header's text.
    pub label: &'static str,
    /// Index of the run's first entry.
    pub start: usize,
    /// How many entries the run has.
    pub len: usize,
}

/// The day boundaries "Today" and "This week" are measured from: the local
/// date and the offset that turns a timestamp into one.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    today: Date,
    offset: UtcOffset,
}

impl Clock {
    pub fn now() -> Self {
        // Without the local offset (which `time` refuses to read on some
        // platforms) days are UTC days: off by hours at worst, never wrong
        // about which files are old.
        let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        Self {
            today: OffsetDateTime::now_utc().to_offset(offset).date(),
            offset,
        }
    }

    #[cfg(test)]
    fn at(today: Date) -> Self {
        Self {
            today,
            offset: UtcOffset::UTC,
        }
    }

    fn date_of(&self, timestamp: u64) -> Option<Date> {
        let moment = OffsetDateTime::from_unix_timestamp(i64::try_from(timestamp).ok()?).ok()?;
        Some(moment.to_offset(self.offset).date())
    }
}

/// Reorders `entries` so each group is one run, keeping the current sort
/// inside every group, and returns the runs in display order. `GroupBy::None`
/// leaves the entries alone and returns no groups.
pub fn group_entries(entries: &mut [FileEntryDto], by: GroupBy, clock: &Clock) -> Vec<Group> {
    if by == GroupBy::None {
        return Vec::new();
    }
    if by == GroupBy::ModifiedFlat {
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.modified));
        return Vec::new();
    }
    // A stable sort by the group's rank: within a group, the order the
    // listing was already sorted in survives.
    entries.sort_by_key(|entry| group_of(entry, by, clock).0);
    let mut groups: Vec<Group> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let (_, label) = group_of(entry, by, clock);
        match groups.last_mut() {
            Some(group) if group.label == label => group.len += 1,
            _ => groups.push(Group {
                label,
                start: index,
                len: 1,
            }),
        }
    }
    groups
}

// The group an entry falls in, as (display rank, label).
fn group_of(entry: &FileEntryDto, by: GroupBy, clock: &Clock) -> (u8, &'static str) {
    let is_dir = entry.kind == "dir";
    match by {
        GroupBy::None => (0, ""),
        GroupBy::Kind => {
            const KINDS: [(KindFilter, &str); 7] = [
                (KindFilter::Folders, k!("Pastas")),
                (KindFilter::Images, k!("Imagens")),
                (KindFilter::Videos, k!("Vídeos")),
                (KindFilter::Audio, k!("Áudio")),
                (KindFilter::Documents, k!("Documentos")),
                (KindFilter::Archives, k!("Compactados")),
                (KindFilter::Programs, k!("Programas")),
            ];
            KINDS
                .iter()
                .zip(0u8..)
                .find(|((kind, _), _)| kind.matches(entry))
                .map_or((KINDS.len() as u8, t!("Outros")), |((_, label), rank)| {
                    (rank, i18n::tr(label))
                })
        }
        GroupBy::Modified => {
            let Some(date) = clock.date_of(entry.modified) else {
                return (7, t!("Sem data"));
            };
            let today = clock.today;
            let week_start =
                today - Duration::days(i64::from(today.weekday().number_days_from_monday()));
            if date >= today {
                (0, t!("Hoje"))
            } else if Some(date) == today.previous_day() {
                (1, t!("Ontem"))
            } else if date >= week_start {
                (2, t!("Esta semana"))
            } else if date.year() == today.year() && date.month() == today.month() {
                (3, t!("Este mês"))
            } else if date.year() == today.year() {
                (4, t!("Este ano"))
            } else if date.year() == today.year() - 1 {
                (5, t!("Ano passado"))
            } else {
                (6, t!("Mais antigos"))
            }
        }
        // Explorer's own size bands.
        GroupBy::ModifiedFlat => (0, ""),
        GroupBy::Size => match entry.size {
            _ if is_dir => (0, t!("Pastas")),
            0 => (1, t!("Vazios")),
            size if size < 16 * 1024 => (2, t!("Minúsculos (até 16 KB)")),
            size if size < 1024 * 1024 => (3, t!("Pequenos (16 KB a 1 MB)")),
            size if size < 128 * 1024 * 1024 => (4, t!("Médios (1 a 128 MB)")),
            size if size < 1024 * 1024 * 1024 => (5, t!("Grandes (128 MB a 1 GB)")),
            size if size < 4 * 1024 * 1024 * 1024 => (6, t!("Enormes (1 a 4 GB)")),
            _ => (7, t!("Gigantes (mais de 4 GB)")),
        },
    }
}

/// One row of the list view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListRow {
    /// The header of `groups[index]`.
    Group(usize),
    /// `filtered_entries[index]`.
    Entry(usize),
}

/// The list's rows: every entry, with a header before each group.
pub fn list_rows(groups: &[Group], entries: usize) -> Vec<ListRow> {
    if groups.is_empty() {
        return (0..entries).map(ListRow::Entry).collect();
    }
    let mut rows = Vec::with_capacity(entries + groups.len());
    for (index, group) in groups.iter().enumerate() {
        rows.push(ListRow::Group(index));
        rows.extend((group.start..group.start + group.len).map(ListRow::Entry));
    }
    rows
}

/// One row of the grid view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridRow {
    /// The header of `groups[index]`.
    Group(usize),
    /// Tiles for `filtered_entries[first..end]`.
    Tiles { first: usize, end: usize },
}

/// The grid's rows: each group starts on a row of its own, under its header.
pub fn grid_rows(groups: &[Group], entries: usize, columns: usize) -> Vec<GridRow> {
    let columns = columns.max(1);
    let tiles = |start: usize, len: usize| {
        (start..start + len)
            .step_by(columns)
            .map(move |first| GridRow::Tiles {
                first,
                end: (first + columns).min(start + len),
            })
    };
    if groups.is_empty() {
        return tiles(0, entries).collect();
    }
    let mut rows = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        rows.push(GridRow::Group(index));
        rows.extend(tiles(group.start, group.len));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn entry(name: &str, kind: &str, size: u64, modified: u64) -> FileEntryDto {
        FileEntryDto {
            name: name.into(),
            path: format!("C:\\x\\{name}"),
            kind: kind.into(),
            size,
            modified,
        }
    }

    fn names(entries: &[FileEntryDto]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    fn labels(groups: &[Group]) -> Vec<&str> {
        groups.iter().map(|group| group.label).collect()
    }

    #[test]
    fn kind_groups_are_contiguous_and_keep_the_sort_inside() {
        let mut entries = vec![
            entry("b.png", "file", 1, 0),
            entry("docs", "dir", 0, 0),
            entry("a.mp4", "file", 1, 0),
            entry("a.png", "file", 1, 0),
            entry("z.weird", "file", 1, 0),
        ];
        let groups = group_entries(
            &mut entries,
            GroupBy::Kind,
            &Clock::at(date!(2026 - 11 - 03)),
        );
        assert_eq!(
            names(&entries),
            ["docs", "b.png", "a.png", "a.mp4", "z.weird"]
        );
        assert_eq!(labels(&groups), ["Pastas", "Imagens", "Vídeos", "Outros"]);
        assert_eq!(
            groups[1],
            Group {
                label: "Imagens",
                start: 1,
                len: 2
            }
        );
    }

    #[test]
    fn modified_groups_follow_the_calendar() {
        // Thursday 5 November 2026, UTC; the week began on Monday the 2nd.
        let clock = Clock::at(date!(2026 - 11 - 05));
        let day = 86_400;
        let today = 1_793_836_800; // 2026-11-05 00:00 UTC
        let mut entries = vec![
            entry("old", "file", 1, 1_000_000_000),
            entry("today", "file", 1, today + 3_600),
            entry("monday", "file", 1, today - 3 * day + 10),
            entry("sunday", "file", 1, today - 4 * day),
            entry("yesterday", "file", 1, today - 1),
            entry("october", "file", 1, today - 20 * day),
            entry("lastyear", "file", 1, today - 400 * day),
        ];
        let groups = group_entries(&mut entries, GroupBy::Modified, &clock);
        assert_eq!(
            labels(&groups),
            [
                "Hoje",
                "Ontem",
                "Esta semana",
                "Este mês",
                "Este ano",
                "Ano passado",
                "Mais antigos"
            ]
        );
        assert_eq!(names(&entries)[..3], ["today", "yesterday", "monday"]);
    }

    #[test]
    fn modified_flat_sorts_newest_first_without_group_headers() {
        let mut entries = vec![
            entry("older", "file", 1, 10),
            entry("newest", "file", 1, 30),
            entry("middle", "file", 1, 20),
        ];

        let groups = group_entries(
            &mut entries,
            GroupBy::ModifiedFlat,
            &Clock::at(date!(2026 - 11 - 05)),
        );

        assert_eq!(names(&entries), ["newest", "middle", "older"]);
        assert!(groups.is_empty());
    }

    #[test]
    fn size_groups_put_folders_first_then_grow() {
        let mut entries = vec![
            entry("big", "file", 2 * 1024 * 1024 * 1024, 0),
            entry("empty", "file", 0, 0),
            entry("dir", "dir", 0, 0),
            entry("tiny", "file", 10, 0),
        ];
        let groups = group_entries(
            &mut entries,
            GroupBy::Size,
            &Clock::at(date!(2026 - 11 - 03)),
        );
        assert_eq!(names(&entries), ["dir", "empty", "tiny", "big"]);
        assert_eq!(groups.len(), 4);
    }

    #[test]
    fn rows_put_a_header_before_each_group() {
        let groups = vec![
            Group {
                label: "A",
                start: 0,
                len: 3,
            },
            Group {
                label: "B",
                start: 3,
                len: 1,
            },
        ];
        assert_eq!(
            list_rows(&groups, 4),
            [
                ListRow::Group(0),
                ListRow::Entry(0),
                ListRow::Entry(1),
                ListRow::Entry(2),
                ListRow::Group(1),
                ListRow::Entry(3),
            ]
        );
        assert_eq!(
            grid_rows(&groups, 4, 2),
            [
                GridRow::Group(0),
                GridRow::Tiles { first: 0, end: 2 },
                GridRow::Tiles { first: 2, end: 3 },
                GridRow::Group(1),
                GridRow::Tiles { first: 3, end: 4 },
            ]
        );
        assert_eq!(list_rows(&[], 2), [ListRow::Entry(0), ListRow::Entry(1)]);
        assert_eq!(grid_rows(&[], 3, 2).len(), 2);
    }
}
