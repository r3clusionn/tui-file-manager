//! Previews of the entry under the cursor, built on worker threads so that a slow disk, a large
//! directory or a network share never holds up a key press.
//!
//! A preview is a few header lines (what the file is) and a body (its first lines, a hex dump, the
//! names in a directory or an archive). [`Previewer`] keeps a small cache and a pool of workers that
//! take requests from a single slot: a new request replaces one that no worker has started, so
//! scrolling past a hundred files does not queue a hundred previews.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::entry::{self, human_size, Kind, SortOrder};

/// How a preview line is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Header,
    Dim,
    Dir,
    Link,
    Exec,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub tone: Tone,
}

impl Line {
    fn new(text: impl Into<String>, tone: Tone) -> Line {
        Line { text: text.into(), tone }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preview {
    pub lines: Vec<Line>,
}

/// What a preview depends on besides the file's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Options {
    /// Width of the preview column; hex dumps fit their bytes per row to it.
    pub width: u16,
    pub show_hidden: bool,
    pub order: SortOrder,
}

/// Limits that keep a preview cheap whatever the file is.
const TEXT_BYTES: usize = 64 * 1024;
const MAX_LINES: usize = 300;
const HEX_BYTES: usize = 1024;
const MAX_ENTRIES: usize = 1000;

/// Builds a preview synchronously. This is what the workers run.
pub fn build(path: &Path, opts: &Options) -> Preview {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            // A broken link still has its own metadata.
            if let Ok(target) = fs::read_link(path) {
                return Preview { lines: vec![Line::new(format!("broken link to {}", target.display()), Tone::Error)] };
            }
            return error(&e);
        }
    };
    let mut lines = Vec::new();
    if let Ok(target) = fs::read_link(path) {
        lines.push(Line::new(format!("-> {}", target.display()), Tone::Link));
    }
    if meta.is_dir() {
        lines.extend(dir_preview(path, opts).lines);
        return Preview { lines };
    }
    match file_preview(path, meta.len(), opts) {
        Ok(p) => lines.extend(p.lines),
        Err(e) => lines.extend(error(&e).lines),
    }
    Preview { lines }
}

fn error(e: &io::Error) -> Preview {
    Preview { lines: vec![Line::new(e.to_string(), Tone::Error)] }
}

fn dir_preview(path: &Path, opts: &Options) -> Preview {
    let mut entries = match entry::read_dir(path) {
        Ok(v) => v,
        Err(e) => return error(&e),
    };
    let total = entries.len();
    if !opts.show_hidden {
        entries.retain(|e| !e.hidden);
    }
    let hidden = total - entries.len();
    entry::sort(&mut entries, opts.order);
    let mut lines = Vec::with_capacity(entries.len().min(MAX_ENTRIES) + 1);
    if entries.is_empty() {
        lines.push(Line::new(if hidden > 0 { format!("{hidden} hidden") } else { "empty".to_string() }, Tone::Dim));
    }
    for e in entries.iter().take(MAX_ENTRIES) {
        let tone = match e.kind {
            Kind::Dir => Tone::Dir,
            Kind::Link { .. } => Tone::Link,
            _ if e.executable => Tone::Exec,
            _ => Tone::Plain,
        };
        lines.push(Line::new(e.name.clone(), tone));
    }
    if entries.len() > MAX_ENTRIES {
        lines.push(Line::new(format!("... {} more", entries.len() - MAX_ENTRIES), Tone::Dim));
    }
    Preview { lines }
}

