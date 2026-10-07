//! ext2 / ext3 / ext4 (Linux).
//!
//! * Files that still exist are listed by walking the directories from the
//!   root and reading each file's extent tree (ext4) or block map (ext2/3).
//! * A deleted file's name survives in its directory: Linux does not erase
//!   the entry, it only makes the entry before it longer, so the name and
//!   inode number are still there in the gap.
//! * ext3/4 clear a deleted file's block list, but the journal holds older
//!   copies of the inode tables: the newest copy in which the file still
//!   existed gives its data back (the way extundelete works). ext2 keeps
//!   the block list in the inode itself.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};

use super::{Condition, DeletedFile, Extent, FileData, FsKind, Volume};
use crate::bytes::{be32, le16, le32, u8_at};
use crate::ranges::{ByteRange, bit_is_set, zero_bit_runs};
use crate::source::{Source, read_tolerant};

const SUPERBLOCK: u64 = 1024;
const MAGIC: u16 = 0xEF53;
const ROOT_INODE: u32 = 2;
const INCOMPAT_64BIT: u32 = 0x80;
const FLAG_EXTENTS: u32 = 0x8_0000;
const FLAG_INLINE: u32 = 0x1000_0000;
const BG_BLOCK_UNINIT: u16 = 0x2;
const JOURNAL_MAGIC: u32 = 0xC03B_3998;
/// Largest directory read (bytes).
const MAX_DIR: u64 = 64 << 20;
/// Most extent-tree or block-map entries followed for one file.
const MAX_RUNS: usize = 1 << 20;

/// Whether `bs` (the 1024 bytes at offset 1024) is an ext superblock.
pub fn detect(sb: &[u8]) -> bool {
    le16(sb, 56) == Some(MAGIC)
        && le32(sb, 24).is_some_and(|l| l <= 6)
        && le32(sb, 32).is_some_and(|b| b > 0)
        && le32(sb, 40).is_some_and(|i| i > 0)
}

struct Group {
    block_bitmap: u64,
    inode_table: u64,
    uninit: bool,
}

pub struct Ext {
    src: Source,
    block: u64,
    blocks: u64,
    first_data_block: u64,
    blocks_per_group: u64,
    inodes_per_group: u64,
    inodes: u64,
    inode_size: u64,
    groups: Vec<Group>,
    journal_inode: u32,
    /// ext3/4 (journal) or ext2.
    has_journal: bool,
    extents: bool,
}

/// What the journal still holds: inode number -> newest copy in which the
/// file had data, and old directory blocks (their block, their entries).
type JournalCopies = (HashMap<u32, Vec<u8>>, Vec<(u64, Vec<Entry>)>);

/// A directory entry: live, or deleted (found in the gap of another).
struct Entry {
    inode: u32,
    name: String,
    kind: u8,
    deleted: bool,
}

impl Ext {
    pub fn open(src: Source) -> Result<Self> {
        let sb = src.read_vec(SUPERBLOCK, 1024).context("reading the ext superblock")?;
        ensure!(detect(&sb), "not an ext2/3/4 superblock");
        let n = |o: usize| u64::from(le32(&sb, o).unwrap_or(0));
        let block = 1024u64 << n(24);
        let incompat = le32(&sb, 0x60).unwrap_or(0);
        let wide = incompat & INCOMPAT_64BIT != 0;
        let blocks = n(4) | if wide { n(0x150) << 32 } else { 0 };
        let (first_data_block, blocks_per_group, inodes_per_group) = (n(20), n(32), n(40));
        let inode_size = if n(76) >= 1 { u64::from(le16(&sb, 88).unwrap_or(128)) } else { 128 };
        let desc_size = if wide { u64::from(le16(&sb, 0xFE).unwrap_or(32)).max(32) } else { 32 };
        ensure!(blocks > first_data_block && (128..=4096).contains(&inode_size), "invalid ext geometry");
        let count = (blocks - first_data_block).div_ceil(blocks_per_group);
        ensure!(count > 0 && count < 1 << 24, "invalid ext group count {count}");
        let gdt = src
            .read_vec((first_data_block + 1) * block, (count * desc_size) as usize)
            .context("reading the ext group descriptors")?;
        let groups = (0..count as usize)
            .map(|g| {
                let d = &gdt[g * desc_size as usize..];
                let half = |lo: usize, hi: usize| {
                    u64::from(le32(d, lo).unwrap_or(0))
                        | if desc_size >= 64 { u64::from(le32(d, hi).unwrap_or(0)) << 32 } else { 0 }
                };
                Group {
                    block_bitmap: half(0, 0x20),
                    inode_table: half(8, 0x28),
                    uninit: le16(d, 0x12).unwrap_or(0) & BG_BLOCK_UNINIT != 0,
                }
            })
            .collect();
        let compat = le32(&sb, 0x5C).unwrap_or(0);
        Ok(Ext {
            src,
            block,
            blocks,
            first_data_block,
            blocks_per_group,
            inodes_per_group,
            inodes: n(0),
            inode_size,
            groups,
            journal_inode: le32(&sb, 0xE0).unwrap_or(0),
            has_journal: compat & 0x4 != 0,
            extents: incompat & 0x40 != 0,
        })
    }

