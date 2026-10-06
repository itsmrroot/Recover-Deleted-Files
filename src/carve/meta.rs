//! Names and dates for carved files, read from metadata inside them.
//!
//! A carved file has no name of its own, so instead of `f0001a2b3000.jpg`
//! it is named after what it says about itself: when and with which camera
//! a photo was taken (EXIF), when a video was recorded (MP4/MOV `mvhd`),
//! a song's artist and title (ID3) or a PDF's title. The date also lets
//! carved files be sorted and filtered by date.
//!
//! Everything here is best effort and defensive: damaged metadata simply
//! yields nothing.

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime};

use super::Reader;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// When the content was made (photo taken, video recorded, ...).
    pub date: Option<NaiveDateTime>,
    /// A file name without extension, already safe to use.
    pub title: Option<String>,
}

/// Reads the metadata of a carved file of `format` and length `len`.
pub fn read(format: &str, ext: &str, r: &mut Reader, len: u64) -> Meta {
    let meta = match format {
        "jpeg" => jpeg(r, len),
        "tiff" => tiff(r, 0).map(photo_meta).unwrap_or_default(),
        "bmff" if !matches!(ext, "heic" | "avif") => Meta { date: bmff_date(r, len), title: None },
        "mp3" => Meta { date: None, title: id3_title(r) },
        "pdf" => pdf(r, len),
        _ => Meta::default(),
    };
    Meta { date: meta.date.filter(plausible), title: meta.title.as_deref().and_then(sanitize) }
}

/// Rejects the zero dates and clock resets devices write when they do not
/// know the time.
fn plausible(d: &NaiveDateTime) -> bool {
    (1990..=2100).contains(&d.year())
}

/// Makes `s` usable as a file name on every system.
fn sanitize(s: &str) -> Option<String> {
    let cleaned: String =
        s.chars()
            .map(|c| {
                if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                    ' '
                } else {
                    c
                }
            })
            .collect();
    let mut out = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut = out.len().min(100);
    while !out.is_char_boundary(cut) {
        cut -= 1;
    }
    out.truncate(cut);
    let out = out.trim().trim_end_matches('.').trim().to_string();
    (!out.is_empty()).then_some(out)
}

// ---------------------------------------------------------------------------
// EXIF (JPEG and TIFF-based RAW)

#[derive(Debug, Default)]
struct Exif {
    make: Option<String>,
    model: Option<String>,
    taken: Option<NaiveDateTime>,
    modified: Option<NaiveDateTime>,
}

/// "2023-07-14 15.32.10 Canon EOS 80D".
fn photo_meta(e: Exif) -> Meta {
    let date = e.taken.or(e.modified);
    let camera = match (e.make.as_deref(), e.model.as_deref()) {
        (Some(make), Some(model)) => {
            let first = make.split_whitespace().next().unwrap_or(make);
            if model.to_ascii_lowercase().starts_with(&first.to_ascii_lowercase()) {
                Some(model.to_string())
            } else {
                Some(format!("{make} {model}"))
            }
        }
        (None, Some(m)) | (Some(m), None) => Some(m.to_string()),
        (None, None) => None,
    };
    let stamp = date.map(|d| d.format("%Y-%m-%d %H.%M.%S").to_string());
    let title = match (stamp, camera) {
        (Some(s), Some(c)) => Some(format!("{s} {c}")),
        (Some(s), None) => Some(s),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    };
    Meta { date, title }
}

/// Finds the EXIF block (APP1 "Exif") among the segments before the image data.
fn jpeg(r: &mut Reader, len: u64) -> Meta {
    let mut pos = 2u64;
    let end = len.min(256 << 10);
    while pos + 4 <= end {
        let (Some(0xFF), Some(m)) = (r.u8(pos), r.u8(pos + 1)) else { break };
        if m == 0xDA || m == 0xD9 {
            break;
        }
        let Some(seg) = r.be16(pos + 2).map(u64::from) else { break };
        if seg < 2 {
            break;
        }
        if m == 0xE1 && r.starts_with(pos + 4, b"Exif\0\0") {
            return tiff(r, pos + 10).map(photo_meta).unwrap_or_default();
        }
        pos += 2 + seg;
    }
    Meta::default()
}

