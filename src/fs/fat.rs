//! FAT12 / FAT16 / FAT32 deleted-file recovery (USB sticks, SD cards,
//! camera media).
//!
//! Deleting a file on FAT overwrites the first byte of its directory entry
//! with `0xE5` and zeroes its cluster chain in the FAT. The entry keeps the
//! rest of the name, the start cluster and the size, so recovery reads
//! `ceil(size / cluster)` clusters starting there. Without the chain we
//! must assume the file was contiguous — true for the vast majority of
//! photos and videos written by cameras and phones.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};

use super::{Condition, DeletedFile, Extent, FileData, FsKind, Volume, dos_datetime};
use crate::bytes::{le16, le32, u8_at};
use crate::ranges::ByteRange;
use crate::source::{Source, read_tolerant};

const DELETED: u8 = 0xE5;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_LFN: u8 = 0x0F;
/// Upper bound on clusters read for a single directory (corruption guard).
const MAX_DIR_CLUSTERS: usize = 4096;
/// Deleted directories have no chain; read at most this many clusters.
const MAX_DELETED_DIR_CLUSTERS: usize = 256;

#[derive(Debug, Clone)]
struct Geometry {
    kind: FsKind,
    bytes_per_sector: u64,
    sectors_per_cluster: u64,
    reserved: u64,
    num_fats: u64,
    fat_sectors: u64,
    root_dir_sectors: u64,
    data_start: u64,
    cluster_count: u64,
    root_cluster: u32,
}

impl Geometry {
    fn parse(bs: &[u8]) -> Option<Self> {
        if bs.get(510..512)? != [0x55, 0xAA] || !matches!(bs[0], 0xEB | 0xE9) {
            return None;
        }
        let bps = u64::from(le16(bs, 11)?);
        let spc = u64::from(u8_at(bs, 13)?);
        let reserved = u64::from(le16(bs, 14)?);
        let num_fats = u64::from(u8_at(bs, 16)?);
        let root_entries = u64::from(le16(bs, 17)?);
        let total16 = u64::from(le16(bs, 19)?);
        let fat16 = u64::from(le16(bs, 22)?);
        let total32 = u64::from(le32(bs, 32)?);
        let fat32 = u64::from(le32(bs, 36)?);
        if !matches!(bps, 512 | 1024 | 2048 | 4096)
            || !spc.is_power_of_two()
            || reserved == 0
            || !(1..=4).contains(&num_fats)
        {
            return None;
        }
        let fat_sectors = if fat16 != 0 { fat16 } else { fat32 };
        let total = if total16 != 0 { total16 } else { total32 };
        let root_dir_sectors = (root_entries * 32).div_ceil(bps);
        let data_start = reserved + num_fats * fat_sectors + root_dir_sectors;
        if fat_sectors == 0 || total <= data_start {
            return None;
        }
        let cluster_count = (total - data_start) / spc;
        let kind = if cluster_count < 4085 {
            FsKind::Fat12
        } else if cluster_count < 65525 {
            FsKind::Fat16
        } else {
            FsKind::Fat32
        };
        if kind == FsKind::Fat32 && (root_entries != 0 || fat16 != 0) {
            return None;
        }
        // The FAT must be large enough to describe every cluster.
        let bits = match kind {
            FsKind::Fat12 => 12,
            FsKind::Fat16 => 16,
            _ => 32,
        };
        if fat_sectors * bps * 8 < (cluster_count + 2) * bits {
            return None;
        }
        Some(Geometry {
            kind,
            bytes_per_sector: bps,
            sectors_per_cluster: spc,
            reserved,
            num_fats,
            fat_sectors,
            root_dir_sectors,
            data_start,
            cluster_count,
            root_cluster: if kind == FsKind::Fat32 { le32(bs, 44)? } else { 0 },
        })
    }
}

pub fn detect_boot_sector(bs: &[u8]) -> Option<FsKind> {
    Geometry::parse(bs).map(|g| g.kind)
}

