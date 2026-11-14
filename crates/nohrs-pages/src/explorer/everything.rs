//! "Everything" mode: the bar under the listing searches every name on the
//! computer's fixed drives as you type, the way voidtools' Everything does.
//!
//! Everything reads the NTFS master file table, which needs administrator
//! rights; Prime Files installs per user, so it walks the drives instead and
//! keeps what it found as a flat table of names in memory. A search is then a
//! scan of that table on every core — a few tens of milliseconds for a
//! million names — and never touches the disk except to fill in size and date
//! for the hits shown.
//!
//! The table is saved to the cache folder after each walk and read back the
//! first time the mode is used, so a search answers at once while a fresh
//! walk runs behind it when the saved one is older than [`STALE_AFTER`].
//! Between walks, a watcher on each drive (`Fresh`) adds what is created or
//! renamed, so a file saved a second ago is found; one deleted is dropped
//! when the hits are read, by the size and date read for each.
//!
//! With the indexer service installed (Settings › Search › Instant
//! indexing, `nohrs_indexer`), each NTFS drive comes from its master file
//! table instead, read by the service in seconds; only the others are
//! walked.
//!
//! Each entry keeps its size and last write, so `tamanho:` and `data:` are
//! answered from the table, and a mask of its letters (`letter_mask`),
//! which turns most of a scan into one AND (both from Kensaku's design).
//!
//! Each drive is a segment of its own, published the moment its walk ends:
//! a refresh replaces one drive at a time instead of the whole. The very
//! first walk is searchable while it runs: a search reads the drive being
//! walked under its lock, which holds the walkers for the few milliseconds
//! a scan takes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gpui::{App, AppContext as _, BorrowAppContext as _, Context, Global, Subscription, Task, WeakEntity};
use nohrs_core::config::IndexSource;
use nohrs_core::telemetry::LogErr;
use nohrs_core::{t, tf};
use nohrs_services::fs::listing::FileEntryDto;

use super::ExplorerPane;
use super::quick_search::DeepSearch;

#[path = "everything/scan.rs"]
mod scan;
#[path = "everything/workers.rs"]
mod workers;

#[cfg(test)]
#[path = "everything/search_tests.rs"]
mod search_tests;

/// A saved table older than this is walked again when the mode opens. A
/// full walk of a busy system drive measured 14 minutes (2.5 million names
/// on this machine's C:, where every folder opened passes the system's file
/// filters; D: walked 88 thousand in under a second), so refreshing more
/// often would keep the disk busy for little.
/// With the watcher keeping a running app's table current, a walk is the
/// repair for what changed while it was closed.
const STALE_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
/// The same with the indexer service, whose read is a minute for a machine's
/// whole file table. It was half an hour, on the idea that a cheap repair can
/// run often; measured, "cheap" meant a minute of reading and a 1 GB file
/// written again every half hour, for a table the watcher keeps current
/// anyway. Half a day is what a table only needs to be repaired for: what
/// changed while the app was closed.
const STALE_WITH_MFT: Duration = Duration::from_secs(12 * 60 * 60);

/// How long the index stays in memory after the last pane stops searching the
/// whole computer: a table of fourteen million names is about a gigabyte and
/// a half, worth giving back on a machine that is short of memory, and worth
/// reading back from the cache (seconds) the next time the bar asks for it.
const KEEP_FOR: Duration = Duration::from_secs(10 * 60);
/// Hits handed to the listing. Past this a search is too broad to read
/// through anyway, and each hit costs a metadata read.
const MAX_RESULTS: usize = 5_000;
/// The first hits go out in a small batch so the listing fills at once.
const FIRST_BATCH: usize = 200;
const BATCH: usize = 1_000;
const RESULT_SLICE: Duration = Duration::from_millis(40);
const NO_PARENT: u32 = u32::MAX;
const FILE_MAGIC: &[u8; 8] = b"PFEVRY03";
// Before 0.3.2: no size or date. Still read, with both unknown, so an
// update searches at once instead of walking again from nothing.
const FILE_MAGIC_V2: &[u8; 8] = b"PFEVRY02";
/// The size of an entry from a table saved without sizes.
const UNKNOWN_SIZE: u64 = u64::MAX;
/// The most hits one search asks the service for: enough to order and
/// scroll, and a query that finds more is asking for a folder, not a file.
const SERVICE_HITS: u32 = 20_000;
/// Hits below this from the matching pass call in the near-miss pass.
const FUZZY_BELOW: usize = 20;
/// Hits below this from Everything let the pinyin pass run: a name typed as
/// initials ("wjj" for 文件夹) answers with few literal hits, if any.
const PINYIN_BELOW: usize = 20;
/// Near misses shown at most: past the first few, they are noise.
const FUZZY_MOST: usize = 50;
/// Entries the watcher keeps between walks; past it the oldest half goes.
const FRESH_LIMIT: usize = 50_000;
const DIR: u8 = 1;
// Under Windows, Program Files, AppData, a .git or node_modules: ranked
// after the person's own files that match as well.
const NOISE: u8 = 2;

#[derive(Clone, Copy)]
struct Entry {
    /// Bytes; `UNKNOWN_SIZE` from an old table. Folders 0.
    size: u64,
    /// `letter_mask` of the lowered name; worked out, never saved.
    mask: u64,
    parent: u32,
    name: u32,
    lower: u32,
    pinyin: u32,
    /// Last write, whole seconds since 1970; 0 when unknown.
    modified: u32,
    name_len: u16,
    lower_len: u16,
    pinyin_len: u16,
    flags: u8,
}

/// Every name on the indexed drives, a segment per drive.
pub(crate) struct Index {
    segments: Vec<Arc<Segment>>,
    /// When the walk that produced it finished.
    pub built: SystemTime,
}

impl Index {
    /// Older than [`STALE_AFTER`], or with a drive whose first walk was
    /// cut short.
    fn stale(&self) -> bool {
        // Read from the MFT, a refresh costs seconds: worth it far sooner.
        let after = if mft::running() { STALE_WITH_MFT } else { STALE_AFTER };
        self.segments.iter().any(|segment| !segment.complete) || self.built.elapsed().map_or(true, |age| age > after)
    }

    /// How many names it holds.
    pub fn len(&self) -> usize {
        self.segments.iter().map(|segment| segment.entries.len()).sum()
    }
}

/// One drive's names.
struct Segment {
    names: String,
    // The names lowercased, apart: lowercasing can change a name's length,
    // and matching against a lowered copy keeps the scan a plain `contains`.
    lower: String,
    // Tone-free full pinyin, derived while building/loading the index.
    pinyin: String,
    entries: Vec<Entry>,
    // 运行时派生，不改变缓存格式；连续扫描之外只补查这些名字的拼音。
    non_ascii: Vec<u32>,
    // Whole paths, in a table of loose entries with no parents to walk:
    // `Fresh`'s. Empty in a drive's table.
    paths: Vec<String>,
    /// The walk of this drive got to the end. A table saved in the middle
    /// of a first walk is not: searched at once on the next start, and
    /// walked again behind it.
    complete: bool,
}

impl Segment {
    /// The drive's own name ("C:"), its first entry.
    fn root(&self) -> &str {
        self.entries.first().map_or("", |entry| self.name(entry))
    }

    fn name(&self, entry: &Entry) -> &str {
        let start = entry.name as usize;
        self.names.get(start..start + entry.name_len as usize).unwrap_or("")
    }

    fn lower(&self, entry: &Entry) -> &str {
        let start = entry.lower as usize;
        self.lower.get(start..start + entry.lower_len as usize).unwrap_or("")
    }

    fn pinyin(&self, entry: &Entry) -> &str {
        let start = entry.pinyin as usize;
        self.pinyin.get(start..start + entry.pinyin_len as usize).unwrap_or("")
    }

    fn path(&self, index: u32) -> String {
        if let Some(path) = self.paths.get(index as usize) {
            return path.clone();
        }
        let mut parts = Vec::new();
        let mut current = index;
        // A broken table cannot loop forever: no path is deeper than the
        // table is long.
        while current != NO_PARENT && parts.len() <= self.entries.len() {
            let Some(entry) = self.entries.get(current as usize) else {
                break;
            };
            parts.push(self.name(entry));
            current = entry.parent;
        }
        parts.reverse();
        let mut path = parts.join("\\");
        // A drive alone is "C:\", not "C:".
        if parts.len() == 1 {
            path.push('\\');
        }
        path
    }
}

/// What was typed: terms that must all match, terms that must not, and the
/// filters (`ext:`, `tipo:`, `tamanho:`, `data:`, `em:`, `pasta:`) that
/// narrow by what the table knows of each entry. The grammar follows
/// Kensaku's (MIT, github.com/yjid/kensaku): plain typing always works, a
/// key it does not know is searched as text, and a key still being typed
/// (`tamanho:`) is left out instead of finding nothing.
#[derive(Clone)]
struct Query {
    terms: Vec<Term>,
    excluded: Vec<Term>,
    exts: Vec<String>,
    excluded_exts: Vec<String>,
    /// Folders only (`true`) or files only.
    kind: Option<bool>,
    /// Inclusive byte range.
    size: Option<(u64, u64)>,
    /// Inclusive range of last writes, seconds since 1970.
    modified: Option<(u32, u32)>,
    /// Lowered folder paths, each ending in `\`, a hit must be under.
    scopes: Vec<String>,
    // The name terms joined, to rank exact and prefix name matches.
    whole: String,
    // The first name term, to rank a match at the start of a word.
    first: Option<String>,
    // The letters every name term needs (`letter_mask`).
    mask: u64,
    /// Whether the name terms are Latin letters, so a name written in Han
    /// characters is also worth reading as its pinyin initials.
    pinyin: bool,
    needs_path: bool,
    now: u32,
    // The one plain name term a search with too few hits tries again with,
    // its letters in order but not side by side ("bnkwav" finds bonk.wav).
    fuzzy: Option<String>,
    /// The filters recognised, as typed, for the bar to show.
    filters: Vec<String>,
}

#[derive(Debug, Clone)]
enum Term {
    /// Part of the name.
    Name(String),
    /// Part of the full path (the term had a `\` or `/`).
    Path(String),
    /// A wildcard pattern over the whole name (`*.wav`).
    Pattern(Vec<char>),
}

struct Token {
    text: String,
    negated: bool,
    // Opened with a quote: a phrase, never a `key:value`.
    literal: bool,
}

// Splits on spaces outside quotes; a leading `-` negates.
fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut current: Option<Token> = None;
    let mut quoted = false;
    for character in text.chars() {
        if character.is_whitespace() && !quoted {
            if let Some(token) = current.take().filter(|token| !token.text.is_empty()) {
                tokens.push(token);
            }
            continue;
        }
        let token = current.get_or_insert_with(|| Token { text: String::new(), negated: false, literal: false });
        match character {
            '"' => {
                if token.text.is_empty() {
                    token.literal = true;
                }
                quoted = !quoted;
            }
            '-' if token.text.is_empty() && !token.negated && !token.literal => token.negated = true,
            other => token.text.push(other),
        }
    }
    tokens.extend(current.filter(|token| !token.text.is_empty()));
    tokens
}

enum Filter {
    Exts(Vec<String>),
    Kind(bool),
    Size(u64, u64),
    Modified(u32, u32),
    Scope(String),
    // `em:sfx`: under a folder whose name starts so, anywhere.
    Folder(String),
}

enum Parsed {
    Filter(Filter),
    // A known key whose value is missing or not understood yet.
    Incomplete,
    Unknown,
}

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;
const DAY: i64 = 86_400;

const IMAGES: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "avif", "svg", "ico", "psd", "raw", "cr2",
    "cr3", "nef", "arw", "dng", "exr", "tga",
];
const VIDEOS: &[&str] =
    &["mp4", "mov", "mkv", "avi", "webm", "wmv", "m4v", "flv", "mpg", "mpeg", "ts", "mts", "m2ts", "3gp", "mxf"];
const SOUNDS: &[&str] = &["mp3", "wav", "flac", "aac", "ogg", "opus", "m4a", "wma", "aif", "aiff", "mid", "midi"];
const DOCUMENTS: &[&str] =
    &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt", "md", "csv", "epub"];
const ARCHIVES: &[&str] = &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "iso", "cab", "zxp"];
const CODE: &[&str] = &[
    "rs", "js", "mjs", "ts", "tsx", "jsx", "py", "c", "h", "cpp", "hpp", "cs", "java", "go", "rb", "php", "html", "css",
    "json", "toml", "yaml", "yml", "xml", "sh", "ps1", "bat", "lua", "sql", "kt", "swift",
];
const PROGRAMS: &[&str] = &["exe", "msi", "bat", "cmd", "lnk", "appx", "msix"];
// The projects of the programs this app's people edit with.
const PROJECTS: &[&str] =
    &["prproj", "aep", "aepx", "mogrt", "psd", "ai", "blend", "fbx", "c4d", "drp", "veg", "kdenlive", "fla", "indd"];

fn category(value: &str) -> Parsed {
    let exts = match value {
        "imagem" | "imagens" | "image" | "images" | "img" | "foto" | "fotos" | "photo" | "picture" => IMAGES,
        "video" | "vídeo" | "videos" | "vídeos" => VIDEOS,
        "audio" | "áudio" | "audios" | "áudios" | "som" | "sons" | "sound" | "music" | "musica" | "música" => SOUNDS,
        "doc" | "docs" | "documento" | "documentos" | "document" | "documents" => DOCUMENTS,
        "compactado" | "compactados" | "zip" | "archive" | "archives" => ARCHIVES,
        "codigo" | "código" | "code" => CODE,
        "programa" | "programas" | "app" | "apps" | "exe" | "program" => PROGRAMS,
        "projeto" | "projetos" | "project" | "projects" => PROJECTS,
        "pasta" | "pastas" | "folder" | "folders" | "dir" => return Parsed::Filter(Filter::Kind(true)),
        "arquivo" | "arquivos" | "file" | "files" => return Parsed::Filter(Filter::Kind(false)),
        _ => return Parsed::Incomplete,
    };
    Parsed::Filter(Filter::Exts(exts.iter().map(|ext| (*ext).to_string()).collect()))
}

fn parse_filter(key: &str, value: &str, cwd: &str, now: time::OffsetDateTime) -> Parsed {
    let parsed = match key {
        "ext" | "extensão" | "extensao" => {
            let exts: Vec<String> = value
                .split([',', ';', '|'])
                .map(|ext| ext.trim().trim_start_matches('.').to_string())
                .filter(|ext| !ext.is_empty())
                .collect();
            (!exts.is_empty()).then_some(Filter::Exts(exts))
        }
        "tipo" | "type" | "is" => return category(value),
        "pasta" | "folder" | "dir" if value.is_empty() => Some(Filter::Kind(true)),
        "arquivo" | "file" if value.is_empty() => Some(Filter::Kind(false)),
        "tamanho" | "size" | "tam" => size_range(value).map(|(low, high)| Filter::Size(low, high)),
        "data" | "date" | "modificado" | "modified" | "dm" | "mod" => {
            date_range(value, now).map(|(from, to)| Filter::Modified(from, to))
        }
        "em" | "in" | "dentro" | "caminho" | "path" => scope(value, cwd),
        _ => return Parsed::Unknown,
    };
    parsed.map_or(Parsed::Incomplete, Parsed::Filter)
}

// `10mb`, `1,5gb`, `300k`: a number and a unit, 1024 a step.
fn bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|character: char| !(character.is_ascii_digit() || character == '.' || character == ','))?;
    if split == 0 {
        return None;
    }
    let (number, unit) = text.split_at(split);
    let number: f64 = number.replace(',', ".").parse().ok()?;
    let unit = match unit.trim() {
        "b" => 1,
        "k" | "kb" => KB,
        "m" | "mb" => MB,
        "g" | "gb" => GB,
        "t" | "tb" => GB * 1024,
        _ => return None,
    };
    Some((number * unit as f64) as u64)
}

fn bytes_or_plain(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok().or_else(|| bytes(text))
}

// `>10mb`, `<=1gb`, `10mb..1gb`, `vazio`, and Everything's size words; a
// size alone means at least that much.
fn size_range(value: &str) -> Option<(u64, u64)> {
    let named = match value {
        "vazio" | "empty" => Some((0, 0)),
        "minusculo" | "minúsculo" | "tiny" => Some((0, 10 * KB)),
        "pequeno" | "small" => Some((10 * KB, 100 * KB)),
        "medio" | "médio" | "medium" => Some((100 * KB, MB)),
        "grande" | "large" => Some((MB, 16 * MB)),
        "enorme" | "huge" => Some((16 * MB, 128 * MB)),
        "gigante" | "gigantic" => Some((128 * MB, u64::MAX)),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if let Some((low, high)) = value.split_once("..") {
        return Some((bytes_or_plain(low)?, bytes_or_plain(high)?));
    }
    let (operator, amount) = split_operator(value);
    let amount = bytes_or_plain(amount)?;
    Some(match operator {
        ">" => (amount.saturating_add(1), u64::MAX),
        "<" => (0, amount.saturating_sub(1)),
        "<=" => (0, amount),
        "=" => (amount, amount),
        _ => (amount, u64::MAX),
    })
}

fn split_operator(value: &str) -> (&str, &str) {
    for operator in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = value.strip_prefix(operator) {
            return (operator, rest);
        }
    }
    ("", value)
}

fn clamp_seconds(seconds: i64) -> u32 {
    seconds.clamp(0, i64::from(u32::MAX)) as u32
}

// `hoje`, `ontem`, `semana`, `<7d` (in the last 7 days), `>2a` (older than
// two years), `2026`, `2026-10`, `05/10/2026`, `>=2026-01-01`, and
// `2026-01..2026-03`. Days start at local midnight.
fn date_range(value: &str, now: time::OffsetDateTime) -> Option<(u32, u32)> {
    let now_seconds = now.unix_timestamp();
    let midnight = now.replace_time(time::Time::MIDNIGHT).unix_timestamp();
    let named = match value {
        "hoje" | "today" => Some((midnight, i64::MAX)),
        "ontem" | "yesterday" => Some((midnight - DAY, midnight - 1)),
        "semana" | "week" => Some((now_seconds - 7 * DAY, i64::MAX)),
        "mes" | "mês" | "month" => Some((now_seconds - 30 * DAY, i64::MAX)),
        "ano" | "year" => Some((now_seconds - 365 * DAY, i64::MAX)),
        _ => None,
    };
    if let Some((from, to)) = named {
        return Some((clamp_seconds(from), clamp_seconds(to)));
    }
    if let Some((from, to)) = value.split_once("..") {
        let (start, _) = period(from, now.offset())?;
        let (_, end) = period(to, now.offset())?;
        return Some((clamp_seconds(start), clamp_seconds(end)));
    }
    let (operator, rest) = split_operator(value);
    if let Some(span) = span_seconds(rest) {
        let edge = now_seconds - span;
        return Some(match operator {
            ">" | ">=" => (0, clamp_seconds(edge)),
            _ => (clamp_seconds(edge), u32::MAX),
        });
    }
    let (start, end) = period(rest, now.offset())?;
    let (from, to) = match operator {
        "<" => (0, start - 1),
        "<=" => (0, end),
        ">" => (end + 1, i64::MAX),
        ">=" => (start, i64::MAX),
        _ => (start, end),
    };
    Some((clamp_seconds(from), clamp_seconds(to)))
}