fn file_preview(path: &Path, len: u64, opts: &Options) -> io::Result<Preview> {
    let mut f = File::open(path)?;
    let mut head = vec![0u8; TEXT_BYTES.min(len as usize)];
    let n = read_full(&mut f, &mut head)?;
    head.truncate(n);
    let mut lines = Vec::new();
    if len == 0 {
        lines.push(Line::new("empty file", Tone::Dim));
        return Ok(Preview { lines });
    }
    // Things recognised by their first bytes get a description; images and archives say more.
    let kind = sniff(&head);
    if let Some(mut desc) = image_info(&head) {
        // Metadata (EXIF, ICC profiles, thumbnails) can push a JPEG's frame header past the
        // sample; then follow the segments through the file itself.
        if desc == "JPEG image" && (n as u64) < len {
            if let Some(d) = jpeg_from_file(&mut f) {
                desc = d;
            }
        }
        if desc.starts_with("ICO image") {
            if let Some(d) = ico_from_file(&mut f, &head) {
                desc = d;
            }
        }
        // The terminal cannot show the picture; its format and size are what matter.
        lines.push(Line::new(desc, Tone::Header));
        return Ok(Preview { lines });
    } else if let Some(k) = kind.clone() {
        lines.push(Line::new(k, Tone::Header));
    }
    // Zip archives are found by their first bytes or their extension (an archive can have data in
    // front of it); a program may be a self-extracting archive, so try those quietly too.
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let zip_ext = ["zip", "jar", "docx", "xlsx", "pptx", "odt", "ods", "epub", "apk", "nupkg", "whl", "vsix", "crx"]
        .contains(&ext.as_str());
    let starts_zip = head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06");
    let program = head.starts_with(b"MZ");
    if starts_zip || zip_ext || program {
        match zip_list(&mut f, len) {
            Ok(list) if !list.is_empty() || !program => {
                if !starts_zip && kind.is_none() {
                    lines.push(Line::new("Zip archive", Tone::Header));
                }
                lines.push(Line::new(
                    format!("{} entries{}", list.len(), if program { " (self-extracting archive)" } else { "" }),
                    Tone::Header,
                ));
                for (name, size) in list.iter().take(MAX_ENTRIES) {
                    let tone = if name.ends_with('/') { Tone::Dir } else { Tone::Plain };
                    lines.push(Line::new(format!("{name}  {}", human_size(*size)), tone));
                }
                return Ok(Preview { lines });
            }
            Ok(_) => {}
            Err(_) if program => {}
            Err(e) => lines.push(Line::new(format!("zip: {e}"), Tone::Error)),
        }
    }
    if let Some(text) = decode_text(&head) {
        lines.extend(text_lines(&text, (n as u64) < len).into_iter().map(|t| Line::new(t, Tone::Plain)));
    } else {
        lines.extend(
            hex_dump(&head[..head.len().min(HEX_BYTES)], opts.width as usize).into_iter().map(|t| Line::new(t, Tone::Dim)),
        );
    }
    Ok(Preview { lines })
}

