//! HFS+ and HFSX (Mac OS Extended, before APFS; still common on external
//! drives and older Time Machine disks).
//!
//! Every file and folder is a record in the catalog B-tree, with its name,
//! parent folder and data location. Deleting a file removes its record
//! from the tree, but older copies stay behind: in catalog nodes that were
//! freed, and in the journal, which keeps copies of the catalog nodes it
//! changed. Every catalog node found there is parsed; file records that the
//! live tree no longer has are the deleted files.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};

use super::{Condition, DeletedFile, Extent, FileData, FsKind, Volume};
use crate::bytes::{be16, be32, be64};
use crate::ranges::{ByteRange, zero_bit_runs};
use crate::source::{Source, read_tolerant};

const HEADER: u64 = 1024;
const ROOT_FOLDER: u32 = 2;
const KIND_LEAF: u8 = 0xFF;
const FOLDER: u16 = 1;
const FILE: u16 = 2;
/// Largest catalog or journal read into memory.
const MAX_META: u64 = 1 << 30;

/// Whether the 512 bytes at offset 1024 are an HFS+/HFSX volume header.
pub fn detect(h: &[u8]) -> bool {
    matches!(h.get(..2), Some(b"H+" | b"HX"))
        && be16(h, 2).is_some_and(|v| v == 4 || v == 5)
        && be32(h, 40).is_some_and(|b| b.is_power_of_two() && (512..=1 << 20).contains(&b))
}

/// A fork: its size and where its blocks are.
#[derive(Debug, Clone, Default)]
struct Fork {
    size: u64,
    extents: Vec<(u32, u32)>,
}

impl Fork {
    fn parse(b: &[u8]) -> Self {
        let extents =
            (0..8).filter_map(|i| Some((be32(b, 16 + i * 8)?, be32(b, 20 + i * 8)?))).filter(|e| e.1 > 0).collect();
        Fork { size: be64(b, 0).unwrap_or(0), extents }
    }
}

/// A catalog record of a file or folder.
#[derive(Debug, Clone)]
struct Record {
    parent: u32,
    name: String,
    id: u32,
    folder: bool,
    created: u32,
    modified: u32,
    data: Fork,
}

pub struct HfsPlus {
    src: Source,
    block: u64,
    blocks: u64,
    catalog: Fork,
    extents_file: Fork,
    allocation: Fork,
    journal: Option<(u64, u64)>,
}

impl HfsPlus {
    pub fn open(src: Source) -> Result<Self> {
        let h = src.read_vec(HEADER, 512).context("reading the HFS+ volume header")?;
        ensure!(detect(&h), "not an HFS+ volume header");
        let block = u64::from(be32(&h, 40).unwrap_or(4096));
        let blocks = u64::from(be32(&h, 44).unwrap_or(0));
        let mut vol = HfsPlus {
            block,
            blocks,
            catalog: Fork::parse(&h[272..352]),
            extents_file: Fork::parse(&h[192..272]),
            allocation: Fork::parse(&h[112..192]),
            journal: None,
            src,
        };
        // Journaled (attribute bit 13): the journal info block says where.
        if be32(&h, 4).unwrap_or(0) & (1 << 13) != 0 {
            let info = be32(&h, 12).unwrap_or(0);
            if let Ok(j) = vol.src.read_vec(u64::from(info) * block, 52) {
                let (off, size) = (be64(&j, 36).unwrap_or(0), be64(&j, 44).unwrap_or(0));
                if size > 0 && off + size <= blocks * block {
                    vol.journal = Some((off, size.min(MAX_META)));
                }
            }
        }
        ensure!(!vol.catalog.extents.is_empty(), "HFS+ catalog file missing");
        Ok(vol)
    }

    fn extents_of(&self, fork: &Fork) -> Vec<Extent> {
        fork.extents
            .iter()
            .filter(|e| u64::from(e.0) + u64::from(e.1) <= self.blocks)
            .map(|e| Extent { offset: Some(u64::from(e.0) * self.block), len: u64::from(e.1) * self.block })
            .collect()
    }

    /// The whole content of a fork (special files), up to `MAX_META`.
    fn read_fork(&self, fork: &Fork) -> Vec<u8> {
        let mut out = Vec::new();
        for e in self.extents_of(fork) {
            if out.len() as u64 >= fork.size.min(MAX_META) {
                break;
            }
            let start = out.len();
            out.resize(start + e.len as usize, 0);
            read_tolerant(self.src.as_ref(), e.offset.unwrap_or(0), &mut out[start..]);
        }
        out.truncate(fork.size.min(MAX_META) as usize);
        out
    }

    /// The catalog file, with the extents its fork record cannot hold
    /// (from the extents overflow file).
    fn catalog_bytes(&self, overflow: &HashMap<u32, Vec<(u32, u32)>>) -> Vec<u8> {
        let mut fork = self.catalog.clone();
        if let Some(more) = overflow.get(&4) {
            fork.extents.extend(more);
        }
        self.read_fork(&fork)
    }

