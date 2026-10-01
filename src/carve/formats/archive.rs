//! Archives and containers: ZIP (+ DOCX/XLSX/PPTX/ODF/EPUB/APK/JAR), 7z,
//! RAR 4/5 and SQLite databases.

use super::crc32;
use crate::carve::{Category, Format, Hit, Reader};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

pub struct Zip;

const LOCAL: u32 = 0x0403_4B50;
const CENTRAL: u32 = 0x0201_4B50;
const EOCD: u32 = 0x0605_4B50;
const EOCD64: u32 = 0x0606_4B50;

impl Zip {
    /// Picks the real file type of a ZIP-based document.
    fn classify(r: &mut Reader, cd_off: u64, cd_size: u64) -> &'static str {
        let first_name_len = r.le16(26).unwrap_or(0) as usize;
        let extra_len = u64::from(r.le16(28).unwrap_or(0));
        if r.bytes(30, first_name_len) == Some(b"mimetype") {
            let at = 30 + first_name_len as u64 + extra_len;
            let mime = r.bytes(at, 48.min((r.limit() - at) as usize)).unwrap_or(&[]).to_vec();
            let has = |s: &[u8]| memchr::memmem::find(&mime, s).is_some();
            return if has(b"opendocument.text") {
                "odt"
            } else if has(b"opendocument.spreadsheet") {
                "ods"
            } else if has(b"opendocument.presentation") {
                "odp"
            } else if has(b"epub+zip") {
                "epub"
            } else {
                "zip"
            };
        }
        let n = cd_size.min(MIB) as usize;
        let Some(cd) = r.bytes(cd_off, n) else { return "zip" };
        let has = |s: &[u8]| memchr::memmem::find(cd, s).is_some();
        if has(b"word/document") {
            "docx"
        } else if has(b"xl/workbook") {
            "xlsx"
        } else if has(b"ppt/presentation") {
            "pptx"
        } else if has(b"AndroidManifest.xml") {
            "apk"
        } else if has(b"META-INF/MANIFEST.MF") {
            "jar"
        } else {
            "zip"
        }
    }
}

impl Format for Zip {
    fn name(&self) -> &'static str {
        "zip"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[
            ("zip", Category::Archive),
            ("docx", Category::Document),
            ("xlsx", Category::Document),
            ("pptx", Category::Document),
            ("odt", Category::Document),
            ("ods", Category::Document),
            ("odp", Category::Document),
            ("epub", Category::Document),
            ("apk", Category::Archive),
            ("jar", Category::Archive),
        ]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"P"
    }
    fn max_size(&self) -> u64 {
        16 * GIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.starts_with(b"PK\x03\x04")
            && crate::bytes::le16(h, 4).is_some_and(|v| v <= 63)
            && matches!(crate::bytes::le16(h, 8), Some(0 | 8 | 9 | 12 | 14 | 93 | 95 | 98 | 99))
    }

    /// Hops over local file entries while their sizes are known, then looks
    /// for an end-of-central-directory record whose offsets are consistent
    /// with this archive (which rejects EOCDs of nested or other archives).
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let mut pos = 0u64;
        loop {
            match r.le32(pos)? {
                LOCAL => {
                    let flags = r.le16(pos + 6)?;
                    let csize = r.le32(pos + 18)?;
                    if flags & 0x08 != 0 || csize == u32::MAX {
                        break; // sizes in a trailing data descriptor
                    }
                    let n = u64::from(r.le16(pos + 26)?);
                    let e = u64::from(r.le16(pos + 28)?);
                    pos += 30 + n + e + u64::from(csize);
                }
                CENTRAL | EOCD | EOCD64 => break,
                _ => return None,
            }
        }
        let mut from = pos;
        loop {
            let p = r.find(from, b"PK\x05\x06", r.limit())?;
            let cd_size = r.le32(p + 12)?;
            let cd_off = r.le32(p + 16)?;
            let comment = u64::from(r.le16(p + 20)?);
            if cd_off == u32::MAX || cd_size == u32::MAX {
                // ZIP64: locator sits right before the EOCD.
                if p >= 20 && r.le32(p - 20)? == 0x0706_4B50 {
                    let z = r.le64(p - 12)?;
                    if r.le32(z)? == EOCD64 {
                        let size = r.le64(z + 40)?;
                        let off = r.le64(z + 48)?;
                        if off.checked_add(size) == Some(z) {
                            let len = p + 22 + comment;
                            return (len <= r.limit()).then(|| Hit { len, ext: Self::classify(r, off, size) });
                        }
                    }
                }
            } else if u64::from(cd_off) + u64::from(cd_size) == p {
                let len = p + 22 + comment;
                return (len <= r.limit())
                    .then(|| Hit { len, ext: Self::classify(r, u64::from(cd_off), u64::from(cd_size)) });
            }
            from = p + 4;
        }
    }
}