// `7d`, `2sem`, `3m`, `1a`: how far back, in seconds.
fn span_seconds(text: &str) -> Option<i64> {
    let split = text.find(|character: char| !character.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let (number, unit) = text.split_at(split);
    let number: i64 = number.parse().ok()?;
    let unit = match unit {
        "h" => 3_600,
        "d" => DAY,
        "s" | "sem" | "w" => 7 * DAY,
        "m" | "mes" | "mês" | "mo" => 30 * DAY,
        "a" | "ano" | "anos" | "y" => 365 * DAY,
        _ => return None,
    };
    number.checked_mul(unit)
}

// The first and last second of a year, a month or a day.
fn period(text: &str, offset: time::UtcOffset) -> Option<(i64, i64)> {
    let numbers: Vec<&str> = text.split(['-', '/', '.']).collect();
    let parse = |text: &str| text.parse::<i32>().ok();
    // Day first when written with slashes, as people here write dates.
    let (year, month, day) = match numbers.as_slice() {
        [year] if year.len() == 4 => (parse(year)?, None, None),
        [year, month] if year.len() == 4 => (parse(year)?, Some(parse(month)?), None),
        [month, year] if year.len() == 4 => (parse(year)?, Some(parse(month)?), None),
        [year, month, day] if year.len() == 4 => (parse(year)?, Some(parse(month)?), Some(parse(day)?)),
        [day, month, year] if year.len() == 4 => (parse(year)?, Some(parse(month)?), Some(parse(day)?)),
        _ => return None,
    };
    let start_of = |year: i32, month: i32, day: i32| -> Option<i64> {
        let month = time::Month::try_from(u8::try_from(month).ok()?).ok()?;
        let date = time::Date::from_calendar_date(year, month, u8::try_from(day).ok()?).ok()?;
        Some(date.with_time(time::Time::MIDNIGHT).assume_offset(offset).unix_timestamp())
    };
    let (start, next) = match (month, day) {
        (None, _) => (start_of(year, 1, 1)?, start_of(year + 1, 1, 1)?),
        (Some(month), None) => {
            let next = if month == 12 { start_of(year + 1, 1, 1)? } else { start_of(year, month + 1, 1)? };
            (start_of(year, month, 1)?, next)
        }
        (Some(month), Some(day)) => {
            let start = start_of(year, month, day)?;
            (start, start + DAY)
        }
    };
    Some((start, next - 1))
}

// `em:C:\Projetos` is everything under that folder; `em:.` the folder on
// screen; `em:sfx` any folder whose name starts so.
fn scope(value: &str, cwd: &str) -> Option<Filter> {
    let value = value.trim().replace('/', "\\");
    if value.is_empty() {
        return None;
    }
    let absolute = if value == "." || value == "aqui" || value == "here" {
        Some(cwd.to_lowercase())
    } else if value.as_bytes().get(1) == Some(&b':') || value.starts_with("\\\\") {
        Some(value.clone())
    } else {
        None
    };
    Some(match absolute {
        Some(mut folder) => {
            if !folder.ends_with('\\') {
                folder.push('\\');
            }
            Filter::Scope(folder)
        }
        None => Filter::Folder(format!("\\{}", value.trim_matches('\\'))),
    })
}

/// Which letters a lowered name holds, one bit each (digits and the rest
/// share the high bits). A name lacking a letter of the query cannot match,
/// and one AND says so before the name's text is read: Kensaku's
/// "char bloom", which turns most of a scan into reading 8 bytes an entry.
///
/// A Han character contributes the initial of its pinyin (`文件夹` -> `w`,
/// `j`, `j`), since that is how a name written in Chinese is typed. The mask
/// only lets through what *could* match; the sequence itself is checked
/// later, in `Query::rank`.
fn letter_mask(lower: &str) -> u64 {
    let mask = lower.chars().fold(0u64, |mask, character| {
        let bit = match character {
            'a'..='z' => character as u32 - 'a' as u32,
            '0'..='9' => 26 + (character as u32 - '0' as u32),
            other => match super::pinyin::initial(other) {
                Some(initial) => initial as u32 - 'a' as u32,
                None => 36 + other as u32 % 28,
            },
        };
        mask | 1 << bit
    });
    super::pinyin::full(lower).map_or(mask, |full| {
        full.chars().fold(mask, |mask, character| match character {
            'a'..='z' => mask | 1 << (character as u32 - 'a' as u32),
            _ => mask,
        })
    })
}

// The pinyin initials of a name written with Han characters, or `None` when
// it has none: the second way such a name is typed (`wjj` for 文件夹). Cheap
// to skip for the Latin names that are most of a table.
fn pinyin_initials(lower: &str) -> Option<String> {
    if lower.is_ascii() {
        return None;
    }
    super::pinyin::initials(lower)
}

// `term`'s letters appear in `name` in order, within twice its length:
// a typo or a dropped letter, not four letters strewn over a long name
// (unbounded, "bonk" brought in 900 sounds of the whole computer).
fn subsequence(term: &str, name: &str) -> bool {
    let wanted: Vec<char> = term.chars().collect();
    let Some(&first) = wanted.first() else {
        return false;
    };
    let letters: Vec<char> = name.chars().collect();
    let reach = wanted.len() * 2;
    letters.iter().enumerate().filter(|(_, letter)| **letter == first).any(|(start, _)| {
        let mut next = 1;
        for (at, letter) in letters.iter().enumerate().skip(start + 1) {
            if next == wanted.len() || at - start >= reach {
                break;
            }
            if wanted.get(next) == Some(letter) {
                next += 1;
            }
        }
        next == wanted.len()
    })
}

// `term` begins a word of `name`: at its start or after a separator.
fn starts_a_word(name: &str, term: &str) -> bool {
    name.match_indices(term).any(|(at, _)| {
        at == 0 || name.get(..at).and_then(|before| before.chars().next_back()).is_some_and(|before| !before.is_alphanumeric())
    })
}

// 0 for what changed this week, 1 this season, 2 earlier, 3 unknown: among
// hits matched as well, the recent first, as Kensaku weighs recency.
fn recency(modified: u32, now: u32) -> u32 {
    if modified == 0 {
        return 3;
    }
    let age = i64::from(now) - i64::from(modified);
    if age < 7 * DAY {
        0
    } else if age < 90 * DAY {
        1
    } else {
        2
    }
}

fn local_now() -> time::OffsetDateTime {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    time::OffsetDateTime::now_utc().to_offset(offset)
}

impl Query {
    fn parse(text: &str, cwd: &str) -> Option<Query> {
        Self::parse_at(text, cwd, local_now())
    }

    fn parse_at(text: &str, cwd: &str, now: time::OffsetDateTime) -> Option<Query> {
        let mut query = Query {
            terms: Vec::new(),
            excluded: Vec::new(),
            exts: Vec::new(),
            excluded_exts: Vec::new(),
            kind: None,
            size: None,
            modified: None,
            scopes: Vec::new(),
            whole: String::new(),
            first: None,
            mask: 0,
            pinyin: false,
            needs_path: false,
            now: clamp_seconds(now.unix_timestamp()),
            fuzzy: None,
            filters: Vec::new(),
        };
        let mut any = false;
        for token in tokenize(text) {
            let lower = token.text.to_lowercase();
            if !token.literal
                && let Some((key, value)) = lower.split_once(':')
            {
                match parse_filter(key, value, cwd, now) {
                    Parsed::Filter(filter) => {
                        any = true;
                        query.filters.push(format!("{}{}", if token.negated { "-" } else { "" }, token.text));
                        match (filter, token.negated) {
                            (Filter::Exts(exts), false) => query.exts.extend(exts),
                            (Filter::Exts(exts), true) => query.excluded_exts.extend(exts),
                            (Filter::Kind(dir), negated) => query.kind = Some(dir != negated),
                            (Filter::Size(low, high), _) => query.size = Some((low, high)),
                            (Filter::Modified(from, to), _) => query.modified = Some((from, to)),
                            (Filter::Scope(folder), false) => query.scopes.push(folder),
                            (Filter::Scope(folder) | Filter::Folder(folder), true) => {
                                query.excluded.push(Term::Path(folder));
                            }
                            (Filter::Folder(folder), false) => query.terms.push(Term::Path(folder)),
                        }
                        continue;
                    }
                    Parsed::Incomplete => continue,
                    Parsed::Unknown => {}
                }
            }
            any = true;
            let term = if !token.literal && (lower.contains('*') || lower.contains('?')) {
                Term::Pattern(lower.chars().collect())
            } else if !token.literal && (lower.contains('\\') || lower.contains('/')) {
                Term::Path(lower.replace('/', "\\"))
            } else {
                Term::Name(lower)
            };
            if token.negated {
                query.excluded.push(term);
            } else {
                query.terms.push(term);
            }
        }
        if !any {
            return None;
        }
        let names: Vec<&str> = query
            .terms
            .iter()
            .filter_map(|term| match term {
                Term::Name(part) => Some(part.as_str()),
                _ => None,
            })
            .collect();
        query.whole = names.join(" ");
        query.first = names.first().map(|first| (*first).to_string());
        query.mask = names.iter().fold(0, |mask, name| mask | letter_mask(name));
        // Typed in Latin letters, a name written in Han characters may be
        // meant by its pinyin initials; typed in Han characters, its own text
        // already covers it and the extra pass would be wasted.
        query.pinyin = !names.is_empty() && names.iter().all(|name| name.is_ascii());
        query.fuzzy = match query.terms.as_slice() {
            [Term::Name(only)] if only.chars().count() >= 3 && !only.contains(' ') => Some(only.clone()),
            _ => None,
        };
        query.needs_path = !query.scopes.is_empty()
            || query.terms.iter().chain(&query.excluded).any(|term| matches!(term, Term::Path(_)));
        Some(query)
    }

    /// The plain name terms, which is what the service's own scan can match:
    /// the rest of the query — extensions, sizes, dates, paths, wildcards —
    /// needs the whole path and metadata, and is applied here to its hits.
    fn name_terms(&self) -> Vec<String> {
        self.terms
            .iter()
            .filter_map(|term| match term {
                Term::Name(name) => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    /// Where a hit of the service's scan stands: `None` when one of the
    /// filters refuses it, else the same key `rank` gives its entries
    /// (match class, then system folders after the person's own, then
    /// recency), so the two ways of searching order their hits alike.
    fn score(&self, name: &str, path: &str, dir: bool, size: u64, modified: u32) -> Option<u32> {
        let name = name.to_lowercase();
        if self.kind.is_some_and(|want| want != dir) {
            return None;
        }
        if let Some((low, high)) = self.size
            && (dir || size < low || size > high)
        {
            return None;
        }
        if let Some((from, to)) = self.modified
            && (modified == 0 || modified < from || modified > to)
        {
            return None;
        }
        if !self.exts.is_empty() || !self.excluded_exts.is_empty() {
            let ext = if dir { None } else { name.rsplit_once('.').map(|(_, ext)| ext) };
            if !self.exts.is_empty() && !ext.is_some_and(|ext| self.exts.iter().any(|want| want == ext)) {
                return None;
            }
            if ext.is_some_and(|ext| self.excluded_exts.iter().any(|unwanted| unwanted == ext)) {
                return None;
            }
        }
        // A name written in Han characters is also matched by its pinyin
        // initials, as the local table matches them: Everything answers a
        // search for "wjj" with 文件夹, and that is the query this keeps.
        let initials = (self.pinyin && !name.is_ascii()).then(|| pinyin_initials(&name)).flatten();
        let names_match = self.terms.iter().all(|term| match term {
            Term::Name(part) => {
                name.contains(part.as_str())
                    || initials.as_deref().is_some_and(|initials| initials.contains(part.as_str()))
            }
            Term::Pattern(pattern) => wildcard(pattern, &name),
            Term::Path(_) => true,
        });
        if !names_match {
            return None;
        }
        if self.excluded.iter().any(|term| match term {
            Term::Name(part) => name.contains(part.as_str()),
            Term::Pattern(pattern) => wildcard(pattern, &name),
            Term::Path(_) => false,
        }) {
            return None;
        }
        if self.needs_path {
            let lowered = path.to_lowercase();
            if !self.scopes.is_empty() && !self.scopes.iter().any(|scope| lowered.starts_with(scope.as_str())) {
                return None;
            }
            if !self.terms.iter().all(|term| match term {
                Term::Path(part) => lowered.contains(part.as_str()),
                _ => true,
            }) {
                return None;
            }
            if self.excluded.iter().any(|term| match term {
                Term::Path(part) => lowered.contains(part.as_str()),
                _ => false,
            }) {
                return None;
            }
        }
        let class = if self.whole.is_empty() || name == self.whole {
            0
        } else if name.starts_with(&self.whole) {
            1
        } else if self.first.as_deref().is_some_and(|first| starts_a_word(&name, first)) {
            2
        } else {
            3
        };
        let noise = u32::from(path_is_noise(&path.to_lowercase()));
        Some(class << 8 | noise << 4 | recency(modified, self.now))
    }

    /// The sort key of `entry` if it matches: match class, then system
    /// folders after the person's own, then recency (see `recency`).
    /// `fuzzy` is the second pass, which takes only what the first refused.
    fn rank(&self, segment: &Segment, position: u32, entry: &Entry, fuzzy: bool) -> Option<u32> {
        if entry.mask & self.mask != self.mask {
            return None;
        }
        let dir = entry.flags & DIR != 0;
        if self.kind.is_some_and(|want| want != dir) {
            return None;
        }
        if let Some((low, high)) = self.size
            && (dir || entry.size == UNKNOWN_SIZE || entry.size < low || entry.size > high)
        {
            return None;
        }
        if let Some((from, to)) = self.modified
            && (entry.modified == 0 || entry.modified < from || entry.modified > to)
        {
            return None;
        }
        let name = segment.lower(entry);
        if !self.exts.is_empty() || !self.excluded_exts.is_empty() {
            let ext = if dir { None } else { name.rsplit_once('.').map(|(_, ext)| ext) };
            if !self.exts.is_empty() && !ext.is_some_and(|ext| self.exts.iter().any(|want| want == ext)) {
                return None;
            }
            if ext.is_some_and(|ext| self.excluded_exts.iter().any(|unwanted| unwanted == ext)) {
                return None;
            }
        }
        // Name terms first: building a path walks the parents, so it is done
        // only for entries every name term already let through. A name
        // written in Han characters also matches by its pinyin initials,
        // which is how it is typed on a keyboard without an IME.
        let initials = (self.pinyin && !name.is_ascii()).then(|| pinyin_initials(name)).flatten();
        let full_pinyin = (self.pinyin && !name.is_ascii()).then(|| segment.pinyin(entry));
        let names_match = self.terms.iter().all(|term| match term {
            Term::Name(part) if fuzzy => {
                !name.contains(part.as_str())
                    && (subsequence(part, name)
                        || initials.as_deref().is_some_and(|initials| subsequence(part, initials))
                        || full_pinyin.is_some_and(|full| subsequence(part, full)))
            }
            Term::Name(part) => {
                name.contains(part.as_str())
                    || initials.as_deref().is_some_and(|initials| initials.contains(part.as_str()))
                    || full_pinyin.as_deref().is_some_and(|full| full.contains(part.as_str()))
            }
            Term::Pattern(pattern) => wildcard(pattern, name),
            Term::Path(_) => true,
        });
        if !names_match {
            return None;
        }
        let excluded_by_name = self.excluded.iter().any(|term| match term {
            Term::Name(part) => name.contains(part.as_str()),
            Term::Pattern(pattern) => wildcard(pattern, name),
            Term::Path(_) => false,
        });
        if excluded_by_name {
            return None;
        }
        if self.needs_path {
            let path = segment.path(position).to_lowercase();
            if !self.scopes.is_empty() && !self.scopes.iter().any(|scope| path.starts_with(scope.as_str())) {
                return None;
            }
            let paths_match = self.terms.iter().all(|term| match term {
                Term::Path(part) => path.contains(part.as_str()),
                _ => true,
            });
            let excluded_by_path = self.excluded.iter().any(|term| match term {
                Term::Path(part) => path.contains(part.as_str()),
                _ => false,
            });
            if !paths_match || excluded_by_path {
                return None;
            }
        }
        let class = if fuzzy {
            5
        } else if self.whole.is_empty() || name == self.whole {
            0
        } else if name.starts_with(&self.whole) {
            1
        } else if self.first.as_deref().is_some_and(|first| starts_a_word(name, first)) {
            2
        } else {
            3
        };
        let noise = u32::from(entry.flags & NOISE != 0);
        Some(class << 8 | noise << 4 | recency(entry.modified, self.now))
    }
}

/// The part of `query` a hit's name is highlighted by: its longest name
/// term, without the filters, which are no part of any name.
pub(crate) fn highlight_term(query: &str) -> String {
    tokenize(query)
        .into_iter()
        .filter(|token| !token.negated && (token.literal || !token.text.contains(':')))
        .flat_map(|token| token.text.split(['*', '?', '\\', '/']).map(str::to_string).collect::<Vec<_>>())
        .max_by_key(String::len)
        .unwrap_or_default()
}

// `*` any run, `?` one character, over the whole name.
fn wildcard(pattern: &[char], name: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || name.get(n) == Some(&c) => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((star_p, star_n)) => {
                    p = star_p + 1;
                    n = star_n + 1;
                    star = Some((star_p, star_n + 1));
                }
                None => return false,
            },
        }
    }
    pattern.get(p..).is_some_and(|rest| rest.iter().all(|&c| c == '*'))
}

// ---------------------------------------------------------------------------
// Walking the drives

impl Segment {
    fn empty() -> Self {
        Self {
            names: String::new(),
            lower: String::new(),
            pinyin: String::new(),
            entries: Vec::new(),
            non_ascii: Vec::new(),
            paths: Vec::new(),
            complete: false,
        }
    }

    #[cfg(test)]
    fn push(&mut self, parent: u32, name: &str, dir: bool) -> u32 {
        self.add(parent, name, Found { dir, size: 0, modified: 0 })
    }

    fn add(&mut self, parent: u32, name: &str, found: Found) -> u32 {
        let lowered = name.to_lowercase();
        let mask = letter_mask(&lowered);
        self.add_lowered(parent, name, &lowered, mask, found)
    }

    // `add` with the lowering and the mask done beforehand: the walk does
    // them outside the lock its 32 readers share.
    fn add_lowered(&mut self, parent: u32, name: &str, lowered: &str, mask: u64, found: Found) -> u32 {
        // A name longer than this is not a real Windows name (255 UTF-16
        // units at most); clipping keeps the table's lengths in 16 bits.
        let name = clip(name, u16::MAX as usize);
        let lowered = clip(lowered, u16::MAX as usize);
        let full_pinyin = super::pinyin::full(lowered).unwrap_or_default();
        let parent_entry = self.entries.get(parent as usize).copied();
        let at_root = parent_entry.is_some_and(|entry| entry.parent == NO_PARENT);
        let noise = parent_entry.is_some_and(|entry| entry.flags & NOISE != 0) || noisy(lowered, at_root);
        let entry = Entry {
            size: if found.dir { 0 } else { found.size },
            mask,
            parent,
            name: self.names.len() as u32,
            lower: self.lower.len() as u32,
            pinyin: self.pinyin.len() as u32,
            modified: found.modified,
            name_len: name.len() as u16,
            lower_len: lowered.len() as u16,
            pinyin_len: full_pinyin.len() as u16,
            flags: if found.dir { DIR } else { 0 } | if noise { NOISE } else { 0 },
        };
        self.names.push_str(name);
        self.lower.push_str(lowered);
        self.pinyin.push_str(&full_pinyin);
        if !lowered.is_ascii() {
            self.non_ascii.push(self.entries.len() as u32);
        }
        self.entries.push(entry);
        (self.entries.len() - 1) as u32
    }

    // An entry known by its whole path, in `Fresh`'s table.
    fn add_loose(&mut self, path: String, found: Found) {
        let name = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string();
        let index = self.add(NO_PARENT, &name, found);
        if path_is_noise(&path.to_lowercase())
            && let Some(entry) = self.entries.get_mut(index as usize)
        {
            entry.flags |= NOISE;
        }
        self.paths.push(path);
    }
}

/// What a listing says of an entry.
#[derive(Clone, Copy)]
struct Found {
    dir: bool,
    size: u64,
    modified: u32,
}

impl Found {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            dir: metadata.is_dir(),
            size: metadata.len(),
            modified: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |since| clamp_seconds(since.as_secs() as i64)),
        }
    }
}

fn noisy(lowered: &str, at_root: bool) -> bool {
    matches!(lowered, "appdata" | "node_modules" | ".git" | "$recycle.bin")
        || (at_root && (matches!(lowered, "windows" | "programdata") || lowered.starts_with("program files")))
}

fn path_is_noise(lowered: &str) -> bool {
    let mut parts = lowered.split('\\');
    parts.next();
    parts.enumerate().any(|(depth, part)| noisy(part, depth == 0))
}

fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

struct Walk {
    // Folders waiting to be read, and how many workers are reading one.
    queue: Mutex<(Vec<(PathBuf, u32)>, usize)>,
    wake: Condvar,
    out: Arc<Mutex<Segment>>,
    progress: Arc<AtomicUsize>,
    list: fn(&Path, &mut Vec<Listed>),
}