    fn inode_offset(&self, n: u32) -> Option<u64> {
        let i = u64::from(n.checked_sub(1)?);
        let g = self.groups.get((i / self.inodes_per_group) as usize)?;
        Some(g.inode_table * self.block + (i % self.inodes_per_group) * self.inode_size)
    }

    fn inode(&self, n: u32) -> Option<Vec<u8>> {
        let mut b = vec![0u8; self.inode_size as usize];
        read_tolerant(self.src.as_ref(), self.inode_offset(n)?, &mut b);
        Some(b)
    }

    fn read_block(&self, b: u64) -> Vec<u8> {
        let mut v = vec![0u8; self.block as usize];
        if b < self.blocks {
            read_tolerant(self.src.as_ref(), b * self.block, &mut v);
        }
        v
    }

    /// The data of an inode: extents in file order (gaps are sparse), or
    /// `None` when it has none left.
    fn data(&self, ino: &[u8]) -> Option<FileData> {
        let size = file_size(ino);
        let flags = le32(ino, 32)?;
        let iblock = ino.get(40..100)?;
        if flags & FLAG_INLINE != 0 {
            return Some(FileData::Resident(iblock[..(size as usize).min(60)].to_vec()));
        }
        // (logical block, physical block or none, blocks)
        let mut runs: Vec<(u64, Option<u64>, u64)> = Vec::new();
        if flags & FLAG_EXTENTS != 0 {
            self.extent_node(iblock, 0, &mut runs);
        } else {
            self.block_map(iblock, &mut runs);
        }
        if runs.iter().all(|r| r.1.is_none()) {
            return (size == 0).then(|| FileData::Resident(Vec::new()));
        }
        runs.sort_unstable_by_key(|r| r.0);
        let mut extents = Vec::new();
        let mut next = 0u64;
        for (logical, phys, len) in runs {
            if logical < next {
                continue; // overlapping garbage
            }
            if logical > next {
                extents.push(Extent { offset: None, len: (logical - next) * self.block });
            }
            extents.push(Extent { offset: phys.map(|p| p * self.block), len: len * self.block });
            next = logical + len;
        }
        Some(FileData::Extents(extents))
    }

    fn extent_node(&self, node: &[u8], level: u32, runs: &mut Vec<(u64, Option<u64>, u64)>) {
        if le16(node, 0) != Some(0xF30A) || level > 5 || runs.len() >= MAX_RUNS {
            return;
        }
        let entries = usize::from(le16(node, 2).unwrap_or(0));
        let depth = le16(node, 6).unwrap_or(0);
        for k in 0..entries {
            let e = 12 + k * 12;
            let Some(entry) = node.get(e..e + 12) else { break };
            let logical = u64::from(le32(entry, 0).unwrap_or(0));
            if depth == 0 {
                let raw = u64::from(le16(entry, 4).unwrap_or(0));
                // Over 32768: preallocated but never written (reads as zeros).
                let (len, written) = if raw > 32768 { (raw - 32768, false) } else { (raw, true) };
                let start = u64::from(le16(entry, 6).unwrap_or(0)) << 32 | u64::from(le32(entry, 8).unwrap_or(0));
                if len > 0 && start + len <= self.blocks {
                    runs.push((logical, written.then_some(start), len));
                }
            } else {
                let child = u64::from(le32(entry, 4).unwrap_or(0)) | u64::from(le16(entry, 8).unwrap_or(0)) << 32;
                if child > 0 && child < self.blocks {
                    let b = self.read_block(child);
                    self.extent_node(&b, level + 1, runs);
                }
            }
        }
    }