fn read_full(f: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Returns the text if the bytes look like text: UTF-8 (a character cut at the end of the sample
/// is allowed) or UTF-16 with a byte order mark, and no NUL bytes.
pub fn decode_text(b: &[u8]) -> Option<String> {
    if let Some(rest) = b.strip_prefix(&[0xFF, 0xFE]) {
        return Some(String::from_utf16_lossy(
            &rest.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect::<Vec<_>>(),
        ));
    }
    if let Some(rest) = b.strip_prefix(&[0xFE, 0xFF]) {
        return Some(String::from_utf16_lossy(
            &rest.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect::<Vec<_>>(),
        ));
    }
    let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b);
    if b.contains(&0) {
        return None;
    }
    match std::str::from_utf8(b) {
        Ok(s) => Some(s.to_string()),
        // Only an incomplete character at the very end is forgiven (the sample was cut there).
        Err(e) if e.error_len().is_none() => Some(String::from_utf8_lossy(&b[..e.valid_up_to()]).into_owned()),
        Err(_) => {
            // Mostly-printable single-byte text (Latin-1 and similar) still reads better as text.
            let printable = b.iter().filter(|&&c| c >= 0x20 || c == b'\n' || c == b'\r' || c == b'\t').count();
            if printable * 100 >= b.len() * 95 {
                Some(b.iter().map(|&c| c as char).collect())
            } else {
                None
            }
        }
    }
}

/// Splits text into display lines: tabs become spaces, other control characters become `.`.
fn text_lines(text: &str, cut: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = text.split('\n').peekable();
    while let Some(raw) = lines.next() {
        if out.len() == MAX_LINES {
            break;
        }
        // The last piece of a cut sample is an incomplete line; leave it out.
        if cut && lines.peek().is_none() {
            break;
        }
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let mut line = String::with_capacity(raw.len());
        for c in raw.chars() {
            match c {
                '\t' => {
                    let w = unicode_width::UnicodeWidthStr::width(line.as_str());
                    line.push_str(&" ".repeat(4 - w % 4));
                }
                c if c.is_control() => line.push('.'),
                c => line.push(c),
            }
        }
        out.push(line);
    }
    if text.ends_with('\n') && !cut {
        out.pop();
    }
    out
}

/// A hex dump with as many bytes per row (4, 8 or 16) as fit in `width` columns.
pub fn hex_dump(b: &[u8], width: usize) -> Vec<String> {
    // Each row: 8 offset digits, 2 spaces, 3 columns per byte, 1 space, 1 column per byte.
    let per = [16, 8, 4].into_iter().find(|&n| 8 + 2 + 4 * n < width).unwrap_or(4);
    b.chunks(per)
        .enumerate()
        .map(|(i, row)| {
            let hex: String = row.iter().map(|x| format!("{x:02x} ")).collect();
            let text: String = row.iter().map(|&x| if (0x20..0x7f).contains(&x) { x as char } else { '.' }).collect();
            format!("{:08x}  {hex:<w$} {text}", i * per, w = per * 3)
        })
        .collect()
}

/// Names common file types from their first bytes.
pub fn sniff(b: &[u8]) -> Option<String> {
    let s = |x: &str| Some(x.to_string());
    if b.starts_with(b"MZ") {
        return s("PE executable or DLL (MZ)");
    }
    if b.starts_with(b"\x7fELF") {
        let class = match b.get(4) {
            Some(1) => "32-bit",
            Some(2) => "64-bit",
            _ => "",
        };
        return Some(format!("ELF {class}").trim_end().to_string());
    }
    if b.starts_with(b"%PDF-") {
        let v: String = b[5..].iter().take_while(|c| c.is_ascii_digit() || **c == b'.').map(|&c| c as char).collect();
        return Some(format!("PDF document, version {v}"));
    }
    if b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06") {
        return s("Zip archive");
    }
    if b.starts_with(&[0x1f, 0x8b]) {
        return s("gzip compressed data");
    }
    if b.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return s("7-Zip archive");
    }
    if b.starts_with(b"Rar!\x1a\x07") {
        return s("RAR archive");
    }
    if b.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return s("Zstandard compressed data");
    }
    if b.starts_with(b"\xfd7zXZ\x00") {
        return s("XZ compressed data");
    }
    if b.len() >= 12 && &b[4..8] == b"ftyp" {
        let brand = String::from_utf8_lossy(&b[8..12]).trim().to_string();
        return Some(format!("ISO media (MP4/MOV), brand {brand}"));
    }
    if b.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return s("Matroska or WebM video");
    }
    if b.len() >= 12 && b.starts_with(b"RIFF") {
        return Some(match &b[8..12] {
            b"WAVE" => "WAVE audio".to_string(),
            b"AVI " => "AVI video".to_string(),
            b"WEBP" => "WebP image".to_string(),
            f => format!("RIFF {}", String::from_utf8_lossy(f)),
        });
    }
    if b.starts_with(b"ID3") || (b.len() > 1 && b[0] == 0xff && b[1] & 0xe0 == 0xe0) {
        return s("MP3 audio");
    }
    if b.starts_with(b"fLaC") {
        return s("FLAC audio");
    }
    if b.starts_with(b"OggS") {
        return s("Ogg");
    }
    if b.starts_with(b"SQLite format 3\0") {
        return s("SQLite database");
    }
    if b.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") {
        return s("OLE compound file (old Office document or MSI)");
    }
    None
}

