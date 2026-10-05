//! Partition table discovery (MBR incl. extended/logical partitions, GPT)
//! and file-system detection per partition.

use std::sync::Arc;

use serde::Serialize;

use crate::bytes::{le32, le64, utf16le};
use crate::fs::{self, FsKind};
use crate::ranges::{ByteRange, normalize, subtract};
use crate::source::{ReadAt, Source, SubSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Scheme {
    /// The source is itself a volume (e.g. `\\.\C:` or a partition image).
    None,
    Mbr,
    Gpt,
}

#[derive(Debug, Clone, Serialize)]
pub struct Partition {
    /// 1-based, in table order.
    pub index: usize,
    pub start: u64,
    pub len: u64,
    pub scheme: Scheme,
    /// Partition type, e.g. "0x07" or "Microsoft basic data".
    pub kind: String,
    /// GPT partition name (empty for MBR).
    pub name: String,
    pub fs: Option<FsKind>,
}

impl Partition {
    pub fn range(&self) -> ByteRange {
        self.start..self.start + self.len
    }

    pub fn source(&self, disk: &Source) -> Source {
        if self.start == 0 && self.len == disk.size() {
            disk.clone()
        } else {
            Arc::new(SubSource::new(disk.clone(), self.start, self.len))
        }
    }

    /// Directory-friendly label, e.g. `partition2_NTFS`.
    pub fn label(&self) -> String {
        let fs = self.fs.map_or_else(|| "unknown".to_string(), |f| f.to_string());
        match self.scheme {
            Scheme::None => format!("volume_{fs}"),
            _ => format!("partition{}_{fs}", self.index),
        }
    }
}

/// Finds the volumes on `disk`. A source that starts with a recognised
/// boot sector is treated as a single volume; otherwise GPT, then MBR, is
/// tried. With no table at all the whole source is returned as one
/// partition with no file system (it can still be carved).
pub fn discover(disk: &Source) -> Vec<Partition> {
    let size = disk.size();
    if let Some(kind) = fs::detect(disk.as_ref()) {
        return vec![Partition {
            index: 1,
            start: 0,
            len: size,
            scheme: Scheme::None,
            kind: "volume".into(),
            name: String::new(),
            fs: Some(kind),
        }];
    }
    let mut parts = parse_gpt(disk.as_ref()).or_else(|| parse_mbr(disk.as_ref())).unwrap_or_default();
    parts.retain(|p| p.len > 0 && p.start < size);
    for p in &mut parts {
        p.len = p.len.min(size - p.start);
        p.fs = fs::detect(&SubSource::new(disk.clone(), p.start, p.len));
    }
    if parts.is_empty() {
        parts.push(Partition {
            index: 1,
            start: 0,
            len: size,
            scheme: Scheme::None,
            kind: "raw".into(),
            name: String::new(),
            fs: None,
        });
    }
    parts
}

/// Disk regions not covered by any partition (deleted partitions often
/// live here).
pub fn unpartitioned(disk_size: u64, parts: &[Partition]) -> Vec<ByteRange> {
    let used = normalize(parts.iter().map(Partition::range).collect());
    subtract(std::slice::from_ref(&(0..disk_size)), &used)
}

fn parse_mbr(src: &dyn ReadAt) -> Option<Vec<Partition>> {
    const SECTOR: u64 = 512;
    let mbr = src.read_vec(0, 512).ok()?;
    if mbr[510..512] != [0x55, 0xAA] {
        return None;
    }
    let entries: Vec<&[u8]> = (0..4).map(|i| &mbr[446 + i * 16..446 + (i + 1) * 16]).collect();
    // Boot flags other than 0x00/0x80 mean this is not a partition table.
    if entries.iter().any(|e| e[0] != 0 && e[0] != 0x80) {
        return None;
    }
    let mut out = Vec::new();
    let mut index = 0;
    for e in &entries {
        let ty = e[4];
        let start = u64::from(le32(e, 8)?);
        let count = u64::from(le32(e, 12)?);
        if ty == 0 || count == 0 {
            continue;
        }
        if matches!(ty, 0x05 | 0x0F | 0x85) {
            // Extended partition: a chain of EBRs, each describing one
            // logical partition (relative to itself) and the next EBR
            // (relative to the extended partition start).
            let mut ebr = start;
            for _ in 0..128 {
                let Ok(b) = src.read_vec(ebr * SECTOR, 512) else { break };
                if b[510..512] != [0x55, 0xAA] {
                    break;
                }
                let (lt, ls, lc) = (b[446 + 4], le32(&b, 446 + 8)?, le32(&b, 446 + 12)?);
                if lt != 0 && lc != 0 {
                    index += 1;
                    out.push(mbr_partition(index, (ebr + u64::from(ls)) * SECTOR, u64::from(lc) * SECTOR, lt));
                }
                let next = u64::from(le32(&b, 446 + 16 + 8)?);
                if next == 0 {
                    break;
                }
                ebr = start + next;
            }
            continue;
        }
        index += 1;
        out.push(mbr_partition(index, start * SECTOR, count * SECTOR, ty));
    }
    Some(out)
}

fn mbr_partition(index: usize, start: u64, len: u64, ty: u8) -> Partition {
    Partition { index, start, len, scheme: Scheme::Mbr, kind: format!("0x{ty:02X}"), name: String::new(), fs: None }
}

fn parse_gpt(src: &dyn ReadAt) -> Option<Vec<Partition>> {
    for ss in [512u64, 4096] {
        let Ok(h) = src.read_vec(ss, 92) else { continue };
        if &h[0..8] != b"EFI PART" {
            continue;
        }
        let entries_lba = le64(&h, 72)?;
        let count = le32(&h, 80)?.min(1024) as usize;
        let esize = le32(&h, 84)? as usize;
        if !(128..=4096).contains(&esize) {
            return None;
        }
        let table = src.read_vec(entries_lba * ss, count * esize).ok()?;
        let mut out = Vec::new();
        for (i, e) in table.chunks_exact(esize).enumerate() {
            if e[0..16].iter().all(|&b| b == 0) {
                continue;
            }
            let first = le64(e, 32)?;
            let last = le64(e, 40)?;
            if last < first {
                continue;
            }
            out.push(Partition {
                index: i + 1,
                start: first * ss,
                len: (last - first + 1) * ss,
                scheme: Scheme::Gpt,
                kind: gpt_type_name(&e[0..16]),
                name: utf16le(&e[56..128]),
                fs: None,
            });
        }
        return Some(out);
    }
    None
}

fn guid_string(g: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{}",
        u32::from_le_bytes([g[0], g[1], g[2], g[3]]),
        u16::from_le_bytes([g[4], g[5]]),
        u16::from_le_bytes([g[6], g[7]]),
        g[8],
        g[9],
        g[10..16].iter().map(|b| format!("{b:02X}")).collect::<String>()
    )
}

