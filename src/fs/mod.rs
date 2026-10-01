//! File-system aware recovery.
//!
//! Each supported file system finds deleted entries in its own metadata and
//! describes where their data lived as a list of [`Extent`]s. Extraction is
//! shared and lives here.

pub mod exfat;
pub mod fat;
pub mod ntfs;

use std::fmt;
use std::io::{self, Write};

use anyhow::Result;
use chrono::NaiveDateTime;
use serde::Serialize;

use crate::ranges::ByteRange;
use crate::source::{ReadAt, Source, read_tolerant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FsKind {
    Ntfs,
    Fat12,
    Fat16,
    Fat32,
    ExFat,
}

impl fmt::Display for FsKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FsKind::Ntfs => "NTFS",
            FsKind::Fat12 => "FAT12",
            FsKind::Fat16 => "FAT16",
            FsKind::Fat32 => "FAT32",
            FsKind::ExFat => "exFAT",
        })
    }
}

/// A contiguous piece of file data, in bytes relative to the volume start.
/// `offset == None` is a sparse (never written, reads as zeros) region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub offset: Option<u64>,
    pub len: u64,
}

#[derive(Debug, Clone)]
pub enum FileData {
    /// Stored inside the metadata itself (small NTFS files).
    Resident(Vec<u8>),
    Extents(Vec<Extent>),
    /// NTFS LZNT1-compressed stream: extents in VCN order, grouped into
    /// compression units of `unit` bytes.
    Compressed {
        extents: Vec<Extent>,
        unit: u64,
    },
    /// Metadata survived but the location of the data did not.
    Lost,
}

/// How likely the data is to still be intact, judged from the volume's
/// allocation map: a deleted file's clusters that have since been
/// allocated to something else have (probably) been overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    Recoverable,
    /// Percentage of clusters still unallocated.
    Partial(u8),
    Overwritten,
}

impl Condition {
    pub fn from_counts(free: u64, total: u64) -> Self {
        if total == 0 || free == total {
            Condition::Recoverable
        } else if free == 0 {
            Condition::Overwritten
        } else {
            Condition::Partial(((free * 100) / total).min(99) as u8)
        }
    }

    pub fn is_recoverable(&self) -> bool {
        !matches!(self, Condition::Overwritten)
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Condition::Recoverable => f.write_str("recoverable"),
            Condition::Partial(p) => write!(f, "partial ({p}% intact)"),
            Condition::Overwritten => f.write_str("overwritten"),
        }
    }
}

