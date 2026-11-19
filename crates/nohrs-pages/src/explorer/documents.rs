//! Documents in the preview: Word, OpenDocument, RTF, and what else Windows
//! Search can read, shown as formatted text (headings, lists, bold and
//! italics, tables) through the preview's Markdown view.
//!
//! The open formats are read here, which keeps their structure: a .docx or
//! .odt is a zip of XML (the paragraph and run elements map to Markdown), an
//! .rtf is text with control words. The closed ones (.doc, .ppt, .xls) and
//! the other Office files are read through the IFilter Windows Search indexes
//! them with: Windows ships the one for Office files, so text comes out even
//! with no Office installed, though without its formatting.
//!
//! Everything runs on the preview's background read. Each format has a cap on
//! what it reads (`MAX_XML`, `MAX_TEXT`): a 300-page report previews its
//! beginning, not a minute of parsing nobody waits for.

use std::io::Read as _;
use std::path::Path;

/// Most of a document part read (decompressed).
const MAX_XML: u64 = 24 * 1024 * 1024;
/// Most text a preview shows; the rest is cut with a note.
const MAX_TEXT: usize = 300_000;

/// Whether the preview reads `path` as a document.
pub(crate) fn is_document(path: &str) -> bool {
    matches!(
        extension(path).as_str(),
        "docx"
            | "docm"
            | "dotx"
            | "dotm"
            | "odt"
            | "ott"
            | "rtf"
            | "doc"
            | "dot"
            | "pptx"
            | "ppt"
            | "xlsx"
            | "xls"
            | "odp"
            | "ods"
            | "msg"
    )
}

fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// The document at `path` as Markdown, or why it could not be read.
pub(crate) fn read(path: &str) -> Result<String, String> {
    let markdown = match extension(path).as_str() {
        "docx" | "docm" | "dotx" | "dotm" => docx(path),
        "odt" | "ott" => odt(path),
        "rtf" => rtf(path),
        _ => platform::filter_text(path).map(|text| escape_plain(&text)),
    }?;
    if markdown.trim().is_empty() {
        return Err("empty".into());
    }
    Ok(cap(markdown))
}

fn cap(mut text: String) -> String {
    if text.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n\n---\n\n");
        text.push_str(nohrs_core::t!(
            "*(O resto do documento não cabe na visualização.)*"
        ));
    }
    text
}

/// One part of a zip (`word/document.xml`), decompressed, up to [`MAX_XML`].
/// Read raw and inflated here: the zip crate is built without its codecs,
/// which only this needs.
#[allow(clippy::disallowed_methods)]
fn zip_part(path: &str, name: &str) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| error.to_string())?;
    let index = archive
        .index_for_name(name)
        .ok_or_else(|| format!("{name} not found"))?;
    let entry = archive
        .by_index_raw(index)
        .map_err(|error| error.to_string())?;
    let stored = entry.compression() == zip::CompressionMethod::Stored;
    let mut bytes = Vec::new();
    if stored {
        entry
            .take(MAX_XML)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
    } else {
        flate2::read::DeflateDecoder::new(entry)
            .take(MAX_XML)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A piece of XML: a tag (`<w:p …>`, `</w:p>`, `<w:br/>`) or the text
/// between tags, entities decoded.
#[derive(Debug, PartialEq)]
enum Token<'a> {
    Open {
        name: &'a str,
        attributes: &'a str,
        empty: bool,
    },
    Close(&'a str),
    Text(String),
}

/// Splits XML into [`Token`]s. Enough for the documents read here, which are
/// machine-written: comments, declarations and processing instructions are
/// skipped, CDATA is text.
fn tokens(xml: &str) -> impl Iterator<Item = Token<'_>> {
    let mut rest = xml;
    std::iter::from_fn(move || {
        loop {
            if rest.is_empty() {
                return None;
            }
            if let Some(after) = rest.strip_prefix('<') {
                if let Some(cdata) = after.strip_prefix("![CDATA[") {
                    let end = cdata.find("]]>").unwrap_or(cdata.len());
                    let text = cdata[..end].to_string();
                    rest = cdata.get(end + 3..).unwrap_or("");
                    return Some(Token::Text(text));
                }
                let end = after.find('>').unwrap_or(after.len());
                let tag = &after[..end];
                rest = after.get(end + 1..).unwrap_or("");
                if tag.starts_with('?') || tag.starts_with('!') {
                    continue;
                }
                if let Some(name) = tag.strip_prefix('/') {
                    return Some(Token::Close(name.trim()));
                }
                let empty = tag.ends_with('/');
                let tag = tag.trim_end_matches('/');
                let (name, attributes) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
                return Some(Token::Open {
                    name,
                    attributes,
                    empty,
                });
            }
            let end = rest.find('<').unwrap_or(rest.len());
            let text = &rest[..end];
            rest = &rest[end..];
            if !text.is_empty() {
                return Some(Token::Text(unescape(text)));
            }
        }
    })
}

