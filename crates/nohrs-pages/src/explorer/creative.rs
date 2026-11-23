//! Previews of the creative apps' files: Photoshop (.psd, .psb), Illustrator
//! (.ai), Affinity (.af, .afphoto, .afdesign, .afpub), Premiere Pro (.prproj)
//! and After Effects (.aep, .aepx).
//!
//! Windows has no thumbnails for most of these without the apps' own shell
//! extensions, so each is read here for what it carries:
//!
//! * a Photoshop file keeps the whole flattened image after its layers (when
//!   saved with "Maximize compatibility", Photoshop's default); it is decoded
//!   straight to the preview's size, a row at a time, so a 10,000 px poster
//!   never sits in memory whole (OTIMIZACAO.md §3.5);
//! * an Illustrator file saved with PDF compatibility (the default) is a PDF,
//!   and goes to the preview's PDF reader; one without it carries a JPEG
//!   thumbnail in its metadata;
//! * an Affinity file carries a PNG thumbnail of the document;
//! * a Premiere or After Effects project is a description of a timeline, not
//!   an image: the preview lists its sequences and the media it uses, marking
//!   what is missing from disk, which is what one opens a project to check.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// What the preview shows for a creative app's file.
pub(crate) enum Creative {
    /// An image, already in the BGRA the preview draws, with a line about
    /// the document (its size, its layers).
    Picture {
        width: u32,
        height: u32,
        bgra: Vec<u8>,
        caption: String,
    },
    /// A PDF the PDF reader can show (an Illustrator file).
    Pdf,
    /// A project's summary, in Markdown.
    Summary(String),
}

/// Whether the preview reads `path` here.
pub(crate) fn is_creative(path: &str) -> bool {
    matches!(
        extension(path).as_str(),
        "psd" | "psb" | "ai" | "af" | "afphoto" | "afdesign" | "afpub" | "prproj" | "aep" | "aepx"
    )
}

fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Reads `path` for the preview, its picture fitted in `edge` pixels.
pub(crate) fn read(path: &str, edge: u32) -> Result<Creative, String> {
    match extension(path).as_str() {
        "psd" | "psb" => psd(path, edge),
        "ai" => illustrator(path, edge),
        "af" | "afphoto" | "afdesign" | "afpub" => affinity(path, edge),
        "prproj" => premiere(path),
        "aep" | "aepx" => after_effects(path),
        _ => Err("not a creative file".into()),
    }
}

#[allow(clippy::disallowed_methods)]
fn open(path: &str) -> Result<std::io::BufReader<std::fs::File>, String> {
    std::fs::File::open(path)
        .map(|file| std::io::BufReader::with_capacity(256 * 1024, file))
        .map_err(|error| error.to_string())
}

fn read_u16(reader: &mut impl Read) -> Result<u16, String> {
    let mut bytes = [0; 2];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u16::from_be_bytes(bytes))
}

fn read_u32(reader: &mut impl Read) -> Result<u32, String> {
    let mut bytes = [0; 4];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> Result<u64, String> {
    let mut bytes = [0; 8];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u64::from_be_bytes(bytes))
}