    /// ext2/3: 12 direct blocks, then single, double and triple indirect.
    fn block_map(&self, iblock: &[u8], runs: &mut Vec<(u64, Option<u64>, u64)>) {
        let per = self.block / 4;
        let mut logical = 0u64;
        for k in 0..12 {
            self.map_ptr(u64::from(le32(iblock, k * 4).unwrap_or(0)), 0, &mut logical, runs);
        }
        for (k, depth) in [(12, 1), (13, 2), (14, 3)] {
            let ptr = u64::from(le32(iblock, k * 4).unwrap_or(0));
            if ptr == 0 {
                logical += per.pow(depth);
            } else {
                self.map_ptr(ptr, depth, &mut logical, runs);
            }
        }
    }

    fn map_ptr(&self, ptr: u64, depth: u32, logical: &mut u64, runs: &mut Vec<(u64, Option<u64>, u64)>) {
        let per = self.block / 4;
        if runs.len() >= MAX_RUNS {
            return;
        }
        if depth == 0 {
            if ptr != 0 && ptr < self.blocks {
                match runs.last_mut() {
                    Some(r) if r.1.is_some_and(|p| p + r.2 == ptr) && r.0 + r.2 == *logical => r.2 += 1,
                    _ => runs.push((*logical, Some(ptr), 1)),
                }
            }
            *logical += 1;
            return;
        }
        if ptr == 0 || ptr >= self.blocks {
            *logical += per.pow(depth);
            return;
        }
        let b = self.read_block(ptr);
        for k in 0..per as usize {
            self.map_ptr(u64::from(le32(&b, k * 4).unwrap_or(0)), depth - 1, logical, runs);
        }
    }

    /// Entries of a directory (live ones, and deleted ones in the gaps),
    /// and the blocks it is stored in.
    fn entries(&self, ino: &[u8]) -> (Vec<Entry>, Vec<u64>) {
        let Some(FileData::Extents(extents)) = self.data(ino) else { return (Vec::new(), Vec::new()) };
        let mut out = Vec::new();
        let mut blocks = Vec::new();
        let mut read = 0u64;
        for e in &extents {
            let Some(off) = e.offset else { continue };
            for k in 0..e.len / self.block {
                if read >= MAX_DIR {
                    return (out, blocks);
                }
                let b = off / self.block + k;
                blocks.push(b);
                parse_dir_block(&self.read_block(b), self.inodes, &mut out);
                read += self.block;
            }
        }
        (out, blocks)
    }

    /// Bitmap of used blocks, or `None` if unreadable.
    fn bitmap(&self) -> Option<Vec<u8>> {
        let bpg = self.blocks_per_group as usize;
        let mut map = vec![0u8; (self.groups.len() * bpg).div_ceil(8)];
        for (g, group) in self.groups.iter().enumerate() {
            if group.uninit {
                continue; // never used: all free
            }
            let b = self.read_block(group.block_bitmap);
            for bit in 0..bpg.min(b.len() * 8) {
                if b[bit / 8] & (1 << (bit % 8)) != 0 {
                    let i = g * bpg + bit;
                    map[i / 8] |= 1 << (i % 8);
                }
            }
        }
        Some(map)
    }

    /// Condition of a deleted file's data, from the block bitmap.
    fn condition(&self, data: &FileData, size: u64, bitmap: Option<&[u8]>) -> Condition {
        let extents = match data {
            FileData::Lost => return Condition::Overwritten,
            FileData::Resident(_) => return Condition::Recoverable,
            FileData::Extents(e) | FileData::Compressed { extents: e, .. } => e,
        };
        let Some(bitmap) = bitmap else { return Condition::Recoverable };
        let (mut free, mut total, mut left) = (0u64, 0u64, size.div_ceil(self.block));
        for e in extents {
            let n = (e.len / self.block).min(left);
            left -= n;
            let Some(off) = e.offset else { continue };
            for b in off / self.block..off / self.block + n {
                total += 1;
                if bit_is_set(bitmap, b - self.first_data_block) == Some(false) {
                    free += 1;
                }
            }
        }
        Condition::from_counts(free, total)
    }

