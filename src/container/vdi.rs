//! VDI (VirtualBox). A header, then a block map: each block (1 MiB) is
//! stored, never written (zeros), or — in a snapshot — the parent's.
//! Snapshot parents are named by UUID only, so they are looked for among
//! the .vdi files next to the snapshot and in the folder above.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Blocks, read_blocks};
use crate::bytes::{le32, le64};
use crate::source::{DiskSource, ReadAt, Source};

pub const SIGNATURE: u32 = 0xBEDA_107F;
const NOT_ALLOCATED: u32 = u32::MAX;
const ZEROED: u32 = u32::MAX - 1;

pub struct Vdi {
    file: DiskSource,
    size: u64,
    block_size: u64,
    /// Bytes of extra data before each block's data.
    extra: u64,
    data_at: u64,
    map: Vec<u32>,
    parent: Option<Source>,
}

impl Vdi {
    pub fn open(file: DiskSource) -> Result<Self> {
        let h = header(&file)?;
        let image_type = le32(&h, 0x4C).unwrap_or(0);
        let map_at = u64::from(le32(&h, 0x154).unwrap_or(0));
        let data_at = u64::from(le32(&h, 0x158).unwrap_or(0));
        let size = le64(&h, 0x170).unwrap_or(0);
        let block_size = u64::from(le32(&h, 0x178).unwrap_or(0));
        let extra = u64::from(le32(&h, 0x17C).unwrap_or(0));
        let blocks = le32(&h, 0x180).unwrap_or(0) as usize;
        anyhow::ensure!(block_size >= 512 && size > 0, "damaged VDI header");
        let mut raw = vec![0u8; blocks * 4];
        file.read_exact_at(map_at, &mut raw).context("reading the VDI block map")?;
        let map = raw.as_chunks::<4>().0.iter().map(|e| u32::from_le_bytes([e[0], e[1], e[2], e[3]])).collect();
        let parent = if image_type == 4 {
            let want: [u8; 16] = h[0x1A8..0x1B8].try_into()?;
            Some(find_parent(Path::new(file.path()), &want)?)
        } else {
            None
        };
        Ok(Self { file, size, block_size, extra, data_at, map, parent })
    }
}

fn header(file: &DiskSource) -> Result<Vec<u8>> {
    let mut h = vec![0u8; 0x200];
    file.read_exact_at(0, &mut h)?;
    anyhow::ensure!(le32(&h, 0x40) == Some(SIGNATURE), "not a VDI file");
    let version = le32(&h, 0x44).unwrap_or(0);
    if version >> 16 != 1 {
        bail!("unsupported VDI version {}.{}", version >> 16, version & 0xFFFF);
    }
    Ok(h)
}

fn find_parent(path: &Path, uuid: &[u8; 16]) -> Result<Source> {
    let here = path.parent().unwrap_or(Path::new("."));
    let mut dirs: Vec<PathBuf> = vec![here.to_path_buf()];
    if let Some(up) = here.parent() {
        dirs.push(up.to_path_buf());
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| !x.eq_ignore_ascii_case("vdi")) || p == path {
                continue;
            }
            let Ok(f) = DiskSource::open(&p.to_string_lossy()) else { continue };
            if header(&f).is_ok_and(|h| &h[0x188..0x198] == uuid) {
                return Ok(super::open(&p.to_string_lossy())?.0);
            }
        }
    }
    bail!("this VDI is a snapshot; its parent disk was not found next to it")
}

impl Blocks for Vdi {
    fn disk_size(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u64 {
        self.block_size
    }

    fn read_in_block(&self, index: u64, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        match self.map.get(index as usize).copied().unwrap_or(NOT_ALLOCATED) {
            NOT_ALLOCATED if self.parent.is_some() => {
                self.parent.as_ref().map_or(Ok(()), |p| p.read_exact_at(index * self.block_size + offset, buf))
            }
            NOT_ALLOCATED | ZEROED => {
                buf.fill(0);
                Ok(())
            }
            b => {
                let at = self.data_at + u64::from(b) * (self.block_size + self.extra) + self.extra + offset;
                self.file.read_exact_at(at, buf)
            }
        }
    }
}

impl ReadAt for Vdi {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        read_blocks(self, offset, buf)
    }

    fn size(&self) -> u64 {
        self.size
    }
}
