//! exFAT deleted-file recovery (SDXC cards, large USB drives, cameras).
//!
//! exFAT marks a deleted entry set by clearing bit 7 of each entry's type
//! (0x85 File -> 0x05, 0xC0 Stream -> 0x40, 0xC1 Name -> 0x41) and frees the
//! clusters in the allocation bitmap. The stream extension still holds the
//! first cluster, the exact length and the `NoFatChain` flag, which tells us
//! the file was contiguous — so recovery is often exact.

use std::collections::HashSet;

use anyhow::{Context, Result, ensure};

use super::{Condition, DeletedFile, Extent, FileData, FsKind, Volume, dos_datetime};
use crate::bytes::{le16, le32, le64, u8_at, utf16le};
use crate::ranges::{ByteRange, bit_is_set, zero_bit_runs};
use crate::source::{Source, read_tolerant};

const ENTRY_FILE: u8 = 0x85;
const ENTRY_STREAM: u8 = 0xC0;
const ENTRY_NAME: u8 = 0xC1;
const ENTRY_BITMAP: u8 = 0x81;
const IN_USE: u8 = 0x80;
const ATTR_DIRECTORY: u16 = 0x10;
const FLAG_NO_FAT_CHAIN: u8 = 0x02;
const MAX_DIR_BYTES: u64 = 64 << 20;

pub struct ExFat {
    src: Source,
    cluster: u64,
    heap_offset: u64,
    cluster_count: u32,
    root_cluster: u32,
    fat: Vec<u8>,
    bitmap: Vec<u8>,
}

/// Where a directory's (or file's) data is.
#[derive(Debug, Clone, Copy)]
struct Alloc {
    first_cluster: u32,
    len: u64,
    contiguous: bool,
}

struct EntrySet {
    name: String,
    attrs: u16,
    deleted: bool,
    alloc: Alloc,
    created: Option<chrono::NaiveDateTime>,
    modified: Option<chrono::NaiveDateTime>,
}

impl ExFat {
    pub fn open(src: Source) -> Result<Self> {
        let bs = src.read_vec(0, 512).context("reading exFAT boot sector")?;
        ensure!(&bs[3..11] == b"EXFAT   ", "not an exFAT boot sector");
        let fat_offset = u64::from(le32(&bs, 80).unwrap_or(0));
        let fat_len = u64::from(le32(&bs, 84).unwrap_or(0));
        let heap = u64::from(le32(&bs, 88).unwrap_or(0));
        let cluster_count = le32(&bs, 92).unwrap_or(0);
        let root_cluster = le32(&bs, 96).unwrap_or(0);
        let bps_shift = u32::from(u8_at(&bs, 108).unwrap_or(0));
        let spc_shift = u32::from(u8_at(&bs, 109).unwrap_or(0));
        ensure!((9..=12).contains(&bps_shift) && bps_shift + spc_shift <= 25, "invalid exFAT geometry");
        let bps = 1u64 << bps_shift;
        let cluster = bps << spc_shift;

        let fat_bytes = (u64::from(cluster_count) + 2) * 4;
        ensure!(fat_bytes <= fat_len * bps && fat_bytes <= 1 << 30, "invalid exFAT FAT size");
        let mut fat = vec![0u8; fat_bytes as usize];
        read_tolerant(src.as_ref(), fat_offset * bps, &mut fat);

        let mut v =
            ExFat { src, cluster, heap_offset: heap * bps, cluster_count, root_cluster, fat, bitmap: Vec::new() };
        ensure!(v.valid(root_cluster), "invalid exFAT root directory cluster");
        v.bitmap = v.load_bitmap().unwrap_or_else(|e| {
            log::warn!("exFAT allocation bitmap unreadable, overwrite detection disabled: {e:#}");
            Vec::new()
        });
        Ok(v)
    }

    fn valid(&self, cl: u32) -> bool {
        cl >= 2 && cl - 2 < self.cluster_count
    }

