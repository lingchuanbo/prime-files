//! What the service and the app say over the pipe.
//!
//! The app writes one request line; the service answers and closes.
//!
//! ```text
//! request   "PFIX1 HELLO\n" | "PFIX1 SNAPSHOT\n" | "PFIX1 SEARCH\n"
//! hello     "PFIX1 OK <version>\n"
//! snapshot  "PFIX" u32 version, u32 volumes, then per volume:
//!             u16 root length, root (UTF-8, "C:"), u64 entries, then per entry:
//!               u32 parent (u32::MAX for the root, the first entry)
//!               u64 size, u32 last write (seconds since 1970), u8 flags (1 = folder)
//!               u16 name length, name (UTF-8)
//! search    "PFIX1 SEARCH\n" then, as a body:
//!             u16 term count, then per term: u16 length, term (UTF-8)
//!             u32 hits wanted
//!           answered by "PFIS" u32 version, u32 count, then per hit:
//!             u8 flags (1 = folder), u64 size, u32 last write,
//!             u16 path length, whole path (UTF-8, "C:\Users\…")
//! error     "PFER" u16 length, message (UTF-8)
//! ```
//!
//! A snapshot lists each volume's names in tree order: every entry's parent
//! comes before it, so the app appends them to its own table as they arrive.
//! A search asks the service's own copy of those names instead, so the app
//! never holds the fourteen million of them: what comes back is the few
//! thousand paths that matched.
//!
//! Numbers are little-endian. Changing any of this changes [`VERSION`], and
//! the round-trip test below with it (CLAUDE.md, rule 7).

use std::io::{self, Read, Write};

#[path = "protocol/search.rs"]
mod search;

#[cfg(test)]
#[path = "protocol/search_tests.rs"]
mod search_tests;

/// The protocol's version: 2 added the search request.
pub const VERSION: u32 = 2;
/// Asks for every volume's names.
pub const SNAPSHOT: &[u8] = b"PFIX1 SNAPSHOT\n";
/// Asks which version the service speaks.
pub const HELLO: &[u8] = b"PFIX1 HELLO\n";
/// Asks for the names holding the terms; the body follows on the next lines.
pub const SEARCH: &[u8] = b"PFIX1 SEARCH\n";
/// Schedules a background rebuild, keeping the current search snapshot.
pub const REFRESH: &[u8] = b"PFIX1 REFRESH\n";
/// The root's parent.
pub const NO_PARENT: u32 = u32::MAX;
/// The most hits one search may ask for.
pub const MOST_HITS: u32 = 50_000;

const MAGIC: &[u8; 4] = b"PFIX";
const HITS: &[u8; 4] = b"PFIS";
const ERROR: &[u8; 4] = b"PFER";
const FOLDER: u8 = 1;
// No real name is longer (255 UTF-16 units, at most 765 bytes of UTF-8).
const LONGEST_NAME: usize = 1024;
// A whole path, in UTF-8 bytes: well past the 260 a Windows path usually is.
const LONGEST_PATH: usize = 32_768;
const MOST_ENTRIES: u64 = 200_000_000;

/// One volume's names, in tree order.
#[derive(Default)]
pub struct Volume {
    /// "C:".
    pub root: String,
    names: String,
    // The names lowercased, apart: a search compares against these, so it is
    // a plain substring test rather than a fold per byte.
    lower: String,
    entries: Vec<Entry>,
}

#[derive(Clone, Copy)]
struct Entry {
    parent: u32,
    start: u32,
    len: u16,
    lower_start: u32,
    lower_len: u16,
    dir: bool,
    size: u64,
    modified: u32,
}

impl Volume {
    /// A volume whose first entry, the root, is named `root`.
    pub fn new(root: &str) -> Self {
        let mut volume = Self { root: root.to_string(), ..Self::default() };
        volume.push(NO_PARENT, root, true, 0, 0);
        volume
    }