/// Describes PNG, JPEG, GIF, BMP and WebP images from their headers: format, size and depth.
pub fn image_info(b: &[u8]) -> Option<String> {
    let be16 = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]) as u32;
    let le16 = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as u32;
    let be32 = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let le32 = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    if b.len() >= 26 && b.starts_with(b"\x89PNG\r\n\x1a\n") && &b[12..16] == b"IHDR" {
        let color = match b[25] {
            0 => "grey",
            2 => "RGB",
            3 => "palette",
            4 => "grey and alpha",
            6 => "RGBA",
            _ => "unknown colour type",
        };
        return Some(format!("PNG image, {} x {}, {}-bit {color}", be32(16), be32(20), b[24]));
    }
    if b.len() >= 22 && b.starts_with(&[0, 0, 1, 0]) {
        // An icon directory: count, then 16 bytes per image whose first two bytes are the width
        // and height (0 meaning 256). Report the largest, as image viewers do.
        let count = le16(4) as usize;
        let entries_end = 6 + 16 * count;
        if (1..=256).contains(&count) && b.len() >= entries_end && le32(18) as usize >= entries_end {
            let dims = |i: usize| {
                let e = 6 + 16 * i;
                let d = |x: u8| if x == 0 { 256 } else { x as u32 };
                (d(b[e]), d(b[e + 1]))
            };
            let (w, h) = (0..count).map(dims).max_by_key(|&(w, h)| w * h).unwrap_or((0, 0));
            let images = if count == 1 { "1 image".to_string() } else { format!("{count} images") };
            return Some(format!("ICO image, {w} x {h}, {images}"));
        }
    }
    if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return Some(format!("GIF image, {} x {}", le16(6), le16(8)));
    }
    if b.len() >= 26 && b.starts_with(b"BM") {
        let header = le32(14);
        if header == 12 {
            return Some(format!("BMP image, {} x {}, {}-bit", le16(18), le16(20), le16(24)));
        }
        if header >= 40 && b.len() >= 30 {
            let h = le32(22) as i32;
            return Some(format!("BMP image, {} x {}, {}-bit", le32(18) as i32, h.unsigned_abs(), le16(28)));
        }
    }
    if b.len() >= 30 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        let (w, h) = match &b[12..16] {
            b"VP8 " if b.len() >= 30 => (le16(26) & 0x3fff, le16(28) & 0x3fff),
            b"VP8L" if b.len() >= 25 => {
                let bits = le32(21);
                ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            }
            b"VP8X" => {
                let w = (b[24] as u32 | (b[25] as u32) << 8 | (b[26] as u32) << 16) + 1;
                let h = (b[27] as u32 | (b[28] as u32) << 8 | (b[29] as u32) << 16) + 1;
                (w, h)
            }
            _ => return Some("WebP image".to_string()),
        };
        return Some(format!("WebP image, {w} x {h}"));
    }
    if b.len() >= 4 && b.starts_with(&[0xff, 0xd8, 0xff]) {
        // Walk the marker segments to the first start-of-frame. Every segment from APP0 on has a
        // length, so the walk is bounded by the bytes we have.
        let mut i = 2;
        while i + 4 <= b.len() {
            if b[i] != 0xff {
                return Some("JPEG image".to_string());
            }
            let m = b[i + 1];
            if m == 0xff {
                i += 1;
                continue;
            }
            if matches!(m, 0xd0..=0xd9 | 0x01) {
                i += 2;
                continue;
            }
            let seg = be16(i + 2) as usize;
            let sof = matches!(m, 0xc0..=0xcf) && !matches!(m, 0xc4 | 0xc8 | 0xcc);
            if sof && i + 9 < b.len() {
                let kind = match m {
                    0xc0 | 0xc1 => "baseline",
                    0xc2 | 0xc6 | 0xca | 0xce => "progressive",
                    _ => "lossless or hierarchical",
                };
                let comps = b[i + 9];
                return Some(format!("JPEG image, {} x {}, {kind}, {comps} components", be16(i + 7), be16(i + 5)));
            }
            i += 2 + seg;
        }
        return Some("JPEG image".to_string());
    }
    None
}

/// Walks a JPEG's marker segments by seeking through the file, for frame headers that lie beyond
/// the sample. Stops after 10,000 segments or at the start of the image data.
fn jpeg_from_file(f: &mut File) -> Option<String> {
    let mut pos = 2u64;
    for _ in 0..10_000 {
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut h = [0u8; 10];
        let n = read_full(f, &mut h).ok()?;
        if n < 4 || h[0] != 0xff {
            return None;
        }
        let m = h[1];
        if m == 0xff {
            pos += 1;
            continue;
        }
        if matches!(m, 0xd0..=0xd9 | 0x01) {
            if m == 0xd9 || m == 0xda {
                return None;
            }
            pos += 2;
            continue;
        }
        if m == 0xda {
            return None;
        }
        if n == 10 {
            if let Some(d) = image_info(&[&[0xff, 0xd8][..], &h].concat()) {
                if d != "JPEG image" {
                    return Some(d);
                }
            }
        }
        pos += 2 + u16::from_be_bytes([h[2], h[3]]) as u64;
    }
    None
}