pub struct Fat {
    src: Source,
    g: Geometry,
    cluster: u64,
    fat: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum DirLoc {
    /// FAT12/16 fixed-size root directory region.
    FixedRoot,
    Chain(u32),
    Contiguous(u32),
}

/// A parsed 32-byte directory entry (plus its long name).
struct RawEntry {
    name: String,
    attr: u8,
    first_cluster: u32,
    size: u32,
    deleted: bool,
    created: Option<chrono::NaiveDateTime>,
    modified: Option<chrono::NaiveDateTime>,
}

impl Fat {
    pub fn open(src: Source) -> Result<Self> {
        let bs = src.read_vec(0, 512).context("reading FAT boot sector")?;
        let g = Geometry::parse(&bs).context("not a valid FAT boot sector")?;
        let cluster = g.bytes_per_sector * g.sectors_per_cluster;
        let fat_len = (g.fat_sectors * g.bytes_per_sector) as usize;
        ensure!(fat_len <= 1 << 30, "FAT too large");
        let mut fat = vec![0u8; fat_len];
        let bad = read_tolerant(src.as_ref(), g.reserved * g.bytes_per_sector, &mut fat);
        if bad > 0 {
            log::warn!("{bad} bytes of the FAT are unreadable");
        }
        Ok(Fat { src, g, cluster, fat })
    }

    fn entry(&self, cl: u32) -> u32 {
        let i = cl as usize;
        match self.g.kind {
            FsKind::Fat12 => {
                let v = le16(&self.fat, i + i / 2).unwrap_or(0xFFFF);
                u32::from(if cl & 1 == 1 { v >> 4 } else { v & 0x0FFF })
            }
            FsKind::Fat16 => u32::from(le16(&self.fat, i * 2).unwrap_or(0xFFFF)),
            _ => le32(&self.fat, i * 4).unwrap_or(0x0FFF_FFFF) & 0x0FFF_FFFF,
        }
    }

    fn end_of_chain(&self) -> u32 {
        match self.g.kind {
            FsKind::Fat12 => 0x0FF7,
            FsKind::Fat16 => 0xFFF7,
            _ => 0x0FFF_FFF7,
        }
    }

    fn valid(&self, cl: u32) -> bool {
        cl >= 2 && u64::from(cl) < self.g.cluster_count + 2
    }

    fn cluster_offset(&self, cl: u32) -> u64 {
        self.g.data_start * self.g.bytes_per_sector + u64::from(cl - 2) * self.cluster
    }

    fn chain(&self, start: u32, max: usize) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cl = start;
        let eoc = self.end_of_chain();
        while self.valid(cl) && out.len() < max {
            out.push(cl);
            let next = self.entry(cl);
            if next == 0 || next >= eoc {
                break;
            }
            cl = next;
        }
        out
    }

    fn read_dir(&self, loc: DirLoc) -> Vec<u8> {
        let src = self.src.as_ref();
        match loc {
            DirLoc::FixedRoot => {
                let off = (self.g.reserved + self.g.num_fats * self.g.fat_sectors) * self.g.bytes_per_sector;
                let mut b = vec![0u8; (self.g.root_dir_sectors * self.g.bytes_per_sector) as usize];
                read_tolerant(src, off, &mut b);
                b
            }
            DirLoc::Chain(start) => {
                let mut out = Vec::new();
                for cl in self.chain(start, MAX_DIR_CLUSTERS) {
                    let at = out.len();
                    out.resize(at + self.cluster as usize, 0);
                    read_tolerant(src, self.cluster_offset(cl), &mut out[at..]);
                }
                out
            }
            DirLoc::Contiguous(start) => {
                let mut out = Vec::new();
                let mut cl = start;
                while self.valid(cl) && out.len() < MAX_DELETED_DIR_CLUSTERS * self.cluster as usize {
                    let at = out.len();
                    out.resize(at + self.cluster as usize, 0);
                    read_tolerant(src, self.cluster_offset(cl), &mut out[at..]);
                    if out[at..].as_chunks::<32>().0.iter().any(|e| e[0] == 0) {
                        break; // end-of-directory marker
                    }
                    cl += 1;
                }
                out
            }
        }
    }