/// Walks every fixed drive, one after the other: each drive's table, while
/// it fills, goes to `walking`, and each finished drive's segment (with those
/// before it) to `finished`. `progress` counts names found so far.
fn build(
    previous: Option<Arc<Index>>,
    progress: Arc<AtomicUsize>,
    mut walking: impl FnMut(Arc<Mutex<Segment>>),
    mut finished: impl FnMut(Vec<Arc<Segment>>),
) -> Index {
    let started = Instant::now();
    let mut segments: Vec<Arc<Segment>> = Vec::new();
    let path = index_file();
    let from_mft = mft::snapshot(&progress);
    for drive in platform::fixed_drives() {
        let root = drive.trim_end_matches('\\').to_string();
        if let Some(segment) = from_mft.iter().find(|segment| segment.root().eq_ignore_ascii_case(&root)) {
            segments.push(segment.clone());
            let mut kept = segments.clone();
            if let Some(previous) = &previous {
                kept.extend(
                    previous
                        .segments
                        .iter()
                        .filter(|old| !segments.iter().any(|new| new.root() == old.root()))
                        .cloned(),
                );
            }
            let all: Vec<&Segment> = kept.iter().map(|segment| &**segment).collect();
            let built = previous.as_ref().map_or(SystemTime::UNIX_EPOCH, |previous| previous.built);
            if let Err(error) = save_segments(built, &all, &path) {
                tracing::warn!("could not save the everything index so far: {error}");
            }
            finished(segments.clone());
            continue;
        }
        let table = Arc::new(Mutex::new(Segment::empty()));
        walking(table.clone());
        // What the saved file keeps besides the drive being walked: the
        // drives done in this walk and, for the rest, the previous ones.
        let others = |segments: &[Arc<Segment>]| -> Vec<Arc<Segment>> {
            let mut kept = segments.to_vec();
            if let Some(previous) = &previous {
                kept.extend(
                    previous
                        .segments
                        .iter()
                        .filter(|old| old.root() != root && !segments.iter().any(|new| new.root() == old.root()))
                        .cloned(),
                );
            }
            kept
        };
        // A drive with no whole table yet has its walk saved as it goes:
        // the first walk of a system drive takes minutes, and an app closed
        // in the middle (an update, a restart) started it over from nothing.
        let first_walk = !previous.as_ref().is_some_and(|previous| {
            previous.segments.iter().any(|old| old.root() == root && old.complete)
        });
        let kept = others(&segments);
        let mut checkpoint = |table: &Segment| {
            if !first_walk {
                return;
            }
            let mut all: Vec<&Segment> = kept.iter().map(|segment| &**segment).collect();
            all.push(table);
            if let Err(error) = save_segments(SystemTime::UNIX_EPOCH, &all, &path) {
                tracing::warn!("could not save the everything index so far: {error}");
            }
        };
        let segment = Arc::new(walk_drive(&drive, table, progress.clone(), WALKERS, platform::list, &mut checkpoint));
        tracing::info!(
            drive = %drive,
            entries = segment.entries.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "everything drive indexed"
        );
        segments.push(segment);
        // Each finished drive is saved with the rest as they were, so a
        // drive done is never walked from nothing again.
        let kept = others(&segments);
        let all: Vec<&Segment> = kept.iter().map(|segment| &**segment).collect();
        let built = previous.as_ref().map_or(SystemTime::UNIX_EPOCH, |previous| previous.built);
        if let Err(error) = save_segments(built, &all, &path) {
            tracing::warn!("could not save the everything index so far: {error}");
        }
        finished(segments.clone());
    }
    Index { segments, built: SystemTime::now() }
}

/// The indexer service (`nohrs_indexer`): asking it for the names, and
/// installing or removing it from the settings page.
pub(crate) mod mft {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use gpui::{App, AppContext as _};
    use nohrs_core::telemetry::LogErr as _;

    use super::{Everything, Found, Segment};