    /// Adds an entry under `parent` (an index this returned before) and
    /// returns its own index.
    pub fn push(&mut self, parent: u32, name: &str, dir: bool, size: u64, modified: u32) -> u32 {
        let end = clip(name);
        let name = &name[..end];
        let lower = name.to_lowercase();
        let lower_end = clip(&lower);
        self.entries.push(Entry {
            parent,
            start: self.names.len() as u32,
            len: name.len() as u16,
            lower_start: self.lower.len() as u32,
            lower_len: lower_end as u16,
            dir,
            size,
            modified,
        });
        self.names.push_str(name);
        self.lower.push_str(&lower[..lower_end]);
        (self.entries.len() - 1) as u32
    }

    /// Fills every folder's size with what it holds, the folders inside it
    /// included: a search hit can then show how big a folder is without
    /// opening it. The entries are in tree order — a parent before its
    /// children — so one pass from the end adds each size to its parent.
    pub fn sum_folder_sizes(&mut self) {
        let mut sizes: Vec<u64> = self.entries.iter().map(|entry| if entry.dir { 0 } else { entry.size }).collect();
        for index in (0..self.entries.len()).rev() {
            let parent = self.entries[index].parent;
            if parent != NO_PARENT
                && let Some(total) = sizes.get(parent as usize).copied()
            {
                sizes[parent as usize] = total.saturating_add(sizes[index]);
            }
        }
        for (entry, size) in self.entries.iter_mut().zip(sizes) {
            if entry.dir {
                entry.size = size;
            }
        }
    }

    /// How many entries it holds, the root included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it holds only nothing (not even a root).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn name_of(&self, entry: &Entry) -> &str {
        self.names.get(entry.start as usize..entry.start as usize + entry.len as usize).unwrap_or("")
    }

    fn lower_of(&self, entry: &Entry) -> &str {
        self.lower.get(entry.lower_start as usize..entry.lower_start as usize + entry.lower_len as usize).unwrap_or("")
    }

    /// The whole path of entry `index`: its name and every parent's, from the
    /// drive down.
    fn path(&self, index: u32) -> String {
        let mut parts = Vec::new();
        let mut current = index;
        while current != NO_PARENT && parts.len() <= self.entries.len() {
            let Some(entry) = self.entries.get(current as usize) else {
                break;
            };
            parts.push(self.name_of(entry));
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

// How many bytes of `text` fit in an entry's name: clipped at a character
// boundary, and inside what the format carries.
fn clip(text: &str) -> usize {
    let mut end = text.len().min(LONGEST_NAME);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Receives a snapshot as it is read.
pub trait Sink {
    /// A volume starts; its entries follow.
    fn volume(&mut self, root: &str);
    /// One entry of the current volume, numbered from 0 in arrival order;
    /// `parent` is an earlier number, or [`NO_PARENT`] for the root.
    fn entry(&mut self, parent: u32, name: &str, dir: bool, size: u64, modified: u32);
}

/// Writes a snapshot of `volumes`.
pub fn write(out: &mut impl Write, volumes: &[Volume]) -> io::Result<()> {
    write_volumes(out, volumes.iter(), volumes.len())
}

#[cfg(windows)]
pub(crate) fn write_shared(out: &mut impl Write, volumes: &[std::sync::Arc<Volume>]) -> io::Result<()> {
    write_volumes(out, volumes.iter().map(std::ops::Deref::deref), volumes.len())
}

fn write_volumes<'a>(out: &mut impl Write, volumes: impl Iterator<Item = &'a Volume>, count: usize) -> io::Result<()> {
    out.write_all(MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&(count as u32).to_le_bytes())?;
    for volume in volumes {
        out.write_all(&(volume.root.len() as u16).to_le_bytes())?;
        out.write_all(volume.root.as_bytes())?;
        out.write_all(&(volume.entries.len() as u64).to_le_bytes())?;
        for entry in &volume.entries {
            let name = volume.names.get(entry.start as usize..entry.start as usize + entry.len as usize).unwrap_or("");
            out.write_all(&entry.parent.to_le_bytes())?;
            out.write_all(&entry.size.to_le_bytes())?;
            out.write_all(&entry.modified.to_le_bytes())?;
            out.write_all(&[if entry.dir { FOLDER } else { 0 }])?;
            out.write_all(&(name.len() as u16).to_le_bytes())?;
            out.write_all(name.as_bytes())?;
        }
    }
    out.flush()
}

/// Writes an error answer.
pub fn write_error(out: &mut impl Write, message: &str) -> io::Result<()> {
    let message = &message.as_bytes()[..message.len().min(4096)];
    out.write_all(ERROR)?;
    out.write_all(&(message.len() as u16).to_le_bytes())?;
    out.write_all(message)?;
    out.flush()
}

/// One path that matched a search, and what the listing needs of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The whole path, "C:\Users\Bobo\nota.txt".
    pub path: String,
    /// Whether it is a folder.
    pub dir: bool,
    /// Bytes; folders carry 0.
    pub size: u64,
    /// Last write, seconds since 1970.
    pub modified: u32,
}

/// Writes a search request: the terms every name must hold, and how many
/// hits are wanted.
pub fn write_search_request(out: &mut impl Write, terms: &[String], limit: u32) -> io::Result<()> {
    out.write_all(SEARCH)?;
    out.write_all(&(terms.len().min(u16::MAX as usize) as u16).to_le_bytes())?;
    for term in terms.iter().take(u16::MAX as usize) {
        let bytes = term.as_bytes();
        let bytes = &bytes[..bytes.len().min(u16::MAX as usize)];
        out.write_all(&(bytes.len() as u16).to_le_bytes())?;
        out.write_all(bytes)?;
    }
    out.write_all(&limit.min(MOST_HITS).to_le_bytes())?;
    out.flush()
}

/// Reads a search request's terms and wanted hits.
pub fn read_search_request(input: &mut impl Read) -> io::Result<(Vec<String>, u32)> {
    let count = u16::from_le_bytes(read_array(input)?) as usize;
    let mut terms = Vec::with_capacity(count.min(64));
    let mut buffer = Vec::new();
    for _ in 0..count {
        let length = u16::from_le_bytes(read_array(input)?) as usize;
        if length > LONGEST_NAME {
            return Err(invalid("search term too long"));
        }
        read_text(input, length, &mut buffer)?;
        terms.push(std::str::from_utf8(&buffer).map_err(|_| invalid("term is not UTF-8"))?.to_lowercase());
    }
    let limit = u32::from_le_bytes(read_array(input)?).min(MOST_HITS);
    Ok((terms, limit))
}

/// Writes what a search found.
pub fn write_hits(out: &mut impl Write, hits: &[Hit]) -> io::Result<()> {
    out.write_all(HITS)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&(hits.len() as u32).to_le_bytes())?;
    for hit in hits {
        let path = hit.path.as_bytes();
        let path = &path[..path.len().min(LONGEST_PATH)];
        out.write_all(&[if hit.dir { FOLDER } else { 0 }])?;
        out.write_all(&hit.size.to_le_bytes())?;
        out.write_all(&hit.modified.to_le_bytes())?;
        out.write_all(&(path.len() as u16).to_le_bytes())?;
        out.write_all(path)?;
    }
    out.flush()
}