/// The value of attribute `key` in a tag's attribute text.
fn attribute<'a>(attributes: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = attributes;
    while let Some(at) = rest.find(key) {
        let after = &rest[at + key.len()..];
        let preceded = at == 0 || rest[..at].ends_with(char::is_whitespace);
        if preceded && let Some(value) = after.trim_start().strip_prefix('=') {
            let value = value.trim_start();
            let quote = value.chars().next()?;
            let value = &value[1..];
            return value.find(quote).map(|end| &value[..end]);
        }
        rest = after;
    }
    None
}

fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let Some(end) = after.find(';').filter(|&end| end <= 10) else {
            out.push('&');
            rest = after;
            continue;
        };
        let entity = &after[..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|decimal| decimal.parse().ok())
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(character) => out.push(character),
            None => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Text that Markdown would otherwise read as formatting, made literal.
fn escape_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '*' | '_' | '`' | '[' | ']' | '<' | '>' | '#' | '|' | '\\'
        ) {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

// Plain text from a filter: paragraphs kept, nothing read as Markdown.
fn escape_plain(text: &str) -> String {
    text.lines()
        .map(|line| escape_inline(line.trim_end()))
        .collect::<Vec<_>>()
        .join("\n\n")
        .replace("\n\n\n\n", "\n\n")
}

/// Text with the run's formatting around it, the spaces left outside the
/// markers (`** bold**` is not bold in Markdown).
fn styled(text: &str, bold: bool, italic: bool) -> String {
    let core = text.trim();
    if core.is_empty() || (!bold && !italic) {
        return escape_inline(text);
    }
    let marker = match (bold, italic) {
        (true, true) => "***",
        (true, false) => "**",
        _ => "*",
    };
    let leading = &text[..text.len() - text.trim_start().len()];
    let trailing = &text[text.trim_end().len()..];
    format!("{leading}{marker}{}{marker}{trailing}", escape_inline(core))
}

/// Builds Markdown from paragraphs, list items, table cells and runs, the
/// shape both Word's and OpenDocument's XML come in.
#[derive(Default)]
struct Writer {
    out: String,
    paragraph: String,
    heading: Option<usize>,
    list_depth: Option<usize>,
    row: Vec<String>,
    rows_in_table: usize,
    table_depth: usize,
}

impl Writer {
    fn text(&mut self, text: &str, bold: bool, italic: bool) {
        self.paragraph.push_str(&styled(text, bold, italic));
    }

    fn raw(&mut self, text: &str) {
        self.paragraph.push_str(text);
    }

    fn end_paragraph(&mut self) {
        let text = std::mem::take(&mut self.paragraph);
        let heading = self.heading.take();
        let list = self.list_depth.take();
        if self.table_depth > 0 {
            // In a cell: its paragraphs join with a line break.
            if let Some(cell) = self.row.last_mut() {
                if !cell.is_empty() && !text.trim().is_empty() {
                    cell.push_str("<br>");
                }
                cell.push_str(text.trim());
            }
            return;
        }
        let text = text.trim_end();
        if text.trim().is_empty() {
            return;
        }
        match (heading, list) {
            (Some(level), _) => {
                self.out.push_str(&"#".repeat(level.clamp(1, 6)));
                self.out.push(' ');
                self.out.push_str(text.trim());
            }
            (None, Some(depth)) => {
                self.out.push_str(&"  ".repeat(depth));
                self.out.push_str("- ");
                self.out.push_str(text.trim());
            }
            (None, None) => self.out.push_str(text),
        }
        self.out.push_str("\n\n");
    }

    fn start_table(&mut self) {
        if self.table_depth == 0 {
            self.rows_in_table = 0;
        }
        self.table_depth += 1;
    }

    fn end_table(&mut self) {
        self.table_depth = self.table_depth.saturating_sub(1);
        if self.table_depth == 0 {
            self.out.push('\n');
        }
    }

    fn start_cell(&mut self) {
        if self.table_depth == 1 {
            self.row.push(String::new());
        }
    }

    fn end_row(&mut self) {
        if self.table_depth != 1 {
            return;
        }
        let cells = std::mem::take(&mut self.row);
        if cells.is_empty() {
            return;
        }
        let line = cells
            .iter()
            .map(|cell| cell.replace('|', "\\|"))
            .collect::<Vec<_>>()
            .join(" | ");
        self.out.push_str("| ");
        self.out.push_str(&line);
        self.out.push_str(" |\n");
        if self.rows_in_table == 0 {
            // A Markdown table needs its first row to be a heading.
            self.out.push('|');
            self.out.push_str(&" --- |".repeat(cells.len()));
            self.out.push('\n');
        }
        self.rows_in_table += 1;
    }
}

/// The heading level a Word paragraph style names: "Heading2" in English,
/// "Ttulo2" in a Portuguese or Spanish Word (the style id drops accents),
/// "berschrift2" in German, "Titre2"; "Title" is a level-one heading.
fn heading_level(style: &str) -> Option<usize> {
    let lower = style.to_lowercase();
    if lower == "title" || lower == "ttulo" || lower == "titulo" {
        return Some(1);
    }
    let digits: String = lower.chars().filter(char::is_ascii_digit).collect();
    let named = [
        "heading",
        "ttulo",
        "titulo",
        "berschrift",
        "titre",
        "titolo",
        "kop",
    ]
    .iter()
    .any(|name| lower.starts_with(name));
    if named && !digits.is_empty() {
        return digits.parse().ok();
    }
    None
}

fn docx(path: &str) -> Result<String, String> {
    let xml = zip_part(path, "word/document.xml")?;
    let mut writer = Writer::default();
    let (mut bold, mut italic) = (false, false);
    let mut in_run_properties = false;
    let mut in_text = false;
    for token in tokens(&xml) {
        match token {
            Token::Open {
                name,
                attributes,
                empty,
            } => match name {
                "w:pStyle" => {
                    writer.heading = attribute(attributes, "w:val").and_then(heading_level)
                }
                "w:ilvl" => {
                    writer.list_depth = Some(
                        attribute(attributes, "w:val")
                            .and_then(|level| level.parse().ok())
                            .unwrap_or(0),
                    )
                }
                "w:numPr" if writer.list_depth.is_none() => writer.list_depth = Some(0),
                "w:r" => {
                    bold = false;
                    italic = false;
                }
                "w:rPr" if !empty => in_run_properties = true,
                "w:b" | "w:bCs" if in_run_properties => {
                    bold = !matches!(attribute(attributes, "w:val"), Some("0" | "false"))
                }
                "w:i" | "w:iCs" if in_run_properties => {
                    italic = !matches!(attribute(attributes, "w:val"), Some("0" | "false"))
                }
                "w:t" if !empty => in_text = true,
                "w:tab" => writer.raw("\t"),
                "w:br" | "w:cr" => writer.raw("  \n"),
                "w:tbl" => writer.start_table(),
                "w:tc" => writer.start_cell(),
                "w:p" if empty => writer.end_paragraph(),
                _ => {}
            },
            Token::Close(name) => match name {
                "w:rPr" => in_run_properties = false,
                "w:t" => in_text = false,
                "w:p" => writer.end_paragraph(),
                "w:tr" => writer.end_row(),
                "w:tbl" => writer.end_table(),
                _ => {}
            },
            Token::Text(text) if in_text => writer.text(&text, bold, italic),
            Token::Text(_) => {}
        }
    }
    writer.end_paragraph();
    Ok(writer.out)
}

fn odt(path: &str) -> Result<String, String> {
    let xml = zip_part(path, "content.xml")?;
    let mut writer = Writer::default();
    let mut list_depth: usize = 0;
    // Text is only that of paragraphs and headings; elsewhere (fields'
    // declarations, the forms) it is not part of the document's body.
    let mut in_paragraph = 0usize;
    for token in tokens(&xml) {
        match token {
            Token::Open {
                name,
                attributes,
                empty,
            } => match name {
                "text:h" if !empty => {
                    in_paragraph += 1;
                    writer.heading = Some(
                        attribute(attributes, "text:outline-level")
                            .and_then(|level| level.parse().ok())
                            .unwrap_or(1),
                    );
                }
                "text:p" if !empty => {
                    in_paragraph += 1;
                    if list_depth > 0 {
                        writer.list_depth = Some(list_depth - 1);
                    }
                }
                "text:list" if !empty => list_depth += 1,
                "text:tab" => writer.raw("\t"),
                "text:line-break" => writer.raw("  \n"),
                "text:s" => {
                    let count = attribute(attributes, "text:c")
                        .and_then(|count| count.parse().ok())
                        .unwrap_or(1usize);
                    writer.raw(&" ".repeat(count.min(64)));
                }
                "table:table" if !empty => writer.start_table(),
                "table:table-cell" => writer.start_cell(),
                _ => {}
            },
            Token::Close(name) => match name {
                "text:h" | "text:p" => {
                    in_paragraph = in_paragraph.saturating_sub(1);
                    writer.end_paragraph();
                }
                "text:list" => list_depth = list_depth.saturating_sub(1),
                "table:table-row" => writer.end_row(),
                "table:table" => writer.end_table(),
                _ => {}
            },
            Token::Text(text) if in_paragraph > 0 => writer.text(&text, false, false),
            Token::Text(_) => {}
        }
    }
    Ok(writer.out)
}

#[allow(clippy::disallowed_methods)]
fn rtf(path: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_XML).read_to_end(&mut bytes))
        .map_err(|error| error.to_string())?;
    Ok(escape_plain(&rtf_text(&bytes)))
}

