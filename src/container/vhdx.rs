//! VHDX (Hyper-V, Windows 8 and later). All numbers are little-endian.
//!
//! Two headers (the newer valid one counts) point to a region table, which
//! points to the block allocation table (BAT) and the metadata (block size,
//! disk size, sector size, parent). The BAT interleaves one sector-bitmap
//! entry after every "chunk" of payload entries; the bitmaps only matter
//! for differencing disks.

use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Blocks, bad, crc32c, read_blocks};
use crate::bytes::{le16, le32, le64};
use crate::source::{DiskSource, ReadAt, Source};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

const BAT: [u8; 16] = [0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08];
const METADATA: [u8; 16] =
    [0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B, 0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E];
const FILE_PARAMETERS: [u8; 16] =
    [0x37, 0x67, 0xA1, 0xCA, 0x36, 0xFA, 0x43, 0x4D, 0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B];
const DISK_SIZE: [u8; 16] =
    [0x24, 0x42, 0xA5, 0x2F, 0x1B, 0xCD, 0x76, 0x48, 0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4, 0xB8];
const SECTOR_SIZE: [u8; 16] =
    [0x1D, 0xBF, 0x41, 0x81, 0x6F, 0xA9, 0x09, 0x47, 0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB, 0x5F];
const PARENT_LOCATOR: [u8; 16] =
    [0x2D, 0x5F, 0xD3, 0xA8, 0x0B, 0xB3, 0x4D, 0x45, 0xAB, 0xF7, 0xD3, 0xD8, 0x48, 0x34, 0xAB, 0x0C];

// Payload block states.
const ZERO: u64 = 2;
const UNMAPPED: u64 = 3;
const FULLY_PRESENT: u64 = 6;
const PARTIALLY_PRESENT: u64 = 7;

pub struct Vhdx {
    file: DiskSource,
    size: u64,
    block_size: u64,
    sector_size: u64,
    chunk_ratio: u64,
    bat: Vec<u64>,
    parent: Option<Source>,
}

impl Vhdx {
    pub fn open(file: DiskSource) -> Result<Self> {
        let path = file.path().to_string();
        // The valid header with the highest sequence number.
        let mut best: Option<(u64, Vec<u8>)> = None;
        for at in [64 * KIB, 128 * KIB] {
            let mut h = vec![0u8; 4096];
            if file.read_exact_at(at, &mut h).is_err() || !h.starts_with(b"head") {
                continue;
            }
            let sum = le32(&h, 4).unwrap_or(0);
            h[4..8].fill(0);
            if crc32c(&h) != sum {
                continue;
            }
            let seq = le64(&h, 8).unwrap_or(0);
            if best.as_ref().is_none_or(|(s, _)| seq > *s) {
                best = Some((seq, h));
            }
        }
        let Some((_, header)) = best else { bail!("damaged VHDX: no valid header") };
        if header[48..64].iter().any(|&b| b != 0) {
            // A log that was not replayed: the newest few writes are not
            // in their place. What is there is still read.
            log::warn!("{path}: the VHDX log was not replayed (the disk was not shut down cleanly)");
        }
        let (bat_at, bat_len, meta_at, meta_len) = regions(&file)?;
        let mut meta = vec![0u8; meta_len as usize];
        file.read_exact_at(meta_at, &mut meta).context("reading the VHDX metadata")?;
        anyhow::ensure!(meta.starts_with(b"metadata"), "damaged VHDX metadata");
        let item = |id: &[u8; 16]| -> Option<&[u8]> {
            let count = usize::from(le16(&meta, 10)?);
            (0..count).find_map(|i| {
                let e = 32 + i * 32;
                if meta.get(e..e + 16)? != id {
                    return None;
                }
                let off = le32(&meta, e + 16)? as usize;
                let len = le32(&meta, e + 20)? as usize;
                meta.get(off..off + len)
            })
        };
        let params = item(&FILE_PARAMETERS).context("VHDX without block size")?;
        let block_size = u64::from(le32(params, 0).unwrap_or(0));
        let has_parent = le32(params, 4).unwrap_or(0) & 2 != 0;
        let size = item(&DISK_SIZE).and_then(|d| le64(d, 0)).context("VHDX without disk size")?;
        let sector_size = item(&SECTOR_SIZE).and_then(|d| le32(d, 0)).map_or(512, u64::from);
        anyhow::ensure!(block_size >= MIB && block_size.is_power_of_two(), "bad VHDX block size");
        let chunk_ratio = (1u64 << 23) * sector_size / block_size;
        let mut raw = vec![0u8; bat_len as usize];
        file.read_exact_at(bat_at, &mut raw).context("reading the VHDX block table")?;
        let bat = raw.as_chunks::<8>().0.iter().map(|e| u64::from_le_bytes(*e)).collect();
        let parent = if has_parent {
            let locator = item(&PARENT_LOCATOR).context("differencing VHDX without parent locator")?;
            Some(open_parent(locator, Path::new(&path))?)
        } else {
            None
        };
        Ok(Self { file, size, block_size, sector_size, chunk_ratio, bat, parent })
    }