/// Reads what a search answered. An error answer comes back as `Err`.
pub fn read_hits(input: &mut impl Read) -> io::Result<Vec<Hit>> {
    let magic = read_array::<4>(input)?;
    if &magic == ERROR {
        let length = u16::from_le_bytes(read_array(input)?) as usize;
        let mut message = Vec::new();
        read_text(input, length, &mut message)?;
        return Err(io::Error::other(String::from_utf8_lossy(&message).into_owned()));
    }
    if &magic != HITS {
        return Err(invalid("not a search answer"));
    }
    let version = u32::from_le_bytes(read_array(input)?);
    if version != VERSION {
        return Err(invalid(&format!("version {version}, this app reads {VERSION}")));
    }
    let count = u32::from_le_bytes(read_array(input)?);
    if count > MOST_HITS {
        return Err(invalid("too many hits"));
    }
    let mut hits = Vec::with_capacity(count.min(4096) as usize);
    let mut buffer = Vec::new();
    for _ in 0..count {
        let flags = read_array::<1>(input)?[0];
        let size = u64::from_le_bytes(read_array(input)?);
        let modified = u32::from_le_bytes(read_array(input)?);
        let length = u16::from_le_bytes(read_array(input)?) as usize;
        if length > LONGEST_PATH {
            return Err(invalid("path too long"));
        }
        read_text(input, length, &mut buffer)?;
        hits.push(Hit {
            path: std::str::from_utf8(&buffer).map_err(|_| invalid("path is not UTF-8"))?.to_string(),
            dir: flags & FOLDER != 0,
            size,
            modified,
        });
    }
    Ok(hits)
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("indexer snapshot: {what}"))
}