/// The text of an RTF document: control words dropped, paragraphs and tabs
/// kept, `\'hh` read as Windows-1252 and `\uN` as Unicode (its fallback
/// character skipped). Groups that are no text (fonts, colours, styles,
/// pictures, `\*` destinations) are skipped whole.
fn rtf_text(bytes: &[u8]) -> String {
    const SKIPPED: [&str; 10] = [
        "fonttbl",
        "colortbl",
        "stylesheet",
        "info",
        "pict",
        "header",
        "footer",
        "themedata",
        "datastore",
        "listtable",
    ];
    let mut out = String::new();
    // For each open group, whether its text is skipped.
    let mut skipping: Vec<bool> = vec![false];
    let mut skip_fallback = 0usize;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let skip = skipping.last().copied().unwrap_or(false);
        match byte {
            b'{' => {
                skipping.push(skip);
                index += 1;
            }
            b'}' => {
                if skipping.len() > 1 {
                    skipping.pop();
                }
                index += 1;
            }
            b'\\' => {
                index += 1;
                let Some(&next) = bytes.get(index) else {
                    break;
                };
                if next == b'\'' {
                    let hex = bytes
                        .get(index + 1..index + 3)
                        .and_then(|hex| std::str::from_utf8(hex).ok());
                    if let Some(code) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok())
                        && !skip
                    {
                        if skip_fallback > 0 {
                            skip_fallback -= 1;
                        } else {
                            out.push(cp1252(code));
                        }
                    }
                    index += 3;
                    continue;
                }
                if !next.is_ascii_alphabetic() {
                    // A control symbol: an escaped brace or backslash is text;
                    // `\*` marks a destination to skip.
                    match next {
                        b'*' => {
                            if let Some(top) = skipping.last_mut() {
                                *top = true;
                            }
                        }
                        b'{' | b'}' | b'\\' if !skip => out.push(next as char),
                        b'~' if !skip => out.push('\u{a0}'),
                        _ => {}
                    }
                    index += 1;
                    continue;
                }
                let start = index;
                while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                    index += 1;
                }
                let word = std::str::from_utf8(&bytes[start..index]).unwrap_or("");
                let number_start = index;
                if index < bytes.len() && (bytes[index] == b'-' || bytes[index].is_ascii_digit()) {
                    index += 1;
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        index += 1;
                    }
                }
                let number: Option<i32> = std::str::from_utf8(&bytes[number_start..index])
                    .ok()
                    .and_then(|n| n.parse().ok());
                if index < bytes.len() && bytes[index] == b' ' {
                    index += 1;
                }
                if SKIPPED.contains(&word) {
                    if let Some(top) = skipping.last_mut() {
                        *top = true;
                    }
                    continue;
                }
                if skip {
                    continue;
                }
                match word {
                    "par" | "line" | "sect" | "page" => out.push('\n'),
                    "tab" | "cell" => out.push('\t'),
                    "row" => out.push('\n'),
                    "u" => {
                        if let Some(code) = number {
                            let code = if code < 0 { code + 65536 } else { code } as u32;
                            if let Some(character) = char::from_u32(code) {
                                out.push(character);
                            }
                            skip_fallback = 1;
                        }
                    }
                    _ => {}
                }
            }
            b'\r' | b'\n' => index += 1,
            _ => {
                if !skip {
                    if skip_fallback > 0 {
                        skip_fallback -= 1;
                    } else {
                        out.push(cp1252(byte));
                    }
                }
                index += 1;
            }
        }
    }
    out
}