fn skip(reader: &mut (impl Read + Seek), length: u64) -> Result<(), String> {
    reader
        .seek(SeekFrom::Current(length as i64))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// A fitted size for `width` x `height` within `edge`, and the step between
/// the source rows and columns that are kept.
fn fit(width: u32, height: u32, edge: u32) -> (u32, u32, u32) {
    let step = width.max(height).div_ceil(edge.max(1)).max(1);
    (width.div_ceil(step), height.div_ceil(step), step)
}

/// Photoshop's flattened image, at the end of the file. Channels are stored
/// one after another (all of red, then green…), each row either raw or
/// PackBits-compressed; the rows kept are decoded and sampled into the
/// fitted image, the others are skipped without decoding.
fn psd(path: &str, edge: u32) -> Result<Creative, String> {
    let mut file = open(path)?;
    let mut signature = [0; 4];
    file.read_exact(&mut signature)
        .map_err(|error| error.to_string())?;
    if &signature != b"8BPS" {
        return Err("not a Photoshop file".into());
    }
    let version = read_u16(&mut file)?;
    let big = version == 2;
    skip(&mut file, 6)?;
    let channels = read_u16(&mut file)? as usize;
    let height = read_u32(&mut file)?;
    let width = read_u32(&mut file)?;
    let depth = read_u16(&mut file)?;
    let mode = read_u16(&mut file)?;
    if width == 0 || height == 0 || channels == 0 {
        return Err("empty image".into());
    }
    // Colour mode data, image resources, then layers and masks.
    let color_data = read_u32(&mut file)? as u64;
    skip(&mut file, color_data)?;
    let resources = read_u32(&mut file)? as u64;
    skip(&mut file, resources)?;
    let layers = if big {
        read_u64(&mut file)?
    } else {
        read_u32(&mut file)? as u64
    };
    skip(&mut file, layers)?;
    let compression = read_u16(&mut file)?;
    let bytes_per_sample: usize = match depth {
        8 => 1,
        16 => 2,
        32 => 4,
        _ => return Err(format!("{depth}-bit images are not read")),
    };
    let row_bytes = width as usize * bytes_per_sample;
    let (out_width, out_height, step) = fit(width, height, edge);
    // Which channels make the picture, by colour mode: RGB (3), CMYK (4),
    // grayscale and the rest (1); one more is the alpha when present.
    let colour_channels = match mode {
        3 => 3,
        4 => 4,
        _ => 1,
    };
    let wanted = colour_channels.min(channels);
    let has_alpha = channels > colour_channels && mode != 4;
    let mut planes: Vec<Vec<u8>> =
        vec![vec![0; (out_width * out_height) as usize]; wanted + usize::from(has_alpha)];
    let total_rows = height as usize * channels;
    let row_lengths: Vec<usize> = if compression == 1 {
        let mut lengths = Vec::with_capacity(total_rows);
        for _ in 0..total_rows {
            lengths.push(if big {
                read_u32(&mut file)? as usize
            } else {
                read_u16(&mut file)? as usize
            });
        }
        lengths
    } else if compression == 0 {
        vec![row_bytes; total_rows]
    } else {
        return Err("compressed with ZIP, which is not read".into());
    };
    let mut packed = Vec::new();
    let mut row = vec![0u8; row_bytes];
    for channel in 0..channels {
        let plane = if channel < wanted {
            Some(channel)
        } else if has_alpha && channel == colour_channels {
            Some(wanted)
        } else {
            None
        };
        for y in 0..height as usize {
            let length = row_lengths[channel * height as usize + y];
            let kept = plane.is_some() && (y as u32).is_multiple_of(step);
            if !kept {
                skip(&mut file, length as u64)?;
                continue;
            }
            if compression == 1 {
                packed.resize(length, 0);
                file.read_exact(&mut packed)
                    .map_err(|error| error.to_string())?;
                unpack_bits(&packed, &mut row);
            } else {
                file.read_exact(&mut row)
                    .map_err(|error| error.to_string())?;
            }
            let Some(plane) = plane else {
                continue;
            };
            let out_y = (y as u32 / step) as usize;
            let target =
                &mut planes[plane][out_y * out_width as usize..(out_y + 1) * out_width as usize];
            for (out_x, value) in target.iter_mut().enumerate() {
                let x = out_x * step as usize;
                // The high byte of a 16-bit sample; a 32-bit one is a float
                // from 0 to 1.
                *value = match bytes_per_sample {
                    1 => row[x],
                    2 => row[x * 2],
                    _ => {
                        let bits = [row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]];
                        (f32::from_be_bytes(bits).clamp(0.0, 1.0) * 255.0) as u8
                    }
                };
            }
        }
    }
    let pixels = (out_width * out_height) as usize;
    let mut bgra = vec![0u8; pixels * 4];
    for index in 0..pixels {
        let (red, green, blue) = match (mode, wanted) {
            (4, 4) => {
                // Photoshop stores CMYK inverted: 255 is no ink.
                let (cyan, magenta, yellow, black) = (
                    planes[0][index] as u32,
                    planes[1][index] as u32,
                    planes[2][index] as u32,
                    planes[3][index] as u32,
                );
                (
                    (cyan * black / 255) as u8,
                    (magenta * black / 255) as u8,
                    (yellow * black / 255) as u8,
                )
            }
            (_, 3..) => (planes[0][index], planes[1][index], planes[2][index]),
            _ => (planes[0][index], planes[0][index], planes[0][index]),
        };
        let alpha = if has_alpha {
            planes[wanted][index]
        } else {
            255
        };
        let premultiply = |channel: u8| ((channel as u16 * alpha as u16 + 127) / 255) as u8;
        bgra[index * 4] = premultiply(blue);
        bgra[index * 4 + 1] = premultiply(green);
        bgra[index * 4 + 2] = premultiply(red);
        bgra[index * 4 + 3] = alpha;
    }
    Ok(Creative::Picture {
        width: out_width,
        height: out_height,
        bgra,
        caption: format!("{width} × {height} px · {depth} bits"),
    })
}

