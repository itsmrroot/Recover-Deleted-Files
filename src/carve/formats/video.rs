//! Video containers: ISO-BMFF (MP4/MOV/HEIC/CR3), Matroska/WebM, RIFF
//! (AVI/WAV/WebP), ASF (WMV/WMA) and MPEG transport streams (AVCHD .MTS).

use crate::carve::{Category, Format, Hit, Reader};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// ISO base media file format: a flat list of top-level boxes.
pub struct Bmff;

const TOP_LEVEL_BOXES: &[&[u8; 4]] = &[
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"pdin", b"moof", b"mfra", b"styp",
    b"sidx", b"ssix", b"prft", b"emsg", b"pnot", b"PICT", b"junk",
];

impl Format for Bmff {
    fn name(&self) -> &'static str {
        "bmff"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[
            ("mp4", Category::Video),
            ("mov", Category::Video),
            ("m4v", Category::Video),
            ("3gp", Category::Video),
            ("heic", Category::Image),
            ("avif", Category::Image),
            ("cr3", Category::Image),
            ("m4a", Category::Audio),
        ]
    }
    fn first_bytes(&self) -> &'static [u8] {
        &[] // a leading mdat box can have any size
    }
    fn max_size(&self) -> u64 {
        256 * GIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        let size = crate::bytes::be32(h, 0).unwrap_or(0);
        match h.get(4..8) {
            Some(b"ftyp") => {
                (16..=512).contains(&size)
                    && size.is_multiple_of(4)
                    && h.get(8..12).is_some_and(|b| b.iter().all(|c| c.is_ascii_graphic() || *c == b' '))
            }
            // Classic QuickTime files have no ftyp and start with one of
            // these; measure() insists on both a moov and an mdat.
            Some(b"moov" | b"mdat" | b"wide" | b"pnot") => size == 1 || size >= 16,
            _ => false,
        }
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let legacy = !r.starts_with(4, b"ftyp");
        let brand: [u8; 4] = if legacy { *b"qt  " } else { r.bytes(8, 4)?.try_into().ok()? };
        let mut pos = 0u64;
        let (mut has_index, mut has_data) = (false, false);
        while pos + 8 <= r.limit() {
            let size32 = u64::from(r.be32(pos)?);
            let ty: [u8; 4] = r.bytes(pos + 4, 4)?.try_into().ok()?;
            if !TOP_LEVEL_BOXES.contains(&&ty) {
                break;
            }
            let size = match size32 {
                1 => r.be64(pos + 8)?,
                0 => break, // "extends to end of file": length unknowable
                s => s,
            };
            if size < 8 {
                break;
            }
            match &ty {
                b"moov" | b"meta" | b"moof" => has_index = true,
                b"mdat" => has_data = true,
                _ => {}
            }
            if pos + size > r.limit() {
                break;
            }
            pos += size;
        }
        if !has_index || pos <= 16 || (legacy && !has_data) {
            return None;
        }
        let ext = match &brand {
            b"qt  " => "mov",
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"mif1" | b"msf1" => "heic",
            b"avif" | b"avis" => "avif",
            b"crx " => "cr3",
            b"M4A " | b"M4B " => "m4a",
            b"M4V " | b"M4VH" | b"M4VP" => "m4v",
            [b'3', b'g', ..] => "3gp",
            _ => "mp4",
        };
        Some(Hit { len: pos, ext })
    }
}

/// Matroska / WebM (EBML).
pub struct Matroska;

/// EBML element ID (keeps the length marker bits, as IDs are compared raw).
fn ebml_id(r: &mut Reader, pos: u64) -> Option<(u32, u64)> {
    let b = r.u8(pos)?;
    let len = b.leading_zeros() as u64 + 1;
    if len > 4 {
        return None;
    }
    let mut id = u32::from(b);
    for i in 1..len {
        id = id << 8 | u32::from(r.u8(pos + i)?);
    }
    Some((id, len))
}

/// EBML variable-size integer: (value, encoded length, is "unknown size").
fn ebml_size(r: &mut Reader, pos: u64) -> Option<(u64, u64, bool)> {
    let b = r.u8(pos)?;
    let len = b.leading_zeros() as u64 + 1;
    if len > 8 {
        return None;
    }
    let mut v = u64::from(b) & (0xFF >> len);
    for i in 1..len {
        v = v << 8 | u64::from(r.u8(pos + i)?);
    }
    let unknown = v == (1u64 << (7 * len)) - 1;
    Some((v, len, unknown))
}

const SEGMENT: u32 = 0x1853_8067;
const SEGMENT_CHILDREN: &[u32] = &[
    0x114D_9B74, // SeekHead
    0x1549_A966, // Info
    0x1654_AE6B, // Tracks
    0x1F43_B675, // Cluster
    0x1C53_BB6B, // Cues
    0x1043_A770, // Chapters
    0x1254_C367, // Tags
    0x1941_A469, // Attachments
    0xEC,        // Void
    0xBF,        // CRC-32
];