/// A Windows-1252 byte as a character (the code page RTF files from Word
/// are written in, outside `\u`).
fn cp1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9F => HIGH[(byte - 0x80) as usize],
        _ => byte as char,
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! Windows Search's filters have no safe binding.

    use windows::Win32::Storage::IndexServer::{
        CHUNK_TEXT, FILTER_E_END_OF_CHUNKS, FILTER_E_NO_MORE_TEXT,
        IFILTER_INIT_APPLY_INDEX_ATTRIBUTES, IFILTER_INIT_CANON_PARAGRAPHS, IFilter, LoadIFilter,
        STAT_CHUNK,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
    use windows::core::{HSTRING, Interface, PWSTR};

    /// Most UTF-16 units read from a filter.
    const MAX_UNITS: usize = 400_000;

    /// The text the file's Windows Search filter gives, read on a thread of
    /// its own: COM is set up for it there and torn down after, so the
    /// preview's pool thread keeps no apartment that a later caller would
    /// trip on (as the Recycle Bin did, `ops::os_trash_delete`).
    pub fn filter_text(path: &str) -> Result<String, String> {
        let path = path.to_string();
        std::thread::Builder::new()
            .name("nohrs-ifilter".into())
            .spawn(move || {
                let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
                let text = read(&path);
                if initialized {
                    unsafe { CoUninitialize() };
                }
                text
            })
            .map_err(|error| error.to_string())?
            .join()
            .map_err(|_| "the filter crashed".to_string())?
    }

    fn read(path: &str) -> Result<String, String> {
        let mut raw = std::ptr::null_mut();
        unsafe { LoadIFilter(&HSTRING::from(path), None, &mut raw) }
            .map_err(|error| error.message().to_string())?;
        if raw.is_null() {
            return Err("no filter".into());
        }
        let unknown = unsafe { windows::core::IUnknown::from_raw(raw) };
        let filter: IFilter = unknown
            .cast()
            .map_err(|error| error.message().to_string())?;
        let mut flags = 0u32;
        let init = unsafe {
            filter.Init(
                (IFILTER_INIT_CANON_PARAGRAPHS.0 | IFILTER_INIT_APPLY_INDEX_ATTRIBUTES.0) as u32,
                &[],
                &mut flags,
            )
        };
        if init < 0 {
            return Err(format!("filter init {init:#x}"));
        }
        let mut units: Vec<u16> = Vec::new();
        let mut buffer = vec![0u16; 8192];
        'chunks: loop {
            let mut chunk = STAT_CHUNK::default();
            let got = unsafe { filter.GetChunk(&mut chunk) };
            if got == FILTER_E_END_OF_CHUNKS.0 || got < 0 && got != FILTER_E_NO_MORE_TEXT.0 {
                break;
            }
            if chunk.flags != CHUNK_TEXT {
                continue;
            }
            // CHUNK_EOP and CHUNK_EOC: a new paragraph or chapter.
            if chunk.breakType.0 >= 3 && !units.is_empty() {
                units.push(u16::from(b'\n'));
            }
            loop {
                let mut count = buffer.len() as u32;
                let read = unsafe { filter.GetText(&mut count, PWSTR(buffer.as_mut_ptr())) };
                if read == FILTER_E_NO_MORE_TEXT.0 || read < 0 {
                    break;
                }
                units.extend_from_slice(&buffer[..(count as usize).min(buffer.len())]);
                if units.len() > MAX_UNITS {
                    break 'chunks;
                }
            }
        }
        // Filters mark paragraph breaks with carriage returns.
        Ok(String::from_utf16_lossy(&units)
            .replace("\r\n", "\n")
            .replace('\r', "\n"))
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn filter_text(_path: &str) -> Result<String, String> {
        Err("no filter here".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_xml_becomes_markdown() {
        let xml = r#"<?xml version="1.0"?><w:document><w:body>
            <w:p><w:pPr><w:pStyle w:val="Ttulo1"/></w:pPr><w:r><w:t>Roteiro &amp; cortes</w:t></w:r></w:p>
            <w:p><w:r><w:t xml:space="preserve">Texto </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>forte</w:t></w:r><w:r><w:rPr><w:b w:val="0"/></w:rPr><w:t> e fim*</w:t></w:r></w:p>
            <w:p><w:pPr><w:numPr><w:ilvl w:val="1"/></w:numPr></w:pPr><w:r><w:t>item</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr>
            <w:tr><w:tc><w:p><w:r><w:t>1</w:t></w:r></w:p></w:tc><w:tc><w:p/></w:tc></w:tr></w:tbl>
        </w:body></w:document>"#;
        let folder = tempfile::tempdir().expect("temp folder");
        let path = folder.path().join("a.docx");
        {
            let file = std::fs::File::create(&path).expect("create");
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file(
                "word/document.xml",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .expect("entry");
            std::io::Write::write_all(&mut zip, xml.as_bytes()).expect("write");
            zip.finish().expect("finish");
        }
        let markdown = read(&path.to_string_lossy()).expect("read");
        assert!(markdown.contains("# Roteiro & cortes"), "{markdown}");
        assert!(markdown.contains("Texto **forte** e fim\\*"), "{markdown}");
        assert!(markdown.contains("  - item"), "{markdown}");
        assert!(
            markdown.contains("| A | B |\n| --- | --- |\n| 1 |  |"),
            "{markdown}"
        );
    }

    #[test]
    fn rtf_keeps_text_and_drops_control_words() {
        let rtf = concat!(r"{\rtf1\ansi{\fonttbl{\f0 Arial;}}{\*\generator Word;}\f0 Ol\'e1 \b mundo\b0\par Linha \", "u8364? fim}").as_bytes();
        assert_eq!(rtf_text(rtf), "Olá mundo\nLinha € fim");
    }

    #[test]
    fn headings_are_found_in_any_language() {
        assert_eq!(heading_level("Heading2"), Some(2));
        assert_eq!(heading_level("Ttulo3"), Some(3));
        assert_eq!(heading_level("Title"), Some(1));
        assert_eq!(heading_level("Normal"), None);
    }

    #[test]
    fn attributes_are_matched_by_whole_name() {
        assert_eq!(attribute(r#"w:val="0" xml:val="x""#, "w:val"), Some("0"));
        assert_eq!(
            attribute(r#"text:outline-level="2""#, "text:outline-level"),
            Some("2")
        );
        assert_eq!(attribute(r#"w:valx="1""#, "w:val"), None);
    }
}