    /// What the settings page shows.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum State {
        Off,
        /// Installed and not running.
        Stopped,
        On,
    }

    /// The service's state, as the Service Control Manager says.
    pub(crate) fn state() -> State {
        #[cfg(windows)]
        {
            match nohrs_indexer::client::status() {
                nohrs_indexer::client::Status::Running => State::On,
                nohrs_indexer::client::Status::Stopped => State::Stopped,
                nohrs_indexer::client::Status::NotInstalled => State::Off,
            }
        }
        #[cfg(not(windows))]
        State::Off
    }

    pub(crate) fn running() -> bool {
        state() == State::On
    }

    // Builds each volume's table as the service streams it.
    #[cfg(windows)]
    struct Builder<'a> {
        done: Vec<Arc<Segment>>,
        current: Option<Segment>,
        progress: &'a AtomicUsize,
        count: usize,
    }

    #[cfg(windows)]
    impl Builder<'_> {
        fn close(&mut self) {
            if let Some(mut segment) = self.current.take() {
                segment.complete = true;
                self.done.push(Arc::new(segment));
            }
        }
    }

    #[cfg(windows)]
    impl nohrs_indexer::protocol::Sink for Builder<'_> {
        fn volume(&mut self, _root: &str) {
            self.close();
            self.current = Some(Segment::empty());
        }

        fn entry(&mut self, parent: u32, name: &str, dir: bool, size: u64, modified: u32) {
            if let Some(segment) = self.current.as_mut() {
                segment.add(parent, name, Found { dir, size, modified });
            }
            self.count += 1;
            if self.count % 4096 == 0 {
                self.progress.fetch_add(4096, Ordering::Relaxed);
            }
        }
    }

    /// Every NTFS drive's table from the service, or none when it is not
    /// installed or cannot answer (the drives are walked then).
    /// Whether the running service can search its own copy of the names
    /// (protocol 2 and later). Asked once: the answer only changes when the
    /// service is reinstalled, and the app is restarted then any way.
    pub(crate) fn searchable() -> bool {
        #[cfg(windows)]
        {
            static SEARCHABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *SEARCHABLE.get_or_init(|| match nohrs_indexer::client::hello() {
                Ok(version) => version >= nohrs_indexer::protocol::VERSION,
                Err(_) => false,
            })
        }
        #[cfg(not(windows))]
        false
    }

    /// Asks the service for the paths holding every one of `terms`. The
    /// service scans the names it keeps, so nothing of the table is loaded
    /// here: what comes back is the few thousand paths that matched.
    pub(crate) fn search(terms: &[String], limit: u32) -> Result<Vec<nohrs_indexer::protocol::Hit>, String> {
        #[cfg(windows)]
        {
            let started = Instant::now();
            let hits = nohrs_indexer::client::search(terms, limit).map_err(|error| error.to_string())?;
            tracing::debug!(
                terms = terms.len(),
                hits = hits.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "everything service search"
            );
            Ok(hits)
        }
        #[cfg(not(windows))]
        {
            let _ = (terms, limit);
            Err("the service runs on Windows only".to_string())
        }
    }

    /// Throws the service's kept names away, so its next search reads the
    /// volumes again. Nothing to do without the service.
    pub(super) fn refresh() -> Result<(), String> {
        #[cfg(windows)]
        {
            nohrs_indexer::client::refresh().map_err(|error| error.to_string())
        }
        #[cfg(not(windows))]
        {
            Err("the service runs on Windows only".to_string())
        }
    }

    pub(super) fn snapshot(progress: &AtomicUsize) -> Vec<Arc<Segment>> {
        #[cfg(windows)]
        {
            if !running() {
                return Vec::new();
            }
            let started = Instant::now();
            let mut builder = Builder { done: Vec::new(), current: None, progress, count: 0 };
            match nohrs_indexer::client::snapshot(&mut builder) {
                Ok(()) => {
                    builder.close();
                    tracing::info!(
                        drives = builder.done.len(),
                        names = builder.count,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "everything index read from the MFT"
                    );
                    builder.done
                }
                Err(error) => {
                    tracing::warn!("the indexer service did not answer; walking the drives: {error}");
                    Vec::new()
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _unused = (progress, Instant::now());
            Vec::new()
        }
    }

    /// Installs (`true`) or removes the service: Windows asks the person
    /// first. The global's `mft_busy` holds the page's buttons meanwhile, and
    /// `mft_error` says what went wrong.
    pub(crate) fn set(enable: bool, cx: &mut App) {
        let state = cx.default_global::<Everything>();
        if state.mft_busy {
            return;
        }
        state.mft_busy = true;
        state.mft_error = None;
        let work = cx.background_spawn(async move { change(enable) });
        cx.spawn(async move |cx| {
            let outcome = work.await;
            cx.update(|cx| {
                let state = cx.global_mut::<Everything>();
                state.mft_busy = false;
                state.mft_error = outcome.err();
                state.generation += 1;
                // The settings page reads the state as it draws.
                cx.refresh_windows();
                // On: read the tables now instead of at the next refresh.
                if enable && running() {
                    super::rebuild(cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    #[cfg(windows)]
    fn change(enable: bool) -> Result<(), String> {
        use nohrs_indexer::client;
        let report = std::env::temp_dir().join(format!("prime-files-indexer-{}.txt", std::process::id()));
        if let Err(error) = std::fs::remove_file(&report)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error.to_string());
        }
        let arguments = if enable {
            format!("--install --allow {}", client::current_user_sid().map_err(|error| error.to_string())?)
        } else {
            "--uninstall".to_string()
        };
        let outcome = client::run_elevated(&arguments, &report).map_err(|error| error.to_string())?;
        match outcome {
            // Declined at the prompt: nothing changed, nothing to report.
            None => Ok(()),
            Some((0, _)) => Ok(()),
            Some((code, message)) if message.is_empty() => Err(format!("code {code}")),
            Some((_, message)) => Err(message),
        }
    }

    #[cfg(not(windows))]
    fn change(_enable: bool) -> Result<(), String> {
        Err("Windows only".to_string())
    }
}

/// The index of an installed Everything (voidtools), asked instead of walked.
///
/// Everything already keeps every name of the drives — its own index, built
/// from the NTFS journal — so for the people who run it there is nothing to
/// walk or keep here: a query costs one message to that program and comes back
/// with the hits. Two ways to reach it, tried in this order:
///
/// * The official SDK (`Everything64.dll` for this architecture, shipped in
///   the program's `dll` folder). Its calls are only made when the setting
///   asks for them.
/// * Everything's own IPC window (`WM_COPYDATA`), the protocol under that SDK.
///   The SDK looks for the default instance's window class only, and a named
///   instance (`Everything.exe -instance 1.5a`) registers a suffixed class, so
///   the names are asked for directly there.
///
/// Either way the work runs on background threads: a query waits on that
/// program's window, and the UI thread never does.
pub(crate) mod remote {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// One name Everything knows.
    pub(crate) struct Hit {
        pub path: String,
        pub dir: bool,
        /// Bytes; 0 for a folder or when Everything did not say.
        pub size: u64,
        /// Last write, whole seconds since 1970; 0 when unknown.
        pub modified: u32,
    }

    /// What the settings page says about Everything's index.
    #[derive(Clone)]
    pub(crate) struct Info {
        pub entries: u64,
        pub version: String,
        /// The modifier that makes a query match pinyin initials, when this
        /// Everything has one (1.5, build 1320 and later); `None` otherwise.
        pub pinyin: Option<&'static str>,
        /// The modifier that matches full pinyin on Everything 1.5 build 1387+.
        pub full_pinyin: Option<&'static str>,
    }

    /// Why Everything's index could not answer.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Error {
        /// No SDK DLL next to the program.
        Missing,
        /// Everything is not running, or runs elevated where this process's
        /// messages cannot reach it.
        NotRunning,
        /// Running, but its index is still being read.
        NotReady,
        /// One query is holding it, and did not answer in time: the program
        /// itself may be stuck.
        Busy,
        /// Everything runs under a named instance, which the SDK does not
        /// find: it looks for the default one only.
        NamedInstance(String),
        /// Anything else, as the SDK's own error number.
        Code(u32),
    }

    /// Everything's own index, as its SDK or its window answers.
    #[cfg(windows)]
    pub(crate) fn probe() -> Result<Info, Error> {
        match dll::probe() {
            Ok(info) => Ok(info),
            Err(dll_error) => match ipc::probe() {
                Ok(info) => Ok(info),
                // A window was there but did not answer: its reason is the one
                // to show. No window at all leaves the SDK's reason.
                Err(ipc::Failure::Failed(error)) => Err(error),
                Err(ipc::Failure::Unavailable) => Err(dll_error),
            },
        }
    }

    /// The names holding every one of `terms`, up to `limit` of them.
    ///
    /// A name written in Han characters is also matched by its pinyin initials
    /// ("wjj" for 文件夹), and Everything only does that when it is told to —
    /// or not at all, depending on its version. So when a search finds almost
    /// nothing, it is asked again with the pinyin modifier this Everything
    /// understands; the hits of both are merged.
    #[cfg(windows)]
    pub(crate) fn search(
        terms: &[String],
        limit: u32,
        cancel: &Arc<AtomicBool>,
        pinyin: Option<&'static str>,
        full_pinyin: Option<&'static str>,
    ) -> Result<Vec<Hit>, Error> {
        let mut hits = search_via(terms, limit, cancel, None)?;
        let try_pinyin = hits.len() < super::PINYIN_BELOW;
        for modifier in [pinyin, full_pinyin].into_iter().flatten() {
            if !try_pinyin || cancel.load(Ordering::Relaxed) {
                break;
            }
            match search_via(terms, limit, cancel, Some(modifier)) {
                Ok(more) => hits.extend(more),
                Err(error) => tracing::debug!("Everything could not search by pinyin: {error:?}"),
            }
        }
        Ok(hits)
    }

    #[cfg(windows)]
    fn search_via(
        terms: &[String],
        limit: u32,
        cancel: &Arc<AtomicBool>,
        modifier: Option<&str>,
    ) -> Result<Vec<Hit>, Error> {
        match dll::search(terms, limit, cancel.as_ref(), modifier) {
            Ok(hits) => Ok(hits),
            Err(dll_error) => match ipc::search(terms, limit, cancel, modifier) {
                Ok(hits) => Ok(hits),
                Err(ipc::Failure::Failed(error)) => Err(error),
                Err(ipc::Failure::Unavailable) => Err(dll_error),
            },
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn probe() -> Result<Info, Error> {
        Err(Error::Missing)
    }

    #[cfg(not(windows))]
    pub(crate) fn search(
        _terms: &[String],
        _limit: u32,
        _cancel: &Arc<AtomicBool>,
        _pinyin: Option<&'static str>,
        _full_pinyin: Option<&'static str>,
    ) -> Result<Vec<Hit>, Error> {
        Err(Error::Missing)
    }

    /// The search modifier that makes a term match pinyin initials, when this
    /// Everything understands one: `pinyin:` up to 1.5 build 1386 (single
    /// character initials were the only method then), `pinyin-jian:` from
    /// build 1387 (where plain `pinyin:` became QuanPin). Everything 1.4 has
    /// no pinyin matching, and builds 1384–1386 default to QuanPin, so they
    /// are left to whatever the person configured there.
    #[cfg(windows)]
    pub(super) fn pinyin_modifier() -> Option<&'static str> {
        let (major, minor, _, build) = ipc::version()?;
        if major != 1 || minor < 5 {
            return None;
        }
        match build {
            1387.. => Some("pinyin-jian:"),
            1320..=1383 => Some("pinyin:"),
            _ => None,
        }
    }

    #[cfg(windows)]
    pub(super) fn full_pinyin_modifier() -> Option<&'static str> {
        let (major, minor, _, build) = ipc::version()?;
        (major == 1 && minor >= 5 && build >= 1387).then_some("pinyin:")
    }

    /// The search Everything is asked for plain name terms: each in quotes,
    /// which keeps its own operators (a `|`, a `!`, a `:` and its wildcards)
    /// out of the way, joined by spaces so a hit must hold all of them — what
    /// the local scan does. `modifier`, when given, is put in front of each
    /// term so it matches pinyin initials too. A `"` cannot appear in a
    /// Windows name, and the tokenizer already drops the ones typed.
    /// Terminated for the wide APIs.
    #[cfg(windows)]
    pub(super) fn search_text(terms: &[String], modifier: Option<&str>) -> Vec<u16> {
        let mut text = String::new();
        for term in terms {
            if !text.is_empty() {
                text.push(' ');
            }
            if let Some(modifier) = modifier {
                text.push_str(modifier);
            }
            text.push('"');
            text.extend(term.chars().filter(|character| *character != '"'));
            text.push('"');
        }
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// FILETIME (100 ns from 1601) to whole seconds since 1970.
    #[cfg(windows)]
    pub(super) fn filetime_seconds(ticks: u64) -> u32 {
        // From 1601 to 1970, in the same ticks.
        const EPOCH_TICKS: u64 = 116_444_736_000_000_000;
        if ticks == 0 {
            return 0;
        }
        let seconds = ticks.saturating_sub(EPOCH_TICKS) / 10_000_000;
        super::clamp_seconds(i64::try_from(seconds).unwrap_or(i64::from(u32::MAX)))
    }

    #[cfg(windows)]
    #[allow(unsafe_code)]
    mod discover {
        use windows::Win32::Foundation::{HWND, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, FindWindowW, GetClassNameW};
        use windows::core::{BOOL, PCWSTR};

        /// Everything's IPC window class, unsuffixed for the default instance.
        const CLASS: &str = "EVERYTHING_TASKBAR_NOTIFICATION";

        fn wide(text: &str) -> Vec<u16> {
            text.encode_utf16().chain(std::iter::once(0)).collect()
        }

        /// Everything's IPC window: the default instance's, else any named
        /// one's (`EVERYTHING_TASKBAR_NOTIFICATION_(name)`).
        pub(super) fn window() -> Option<HWND> {
            let class = wide(CLASS);
            // SAFETY: the class name is a terminated wide string that lives
            // for the call; no window title is asked for.
            if let Ok(window) = unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) } {
                return Some(window);
            }
            let mut found: Option<HWND> = None;
            let param = LPARAM(std::ptr::from_mut(&mut found).cast::<core::ffi::c_void>() as isize);
            // SAFETY: the callback only writes through that pointer, and the
            // enumeration finishes before `found` goes away.
            let _ = unsafe { EnumWindows(Some(visit), param) };
            found
        }

        /// The name of a running Everything instance the SDK cannot reach: a
        /// named one registers `EVERYTHING_TASKBAR_NOTIFICATION_(name)`, while
        /// the SDK looks for the unsuffixed default class only.
        pub(super) fn named_instance() -> Option<String> {
            let mut found: Option<String> = None;
            let param = LPARAM(std::ptr::from_mut(&mut found).cast::<core::ffi::c_void>() as isize);
            // SAFETY: as in `window`.
            let _ = unsafe { EnumWindows(Some(visit_named), param) };
            found
        }

        unsafe extern "system" fn visit(window: HWND, param: LPARAM) -> BOOL {
            // SAFETY: `param` is the `Option<HWND>` the caller keeps on its
            // stack for the whole (synchronous) enumeration.
            let found = unsafe { &mut *(param.0 as *mut Option<HWND>) };
            if class_of(window).starts_with(CLASS) {
                *found = Some(window);
                return BOOL::from(false);
            }
            BOOL::from(true)
        }

        unsafe extern "system" fn visit_named(window: HWND, param: LPARAM) -> BOOL {
            // SAFETY: as in `visit`, for an `Option<String>`.
            let found = unsafe { &mut *(param.0 as *mut Option<String>) };
            if let Some(name) = class_of(window)
                .strip_prefix("EVERYTHING_TASKBAR_NOTIFICATION_(")
                .and_then(|rest| rest.strip_suffix(')'))
            {
                *found = Some(name.to_string());
                return BOOL::from(false);
            }
            BOOL::from(true)
        }

        fn class_of(window: HWND) -> String {
            let mut class = [0u16; 256];
            // SAFETY: `class` is the buffer the call fills, and its length is
            // what is handed over.
            let length = unsafe { GetClassNameW(window, &mut class) };
            let Ok(length) = usize::try_from(length) else {
                return String::new();
            };
            String::from_utf16_lossy(&class[..length.min(class.len())])
        }
    }

    /// Everything's own IPC: the message protocol the SDK is built on. The SDK
    /// only finds the default instance, so a named one is asked directly:
    /// `WM_COPYDATA` with a QUERY2W to its notification window, and the reply
    /// (a LIST2 buffer) arriving at a window of our own.
    ///
    /// The reply comes asynchronously, so one thread owns that window and
    /// pumps its messages; the searching threads hand it a query and wait for
    /// the answer.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    mod ipc {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
        use std::sync::{Arc, Mutex, OnceLock, PoisonError};
        use std::time::{Duration, Instant};

        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::DataExchange::COPYDATASTRUCT;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, MSG,
            MSGFLT_ALLOW, PM_REMOVE, PeekMessageW, RegisterClassW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
            SendMessageTimeoutW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COPYDATA,
            WNDCLASSW,
        };
        use windows::core::PCWSTR;

        use nohrs_core::telemetry::LogErr as _;

        use super::{Error, Hit, Info, discover, filetime_seconds, search_text};

        /// Everything's `WM_COPYDATA` ids: the query, and the flags the reply
        /// is parsed by.
        const QUERY2W: usize = 18;
        const SORT_NAME_ASCENDING: u32 = 1;
        /// Full path, size and last write: the same data the SDK is asked for.
        const REQUEST: u32 = 0x4 | 0x10 | 0x40;
        // Request flag bits, in the order the reply lays their data out.
        const REQUEST_NAME: u32 = 0x1;
        const REQUEST_PATH: u32 = 0x2;
        const REQUEST_FULL_PATH: u32 = 0x4;
        const REQUEST_EXTENSION: u32 = 0x8;
        const REQUEST_SIZE: u32 = 0x10;
        const REQUEST_DATE_CREATED: u32 = 0x20;
        const REQUEST_DATE_MODIFIED: u32 = 0x40;
        const REQUEST_DATE_ACCESSED: u32 = 0x80;
        const REQUEST_ATTRIBUTES: u32 = 0x100;
        /// The class of our own hidden window that receives the replies.
        const REPLY_CLASS: &str = "PrimeFilesEverythingReply";
        /// The most a query waits for its answer: measured at tens of
        /// milliseconds, and past this the program is stuck.
        const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
        /// How long Everything may take to accept the query itself.
        const SEND_TIMEOUT_MS: u32 = 1_000;

        /// Why the IPC route could not answer.
        pub(super) enum Failure {
            /// No Everything window at all.
            Unavailable,
            /// One was there, but the answer failed.
            Failed(Error),
        }

        struct Job {
            search: Vec<u16>,
            max: u32,
            /// The caller's cancel flag: a newer keystroke makes this query
            /// pointless while it waits its turn.
            cancel: Option<Arc<AtomicBool>>,
            reply: Sender<Answer>,
        }

        enum Answer {
            /// Everything's reply buffer, empty when it did not answer.
            Done(Vec<u8>),
            /// The caller gave up on the query before it went out.
            Cancelled,
        }

        /// The reply the window proc took: only valid inside the message, so
        /// it is copied, and matched to the query by its cookie.
        static REPLY: Mutex<Option<(u32, Vec<u8>)>> = Mutex::new(None);
        /// The cookie of the query in flight.
        static COOKIE: AtomicU32 = AtomicU32::new(0);
        static NEXT_COOKIE: AtomicU32 = AtomicU32::new(0x5046_0001);
        static CLIENT: OnceLock<Option<Sender<Job>>> = OnceLock::new();

        fn client() -> Option<&'static Sender<Job>> {
            CLIENT.get_or_init(start).as_ref()
        }

        /// The reply window and its message pump, on a thread of their own.
        fn start() -> Option<Sender<Job>> {
            let (sender, jobs) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("nohrs-everything-ipc".into())
                .spawn(move || pump(jobs));
            match spawned {
                Ok(_) => Some(sender),
                Err(error) => {
                    tracing::warn!("could not start the Everything IPC thread: {error}");
                    None
                }
            }
        }

        fn pump(jobs: Receiver<Job>) {
            let Some(window) = reply_window() else {
                return;
            };
            // Everything runs in another process and may sit at another
            // integrity level; its reply is a WM_COPYDATA to this window.
            // SAFETY: our own window, and the call only widens its filter.
            unsafe { ChangeWindowMessageFilterEx(window, WM_COPYDATA, MSGFLT_ALLOW, None).log_err() };
            while let Ok(job) = jobs.recv() {
                // The caller typed on: this query would only keep the queue
                // busy, and its answer is already unwanted.
                if job.cancel.as_ref().is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
                    job.reply.send(Answer::Cancelled).log_err();
                    continue;
                }
                let reply = exchange(window, &job);
                job.reply.send(Answer::Done(reply)).log_err();
            }
        }

        /// Sends one query to Everything and pumps our queue until its reply
        /// is copied here, or the deadline passes.
        fn exchange(window: HWND, job: &Job) -> Vec<u8> {
            let Some(target) = discover::window() else {
                return Vec::new();
            };
            let cookie = NEXT_COOKIE.fetch_add(1, Ordering::Relaxed);
            COOKIE.store(cookie, Ordering::Relaxed);
            *REPLY.lock().unwrap_or_else(PoisonError::into_inner) = None;
            let query = query_bytes(window, cookie, job);
            let data = COPYDATASTRUCT {
                dwData: QUERY2W,
                cbData: query.len() as u32,
                lpData: query.as_ptr().cast_mut().cast(),
            };
            // SAFETY: the structure and the search string it points at outlive
            // this synchronous send, whose target is Everything's own window.
            let accepted = unsafe {
                SendMessageTimeoutW(
                    target,
                    WM_COPYDATA,
                    WPARAM(window.0 as usize),
                    LPARAM((&raw const data) as isize),
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    SEND_TIMEOUT_MS,
                    None,
                )
            };
            if accepted.0 == 0 {
                return Vec::new();
            }
            // The answer comes separately, to the window above: pump for it.
            let deadline = Instant::now() + REPLY_TIMEOUT;
            loop {
                if let Some((reply_cookie, bytes)) =
                    REPLY.lock().unwrap_or_else(PoisonError::into_inner).take()
                    && reply_cookie == cookie
                {
                    return bytes;
                }
                if Instant::now() >= deadline {
                    return Vec::new();
                }
                pump_messages();
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn pump_messages() {
            let mut message = MSG::default();
            // SAFETY: the message is a plain buffer the call fills, and each
            // message goes to the window proc that asked for it.
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                // SAFETY: pumping the message just retrieved.
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }

        /// Everything's query buffer (its SDK's QUERY2W): seven 32-bit fields
        /// and the NUL-terminated search, with no padding at all.
        fn query_bytes(window: HWND, cookie: u32, job: &Job) -> Vec<u8> {
            let header = [
                // Everything's header documents that only 32 bits of the
                // window handle are used.
                window.0 as usize as u32,
                cookie,
                0, // search flags: case-insensitive, file names only, no regex
                0, // offset
                job.max,
                REQUEST,
                SORT_NAME_ASCENDING,
            ];
            let mut query = Vec::with_capacity(28 + job.search.len() * 2);
            for value in header {
                query.extend_from_slice(&value.to_le_bytes());
            }
            for unit in &job.search {
                query.extend_from_slice(&unit.to_le_bytes());
            }
            query
        }

        /// The hidden window that takes the replies.
        fn reply_window() -> Option<HWND> {
            let instance = match unsafe { GetModuleHandleW(None) } {
                Ok(module) => HINSTANCE(module.0),
                Err(error) => {
                    tracing::warn!("could not find this program's module: {error}");
                    return None;
                }
            };
            let class = wide(REPLY_CLASS);
            let window_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            // SAFETY: the class names a proc that lives as long as the binary,
            // and the strings it refers to outlive the call.
            if unsafe { RegisterClassW(&window_class) } == 0 {
                let error = std::io::Error::last_os_error();
                tracing::warn!("could not register the Everything IPC window: {error}");
                return None;
            }
            let title = wide("Prime Files");
            // SAFETY: the class is registered above, the instance is this
            // process's, and the window is never shown.
            match unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    PCWSTR(class.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    WINDOW_STYLE::default(),
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(instance),
                    None,
                )
            } {
                Ok(window) => Some(window),
                Err(error) => {
                    tracing::warn!("could not create the Everything IPC window: {error}");
                    None
                }
            }
        }

        unsafe extern "system" fn window_proc(
            window: HWND,
            message: u32,
            _wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if message == WM_COPYDATA {
                // SAFETY: WM_COPYDATA always carries a COPYDATASTRUCT in
                // `lparam`, valid until this proc returns.
                let data = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
                let cookie = data.dwData as u32;
                if cookie == COOKIE.load(Ordering::Relaxed) {
                    let length = data.cbData as usize;
                    let bytes = if data.lpData.is_null() || length == 0 {
                        Vec::new()
                    } else {
                        // SAFETY: the sender promises `cbData` bytes there, and
                        // the copy is made before the message goes away.
                        unsafe { std::slice::from_raw_parts(data.lpData.cast::<u8>(), length) }.to_vec()
                    };
                    *REPLY.lock().unwrap_or_else(PoisonError::into_inner) = Some((cookie, bytes));
                }
                return LRESULT(1);
            }
            // SAFETY: default handling for every other message.
            unsafe { DefWindowProcW(window, message, _wparam, lparam) }
        }

        fn wide(text: &str) -> Vec<u16> {
            text.encode_utf16().chain(std::iter::once(0)).collect()
        }

        /// Everything's version, asked over its window: major, minor, revision
        /// and build. `None` when the window does not answer.
        pub(super) fn version() -> Option<(u32, u32, u32, u32)> {
            use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_USER};

            let target = discover::window()?;
            let mut numbers = [0u32; 4];
            for (at, code) in [0usize, 1, 2, 3].into_iter().enumerate() {
                // SAFETY: the read-only version messages Everything's window
                // answers, with no data exchanged either way.
                let value =
                    unsafe { SendMessageW(target, WM_USER, Some(WPARAM(code)), Some(LPARAM(0))) };
                numbers[at] = u32::try_from(value.0).ok()?;
            }
            Some((numbers[0], numbers[1], numbers[2], numbers[3]))
        }

        pub(super) fn probe() -> Result<Info, Failure> {
            if discover::window().is_none() {
                return Err(Failure::Unavailable);
            }
            // An empty search matches every name, one of which comes back; the
            // count of all of them is what the page shows.
            let reply = ask(search_text(&[], None), 1, None)?;
            let list = list(&reply).ok_or(Failure::Failed(Error::Busy))?;
            if list.totitems == 0 {
                return Err(Failure::Failed(Error::NotReady));
            }
            let version = match discover::named_instance() {
                Some(name) => format!("ipc {name}"),
                None => "ipc".to_string(),
            };
            Ok(Info {
                entries: u64::from(list.totitems),
                version,
                pinyin: super::pinyin_modifier(),
                full_pinyin: super::full_pinyin_modifier(),
            })
        }

        pub(super) fn search(
            terms: &[String],
            limit: u32,
            cancel: &Arc<AtomicBool>,
            modifier: Option<&str>,
        ) -> Result<Vec<Hit>, Failure> {
            if cancel.load(Ordering::Relaxed) {
                return Ok(Vec::new());
            }
            if discover::window().is_none() {
                return Err(Failure::Unavailable);
            }
            let reply = ask(search_text(terms, modifier), limit, Some(cancel.clone()))?;
            let list = list(&reply).ok_or(Failure::Failed(Error::Busy))?;
            let count = list.items.len() / 8;
            let mut hits = Vec::with_capacity(count.min(1024));
            for index in 0..count {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(index) = u32::try_from(index)
                    && let Some(hit) = list.hit(index)
                {
                    hits.push(hit);
                }
            }
            Ok(hits)
        }

        fn ask(search: Vec<u16>, max: u32, cancel: Option<Arc<AtomicBool>>) -> Result<Vec<u8>, Failure> {
            let sender = client().ok_or(Failure::Unavailable)?;
            let (reply, answer) = mpsc::channel();
            sender.send(Job { search, max, cancel, reply }).map_err(|_| Failure::Unavailable)?;
            // The pump thread has its own deadline; this one also covers a
            // thread that is stuck before it can answer.
            match answer.recv_timeout(REPLY_TIMEOUT + Duration::from_secs(2)) {
                Ok(Answer::Done(bytes)) if !bytes.is_empty() => Ok(bytes),
                // The caller moved on: no hits, and no error to show.
                Ok(Answer::Cancelled) => Ok(Vec::new()),
                Ok(Answer::Done(_)) | Err(RecvTimeoutError::Timeout) => Err(Failure::Failed(Error::Busy)),
                Err(RecvTimeoutError::Disconnected) => Err(Failure::Unavailable),
            }
        }

        /// Everything's LIST2 reply: a 20-byte header and 8-byte item entries
        /// whose `data_offset` points into this same buffer.
        struct List<'a> {
            totitems: u32,
            request_flags: u32,
            items: &'a [u8],
            buffer: &'a [u8],
        }

        fn list(reply: &[u8]) -> Option<List<'_>> {
            let totitems = word(reply, 0)?;
            let numitems = word(reply, 4)?;
            // The reply's own flags decide how its data is laid out.
            let request_flags = word(reply, 12)?;
            let end = 20usize.checked_add(usize::try_from(numitems).ok()?.checked_mul(8)?)?;
            Some(List { totitems, request_flags, items: reply.get(20..end)?, buffer: reply })
        }

        impl List<'_> {
            fn hit(&self, index: u32) -> Option<Hit> {
                let at = usize::try_from(index).ok()?.checked_mul(8)?;
                let flags = word(self.items, at)?;
                let mut cursor = usize::try_from(word(self.items, at + 4)?).ok()?;
                let mut name = String::new();
                if self.request_flags & REQUEST_NAME != 0 {
                    name = text(self.buffer, &mut cursor)?;
                }
                let mut path = String::new();
                if self.request_flags & REQUEST_PATH != 0 {
                    path = text(self.buffer, &mut cursor)?;
                }
                let mut full = String::new();
                if self.request_flags & REQUEST_FULL_PATH != 0 {
                    full = text(self.buffer, &mut cursor)?;
                }
                if self.request_flags & REQUEST_EXTENSION != 0 {
                    take(self.buffer, &mut cursor)?;
                }
                let mut size = 0u64;
                if self.request_flags & REQUEST_SIZE != 0 {
                    size = number(self.buffer, &mut cursor)?;
                }
                if self.request_flags & REQUEST_DATE_CREATED != 0 {
                    take(self.buffer, &mut cursor)?;
                }
                let mut modified = 0u32;
                if self.request_flags & REQUEST_DATE_MODIFIED != 0 {
                    modified = filetime_seconds(number(self.buffer, &mut cursor)?);
                }
                if self.request_flags & REQUEST_DATE_ACCESSED != 0 {
                    take(self.buffer, &mut cursor)?;
                }
                if self.request_flags & REQUEST_ATTRIBUTES != 0 {
                    // Attributes come last among the fields a search asks for,
                    // so there is nothing after them to stay aligned for.
                    let _ = word(self.buffer, cursor);
                }
                // 0x1 a folder, 0x2 a drive: neither has a size to show.
                let dir = flags & 0b11 != 0;
                if dir {
                    size = 0;
                }
                let path = if !full.is_empty() {
                    full
                } else if !path.is_empty() && !name.is_empty() {
                    format!("{path}\\{name}")
                } else {
                    name
                };
                (!path.is_empty()).then_some(Hit { path, dir, size, modified })
            }
        }

        fn word(buffer: &[u8], at: usize) -> Option<u32> {
            Some(u32::from_le_bytes(buffer.get(at..at.checked_add(4)?)?.try_into().ok()?))
        }

        /// Skips 8 bytes of a field this search does not use.
        fn take(buffer: &[u8], cursor: &mut usize) -> Option<()> {
            number(buffer, cursor).map(|_| ())
        }

        fn number(buffer: &[u8], cursor: &mut usize) -> Option<u64> {
            let at = *cursor;
            let value = u64::from_le_bytes(buffer.get(at..at.checked_add(8)?)?.try_into().ok()?);
            *cursor = at + 8;
            Some(value)
        }

        /// A string field: a 32-bit count of UTF-16 units without the
        /// terminator, then that many units and the terminator, with no
        /// padding before the next field.
        fn text(buffer: &[u8], cursor: &mut usize) -> Option<String> {
            let count = usize::try_from(word(buffer, *cursor)?).ok()?;
            let start = cursor.checked_add(4)?;
            let end = start.checked_add(count.checked_add(1)?.checked_mul(2)?)?;
            let data = buffer.get(start..end)?;
            let units: Vec<u16> =
                data.chunks_exact(2).take(count).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
            *cursor = end;
            Some(String::from_utf16_lossy(&units))
        }
    }

    #[cfg(windows)]
    #[allow(unsafe_code)]
    mod dll {
        use std::ffi::{CStr, OsString};
        use std::os::windows::ffi::OsStrExt as _;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
        use std::time::{Duration, Instant};

        use windows::Win32::Foundation::{FILETIME, HMODULE};
        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        use windows::core::{BOOL, PCSTR, PCWSTR};

        use super::{Error, Hit, Info, discover, filetime_seconds, search_text};

        /// Everything's SDK has a single search in the DLL — the string, the
        /// request flags and the results — so no two calls may overlap. A
        /// query waits on Everything's own window with no timeout, so the wait
        /// for the turn gives up instead of piling a thread per keystroke on a
        /// stuck program.
        static SDK: Gate = Gate::new();
        /// The most a call waits for the one in front; queries measured in the
        /// tens of milliseconds, so this is only reached by a stuck program.
        const GATE_TIMEOUT: Duration = Duration::from_secs(2);
        /// After a wait gave up, the SDK is left alone this long: every
        /// keystroke would wait again, and the stuck call may still hold it.
        const STALLED_FOR: Duration = Duration::from_secs(30);

        struct Gate {
            busy: Mutex<bool>,
            free: Condvar,
            /// When the last wait gave up on the call in front.
            stalled: Mutex<Option<Instant>>,
        }

        impl Gate {
            const fn new() -> Self {
                Self { busy: Mutex::new(false), free: Condvar::new(), stalled: Mutex::new(None) }
            }

            fn enter(&self) -> Option<Session<'_>> {
                let mut stalled = self.stalled.lock().unwrap_or_else(PoisonError::into_inner);
                if stalled.is_some_and(|when| when.elapsed() < STALLED_FOR) {
                    return None;
                }
                let mut busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
                let deadline = Instant::now() + GATE_TIMEOUT;
                while *busy {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        *stalled = Some(Instant::now());
                        return None;
                    }
                    let (awaited, _) = self.free.wait_timeout(busy, left).unwrap_or_else(PoisonError::into_inner);
                    busy = awaited;
                }
                *stalled = None;
                drop(stalled);
                *busy = true;
                Some(Session { gate: self })
            }
        }

        struct Session<'a> {
            gate: &'a Gate,
        }

        impl Drop for Session<'_> {
            fn drop(&mut self) {
                *self.gate.busy.lock().unwrap_or_else(PoisonError::into_inner) = false;
                self.gate.free.notify_one();
            }
        }

        /// The first path read; a path that fills it is asked for again with
        /// room for the longest one Windows allows.
        const PATH_UNITS: usize = 1024;
        const LONG_PATH_UNITS: usize = 32_768;
        // EVERYTHING_REQUEST_*, from Everything.h.
        const REQUEST_FILE_NAME: u32 = 0x0000_0001;
        const REQUEST_PATH: u32 = 0x0000_0002;
        const REQUEST_FULL_PATH: u32 = 0x0000_0004;
        const REQUEST_SIZE: u32 = 0x0000_0010;
        const REQUEST_DATE_MODIFIED: u32 = 0x0000_0040;
        // EVERYTHING_ERROR_IPC: no Everything client to talk to.
        const ERROR_IPC: u32 = 2;

        type SetSearchW = unsafe extern "system" fn(PCWSTR);
        type QueryW = unsafe extern "system" fn(BOOL) -> BOOL;
        type NumResults = unsafe extern "system" fn() -> u32;
        type TotResults = unsafe extern "system" fn() -> u32;
        type IsFolder = unsafe extern "system" fn(u32) -> BOOL;
        type FullPath = unsafe extern "system" fn(u32, *mut u16, u32) -> u32;
        type GetSize = unsafe extern "system" fn(u32, *mut i64) -> BOOL;
        type GetDate = unsafe extern "system" fn(u32, *mut FILETIME) -> BOOL;
        type GetLastError = unsafe extern "system" fn() -> u32;
        type SetNumber = unsafe extern "system" fn(u32);
        type SetFlag = unsafe extern "system" fn(BOOL);
        type Reset = unsafe extern "system" fn();
        type IsDbLoaded = unsafe extern "system" fn() -> BOOL;
        type Version = unsafe extern "system" fn() -> u32;

        /// The calls this DLL exports. The core ones are required — every SDK
        /// since 1.3 has them — and the rest are used only when present, so a
        /// different SDK build still works for what it can answer.
        struct Api {
            set_search: SetSearchW,
            query: QueryW,
            num_results: NumResults,
            tot_results: Option<TotResults>,
            is_folder: IsFolder,
            full_path: FullPath,
            size: Option<GetSize>,
            date_modified: Option<GetDate>,
            last_error: GetLastError,
            set_max: SetNumber,
            set_offset: Option<SetNumber>,
            request_flags: Option<SetNumber>,
            set_match_case: Option<SetFlag>,
            set_match_whole_word: Option<SetFlag>,
            set_match_path: Option<SetFlag>,
            set_regex: Option<SetFlag>,
            reset: Option<Reset>,
            is_db_loaded: Option<IsDbLoaded>,
            versions: [Option<Version>; 4],
        }

        fn api() -> Option<&'static Api> {
            static API: OnceLock<Option<Api>> = OnceLock::new();
            API.get_or_init(load).as_ref()
        }

        fn load() -> Option<Api> {
            let module = module()?;
            // SAFETY: every name below is looked up in the SDK's own DLL and
            // cast to the type Everything.h declares for it; the lock makes
            // the calls that follow single at a time.
            let api = unsafe {
                Api {
                    set_search: symbol(module, c"Everything_SetSearchW")?,
                    query: symbol(module, c"Everything_QueryW")?,
                    num_results: symbol(module, c"Everything_GetNumResults")?,
                    tot_results: symbol(module, c"Everything_GetTotResults"),
                    is_folder: symbol(module, c"Everything_IsFolderResult")?,
                    full_path: symbol(module, c"Everything_GetResultFullPathNameW")?,
                    size: symbol(module, c"Everything_GetResultSize"),
                    date_modified: symbol(module, c"Everything_GetResultDateModified"),
                    last_error: symbol(module, c"Everything_GetLastError")?,
                    set_max: symbol(module, c"Everything_SetMax")?,
                    set_offset: symbol(module, c"Everything_SetOffset"),
                    request_flags: symbol(module, c"Everything_SetRequestFlags"),
                    set_match_case: symbol(module, c"Everything_SetMatchCase"),
                    set_match_whole_word: symbol(module, c"Everything_SetMatchWholeWord"),
                    set_match_path: symbol(module, c"Everything_SetMatchPath"),
                    set_regex: symbol(module, c"Everything_SetRegex"),
                    reset: symbol(module, c"Everything_Reset"),
                    is_db_loaded: symbol(module, c"Everything_IsDBLoaded"),
                    versions: [
                        symbol(module, c"Everything_GetMajorVersion"),
                        symbol(module, c"Everything_GetMinorVersion"),
                        symbol(module, c"Everything_GetRevision"),
                        symbol(module, c"Everything_GetBuildNumber"),
                    ],
                }
            };
            tracing::debug!(version = %version(&api), "the Everything SDK answered");
            Some(api)
        }

        /// Loads the SDK DLL for this architecture, from the `dll` folder the
        /// installer fills, the program's own folder, or Windows' search.
        fn module() -> Option<HMODULE> {
            let name = match std::env::consts::ARCH {
                "x86_64" => "Everything64.dll",
                "x86" => "Everything32.dll",
                "aarch64" => "EverythingARM64.dll",
                "arm" => "EverythingARM.dll",
                _ => return None,
            };
            let mut candidates: Vec<OsString> = Vec::new();
            if let Ok(executable) = std::env::current_exe()
                && let Some(folder) = executable.parent()
            {
                candidates.push(folder.join("dll").join(name).into_os_string());
                candidates.push(folder.join(name).into_os_string());
                // A test binary runs from `target/<profile>/deps`, one level
                // below the folder the app itself runs from.
                #[cfg(test)]
                {
                    if let Some(above) = folder.parent() {
                        candidates.push(above.join("dll").join(name).into_os_string());
                    }
                }
            }
            // The bare name keeps Windows' own search rules (the program's
            // folder, the system's, PATH), for an SDK installed elsewhere.
            candidates.push(name.into());
            for candidate in candidates {
                let wide: Vec<u16> = candidate.encode_wide().chain(std::iter::once(0)).collect();
                // SAFETY: `wide` is a terminated wide string that outlives the
                // call, and the handle is never freed so the DLL stays loaded.
                if let Ok(module) = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) } {
                    return Some(module);
                }
            }
            tracing::debug!("the Everything SDK DLL was not found next to the program");
            None
        }

        unsafe fn symbol<T>(module: HMODULE, name: &CStr) -> Option<T> {
            let found = unsafe { GetProcAddress(module, PCSTR(name.as_ptr().cast())) }?;
            // SAFETY: a Win32 entry point and the SDK's declared function
            // pointer are both one pointer wide; the name above chooses which
            // declaration applies.
            debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<windows::Win32::Foundation::FARPROC>());
            Some(unsafe { std::mem::transmute_copy(&found) })
        }

        /// Every name Everything has, as the settings page counts them, and
        /// whether it answers at all.
        pub(super) fn probe() -> Result<Info, Error> {
            let api = api().ok_or(Error::Missing)?;
            let _session = SDK.enter().ok_or(Error::Busy)?;
            let empty = [0u16];
            // SAFETY: the search string outlives the call and the pointers are
            // the SDK's own exports; the gate keeps them one at a time.
            unsafe {
                // `Reset` puts back the SDK's default request flags (path and
                // file name), the pair that keeps this probe on the smaller
                // query the SDK sends for it.
                reset(api);
                (api.set_search)(PCWSTR(empty.as_ptr()));
                (api.set_max)(1);
                if let Some(set_offset) = api.set_offset {
                    set_offset(0);
                }
                if let Some(flags) = api.request_flags {
                    flags(REQUEST_FILE_NAME | REQUEST_PATH);
                }
                if !(api.query)(BOOL::from(true)).as_bool() {
                    let error = error(api);
                    reset(api);
                    return Err(error);
                }
                // A client that answered may still be reading its index; the
                // count is then the part it has so far.
                let loaded = api.is_db_loaded.is_none_or(|is_loaded| is_loaded().as_bool());
                let entries = api.tot_results.map_or_else(|| u64::from((api.num_results)()), |tot| u64::from(tot()));
                reset(api);
                if !loaded {
                    return Err(Error::NotReady);
                }
                Ok(Info {
                    entries,
                    version: version(api),
                    pinyin: super::pinyin_modifier(),
                    full_pinyin: super::full_pinyin_modifier(),
                })
            }
        }

        /// The names holding every one of `terms`, up to `limit` of them.
        /// `cancel` is read between hits: a newer keystroke stops this one.
        pub(super) fn search(
            terms: &[String],
            limit: u32,
            cancel: &AtomicBool,
            modifier: Option<&str>,
        ) -> Result<Vec<Hit>, Error> {
            let api = api().ok_or(Error::Missing)?;
            if cancel.load(Ordering::Relaxed) {
                return Ok(Vec::new());
            }
            let _session = SDK.enter().ok_or(Error::Busy)?;
            let search = search_text(terms, modifier);
            // SAFETY: as in `probe`; the search string outlives the call.
            unsafe {
                reset(api);
                (api.set_search)(PCWSTR(search.as_ptr()));
                (api.set_max)(limit);
                if let Some(set_offset) = api.set_offset {
                    set_offset(0);
                }
                if let Some(flags) = api.request_flags {
                    flags(REQUEST_FULL_PATH | REQUEST_SIZE | REQUEST_DATE_MODIFIED);
                }
                // The local scan is a case-insensitive substring match over
                // the name; these are the SDK's defaults, set anyway so a
                // previous query cannot leave them the other way.
                if let Some(command) = api.set_match_case {
                    command(BOOL::from(false));
                }
                if let Some(command) = api.set_match_whole_word {
                    command(BOOL::from(false));
                }
                if let Some(command) = api.set_match_path {
                    command(BOOL::from(false));
                }
                if let Some(command) = api.set_regex {
                    command(BOOL::from(false));
                }
                if !(api.query)(BOOL::from(true)).as_bool() {
                    let error = error(api);
                    reset(api);
                    return Err(error);
                }
                if !api.is_db_loaded.is_none_or(|is_loaded| is_loaded().as_bool()) {
                    reset(api);
                    return Err(Error::NotReady);
                }
                let count = (api.num_results)();
                let mut hits = Vec::with_capacity((count as usize).min(1024));
                for index in 0..count {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some(path) = full_path(api, index) else {
                        continue;
                    };
                    let dir = (api.is_folder)(index).as_bool();
                    let size = if dir { 0 } else { result_size(api, index) };
                    hits.push(Hit { path, dir, size, modified: result_modified(api, index) });
                }
                reset(api);
                if hits.is_empty() && cancel.load(Ordering::Relaxed) {
                    return Ok(Vec::new());
                }
                Ok(hits)
            }
        }

        fn reset(api: &Api) {
            if let Some(reset) = api.reset {
                // SAFETY: the SDK's own reset, called under the lock.
                unsafe { reset() };
            }
        }

        fn error(api: &Api) -> Error {
            // SAFETY: the SDK's own error number for the query that failed.
            let code = unsafe { (api.last_error)() };
            if code == ERROR_IPC {
                // The IPC error means "no client answers", and the client the
                // SDK finds is the default instance only: a named one (the
                // alpha builds share the machine under `-instance`) is
                // running but out of reach, which is worth saying.
                match discover::named_instance() {
                    Some(name) => Error::NamedInstance(name),
                    None => Error::NotRunning,
                }
            } else {
                Error::Code(code)
            }
        }

        fn version(api: &Api) -> String {
            let mut numbers = [0u32; 4];
            for (at, found) in api.versions.iter().enumerate() {
                if let Some(number) = found {
                    // SAFETY: an exported version getter, called under the lock.
                    numbers[at] = unsafe { number() };
                }
            }
            format!("{}.{}.{}.{}", numbers[0], numbers[1], numbers[2], numbers[3])
        }

        unsafe fn full_path(api: &Api, index: u32) -> Option<String> {
            let mut buffer = vec![0u16; PATH_UNITS];
            loop {
                let units = u32::try_from(buffer.len()).ok()?;
                // SAFETY: the buffer and its length match, and the call writes
                // at most that many UTF-16 units, returning how many it copied
                // without the terminator.
                let copied = unsafe { (api.full_path)(index, buffer.as_mut_ptr(), units) };
                let copied = usize::try_from(copied).ok()?;
                if copied == 0 {
                    return None;
                }
                // Filled to the last unit: the path may be cut, so ask again
                // with room for the longest path Windows allows.
                if copied + 1 >= buffer.len() {
                    if buffer.len() >= LONG_PATH_UNITS {
                        return None;
                    }
                    buffer = vec![0u16; LONG_PATH_UNITS];
                    continue;
                }
                let length = buffer.iter().position(|&unit| unit == 0).unwrap_or(copied).min(copied);
                return Some(String::from_utf16_lossy(&buffer[..length]));
            }
        }

        unsafe fn result_size(api: &Api, index: u32) -> u64 {
            let Some(get_size) = api.size else { return 0 };
            let mut size = 0i64;
            // SAFETY: `size` is a LARGE_INTEGER the call may fill.
            if unsafe { get_size(index, &mut size) }.as_bool() { u64::try_from(size).unwrap_or(0) } else { 0 }
        }

        unsafe fn result_modified(api: &Api, index: u32) -> u32 {
            let Some(get_date) = api.date_modified else { return 0 };
            // SAFETY: `date` is a FILETIME the call may fill.
            let mut date = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            if unsafe { get_date(index, &mut date) }.as_bool() {
                filetime_seconds((u64::from(date.dwHighDateTime) << 32) | u64::from(date.dwLowDateTime))
            } else {
                0
            }
        }
    }

    #[cfg(all(test, windows))]
    mod tests {
        use super::*;

        fn text(terms: &[&str], modifier: Option<&str>) -> String {
            let terms: Vec<String> = terms.iter().map(|term| (*term).to_string()).collect();
            let units = search_text(&terms, modifier);
            String::from_utf16_lossy(&units[..units.len() - 1])
        }

        #[test]
        fn the_terms_are_quoted_and_joined_so_all_must_match() {
            assert_eq!(text(&["bonk", "wav"], None), "\"bonk\" \"wav\"");
            assert_eq!(text(&["c++", "a|b"], None), "\"c++\" \"a|b\"");
            assert_eq!(text(&["quo\"te"], None), "\"quote\"");
        }

        #[test]
        fn a_pinyin_modifier_goes_before_each_quoted_term() {
            assert_eq!(
                text(&["wjj", "bg"], Some("pinyin-jian:")),
                "pinyin-jian:\"wjj\" pinyin-jian:\"bg\""
            );
        }

        #[test]
        fn a_file_time_becomes_seconds_since_1970() {
            // 1601 to 1970, in 100 ns ticks, then 2026-11-05 12:00:00 UTC.
            const EPOCH_TICKS: u64 = 116_444_736_000_000_000;
            assert_eq!(filetime_seconds(0), 0);
            assert_eq!(filetime_seconds(EPOCH_TICKS + 1_793_880_000 * 10_000_000), 1_793_880_000);
            // Before 1970 clamps to zero rather than wrapping.
            assert_eq!(filetime_seconds(EPOCH_TICKS / 2), 0);
        }

        /// The real thing: needs Everything installed and running, so it only
        /// runs on request:
        /// `cargo test --release -p nohrs-pages everything_answers -- --ignored`
        #[test]
        #[ignore = "needs Everything installed and running"]
        fn everything_answers_and_searches() {
            let info = match probe() {
                Ok(info) => info,
                Err(Error::Missing) => {
                    panic!("put Everything64.dll in target/release/dll first, or open Everything")
                }
                Err(error) => panic!("Everything did not answer: {error:?}"),
            };
            assert!(info.entries > 0, "Everything indexed no names");
            let cancel = Arc::new(AtomicBool::new(false));
            let hits = search(&["notepad".to_string()], 100, &cancel, None, None).expect("a search");
            assert!(!hits.is_empty(), "no hits for notepad");
            for hit in &hits {
                let name = hit.path.rsplit('\\').next().unwrap_or_default().to_lowercase();
                assert!(name.contains("notepad"), "{name} does not hold the term");
                assert!(!hit.path.is_empty());
            }
            assert!(hits.iter().any(|hit| !hit.dir && hit.size > 0), "a file with no size");
        }

        /// A name written in Han characters, found by its pinyin initials: the
        /// whole point of the modifier. Needs a Han-named file Everything knows.
        #[test]
        #[ignore = "needs Everything installed and running"]
        fn everything_answers_a_name_by_its_pinyin_initials() {
            let Some(modifier) = pinyin_modifier() else {
                eprintln!("this Everything has no pinyin search; nothing to check");
                return;
            };
            let han = search(&["文件".to_string()], 200, &Arc::new(AtomicBool::new(false)), None, None)
                .unwrap_or_default();
            let Some(hit) = han.into_iter().find(|hit| hit.path.chars().any(|ch| !ch.is_ascii())) else {
                eprintln!("no Han-named name in this index; nothing to check");
                return;
            };
            let name = hit.path.rsplit('\\').next().unwrap_or_default().to_lowercase();
            let Some(initials) = crate::explorer::pinyin::initials(&name) else {
                return;
            };
            let by_initials = search(
                &[initials.clone()],
                200,
                &Arc::new(AtomicBool::new(false)),
                Some(modifier),
                None,
            )
            .expect("a pinyin search");
            assert!(
                by_initials.iter().any(|other| other.path == hit.path),
                "{initials} (\u{7b80}\u{6291}) did not find {}",
                hit.path
            );
        }
    }
}