    fn entry(&self, block: u64) -> u64 {
        self.bat.get((block + block / self.chunk_ratio) as usize).copied().unwrap_or(0)
    }

    /// The sector bitmap of the chunk holding `block`, if stored.
    fn bitmap(&self, block: u64) -> Option<u64> {
        let chunk = block / self.chunk_ratio;
        let e = *self.bat.get(((chunk + 1) * (self.chunk_ratio + 1) - 1) as usize)?;
        (e & 7 == FULLY_PRESENT).then_some((e >> 20) * MIB)
    }
}

/// Where the BAT and the metadata are (the first valid region table).
fn regions(file: &DiskSource) -> Result<(u64, u64, u64, u64)> {
    for at in [192 * KIB, 256 * KIB] {
        let mut t = vec![0u8; 64 * KIB as usize];
        if file.read_exact_at(at, &mut t).is_err() || !t.starts_with(b"regi") {
            continue;
        }
        let sum = le32(&t, 4).unwrap_or(0);
        t[4..8].fill(0);
        if crc32c(&t) != sum {
            continue;
        }
        let count = le32(&t, 8).unwrap_or(0) as usize;
        let (mut bat, mut meta) = (None, None);
        for i in 0..count.min(2047) {
            let e = 16 + i * 32;
            let place = (le64(&t, e + 16).unwrap_or(0), u64::from(le32(&t, e + 24).unwrap_or(0)));
            match &t[e..e + 16] {
                id if id == BAT => bat = Some(place),
                id if id == METADATA => meta = Some(place),
                _ => {}
            }
        }
        if let (Some(b), Some(m)) = (bat, meta) {
            return Ok((b.0, b.1, m.0, m.1));
        }
    }
    bail!("damaged VHDX: no valid region table")
}

/// The parent disk named in the locator: relative path first.
fn open_parent(locator: &[u8], path: &Path) -> Result<Source> {
    let count = usize::from(le16(locator, 18).unwrap_or(0));
    let text = |off: usize, len: usize| -> Option<String> {
        let raw = locator.get(off..off + len)?;
        let utf16: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&utf16))
    };
    let mut paths = Vec::new();
    for i in 0..count {
        let e = 20 + i * 12;
        let (Some(ko), Some(vo), Some(kl), Some(vl)) =
            (le32(locator, e), le32(locator, e + 4), le16(locator, e + 8), le16(locator, e + 10))
        else {
            continue;
        };
        let (Some(k), Some(v)) = (text(ko as usize, kl as usize), text(vo as usize, vl as usize)) else { continue };
        match k.as_str() {
            "relative_path" => paths.insert(0, v),
            "absolute_win32_path" | "volume_path" => paths.push(v),
            _ => {}
        }
    }
    let mut last = None;
    for p in &paths {
        match super::sibling(path, p) {
            Ok(f) => return Ok(super::open(f.path())?.0),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no parent named")))
        .context("this is a differencing VHDX; its parent disk is needed")
}

impl Blocks for Vhdx {
    fn disk_size(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u64 {
        self.block_size
    }

    fn read_in_block(&self, index: u64, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let e = self.entry(index);
        let disk_pos = index * self.block_size + offset;
        let data = (e >> 20) * MIB;
        match (e & 7, &self.parent) {
            (FULLY_PRESENT, _) => self.file.read_exact_at(data + offset, buf),
            (PARTIALLY_PRESENT, Some(parent)) => {
                let Some(bitmap_at) = self.bitmap(index) else {
                    return Err(bad("VHDX sector bitmap missing"));
                };
                // One bit per sector of the chunk, low bit first.
                let first_sector = (index % self.chunk_ratio) * self.block_size / self.sector_size;
                let ss = self.sector_size;
                let mut done = 0usize;
                while done < buf.len() {
                    let pos = offset + done as u64;
                    let s = first_sector + pos / ss;
                    let n = ((ss - pos % ss) as usize).min(buf.len() - done);
                    let mut byte = [0u8];
                    self.file.read_exact_at(bitmap_at + s / 8, &mut byte)?;
                    let out = &mut buf[done..done + n];
                    if byte[0] & (1 << (s % 8)) != 0 {
                        self.file.read_exact_at(data + pos, out)?;
                    } else {
                        parent.read_exact_at(disk_pos + done as u64, out)?;
                    }
                    done += n;
                }
                Ok(())
            }
            (ZERO | UNMAPPED, _) => {
                buf.fill(0);
                Ok(())
            }
            // Not present: the parent's, or never written.
            (_, Some(parent)) => parent.read_exact_at(disk_pos, buf),
            (_, None) => {
                buf.fill(0);
                Ok(())
            }
        }
    }
}

impl ReadAt for Vhdx {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        read_blocks(self, offset, buf)
    }

    fn size(&self) -> u64 {
        self.size
    }
}