    /// Extents beyond the first eight, per file ID (data forks only).
    fn overflow(&self) -> HashMap<u32, Vec<(u32, u32)>> {
        let mut out: HashMap<u32, Vec<(u32, u32, u32)>> = HashMap::new();
        let file = self.read_fork(&self.extents_file);
        let Some(node_size) = be16(&file, 32).map(usize::from).filter(|n| n.is_power_of_two() && *n >= 512) else {
            return HashMap::new();
        };
        for node in file.chunks_exact(node_size) {
            for rec in leaf_records(node) {
                // Key: length, fork type, pad, file ID, start block.
                let (Some(klen), Some(&fork), Some(id), Some(start)) =
                    (be16(rec, 0), rec.get(2), be32(rec, 4), be32(rec, 8))
                else {
                    continue;
                };
                if fork != 0 {
                    continue;
                }
                let data = &rec[(2 + usize::from(klen)).min(rec.len())..];
                for i in 0..8 {
                    if let (Some(s), Some(n)) = (be32(data, i * 8), be32(data, i * 8 + 4))
                        && n > 0
                    {
                        out.entry(id).or_default().push((start, s, n));
                    }
                }
            }
        }
        out.into_iter()
            .map(|(id, mut v)| {
                v.sort_unstable();
                v.dedup();
                (id, v.into_iter().map(|(_, s, n)| (s, n)).collect())
            })
            .collect()
    }

    fn bitmap(&self) -> Vec<u8> {
        self.read_fork(&self.allocation)
    }

    fn condition(&self, extents: &[Extent], size: u64, bitmap: &[u8]) -> Condition {
        let (mut free, mut total, mut left) = (0u64, 0u64, size.div_ceil(self.block));
        for e in extents {
            let Some(off) = e.offset else { continue };
            let n = (e.len / self.block).min(left);
            left -= n;
            for b in off / self.block..off / self.block + n {
                total += 1;
                // Most significant bit first.
                if bitmap.get((b / 8) as usize).is_some_and(|&x| x & (0x80 >> (b % 8)) == 0) {
                    free += 1;
                }
            }
        }
        Condition::from_counts(free, total)
    }
}

/// The records of a catalog (or extents) leaf node, or none if `node` is
/// not one.
fn leaf_records(node: &[u8]) -> Vec<&[u8]> {
    let size = node.len();
    let (Some(&kind), Some(&height), Some(count)) = (node.get(8), node.get(9), be16(node, 10)) else {
        return Vec::new();
    };
    let count = usize::from(count);
    if kind != KIND_LEAF || height != 1 || count == 0 || count * 2 + 14 > size {
        return Vec::new();
    }
    // Record offsets are stored backwards from the end; one more marks the
    // start of the free space.
    let offset = |i: usize| be16(node, size - 2 * (i + 1)).map(usize::from);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let (Some(a), Some(b)) = (offset(i), offset(i + 1)) else { return Vec::new() };
        if a < 14 || b <= a || b > size - 2 * (count + 1) || (i == 0 && a != 14) {
            return Vec::new();
        }
        out.push(&node[a..b]);
    }
    out
}

/// A file or folder record (thread records are skipped).
fn parse_record(rec: &[u8]) -> Option<Record> {
    let klen = usize::from(be16(rec, 0)?);
    let parent = be32(rec, 2)?;
    let nlen = usize::from(be16(rec, 6)?);
    if 6 + nlen * 2 > klen || nlen == 0 {
        return None;
    }
    let units: Vec<u16> = (0..nlen).map(|i| be16(rec, 8 + i * 2)).collect::<Option<_>>()?;
    // A "/" in a name (allowed on a Mac) would split the path: shown as
    // ":", as the Terminal does.
    let name = String::from_utf16(&units).ok()?.replace('/', ":");
    let d = rec.get(2 + klen..)?;
    let ty = be16(d, 0)?;
    match ty {
        FOLDER => Some(Record {
            parent,
            name,
            id: be32(d, 8)?,
            folder: true,
            created: be32(d, 12)?,
            modified: be32(d, 16)?,
            data: Fork::default(),
        }),
        FILE => Some(Record {
            parent,
            name,
            id: be32(d, 8)?,
            folder: false,
            created: be32(d, 12)?,
            modified: be32(d, 16)?,
            data: Fork::parse(d.get(88..168)?),
        }),
        _ => None,
    }
}

fn hfs_time(t: u32) -> Option<chrono::NaiveDateTime> {
    // Seconds since 1904-01-01.
    let unix = i64::from(t) - 2_082_844_800;
    (t != 0).then(|| chrono::DateTime::from_timestamp(unix, 0)).flatten().map(|d| d.naive_utc())
}

impl Volume for HfsPlus {
    fn kind(&self) -> FsKind {
        FsKind::HfsPlus
    }