/// PackBits, the run-length scheme Photoshop rows use.
fn unpack_bits(packed: &[u8], row: &mut [u8]) {
    let mut input = 0;
    let mut output = 0;
    while input < packed.len() && output < row.len() {
        let header = packed[input] as i8;
        input += 1;
        if header >= 0 {
            let count = header as usize + 1;
            let end = (input + count).min(packed.len());
            let take = (end - input).min(row.len() - output);
            row[output..output + take].copy_from_slice(&packed[input..input + take]);
            input = end;
            output += take;
        } else if header != -128 {
            let count = (1 - header as isize) as usize;
            let Some(&value) = packed.get(input) else {
                break;
            };
            input += 1;
            let take = count.min(row.len() - output);
            row[output..output + take].fill(value);
            output += take;
        }
    }
}

#[allow(clippy::disallowed_methods)]
fn illustrator(path: &str, edge: u32) -> Result<Creative, String> {
    let mut head = [0u8; 5];
    let mut file = open(path)?;
    file.read_exact(&mut head)
        .map_err(|error| error.to_string())?;
    if &head == b"%PDF-" {
        return Ok(Creative::Pdf);
    }
    // PostScript-only: the thumbnail Illustrator writes in the metadata, a
    // base64 JPEG under `xmpGImg:image`.
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(64 * 1024 * 1024).read_to_end(&mut bytes))
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let start = text.find("<xmpGImg:image>").ok_or("no thumbnail")? + "<xmpGImg:image>".len();
    let end = text[start..]
        .find("</xmpGImg:image>")
        .ok_or("no thumbnail")?
        + start;
    let encoded: String = text[start..end]
        .replace("&#xA;", "")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let jpeg = decode_base64(&encoded).ok_or("bad thumbnail")?;
    picture(&jpeg, edge, String::new())
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let value = |byte: u8| -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => (byte - b'A') as u32,
            b'a'..=b'z' => (byte - b'a' + 26) as u32,
            b'0'..=b'9' => (byte - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let bytes: Vec<u8> = text.bytes().filter(|byte| *byte != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut buffer = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            buffer |= value(*byte)? << (18 - 6 * index);
        }
        out.push((buffer >> 16) as u8);
        if chunk.len() > 2 {
            out.push((buffer >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(buffer as u8);
        }
    }
    Some(out)
}

/// An encoded image (PNG, JPEG), decoded and fitted in `edge`.
fn picture(encoded: &[u8], edge: u32, caption: String) -> Result<Creative, String> {
    let decoded = image::load_from_memory(encoded).map_err(|error| error.to_string())?;
    let decoded = if decoded.width().max(decoded.height()) > edge {
        decoded.thumbnail(edge, edge)
    } else {
        decoded
    };
    let rgba = decoded.into_rgba8();
    let (width, height) = rgba.dimensions();
    let mut bgra = rgba.into_raw();
    for pixel in bgra.as_chunks_mut::<4>().0 {
        let alpha = pixel[3] as u16;
        let premultiply = |channel: u8| ((channel as u16 * alpha + 127) / 255) as u8;
        let (red, green, blue) = (pixel[0], pixel[1], pixel[2]);
        pixel[0] = premultiply(blue);
        pixel[1] = premultiply(green);
        pixel[2] = premultiply(red);
    }
    Ok(Creative::Picture {
        width,
        height,
        bgra,
        caption,
    })
}

/// The PNG thumbnail inside an Affinity document, the largest if there are
/// several.
#[allow(clippy::disallowed_methods)]
fn affinity(path: &str, edge: u32) -> Result<Creative, String> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(256 * 1024 * 1024).read_to_end(&mut bytes))
        .map_err(|error| error.to_string())?;
    let mut best: Option<&[u8]> = None;
    let mut from = 0;
    while let Some(at) = find(&bytes[from..], SIGNATURE).map(|at| at + from) {
        // Chunk by chunk to IEND, which ends the PNG.
        let mut cursor = at + SIGNATURE.len();
        let mut end = None;
        while cursor + 8 <= bytes.len() {
            let length = u32::from_be_bytes([
                bytes[cursor],
                bytes[cursor + 1],
                bytes[cursor + 2],
                bytes[cursor + 3],
            ]) as usize;
            let kind = &bytes[cursor + 4..cursor + 8];
            cursor = cursor.saturating_add(12).saturating_add(length);
            if kind == b"IEND" {
                end = Some(cursor.min(bytes.len()));
                break;
            }
        }
        let Some(end) = end else {
            break;
        };
        if best.is_none_or(|kept| end - at > kept.len()) {
            best = Some(&bytes[at..end]);
        }
        from = end;
    }
    picture(best.ok_or("no thumbnail")?, edge, String::new())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Most media files listed in a project's summary.