/// Reads camera and dates from a TIFF structure starting at `base`.
fn tiff(r: &mut Reader, base: u64) -> Option<Exif> {
    let le = if r.starts_with(base, b"II*\0") {
        true
    } else if r.starts_with(base, b"MM\0*") {
        false
    } else {
        return None;
    };
    let u16_at = |r: &mut Reader, p: u64| if le { r.le16(base + p) } else { r.be16(base + p) };
    let u32_at = |r: &mut Reader, p: u64| if le { r.le32(base + p) } else { r.be32(base + p) };
    let ascii = |r: &mut Reader, entry: u64| -> Option<String> {
        let count = u64::from(u32_at(r, entry + 4)?);
        if count == 0 || count > 256 {
            return None;
        }
        let at = if count <= 4 { entry + 8 } else { u64::from(u32_at(r, entry + 8)?) };
        let bytes = r.bytes(base + at, count as usize)?;
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let s = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let date = |s: Option<String>| s.and_then(|s| NaiveDateTime::parse_from_str(s.trim(), "%Y:%m:%d %H:%M:%S").ok());

    let mut exif = Exif::default();
    let mut exif_ifd = None;
    let ifd0 = u64::from(u32_at(r, 4)?);
    let n = u64::from(u16_at(r, ifd0)?);
    if n == 0 || n > 1000 {
        return None;
    }
    for i in 0..n {
        let entry = ifd0 + 2 + i * 12;
        match u16_at(r, entry)? {
            0x010F => exif.make = ascii(r, entry),
            0x0110 => exif.model = ascii(r, entry),
            0x0132 => exif.modified = date(ascii(r, entry)),
            0x8769 => exif_ifd = u32_at(r, entry + 8).map(u64::from),
            _ => {}
        }
    }
    if let Some(ifd) = exif_ifd {
        let n = u64::from(u16_at(r, ifd).unwrap_or(0));
        for i in 0..n.min(1000) {
            let entry = ifd + 2 + i * 12;
            if u16_at(r, entry) == Some(0x9003) {
                exif.taken = date(ascii(r, entry));
            }
        }
    }
    Some(exif)
}

// ---------------------------------------------------------------------------
// MP4 / MOV

/// Creation time from `moov/mvhd` (seconds since 1904-01-01 UTC).
fn bmff_date(r: &mut Reader, len: u64) -> Option<NaiveDateTime> {
    let moov = find_box(r, 0, len, b"moov")?;
    let mvhd = find_box(r, moov.0, moov.1, b"mvhd")?;
    let start = mvhd.0;
    let secs = match r.u8(start)? {
        0 => u64::from(r.be32(start + 4)?),
        1 => r.be64(start + 4)?,
        _ => return None,
    };
    const MAC_TO_UNIX: u64 = 2_082_844_800;
    let unix = secs.checked_sub(MAC_TO_UNIX)?;
    DateTime::from_timestamp(i64::try_from(unix).ok()?, 0).map(|d| d.naive_utc())
}

/// Content range (start, end) of the first `kind` box in `[from, to)`.
fn find_box(r: &mut Reader, from: u64, to: u64, kind: &[u8; 4]) -> Option<(u64, u64)> {
    let mut pos = from;
    for _ in 0..4096 {
        if pos + 8 > to {
            return None;
        }
        let size32 = u64::from(r.be32(pos)?);
        let (header, size) = match size32 {
            1 => (16, r.be64(pos + 8)?),
            0 => (8, to - pos),
            s => (8, s),
        };
        if size < header || pos.checked_add(size)? > to {
            return None;
        }
        if r.starts_with(pos + 4, kind) {
            return Some((pos + header, pos + size));
        }
        pos += size;
    }
    None
}

// ---------------------------------------------------------------------------
// MP3 (ID3v2)

/// "Artist - Title" (or just the title) from an ID3v2 tag.
fn id3_title(r: &mut Reader) -> Option<String> {
    if !r.starts_with(0, b"ID3") {
        return None;
    }
    let version = r.u8(3)?;
    let flags = r.u8(5)?;
    let syncsafe = |b: &[u8]| b.iter().fold(0u64, |a, &x| (a << 7) | u64::from(x & 0x7F));
    let tag_size = syncsafe(r.bytes(6, 4)?);
    let end = (10 + tag_size).min(r.limit());
    let mut pos = 10u64;
    if flags & 0x40 != 0 && version >= 3 {
        let ext = r.bytes(10, 4)?;
        pos += if version == 4 {
            syncsafe(ext)
        } else {
            4 + u64::from(u32::from_be_bytes([ext[0], ext[1], ext[2], ext[3]]))
        };
    }
    let (id_len, header) = if version == 2 { (3, 6) } else { (4, 10) };
    let (mut title, mut artist) = (None, None);
    while pos + header <= end {
        let id = r.bytes(pos, id_len)?.to_vec();
        if id[0] == 0 {
            break;
        }
        let size = match version {
            2 => {
                let b = r.bytes(pos + 3, 3)?;
                (u64::from(b[0]) << 16) | (u64::from(b[1]) << 8) | u64::from(b[2])
            }
            4 => syncsafe(r.bytes(pos + 4, 4)?),
            _ => u64::from(r.be32(pos + 4)?),
        };
        if size == 0 || pos + header + size > end {
            break;
        }
        let wanted = matches!(&id[..], b"TIT2" | b"TT2" | b"TPE1" | b"TP1");
        if wanted && size <= 1024 {
            let text = id3_text(r.bytes(pos + header, size as usize)?);
            if matches!(&id[..], b"TIT2" | b"TT2") {
                title = text;
            } else {
                artist = text;
            }
        }
        pos += header + size;
    }
    match (artist, title) {
        (Some(a), Some(t)) => Some(format!("{a} - {t}")),
        (None, Some(t)) => Some(t),
        _ => None,
    }
}

/// An ID3 text frame body: encoding byte, then the text.
fn id3_text(b: &[u8]) -> Option<String> {
    let (&enc, text) = b.split_first()?;
    let s = match enc {
        0 => text.iter().map(|&c| char::from(c)).collect(),
        1 | 2 => {
            let (big, body) = match text {
                [0xFE, 0xFF, rest @ ..] => (true, rest),
                [0xFF, 0xFE, rest @ ..] => (false, rest),
                _ => (enc == 2, text),
            };
            let units: Vec<u16> = body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| if big { u16::from_be_bytes(*c) } else { u16::from_le_bytes(*c) })
                .collect();
            String::from_utf16_lossy(&units)
        }
        3 => String::from_utf8_lossy(text).into_owned(),
        _ => return None,
    };
    let s = s.split('\0').next().unwrap_or("").trim().to_string();
    (!s.is_empty()).then_some(s)
}

