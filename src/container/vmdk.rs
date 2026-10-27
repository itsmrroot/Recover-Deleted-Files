//! VMDK (VMware, VirtualBox). Either one "monolithic sparse" file, or a
//! text descriptor listing the extents the disk is made of: FLAT (raw),
//! SPARSE (hosted sparse extents, e.g. split into 2 GB files) or ZERO.
//! Sparse extents keep a grain directory of grain tables; "stream
//! optimized" ones (exported appliances) store each grain deflated. A
//! snapshot is a sparse disk whose absent grains come from its parent.

use std::io;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use super::{BlockCache, Blocks, bad, inflate, read_blocks};
use crate::bytes::{le32, le64};
use crate::source::{DiskSource, ReadAt, Source};

const GD_AT_END: u64 = u64::MAX;

pub fn open(file: DiskSource, path: &Path) -> Result<Source> {
    let mut head = [0u8; 512];
    file.read_exact_at(0, &mut head)?;
    if head.starts_with(b"KDMV") {
        let header = sparse_header(&file)?;
        let text = embedded_descriptor(&file, &header);
        let parent = parent(&text, path)?;
        return Ok(Arc::new(Sparse::open(file, header, parent, 0)?));
    }
    // A text descriptor.
    let mut text = vec![0u8; file.size().min(64 * 1024) as usize];
    file.read_exact_at(0, &mut text)?;
    let text = String::from_utf8_lossy(&text).to_string();
    let parent = parent(&text, path)?;
    let mut extents: Vec<(u64, Source)> = Vec::new();
    let mut at = 0u64;
    for line in text.lines().map(str::trim) {
        let mut words = line.split_whitespace();
        let (Some(access), Some(sectors), Some(kind)) = (words.next(), words.next(), words.next()) else { continue };
        if !matches!(access, "RW" | "RDONLY" | "NOACCESS") {
            continue;
        }
        let Ok(sectors) = sectors.parse::<u64>() else { continue };
        let len = sectors * 512;
        let name = line.find('"').and_then(|a| line[a + 1..].find('"').map(|b| &line[a + 1..a + 1 + b]));
        let src: Source = match (kind, name) {
            ("FLAT" | "VMFS", Some(name)) => {
                let offset = line.rsplit('"').next().and_then(|r| r.trim().parse::<u64>().ok()).unwrap_or(0) * 512;
                let f = super::sibling(path, name)?;
                Arc::new(crate::source::SubSource::new(Arc::new(f), offset, len))
            }
            ("SPARSE" | "VMFSSPARSE", Some(name)) => {
                let f = super::sibling(path, name)?;
                let header = sparse_header(&f)?;
                Arc::new(Sparse::open(f, header, parent.clone(), at)?)
            }
            ("ZERO", _) => Arc::new(Zero(len)),
            _ => bail!("unsupported VMDK extent: {line}"),
        };
        extents.push((at, src));
        at += len;
    }
    anyhow::ensure!(!extents.is_empty(), "VMDK descriptor without extents");
    Ok(Arc::new(crate::source::Concat::new(extents)))
}

#[derive(Clone, Copy)]
struct Header {
    capacity: u64,
    grain: u64,
    gtes_per_gt: u64,
    gd_offset: u64,
    descriptor: (u64, u64),
    compressed: bool,
}

fn sparse_header(f: &DiskSource) -> Result<Header> {
    let read = |at: u64| -> Result<Header> {
        let mut h = [0u8; 512];
        f.read_exact_at(at, &mut h)?;
        anyhow::ensure!(h.starts_with(b"KDMV"), "damaged VMDK header");
        let flags = le32(&h, 8).unwrap_or(0);
        Ok(Header {
            capacity: le64(&h, 12).unwrap_or(0) * 512,
            grain: le64(&h, 20).unwrap_or(0) * 512,
            descriptor: (le64(&h, 28).unwrap_or(0) * 512, le64(&h, 36).unwrap_or(0) * 512),
            gtes_per_gt: u64::from(le32(&h, 44).unwrap_or(0)),
            gd_offset: le64(&h, 56).unwrap_or(0),
            compressed: flags & (1 << 16) != 0 && u16::from_le_bytes([h[77], h[78]]) == 1,
        })
    };
    let mut h = read(0)?;
    if h.gd_offset == GD_AT_END {
        // Stream optimized: the real header is the footer, before the
        // end-of-stream marker.
        h = read(f.size().checked_sub(1024).context("truncated VMDK")?).context("VMDK footer")?;
    }
    anyhow::ensure!(h.grain >= 512 && h.gtes_per_gt > 0 && h.gd_offset != GD_AT_END, "damaged VMDK header");
    Ok(h)
}