const MAX_MEDIA: usize = 400;

/// A Premiere project: gzipped XML. Its sequences by name and the media
/// files it points at.
#[allow(clippy::disallowed_methods)]
fn premiere(path: &str) -> Result<Creative, String> {
    let mut xml = String::new();
    flate2::read::GzDecoder::new(open(path)?)
        .take(512 * 1024 * 1024)
        .read_to_string(&mut xml)
        .map_err(|error| error.to_string())?;
    let version = xml
        .find("<Project ObjectID")
        .and_then(|at| xml[at..].find("Version=\"").map(|v| at + v + 9))
        .and_then(|start| {
            xml[start..]
                .find('"')
                .map(|end| xml[start..start + end].to_string())
        });
    let mut sequences = Vec::new();
    let mut rest = xml.as_str();
    while let Some(at) = rest.find("<Sequence ObjectUID=") {
        rest = &rest[at..];
        let block_end = rest.find("</Sequence>").unwrap_or(rest.len());
        if let Some(name) = tag_text(&rest[..block_end], "Name") {
            sequences.push(name);
        }
        rest = &rest[block_end.min(rest.len())..];
        if rest.len() > 1 {
            rest = &rest[1..];
        }
    }
    sequences.sort_by_key(|name| name.to_lowercase());
    sequences.dedup();
    let media = all_tag_texts(&xml, "ActualMediaFilePath");
    Ok(Creative::Summary(project_summary(
        nohrs_core::t!("Projeto do Premiere Pro"),
        version.map(|version| nohrs_core::tf!("Versão do arquivo {version}", version = version)),
        nohrs_core::t!("Sequências"),
        &sequences,
        media,
    )))
}

/// An After Effects project: RIFX chunks (.aep) or XML (.aepx). The media it
/// uses are kept as JSON in its `alas` chunks (`"fullpath":"…"`), or as
/// `fullpath="…"` in the XML; the app that saved it is in its metadata.
#[allow(clippy::disallowed_methods)]
fn after_effects(path: &str) -> Result<Creative, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(512 * 1024 * 1024).read_to_end(&mut bytes))
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let creator = tag_text(&text, "xmp:CreatorTool");
    let mut media = Vec::new();
    for marker in ["\"fullpath\":\"", "fullpath=\""] {
        let mut rest = text.as_ref();
        while let Some(at) = rest.find(marker) {
            rest = &rest[at + marker.len()..];
            let end = rest.find('"').unwrap_or(rest.len());
            let path = rest[..end].replace("\\\\", "\\").replace("\\/", "/");
            if !path.is_empty() {
                media.push(path);
            }
            rest = &rest[end..];
        }
    }
    Ok(Creative::Summary(project_summary(
        nohrs_core::t!("Projeto do After Effects"),
        creator,
        nohrs_core::t!("Composições"),
        &[],
        media,
    )))
}

/// The text inside the first `<tag>…</tag>` of `xml`.
fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(unescape_xml(&xml[start..end]))
}

fn all_tag_texts(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find(&open) {
        rest = &rest[at + open.len()..];
        let Some(end) = rest.find(&close) else {
            break;
        };
        found.push(unescape_xml(&rest[..end]));
        rest = &rest[end..];
    }
    found
}

