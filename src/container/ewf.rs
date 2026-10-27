//! E01 / Expert Witness Format (EnCase, FTK Imager, ewfacquire): forensic
//! disk images, split into segment files .E01, .E02, … .E99, .EAA, … Each
//! segment is a chain of sections; "volume" gives the geometry, "sectors"
//! holds the chunks (32 KiB of disk each, usually deflated) and "table"
//! says where each chunk starts. EWF2 (.Ex01) is a different format.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{BlockCache, Blocks, bad, inflate, read_blocks};
use crate::bytes::{le32, le64};
use crate::source::{DiskSource, ReadAt};

pub const SIGNATURE: &[u8] = b"EVF\x09\x0d\x0a\xff\x00";

#[derive(Clone, Copy)]
struct Chunk {
    segment: u16,
    offset: u64,
    /// Bytes stored, including an uncompressed chunk's checksum.
    len: u64,
    compressed: bool,
}

pub struct Ewf {
    segments: Vec<DiskSource>,
    size: u64,
    chunk_size: u64,
    chunks: Vec<Chunk>,
    cache: BlockCache,
}

/// The file name of segment `n` (1 = the .E01 itself).
fn segment_name(first: &Path, n: u32) -> Option<PathBuf> {
    let ext = first.extension()?.to_str()?;
    let lower = ext.starts_with(|c: char| c.is_ascii_lowercase());
    let letter = ext.chars().next()?.to_ascii_uppercase();
    let new = if n < 100 {
        format!("{letter}{n:02}")
    } else {
        let k = n - 100;
        let first = (letter as u8).checked_add((k / 676) as u8)?;
        if first > b'Z' {
            return None;
        }
        let (a, b) = ((k % 676) / 26, k % 26);
        format!("{}{}{}", first as char, (b'A' + a as u8) as char, (b'A' + b as u8) as char)
    };
    Some(first.with_extension(if lower { new.to_ascii_lowercase() } else { new }))
}

impl Ewf {
    pub fn open(first: &Path) -> Result<Self> {
        let mut segments = Vec::new();
        let mut chunks = Vec::new();
        let mut geometry: Option<(u64, u64, u64)> = None; // chunks, chunk size, disk size
        let mut done = false;
        for n in 1u32.. {
            let Some(path) = segment_name(first, n) else { break };
            let path = if n == 1 { first.to_path_buf() } else { path };
            if n > 1 && !path.exists() {
                break;
            }
            let f = DiskSource::open(&path.to_string_lossy())
                .with_context(|| format!("segment {} of the E01 image", path.display()))?;
            let mut head = [0u8; 13];
            f.read_exact_at(0, &mut head)?;
            anyhow::ensure!(head.starts_with(SIGNATURE), "{} is not an E01 segment", path.display());
            let segment = segments.len() as u16;
            done |= read_sections(&f, segment, &mut chunks, &mut geometry)?;
            segments.push(f);
            if done {
                break;
            }
        }
        if !done {
            log::warn!("{}: the last E01 segment is missing; the image is incomplete", first.display());
        }
        let Some((_, chunk_size, size)) = geometry else { bail!("damaged E01 image: no volume section") };
        anyhow::ensure!(chunk_size > 0, "damaged E01 volume section");
        Ok(Self { segments, size, chunk_size, chunks, cache: BlockCache::new(64) })
    }
}

/// Walks the sections of one segment. True at the "done" section.
fn read_sections(
    f: &DiskSource,
    segment: u16,
    chunks: &mut Vec<Chunk>,
    geometry: &mut Option<(u64, u64, u64)>,
) -> Result<bool> {
    let mut at = 13u64;
    let mut sectors_end = 0u64;
    for _ in 0..1_000_000 {
        let mut d = [0u8; 76];
        f.read_exact_at(at, &mut d).with_context(|| format!("E01 section at {at}"))?;
        let kind: String = d[..16].iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
        let next = le64(&d, 16).unwrap_or(0);
        let size = le64(&d, 24).unwrap_or(0);
        match kind.as_str() {
            "volume" | "disk" if geometry.is_none() => {
                let mut v = [0u8; 24];
                f.read_exact_at(at + 76, &mut v)?;
                let count = u64::from(le32(&v, 4).unwrap_or(0));
                let per_chunk = u64::from(le32(&v, 8).unwrap_or(0));
                let sector = u64::from(le32(&v, 12).unwrap_or(0));
                let sectors = le64(&v, 16).unwrap_or(0);
                *geometry = Some((count, per_chunk * sector, sectors * sector));
            }
            "sectors" => sectors_end = at + size,
            "table" => {
                let mut t = [0u8; 24];
                f.read_exact_at(at + 76, &mut t)?;
                let n = le32(&t, 0).unwrap_or(0) as usize;
                let base = le64(&t, 8).unwrap_or(0);
                let mut raw = vec![0u8; n * 4];
                f.read_exact_at(at + 76 + 24, &mut raw)?;
                let offsets: Vec<(u64, bool)> = raw
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|e| {
                        let v = u32::from_le_bytes([e[0], e[1], e[2], e[3]]);
                        (base + u64::from(v & 0x7FFF_FFFF), v & 0x8000_0000 != 0)
                    })
                    .collect();
                for (i, &(offset, compressed)) in offsets.iter().enumerate() {
                    let end = offsets.get(i + 1).map_or(sectors_end, |n| n.0);
                    chunks.push(Chunk { segment, offset, len: end.saturating_sub(offset), compressed });
                }
            }
            "done" => return Ok(true),
            "next" => return Ok(false),
            _ => {}
        }
        if next <= at {
            return Ok(false);
        }
        at = next;
    }
    bail!("damaged E01 segment: endless sections")
}

impl Blocks for Ewf {
    fn disk_size(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u64 {
        self.chunk_size
    }

    fn read_in_block(&self, index: u64, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let Some(c) = self.chunks.get(index as usize).copied() else {
            // Past the chunks that were acquired: unreadable.
            return Err(bad("this part of the disk is missing from the E01 image"));
        };
        let f = &self.segments[c.segment as usize];
        if !c.compressed {
            return f.read_exact_at(c.offset + offset, buf);
        }
        let chunk = self.cache.get(index, || {
            let mut data = vec![0u8; c.len as usize];
            f.read_exact_at(c.offset, &mut data)?;
            inflate(&data, true, self.chunk_size as usize)
        })?;
        let o = offset as usize;
        let got = chunk.get(o..o + buf.len()).ok_or_else(|| bad("damaged E01 chunk"))?;
        buf.copy_from_slice(got);
        Ok(())
    }
}

impl ReadAt for Ewf {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        read_blocks(self, offset, buf)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_names() {
        let p = Path::new("/x/case.E01");
        assert_eq!(segment_name(p, 2).unwrap(), Path::new("/x/case.E02"));
        assert_eq!(segment_name(p, 99).unwrap(), Path::new("/x/case.E99"));
        assert_eq!(segment_name(p, 100).unwrap(), Path::new("/x/case.EAA"));
        assert_eq!(segment_name(p, 101).unwrap(), Path::new("/x/case.EAB"));
        assert_eq!(segment_name(p, 100 + 676).unwrap(), Path::new("/x/case.FAA"));
        assert_eq!(segment_name(Path::new("a.e01"), 3).unwrap(), Path::new("a.e03"));
    }
}