fn read_array<const N: usize>(input: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_text(input: &mut impl Read, length: usize, buffer: &mut Vec<u8>) -> io::Result<()> {
    buffer.resize(length, 0);
    input.read_exact(buffer)
}

/// Reads a snapshot into `sink`. Anything out of shape (a parent that is not
/// an earlier entry, a name that is not UTF-8, absurd counts) is an error,
/// never a half-built table passed off as whole.
pub fn read(input: &mut impl Read, sink: &mut impl Sink) -> io::Result<()> {
    let magic = read_array::<4>(input)?;
    if &magic == ERROR {
        let length = u16::from_le_bytes(read_array(input)?) as usize;
        let mut message = Vec::new();
        read_text(input, length, &mut message)?;
        return Err(io::Error::other(String::from_utf8_lossy(&message).into_owned()));
    }
    if &magic != MAGIC {
        return Err(invalid("not a snapshot"));
    }
    let version = u32::from_le_bytes(read_array(input)?);
    if version != VERSION {
        return Err(invalid(&format!("version {version}, this app reads {VERSION}")));
    }
    let volumes = u32::from_le_bytes(read_array(input)?);
    if volumes > 64 {
        return Err(invalid("too many volumes"));
    }
    let mut buffer = Vec::new();
    for _ in 0..volumes {
        let length = u16::from_le_bytes(read_array(input)?) as usize;
        read_text(input, length, &mut buffer)?;
        let root = std::str::from_utf8(&buffer).map_err(|_| invalid("root is not UTF-8"))?.to_string();
        sink.volume(&root);
        let count = u64::from_le_bytes(read_array(input)?);
        if count > MOST_ENTRIES {
            return Err(invalid("too many entries"));
        }
        for index in 0..count {
            let parent = u32::from_le_bytes(read_array(input)?);
            let size = u64::from_le_bytes(read_array(input)?);
            let modified = u32::from_le_bytes(read_array(input)?);
            let flags = read_array::<1>(input)?[0];
            let length = u16::from_le_bytes(read_array(input)?) as usize;
            if length > LONGEST_NAME {
                return Err(invalid("name too long"));
            }
            let in_order = if index == 0 { parent == NO_PARENT } else { u64::from(parent) < index };
            if !in_order {
                return Err(invalid("entry before its parent"));
            }
            read_text(input, length, &mut buffer)?;
            let name = std::str::from_utf8(&buffer).map_err(|_| invalid("name is not UTF-8"))?;
            sink.entry(parent, name, flags & FOLDER != 0, size, modified);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Collect(Vec<String>);

    impl Sink for Collect {
        fn volume(&mut self, root: &str) {
            self.0.push(format!("volume {root}"));
        }
        fn entry(&mut self, parent: u32, name: &str, dir: bool, size: u64, modified: u32) {
            self.0.push(format!("{parent} {name} {dir} {size} {modified}"));
        }
    }

    #[test]
    fn every_folder_holds_the_size_of_what_is_inside_it() {
        let mut c = Volume::new("C:");
        let users = c.push(0, "Users", true, 0, 10);
        let bobo = c.push(users, "Bobo", true, 0, 10);
        let docs = c.push(bobo, "Documentos", true, 0, 10);
        c.push(docs, "a.txt", false, 100, 20);
        c.push(docs, "b.txt", false, 250, 20);
        c.push(bobo, "c.txt", false, 5, 20);
        c.sum_folder_sizes();
        // Files keep their own size.
        let mut hits = Vec::new();
        c.search(&["a.txt".to_string()], 10, &mut hits);
        assert_eq!(hits[0].size, 100);
        // A folder holds everything under it, folders included.
        let size_of = |term: &str, path: &str| {
            let mut hits = Vec::new();
            c.search(&[term.to_lowercase()], 10, &mut hits);
            hits.iter().find(|hit| hit.path == path).map_or(0, |hit| hit.size)
        };
        assert_eq!(size_of("Documentos", r"C:\Users\Bobo\Documentos"), 350);
        assert_eq!(size_of("Bobo", r"C:\Users\Bobo"), 355);
        assert_eq!(size_of("Users", r"C:\Users"), 355);
        // The drive itself is not a hit: a search answers the names inside it.
        assert_eq!(size_of("c:", r"C:\"), 0);
    }

    #[test]
    fn a_search_request_and_its_answer_read_back_as_written() {
        let mut request = Vec::new();
        write_search_request(&mut request, &["wjj".to_string(), "nota".to_string()], 500).unwrap();
        assert!(request.starts_with(SEARCH));
        let (terms, limit) = read_search_request(&mut &request[SEARCH.len()..]).unwrap();
        assert_eq!(terms, ["wjj", "nota"]);
        assert_eq!(limit, 500);

        let hits = vec![
            Hit { path: r"C:\Users\Bobo\note.txt".to_string(), dir: false, size: 12, modified: 7 },
            Hit { path: r"C:\pasta".to_string(), dir: true, size: 0, modified: 0 },
        ];
        let mut answer = Vec::new();
        write_hits(&mut answer, &hits).unwrap();
        assert_eq!(read_hits(&mut answer.as_slice()).unwrap(), hits);

        let mut error = Vec::new();
        write_error(&mut error, "access denied").unwrap();
        assert_eq!(read_hits(&mut error.as_slice()).unwrap_err().to_string(), "access denied");
    }

    #[test]
    fn a_search_finds_the_names_holding_every_term_and_builds_their_paths() {
        let mut c = Volume::new("C:");
        let users = c.push(0, "Users", true, 0, 10);
        let bobo = c.push(users, "Bobo", true, 0, 10);
        c.push(bobo, "Nota Fiscal.txt", false, 1234, 20);
        c.push(bobo, "notas.txt", false, 2, 20);
        c.push(bobo, "outra coisa.md", false, 3, 20);

        let mut hits = Vec::new();
        c.search(&["nota".to_string()], 10, &mut hits);
        let paths: Vec<&str> = hits.iter().map(|hit| hit.path.as_str()).collect();
        assert_eq!(paths, [r"C:\Users\Bobo\Nota Fiscal.txt", r"C:\Users\Bobo\notas.txt"]);
        assert_eq!(hits[0].size, 1234);

        // Every term must be there, and the count is honoured.
        let mut hits = Vec::new();
        c.search(&["nota".to_string(), ".txt".to_string()], 1, &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"C:\Users\Bobo\Nota Fiscal.txt");

        // Nothing holding the term, nothing found.
        let mut hits = Vec::new();
        c.search(&["inexistente".to_string()], 10, &mut hits);
        assert!(hits.is_empty());
    }

    #[test]
    fn a_snapshot_reads_back_as_written() {
        let mut c = Volume::new("C:");
        let users = c.push(0, "Users", true, 0, 10);
        c.push(users, "ção.wav", false, 1234, 20);
        let d = Volume::new("D:");
        let mut bytes = Vec::new();
        write(&mut bytes, &[c, d]).unwrap();
        let mut seen = Collect::default();
        read(&mut bytes.as_slice(), &mut seen).unwrap();
        assert_eq!(
            seen.0,
            [
                "volume C:",
                "4294967295 C: true 0 0",
                "0 Users true 0 10",
                "1 ção.wav false 1234 20",
                "volume D:",
                "4294967295 D: true 0 0",
            ]
        );
    }

    #[test]
    fn a_snapshot_out_of_shape_is_refused() {
        let mut c = Volume::new("C:");
        c.push(5, "orphan", false, 0, 0);
        let mut bytes = Vec::new();
        write(&mut bytes, &[c]).unwrap();
        assert!(read(&mut bytes.as_slice(), &mut Collect::default()).is_err());
        assert!(read(&mut &b"nope"[..], &mut Collect::default()).is_err());
        let mut error = Vec::new();
        write_error(&mut error, "access denied").unwrap();
        let refused = read(&mut error.as_slice(), &mut Collect::default()).unwrap_err();
        assert_eq!(refused.to_string(), "access denied");
    }
}