    fn cluster_offset(&self, cl: u32) -> u64 {
        self.heap_offset + u64::from(cl - 2) * self.cluster
    }

    fn fat_entry(&self, cl: u32) -> u32 {
        le32(&self.fat, cl as usize * 4).unwrap_or(0xFFFF_FFFF)
    }

    fn is_allocated(&self, cl: u32) -> Option<bool> {
        if self.bitmap.is_empty() {
            return None;
        }
        bit_is_set(&self.bitmap, u64::from(cl - 2))
    }

    /// Follows a FAT chain. Returns `None` if it is broken before `max`
    /// clusters (or `max` is `None` and it ends abnormally).
    fn chain(&self, start: u32, max: Option<u64>) -> Option<Vec<u32>> {
        let mut out = Vec::new();
        let mut cl = start;
        let limit = max.unwrap_or(u64::from(self.cluster_count)).min(u64::from(self.cluster_count));
        loop {
            if !self.valid(cl) || out.len() as u64 >= limit.max(1) {
                break;
            }
            out.push(cl);
            let next = self.fat_entry(cl);
            if next >= 0xFFFF_FFF7 {
                break;
            }
            if next == 0 {
                return None;
            }
            cl = next;
        }
        if let Some(m) = max
            && (out.len() as u64) < m
        {
            return None;
        }
        Some(out)
    }

    /// Extents for an allocation, honouring NoFatChain. Deleted chained
    /// files fall back to the contiguous assumption if the chain is gone.
    fn extents(&self, a: Alloc) -> (Vec<Extent>, bool) {
        let clusters = a.len.div_ceil(self.cluster);
        if !a.contiguous
            && let Some(chain) = self.chain(a.first_cluster, Some(clusters))
        {
            let mut ex: Vec<Extent> = Vec::new();
            for cl in chain {
                let off = self.cluster_offset(cl);
                match ex.last_mut() {
                    Some(last) if last.offset.map(|o| o + last.len) == Some(off) => last.len += self.cluster,
                    _ => ex.push(Extent { offset: Some(off), len: self.cluster }),
                }
            }
            return (ex, true);
        }
        let avail = (u64::from(self.cluster_count) + 2 - u64::from(a.first_cluster)).min(clusters);
        (vec![Extent { offset: Some(self.cluster_offset(a.first_cluster)), len: avail * self.cluster }], a.contiguous)
    }

    fn read_alloc(&self, a: Alloc) -> Vec<u8> {
        let (ex, _) = self.extents(a);
        let mut out = Vec::new();
        for e in ex {
            let at = out.len();
            out.resize(at + e.len as usize, 0);
            read_tolerant(self.src.as_ref(), e.offset.unwrap_or(0), &mut out[at..]);
        }
        out.truncate(a.len as usize);
        out
    }

    fn read_root(&self) -> Vec<u8> {
        let chain = self.chain(self.root_cluster, None).unwrap_or_else(|| vec![self.root_cluster]);
        let mut out = Vec::new();
        for cl in chain.into_iter().take((MAX_DIR_BYTES / self.cluster) as usize) {
            let at = out.len();
            out.resize(at + self.cluster as usize, 0);
            read_tolerant(self.src.as_ref(), self.cluster_offset(cl), &mut out[at..]);
        }
        out
    }

    fn load_bitmap(&self) -> Result<Vec<u8>> {
        let root = self.read_root();
        for e in root.as_chunks::<32>().0 {
            if e[0] == ENTRY_BITMAP && e[1] & 1 == 0 {
                let first = le32(e, 20).unwrap_or(0);
                let len = le64(e, 24).unwrap_or(0);
                ensure!(self.valid(first) && len >= u64::from(self.cluster_count).div_ceil(8), "bad bitmap entry");
                return Ok(self.read_alloc(Alloc { first_cluster: first, len, contiguous: false }));
            }
        }
        anyhow::bail!("allocation bitmap entry not found")
    }