pub struct SevenZip;

impl Format for SevenZip {
    fn name(&self) -> &'static str {
        "7z"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("7z", Category::Archive)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"7"
    }
    fn max_size(&self) -> u64 {
        64 * GIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.starts_with(b"7z\xBC\xAF\x27\x1C") && h.get(6) == Some(&0)
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let start_header = r.bytes(12, 20)?;
        if crc32(start_header) != r.le32(8)? {
            return None;
        }
        let off = r.le64(12)?;
        let size = r.le64(20)?;
        if size == 0 || size > GIB {
            return None;
        }
        let len = 32u64.checked_add(off)?.checked_add(size)?;
        (len <= r.limit()).then_some(Hit { len, ext: "7z" })
    }
}

pub struct Rar;

/// RAR5 variable-length integer: (value, encoded length).
fn rar_vint(r: &mut Reader, pos: u64) -> Option<(u64, u64)> {
    let mut v = 0u64;
    for i in 0..10 {
        let b = r.u8(pos + i)?;
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

impl Rar {
    fn measure_v5(r: &mut Reader) -> Option<u64> {
        let mut pos = 8u64;
        loop {
            let crc = r.le32(pos)?;
            let (hsize, n1) = rar_vint(r, pos + 4)?;
            if hsize == 0 || hsize > 2 * MIB {
                return None;
            }
            let hstart = pos + 4 + n1;
            if hsize <= MIB && crc32(r.bytes(pos + 4, (n1 + hsize) as usize)?) != crc {
                return None;
            }
            let (htype, n2) = rar_vint(r, hstart)?;
            let (hflags, n3) = rar_vint(r, hstart + n2)?;
            let mut p = hstart + n2 + n3;
            if hflags & 1 != 0 {
                p += rar_vint(r, p)?.1;
            }
            let data = if hflags & 2 != 0 { rar_vint(r, p)?.0 } else { 0 };
            if !(1..=5).contains(&htype) {
                return None;
            }
            pos = hstart.checked_add(hsize)?.checked_add(data)?;
            if htype == 5 {
                return Some(pos);
            }
        }
    }

    fn measure_v4(r: &mut Reader) -> Option<u64> {
        let mut pos = 7u64;
        let mut blocks = 0;
        while let Some(crc) = r.le16(pos) {
            let ty = r.u8(pos + 2)?;
            let flags = r.le16(pos + 3)?;
            let size = u64::from(r.le16(pos + 5)?);
            if size < 7 || !(0x73..=0x7B).contains(&ty) {
                break;
            }
            if (crc32(r.bytes(pos + 2, (size - 2) as usize)?) & 0xFFFF) as u16 != crc {
                break;
            }
            let add = if flags & 0x8000 != 0 { u64::from(r.le32(pos + 7)?) } else { 0 };
            pos += size + add;
            blocks += 1;
            if ty == 0x7B {
                return Some(pos);
            }
        }
        // Old archives may lack the end block.
        (blocks >= 2).then_some(pos)
    }
}

impl Format for Rar {
    fn name(&self) -> &'static str {
        "rar"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("rar", Category::Archive)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"R"
    }
    fn max_size(&self) -> u64 {
        64 * GIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.starts_with(b"Rar!\x1A\x07\x00") || h.starts_with(b"Rar!\x1A\x07\x01\x00")
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let len = if r.starts_with(6, b"\x01\x00") { Self::measure_v5(r)? } else { Self::measure_v4(r)? };
        (len <= r.limit()).then_some(Hit { len, ext: "rar" })
    }
}

pub struct Sqlite;

impl Format for Sqlite {
    fn name(&self) -> &'static str {
        "sqlite"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("sqlite", Category::Database)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"S"
    }
    fn max_size(&self) -> u64 {
        64 * GIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.starts_with(b"SQLite format 3\0")
    }
    /// Page size x page count, trusted only when the header's
    /// "version-valid-for" matches the change counter (SQLite >= 3.7.0).
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let ps = match r.be16(16)? {
            1 => 65536,
            v if v.is_power_of_two() && v >= 512 => u64::from(v),
            _ => return None,
        };
        let pages = u64::from(r.be32(28)?);
        if pages == 0 || r.be32(24)? != r.be32(92)? {
            return None;
        }
        let len = ps * pages;
        (len <= r.limit()).then_some(Hit { len, ext: "sqlite" })
    }
}