impl Serialize for Condition {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// A deleted file found through file-system metadata.
#[derive(Debug, Clone, Serialize)]
pub struct DeletedFile {
    /// MFT record number (NTFS) or directory-entry byte offset (FAT/exFAT).
    pub id: u64,
    /// Reconstructed path, `/`-separated, relative to the volume root.
    pub path: String,
    pub size: u64,
    pub created: Option<NaiveDateTime>,
    pub modified: Option<NaiveDateTime>,
    pub condition: Condition,
    /// Anything the user should know (encrypted, assumed contiguous, ...).
    pub note: Option<String>,
    #[serde(skip)]
    pub data: FileData,
}

impl DeletedFile {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// Byte ranges (volume relative) holding this file's data.
    pub fn data_ranges(&self) -> Vec<ByteRange> {
        let ex = match &self.data {
            FileData::Extents(e) | FileData::Compressed { extents: e, .. } => e,
            _ => return Vec::new(),
        };
        ex.iter().filter_map(|e| e.offset.map(|o| o..o + e.len)).collect()
    }
}

/// A mounted-for-reading file system.
pub trait Volume: Send + Sync {
    fn kind(&self) -> FsKind;
    /// One-line human description (cluster size, record count, ...).
    fn describe(&self) -> String;
    /// Walks the metadata and returns every deleted file found.
    /// `progress(done, total)` is called periodically.
    fn scan_deleted(&self, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<DeletedFile>>;
    /// Unallocated byte ranges (volume relative) — where deleted data lives.
    fn free_ranges(&self) -> Result<Vec<ByteRange>>;
    /// The underlying volume source (for extraction).
    fn source(&self) -> &Source;
}

/// Identifies the file system whose boot sector starts at offset 0 of `src`.
pub fn detect(src: &dyn ReadAt) -> Option<FsKind> {
    let mut bs = [0u8; 512];
    src.read_exact_at(0, &mut bs).ok()?;
    if &bs[3..11] == b"NTFS    " {
        return Some(FsKind::Ntfs);
    }
    if &bs[3..11] == b"EXFAT   " {
        return Some(FsKind::ExFat);
    }
    fat::detect_boot_sector(&bs)
}

pub fn open(src: Source) -> Result<Box<dyn Volume>> {
    match detect(src.as_ref()) {
        Some(FsKind::Ntfs) => Ok(Box::new(ntfs::Ntfs::open(src)?)),
        Some(FsKind::ExFat) => Ok(Box::new(exfat::ExFat::open(src)?)),
        Some(_) => Ok(Box::new(fat::Fat::open(src)?)),
        None => anyhow::bail!("no supported file system found"),
    }
}

/// Converts a Windows FILETIME (100 ns ticks since 1601-01-01 UTC).
pub fn filetime(ft: u64) -> Option<NaiveDateTime> {
    if ft == 0 {
        return None;
    }
    let secs = (ft / 10_000_000) as i64 - 11_644_473_600;
    let nanos = ((ft % 10_000_000) * 100) as u32;
    chrono::DateTime::from_timestamp(secs, nanos).map(|d| d.naive_utc())
}

/// Converts a DOS date/time pair (FAT, exFAT). Local time, 2 s resolution.
pub fn dos_datetime(date: u16, time: u16) -> Option<NaiveDateTime> {
    let day = u32::from(date & 0x1F);
    let month = u32::from((date >> 5) & 0x0F);
    let year = 1980 + i32::from(date >> 9);
    let d = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    let sec = u32::from(time & 0x1F) * 2;
    let min = u32::from((time >> 5) & 0x3F);
    let hour = u32::from(time >> 11);
    d.and_hms_opt(hour, min, sec)
}

/// Statistics from extracting one file.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExtractStats {
    pub written: u64,
    pub unreadable: u64,
}

const COPY_CHUNK: usize = 1 << 20;

/// Writes the content of `file` (read from volume `vol`) into `out`.
pub fn extract(vol: &dyn ReadAt, file: &DeletedFile, out: &mut dyn Write) -> io::Result<ExtractStats> {
    let mut st = ExtractStats::default();
    match &file.data {
        FileData::Lost => {}
        FileData::Resident(bytes) => {
            let n = bytes.len().min(file.size as usize);
            out.write_all(&bytes[..n])?;
            st.written = n as u64;
        }
        FileData::Extents(extents) => {
            let mut remaining = file.size;
            let mut buf = vec![0u8; COPY_CHUNK];
            for e in extents {
                if remaining == 0 {
                    break;
                }
                let mut left = e.len.min(remaining);
                let mut pos = e.offset;
                while left > 0 {
                    let n = left.min(COPY_CHUNK as u64) as usize;
                    match pos {
                        Some(p) => {
                            st.unreadable += read_tolerant(vol, p, &mut buf[..n]);
                            pos = Some(p + n as u64);
                        }
                        None => buf[..n].fill(0),
                    }
                    out.write_all(&buf[..n])?;
                    left -= n as u64;
                    remaining -= n as u64;
                    st.written += n as u64;
                }
            }
        }
        FileData::Compressed { extents, unit } => {
            st = extract_compressed(vol, extents, *unit, file.size, out)?;
        }
    }
    Ok(st)
}

/// NTFS compressed streams: each compression unit (normally 16 clusters) is
/// stored either raw (fully allocated), LZNT1-compressed (allocated prefix
/// followed by a sparse tail), or not at all (fully sparse = zeros).
fn extract_compressed(
    vol: &dyn ReadAt,
    extents: &[Extent],
    unit: u64,
    size: u64,
    out: &mut dyn Write,
) -> io::Result<ExtractStats> {
    let mut st = ExtractStats::default();
    let mut cursor = ExtentCursor::new(extents);
    let mut remaining = size;
    while remaining > 0 {
        let pieces = cursor.take(unit);
        if pieces.is_empty() {
            break;
        }
        let stored: u64 = pieces.iter().filter(|p| p.offset.is_some()).map(|p| p.len).sum();
        let span: u64 = pieces.iter().map(|p| p.len).sum();
        let mut data = Vec::with_capacity(stored as usize);
        for p in pieces.iter().filter(|p| p.offset.is_some()) {
            let start = data.len();
            data.resize(start + p.len as usize, 0);
            st.unreadable += read_tolerant(vol, p.offset.unwrap_or(0), &mut data[start..]);
        }
        let mut plain = if stored == 0 {
            vec![0u8; span as usize]
        } else if stored >= span {
            data
        } else {
            let mut d = Vec::with_capacity(unit as usize);
            ntfs::lznt1::decompress(&data, &mut d);
            d
        };
        plain.resize(unit as usize, 0);
        let n = remaining.min(unit) as usize;
        out.write_all(&plain[..n])?;
        st.written += n as u64;
        remaining -= n as u64;
    }
    Ok(st)
}

/// Walks a list of extents handing out fixed-size slices.
struct ExtentCursor<'a> {
    extents: &'a [Extent],
    idx: usize,
    used: u64,
}