/// The icon shown is the one the directory lists as largest (as Windows and Pillow choose), but a
/// directory can only say "256 or more" for a side, so its real size comes from the image's own
/// header: a PNG's IHDR, or a BMP header whose height counts the transparency mask too.
fn ico_from_file(f: &mut File, head: &[u8]) -> Option<String> {
    let count = u16::from_le_bytes([head[4], head[5]]) as usize;
    let d = |x: u8| if x == 0 { 256 } else { x as u32 };
    let entry = |i: usize| &head[6 + 16 * i..6 + 16 * (i + 1)];
    // Largest listed area first; among equals, the first listed.
    let pick = (0..count).rev().max_by_key(|&i| d(entry(i)[0]) * d(entry(i)[1]))?;
    let e = entry(pick);
    let mut dims = (d(e[0]), d(e[1]));
    let off = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64;
    let mut h = [0u8; 24];
    if f.seek(SeekFrom::Start(off)).is_ok() && read_full(f, &mut h).ok() == Some(24) {
        if h.starts_with(b"\x89PNG\r\n\x1a\n") {
            dims = (u32::from_be_bytes(h[16..20].try_into().unwrap()), u32::from_be_bytes(h[20..24].try_into().unwrap()));
        } else if u32::from_le_bytes(h[0..4].try_into().unwrap()) == 40 {
            let w = i32::from_le_bytes(h[4..8].try_into().unwrap()).unsigned_abs();
            let hh = i32::from_le_bytes(h[8..12].try_into().unwrap()).unsigned_abs() / 2;
            dims = (w, hh);
        }
    }
    let images = if count == 1 { "1 image".to_string() } else { format!("{count} images") };
    Some(format!("ICO image, {} x {}, {images}", dims.0, dims.1))
}

/// Lists the names and uncompressed sizes in a zip file from its central directory, including
/// zip64 archives. Names without the UTF-8 flag are decoded as code page 437, as the format says.
pub fn zip_list(f: &mut File, len: u64) -> io::Result<Vec<(String, u64)>> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    // The end record is 22 bytes plus a comment of up to 65535 bytes.
    let tail_len = len.min(22 + 65_535 + 20);
    f.seek(SeekFrom::Start(len - tail_len))?;
    let mut tail = vec![0; tail_len as usize];
    f.read_exact(&mut tail)?;
    let eocd = (0..=tail.len().saturating_sub(22))
        .rev()
        .find(|&i| tail[i..].starts_with(b"PK\x05\x06"))
        .ok_or_else(|| bad("no end of central directory record"))?;
    let le16 = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as u64;
    let le32 = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as u64;
    let le64 = |b: &[u8], i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    let eocd_pos = len - tail_len + eocd as u64;
    let mut count = le16(&tail, eocd + 10);
    let mut cd_size = le32(&tail, eocd + 12);
    // The central directory ends where the end records begin. Locating it from there, rather
    // than by its stored offset, also reads archives with data in front of them (self-extracting
    // programs), as Python's zipfile and unzip do.
    let mut records_start = eocd_pos;
    // A zip64 locator sits right before the end record when any of the counts overflowed; the
    // zip64 end record (56 bytes) sits right before the locator.
    if eocd >= 20 && tail[eocd - 20..].starts_with(b"PK\x06\x07") && eocd_pos >= 76 {
        let mut rec = [0u8; 56];
        f.seek(SeekFrom::Start(eocd_pos - 76))?;
        f.read_exact(&mut rec)?;
        if !rec.starts_with(b"PK\x06\x06") {
            return Err(bad("bad zip64 end record"));
        }
        count = le64(&rec, 32);
        cd_size = le64(&rec, 40);
        records_start = eocd_pos - 76;
    }
    if cd_size > 256 << 20 || cd_size > records_start {
        return Err(bad("central directory out of range"));
    }
    let cd_off = records_start - cd_size;
    f.seek(SeekFrom::Start(cd_off))?;
    let mut cd = vec![0; cd_size as usize];
    f.read_exact(&mut cd)?;
    let mut out = Vec::with_capacity(count.min(1 << 20) as usize);
    let mut i = 0;
    while i + 46 <= cd.len() && cd[i..].starts_with(b"PK\x01\x02") {
        let flags = le16(&cd, i + 8);
        let mut size = le32(&cd, i + 24);
        let n = le16(&cd, i + 28) as usize;
        let x = le16(&cd, i + 30) as usize;
        let c = le16(&cd, i + 32) as usize;
        if i + 46 + n + x + c > cd.len() {
            return Err(bad("truncated central directory entry"));
        }
        let raw = &cd[i + 46..i + 46 + n];
        let name = if flags & 0x800 != 0 { String::from_utf8_lossy(raw).into_owned() } else { cp437(raw) };
        // The format says "/" separates folders; some Windows tools store "\" anyway.
        let name = name.replace('\\', "/");
        if size == 0xffff_ffff {
            // The zip64 extra field holds the real size first.
            let extra = &cd[i + 46 + n..i + 46 + n + x];
            let mut j = 0;
            while j + 4 <= extra.len() {
                let id = le16(extra, j);
                let l = le16(extra, j + 2) as usize;
                if id == 1 && l >= 8 && j + 12 <= extra.len() {
                    size = le64(extra, j + 4);
                    break;
                }
                j += 4 + l;
            }
        }
        out.push((name, size));
        i += 46 + n + x + c;
    }
    Ok(out)
}