    /// On FAT32 Windows zeroes the high 16 bits of a deleted entry's start
    /// cluster. On large volumes we try each candidate `lo + k * 65536` and
    /// keep the first free one whose content matches the file's type.
    fn locate_start(&self, e: &RawEntry) -> u32 {
        let lo = e.first_cluster;
        if self.g.kind != FsKind::Fat32 || !e.deleted || lo > 0xFFFF || self.g.cluster_count + 2 <= 0x1_0000 {
            return lo;
        }
        let ext = e.name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase());
        let Some(ext) = ext else { return lo };
        let mut head = [0u8; 64];
        let mut cand = lo;
        while self.valid(cand) {
            if self.entry(cand) == 0 {
                read_tolerant(self.src.as_ref(), self.cluster_offset(cand), &mut head);
                if crate::carve::sniff_matches_ext(&head, &ext) {
                    return cand;
                }
            }
            cand = match cand.checked_add(0x1_0000) {
                Some(c) => c,
                None => break,
            };
        }
        lo
    }

    /// Assigns clusters to deleted files. FAT forgets cluster chains on
    /// delete, so we lay files out in start-cluster order and give each the
    /// next free clusters, skipping clusters that are in use or already
    /// assigned, and skipping over the whole extent of any other deleted
    /// file or directory we run into (the file was written around it). For
    /// unfragmented files this is the plain contiguous read; for files that
    /// filled a gap and continued elsewhere it rebuilds the original layout.
    fn assign_clusters(&self, mut pending: Vec<Pending>, dir_starts: &HashSet<u32>) -> Vec<DeletedFile> {
        const MAX_SKIP: u32 = 4096;
        // Start cluster -> clusters that file would occupy if contiguous.
        let mut starts: HashMap<u32, u32> = dir_starts.iter().map(|&c| (c, 1)).collect();
        for p in pending.iter().filter(|p| p.entry.size > 0) {
            let n = u64::from(p.entry.size).div_ceil(self.cluster).min(u64::from(MAX_SKIP)) as u32;
            let e = starts.entry(p.start).or_insert(0);
            *e = (*e).max(n);
        }
        pending.sort_by_key(|p| p.start);
        let mut claimed: HashSet<u32> = HashSet::new();
        let mut out = Vec::with_capacity(pending.len());
        for p in pending {
            let size = u64::from(p.entry.size);
            let n = size.div_ceil(self.cluster);
            let mut notes = Vec::new();
            if p.start != p.entry.first_cluster {
                notes.push("start cluster high word restored".to_string());
            }
            if p.in_deleted_dir && !p.entry.deleted {
                notes.push("parent directory deleted".to_string());
            }
            let (data, condition) = if size == 0 {
                (FileData::Resident(Vec::new()), Condition::Recoverable)
            } else if !self.valid(p.start) {
                notes.push("start cluster lost".into());
                (FileData::Lost, Condition::Overwritten)
            } else {
                let mut clusters = vec![p.start];
                let (mut cl, mut skipped) = (p.start + 1, 0u32);
                if self.entry(p.start) == 0 {
                    while (clusters.len() as u64) < n && self.valid(cl) && skipped <= MAX_SKIP {
                        if let Some(&other) = starts.get(&cl) {
                            skipped += other;
                            cl += other;
                        } else if self.entry(cl) == 0 && !claimed.contains(&cl) {
                            clusters.push(cl);
                            cl += 1;
                        } else {
                            skipped += 1;
                            cl += 1;
                        }
                    }
                }
                let condition = if (clusters.len() as u64) == n {
                    claimed.extend(&clusters);
                    if skipped > 0 {
                        notes.push(format!("fragmented: skipped {skipped} clusters used by other files (best guess)"));
                    } else if n > 1 {
                        notes.push("assumed contiguous".into());
                    }
                    Condition::Recoverable
                } else {
                    // First cluster in use or no room: fall back to a plain
                    // contiguous read and report how much is still free.
                    let avail = (self.g.cluster_count + 2 - u64::from(p.start)).min(n);
                    clusters = (0..avail as u32).map(|i| p.start + i).collect();
                    let free = clusters.iter().filter(|&&c| self.entry(c) == 0).count() as u64;
                    if n > 1 {
                        notes.push("assumed contiguous".into());
                    }
                    Condition::from_counts(free, n)
                };
                (FileData::Extents(self.cluster_extents(&clusters)), condition)
            };
            out.push(DeletedFile {
                id: p.id,
                path: p.path,
                size,
                created: p.entry.created,
                modified: p.entry.modified,
                condition,
                note: (!notes.is_empty()).then(|| notes.join("; ")),
                data,
            });
        }
        out
    }

    /// A file that still exists: its clusters are those of its FAT chain.
    fn existing_file(&self, id: u64, path: String, e: &RawEntry) -> DeletedFile {
        let size = u64::from(e.size);
        let data = if size == 0 {
            FileData::Resident(Vec::new())
        } else {
            let max = size.div_ceil(self.cluster) as usize;
            FileData::Extents(self.cluster_extents(&self.chain(e.first_cluster, max)))
        };
        DeletedFile {
            id,
            path,
            size,
            created: e.created,
            modified: e.modified,
            condition: Condition::Recoverable,
            note: None,
            data,
        }
    }

    fn cluster_extents(&self, clusters: &[u32]) -> Vec<Extent> {
        let mut ex: Vec<Extent> = Vec::new();
        for &c in clusters {
            let off = self.cluster_offset(c);
            match ex.last_mut() {
                Some(last) if last.offset.map(|o| o + last.len) == Some(off) => last.len += self.cluster,
                _ => ex.push(Extent { offset: Some(off), len: self.cluster }),
            }
        }
        ex
    }
}