    /// Older copies of inodes and directory blocks kept in the journal:
    /// (inode number -> newest copy in which the file had data) and
    /// directory entries from old directory blocks (with their block).
    fn journal(&self) -> JournalCopies {
        let mut inodes: HashMap<u32, (u32, Vec<u8>)> = HashMap::new();
        let mut dirs = Vec::new();
        if !self.has_journal || self.journal_inode == 0 {
            return (HashMap::new(), dirs);
        }
        let Some(FileData::Extents(extents)) = self.inode(self.journal_inode).and_then(|i| self.data(&i)) else {
            return (HashMap::new(), dirs);
        };
        // Journal block k -> block on the volume.
        let mut map: Vec<u64> = Vec::new();
        for e in &extents {
            let Some(off) = e.offset else { continue };
            map.extend((0..e.len / self.block).map(|k| off / self.block + k));
        }
        let Some(&first) = map.first() else { return (HashMap::new(), dirs) };
        let jsb = self.read_block(first);
        if be32(&jsb, 0) != Some(JOURNAL_MAGIC) {
            return (HashMap::new(), dirs);
        }
        let incompat = if be32(&jsb, 4) == Some(4) { be32(&jsb, 40).unwrap_or(0) } else { 0 };
        let (wide, csum_v3) = (incompat & 0x2 != 0, incompat & 0x10 != 0);
        let tag_size = if csum_v3 {
            16
        } else if wide {
            12
        } else {
            8
        };
        let maxlen = (be32(&jsb, 16).unwrap_or(0) as usize).min(map.len());
        let first_log = (be32(&jsb, 20).unwrap_or(1) as usize).max(1);
        let ring = maxlen.saturating_sub(first_log).max(1);
        let at = |k: usize| map.get(first_log + (k - first_log) % ring).copied();
        let tables: Vec<(u64, u64)> = self
            .groups
            .iter()
            .map(|g| (g.inode_table, g.inode_table + (self.inodes_per_group * self.inode_size).div_ceil(self.block)))
            .collect();
        let mut k = first_log;
        while k < maxlen {
            let Some(b) = at(k) else { break };
            let desc = self.read_block(b);
            // A descriptor block lists the volume blocks copied after it.
            if be32(&desc, 0) != Some(JOURNAL_MAGIC) || be32(&desc, 4) != Some(1) {
                k += 1;
                continue;
            }
            let seq = be32(&desc, 8).unwrap_or(0);
            let mut p = 12usize;
            let mut data = k + 1;
            while p + tag_size <= desc.len() {
                let lo = u64::from(be32(&desc, p).unwrap_or(0));
                let flags = if csum_v3 {
                    be32(&desc, p + 4).unwrap_or(0)
                } else {
                    u32::from(crate::bytes::be16(&desc, p + 6).unwrap_or(0))
                };
                let hi = if wide { u64::from(be32(&desc, p + 8).unwrap_or(0)) } else { 0 };
                let target = hi << 32 | lo;
                if let Some(copy_at) = at(data) {
                    let mut copy = self.read_block(copy_at);
                    if flags & 0x1 != 0 {
                        copy[..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes()); // escaped block
                    }
                    self.journal_copy(target, &copy, seq, &tables, &mut inodes, &mut dirs);
                }
                data += 1;
                p += tag_size;
                if flags & 0x2 == 0 {
                    p += 16; // the journal's UUID follows the first tag
                }
                if flags & 0x8 != 0 {
                    break; // last tag
                }
            }
            k = data;
        }
        (inodes.into_iter().map(|(n, (_, b))| (n, b)).collect(), dirs)
    }