fn cp437(b: &[u8]) -> String {
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";
    let high: Vec<char> = HIGH.chars().collect();
    b.iter().map(|&c| if c < 0x80 { c as char } else { high[c as usize - 0x80] }).collect()
}

/// The cache key: a preview is reused only while the file's size and time are unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    opts: Options,
}

struct Job {
    key: Key,
}

struct Slot {
    next: Option<Job>,
    stop: bool,
}

/// Asynchronous previews with a cache.
pub struct Previewer {
    slot: Arc<(Mutex<Slot>, Condvar)>,
    done_rx: Receiver<(Key, Preview)>,
    cache: HashMap<Key, Arc<Preview>>,
    order: Vec<Key>,
    waiting: Option<Key>,
    workers: Vec<thread::JoinHandle<()>>,
}

const CACHE: usize = 128;

impl Previewer {
    /// Starts `threads` workers. `delay` is added to every preview, to measure and test how the
    /// interface behaves with slow storage.
    pub fn new(threads: usize, delay: Duration) -> Previewer {
        let slot = Arc::new((Mutex::new(Slot { next: None, stop: false }), Condvar::new()));
        let (done_tx, done_rx) = mpsc::channel();
        let workers = (0..threads.max(1))
            .map(|_| {
                let slot = Arc::clone(&slot);
                let tx: Sender<(Key, Preview)> = done_tx.clone();
                thread::spawn(move || worker(slot, tx, delay))
            })
            .collect();
        Previewer { slot, done_rx, cache: HashMap::new(), order: Vec::new(), waiting: None, workers }
    }

    /// Returns the preview for `path` if it is ready; otherwise asks a worker for it and returns
    /// `None`. `len` and `modified` come from the listing and decide whether a cached preview is
    /// still valid.
    pub fn get(&mut self, path: &Path, len: u64, modified: Option<SystemTime>, opts: Options) -> Option<Arc<Preview>> {
        self.collect();
        let key = Key { path: path.to_path_buf(), len, modified, opts };
        if let Some(p) = self.cache.get(&key) {
            return Some(Arc::clone(p));
        }
        if self.waiting.as_ref() != Some(&key) {
            let (lock, cv) = &*self.slot;
            lock.lock().unwrap().next = Some(Job { key: key.clone() });
            cv.notify_one();
            self.waiting = Some(key);
        }
        None
    }

    /// Takes finished previews into the cache. Returns true if the one being waited for arrived.
    pub fn collect(&mut self) -> bool {
        let mut arrived = false;
        while let Ok((key, p)) = self.done_rx.try_recv() {
            arrived |= self.waiting.as_ref() == Some(&key);
            self.insert(key, p);
        }
        if arrived {
            self.waiting = None;
        }
        arrived
    }

    /// True while a requested preview has not arrived.
    pub fn is_waiting(&self) -> bool {
        self.waiting.is_some()
    }