    fn condition(&self, ex: &[Extent], size: u64) -> Condition {
        let mut needed = size.div_ceil(self.cluster);
        let (mut free, mut total) = (0u64, 0u64);
        for e in ex {
            let Some(off) = e.offset else { continue };
            let first = ((off - self.heap_offset) / self.cluster) as u32 + 2;
            let n = (e.len / self.cluster).min(needed);
            needed -= n;
            for i in 0..n as u32 {
                total += 1;
                match self.is_allocated(first + i) {
                    Some(false) => free += 1,
                    None => return Condition::Recoverable,
                    Some(true) => {}
                }
            }
        }
        if needed > 0 {
            total += needed; // ran off the end of the volume
        }
        Condition::from_counts(free, total)
    }
}

impl Volume for ExFat {
    fn kind(&self) -> FsKind {
        FsKind::ExFat
    }

    fn describe(&self) -> String {
        format!("exFAT, {} clusters, {} clusters total", crate::units::format_size(self.cluster), self.cluster_count)
    }

    fn source(&self) -> &Source {
        &self.src
    }

    fn cluster_size(&self) -> u64 {
        self.cluster
    }

    fn scan_files(&self, live: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<DeletedFile>> {
        let mut stack: Vec<(Option<Alloc>, String, bool)> = vec![(None, String::new(), false)];
        let mut visited = HashSet::new();
        let mut out = Vec::new();
        let mut done = 0u64;
        let mut id = 0u64;
        while let Some((alloc, dir_path, dir_deleted)) = stack.pop() {
            let bytes = match alloc {
                None => self.read_root(),
                Some(a) => self.read_alloc(Alloc { len: a.len.min(MAX_DIR_BYTES), ..a }),
            };
            for set in parse_dir(&bytes) {
                id += 1;
                let path = if dir_path.is_empty() { set.name.clone() } else { format!("{dir_path}/{}", set.name) };
                let deleted = set.deleted || dir_deleted;
                if set.attrs & ATTR_DIRECTORY != 0 {
                    if self.valid(set.alloc.first_cluster)
                        && set.alloc.len > 0
                        && visited.insert(set.alloc.first_cluster)
                    {
                        stack.push((Some(set.alloc), path, deleted));
                    }
                    continue;
                }
                if !deleted && !live {
                    continue;
                }
                let size = set.alloc.len;
                let mut note = None;
                let (data, condition) = if size == 0 {
                    (FileData::Resident(Vec::new()), Condition::Recoverable)
                } else if !self.valid(set.alloc.first_cluster) {
                    note = Some("start cluster lost".into());
                    (FileData::Lost, Condition::Overwritten)
                } else {
                    let (ex, exact) = self.extents(set.alloc);
                    if !exact {
                        note = Some("assumed contiguous".into());
                    }
                    // A file that still exists owns its clusters.
                    let c = if deleted { self.condition(&ex, size) } else { Condition::Recoverable };
                    (FileData::Extents(ex), c)
                };
                out.push(DeletedFile {
                    id,
                    path,
                    size,
                    created: set.created,
                    modified: set.modified,
                    condition,
                    note,
                    data,
                });
            }
            done += 1;
            progress(done, done + stack.len() as u64);
        }
        Ok(out)
    }

    fn free_ranges(&self) -> Result<Vec<ByteRange>> {
        ensure!(!self.bitmap.is_empty(), "allocation bitmap unavailable");
        Ok(zero_bit_runs(&self.bitmap, u64::from(self.cluster_count))
            .into_iter()
            .map(|r| self.heap_offset + r.start * self.cluster..self.heap_offset + r.end * self.cluster)
            .collect())
    }
}

fn exfat_time(ts: u32) -> Option<chrono::NaiveDateTime> {
    dos_datetime((ts >> 16) as u16, ts as u16)
}

/// Parses directory entry sets, live and deleted.
fn parse_dir(bytes: &[u8]) -> Vec<EntrySet> {
    let entries: Vec<&[u8; 32]> = bytes.as_chunks::<32>().0.iter().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let e = entries[i];
        let ty = e[0];
        if ty == 0x00 {
            break;
        }
        if ty & 0x7F != ENTRY_FILE & 0x7F {
            i += 1;
            continue;
        }
        let deleted = ty & IN_USE == 0;
        let secondary = usize::from(e[1]);
        if !(2..=18).contains(&secondary) || i + secondary >= entries.len() {
            i += 1;
            continue;
        }
        // Every secondary entry must have the same in-use state as the
        // primary; otherwise the set was partially overwritten.
        let want = |t: u8| if deleted { t & 0x7F } else { t };
        let stream = entries[i + 1];
        if stream[0] != want(ENTRY_STREAM) {
            i += 1;
            continue;
        }
        let name_len = usize::from(stream[3]);
        let mut units = Vec::with_capacity(name_len * 2);
        let mut ok = true;
        for k in 2..=secondary {
            let n = entries[i + k];
            if n[0] != want(ENTRY_NAME) {
                // Other secondary types (e.g. vendor extensions) are allowed
                // after the names; stop collecting there.
                ok = k > 2;
                break;
            }
            units.extend_from_slice(&n[2..32]);
        }
        if !ok || name_len == 0 {
            i += 1;
            continue;
        }
        units.truncate(name_len * 2);
        out.push(EntrySet {
            name: utf16le(&units),
            attrs: le16(e, 4).unwrap_or(0),
            deleted,
            alloc: Alloc {
                first_cluster: le32(stream, 20).unwrap_or(0),
                len: le64(stream, 24).unwrap_or(0),
                contiguous: stream[1] & FLAG_NO_FAT_CHAIN != 0,
            },
            created: exfat_time(le32(e, 8).unwrap_or(0)),
            modified: exfat_time(le32(e, 12).unwrap_or(0)),
        });
        i += 1 + secondary;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_set(name: &str, deleted: bool, cluster: u32, len: u64) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_entries = units.len().div_ceil(15);
        let clear = |t: u8| if deleted { t & 0x7F } else { t };
        let mut v = vec![0u8; 32 * (2 + name_entries)];
        v[0] = clear(ENTRY_FILE);
        v[1] = (1 + name_entries) as u8;
        v[32] = clear(ENTRY_STREAM);
        v[33] = FLAG_NO_FAT_CHAIN | 1;
        v[35] = units.len() as u8;
        v[32 + 20..32 + 24].copy_from_slice(&cluster.to_le_bytes());
        v[32 + 24..32 + 32].copy_from_slice(&len.to_le_bytes());
        for (k, chunk) in units.chunks(15).enumerate() {
            let base = 64 + k * 32;
            v[base] = clear(ENTRY_NAME);
            for (j, u) in chunk.iter().enumerate() {
                v[base + 2 + j * 2..base + 4 + j * 2].copy_from_slice(&u.to_le_bytes());
            }
        }
        v
    }

    #[test]
    fn parses_live_and_deleted_sets() {
        let mut dir = entry_set("keep.txt", false, 10, 5);
        dir.extend(entry_set("a rather long video file name.mp4", true, 20, 1 << 20));
        dir.extend([0u8; 32]);
        let sets = parse_dir(&dir);
        assert_eq!(sets.len(), 2);
        assert!(!sets[0].deleted);
        assert!(sets[1].deleted);
        assert_eq!(sets[1].name, "a rather long video file name.mp4");
        assert_eq!(sets[1].alloc.first_cluster, 20);
        assert!(sets[1].alloc.contiguous);
    }

    #[test]
    fn rejects_mixed_state_sets() {
        let mut dir = entry_set("x.jpg", true, 3, 100);
        dir[32] = ENTRY_STREAM; // stream entry reused by a live set
        assert!(parse_dir(&dir).is_empty());
    }
}