/// Folders read at once. The walk waits on the disk, not the processor —
/// a cold walk of this machine's C: used a third of one core — so more
/// readers than cores keep more requests in the drive's queue.
const WALKERS: usize = 32;
/// How often a first walk is saved as it goes.
const CHECKPOINT: Duration = Duration::from_secs(30);

/// One name a folder listing gave.
pub(crate) struct Listed {
    name: String,
    found: Found,
    // A link or junction: named, not followed.
    link: bool,
    // Online-only (OneDrive and the like): reading it would download it.
    cloud: bool,
}

fn walk_drive(
    drive: &str,
    table: Arc<Mutex<Segment>>,
    progress: Arc<AtomicUsize>,
    workers: usize,
    list: fn(&Path, &mut Vec<Listed>),
    checkpoint: &mut dyn FnMut(&Segment),
) -> Segment {
    let root = lock(&table).add(NO_PARENT, drive.trim_end_matches('\\'), Found { dir: true, size: 0, modified: 0 });
    let walk = Arc::new(Walk {
        queue: Mutex::new((vec![(PathBuf::from(drive), root)], 0)),
        wake: Condvar::new(),
        out: table,
        progress,
        list,
    });
    let handles: Vec<_> = (0..workers)
        .filter_map(|_| {
            let walk = walk.clone();
            std::thread::Builder::new()
                .name("nohrs-everything-walk".into())
                // A listing buffer and a few paths; the default 1 MB stack
                // times 32 readers is memory for nothing.
                .stack_size(256 * 1024)
                .spawn(move || worker(&walk))
                .log_err()
        })
        .collect();
    let mut saved = Instant::now();
    while !handles.iter().all(std::thread::JoinHandle::is_finished) {
        std::thread::sleep(Duration::from_millis(100));
        if saved.elapsed() >= CHECKPOINT {
            // Under the lock: the readers wait the second it takes, and no
            // copy of a table of millions is made to spare them that.
            checkpoint(&lock(&walk.out));
            saved = Instant::now();
        }
    }
    for handle in handles {
        if handle.join().is_err() {
            tracing::error!("an index walker panicked");
        }
    }
    let mut segment = std::mem::replace(&mut *lock(&walk.out), Segment::empty());
    segment.complete = true;
    segment
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn worker(walk: &Walk) {
    loop {
        let job = {
            let mut queue = lock(&walk.queue);
            loop {
                if let Some(job) = queue.0.pop() {
                    queue.1 += 1;
                    break Some(job);
                }
                if queue.1 == 0 {
                    break None;
                }
                queue = walk.wake.wait(queue).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let Some((folder, id)) = job else {
            walk.wake.notify_all();
            return;
        };
        // Read outside every lock: this is the slow part.
        let mut listed = Vec::new();
        (walk.list)(&folder, &mut listed);
        let lowered: Vec<(String, u64)> = listed
            .iter()
            .map(|entry| {
                let lower = entry.name.to_lowercase();
                let mask = letter_mask(&lower);
                (lower, mask)
            })
            .collect();
        let mut children = Vec::new();
        {
            let mut out = lock(&walk.out);
            for (entry, (lower, mask)) in listed.iter().zip(&lowered) {
                let child = out.add_lowered(id, &entry.name, lower, *mask, entry.found);
                // Links and junctions are named but not followed: one that
                // points at a parent would walk forever, and the rest point
                // at folders the walk reaches anyway. A cloud folder not yet
                // on the disk (OneDrive's "online only") is named but not
                // read either: listing it makes the sync app fetch it from
                // the network, which held a walk to a few hundred names a
                // second.
                if entry.found.dir && !entry.link && !entry.cloud && !skipped(&entry.name) {
                    children.push((folder.join(&entry.name), child));
                }
            }
        }
        walk.progress.fetch_add(listed.len(), Ordering::Relaxed);
        // The queue is taken from its end, and the drive's root lists
        // Windows after Users: without this the first minutes went to
        // C:\Windows, and a search during the first walk found nothing of
        // the person's own files until it was done.
        if id == 0 {
            children.sort_by_key(|(path, _)| walk_priority(path));
        }
        let mut queue = lock(&walk.queue);
        queue.0.extend(children);
        queue.1 -= 1;
        drop(queue);
        walk.wake.notify_all();
    }
}

// Higher is walked sooner: people look for their own files first, and the
// system's folders are the largest and least searched.
fn walk_priority(path: &Path) -> u8 {
    let name = path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default();
    match name.as_str() {
        "users" => 3,
        "windows" | "programdata" | "$recycle.bin" | "system volume information" => 0,
        name if name.starts_with("program files") => 1,
        _ => 2,
    }
}

fn in_the_cloud(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x40000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x400000;
    attributes & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
}

/// A folder listed by the standard library: what the walk did before
/// `platform::list`, kept for the other systems and to measure against.
#[cfg_attr(windows, allow(dead_code))]
fn list_std(folder: &Path, out: &mut Vec<Listed>) {
    let Ok(read) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in read.filter_map(Result::ok) {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let metadata = entry.metadata().ok();
        #[cfg(windows)]
        let attributes = metadata.as_ref().map_or(0, std::os::windows::fs::MetadataExt::file_attributes);
        #[cfg(not(windows))]
        let attributes = 0;
        out.push(Listed {
            name: entry.file_name().to_string_lossy().into_owned(),
            found: Found {
                dir: kind.is_dir(),
                ..metadata.as_ref().map_or(Found { dir: false, size: UNKNOWN_SIZE, modified: 0 }, Found::of)
            },
            link: kind.is_symlink(),
            cloud: in_the_cloud(attributes),
        });
    }
}

// Folders whose contents nobody searches for and that only answer "access
// denied" or hold copies of what is elsewhere.
fn skipped(name: &str) -> bool {
    name.eq_ignore_ascii_case("$Recycle.Bin")
        || name.eq_ignore_ascii_case("System Volume Information")
        || name.eq_ignore_ascii_case("$WinREAgent")
}

// ---------------------------------------------------------------------------
// The saved table

fn index_file() -> PathBuf {
    nohrs_core::config::paths::cache_dir().join("everything.idx")
}

fn save(index: &Index, path: &Path) -> std::io::Result<()> {
    let segments: Vec<&Segment> = index.segments.iter().map(|segment| &**segment).collect();
    save_segments(index.built, &segments, path)
}

fn save_segments(built: SystemTime, segments: &[&Segment], path: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&temporary)?);
    let built = built.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    file.write_all(FILE_MAGIC)?;
    file.write_all(&built.to_le_bytes())?;
    file.write_all(&(segments.len() as u64).to_le_bytes())?;
    for segment in segments {
        file.write_all(&(segment.entries.len() as u64).to_le_bytes())?;
        file.write_all(&(segment.names.len() as u64).to_le_bytes())?;
        file.write_all(&(segment.lower.len() as u64).to_le_bytes())?;
        file.write_all(&u64::from(segment.complete).to_le_bytes())?;
        for entry in &segment.entries {
            file.write_all(&entry.parent.to_le_bytes())?;
            file.write_all(&entry.name.to_le_bytes())?;
            file.write_all(&entry.lower.to_le_bytes())?;
            file.write_all(&entry.name_len.to_le_bytes())?;
            file.write_all(&entry.lower_len.to_le_bytes())?;
            file.write_all(&entry.size.to_le_bytes())?;
            file.write_all(&entry.modified.to_le_bytes())?;
            // Only what a listing said: the noise flag and the mask are
            // worked out again on reading.
            file.write_all(&[entry.flags & DIR])?;
        }
        file.write_all(segment.names.as_bytes())?;
        file.write_all(segment.lower.as_bytes())?;
    }
    file.into_inner().map_err(|error| error.into_error())?.sync_all()?;
    std::fs::rename(&temporary, path)
}

const ENTRY_BYTES_V2: usize = 17;
const ENTRY_BYTES: usize = 29;

fn number(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

fn load(path: &Path) -> Option<Index> {
    let bytes = std::fs::read(path).ok()?;
    let entry_bytes = match bytes.get(..8)? {
        magic if magic == FILE_MAGIC => ENTRY_BYTES,
        magic if magic == FILE_MAGIC_V2 => ENTRY_BYTES_V2,
        _ => return None,
    };
    let built = SystemTime::UNIX_EPOCH + Duration::from_secs(number(&bytes, 8)?);
    let count = usize::try_from(number(&bytes, 16)?).ok()?;
    let mut at = 24usize;
    let mut segments = Vec::new();
    for _ in 0..count {
        let (segment, next) = load_segment(&bytes, at, entry_bytes == ENTRY_BYTES)?;
        segments.push(Arc::new(segment));
        at = next;
    }
    (at == bytes.len()).then_some(Index { segments, built })
}

// The segment starting at `at`, and where the next one starts.
fn load_segment(bytes: &[u8], at: usize, current: bool) -> Option<(Segment, usize)> {
    let entry_bytes = if current { ENTRY_BYTES } else { ENTRY_BYTES_V2 };
    let count = usize::try_from(number(bytes, at)?).ok()?;
    let names_len = usize::try_from(number(bytes, at + 8)?).ok()?;
    let lower_len = usize::try_from(number(bytes, at + 16)?).ok()?;
    // A table from before 0.3.2 was only ever saved whole.
    let (complete, header) = if current { (number(bytes, at + 24)? != 0, 32) } else { (true, 24) };
    let entries_start = at.checked_add(header)?;
    let entries_end = entries_start.checked_add(count.checked_mul(entry_bytes)?)?;
    let names_end = entries_end.checked_add(names_len)?;
    let lower_end = names_end.checked_add(lower_len)?;
    let mut entries = Vec::with_capacity(count);
    for (position, raw) in bytes.get(entries_start..entries_end)?.chunks_exact(entry_bytes).enumerate() {
        let word = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let half = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
        let (size, modified, flags) = if entry_bytes == ENTRY_BYTES {
            (u64::from_le_bytes(raw[16..24].try_into().ok()?), word(24), raw[28] & DIR)
        } else {
            (UNKNOWN_SIZE, 0, if raw[16] != 0 { DIR } else { 0 })
        };
        let entry = Entry {
            size: if flags & DIR != 0 { 0 } else { size },
            mask: 0,
            parent: word(0),
            name: word(4),
            lower: word(8),
            pinyin: 0,
            modified,
            name_len: half(12),
            lower_len: half(14),
            pinyin_len: 0,
            flags,
        };
        // Every slice the table hands out must land inside its strings, and
        // a parent comes before its children (the noise flag below is
        // passed down in one sweep).
        if entry.name as usize + entry.name_len as usize > names_len
            || entry.lower as usize + entry.lower_len as usize > lower_len
            || (entry.parent != NO_PARENT && entry.parent as usize >= position)
        {
            return None;
        }
        entries.push(entry);
    }
    let names = String::from_utf8(bytes.get(entries_end..names_end)?.to_vec()).ok()?;
    let lower = String::from_utf8(bytes.get(names_end..lower_end)?.to_vec()).ok()?;
    let mut segment = Segment { names, lower, pinyin: String::new(), entries, non_ascii: Vec::new(), paths: Vec::new(), complete };
    // A slice cut inside a character would come back empty; one such entry
    // means the file is not what this version wrote.
    let mut lower_offset = 0;
    let consistent = segment.entries.iter().all(|entry| {
        let start = entry.lower as usize;
        let end = start + entry.lower_len as usize;
        let contiguous = start == lower_offset;
        lower_offset = end;
        contiguous
            && segment.names.is_char_boundary(entry.name as usize)
            && segment.names.is_char_boundary(entry.name as usize + entry.name_len as usize)
            && segment.lower.is_char_boundary(start)
            && segment.lower.is_char_boundary(end)
    });
    if !consistent || lower_offset != segment.lower.len() {
        return None;
    }
    for position in 0..segment.entries.len() {
        let entry = segment.entries[position];
        let parent = segment.entries.get(entry.parent as usize).copied();
        let (full_pinyin, mask, noise) = {
            let lowered = segment.lower(&entry);
            let full_pinyin = super::pinyin::full(lowered).unwrap_or_default();
            let mask = letter_mask(lowered);
            let at_root = parent.is_some_and(|parent| parent.parent == NO_PARENT);
            let noise = parent.is_some_and(|parent| parent.flags & NOISE != 0) || noisy(lowered, at_root);
            (full_pinyin, mask, noise)
        };
        if let Some(entry) = segment.entries.get_mut(position) {
            entry.pinyin = segment.pinyin.len() as u32;
            entry.pinyin_len = full_pinyin.len() as u16;
        }
        segment.pinyin.push_str(&full_pinyin);
        if !segment.lower(&entry).is_ascii() {
            segment.non_ascii.push(position as u32);
        }
        if let Some(entry) = segment.entries.get_mut(position) {
            entry.mask = mask;
            if noise {
                entry.flags |= NOISE;
            }
        }
    }
    Some((segment, lower_end))
}

// ---------------------------------------------------------------------------
// The app-wide state

/// The index every pane searches, and the walk refreshing it.
#[derive(Default)]
pub(crate) struct Everything {
    pub index: Option<Arc<Index>>,
    /// Names found so far by the walk in progress.
    pub building: Option<Arc<AtomicUsize>>,
    // The drive being walked, searchable while it fills.
    walking: Option<Arc<Mutex<Segment>>>,
    loading: bool,
    /// Bumped each time `index` is replaced, so panes rerun their search.
    pub generation: u64,
    // What the watcher saw created since the walks.
    fresh: Arc<Mutex<Fresh>>,
    /// The indexer service is being installed or removed.
    pub mft_busy: bool,
    /// Why the last install or removal failed.
    pub mft_error: Option<String>,
    /// Where `[indexing] source` sends the whole-computer names.
    engine: IndexSource,
    /// Everything's answer to the last probe; `None` until it is asked.
    external: Option<Result<remote::Info, remote::Error>>,
    /// A probe is on its way to Everything.
    probing: bool,
    /// Bumped when `engine` changes: a walk that started under the old choice
    /// does not install its table under the new one.
    engine_epoch: u64,
    _watcher: Option<notify::RecommendedWatcher>,
    _task: Option<Task<()>>,
    /// Panes searching the whole computer right now: the table is kept while
    /// there is at least one, and handed back a while after the last leaves.
    users: usize,
    // The wait that hands the table back, cancelled by a pane coming back.
    idle: Option<Task<()>>,
}

impl Global for Everything {}

/// Where the names of the whole-computer search come from, as resolved.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// Everything's own index, through its SDK or its IPC window.
    Everything,
    /// This process's table, or the indexer service's.
    Own,
    /// `auto`, and Everything has not answered yet.
    Pending,
}

fn source_of(state: &Everything) -> Source {
    match state.engine {
        IndexSource::PrimeFiles => Source::Own,
        IndexSource::Everything => Source::Everything,
        IndexSource::Auto => match state.external {
            Some(Ok(_)) => Source::Everything,
            Some(Err(_)) => Source::Own,
            None => Source::Pending,
        },
    }
}

/// The source in effect for a whole-computer search.
pub(crate) fn source(cx: &App) -> Source {
    cx.try_global::<Everything>().map_or(Source::Own, source_of)
}

/// Entries created or renamed since the walk of their drive, as the
/// watcher reported them, and when each was seen.
struct Fresh {
    table: Segment,
    seen: Vec<SystemTime>,
}

impl Default for Fresh {
    fn default() -> Self {
        Self { table: Segment::empty(), seen: Vec::new() }
    }
}

impl Fresh {
    fn record(&mut self, path: String, found: Found) {
        if self.table.entries.len() >= FRESH_LIMIT {
            let half = self.seen.get(FRESH_LIMIT / 2).copied().unwrap_or(SystemTime::UNIX_EPOCH);
            self.retain(|_, seen| seen > half);
        }
        self.table.add_loose(path, found);
        self.seen.push(SystemTime::now());
    }

    fn retain(&mut self, mut keep: impl FnMut(&str, SystemTime) -> bool) {
        let old = std::mem::take(self);
        for (position, (entry, seen)) in old.table.entries.iter().zip(&old.seen).enumerate() {
            let Some(path) = old.table.paths.get(position) else {
                continue;
            };
            if keep(path, *seen) {
                let found = Found { dir: entry.flags & DIR != 0, size: entry.size, modified: entry.modified };
                self.table.add_loose(path.clone(), found);
                self.seen.push(*seen);
            }
        }
    }
}

// Paths whose creation nobody searches for and that come by the thousand:
// temporary files, the system's own folders, version control.
fn churn(lowered: &str) -> bool {
    let name = lowered.rsplit('\\').next().unwrap_or(lowered);
    lowered.contains("\\appdata\\local\\temp\\")
        || lowered.contains("\\$recycle.bin\\")
        || lowered.contains("\\system volume information\\")
        || lowered.contains("\\.git\\")
        || lowered.get(1..).is_some_and(|rest| rest.starts_with(":\\windows\\"))
        || name.ends_with(".tmp")
        || name.starts_with("~$")
}

// Watches every fixed drive, without administrator rights (Everything's
// USN journal needs them), for names that appear.
fn watch_drives(fresh: Arc<Mutex<Fresh>>) -> Option<notify::RecommendedWatcher> {
    use notify::event::{ModifyKind, RenameMode};
    use notify::{EventKind, RecursiveMode, Watcher as _};
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else {
            return;
        };
        let paths: Vec<PathBuf> = match event.kind {
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Any)) => event.paths,
            // From and to: only the new name is new.
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => event.paths.into_iter().last().into_iter().collect(),
            _ => return,
        };
        for path in paths {
            let text = path.to_string_lossy().into_owned();
            if churn(&text.to_lowercase()) {
                continue;
            }
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            lock(&fresh).record(text, Found::of(&metadata));
        }
    })
    .log_err()?;
    for drive in platform::fixed_drives() {
        if let Err(error) = watcher.watch(Path::new(&drive), RecursiveMode::Recursive) {
            tracing::warn!("cannot watch {drive} for the everything index: {error}");
        }
    }
    Some(watcher)
}

