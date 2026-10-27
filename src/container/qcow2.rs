//! QCOW2 (QEMU, KVM, Proxmox, libvirt). Big-endian. A two-level table (L1
//! → L2) maps each cluster (64 KiB usually) to its place in the file;
//! clusters may be compressed (deflate, or zstd), all-zero, or absent —
//! then they come from the backing file, if there is one.

use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{BlockCache, Blocks, bad, inflate, read_blocks};
use crate::bytes::{be32, be64};
use crate::source::{DiskSource, ReadAt, Source};

const OFFSET_MASK: u64 = 0x00FF_FFFF_FFFF_FE00;
const COMPRESSED: u64 = 1 << 62;
const ZERO: u64 = 1;

pub struct Qcow2 {
    file: DiskSource,
    size: u64,
    cluster_bits: u32,
    l1: Vec<u64>,
    zstd: bool,
    backing: Option<Source>,
    tables: BlockCache,
    clusters: BlockCache,
}

impl Qcow2 {
    pub fn open(file: DiskSource) -> Result<Self> {
        let mut h = [0u8; 112];
        file.read_exact_at(0, &mut h)?;
        let version = be32(&h, 4).unwrap_or(0);
        anyhow::ensure!(version == 2 || version == 3, "unsupported QCOW version {version}");
        let cluster_bits = be32(&h, 20).unwrap_or(0);
        anyhow::ensure!((9..=21).contains(&cluster_bits), "damaged QCOW2 header");
        let size = be64(&h, 24).unwrap_or(0);
        if be32(&h, 32).unwrap_or(0) != 0 {
            bail!("this QCOW2 image is encrypted");
        }
        let l1_size = be32(&h, 36).unwrap_or(0) as usize;
        let l1_at = be64(&h, 40).unwrap_or(0);
        let mut zstd = false;
        if version == 3 {
            let incompatible = be64(&h, 72).unwrap_or(0);
            if incompatible & (1 << 2) != 0 {
                bail!("this QCOW2 image keeps its data in a separate file");
            }
            if incompatible & (1 << 4) != 0 {
                bail!("QCOW2 images with subclusters are not supported");
            }
            let header_len = be32(&h, 100).unwrap_or(0);
            zstd = incompatible & (1 << 3) != 0 && header_len > 104 && h[104] == 1;
        }
        let mut raw = vec![0u8; l1_size * 8];
        file.read_exact_at(l1_at, &mut raw).context("reading the QCOW2 L1 table")?;
        let l1 = raw.as_chunks::<8>().0.iter().map(|e| u64::from_be_bytes(*e)).collect();
        let backing = match (be64(&h, 8).unwrap_or(0), be32(&h, 16).unwrap_or(0)) {
            (0, _) | (_, 0) => None,
            (at, len) => {
                let mut name = vec![0u8; len.min(1023) as usize];
                file.read_exact_at(at, &mut name)?;
                let name = String::from_utf8_lossy(&name).to_string();
                let path = file.path().to_string();
                let f = super::sibling(Path::new(&path), &name)
                    .context("this QCOW2 image is based on another one, which is needed")?;
                Some(super::open(f.path())?.0)
            }
        };
        Ok(Self {
            file,
            size,
            cluster_bits,
            l1,
            zstd,
            backing,
            tables: BlockCache::new(16),
            clusters: BlockCache::new(32),
        })
    }

    fn l2_entry(&self, cluster: u64) -> io::Result<u64> {
        let per_table = 1u64 << (self.cluster_bits - 3);
        let t = cluster / per_table;
        let Some(&l1) = self.l1.get(t as usize) else { return Ok(0) };
        let at = l1 & OFFSET_MASK;
        if at == 0 {
            return Ok(0);
        }
        let table = self.tables.get(t, || {
            let mut raw = vec![0u8; 1 << self.cluster_bits];
            self.file.read_exact_at(at, &mut raw)?;
            Ok(raw)
        })?;
        Ok(be64(&table, ((cluster % per_table) * 8) as usize).unwrap_or(0))
    }
}

impl Blocks for Qcow2 {
    fn disk_size(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u64 {
        1 << self.cluster_bits
    }

    fn read_in_block(&self, index: u64, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let e = self.l2_entry(index)?;
        let cs = 1usize << self.cluster_bits;
        if e & COMPRESSED != 0 {
            let x = 62 - (self.cluster_bits - 8);
            let host = e & ((1 << x) - 1);
            let sectors = ((e >> x) & ((1 << (self.cluster_bits - 8)) - 1)) + 1;
            let len = (sectors * 512 - (host & 511)) as usize;
            let cluster = self.clusters.get(index, || {
                let mut data = vec![0u8; len];
                // The last compressed cluster may end before its last sector.
                let n = self.file.read_at(host, &mut data)?;
                data.truncate(n);
                let mut out = if self.zstd {
                    let mut d = ruzstd::decoding::StreamingDecoder::new(&data[..]).map_err(|e| bad(e.to_string()))?;
                    let mut v = Vec::with_capacity(cs);
                    io::Read::read_to_end(&mut io::Read::take(&mut d, cs as u64), &mut v)?;
                    v
                } else {
                    inflate(&data, false, cs)?
                };
                out.resize(cs, 0);
                Ok(out)
            })?;
            let o = offset as usize;
            buf.copy_from_slice(&cluster[o..o + buf.len()]);
            return Ok(());
        }
        let host = e & OFFSET_MASK;
        if e & ZERO != 0 || (host == 0 && self.backing.is_none()) {
            buf.fill(0);
            return Ok(());
        }
        if host == 0 {
            return self.backing.as_ref().map_or(Ok(()), |b| {
                // The backing disk may be smaller: the rest reads as zeros.
                let pos = (index << self.cluster_bits) + offset;
                let n = b.read_at(pos, buf).unwrap_or(0);
                buf[n..].fill(0);
                Ok(())
            });
        }
        self.file.read_exact_at(host + offset, buf)
    }
}

impl ReadAt for Qcow2 {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        read_blocks(self, offset, buf)
    }

    fn size(&self) -> u64 {
        self.size
    }
}