/// A deleted file found while walking directories, before clusters are
/// assigned to it.
struct Pending {
    id: u64,
    path: String,
    entry: RawEntry,
    start: u32,
    in_deleted_dir: bool,
}

/// A directory waiting to be read.
struct DirTask {
    loc: DirLoc,
    path: String,
    deleted: bool,
    /// First cluster of the parent directory (0 for the root), which a
    /// genuine subdirectory's ".." entry must point back to.
    parent: u32,
    own: u32,
}

impl Volume for Fat {
    fn kind(&self) -> FsKind {
        self.g.kind
    }

    fn describe(&self) -> String {
        format!(
            "{}, {} clusters, {} clusters total",
            self.g.kind,
            crate::units::format_size(self.cluster),
            self.g.cluster_count
        )
    }

    fn source(&self) -> &Source {
        &self.src
    }

    fn cluster_size(&self) -> u64 {
        self.cluster
    }

    fn scan_files(&self, live: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<DeletedFile>> {
        let mut existing = Vec::new();
        let root = if self.g.kind == FsKind::Fat32 { DirLoc::Chain(self.g.root_cluster) } else { DirLoc::FixedRoot };
        let mut stack = vec![DirTask { loc: root, path: String::new(), deleted: false, parent: 0, own: 0 }];
        let mut visited = HashSet::new();
        let mut pending = Vec::new();
        let mut dir_starts = HashSet::new();
        let mut dirs_done = 0u64;
        let mut id = 0u64;
        while let Some(task) = stack.pop() {
            let bytes = self.read_dir(task.loc);
            if let DirLoc::Contiguous(cl) = task.loc {
                // A deleted directory owns its cluster only if the cluster
                // still starts with "." / ".." and ".." points back to the
                // parent; otherwise it was reused (possibly by another
                // deleted directory) and its "entries" would be noise.
                if !looks_like_subdir(&bytes) || dotdot_cluster(&bytes) != task.parent {
                    continue;
                }
                dir_starts.insert(cl);
            }
            if task.own != 0 && !visited.insert(task.own) {
                continue;
            }
            for e in parse_dir(&bytes) {
                id += 1;
                if e.name == "." || e.name == ".." {
                    continue;
                }
                let path = if task.path.is_empty() { e.name.clone() } else { format!("{}/{}", task.path, e.name) };
                let deleted = e.deleted || task.deleted;
                if e.attr & ATTR_DIRECTORY != 0 {
                    if self.valid(e.first_cluster) && !visited.contains(&e.first_cluster) {
                        let loc =
                            if deleted { DirLoc::Contiguous(e.first_cluster) } else { DirLoc::Chain(e.first_cluster) };
                        stack.push(DirTask { loc, path, deleted, parent: task.own, own: e.first_cluster });
                    }
                } else if deleted {
                    let start = self.locate_start(&e);
                    pending.push(Pending { id, path, entry: e, start, in_deleted_dir: task.deleted });
                } else if live {
                    existing.push(self.existing_file(id, path, &e));
                }
            }
            dirs_done += 1;
            progress(dirs_done, dirs_done + stack.len() as u64);
        }
        let mut out = self.assign_clusters(pending, &dir_starts);
        out.extend(existing);
        Ok(out)
    }

    fn free_ranges(&self) -> Result<Vec<ByteRange>> {
        let mut out = Vec::new();
        let mut run: Option<u32> = None;
        let end = (self.g.cluster_count + 2) as u32;
        for cl in 2..end {
            if self.entry(cl) == 0 {
                run.get_or_insert(cl);
            } else if let Some(s) = run.take() {
                out.push(self.cluster_offset(s)..self.cluster_offset(cl));
            }
        }
        if let Some(s) = run {
            out.push(self.cluster_offset(s)..self.cluster_offset(s) + u64::from(end - s) * self.cluster);
        }
        Ok(out)
    }
}

fn looks_like_subdir(b: &[u8]) -> bool {
    b.len() >= 64 && &b[0..11] == b".          " && &b[32..43] == b"..         "
}

/// Start cluster recorded in a directory's ".." entry (0 = root).
fn dotdot_cluster(b: &[u8]) -> u32 {
    let hi = u32::from(le16(b, 32 + 20).unwrap_or(0));
    let lo = u32::from(le16(b, 32 + 26).unwrap_or(0));
    (hi << 16) | lo
}

pub(crate) fn sfn_checksum(name: &[u8]) -> u8 {
    name.iter().take(11).fold(0u8, |s, &c| ((s & 1) << 7).wrapping_add(s >> 1).wrapping_add(c))
}

/// 8.3 name -> "NAME.EXT", honouring the NT lowercase flags.
fn short_name(raw: &[u8; 11], nt_flags: u8) -> String {
    let conv = |b: &[u8], lower: bool| -> String {
        let s: String = b.iter().map(|&c| char::from(c)).collect::<String>().trim_end().to_string();
        if lower { s.to_lowercase() } else { s }
    };
    let base = conv(&raw[..8], nt_flags & 0x08 != 0);
    let ext = conv(&raw[8..], nt_flags & 0x10 != 0);
    if ext.is_empty() { base } else { format!("{base}.{ext}") }
}

fn lfn_units(e: &[u8]) -> impl Iterator<Item = u16> + '_ {
    [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].into_iter().map(move |o| u16::from_le_bytes([e[o], e[o + 1]]))
}