// ---------------------------------------------------------------------------
// PDF

/// Title and creation date from the document information dictionary that
/// the trailer points to (`/Info n g R`). Outline entries also have
/// `/Title`, so the dictionary is located instead of searching blindly.
fn pdf(r: &mut Reader, len: u64) -> Meta {
    const TAIL: u64 = 256 << 10;
    let tail_start = len.saturating_sub(TAIL);
    let Some(info_ref) = last_find(r, tail_start, b"/Info", len) else { return Meta::default() };
    let Some(head) = r.bytes(info_ref + 5, 32.min((len - info_ref - 5) as usize)) else { return Meta::default() };
    let text = String::from_utf8_lossy(head);
    let mut parts = text.split_whitespace();
    let (Some(num), Some(generation), Some(r_kw)) = (parts.next(), parts.next(), parts.next()) else {
        return Meta::default();
    };
    if !r_kw.starts_with('R') || num.parse::<u32>().is_err() || generation.parse::<u32>().is_err() {
        return Meta::default();
    }
    let needle = format!("{num} {generation} obj");
    let Some(obj) = find_object(r, needle.as_bytes(), len) else { return Meta::default() };
    let body_end = r.find(obj, b"endobj", (obj + 8192).min(len)).unwrap_or((obj + 8192).min(len));
    let Some(body) = r.bytes(obj, (body_end - obj) as usize).map(<[u8]>::to_vec) else { return Meta::default() };
    let title = pdf_value(&body, b"/Title").map(|v| pdf_text(&v));
    let date = pdf_value(&body, b"/CreationDate").and_then(|v| pdf_date(&String::from_utf8_lossy(&v)));
    Meta { date, title: title.filter(|t| !t.trim().is_empty()) }
}