fn gpt_type_name(g: &[u8]) -> String {
    let s = guid_string(g);
    let name = match s.as_str() {
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft basic data",
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI system",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows recovery",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS+",
        "7C3457EF-0000-11AA-AA11-00306543ECAC" => "Apple APFS",
        _ => return s,
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemSource;

    #[test]
    fn mbr_with_logical_partitions() {
        let mut d = vec![0u8; 64 * 512];
        let put = |d: &mut Vec<u8>, sector: usize, slot: usize, ty: u8, start: u32, count: u32| {
            let o = sector * 512 + 446 + slot * 16;
            d[o + 4] = ty;
            d[o + 8..o + 12].copy_from_slice(&start.to_le_bytes());
            d[o + 12..o + 16].copy_from_slice(&count.to_le_bytes());
            d[sector * 512 + 510] = 0x55;
            d[sector * 512 + 511] = 0xAA;
        };
        put(&mut d, 0, 0, 0x07, 2, 10);
        put(&mut d, 0, 1, 0x0F, 20, 40); // extended
        put(&mut d, 20, 0, 0x0B, 1, 5); // logical #1 at 21
        put(&mut d, 20, 1, 0x05, 10, 20); // next EBR at 30
        put(&mut d, 30, 0, 0x0C, 2, 8); // logical #2 at 32
        let src: Source = Arc::new(MemSource(d));
        let parts = discover(&src);
        let starts: Vec<_> = parts.iter().map(|p| (p.start / 512, p.len / 512, p.kind.as_str())).collect();
        assert_eq!(starts, vec![(2, 10, "0x07"), (21, 5, "0x0B"), (32, 8, "0x0C")]);
        assert_eq!(unpartitioned(64 * 512, &parts)[0], 0..1024);
    }

    #[test]
    fn guid_formatting() {
        let g = [0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7];
        assert_eq!(gpt_type_name(&g), "Microsoft basic data");
    }
}