impl Everything {
    // Whether the drive being walked is missing from the index, so searches
    // read the walk itself.
    fn walking_is_new(&self) -> bool {
        let Some(table) = &self.walking else {
            return false;
        };
        // A drive only half walked before is searched in both tables, the
        // old half and the walk's: the walk shows what it found since.
        let root = lock(table).root().to_string();
        !self
            .index
            .as_ref()
            .is_some_and(|index| index.segments.iter().any(|segment| segment.root() == root && segment.complete))
    }
}

/// A pane started searching the whole computer: the table is kept, and a wait
/// that was about to hand it back is called off.
fn hold(cx: &mut Context<ExplorerPane>) {
    cx.update_global(|state: &mut Everything, _| {
        state.users += 1;
        state.idle = None;
    });
}

/// A pane stopped: with none left, the table goes back to its file after
/// [`KEEP_FOR`], unless another pane starts searching before then.
fn release(cx: &mut Context<ExplorerPane>) {
    let users = cx.update_global(|state: &mut Everything, _| {
        state.users = state.users.saturating_sub(1);
        state.users
    });
    if users > 0 {
        return;
    }
    let task = cx.spawn(async move |_pane: WeakEntity<ExplorerPane>, cx| {
        cx.background_executor().timer(KEEP_FOR).await;
        cx.update(|cx| {
            let state = cx.default_global::<Everything>();
            if state.users > 0 {
                return;
            }
            // Freed here rather than kept for a search that may never come:
            // the file on disk is what the next one reads.
            let entries = state.index.as_ref().map_or(0, |index| index.len());
            state.index = None;
            state.walking = None;
            state.building = None;
            state.idle = None;
            state.generation += 1;
            tracing::info!(entries, "the everything index was handed back to its file");
        })
        .log_err();
    });
    cx.update_global(|state: &mut Everything, _| state.idle = Some(task));
}

/// Takes the `[indexing] source` the config now asks for. A change throws away
/// the table this process was keeping — a query is now answered from the other
/// side — and asks Everything again for `auto` (and for the page's status).
pub(crate) fn set_engine(engine: IndexSource, cx: &mut App) {
    let state = cx.default_global::<Everything>();
    if state.engine == engine {
        return;
    }
    state.engine = engine;
    state.external = None;
    state.probing = false;
    state.index = None;
    state.walking = None;
    state.building = None;
    state.generation += 1;
    state.engine_epoch += 1;
    tracing::info!(?engine, "the whole-computer index source changed");
    // `auto` has to hear from Everything before it can choose, and the other
    // choices want the page's status line to say what Everything answers.
    if engine != IndexSource::PrimeFiles {
        probe(cx);
    }
    cx.refresh_windows();
}

/// Asks Everything whether it answers — its SDK first, then its IPC window —
/// off the UI thread, and keeps the answer for the bar and the settings page.
/// On `auto` it decides the source, so the panes' next search reads it.
pub(crate) fn probe(cx: &mut App) {
    if cx.default_global::<Everything>().probing {
        return;
    }
    cx.default_global::<Everything>().probing = true;
    let probe = cx.background_spawn(async { remote::probe() });
    cx.spawn(async move |cx| {
        let answer = probe.await;
        let own = cx
            .update(|cx| {
                let state = cx.default_global::<Everything>();
                state.probing = false;
                match &answer {
                    Ok(info) => tracing::info!(
                        entries = info.entries,
                        version = %info.version,
                        "Everything's index answered"
                    ),
                    Err(error) => tracing::debug!("Everything's index did not answer: {error:?}"),
                }
                state.external = Some(answer);
                state.generation += 1;
                source_of(state) == Source::Own
            })
            .log_err();
        // `auto` chose this program's own table: read it now, since the walk
        // that `ensure` would have started was put off while Everything was
        // being asked.
        if own == Some(true) {
            cx.update(ensure).log_err();
        }
        cx.update(|cx| cx.refresh_windows()).log_err();
    })
    .detach();
}

/// What a pane's bar says about Everything's index, when it is the source or
/// when `auto` is still waiting for it.
pub(crate) fn external_text(external: Option<&Result<remote::Info, remote::Error>>) -> String {
    match external {
        None => t!("Verificando o Everything…").to_string(),
        Some(Ok(info)) => tf!("O índice do Everything: {n} itens.", n = info.entries),
        Some(Err(error)) => error_text(error),
    }
}

/// Why Everything's index did not answer, as the page says it.
pub(crate) fn error_text(error: &remote::Error) -> String {
    match error {
        remote::Error::Missing => t!("O Everything não foi encontrado. Instale-o ou escolha outro índice.").to_string(),
        remote::Error::NotRunning => t!("O Everything não está aberto. Abra-o para usar o índice dele.").to_string(),
        remote::Error::NotReady => t!("O Everything está aberto; o índice dele ainda não foi lido.").to_string(),
        remote::Error::Busy => t!("O Everything não está respondendo.").to_string(),
        remote::Error::NamedInstance(instance) => tf!(
            "O Everything está aberto como a instância {instance}. Esta busca só alcança a instância padrão do Everything.",
            instance = instance
        ),
        remote::Error::Code(code) => tf!("O Everything não respondeu: {error}", error = code),
    }
}

/// The line under the index choice when `auto` fell back to this program.
pub(crate) fn auto_note(external: Option<&Result<remote::Info, remote::Error>>) -> String {
    match external {
        Some(Err(remote::Error::NamedInstance(instance))) => tf!(
            "O Everything está aberto como a instância {instance}; usando o índice deste programa.",
            instance = instance
        ),
        _ => t!("O Everything não está aberto; usando o índice deste programa.").to_string(),
    }
}

/// The service's hits for `query`, filtered and ordered the way the local
/// scan orders its own: the service matched the name terms, and the rest of
/// the query (extensions, sizes, dates, paths, wildcards) is applied here,
/// to a few thousand rows rather than to fourteen million.
fn service_targets(query: &Query, cancel: &AtomicBool) -> Vec<(String, String, bool, u64)> {
    let terms = query.name_terms();
    if terms.is_empty() || cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let hits = match mft::search(&terms, SERVICE_HITS) {
        Ok(hits) => hits,
        Err(error) => {
            tracing::debug!("the service could not search: {error}");
            return Vec::new();
        }
    };
    targets_from_hits(query, cancel, hits.into_iter().map(|hit| (hit.path, hit.dir, hit.size, hit.modified)))
}

/// Everything's hits for `query`, filtered and ordered as the service's are.
/// `pinyin` is the modifier that makes its words match pinyin initials, when
/// this Everything has one. A failure is left in `error` for the pane's bar to
/// say why the list is empty — Everything may have been closed since the mode
/// opened.
fn external_targets(
    query: &Query,
    cancel: &Arc<AtomicBool>,
    error: &Mutex<Option<remote::Error>>,
    pinyin: Option<&'static str>,
    full_pinyin: Option<&'static str>,
) -> Vec<(String, String, bool, u64)> {
    let terms = query.name_terms();
    if terms.is_empty() || cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    match remote::search(&terms, SERVICE_HITS, cancel, pinyin, full_pinyin) {
        Ok(hits) => targets_from_hits(
            query,
            cancel,
            hits.into_iter().map(|hit| (hit.path, hit.dir, hit.size, hit.modified)),
        ),
        Err(why) => {
            tracing::debug!("Everything could not search: {why:?}");
            *lock(error) = Some(why);
            Vec::new()
        }
    }
}

/// The hits a search answered by another program (the indexer service, or
/// Everything) to the targets a pane lists. As `gather` sorts: the match
/// class and recency first, then the shorter name — a hit whose name is the
/// query itself outranks one with it buried.
fn targets_from_hits(
    query: &Query,
    cancel: &AtomicBool,
    hits: impl IntoIterator<Item = (String, bool, u64, u32)>,
) -> Vec<(String, String, bool, u64)> {
    let mut scored: Vec<(u32, usize, String, String, bool, u64)> = hits
        .into_iter()
        .filter(|_| !cancel.load(Ordering::Relaxed))
        .filter_map(|(path, dir, size, modified)| {
            let name = path.rsplit('\\').next().unwrap_or(path.as_str()).to_string();
            let score = query.score(&name, &path, dir, size, modified)?;
            Some((score, name.chars().count(), name, path, dir, size))
        })
        .collect();
    scored.sort_by(|left, right| (left.0, left.1, &left.3).cmp(&(right.0, right.1, &right.3)));
    // A literal pass and a pinyin pass can answer with the same name.
    let mut shown = std::collections::HashSet::new();
    scored
        .into_iter()
        .filter_map(|(_, _, name, path, dir, size)| {
            shown.insert(path.to_lowercase()).then_some((name, path, dir, size))
        })
        .collect()
}

/// What the settings page shows about the index.
pub(crate) struct Status {
    pub entries: Option<usize>,
    pub built: Option<SystemTime>,
    pub building: Option<usize>,
    /// The indexer service keeps and searches the names itself: this process
    /// holds none of them.
    pub service: bool,
    /// Where the names come from right now.
    pub source: Source,
    /// Everything's index, as the last probe found it.
    pub external: Option<Result<remote::Info, remote::Error>>,
}