fn last_find(r: &mut Reader, from: u64, needle: &[u8], to: u64) -> Option<u64> {
    let mut last = None;
    let mut cur = from;
    while let Some(p) = r.find(cur, needle, to) {
        last = Some(p);
        cur = p + 1;
    }
    last
}

/// `n g obj` at the start of a line (not the tail of a longer number).
fn find_object(r: &mut Reader, needle: &[u8], len: u64) -> Option<u64> {
    let mut cur = 0;
    while let Some(p) = r.find(cur, needle, len) {
        if p == 0 || !r.u8(p - 1).is_some_and(|b| b.is_ascii_digit()) {
            return Some(p + needle.len() as u64);
        }
        cur = p + 1;
    }
    None
}

/// Raw bytes of a string value after `key` in a dictionary body.
fn pdf_value(body: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    let at = memchr::memmem::find(body, key)? + key.len();
    let rest = &body[at..];
    let start = rest.iter().position(|b| !b.is_ascii_whitespace())?;
    let rest = &rest[start..];
    match rest.first()? {
        b'(' => {
            let mut out = Vec::new();
            let mut depth = 0usize;
            let mut i = 1;
            while i < rest.len() {
                let c = rest[i];
                match c {
                    b'\\' => {
                        i += 1;
                        let e = *rest.get(i)?;
                        match e {
                            b'n' => out.push(b'\n'),
                            b'r' => out.push(b'\r'),
                            b't' => out.push(b'\t'),
                            b'b' => out.push(8),
                            b'f' => out.push(12),
                            b'0'..=b'7' => {
                                let mut v = 0u32;
                                let mut k = 0;
                                while k < 3 && rest.get(i + k).is_some_and(|d| (b'0'..=b'7').contains(d)) {
                                    v = v * 8 + u32::from(rest[i + k] - b'0');
                                    k += 1;
                                }
                                out.push((v & 0xFF) as u8);
                                i += k - 1;
                            }
                            b'\r' | b'\n' => {}
                            other => out.push(other),
                        }
                    }
                    b'(' => {
                        depth += 1;
                        out.push(c);
                    }
                    b')' if depth == 0 => return Some(out),
                    b')' => {
                        depth -= 1;
                        out.push(c);
                    }
                    _ => out.push(c),
                }
                i += 1;
                if out.len() > 1024 {
                    return None;
                }
            }
            None
        }
        b'<' => {
            let end = rest.iter().position(|&b| b == b'>')?;
            let hex: Vec<u8> = rest[1..end].iter().copied().filter(u8::is_ascii_hexdigit).collect();
            let mut out = Vec::with_capacity(hex.len() / 2 + 1);
            for pair in hex.chunks(2) {
                let s = std::str::from_utf8(pair).ok()?;
                let v = u8::from_str_radix(&format!("{s:0<2}"), 16).ok()?;
                out.push(v);
            }
            Some(out)
        }
        _ => None,
    }
}