impl Format for Matroska {
    fn name(&self) -> &'static str {
        "matroska"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("mkv", Category::Video), ("webm", Category::Video)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        &[0x1A]
    }
    fn max_size(&self) -> u64 {
        256 * GIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let (_, idl) = ebml_id(r, 0)?;
        let (hsize, sl, unknown) = ebml_size(r, idl)?;
        if unknown || hsize > 4096 {
            return None;
        }
        let header_end = idl + sl + hsize;
        let header = r.bytes(0, header_end as usize)?;
        let ext = if memchr::memmem::find(header, b"webm").is_some() {
            "webm"
        } else if memchr::memmem::find(header, b"matroska").is_some() {
            "mkv"
        } else {
            return None;
        };
        let (id, il) = ebml_id(r, header_end)?;
        if id != SEGMENT {
            return None;
        }
        let (size, sl, unknown) = ebml_size(r, header_end + il)?;
        let data = header_end + il + sl;
        if !unknown {
            let end = data.checked_add(size)?;
            return (end <= r.limit()).then_some(Hit { len: end, ext });
        }
        // Unknown-size segment (live recording): walk its children.
        let mut pos = data;
        while let Some((cid, cl)) = ebml_id(r, pos) {
            if !SEGMENT_CHILDREN.contains(&cid) {
                break;
            }
            let Some((csize, csl, cunknown)) = ebml_size(r, pos + cl) else { break };
            let next = pos + cl + csl + csize;
            if cunknown || next > r.limit() {
                break;
            }
            pos = next;
        }
        (pos > data).then_some(Hit { len: pos, ext })
    }
}

/// RIFF containers: WebP, AVI (including OpenDML AVIX extensions), WAV.
pub struct Riff;

impl Format for Riff {
    fn name(&self) -> &'static str {
        "riff"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("avi", Category::Video), ("wav", Category::Audio), ("webp", Category::Image)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"R"
    }
    fn max_size(&self) -> u64 {
        64 * GIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.starts_with(b"RIFF") && matches!(h.get(8..12), Some(b"AVI " | b"WAVE" | b"WEBP"))
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let form: [u8; 4] = r.bytes(8, 4)?.try_into().ok()?;
        let size = u64::from(r.le32(4)?);
        if size < 12 {
            return None;
        }
        let mut end = 8 + size + (size & 1);
        if end > r.limit() {
            return None;
        }
        let ext = match &form {
            b"AVI " => {
                // Files over 1 GiB continue in further "RIFF....AVIX" lists.
                while r.starts_with(end, b"RIFF") && r.starts_with(end + 8, b"AVIX") {
                    let s = u64::from(r.le32(end + 4)?);
                    let next = end + 8 + s + (s & 1);
                    if next > r.limit() {
                        break;
                    }
                    end = next;
                }
                "avi"
            }
            b"WAVE" => "wav",
            _ => "webp",
        };
        Some(Hit { len: end, ext })
    }
}

/// Advanced Systems Format (WMV / WMA).
pub struct Asf;

const ASF_HEADER: [u8; 16] =
    [0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];
const ASF_FILE_PROPERTIES: [u8; 16] =
    [0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];
const ASF_STREAM_PROPERTIES: [u8; 16] =
    [0x91, 0x07, 0xDC, 0xB7, 0xB7, 0xA9, 0xCF, 0x11, 0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];
const ASF_VIDEO_MEDIA: [u8; 16] =
    [0xC0, 0xEF, 0x19, 0xBC, 0x4D, 0x5B, 0xCF, 0x11, 0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B];

impl Format for Asf {
    fn name(&self) -> &'static str {
        "asf"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("wmv", Category::Video), ("wma", Category::Audio)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        &[0x30]
    }
    fn max_size(&self) -> u64 {
        64 * GIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.starts_with(&ASF_HEADER)
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let header_size = r.le64(16)?;
        let objects = r.le32(24)?;
        if !(30..=16 * MIB).contains(&header_size) || objects > 1000 {
            return None;
        }
        let mut pos = 30u64;
        let mut file_size = None;
        let mut video = false;
        for _ in 0..objects {
            let guid: [u8; 16] = r.bytes(pos, 16)?.try_into().ok()?;
            let size = r.le64(pos + 16)?;
            if size < 24 || pos + size > header_size {
                return None;
            }
            if guid == ASF_FILE_PROPERTIES {
                file_size = Some(r.le64(pos + 40)?);
            } else if guid == ASF_STREAM_PROPERTIES && r.bytes(pos + 24, 16)? == ASF_VIDEO_MEDIA {
                video = true;
            }
            pos += size;
        }
        let len = file_size?;
        (len > header_size && len <= r.limit()).then_some(Hit { len, ext: if video { "wmv" } else { "wma" } })
    }
}

/// MPEG-2 transport streams: plain 188-byte packets (.ts) or the 192-byte
/// timestamped packets used by AVCHD camcorders and Blu-ray (.mts/.m2ts).
pub struct TransportStream;

impl TransportStream {
    fn layout(h: &[u8]) -> Option<(u64, u64)> {
        let synced = |stride: usize, off: usize| (0..5).all(|k| h.get(off + k * stride) == Some(&0x47));
        if synced(192, 4) {
            Some((192, 4))
        } else if synced(188, 0) {
            Some((188, 0))
        } else {
            None
        }
    }
}

impl Format for TransportStream {
    fn name(&self) -> &'static str {
        "mpeg-ts"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("m2ts", Category::Video), ("ts", Category::Video)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        &[] // M2TS starts with a 4-byte timestamp: any value
    }
    fn max_size(&self) -> u64 {
        64 * GIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        Self::layout(h).is_some()
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let head = r.bytes(0, 1024.min(r.limit() as usize))?.to_vec();
        let (stride, sync) = Self::layout(&head)?;
        let mut pos = 0u64;
        while pos + stride <= r.limit() && r.u8(pos + sync) == Some(0x47) {
            pos += stride;
        }
        (pos >= 16 * stride).then_some(Hit { len: pos, ext: if stride == 192 { "m2ts" } else { "ts" } })
    }
}