/// The index's state, for display.
pub(crate) fn status(cx: &App) -> Status {
    let everything = cx.try_global::<Everything>();
    let source = everything.map_or(Source::Own, source_of);
    Status {
        entries: everything.and_then(|state| state.index.as_ref()).map(|index| index.len()),
        built: everything.and_then(|state| state.index.as_ref()).map(|index| index.built),
        building: everything
            .and_then(|state| state.building.as_ref())
            .map(|progress| progress.load(Ordering::Relaxed)),
        service: mft::searchable(),
        source,
        external: everything.and_then(|state| state.external.clone()),
    }
}

/// Reads the saved index on first use and walks the drives again when it is
/// missing or old. With Everything answering, nothing of that table belongs in
/// this process, and `auto` has to hear from Everything before it can choose.
pub(crate) fn ensure(cx: &mut App) {
    match source(cx) {
        Source::Everything => return,
        Source::Pending => {
            probe(cx);
            return;
        }
        Source::Own => {}
    }
    let state = cx.default_global::<Everything>();
    if state._watcher.is_none() {
        state._watcher = watch_drives(state.fresh.clone());
    }
    // The service keeps the names and searches them itself: nothing of the
    // table belongs in this process, which would be a gigabyte of it.
    if mft::searchable() {
        return;
    }
    if state.index.is_some() {
        let stale = state.index.as_ref().is_some_and(|index| index.stale());
        if stale && state.building.is_none() {
            rebuild(cx);
        }
        return;
    }
    if state.loading || state.building.is_some() {
        return;
    }
    state.loading = true;
    let load = cx.background_spawn(async { load(&index_file()) });
    let task = cx.spawn(async move |cx| {
        let loaded = load.await;
        cx.update(|cx| {
            let state = cx.default_global::<Everything>();
            state.loading = false;
            let stale = loaded.as_ref().map_or(true, Index::stale);
            if let Some(index) = loaded {
                state.index = Some(Arc::new(index));
                state.generation += 1;
            }
            if stale {
                rebuild(cx);
            }
        })
        .log_err();
    });
    cx.global_mut::<Everything>()._task = Some(task);
}

// What the walk thread reports.
enum Walked {
    // The table of the drive now being walked.
    Walking(Arc<Mutex<Segment>>),
    // Every drive finished so far.
    Drives(Vec<Arc<Segment>>),
    All(Index),
}

/// Walks the drives again now, keeping the current index until it is done.
/// With the service searching, the names are the service's to read again, and
/// this process asks it to.
pub(crate) fn rebuild(cx: &mut App) {
    match source(cx) {
        // Everything keeps its own index: there is nothing here to walk or
        // ask again, only the page's status line to refresh.
        Source::Everything | Source::Pending => {
            probe(cx);
            return;
        }
        Source::Own => {}
    }
    if mft::searchable() {
        let task = cx.background_spawn(async {
            match mft::refresh() {
                Ok(()) => tracing::info!("the indexer service will re-read the volumes"),
                Err(error) => tracing::warn!("the indexer service could not be asked to reindex: {error}"),
            }
        });
        cx.global_mut::<Everything>()._task = Some(task);
        return;
    }
    let state = cx.default_global::<Everything>();
    if state.building.is_some() {
        return;
    }
    let epoch = state.engine_epoch;
    let progress = Arc::new(AtomicUsize::new(0));
    state.building = Some(progress.clone());
    let previous = state.index.clone();
    // What the watcher saw before this walk, the walk sees too.
    let walk_started = SystemTime::now();
    let fresh = state.fresh.clone();
    let (sender, receiver) = async_channel::unbounded::<Walked>();
    let spawned = std::thread::Builder::new().name("nohrs-everything".into()).spawn({
        let progress = progress.clone();
        move || {
            let index = build(
                previous,
                progress,
                |table| {
                    sender.send_blocking(Walked::Walking(table)).log_err();
                },
                |segments| {
                    sender.send_blocking(Walked::Drives(segments)).log_err();
                },
            );
            if let Err(error) = save(&index, &index_file()) {
                tracing::warn!("could not save the everything index: {error}");
            }
            sender.send_blocking(Walked::All(index)).log_err();
        }
    });
    if let Err(error) = spawned {
        tracing::error!("could not start the everything walk: {error}");
        cx.global_mut::<Everything>().building = None;
        return;
    }
    let task = cx.spawn(async move |cx| {
        // Wakes the panes a few times a second so the count moves while the
        // walk runs; the walk itself never waits on the UI.
        let mut ticks = 0u32;
        loop {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            ticks += 1;
            // The choice changed while the walk ran: its table is not this
            // process's to keep any more.
            if !matches!(
                cx.update(|cx| cx.default_global::<Everything>().engine_epoch == epoch),
                Ok(true)
            ) {
                return;
            }
            match receiver.try_recv() {
                Ok(Walked::Walking(table)) => {
                    if cx.update(|cx| cx.global_mut::<Everything>().walking = Some(table)).is_err() {
                        return;
                    }
                }
                // A drive is done: searchable now, beside what the previous
                // index knew of the drives still being walked.
                Ok(Walked::Drives(done)) => {
                    let roots: Vec<String> = done.iter().map(|segment| segment.root().to_lowercase()).collect();
                    lock(&fresh).retain(|path, seen| {
                        seen >= walk_started || !roots.iter().any(|root| path.to_lowercase().starts_with(root.as_str()))
                    });
                    let updated = cx.update(|cx| {
                        let state = cx.global_mut::<Everything>();
                        let mut segments = done.clone();
                        if let Some(old) = &state.index {
                            segments.extend(
                                old.segments
                                    .iter()
                                    .filter(|segment| !done.iter().any(|new| new.root() == segment.root()))
                                    .cloned(),
                            );
                        }
                        let built = state.index.as_ref().map_or_else(SystemTime::now, |old| old.built);
                        state.index = Some(Arc::new(Index { segments, built }));
                        state.walking = None;
                        state.generation += 1;
                    });
                    if updated.is_err() {
                        return;
                    }
                }
                Ok(Walked::All(index)) => {
                    lock(&fresh).retain(|_, seen| seen >= walk_started);
                    cx.update(|cx| {
                        let state = cx.global_mut::<Everything>();
                        state.index = Some(Arc::new(index));
                        state.building = None;
                        state.walking = None;
                        state.generation += 1;
                    })
                    .log_err();
                    return;
                }
                Err(async_channel::TryRecvError::Closed) => {
                    cx.update(|cx| {
                        let state = cx.global_mut::<Everything>();
                        state.building = None;
                        state.walking = None;
                    })
                    .log_err();
                    return;
                }
                // Still walking: touching the global repaints the count. On
                // a first walk (no older table of the drive to answer from),
                // every few seconds a search also reruns, to take in what the
                // walk found since.
                Err(async_channel::TryRecvError::Empty) => {
                    let touched = cx.update(|cx| {
                        let state = cx.global_mut::<Everything>();
                        if ticks % 12 == 0 && state.walking_is_new() {
                            state.generation += 1;
                        }
                    });
                    if touched.is_err() {
                        return;
                    }
                }
            }
        }
    });
    cx.global_mut::<Everything>()._task = Some(task);
}

// ---------------------------------------------------------------------------
// The pane

/// The pane's side of the mode.
pub(crate) struct EverythingMode {
    // The query and index generation the hits on screen are for.
    last: Option<(String, u64)>,
    cancel: Arc<AtomicBool>,
    _subscription: Subscription,
}

impl ExplorerPane {
    /// Whether the bar searches the whole computer.
    pub(crate) fn everything_on(&self) -> bool {
        self.everything.is_some()
    }

    /// Turns the mode on or off. On, it leaves subfolder search and starts
    /// reading the index; off, the folder comes back.
    pub(crate) fn set_everything(&mut self, on: bool, cx: &mut Context<Self>) {
        if on == self.everything_on() {
            return;
        }
        if on {
            self.quick_recursive = false;
            self.name_filter.clear();
            self.end_subfolder_search();
            hold(cx);
            ensure(cx);
            // The global changes when the index arrives or the walk counts
            // more names: rerun or repaint.
            let subscription = cx.observe_global::<Everything>(|_pane, cx| cx.notify());
            self.everything = Some(EverythingMode {
                last: None,
                cancel: Arc::new(AtomicBool::new(false)),
                _subscription: subscription,
            });
        } else {
            if let Some(mode) = self.everything.take() {
                mode.cancel.store(true, Ordering::Relaxed);
            }
            self.end_subfolder_search();
            release(cx);
        }
        cx.notify();
    }

    /// Runs `query` against the index, unless the hits on screen already
    /// answer it. Called each render with what the bar holds.
    pub(crate) fn search_everything(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(mode) = self.everything.as_mut() else {
            return;
        };
        let state = cx.try_global::<Everything>();
        let generation = state.map_or(0, |state| state.generation);
        let source = state.map_or(Source::Own, source_of);
        let index = state.and_then(|state| state.index.clone());
        let walking = state.filter(|state| state.walking_is_new()).and_then(|state| state.walking.clone());
        let fresh = state.map(|state| state.fresh.clone());
        let built = index.as_ref().map(|index| index.built);
        let partial = index.as_ref().is_none_or(|index| index.segments.iter().any(|segment| !segment.complete));
        let key = (query.clone(), generation);
        if mode.last.as_ref() == Some(&key) {
            return;
        }
        if query.trim().is_empty() {
            mode.last = Some(key);
            mode.cancel.store(true, Ordering::Relaxed);
            self.end_subfolder_search();
            return;
        }
        // `auto` has not heard from Everything yet; its probe reruns this.
        if source == Source::Pending {
            return;
        }
        // The service holds the names itself: asking it costs a pipe message
        // and leaves this process holding nothing of the table. The index
        // here is only for a machine without the service.
        let by_service = source == Source::Own && mft::searchable();
        let by_everything = source == Source::Everything;
        if !by_everything && index.is_none() && walking.is_none() && !by_service {
            // Nothing to search yet; the walk reruns this.
            return;
        }
        let Some(parsed) = Query::parse(&query, &self.cwd) else {
            return;
        };
        // A name typed as pinyin initials is a Han name Everything only finds
        // when told to; the modifier it understands was worked out at the
        // probe. Only an all-Latin query is worth that pass.
        let pinyin = if parsed.pinyin {
            state
                .and_then(|state| state.external.as_ref())
                .and_then(|answer| answer.as_ref().ok())
                .and_then(|info| info.pinyin)
        } else {
            None
        };
        let full_pinyin = if parsed.pinyin {
            state
                .and_then(|state| state.external.as_ref())
                .and_then(|answer| answer.as_ref().ok())
                .and_then(|info| info.full_pinyin)
        } else {
            None
        };
        let filters = parsed.filters.join(" ");
        mode.last = Some(key);
        mode.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        mode.cancel = cancel.clone();

        let (sender, receiver) = async_channel::unbounded::<Vec<FileEntryDto>>();
        let took = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let index_for_thread = index.clone();
        let walking_for_thread = walking.clone();
        let fresh_for_thread = fresh.clone();
        let parsed_for_thread = parsed.clone();
        // Why Everything's list came back empty, for the bar to say.
        let external_error = Arc::new(Mutex::new(None));
        let external_error_for_thread = external_error.clone();
        let spawned = std::thread::Builder::new().name("nohrs-everything-search".into()).spawn({
            let took = took.clone();
            move || {
            let started = Instant::now();
            let targets = if by_everything {
                external_targets(
                    &parsed_for_thread,
                    &cancel,
                    &external_error_for_thread,
                    pinyin,
                    full_pinyin,
                )
            } else if by_service {
                service_targets(&parsed_for_thread, &cancel)
            } else {
                // The names here know no folder sizes: a folder shows none.
                gather(
                    index_for_thread.as_deref(),
                    walking_for_thread.as_deref(),
                    fresh_for_thread.as_deref(),
                    &parsed_for_thread,
                    &cancel,
                )
                .into_iter()
                .map(|(name, path, dir)| (name, path, dir, 0))
                .collect()
            };
            let elapsed = started.elapsed().as_millis() as u64;
            took.store(elapsed.max(1), Ordering::Relaxed);
            tracing::debug!(hits = targets.len(), elapsed_ms = elapsed, "everything search");
            let mut batch = Vec::new();
            let mut limit = FIRST_BATCH;
            let mut last_sent = Instant::now();
            for (name, path, dir, folder_size) in targets {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                if batch_ready(batch.len(), limit, last_sent.elapsed()) {
                    if sender.send_blocking(std::mem::take(&mut batch)).is_err() {
                        return;
                    }
                    limit = BATCH;
                    last_sent = Instant::now();
                }
                // Gone since the walk: not shown.
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                batch.push(FileEntryDto {
                    name,
                    path,
                    kind: if dir { "dir" } else { "file" }.to_string(),
                    // A folder found by the service knows what it holds.
                    size: if dir { folder_size } else { metadata.len() },
                    modified: metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map_or(0, |since| since.as_secs()),
                });
                if batch_ready(batch.len(), limit, last_sent.elapsed()) {
                    if sender.send_blocking(std::mem::take(&mut batch)).is_err() {
                        return;
                    }
                    limit = BATCH;
                    last_sent = Instant::now();
                }
            }
            if !batch.is_empty() {
                sender.send_blocking(batch).log_err();
            }
        }});
        if let Err(error) = spawned {
            tracing::error!("could not start the everything search: {error}");
            return;
        }
        // The hits on screen stay until the new ones come: typing never
        // blanks the list (Kensaku's rule), and most keystrokes narrow it.
        let task = cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let mut first = true;
            while let Ok(batch) = receiver.recv().await {
                if this.update(cx, |pane, cx| {
                    if std::mem::take(&mut first) {
                        pane.begin_search_hits();
                    }
                    pane.add_search_hits(batch);
                    cx.notify();
                })
                .is_err()
                {
                    return;
                }
            }
            let took = took.load(Ordering::Relaxed);
            // Why Everything's list is empty, when it was Everything that
            // could not answer.
            let why = lock(&external_error).take();
            this.update(cx, |pane, cx| {
                if first && took > 0 {
                    pane.begin_search_hits();
                }
                if let Some(search) = pane.deep_search.as_mut() {
                    search.running = false;
                    // What was understood and how fresh the answer is.
                    let mut detail: Vec<String> = Vec::new();
                    if !filters.is_empty() {
                        detail.push(filters);
                    }
                    if took > 0 {
                        detail.push(format!("{took} ms"));
                    }
                    if let Some(why) = &why {
                        detail.push(error_text(why));
                    }
                    let building = cx
                        .try_global::<Everything>()
                        .and_then(|state| state.building.as_ref())
                        .map(|progress| progress.load(Ordering::Relaxed));
                    match (building, built.and_then(|built| built.elapsed().ok())) {
                        // A first walk still under way: say so, not an age.
                        (Some(found), _) if partial => {
                            detail.push(nohrs_core::tf!("Indexando o computador… {n} itens", n = found));
                        }
                        (_, Some(age)) if !partial => {
                            detail.push(nohrs_core::tf!("índice de {age} atrás", age = short_age(age)));
                        }
                        _ => {}
                    }
                    search.detail = detail.join(" · ");
                }
                cx.notify();
            })
            .log_err();
        });
        self.deep_search = Some(DeepSearch::new(query, task));
        cx.notify();
    }

    /// The bar's status while the index is being read or walked, and why the
    /// list is empty when Everything could not answer.
    pub(crate) fn everything_status(&self, cx: &App) -> Option<String> {
        self.everything.as_ref()?;
        let state = cx.try_global::<Everything>()?;
        match source_of(state) {
            Source::Everything => match &state.external {
                Some(Ok(_)) => None,
                Some(Err(error)) => Some(error_text(error)),
                None => Some(nohrs_core::t!("Verificando o Everything…").to_string()),
            },
            Source::Pending => Some(nohrs_core::t!("Verificando o Everything…").to_string()),
            Source::Own => match (&state.index, &state.building) {
                (None, Some(progress)) => Some(nohrs_core::tf!(
                    "Indexando o computador… {n} itens",
                    n = progress.load(Ordering::Relaxed)
                )),
                (None, None) => Some(nohrs_core::t!("Lendo o índice…").to_string()),
                _ => None,
            },
        }
    }
}

// "3 min", "2 h", "4 d": how old the index is, in the bar.
fn short_age(age: Duration) -> String {
    let minutes = age.as_secs() / 60;
    if minutes < 1 {
        "< 1 min".to_string()
    } else if minutes < 60 {
        format!("{minutes} min")
    } else if minutes < 48 * 60 {
        format!("{} h", minutes / 60)
    } else {
        format!("{} d", minutes / (24 * 60))
    }
}

const WALKING: usize = usize::MAX;
const FRESH: usize = usize::MAX - 1;

fn batch_ready(count: usize, limit: usize, elapsed: Duration) -> bool {
    count > 0 && (count >= limit || elapsed >= RESULT_SLICE)
}