    /// Blocks until the awaited preview arrives or `timeout` passes.
    pub fn wait(&mut self, timeout: Duration) {
        let end = Instant::now() + timeout;
        while let Some(want) = self.waiting.clone() {
            let left = end.saturating_duration_since(Instant::now());
            match self.done_rx.recv_timeout(left) {
                Ok((key, p)) => {
                    let hit = key == want;
                    self.insert(key, p);
                    if hit {
                        self.waiting = None;
                    }
                }
                Err(_) => return,
            }
        }
    }

    /// Forgets cached previews (after files changed under us).
    pub fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
    }

    fn insert(&mut self, key: Key, p: Preview) {
        if self.cache.len() >= CACHE {
            let old = self.order.remove(0);
            self.cache.remove(&old);
        }
        self.order.push(key.clone());
        self.cache.insert(key, Arc::new(p));
    }
}

impl Drop for Previewer {
    fn drop(&mut self) {
        let (lock, cv) = &*self.slot;
        lock.lock().unwrap().stop = true;
        cv.notify_all();
        // Workers busy on a slow file are not joined: dropping must not wait for them.
        self.workers.clear();
    }
}

fn worker(slot: Arc<(Mutex<Slot>, Condvar)>, tx: Sender<(Key, Preview)>, delay: Duration) {
    let (lock, cv) = &*slot;
    loop {
        let job = {
            let mut s = lock.lock().unwrap();
            loop {
                if s.stop {
                    return;
                }
                if let Some(j) = s.next.take() {
                    break j;
                }
                s = cv.wait(s).unwrap();
            }
        };
        if !delay.is_zero() {
            thread::sleep(delay);
        }
        let p = build(&job.key.path, &job.key.opts);
        if tx.send((job.key, p)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_rows_fit_the_width() {
        let b: Vec<u8> = (0..40).collect();
        let rows = hex_dump(&b, 80);
        assert_eq!(rows[0], "00000000  00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f  ................");
        assert_eq!(rows.len(), 3);
        assert!(hex_dump(&b, 50).iter().all(|r| r.len() <= 50));
        assert_eq!(hex_dump(&b, 50).len(), 5);
        assert_eq!(hex_dump(b"AB", 30)[0], "00000000  41 42        AB");
    }

    #[test]
    fn text_detection() {
        assert_eq!(decode_text(b"hello\nworld\n").as_deref(), Some("hello\nworld\n"));
        assert_eq!(decode_text(b"\xef\xbb\xbfbom").as_deref(), Some("bom"));
        assert_eq!(decode_text(b"\xff\xfeh\0i\0").as_deref(), Some("hi"));
        assert!(decode_text(b"MZ\x90\0\x03").is_none());
        // A UTF-8 character cut at the end of the sample is not a reason to call it binary.
        assert_eq!(decode_text("ab\u{e9}".as_bytes().split_last().unwrap().1).as_deref(), Some("ab"));
        assert_eq!(decode_text(b"caf\xe9 au lait").as_deref(), Some("caf\u{e9} au lait"));
    }

    #[test]
    fn text_lines_expand_tabs_and_drop_a_cut_line() {
        assert_eq!(text_lines("a\tb\r\nc\x07\n", false), ["a   b", "c."]);
        assert_eq!(text_lines("one\ntwo\nthr", true), ["one", "two"]);
        assert_eq!(text_lines("x", false), ["x"]);
    }

    #[test]
    fn images() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend(640u32.to_be_bytes());
        png.extend(480u32.to_be_bytes());
        png.extend([8, 6, 0, 0, 0]);
        assert_eq!(image_info(&png).unwrap(), "PNG image, 640 x 480, 8-bit RGBA");
        let gif = b"GIF89a\x40\x01\xf0\x00\x00\x00";
        assert_eq!(image_info(gif).unwrap(), "GIF image, 320 x 240");
        let mut jpg = vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc2, 0, 17, 8];
        jpg.extend(1080u16.to_be_bytes());
        jpg.extend(1920u16.to_be_bytes());
        jpg.extend([3, 0, 0, 0]);
        assert_eq!(image_info(&jpg).unwrap(), "JPEG image, 1920 x 1080, progressive, 3 components");
    }

    #[test]
    fn cp437_names() {
        assert_eq!(cp437(b"caf\x82.txt"), "café.txt");
        assert_eq!(cp437(&[0xff]), "\u{a0}");
    }
}