fn parse_dir(bytes: &[u8]) -> Vec<RawEntry> {
    let mut out = Vec::new();
    // LFN entries seen since the last short entry, in disk order.
    let mut lfn: Vec<&[u8]> = Vec::new();
    for e in bytes.as_chunks::<32>().0 {
        let first = e[0];
        if first == 0x00 {
            break;
        }
        let attr = e[11];
        if attr & 0x3F == ATTR_LFN {
            lfn.push(e);
            continue;
        }
        if attr & ATTR_VOLUME_ID != 0 {
            lfn.clear();
            continue;
        }
        let deleted = first == DELETED;
        let mut raw: [u8; 11] = e[0..11].try_into().unwrap_or([b' '; 11]);
        if first == 0x05 {
            raw[0] = DELETED; // 0x05 escapes a real leading 0xE5 (KANJI)
        }
        let long = take_long_name(&mut lfn, &mut raw, deleted);
        if deleted && long.is_none() {
            raw[0] = b'_';
        }
        let name = long.unwrap_or_else(|| short_name(&raw, e[12]));
        let hi = u32::from(le16(e, 20).unwrap_or(0));
        let lo = u32::from(le16(e, 26).unwrap_or(0));
        out.push(RawEntry {
            name,
            attr,
            first_cluster: (hi << 16) | lo,
            size: le32(e, 28).unwrap_or(0),
            deleted,
            created: dos_datetime(le16(e, 16).unwrap_or(0), le16(e, 14).unwrap_or(0)),
            modified: dos_datetime(le16(e, 24).unwrap_or(0), le16(e, 22).unwrap_or(0)),
        });
    }
    out
}