fn embedded_descriptor(f: &DiskSource, h: &Header) -> String {
    let (at, len) = h.descriptor;
    let mut d = vec![0u8; len.min(1 << 20) as usize];
    if at == 0 || f.read_exact_at(at, &mut d).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&d).trim_end_matches('\0').to_string()
}

/// The parent of a snapshot, from `parentFileNameHint`.
fn parent(descriptor: &str, path: &Path) -> Result<Option<Source>> {
    let field = |key: &str| {
        descriptor.lines().find_map(|l| l.trim().strip_prefix(key)).map(|v| v.trim_start_matches([' ', '=']).trim())
    };
    if field("parentCID").is_none_or(|c| c.eq_ignore_ascii_case("ffffffff")) {
        return Ok(None);
    }
    let Some(hint) = field("parentFileNameHint") else { return Ok(None) };
    let f = super::sibling(path, hint).context("this VMDK is a snapshot; its parent disk is needed")?;
    Ok(Some(super::open(f.path())?.0))
}

/// A hosted sparse extent.
struct Sparse {
    file: DiskSource,
    h: Header,
    /// Sector offsets of the grain tables.
    gd: Vec<u32>,
    parent: Option<Source>,
    /// Where this extent starts on the disk (for reading from the parent).
    at: u64,
    tables: BlockCache,
    grains: BlockCache,
}

impl Sparse {
    fn open(file: DiskSource, h: Header, parent: Option<Source>, at: u64) -> Result<Self> {
        let grains = h.capacity.div_ceil(h.grain);
        let tables = grains.div_ceil(h.gtes_per_gt) as usize;
        let mut raw = vec![0u8; tables * 4];
        file.read_exact_at(h.gd_offset * 512, &mut raw).context("reading the VMDK grain directory")?;
        let gd = raw.as_chunks::<4>().0.iter().map(|e| u32::from_le_bytes([e[0], e[1], e[2], e[3]])).collect();
        Ok(Self { file, h, gd, parent, at, tables: BlockCache::new(16), grains: BlockCache::new(32) })
    }

    /// The sector of grain `g`: 0 absent, 1 zeroed.
    fn grain_sector(&self, g: u64) -> io::Result<u32> {
        let t = (g / self.h.gtes_per_gt) as usize;
        let Some(&table) = self.gd.get(t) else { return Ok(0) };
        if table == 0 {
            return Ok(0);
        }
        let gt = self.tables.get(t as u64, || {
            let mut raw = vec![0u8; self.h.gtes_per_gt as usize * 4];
            self.file.read_exact_at(u64::from(table) * 512, &mut raw)?;
            Ok(raw)
        })?;
        let e = (g % self.h.gtes_per_gt) as usize * 4;
        Ok(le32(&gt, e).unwrap_or(0))
    }
}

impl Blocks for Sparse {
    fn disk_size(&self) -> u64 {
        self.h.capacity
    }

    fn block_size(&self) -> u64 {
        self.h.grain
    }

    fn read_in_block(&self, index: u64, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        match self.grain_sector(index)? {
            0 => match &self.parent {
                Some(p) => p.read_exact_at(self.at + index * self.h.grain + offset, buf),
                None => {
                    buf.fill(0);
                    Ok(())
                }
            },
            1 => {
                buf.fill(0);
                Ok(())
            }
            s if self.h.compressed => {
                let grain = self.grains.get(index, || {
                    // u64 lba, u32 size, then the deflated grain.
                    let at = u64::from(s) * 512;
                    let mut head = [0u8; 12];
                    self.file.read_exact_at(at, &mut head)?;
                    let len = le32(&head, 8).unwrap_or(0) as usize;
                    let mut data = vec![0u8; len];
                    self.file.read_exact_at(at + 12, &mut data)?;
                    let mut g = inflate(&data, true, self.h.grain as usize)?;
                    g.resize(self.h.grain as usize, 0);
                    Ok(g)
                })?;
                let o = offset as usize;
                buf.copy_from_slice(grain.get(o..o + buf.len()).ok_or_else(|| bad("short VMDK grain"))?);
                Ok(())
            }
            s => self.file.read_exact_at(u64::from(s) * 512 + offset, buf),
        }
    }
}

impl ReadAt for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        read_blocks(self, offset, buf)
    }

    fn size(&self) -> u64 {
        self.h.capacity
    }
}

struct Zero(u64);

impl ReadAt for Zero {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.0.saturating_sub(offset)) as usize;
        buf[..n].fill(0);
        Ok(n)
    }

    fn size(&self) -> u64 {
        self.0
    }
}