    fn describe(&self) -> String {
        format!(
            "Mac OS Extended, {} blocks{}",
            crate::units::format_size(self.block),
            if self.journal.is_some() { ", journaled" } else { "" }
        )
    }

    fn source(&self) -> &Source {
        &self.src
    }

    fn cluster_size(&self) -> u64 {
        self.block
    }

    fn scan_files(&self, live: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<DeletedFile>> {
        let overflow = self.overflow();
        let catalog = self.catalog_bytes(&overflow);
        let node_size = be16(&catalog, 32).map(usize::from).filter(|n| n.is_power_of_two() && *n >= 512);
        let node_size = node_size.context("unreadable HFS+ catalog header")?;
        let first_leaf = be32(&catalog, 24).unwrap_or(0) as usize;
        let nodes = catalog.len() / node_size;

        // The live tree: the chain of leaf nodes.
        let mut live_nodes = HashSet::new();
        let mut n = first_leaf;
        while n != 0 && n < nodes && live_nodes.insert(n) {
            n = be32(&catalog[n * node_size..], 0).unwrap_or(0) as usize;
        }
        let mut current: Vec<Record> = Vec::new();
        let mut old: Vec<Record> = Vec::new();
        for (i, node) in catalog.chunks_exact(node_size).enumerate() {
            let records = leaf_records(node).into_iter().filter_map(parse_record);
            if live_nodes.contains(&i) {
                current.extend(records);
            } else {
                old.extend(records);
            }
            progress(i as u64 + 1, nodes as u64 * 2);
        }
        // Older copies of catalog nodes in the journal, anywhere in it.
        if let Some((off, size)) = self.journal {
            let mut j = vec![0u8; size as usize];
            read_tolerant(self.src.as_ref(), off, &mut j);
            let mut p = 0;
            while p + node_size <= j.len() {
                let found: Vec<Record> =
                    leaf_records(&j[p..p + node_size]).into_iter().filter_map(parse_record).collect();
                if found.is_empty() {
                    p += 512;
                } else {
                    old.extend(found);
                    p += node_size;
                }
            }
        }

        // Folders, live and old, for paths.
        let mut folders: HashMap<u32, (u32, String)> = HashMap::new();
        for r in old.iter().chain(&current).filter(|r| r.folder) {
            folders.insert(r.id, (r.parent, r.name.clone()));
        }
        let path = |r: &Record| {
            let mut parts = vec![r.name.clone()];
            let mut at = r.parent;
            for _ in 0..256 {
                if at == ROOT_FOLDER {
                    break;
                }
                match folders.get(&at) {
                    Some((parent, name)) => {
                        parts.push(name.clone());
                        at = *parent;
                    }
                    None => {
                        parts.push("$Orphan".into());
                        break;
                    }
                }
            }
            parts.reverse();
            parts.join("/")
        };

        let bitmap = self.bitmap();
        let existing: HashSet<u32> = current.iter().filter(|r| !r.folder).map(|r| r.id).collect();
        let mut out = Vec::new();
        if live {
            for r in current.iter().filter(|r| !r.folder) {
                let mut fork = r.data.clone();
                if let Some(more) = overflow.get(&r.id) {
                    fork.extents.extend(more);
                }
                out.push(DeletedFile {
                    id: u64::from(r.id),
                    path: path(r),
                    size: fork.size,
                    created: hfs_time(r.created),
                    modified: hfs_time(r.modified),
                    condition: Condition::Recoverable,
                    note: None,
                    data: FileData::Extents(self.extents_of(&fork)),
                });
            }
        }
        let mut seen = HashSet::new();
        for r in old.iter().filter(|r| !r.folder && !existing.contains(&r.id)) {
            // A file can have several old copies: keep the one with data.
            if r.data.extents.is_empty() && r.data.size > 0 || !seen.insert(r.id) {
                continue;
            }
            let extents = self.extents_of(&r.data);
            let stored: u64 = extents.iter().map(|e| e.len).sum();
            let note = (stored < r.data.size).then(|| "only the first part's location is known".to_string());
            let condition = self.condition(&extents, r.data.size, &bitmap);
            out.push(DeletedFile {
                id: u64::from(r.id),
                path: path(r),
                size: r.data.size,
                created: hfs_time(r.created),
                modified: hfs_time(r.modified),
                condition,
                note,
                data: if r.data.size == 0 { FileData::Resident(Vec::new()) } else { FileData::Extents(extents) },
            });
        }
        Ok(out)
    }

    fn free_ranges(&self) -> Result<Vec<ByteRange>> {
        let bitmap = self.bitmap();
        ensure!(!bitmap.is_empty(), "allocation file unreadable");
        // The bitmap is most significant bit first; zero_bit_runs wants LSB.
        let lsb: Vec<u8> = bitmap.iter().map(|b| b.reverse_bits()).collect();
        Ok(zero_bit_runs(&lsb, self.blocks).into_iter().map(|r| r.start * self.block..r.end * self.block).collect())
    }
}