    fn journal_copy(
        &self,
        target: u64,
        copy: &[u8],
        seq: u32,
        tables: &[(u64, u64)],
        inodes: &mut HashMap<u32, (u32, Vec<u8>)>,
        dirs: &mut Vec<(u64, Vec<Entry>)>,
    ) {
        if let Some(g) = tables.iter().position(|t| (t.0..t.1).contains(&target)) {
            // A copy of part of an inode table.
            let per_block = self.block / self.inode_size;
            let first = g as u64 * self.inodes_per_group + (target - tables[g].0) * per_block + 1;
            for s in 0..per_block {
                let ino = &copy[(s * self.inode_size) as usize..((s + 1) * self.inode_size) as usize];
                let is_file = le16(ino, 0).unwrap_or(0) & 0xF000 == 0x8000;
                let links = le16(ino, 26).unwrap_or(0);
                if is_file && links > 0 && file_size(ino) > 0 && self.data(ino).is_some_and(|d| has_data(&d)) {
                    let n = (first + s) as u32;
                    if inodes.get(&n).is_none_or(|(old, _)| seq.wrapping_sub(*old) as i32 > 0) {
                        inodes.insert(n, (seq, ino.to_vec()));
                    }
                }
            }
        } else {
            let mut entries = Vec::new();
            if parse_dir_block(copy, self.inodes, &mut entries) {
                dirs.push((target, entries));
            }
        }
    }
}

fn has_data(d: &FileData) -> bool {
    match d {
        FileData::Extents(e) => e.iter().any(|x| x.offset.is_some()),
        FileData::Resident(v) => !v.is_empty(),
        _ => false,
    }
}

fn file_size(ino: &[u8]) -> u64 {
    u64::from(le32(ino, 4).unwrap_or(0)) | u64::from(le32(ino, 108).unwrap_or(0)) << 32
}

fn time(ino: &[u8], at: usize) -> Option<chrono::NaiveDateTime> {
    let t = le32(ino, at)?;
    (t != 0).then(|| chrono::DateTime::from_timestamp(i64::from(t), 0)).flatten().map(|d| d.naive_utc())
}

/// Parses one directory block into `out`. Returns false if it does not
/// look like one (the entries do not chain to its end).
fn parse_dir_block(b: &[u8], inodes: u64, out: &mut Vec<Entry>) -> bool {
    let mut off = 0usize;
    let mut found = Vec::new();
    while off + 8 <= b.len() {
        let (Some(ino), Some(rec), Some(nlen), Some(kind)) =
            (le32(b, off), le16(b, off + 4), u8_at(b, off + 6), u8_at(b, off + 7))
        else {
            return false;
        };
        let rec = usize::from(rec);
        if rec < 8 || rec % 4 != 0 || off + rec > b.len() {
            return false;
        }
        let nlen = usize::from(nlen);
        if ino != 0
            && nlen > 0
            && let Some(name) = entry_name(b, off + 8, nlen)
        {
            found.push(Entry { inode: ino, name, kind, deleted: false });
        }
        // Deleted entries hide in the gap after this one's name.
        let mut p = off + 8 + nlen.div_ceil(4) * 4;
        while p + 8 <= off + rec {
            let gap_ino = le32(b, p).unwrap_or(0);
            let gap_len = usize::from(u8_at(b, p + 6).unwrap_or(0));
            let gap_kind = u8_at(b, p + 7).unwrap_or(0);
            if gap_ino != 0
                && u64::from(gap_ino) <= inodes
                && gap_len > 0
                && gap_kind <= 7
                && p + 8 + gap_len <= off + rec
                && let Some(name) = entry_name(b, p + 8, gap_len)
            {
                found.push(Entry { inode: gap_ino, name, kind: gap_kind, deleted: true });
                p += 8 + gap_len.div_ceil(4) * 4;
            } else {
                p += 4;
            }
        }
        off += rec;
    }
    let ok = off == b.len();
    if ok {
        out.extend(found);
    }
    ok
}

fn entry_name(b: &[u8], at: usize, len: usize) -> Option<String> {
    let raw = b.get(at..at + len)?;
    if raw.iter().any(|&c| c == 0 || c == b'/') {
        return None;
    }
    let s = String::from_utf8(raw.to_vec()).ok()?;
    (s != "." && s != "..").then_some(s)
}

impl Volume for Ext {
    fn kind(&self) -> FsKind {
        FsKind::Ext
    }

    fn describe(&self) -> String {
        let v = if self.extents {
            "ext4"
        } else if self.has_journal {
            "ext3"
        } else {
            "ext2"
        };
        format!("{v}, {} blocks, {} block groups", crate::units::format_size(self.block), self.groups.len())
    }

    fn source(&self) -> &Source {
        &self.src
    }

