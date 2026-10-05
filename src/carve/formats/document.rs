//! Documents: PDF and OLE2 compound files (legacy .doc/.xls/.ppt/.msg).
//! Office Open XML (.docx etc.) is ZIP-based and handled in `archive`.

use crate::carve::{Category, Format, Hit, Reader};

const MIB: u64 = 1 << 20;

pub struct Pdf;

impl Pdf {
    /// Linearized ("fast web view") PDFs state their exact length.
    fn linearized_len(r: &mut Reader) -> Option<u64> {
        let n = 1024.min(r.limit()) as usize;
        let head = r.bytes(0, n)?;
        let lin = memchr::memmem::find(head, b"/Linearized")?;
        let rest = &head[lin..];
        let l = memchr::memmem::find(rest, b"/L ")? + 3;
        let digits: String = rest[l..].iter().take_while(|b| b.is_ascii_digit()).map(|&b| char::from(b)).collect();
        let len: u64 = digits.parse().ok()?;
        if len < 64 || len > r.limit() {
            return None;
        }
        // Must end with %%EOF (possibly followed by an EOL).
        let tail = r.bytes(len - 8, 8)?;
        memchr::memmem::find(tail, b"%%EOF").map(|_| len)
    }
}

impl Format for Pdf {
    fn name(&self) -> &'static str {
        "pdf"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("pdf", Category::Document)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"%"
    }
    fn max_size(&self) -> u64 {
        256 * MIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.starts_with(b"%PDF-") && h.get(5).is_some_and(u8::is_ascii_digit) && h.get(6) == Some(&b'.')
    }

    /// A PDF ends at its last `%%EOF`; incremental updates append more
    /// `%%EOF`s, so after each one we look a little further — but never past
    /// the start of another PDF.
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        if let Some(len) = Self::linearized_len(r) {
            return Some(Hit { len, ext: "pdf" });
        }
        const FOLLOW_UP: u64 = 4 * MIB;
        let mut last = None;
        let mut pos = 8u64;
        let mut until = r.limit();
        while let Some(p) = r.find(pos, b"%%EOF", until) {
            if last.is_some() && r.find(pos, b"%PDF-", p).is_some() {
                break;
            }
            let mut end = p + 5;
            for _ in 0..2 {
                match r.u8(end) {
                    Some(b'\r' | b'\n') => end += 1,
                    _ => break,
                }
            }
            last = Some(end);
            pos = end;
            until = (end + FOLLOW_UP).min(r.limit());
        }
        last.map(|len| Hit { len, ext: "pdf" })
    }
}

/// OLE2 / Compound File Binary.
pub struct Ole;

const OLE_SIG: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const FREESECT: u32 = 0xFFFF_FFFF;
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;

impl Ole {
    fn fat_sectors(r: &mut Reader, ss: u64) -> Option<Vec<u32>> {
        let num_fat = r.le32(44)?;
        if num_fat == 0 || num_fat > 1 << 20 {
            return None;
        }
        let mut ids = Vec::with_capacity(num_fat as usize);
        for i in 0..num_fat.min(109) {
            ids.push(r.le32(76 + u64::from(i) * 4)?);
        }
        // Larger files continue the list in DIFAT sectors.
        let mut difat = r.le32(68)?;
        let per = ss / 4 - 1;
        let mut guard = 0;
        while ids.len() < num_fat as usize && difat < ENDOFCHAIN && guard < 1 << 16 {
            let base = (u64::from(difat) + 1) * ss;
            for k in 0..per {
                if ids.len() == num_fat as usize {
                    break;
                }
                ids.push(r.le32(base + k * 4)?);
            }
            difat = r.le32(base + per * 4)?;
            guard += 1;
        }
        (ids.len() == num_fat as usize).then_some(ids)
    }

    /// Next sector in a FAT chain.
    fn next_sector(r: &mut Reader, fat: &[u32], ss: u64, sector: u32) -> Option<u32> {
        let per = (ss / 4) as usize;
        let fat_sector = *fat.get(sector as usize / per)?;
        r.le32((u64::from(fat_sector) + 1) * ss + (sector as usize % per) as u64 * 4)
    }

    /// Names the document type from the stream names in the directory.
    fn classify(r: &mut Reader, ss: u64, fat: &[u32]) -> &'static str {
        let Some(mut sector) = r.le32(48) else { return "doc" };
        for _ in 0..64 {
            if sector >= ENDOFCHAIN {
                break;
            }
            let Some(dir) = r.bytes((u64::from(sector) + 1) * ss, ss as usize).map(<[u8]>::to_vec) else {
                break;
            };
            for e in dir.as_chunks::<128>().0 {
                let n = usize::from(crate::bytes::le16(e, 64).unwrap_or(0)).min(64);
                let name = crate::bytes::utf16le(&e[..n]);
                match name.as_str() {
                    "WordDocument" => return "doc",
                    "Workbook" | "Book" => return "xls",
                    "PowerPoint Document" | "Current User" => return "ppt",
                    n if n.starts_with("__substg1.0_") => return "msg",
                    _ => {}
                }
            }
            match Self::next_sector(r, fat, ss, sector) {
                Some(next) => sector = next,
                None => break,
            }
        }
        "doc"
    }
}

impl Format for Ole {
    fn name(&self) -> &'static str {
        "ole2"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[
            ("doc", Category::Document),
            ("xls", Category::Document),
            ("ppt", Category::Document),
            ("msg", Category::Document),
        ]
    }
    fn first_bytes(&self) -> &'static [u8] {
        &[0xD0]
    }
    fn max_size(&self) -> u64 {
        2048 * MIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        h.starts_with(&OLE_SIG)
            && matches!(crate::bytes::le16(h, 26), Some(3 | 4))
            && h.get(28..30) == Some(&[0xFE, 0xFF])
            && matches!(crate::bytes::le16(h, 30), Some(9 | 12))
    }

    /// The file ends after the highest sector marked in use in the FAT.
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let ss = 1u64 << r.le16(30)?;
        let fat = Self::fat_sectors(r, ss)?;
        let per = ss / 4;
        let mut max_sector = 0u64;
        for (k, &id) in fat.iter().enumerate() {
            if id >= ENDOFCHAIN {
                return None;
            }
            max_sector = max_sector.max(u64::from(id));
            let base = (u64::from(id) + 1) * ss;
            let entries = r.bytes(base, ss as usize)?.to_vec();
            for (j, v) in entries.as_chunks::<4>().0.iter().enumerate().rev() {
                if u32::from_le_bytes(*v) != FREESECT {
                    max_sector = max_sector.max(k as u64 * per + j as u64);
                    break;
                }
            }
        }
        let len = (max_sector + 2) * ss;
        if len > r.limit() {
            return None;
        }
        Some(Hit { len, ext: Self::classify(r, ss, &fat) })
    }
}