/// Assembles the pending LFN entries into a long name if they belong to the
/// short entry `raw`. For deleted entries the first byte of `raw` was
/// overwritten with 0xE5; we recover it by finding the byte value that
/// makes the short-name checksum match the LFN checksum.
fn take_long_name(lfn: &mut Vec<&[u8]>, raw: &mut [u8; 11], deleted: bool) -> Option<String> {
    if lfn.is_empty() {
        return None;
    }
    let parts = std::mem::take(lfn);
    let checksum = parts[0][13];
    if parts.iter().any(|p| p[13] != checksum) {
        return None;
    }
    let units: Vec<u16> =
        parts.iter().rev().flat_map(|p| lfn_units(p)).take_while(|&u| u != 0x0000 && u != 0xFFFF).collect();
    let name = String::from_utf16_lossy(&units);
    if name.is_empty() {
        return None;
    }
    if !deleted {
        return (sfn_checksum(raw) == checksum).then_some(name);
    }
    // Most likely first byte: the long name's own first letter, uppercased.
    let guess = name.chars().next().filter(char::is_ascii).map(|c| c.to_ascii_uppercase() as u8);
    let candidates = guess.into_iter().chain(0x20..=0xFFu8);
    for c in candidates {
        raw[0] = c;
        if sfn_checksum(raw) == checksum {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lfn_entry(ord: u8, chars: &[u16], checksum: u8) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[0] = ord;
        e[11] = ATTR_LFN;
        e[13] = checksum;
        let mut padded = chars.to_vec();
        if padded.len() < 13 {
            padded.push(0);
        }
        padded.resize(13, 0xFFFF);
        for (k, o) in [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].into_iter().enumerate() {
            e[o..o + 2].copy_from_slice(&padded[k].to_le_bytes());
        }
        e
    }

    #[test]
    fn recovers_deleted_long_name_and_first_char() {
        let sfn = *b"HOLIDA~1JPG";
        let chk = sfn_checksum(&sfn);
        let name: Vec<u16> = "holiday photo 2024.jpg".encode_utf16().collect();
        let mut dir = Vec::new();
        // Two LFN entries in disk order: part 2 (last) first, then part 1.
        dir.extend_from_slice(&lfn_entry(DELETED, &name[13..], chk));
        dir.extend_from_slice(&lfn_entry(DELETED, &name[..13], chk));
        let mut short = [0u8; 32];
        short[..11].copy_from_slice(&sfn);
        short[0] = DELETED;
        short[11] = 0x20;
        short[26..28].copy_from_slice(&5u16.to_le_bytes());
        short[28..32].copy_from_slice(&12345u32.to_le_bytes());
        dir.extend_from_slice(&short);
        dir.extend_from_slice(&[0u8; 32]);

        let entries = parse_dir(&dir);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].deleted);
        assert_eq!(entries[0].name, "holiday photo 2024.jpg");
        assert_eq!(entries[0].first_cluster, 5);
        assert_eq!(entries[0].size, 12345);
    }

    #[test]
    fn deleted_short_name_without_lfn() {
        let mut short = [0u8; 32];
        short[..11].copy_from_slice(b"\xE5EPORT  TXT");
        short[11] = 0x20;
        let entries = parse_dir(&short);
        assert_eq!(entries[0].name, "_EPORT.TXT");
    }

    #[test]
    fn checksum_is_rotate_right_add() {
        let expected = b"FILENAMEEXT".iter().fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c));
        assert_eq!(sfn_checksum(b"FILENAMEEXT"), expected);
    }
}
