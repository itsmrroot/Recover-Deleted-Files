//! VHD (Virtual PC, Hyper-V generation 1, Windows "Create VHD"): fixed,
//! dynamic and differencing disks. All numbers are big-endian.
//!
//! A fixed disk is the raw disk followed by a 512-byte footer. A dynamic
//! disk has a block allocation table: each block (2 MiB usually) is either
//! absent (zeros) or stored as a sector bitmap followed by its data. A
//! differencing disk is a dynamic disk whose absent sectors come from its
//! parent.

use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Blocks, bad, read_blocks};
use crate::bytes::{be32, be64};
use crate::source::{DiskSource, ReadAt, Source};

pub struct Vhd {
    file: DiskSource,
    size: u64,
    layout: Layout,
}

enum Layout {
    Fixed,
    Dynamic {
        block_size: u64,
        /// Sector of each block in the file, `None` if absent.
        bat: Vec<Option<u64>>,
        bitmap_bytes: u64,
        parent: Option<Source>,
    },
}

impl Vhd {
    pub fn open(file: DiskSource, path: &Path) -> Result<Self> {
        let mut footer = [0u8; 512];
        file.read_exact_at(file.size() - 512, &mut footer)?;
        if !footer.starts_with(b"conectix") {
            // A dynamic disk whose last sector was cut off: its copy is at 0.
            file.read_exact_at(0, &mut footer)?;
        }
        let size = be64(&footer, 48).context("damaged VHD footer")?;
        let disk_type = be32(&footer, 60).unwrap_or(0);
        let layout = match disk_type {
            2 => {
                anyhow::ensure!(file.size() >= size, "the VHD file is shorter than its disk");
                Layout::Fixed
            }
            3 | 4 => {
                let at = be64(&footer, 16).context("damaged VHD footer")?;
                let mut h = vec![0u8; 1024];
                file.read_exact_at(at, &mut h).context("reading the VHD header")?;
                anyhow::ensure!(h.starts_with(b"cxsparse"), "damaged VHD header");
                let table = be64(&h, 16).unwrap_or(0);
                let entries = be32(&h, 28).unwrap_or(0) as usize;
                let block_size = u64::from(be32(&h, 32).unwrap_or(0));
                anyhow::ensure!(block_size >= 512 && block_size.is_power_of_two(), "bad VHD block size");
                let mut raw = vec![0u8; entries * 4];
                file.read_exact_at(table, &mut raw).context("reading the VHD block table")?;
                let bat = raw
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|e| {
                        let s = u32::from_be_bytes([e[0], e[1], e[2], e[3]]);
                        (s != u32::MAX).then_some(u64::from(s))
                    })
                    .collect();
                let bitmap_bytes = (block_size / 512).div_ceil(8).div_ceil(512) * 512;
                let parent = if disk_type == 4 { Some(open_parent(&file, &h, path)?) } else { None };
                Layout::Dynamic { block_size, bat, bitmap_bytes, parent }
            }
            t => bail!("unsupported VHD type {t}"),
        };
        Ok(Self { file, size, layout })
    }
}

/// The parent of a differencing disk, found through its locators (or, as
/// a last resort, its name next to this file).
fn open_parent(file: &DiskSource, h: &[u8], path: &Path) -> Result<Source> {
    let mut names = Vec::new();
    for i in 0..8 {
        let e = 576 + i * 24;
        let code = be32(h, e).unwrap_or(0);
        let len = be32(h, e + 8).unwrap_or(0) as usize;
        let off = be64(h, e + 16).unwrap_or(0);
        // W2ru / W2ku: relative / absolute Windows path, UTF-16LE.
        if (code == 0x5732_7275 || code == 0x5732_6B75) && len > 0 && len < 4096 {
            let mut raw = vec![0u8; len];
            if file.read_exact_at(off, &mut raw).is_ok() {
                let utf16: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                names.push(String::from_utf16_lossy(&utf16).trim_end_matches('\0').to_string());
            }
        }
    }
    // The parent's name, UTF-16BE, at 64.
    let utf16: Vec<u16> = h[64..576]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    names.push(String::from_utf16_lossy(&utf16));
    let mut last = None;
    for name in names.iter().filter(|n| !n.is_empty()) {
        match super::sibling(path, name) {
            Ok(parent) => {
                let parent_path = parent.path().to_string();
                return Ok(super::open(&parent_path)?.0);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no parent named")))
        .context("this is a differencing VHD; its parent disk is needed")
}

impl Blocks for Vhd {
    fn disk_size(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u64 {
        match &self.layout {
            Layout::Fixed => 1 << 20,
            Layout::Dynamic { block_size, .. } => *block_size,
        }
    }

    fn read_in_block(&self, index: u64, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let (block_size, bat, bitmap_bytes, parent) = match &self.layout {
            Layout::Fixed => return self.file.read_exact_at(index * (1 << 20) + offset, buf),
            Layout::Dynamic { block_size, bat, bitmap_bytes, parent } => (*block_size, bat, *bitmap_bytes, parent),
        };
        let disk_pos = index * block_size + offset;
        let Some(Some(sector)) = bat.get(index as usize) else {
            return match parent {
                Some(p) => p.read_exact_at(disk_pos, buf),
                None => {
                    buf.fill(0);
                    Ok(())
                }
            };
        };
        let data = sector * 512 + bitmap_bytes;
        let Some(parent) = parent else {
            return self.file.read_exact_at(data + offset, buf);
        };
        // Differencing: sector by sector, from here or from the parent.
        let mut bitmap = vec![0u8; bitmap_bytes as usize];
        self.file.read_exact_at(sector * 512, &mut bitmap)?;
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let s = pos / 512;
            let n = ((512 - pos % 512) as usize).min(buf.len() - done);
            let here = bitmap.get((s / 8) as usize).is_some_and(|b| b & (0x80 >> (s % 8)) != 0);
            let out = &mut buf[done..done + n];
            if here {
                self.file.read_exact_at(data + pos, out)?;
            } else {
                parent.read_exact_at(disk_pos + done as u64, out).map_err(|e| bad(format!("parent disk: {e}")))?;
            }
            done += n;
        }
        Ok(())
    }
}

impl ReadAt for Vhd {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        read_blocks(self, offset, buf)
    }

    fn size(&self) -> u64 {
        self.size
    }
}