// The best hits of the index, of the drive being walked and of what the
// watcher saw since, merged and ranked, as (name, path, is a folder). The
// two live tables are read under their locks and let go before any disk is
// touched. With too few hits, the near-miss pass adds what has the letters
// in order.
fn gather(
    index: Option<&Index>,
    walking: Option<&Mutex<Segment>>,
    fresh: Option<&Mutex<Fresh>>,
    query: &Query,
    cancel: &Arc<AtomicBool>,
) -> Vec<(String, String, bool)> {
    let table = walking.map(lock);
    let fresh = fresh.map(lock);
    let mut sources: Vec<(usize, &Segment)> = Vec::new();
    if let Some(index) = index {
        sources.extend(index.segments.iter().enumerate().map(|(at, segment)| (at, &**segment)));
    }
    if let Some(table) = &table {
        sources.push((WALKING, &**table));
    }
    if let Some(fresh) = &fresh {
        sources.push((FRESH, &fresh.table));
    }
    let scan = |fuzzy: bool| {
        let mut hits = scan::search_index(index, query, fuzzy, cancel);
        for (source, segment) in sources.iter().filter(|(source, _)| *source == WALKING || *source == FRESH) {
            hits.extend(
                segment
                    .search(query, fuzzy, cancel)
                    .into_iter()
                    .map(|(rank, length, position)| (rank, length, *source, position)),
            );
        }
        hits
    };
    let mut hits = scan(false);
    if hits.len() < FUZZY_BELOW && query.fuzzy.is_some() {
        let mut near = scan(true);
        scan::best_prefix(&mut near, FUZZY_MOST);
        near.truncate(FUZZY_MOST);
        hits.extend(near);
    }
    scan::unique_targets(hits, &sources, cancel)
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod platform {
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;

    use windows::Win32::Storage::FileSystem::{
        FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, WIN32_FIND_DATAW,
    };
    use windows::core::{HSTRING, PCWSTR};

    // DRIVE_FIXED; the constant's type moved between windows-rs versions.
    const DRIVE_FIXED: u32 = 3;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
    // 100 ns ticks from 1601 to 1970.
    const EPOCH_TICKS: u64 = 116_444_736_000_000_000;

    /// Lists `folder` the way the walk needs it. Unlike `std::fs::read_dir`
    /// it asks for large fetches (`FIND_FIRST_EX_LARGE_FETCH`: fewer trips
    /// to the file system per folder, which on a cold disk is most of the
    /// cost) and takes size, date and attributes from the listing itself,
    /// with no path built per entry.
    pub fn list(folder: &Path, out: &mut Vec<super::Listed>) {
        let mut pattern: Vec<u16> = Vec::new();
        // Past MAX_PATH only the verbatim form opens.
        let text = folder.as_os_str();
        if text.len() > 240 && !text.to_string_lossy().starts_with("\\\\") {
            pattern.extend("\\\\?\\".encode_utf16());
        }
        pattern.extend(text.encode_wide());
        if pattern.last() != Some(&u16::from(b'\\')) {
            pattern.push(u16::from(b'\\'));
        }
        pattern.push(u16::from(b'*'));
        pattern.push(0);
        let mut data = WIN32_FIND_DATAW::default();
        let handle = unsafe {
            FindFirstFileExW(
                PCWSTR(pattern.as_ptr()),
                FindExInfoBasic,
                std::ptr::from_mut(&mut data).cast(),
                FindExSearchNameMatch,
                None,
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        let Ok(handle) = handle else {
            return;
        };
        loop {
            let length = data.cFileName.iter().position(|&unit| unit == 0).unwrap_or(data.cFileName.len());
            let name = &data.cFileName[..length];
            let dot = name == [u16::from(b'.')] || name == [u16::from(b'.'), u16::from(b'.')];
            if !dot {
                let attributes = data.dwFileAttributes;
                let dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                let tag = data.dwReserved0;
                let ticks = (u64::from(data.ftLastWriteTime.dwHighDateTime) << 32)
                    | u64::from(data.ftLastWriteTime.dwLowDateTime);
                let seconds = ticks.saturating_sub(EPOCH_TICKS) / 10_000_000;
                out.push(super::Listed {
                    name: String::from_utf16_lossy(name),
                    found: super::Found {
                        dir,
                        size: if dir { 0 } else { (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow) },
                        modified: if ticks == 0 { 0 } else { super::clamp_seconds(seconds as i64) },
                    },
                    link: attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                        && (tag == IO_REPARSE_TAG_SYMLINK || tag == IO_REPARSE_TAG_MOUNT_POINT),
                    cloud: super::in_the_cloud(attributes),
                });
            }
            if unsafe { FindNextFileW(handle, &mut data) }.is_err() {
                break;
            }
        }
        if let Err(error) = unsafe { FindClose(handle) } {
            tracing::debug!("FindClose failed: {error}");
        }
    }

    /// `C:\`, `D:\`… for every fixed drive but cloud drives (Google Drive
    /// mounts one), whose every folder read is a trip to the network.
    pub fn fixed_drives() -> Vec<String> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| format!("{}:\\", char::from(b'A' + bit)))
            .filter(|root| {
                let wide = HSTRING::from(root.as_str());
                if unsafe { GetDriveTypeW(&wide) } != DRIVE_FIXED {
                    return false;
                }
                let mut label = [0u16; 261];
                let named = unsafe { GetVolumeInformationW(&wide, Some(&mut label), None, None, None, None) }.is_ok();
                let label = String::from_utf16_lossy(&label);
                !(named && label.trim_end_matches('\0').to_lowercase().contains("google drive"))
            })
            .collect()
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    pub use super::list_std as list;

    pub fn fixed_drives() -> Vec<String> {
        vec!["/".to_string()]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn auto_waits_for_everything_then_falls_back_to_the_own_table() {
        let mut state = Everything::default();
        assert!(matches!(source_of(&state), Source::Pending));
        state.external = Some(Err(remote::Error::NotRunning));
        assert!(matches!(source_of(&state), Source::Own));
        state.external = Some(Ok(remote::Info {
            entries: 10,
            version: String::new(),
            pinyin: None,
            full_pinyin: None,
        }));
        assert!(matches!(source_of(&state), Source::Everything));
        // An explicit choice does not depend on the probe.
        state.engine = IndexSource::PrimeFiles;
        assert!(matches!(source_of(&state), Source::Own));
        state.engine = IndexSource::Everything;
        assert!(matches!(source_of(&state), Source::Everything));
    }

    fn sample() -> Index {
        let mut other = Segment::empty();
        let d = other.push(NO_PARENT, "D:", true);
        other.push(d, "kickstarter.txt", false);
        let mut builder = Segment::empty();
        let drive = builder.push(NO_PARENT, "C:", true);
        let music = builder.push(drive, "Música", true);
        builder.push(music, "Kick 01.WAV", false);
        builder.push(music, "kick", false);
        let projects = builder.push(drive, "Projetos", true);
        builder.push(projects, "kick_final.mp3", false);
        builder.complete = true;
        other.complete = true;
        Index { segments: vec![Arc::new(builder), Arc::new(other)], built: SystemTime::now() }
    }

    fn names(index: &Index, query: &str) -> Vec<String> {
        let query = Query::parse(query, "C:\\Projetos").unwrap();
        gather(Some(index), None, None, &query, &Arc::new(AtomicBool::new(false)))
            .into_iter()
            .map(|(_, path, _)| path)
            .collect()
    }

    #[test]
    fn exact_names_come_first_and_case_does_not_matter() {
        let index = sample();
        assert_eq!(
            names(&index, "KICK"),
            ["C:\\Música\\kick", "C:\\Música\\Kick 01.WAV", "C:\\Projetos\\kick_final.mp3", "D:\\kickstarter.txt"]
        );
    }

    #[test]
    fn every_term_must_match_and_slashes_match_the_path() {
        let index = sample();
        assert_eq!(names(&index, "kick projetos\\"), ["C:\\Projetos\\kick_final.mp3"]);
        assert_eq!(names(&index, "*.wav"), ["C:\\Música\\Kick 01.WAV"]);
        assert_eq!(names(&index, "música"), ["C:\\Música"]);
    }

    #[test]
    fn the_saved_table_reads_back_the_same() {
        let index = sample();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("everything.idx");
        save(&index, &path).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.len(), index.len());
        assert_eq!(loaded.segments.len(), 2);
        assert_eq!(names(&loaded, "kick"), names(&index, "kick"));
        // A cut file is refused instead of read wrong.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
        assert!(load(&path).is_none());
    }

    #[test]
    fn a_drive_being_walked_is_searched_beside_the_finished_ones() {
        let index = sample();
        let mut walking = Segment::empty();
        let e = walking.push(NO_PARENT, "E:", true);
        walking.push(e, "Kick.wav", false);
        let walking = Mutex::new(walking);
        let query = Query::parse("kick", "").unwrap();
        let found = gather(Some(&index), Some(&walking), None, &query, &Arc::new(AtomicBool::new(false)));
        let paths: Vec<&str> = found.iter().map(|(_, path, _)| path.as_str()).collect();
        assert_eq!(paths.first(), Some(&"C:\\Música\\kick"));
        assert!(paths.contains(&"E:\\Kick.wav"));
        assert_eq!(paths.len(), 5);
    }

    #[test]
    fn the_root_walks_users_first_and_windows_last() {
        let mut children = vec![
            (PathBuf::from("C:\\Users"), 1),
            (PathBuf::from("C:\\Windows"), 2),
            (PathBuf::from("C:\\Program Files"), 3),
            (PathBuf::from("C:\\Projetos"), 4),
        ];
        children.sort_by_key(|(path, _)| walk_priority(path));
        // Taken from the end: Windows last of all, Users first.
        let order: Vec<u32> = children.iter().rev().map(|(_, id)| *id).collect();
        assert_eq!(order, [1, 4, 3, 2]);
    }

    // 2026-11-05 12:00 UTC, the day these were written.
    const NOW: i64 = 1_793_880_000;

    fn now() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap()
    }

    fn found(dir: bool, size: u64, days_old: i64) -> Found {
        Found { dir, size, modified: clamp_seconds(NOW - days_old * DAY) }
    }

    // A drive with sizes and dates, a system folder among the person's own.
    fn detailed() -> Index {
        let mut table = Segment::empty();
        let drive = table.add(NO_PARENT, "C:", found(true, 0, 400));
        let users = table.add(drive, "Users", found(true, 0, 400));
        let sfx = table.add(users, "SFX", found(true, 0, 400));
        table.add(sfx, "Bonk.wav", found(false, 300 * KB, 2));
        table.add(sfx, "Big Bonk.wav", found(false, 40 * MB, 200));
        table.add(sfx, "bonk notes.md", found(false, 2 * KB, 30));
        let windows = table.add(drive, "Windows", found(true, 0, 400));
        let media = table.add(windows, "Media", found(true, 0, 400));
        table.add(media, "bonk.wav", found(false, 300 * KB, 2));
        table.complete = true;
        Index { segments: vec![Arc::new(table)], built: SystemTime::now() }
    }

    fn search(index: &Index, query: &str) -> Vec<String> {
        let query = Query::parse_at(query, "C:\\Users\\SFX", now()).unwrap();
        gather(Some(index), None, None, &query, &Arc::new(AtomicBool::new(false)))
            .into_iter()
            .map(|(_, path, _)| path)
            .collect()
    }

    #[test]
    fn the_persons_files_rank_before_the_systems_and_recent_before_old() {
        let index = detailed();
        assert_eq!(
            search(&index, "bonk"),
            [
                // Prefix matches, the week's first, the system's after the
                // person's; then the word inside a name.
                "C:\\Users\\SFX\\Bonk.wav",
                "C:\\Users\\SFX\\bonk notes.md",
                "C:\\Windows\\Media\\bonk.wav",
                "C:\\Users\\SFX\\Big Bonk.wav",
            ]
        );
    }

    // Windows' zh-CN collation supplies the initials; elsewhere the names are
    // matched by their own text alone.
    #[cfg(windows)]
    #[test]
    fn a_name_written_in_han_characters_is_found_by_its_pinyin_initials() {
        let mut table = Segment::empty();
        let drive = table.add(NO_PARENT, "C:", found(true, 0, 400));
        table.add(drive, "文件夹", found(true, 0, 400));
        table.add(drive, "工作报告.docx", found(false, 2 * KB, 9));
        table.add(drive, "notes.txt", found(false, 1 * KB, 9));
        let folder = table.add(drive, "项目A", found(true, 0, 400));
        table.add(folder, "照片.png", found(false, 4 * KB, 9));
        table.complete = true;
        let index = Index { segments: vec![Arc::new(table)], built: SystemTime::now() };

        // Initials, whole and partial, as typed on an English keyboard.
        assert_eq!(search(&index, "wjj"), ["C:\\文件夹"]);
        assert_eq!(search(&index, "gzbg"), ["C:\\工作报告.docx"]);
        assert_eq!(search(&index, "zp"), ["C:\\项目A\\照片.png"]);
        // The name itself still matches, and Latin names are unaffected.
        assert_eq!(search(&index, "报告"), ["C:\\工作报告.docx"]);
        assert_eq!(search(&index, "notes"), ["C:\\notes.txt"]);
        // Letters the initials do not spell find nothing.
        assert!(search(&index, "qqq").is_empty());
        // A wildcard covers the name itself, never its initials.
        assert!(search(&index, "*.png").len() == 1);
    }

    // Everything answers "wjj" with 文件夹; the filter that keeps what another
    // program found must accept those, not only a name holding the letters.
    #[cfg(windows)]
    #[test]
    fn a_hit_from_another_program_is_kept_by_its_pinyin_initials() {
        let query = Query::parse_at("wjj", "", now()).unwrap();
        assert!(query.score("文件夹", "C:\\文件夹", true, 0, clamp_seconds(NOW)).is_some());
        // A Latin name still has to hold the letters itself.
        assert!(query.score("notes.txt", "C:\\notes.txt", false, 1, clamp_seconds(NOW)).is_none());
        // A Han name whose initials are something else is not kept.
        assert!(query.score("工作报告.docx", "C:\\工作报告.docx", false, 1, clamp_seconds(NOW)).is_none());
    }

    #[test]
    fn filters_narrow_by_extension_kind_size_date_and_folder() {
        let index = detailed();
        assert_eq!(search(&index, "bonk ext:md"), ["C:\\Users\\SFX\\bonk notes.md"]);
        assert_eq!(search(&index, "tipo:som tamanho:>10mb"), ["C:\\Users\\SFX\\Big Bonk.wav"]);
        assert_eq!(search(&index, "bonk data:<7d em:."), ["C:\\Users\\SFX\\Bonk.wav"]);
        assert_eq!(search(&index, "pasta: s"), ["C:\\Users\\SFX", "C:\\Users", "C:\\Windows"]);
        assert_eq!(search(&index, "wav -big em:users"), ["C:\\Users\\SFX\\Bonk.wav"]);
        assert_eq!(search(&index, "\"big bonk\""), ["C:\\Users\\SFX\\Big Bonk.wav"]);
        assert_eq!(search(&index, "bonk -ext:wav"), ["C:\\Users\\SFX\\bonk notes.md"]);
    }

    #[test]
    fn a_filter_still_being_typed_is_left_out_and_an_unknown_key_is_text() {
        let index = detailed();
        assert_eq!(search(&index, "notes tamanho:"), ["C:\\Users\\SFX\\bonk notes.md"]);
        assert!(Query::parse_at("tamanho:", "", now()).is_none());
        assert!(search(&index, "foo:bonk").is_empty());
    }

    #[test]
    fn too_few_hits_bring_in_the_letters_in_order() {
        let index = detailed();
        assert_eq!(search(&index, "bnknotes"), ["C:\\Users\\SFX\\bonk notes.md"]);
        assert!(subsequence("bonk", "bomnk.wav"));
        assert!(!subsequence("bonk", "boom - robo night k.mp3"));
    }

    #[test]
    fn sizes_and_dates_are_read_the_way_people_write_them() {
        assert_eq!(size_range(">10mb"), Some((10 * MB + 1, u64::MAX)));
        assert_eq!(size_range("1,5gb..2gb"), Some(((1.5 * GB as f64) as u64, 2 * GB)));
        assert_eq!(size_range("vazio"), Some((0, 0)));
        assert_eq!(size_range("10"), Some((10, u64::MAX)));
        let utc = time::UtcOffset::UTC;
        let (start, end) = period("05/11/2026", utc).unwrap();
        assert_eq!(end - start, DAY - 1);
        assert_eq!(period("2026-11-05", utc), Some((start, end)));
        let (from, to) = date_range("2026", now()).unwrap();
        assert!(from < clamp_seconds(NOW) && clamp_seconds(NOW) < to);
        assert_eq!(date_range(">2a", now()), Some((0, clamp_seconds(NOW - 2 * 365 * DAY))));
        assert!(date_range("hoje", now()).is_some_and(|(from, _)| from <= clamp_seconds(NOW)));
    }

    #[test]
    fn sizes_and_dates_survive_saving_and_old_tables_still_read() {
        let index = detailed();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("everything.idx");
        save(&index, &path).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(search(&loaded, "tipo:som tamanho:>10mb"), ["C:\\Users\\SFX\\Big Bonk.wav"]);
        assert_eq!(search(&loaded, "bonk"), search(&index, "bonk"));

        // The same table as 0.3.1 wrote it: no size or date per entry.
        let segment = &index.segments[0];
        let mut old = Vec::new();
        old.extend_from_slice(FILE_MAGIC_V2);
        old.extend_from_slice(&0u64.to_le_bytes());
        old.extend_from_slice(&1u64.to_le_bytes());
        old.extend_from_slice(&(segment.entries.len() as u64).to_le_bytes());
        old.extend_from_slice(&(segment.names.len() as u64).to_le_bytes());
        old.extend_from_slice(&(segment.lower.len() as u64).to_le_bytes());
        for entry in &segment.entries {
            old.extend_from_slice(&entry.parent.to_le_bytes());
            old.extend_from_slice(&entry.name.to_le_bytes());
            old.extend_from_slice(&entry.lower.to_le_bytes());
            old.extend_from_slice(&entry.name_len.to_le_bytes());
            old.extend_from_slice(&entry.lower_len.to_le_bytes());
            old.push(entry.flags & DIR);
        }
        old.extend_from_slice(segment.names.as_bytes());
        old.extend_from_slice(segment.lower.as_bytes());
        std::fs::write(&path, old).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(search(&loaded, "bonk").len(), 4);
        assert!(search(&loaded, "bonk tamanho:>1kb").is_empty());
    }

    #[test]
    fn what_the_watcher_saw_is_found_once() {
        let index = detailed();
        let fresh = Mutex::new(Fresh::default());
        lock(&fresh).record("C:\\Users\\SFX\\Bonk Remix.wav".into(), found(false, KB, 0));
        lock(&fresh).record("C:\\Users\\SFX\\Bonk.wav".into(), found(false, KB, 0));
        let query = Query::parse_at("bonk remix", "", now()).unwrap();
        let hits = gather(Some(&index), None, Some(&fresh), &query, &Arc::new(AtomicBool::new(false)));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "C:\\Users\\SFX\\Bonk Remix.wav");
        let query = Query::parse_at("bonk ext:wav", "", now()).unwrap();
        let hits = gather(Some(&index), None, Some(&fresh), &query, &Arc::new(AtomicBool::new(false)));
        assert_eq!(hits.iter().filter(|(_, path, _)| path == "C:\\Users\\SFX\\Bonk.wav").count(), 1);
        lock(&fresh).retain(|path, _| !path.ends_with("Remix.wav"));
        assert_eq!(lock(&fresh).table.paths, ["C:\\Users\\SFX\\Bonk.wav"]);
        assert!(churn("c:\\users\\a\\appdata\\local\\temp\\x.wav"));
        assert!(!churn("c:\\users\\a\\desktop\\x.wav"));
    }

    // cargo test -p nohrs-pages --release letter_mask_speed -- --ignored --nocapture
    #[test]
    #[ignore]
    fn letter_mask_speed() {
        let words = ["render", "final", "bonk", "projeto", "audio", "cena", "take", "export", "img", "backup", "v2"];
        let mut table = Segment::empty();
        let drive = table.add(NO_PARENT, "C:", found(true, 0, 1));
        let mut seed = 7u64;
        for index in 0..2_000_000u32 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let first = words[(seed >> 33) as usize % words.len()];
            let second = words[(seed >> 45) as usize % words.len()];
            table.add(drive, &format!("{first}_{second}_{index}.dat"), found(false, 1, 1));
        }
        let index = Index { segments: vec![Arc::new(table)], built: SystemTime::now() };
        for text in ["zq", "bonk render", "xyz"] {
            let mut query = Query::parse_at(text, "", now()).unwrap();
            query.fuzzy = None;
            let started = Instant::now();
            let with = gather(Some(&index), None, None, &query, &Arc::new(AtomicBool::new(false))).len();
            let masked = started.elapsed();
            query.mask = 0;
            let started = Instant::now();
            let without = gather(Some(&index), None, None, &query, &Arc::new(AtomicBool::new(false))).len();
            let plain = started.elapsed();
            assert_eq!(with, without);
            println!("{text:>12}: {with} hits, with mask {masked:?}, without {plain:?}");
        }
    }

    // NOHRS_WALK_ROOT=C:\\ cargo test -p nohrs-pages --release walk_speed -- --ignored --nocapture
    // Each run reads the same folders, so after the first they come from
    // the system's cache: this compares the listing's own cost, not the disk's.
    #[test]
    #[ignore]
    fn walk_speed() {
        let root = std::env::var("NOHRS_WALK_ROOT").unwrap_or_else(|_| "C:\\Users".into());
        let runs: [(&str, fn(&Path, &mut Vec<Listed>), usize); 5] = [
            ("warm-up", platform::list, 32),
            ("std, 12", list_std, 12),
            ("large fetch, 12", platform::list, 12),
            ("large fetch, 32", platform::list, 32),
            ("large fetch, 64", platform::list, 64),
        ];
        for (label, list, workers) in runs {
            let started = Instant::now();
            let table = Arc::new(Mutex::new(Segment::empty()));
            let segment = walk_drive(&root, table, Arc::new(AtomicUsize::new(0)), workers, list, &mut |_| {});
            let elapsed = started.elapsed();
            println!(
                "{label:>16}: {} names in {elapsed:?} ({:.0} k/s)",
                segment.entries.len(),
                segment.entries.len() as f64 / elapsed.as_secs_f64() / 1000.0
            );
        }
    }

    #[test]
    fn the_new_listing_reads_what_the_standard_one_does() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.wav"), [0u8; 1234]).unwrap();
        std::fs::create_dir(dir.path().join("pasta com espaço")).unwrap();
        std::fs::write(dir.path().join("pasta com espaço").join("ção.txt"), b"x").unwrap();
        let read = |list: fn(&Path, &mut Vec<Listed>)| {
            let mut out = Vec::new();
            list(dir.path(), &mut out);
            let mut seen: Vec<(String, bool, u64, bool)> =
                out.into_iter().map(|entry| (entry.name, entry.found.dir, entry.found.size, entry.found.modified > 0)).collect();
            seen.sort();
            seen
        };
        assert_eq!(read(platform::list), read(list_std));
        assert_eq!(read(platform::list), [("a.wav".to_string(), false, 1234, true), ("pasta com espaço".to_string(), true, 0, true)]);
    }

    #[test]
    fn a_walk_cut_short_is_kept_searchable_and_walked_again() {
        let whole = detailed();
        let mut partial = Segment::empty();
        let drive = partial.add(NO_PARENT, "D:", found(true, 0, 1));
        partial.add(drive, "Bonk Remix.wav", found(false, KB, 1));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("everything.idx");
        save_segments(SystemTime::UNIX_EPOCH, &[&*whole.segments[0], &partial], &path).unwrap();
        let loaded = load(&path).unwrap();
        assert!(loaded.segments[0].complete);
        assert!(!loaded.segments[1].complete);
        assert!(loaded.stale());
        assert!(search(&loaded, "remix").contains(&"D:\\Bonk Remix.wav".to_string()));
        assert!(!whole.stale());
    }

    #[test]
    fn highlighting_skips_the_filters() {
        assert_eq!(highlight_term("bonk ext:wav tamanho:>1mb"), "bonk");
        assert_eq!(highlight_term("\"big bonk\" -remix"), "big bonk");
    }

    #[test]
    fn wildcards_cover_the_whole_name() {
        let pattern: Vec<char> = "k?ck*.wav".chars().collect();
        assert!(wildcard(&pattern, "kick 01.wav"));
        assert!(!wildcard(&pattern, "kick 01.wav.bak"));
        assert!(wildcard(&"*".chars().collect::<Vec<_>>(), ""));
    }
}