/// PDF text strings are UTF-16BE with a byte order mark, or (close enough)
/// Latin-1.
fn pdf_text(b: &[u8]) -> String {
    if let [0xFE, 0xFF, rest @ ..] = b {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
        String::from_utf16_lossy(&units)
    } else {
        b.iter().map(|&c| char::from(c)).collect()
    }
}

/// `D:YYYYMMDDHHmmSS...` (later parts optional).
fn pdf_date(s: &str) -> Option<NaiveDateTime> {
    let digits: String = s.trim_start_matches("D:").chars().take_while(char::is_ascii_digit).collect();
    let part =
        |a: usize, b: usize, default: u32| digits.get(a..b).and_then(|x| x.parse::<u32>().ok()).unwrap_or(default);
    let year = digits.get(0..4)?.parse::<i32>().ok()?;
    NaiveDate::from_ymd_opt(year, part(4, 6, 1), part(6, 8, 1))?.and_hms_opt(
        part(8, 10, 0),
        part(10, 12, 0),
        part(12, 14, 0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemSource;

    fn read_meta(format: &str, ext: &str, data: Vec<u8>) -> Meta {
        let len = data.len() as u64;
        let src = MemSource(data);
        let mut r = Reader::new(&src, 0, len);
        read(format, ext, &mut r, len)
    }

    /// A little-endian TIFF/EXIF block: IFD0 with Make, Model and a pointer
    /// to an EXIF IFD holding DateTimeOriginal.
    fn exif_block(make: &str, model: &str, taken: &str) -> Vec<u8> {
        let mut v = b"II*\0".to_vec();
        v.extend_from_slice(&8u32.to_le_bytes());
        // IFD0 at 8: 3 entries.
        let ifd0_len = 2 + 3 * 12 + 4;
        let exif_ifd = 8 + ifd0_len;
        let exif_len = 2 + 12 + 4;
        let data = exif_ifd + exif_len;
        let make_at = data;
        let model_at = make_at + make.len() + 1;
        let taken_at = model_at + model.len() + 1;
        let entry = |v: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32| {
            v.extend_from_slice(&tag.to_le_bytes());
            v.extend_from_slice(&typ.to_le_bytes());
            v.extend_from_slice(&count.to_le_bytes());
            v.extend_from_slice(&value.to_le_bytes());
        };
        // Text of up to 4 bytes (with its terminating zero) is stored in the
        // entry itself, longer text at an offset.
        let text = |s: &str, at: usize| {
            if s.len() < 4 {
                let mut b = [0u8; 4];
                b[..s.len()].copy_from_slice(s.as_bytes());
                u32::from_le_bytes(b)
            } else {
                at as u32
            }
        };
        v.extend_from_slice(&3u16.to_le_bytes());
        entry(&mut v, 0x010F, 2, make.len() as u32 + 1, text(make, make_at));
        entry(&mut v, 0x0110, 2, model.len() as u32 + 1, text(model, model_at));
        entry(&mut v, 0x8769, 4, 1, exif_ifd as u32);
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        entry(&mut v, 0x9003, 2, taken.len() as u32 + 1, taken_at as u32);
        v.extend_from_slice(&0u32.to_le_bytes());
        for s in [make, model, taken] {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        v
    }

    #[test]
    fn jpeg_exif_gives_date_and_camera() {
        let exif = exif_block("Canon", "Canon EOS 80D", "2023:07:14 15:32:10");
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpg.extend_from_slice(&((exif.len() + 8) as u16).to_be_bytes());
        jpg.extend_from_slice(b"Exif\0\0");
        jpg.extend_from_slice(&exif);
        jpg.extend_from_slice(&[0xFF, 0xDA, 0, 2, 0xFF, 0xD9]);
        let m = read_meta("jpeg", "jpg", jpg);
        assert_eq!(m.title.as_deref(), Some("2023-07-14 15.32.10 Canon EOS 80D"));
        assert_eq!(m.date.unwrap().to_string(), "2023-07-14 15:32:10");
    }

    #[test]
    fn raw_files_and_unrelated_make_model() {
        let m = read_meta("tiff", "nef", exif_block("NIKON CORPORATION", "D750", "2019:01:02 03:04:05"));
        assert_eq!(m.title.as_deref(), Some("2019-01-02 03.04.05 NIKON CORPORATION D750"));
        // A zero date is not a date.
        let m = read_meta("tiff", "tif", exif_block("Scanner", "X1", "0000:00:00 00:00:00"));
        assert_eq!(m.title.as_deref(), Some("Scanner X1"));
        assert_eq!(m.date, None);
    }

    #[test]
    fn mp4_creation_time() {
        // ftyp, then moov containing mvhd (version 0).
        let mut v = Vec::new();
        v.extend_from_slice(&16u32.to_be_bytes());
        v.extend_from_slice(b"ftypisom\0\0\0\0");
        let created: u32 = (1_700_000_000u64 + 2_082_844_800) as u32; // 2023-11-14 22:13:20 UTC
        let mut mvhd = vec![0u8; 100];
        mvhd[4..8].copy_from_slice(&created.to_be_bytes());
        v.extend_from_slice(&((8 + 8 + mvhd.len()) as u32).to_be_bytes());
        v.extend_from_slice(b"moov");
        v.extend_from_slice(&((8 + mvhd.len()) as u32).to_be_bytes());
        v.extend_from_slice(b"mvhd");
        v.extend_from_slice(&mvhd);
        let m = read_meta("bmff", "mp4", v);
        assert_eq!(m.date.unwrap().to_string(), "2023-11-14 22:13:20");
        assert_eq!(m.title, None);
    }

    #[test]
    fn id3_artist_and_title() {
        let frame = |id: &[u8], text: &str| {
            let mut f = id.to_vec();
            f.extend_from_slice(&((text.len() + 1) as u32).to_be_bytes());
            f.extend_from_slice(&[0, 0, 3]); // flags, UTF-8
            f.extend_from_slice(text.as_bytes());
            f
        };
        let mut frames = frame(b"TPE1", "Fairuz");
        frames.extend(frame(b"TIT2", "Li Beirut / Live"));
        let mut v = b"ID3\x03\x00\x00".to_vec();
        let n = frames.len() as u32;
        v.extend_from_slice(&[(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]);
        v.extend(frames);
        v.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        let m = read_meta("mp3", "mp3", v);
        assert_eq!(m.title.as_deref(), Some("Fairuz - Li Beirut Live"));
    }

    #[test]
    fn pdf_info_dictionary_not_outline_titles() {
        let pdf = b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\n\
            2 0 obj << /Title (Chapter 1) >> endobj\n\
            12 0 obj << /Title (Not this) >> endobj\n\
            7 0 obj << /Title (Annual Report \\(2024\\)) /CreationDate (D:20240301120000Z) >> endobj\n\
            trailer << /Root 1 0 R /Info 7 0 R >>\n%%EOF\n";
        let m = read_meta("pdf", "pdf", pdf.to_vec());
        assert_eq!(m.title.as_deref(), Some("Annual Report (2024)"));
        assert_eq!(m.date.unwrap().to_string(), "2024-03-01 12:00:00");
        // UTF-16 hex title.
        let pdf = b"%PDF-1.7\n3 0 obj << /Title <FEFF00480069> >> endobj\ntrailer << /Info 3 0 R >>\n%%EOF";
        assert_eq!(read_meta("pdf", "pdf", pdf.to_vec()).title.as_deref(), Some("Hi"));
    }

    #[test]
    fn names_are_safe() {
        assert_eq!(sanitize("  a/b:c*d?  "), Some("a b c d".into()));
        assert_eq!(sanitize("..."), None);
        assert_eq!(sanitize(&"é".repeat(80)).unwrap().len(), 100);
        assert_eq!(read_meta("png", "png", vec![0; 10]), Meta::default());
    }
}