fn unescape_xml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// A project's summary: what made it, its sequences, and its media files,
/// the missing ones first and marked, so a moved drive shows at a glance.
#[allow(clippy::disallowed_methods)]
fn project_summary(
    title: &str,
    detail: Option<String>,
    sequences_title: &str,
    sequences: &[String],
    mut media: Vec<String>,
) -> String {
    media.sort_by_key(|path| path.to_lowercase());
    media.dedup_by_key(|path| path.to_lowercase());
    let total = media.len();
    let mut rows: Vec<(bool, String)> = media
        .into_iter()
        .take(MAX_MEDIA)
        .map(|path| (Path::new(&path).exists(), path))
        .collect();
    rows.sort_by_key(|(present, _)| *present);
    let missing = rows.iter().filter(|(present, _)| !present).count();
    let mut out = format!("# {title}\n\n");
    if let Some(detail) = detail {
        out.push_str(&format!("{detail}\n\n"));
    }
    if !sequences.is_empty() {
        out.push_str(&format!("## {sequences_title} ({})\n\n", sequences.len()));
        for name in sequences.iter().take(200) {
            out.push_str(&format!("- {}\n", name.replace('*', "\\*")));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "## {} ({total})\n\n",
        nohrs_core::t!("Mídia usada")
    ));
    if missing > 0 {
        let line = nohrs_core::i18n::plural(
            missing,
            nohrs_core::k!("{n} arquivo não encontrado no disco"),
            nohrs_core::k!("{n} arquivos não encontrados no disco"),
        );
        out.push_str(&format!("**{line}**\n\n"));
    }
    for (present, path) in rows {
        let name = Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or(path.clone());
        let mark = if present { "" } else { "⚠ " };
        // Name, then where it is, on one line: the preview's Markdown keeps
        // neither a line break inside an item nor inline code apart.
        out.push_str(&format!(
            "- {mark}**{}** — {}\n",
            name.replace('*', "\\*"),
            path.replace('*', "\\*")
        ));
    }
    if total > MAX_MEDIA {
        out.push_str(&format!(
            "\n{}\n",
            nohrs_core::tf!("… e mais {n}", n = total - MAX_MEDIA)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 4 x 2 RGB Photoshop file, PackBits rows, as Photoshop writes one.
    fn tiny_psd() -> Vec<u8> {
        let mut file = Vec::new();
        file.extend_from_slice(b"8BPS");
        file.extend_from_slice(&1u16.to_be_bytes());
        file.extend_from_slice(&[0; 6]);
        file.extend_from_slice(&3u16.to_be_bytes());
        file.extend_from_slice(&2u32.to_be_bytes());
        file.extend_from_slice(&4u32.to_be_bytes());
        file.extend_from_slice(&8u16.to_be_bytes());
        file.extend_from_slice(&3u16.to_be_bytes());
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&1u16.to_be_bytes());
        // Each row: one run of four (header -3, value).
        for _ in 0..6 {
            file.extend_from_slice(&2u16.to_be_bytes());
        }
        for value in [200u8, 200, 10, 10, 50, 50] {
            file.extend_from_slice(&[(-3i8) as u8, value]);
        }
        file
    }

    #[test]
    fn a_photoshop_composite_is_decoded_and_fitted() {
        let folder = tempfile::tempdir().expect("temp folder");
        let path = folder.path().join("a.psd");
        std::fs::write(&path, tiny_psd()).expect("write");
        let Creative::Picture {
            width,
            height,
            bgra,
            ..
        } = read(&path.to_string_lossy(), 512).expect("read")
        else {
            panic!("not a picture");
        };
        assert_eq!((width, height), (4, 2));
        // Red 200, green 10, blue 50, opaque, as BGRA.
        assert_eq!(&bgra[..4], &[50, 10, 200, 255]);
        let Creative::Picture { width, height, .. } =
            read(&path.to_string_lossy(), 2).expect("read")
        else {
            panic!("not a picture");
        };
        assert_eq!((width, height), (2, 1));
    }

    #[test]
    fn packbits_runs_and_literals() {
        let mut row = [0u8; 6];
        unpack_bits(&[(-2i8) as u8, 7, 2, 1, 2, 3], &mut row);
        assert_eq!(row, [7, 7, 7, 1, 2, 3]);
    }

    #[test]
    fn a_premiere_project_lists_sequences_and_missing_media() {
        let folder = tempfile::tempdir().expect("temp folder");
        let present = folder.path().join("clip.mp4");
        std::fs::write(&present, "x").expect("write");
        let xml = format!(
            "<PremiereData><Project ObjectID=\"1\" Version=\"43\"/><Sequence ObjectUID=\"a\"><Name>Corte final</Name></Sequence>\
             <Media><ActualMediaFilePath>{}</ActualMediaFilePath></Media><Media><ActualMediaFilePath>Z:\\sumiu.wav</ActualMediaFilePath></Media></PremiereData>",
            present.display()
        );
        let path = folder.path().join("a.prproj");
        let mut encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).expect("create"),
            flate2::Compression::fast(),
        );
        std::io::Write::write_all(&mut encoder, xml.as_bytes()).expect("write");
        encoder.finish().expect("finish");
        let Creative::Summary(text) = read(&path.to_string_lossy(), 512).expect("read") else {
            panic!("not a summary");
        };
        assert!(text.contains("- Corte final"), "{text}");
        assert!(text.contains("⚠ **sumiu.wav**"), "{text}");
        assert!(text.contains("- **clip.mp4**"), "{text}");
        // The missing file comes first.
        assert!(text.find("sumiu.wav") < text.find("clip.mp4"), "{text}");
    }

    #[test]
    fn base64_round_trips_a_few_bytes() {
        assert_eq!(decode_base64("TWFu").as_deref(), Some(&b"Man"[..]));
        assert_eq!(decode_base64("TWE=").as_deref(), Some(&b"Ma"[..]));
    }
}