    fn cluster_size(&self) -> u64 {
        self.block
    }

    fn scan_files(&self, live: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<DeletedFile>> {
        let root = self.inode(ROOT_INODE).context("reading the root directory")?;
        // Walk the directory tree.
        let mut stack = vec![(root, String::new())];
        let mut visited = HashSet::from([ROOT_INODE]);
        // Deleted names: inode -> path; and where each directory is stored.
        let mut deleted: HashMap<u32, String> = HashMap::new();
        let mut dir_blocks: HashMap<u64, String> = HashMap::new();
        let mut out = Vec::new();
        let mut done = 0u64;
        while let Some((ino, dir)) = stack.pop() {
            let (entries, blocks) = self.entries(&ino);
            for b in blocks {
                dir_blocks.insert(b, dir.clone());
            }
            for e in entries {
                let path = if dir.is_empty() { e.name.clone() } else { format!("{dir}/{}", e.name) };
                if e.deleted {
                    if e.kind != 2 {
                        deleted.entry(e.inode).or_insert(path);
                    }
                    continue;
                }
                match e.kind {
                    2 if visited.insert(e.inode) => {
                        if let Some(child) = self.inode(e.inode) {
                            stack.push((child, path));
                        }
                    }
                    1 if live => {
                        if let Some(i) = self.inode(e.inode) {
                            let size = file_size(&i);
                            if let Some(data) = self.data(&i) {
                                out.push(DeletedFile {
                                    id: u64::from(e.inode),
                                    path,
                                    size,
                                    created: time(&i, 0x90).or_else(|| time(&i, 12)),
                                    modified: time(&i, 16),
                                    condition: Condition::Recoverable,
                                    note: None,
                                    data,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            done += 1;
            progress(done, done + stack.len() as u64);
        }

        // Deleted files: from the names in the gaps and from the journal.
        let (old_inodes, old_dirs) = self.journal();
        // Old directory blocks name files the directory no longer lists.
        for (block, entries) in old_dirs {
            let dir = dir_blocks.get(&block).cloned();
            for e in entries.into_iter().filter(|e| e.kind == 1) {
                let path = match &dir {
                    Some(d) if !d.is_empty() => format!("{d}/{}", e.name),
                    Some(_) => e.name,
                    None => format!("$Orphan/{}", e.name),
                };
                deleted.entry(e.inode).or_insert(path);
            }
        }
        let bitmap = self.bitmap();
        let mut candidates: Vec<u32> = deleted.keys().chain(old_inodes.keys()).copied().collect();
        candidates.sort_unstable();
        candidates.dedup();
        for n in candidates {
            let Some(now) = self.inode(n) else { continue };
            // Still linked: renamed or reused, not deleted.
            if le16(&now, 26).unwrap_or(0) > 0 {
                continue;
            }
            let path = deleted.get(&n).cloned().unwrap_or_else(|| format!("$Orphan/inode_{n}"));
            // The inode as it is now (ext2 keeps the block list), or the
            // newest copy in the journal.
            let (ino, from_journal) = match self.data(&now).filter(has_data) {
                Some(_) => (now, false),
                None => match old_inodes.get(&n) {
                    Some(old) => (old.clone(), true),
                    None => (now, false),
                },
            };
            let size = file_size(&ino);
            let data = self.data(&ino).filter(has_data).unwrap_or(FileData::Lost);
            let mut note = from_journal.then(|| "data location from the journal".to_string());
            if matches!(data, FileData::Lost) {
                note = Some("data location lost".into());
            }
            out.push(DeletedFile {
                id: u64::from(n),
                path,
                size,
                created: time(&ino, 0x90).or_else(|| time(&ino, 12)),
                modified: time(&ino, 16),
                condition: self.condition(&data, size, bitmap.as_deref()),
                note,
                data,
            });
        }
        Ok(out)
    }

    fn free_ranges(&self) -> Result<Vec<ByteRange>> {
        let bitmap = self.bitmap().context("reading the block bitmaps")?;
        let n = self.blocks - self.first_data_block;
        Ok(zero_bit_runs(&bitmap, n)
            .into_iter()
            .map(|r| (r.start + self.first_data_block) * self.block..(r.end + self.first_data_block) * self.block)
            .collect())
    }
}
