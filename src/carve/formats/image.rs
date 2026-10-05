//! Still images: JPEG, PNG, GIF, BMP, TIFF and TIFF-based camera RAW.

use std::collections::HashSet;

use crate::carve::{Category, Format, Hit, Reader};

const MIB: u64 = 1 << 20;

pub struct Jpeg;

impl Format for Jpeg {
    fn name(&self) -> &'static str {
        "jpeg"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("jpg", Category::Image)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        &[0xFF]
    }
    fn max_size(&self) -> u64 {
        256 * MIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.len() >= 4 && h[..3] == [0xFF, 0xD8, 0xFF] && h[3] >= 0xC0 && h[3] != 0xFF
    }

    /// Walks marker segments; inside entropy-coded scans, `FF 00` (byte
    /// stuffing) and `FF D0..D7` (restart markers) are data, anything else
    /// is the next marker. Progressive JPEGs have several scans.
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let mut pos = 2u64;
        let (mut seen_frame, mut seen_scan) = (false, false);
        loop {
            if r.u8(pos)? != 0xFF {
                return None;
            }
            let mut m = r.u8(pos + 1)?;
            while m == 0xFF {
                pos += 1;
                m = r.u8(pos + 1)?;
            }
            match m {
                0xD9 => {
                    return (seen_frame && seen_scan).then_some(Hit { len: pos + 2, ext: "jpg" });
                }
                0x00 | 0xD8 => return None,
                0x01 | 0xD0..=0xD7 => pos += 2,
                _ => {
                    let len = u64::from(r.be16(pos + 2)?);
                    if len < 2 {
                        return None;
                    }
                    if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
                        seen_frame = true;
                    }
                    pos += 2 + len;
                    if m == 0xDA {
                        seen_scan = true;
                        loop {
                            let p = r.find_byte(pos, 0xFF, r.limit())?;
                            match r.u8(p + 1)? {
                                0x00 | 0xD0..=0xD7 => pos = p + 2,
                                0xFF => pos = p + 1,
                                _ => {
                                    pos = p;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub struct Png;

impl Format for Png {
    fn name(&self) -> &'static str {
        "png"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("png", Category::Image)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        &[0x89]
    }
    fn max_size(&self) -> u64 {
        512 * MIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.starts_with(b"\x89PNG\r\n\x1a\n") && h.get(12..16) == Some(b"IHDR")
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let mut pos = 8u64;
        loop {
            let len = u64::from(r.be32(pos)?);
            let ty = r.bytes(pos + 4, 4)?;
            if len > 0x7FFF_FFFF || !ty.iter().all(u8::is_ascii_alphabetic) {
                return None;
            }
            let end = ty == b"IEND";
            pos += 12 + len;
            if end {
                return (pos <= r.limit()).then_some(Hit { len: pos, ext: "png" });
            }
        }
    }
}

pub struct Gif;

impl Format for Gif {
    fn name(&self) -> &'static str {
        "gif"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("gif", Category::Image)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"G"
    }
    fn max_size(&self) -> u64 {
        128 * MIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        (h.starts_with(b"GIF87a") || h.starts_with(b"GIF89a")) && h.len() >= 10 && h[6..10] != [0, 0, 0, 0]
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let flags = r.u8(10)?;
        let mut pos = 13u64;
        if flags & 0x80 != 0 {
            pos += 3 << ((flags & 7) + 1);
        }
        let mut images = 0;
        loop {
            match r.u8(pos)? {
                0x3B => return (images > 0).then_some(Hit { len: pos + 1, ext: "gif" }),
                0x2C => {
                    let f = r.u8(pos + 9)?;
                    pos += 10;
                    if f & 0x80 != 0 {
                        pos += 3 << ((f & 7) + 1);
                    }
                    pos += 1; // LZW minimum code size
                    pos = skip_sub_blocks(r, pos)?;
                    images += 1;
                }
                0x21 => pos = skip_sub_blocks(r, pos + 2)?,
                _ => return None,
            }
        }
    }
}

fn skip_sub_blocks(r: &mut Reader, mut pos: u64) -> Option<u64> {
    loop {
        let n = r.u8(pos)?;
        pos += 1 + u64::from(n);
        if n == 0 {
            return Some(pos);
        }
    }
}

pub struct Bmp;

impl Format for Bmp {
    fn name(&self) -> &'static str {
        "bmp"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("bmp", Category::Image)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"B"
    }
    fn max_size(&self) -> u64 {
        512 * MIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        let get32 = |o: usize| crate::bytes::le32(h, o);
        let get16 = |o: usize| crate::bytes::le16(h, o);
        h.starts_with(b"BM")
            && get32(6) == Some(0)
            && matches!(get32(14), Some(12 | 40 | 52 | 56 | 64 | 108 | 124))
            && get16(26) == Some(1)
            && matches!(get16(28), Some(1 | 4 | 8 | 16 | 24 | 32))
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let size = u64::from(r.le32(2)?);
        let data_off = u64::from(r.le32(10)?);
        (size >= 26 && data_off < size && size <= r.limit()).then_some(Hit { len: size, ext: "bmp" })
    }
}

/// TIFF and TIFF-based RAW formats (CR2, NEF, ARW, DNG, PEF, SRW).
pub struct Tiff;

struct Endian(bool);

impl Endian {
    fn u16(&self, r: &mut Reader, p: u64) -> Option<u16> {
        if self.0 { r.le16(p) } else { r.be16(p) }
    }
    fn u32(&self, r: &mut Reader, p: u64) -> Option<u32> {
        if self.0 { r.le32(p) } else { r.be32(p) }
    }
}

impl Format for Tiff {
    fn name(&self) -> &'static str {
        "tiff"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[
            ("tif", Category::Image),
            ("cr2", Category::Image),
            ("nef", Category::Image),
            ("arw", Category::Image),
            ("dng", Category::Image),
            ("pef", Category::Image),
            ("srw", Category::Image),
        ]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"IM"
    }
    fn max_size(&self) -> u64 {
        1024 * MIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        let first = if h.starts_with(b"II*\0") {
            crate::bytes::le32(h, 4)
        } else if h.starts_with(b"MM\0*") {
            crate::bytes::be32(h, 4)
        } else {
            None
        };
        first.is_some_and(|o| (8..1 << 30).contains(&o))
    }

    /// Walks every IFD (main chain, SubIFDs, EXIF) and returns the furthest
    /// byte referenced by any tag, strip, tile or thumbnail.
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let e = Endian(r.starts_with(0, b"II"));
        let mut queue = vec![u64::from(e.u32(r, 4)?)];
        let mut visited = HashSet::new();
        let mut end = 8u64;
        let mut make = String::new();
        let mut dng = false;
        let mut ifds = 0;
        while let Some(off) = queue.pop() {
            if off < 8 || !visited.insert(off) || ifds >= 64 {
                continue;
            }
            ifds += 1;
            let n = u64::from(e.u16(r, off)?);
            if n == 0 || n > 1000 {
                if ifds == 1 {
                    return None;
                }
                continue;
            }
            end = end.max(off + 2 + n * 12 + 4);
            let (mut offsets, mut counts) = (Vec::new(), Vec::new());
            for i in 0..n {
                let ent = off + 2 + i * 12;
                let tag = e.u16(r, ent)?;
                let typ = e.u16(r, ent + 2)?;
                let count = u64::from(e.u32(r, ent + 4)?);
                let size = match typ {
                    1 | 2 | 6 | 7 => 1,
                    3 | 8 => 2,
                    4 | 9 | 11 | 13 => 4,
                    5 | 10 | 12 => 8,
                    _ => continue,
                };
                let bytes = count.saturating_mul(size);
                let at = if bytes <= 4 { ent + 8 } else { u64::from(e.u32(r, ent + 8)?) };
                if bytes > 4 {
                    end = end.max(at.saturating_add(bytes));
                }
                let values = |r: &mut Reader| -> Vec<u64> {
                    (0..count.min(1 << 16))
                        .map_while(|k| match size {
                            2 => e.u16(r, at + k * 2).map(u64::from),
                            4 => e.u32(r, at + k * 4).map(u64::from),
                            _ => None,
                        })
                        .collect()
                };
                match tag {
                    273 | 324 | 513 => offsets.push((tag, values(r))),
                    279 | 325 | 514 => counts.push((tag, values(r))),
                    330 | 34665 => queue.extend(values(r)),
                    271 => {
                        if let Some(b) = r.bytes(at, count.min(32) as usize) {
                            make = String::from_utf8_lossy(b).to_ascii_uppercase();
                        }
                    }
                    50706 => dng = true,
                    _ => {}
                }
            }
            // Pair StripOffsets/StripByteCounts, TileOffsets/TileByteCounts,
            // JPEGInterchangeFormat/Length.
            for (otag, offs) in &offsets {
                let ctag = match otag {
                    273 => 279,
                    324 => 325,
                    _ => 514,
                };
                if let Some((_, cnts)) = counts.iter().find(|(t, _)| *t == ctag) {
                    for (o, c) in offs.iter().zip(cnts) {
                        end = end.max(o.saturating_add(*c));
                    }
                }
            }
            let next = u64::from(e.u32(r, off + 2 + n * 12)?);
            if next != 0 {
                queue.push(next);
            }
        }
        if end > r.limit() {
            return None;
        }
        let ext = if dng {
            "dng"
        } else if r.starts_with(8, b"CR") {
            "cr2"
        } else if make.starts_with("NIKON") {
            "nef"
        } else if make.starts_with("SONY") {
            "arw"
        } else if make.starts_with("PENTAX") {
            "pef"
        } else if make.starts_with("SAMSUNG") {
            "srw"
        } else {
            "tif"
        };
        Some(Hit { len: end, ext })
    }
}