impl<'a> ExtentCursor<'a> {
    fn new(extents: &'a [Extent]) -> Self {
        Self { extents, idx: 0, used: 0 }
    }

    fn take(&mut self, mut want: u64) -> Vec<Extent> {
        let mut out = Vec::new();
        while want > 0 && self.idx < self.extents.len() {
            let e = self.extents[self.idx];
            let avail = e.len - self.used;
            let n = avail.min(want);
            out.push(Extent { offset: e.offset.map(|o| o + self.used), len: n });
            self.used += n;
            want -= n;
            if self.used == e.len {
                self.idx += 1;
                self.used = 0;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemSource;

    fn file(data: FileData, size: u64) -> DeletedFile {
        DeletedFile {
            id: 0,
            path: "x".into(),
            size,
            created: None,
            modified: None,
            condition: Condition::Recoverable,
            note: None,
            data,
        }
    }

    #[test]
    fn extracts_fragmented_and_sparse() {
        let vol = MemSource((0..=255u8).cycle().take(4096).collect());
        let f = file(
            FileData::Extents(vec![
                Extent { offset: Some(10), len: 4 },
                Extent { offset: None, len: 3 },
                Extent { offset: Some(100), len: 10 },
            ]),
            9,
        );
        let mut out = Vec::new();
        let st = extract(&vol, &f, &mut out).unwrap();
        assert_eq!(out, vec![10, 11, 12, 13, 0, 0, 0, 100, 101]);
        assert_eq!(st.written, 9);
    }

    #[test]
    fn condition_buckets() {
        assert_eq!(Condition::from_counts(10, 10), Condition::Recoverable);
        assert_eq!(Condition::from_counts(0, 10), Condition::Overwritten);
        assert_eq!(Condition::from_counts(5, 10), Condition::Partial(50));
        assert_eq!(Condition::from_counts(0, 0), Condition::Recoverable);
    }

    #[test]
    fn time_conversions() {
        // 2020-01-01 00:00:00 UTC
        let t = filetime(132_223_104_000_000_000).unwrap();
        assert_eq!(t.to_string(), "2020-01-01 00:00:00");
        // 2023-06-15 13:45:30
        let date = ((2023 - 1980) << 9) | (6 << 5) | 15;
        let time = (13 << 11) | (45 << 5) | 15;
        assert_eq!(dos_datetime(date, time).unwrap().to_string(), "2023-06-15 13:45:30");
        assert_eq!(dos_datetime(0, 0), None);
    }

    #[test]
    fn compressed_units() {
        // Unit = 16 bytes for the test. Unit 0 stored raw, unit 1 sparse,
        // unit 2 compressed (6 stored bytes + sparse tail).
        let mut disk = vec![0u8; 256];
        disk[0..16].copy_from_slice(b"0123456789abcdef");
        let comp = [0x05, 0xB0, 0x08, b'a', b'b', b'c', 0x06, 0x20];
        disk[64..72].copy_from_slice(&comp);
        let vol = MemSource(disk);
        let f = file(
            FileData::Compressed {
                extents: vec![
                    Extent { offset: Some(0), len: 16 },
                    Extent { offset: None, len: 16 },
                    Extent { offset: Some(64), len: 8 },
                    Extent { offset: None, len: 8 },
                ],
                unit: 16,
            },
            44,
        );
        let mut out = Vec::new();
        extract(&vol, &f, &mut out).unwrap();
        assert_eq!(&out[..16], b"0123456789abcdef");
        assert!(out[16..32].iter().all(|&b| b == 0));
        assert_eq!(&out[32..44], b"abcabcabcabc");
    }
}
