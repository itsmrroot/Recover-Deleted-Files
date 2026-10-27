//! XFS (Red Hat, CentOS, some NAS drives). Big-endian.
//!
//! The volume is split into allocation groups (AGs), each with its own
//! inode B-tree (which inodes exist, which are free) and free-space
//! B-tree. A file's inode lists its extents, inline or in a B-tree.
//!
//! Deleting a file frees its inode and clears it. But the journal (log)
//! keeps, until its space is reused, copies of each inode as it changed —
//! with its size, times and list of extents — and copies of small
//! directories, with their names. Older kernels also left the extent list
//! in the freed inode, and a freed entry of a larger directory keeps its
//! name and most of the inode number. Deleted files are put together from
//! all of these.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use chrono::NaiveDateTime;

use super::{Condition, DeletedFile, Extent, FileData, FsKind, Volume};
use crate::bytes::{be16, be32, be64};
use crate::ranges::{ByteRange, normalize};
use crate::source::Source;

pub fn detect(bs: &[u8]) -> bool {
    bs.starts_with(b"XFSB") && be32(bs, 4).is_some_and(|b| (512..=65536).contains(&b))
}

const ROOT_LEAF_OFFSET: u64 = 32 << 30; // directory leaf blocks start at 32 GiB

pub struct Xfs {
    src: Source,
    block: u64,
    ag_blocks: u64,
    ag_count: u64,
    ag_blk_log: u32,
    inode_size: u64,
    inopb_log: u32,
    dir_block: u64,
    root: u64,
    v5: bool,
    ftype: bool,
    bigtime: bool,
    nrext64: bool,
    label: String,
}

/// An inode as read: its raw bytes.
struct Inode(Vec<u8>);

impl Inode {
    fn mode(&self) -> u16 {
        be16(&self.0, 2).unwrap_or(0)
    }
    fn format(&self) -> u8 {
        self.0.get(5).copied().unwrap_or(0)
    }
    fn size(&self) -> u64 {
        be64(&self.0, 56).unwrap_or(0)
    }
    fn is_v3(&self) -> bool {
        self.0.get(4) == Some(&3)
    }
}

impl Xfs {
    pub fn open(src: Source) -> Result<Self> {
        let sb = src.read_vec(0, 512).context("reading the XFS superblock")?;
        anyhow::ensure!(detect(&sb), "not XFS");
        let version = be16(&sb, 100).unwrap_or(0);
        let v5 = version & 0xF == 5;
        let incompat = if v5 { be32(&sb, 216).unwrap_or(0) } else { 0 };
        let features2 = be32(&sb, 200).unwrap_or(0);
        let block = u64::from(be32(&sb, 4).unwrap_or(4096));
        Ok(Self {
            block,
            ag_blocks: u64::from(be32(&sb, 84).unwrap_or(0)),
            ag_count: u64::from(be32(&sb, 88).unwrap_or(0)),
            ag_blk_log: u32::from(sb[124]),
            inode_size: u64::from(be16(&sb, 104).unwrap_or(256)),
            inopb_log: u32::from(sb[123]),
            dir_block: block << sb[192],
            root: be64(&sb, 56).unwrap_or(0),
            v5,
            ftype: if v5 { incompat & 1 != 0 } else { features2 & 0x200 != 0 },
            bigtime: incompat & 0x8 != 0,
            nrext64: incompat & 0x20 != 0,
            label: String::from_utf8_lossy(&sb[108..120]).trim_end_matches('\0').to_string(),
            src,
        })
    }

    /// Byte offset of a filesystem block number (AG number in the high
    /// bits).
    fn fsb(&self, b: u64) -> u64 {
        let ag = b >> self.ag_blk_log;
        let agb = b & ((1 << self.ag_blk_log) - 1);
        (ag * self.ag_blocks + agb) * self.block
    }

    fn agb(&self, ag: u64, agb: u64) -> u64 {
        (ag * self.ag_blocks + agb) * self.block
    }

    fn inode_at(&self, ino: u64) -> u64 {
        let per_ag = self.ag_blk_log + self.inopb_log;
        let ag = ino >> per_ag;
        let agino = ino & ((1 << per_ag) - 1);
        self.agb(ag, agino >> self.inopb_log) + (agino & ((1 << self.inopb_log) - 1)) * self.inode_size
    }

    fn inode(&self, ino: u64) -> Option<Inode> {
        let b = self.src.read_vec(self.inode_at(ino), self.inode_size as usize).ok()?;
        b.starts_with(b"IN").then_some(Inode(b))
    }

    fn fork(&self, i: &Inode) -> usize {
        if i.is_v3() { 176 } else { 100 }
    }

    /// The data fork's bytes (up to the attribute fork).
    fn data_fork<'a>(&self, i: &'a Inode) -> &'a [u8] {
        let start = self.fork(i);
        let forkoff = usize::from(i.0.get(82).copied().unwrap_or(0)) * 8;
        let end = if forkoff > 0 { start + forkoff } else { i.0.len() };
        i.0.get(start..end.min(i.0.len())).unwrap_or_default()
    }

    fn time(&self, i: &Inode, at: usize) -> Option<NaiveDateTime> {
        let t = if self.bigtime && be64(&i.0, 120).is_some_and(|f| f & 8 != 0) {
            let ns = be64(&i.0, at)?;
            let secs = (ns / 1_000_000_000) as i64 - (1 << 31);
            chrono::DateTime::from_timestamp(secs, (ns % 1_000_000_000) as u32)
        } else {
            chrono::DateTime::from_timestamp(i64::from(be32(&i.0, at)? as i32), be32(&i.0, at + 4)?)
        };
        t.map(|d| d.naive_utc()).filter(|d| chrono::Datelike::year(d) > 1970)
    }

    /// Extent records (file block, disk byte, block count) of an inode, in
    /// the fork itself or in its B-tree. `count` limits the inline list
    /// (a freed inode says 0, but its records may remain: `None` reads
    /// every plausible one).
    fn extents(&self, i: &Inode, count: Option<usize>) -> Vec<(u64, u64, u64)> {
        let fork = self.data_fork(i);
        match i.format() {
            2 => {
                let max = fork.len() / 16;
                let n = count.unwrap_or(max).min(max);
                (0..n).filter_map(|k| record(fork, k * 16)).map(|(o, b, c)| (o, self.fsb(b), c)).collect()
            }
            3 => {
                // bmdr root: level, numrecs, keys, then pointers.
                let level = be16(fork, 0).unwrap_or(0);
                let n = usize::from(be16(fork, 2).unwrap_or(0));
                let max = (fork.len() - 4) / 16;
                let mut out = Vec::new();
                for k in 0..n.min(max) {
                    if let Some(ptr) = be64(fork, 4 + max * 8 + k * 8) {
                        self.bmbt(ptr, level.saturating_sub(1), &mut out, 0);
                    }
                }
                out
            }
            _ => Vec::new(),
        }
    }

    fn bmbt(&self, block: u64, level: u16, out: &mut Vec<(u64, u64, u64)>, depth: usize) {
        if depth > 8 {
            return;
        }
        let Ok(b) = self.src.read_vec(self.fsb(block), self.block as usize) else { return };
        let header = if b.starts_with(b"BMA3") {
            72
        } else if b.starts_with(b"BMAP") {
            24
        } else {
            return;
        };
        let n = usize::from(be16(&b, 6).unwrap_or(0));
        if level == 0 || be16(&b, 4) == Some(0) {
            for k in 0..n {
                if let Some((o, s, c)) = record(&b, header + k * 16) {
                    out.push((o, self.fsb(s), c));
                }
            }
        } else {
            let max = (b.len() - header) / 16;
            for k in 0..n.min(max) {
                if let Some(ptr) = be64(&b, header + max * 8 + k * 8) {
                    self.bmbt(ptr, level - 1, out, depth + 1);
                }
            }
        }
    }

    fn file_data(&self, extents: &[(u64, u64, u64)], size: u64) -> FileData {
        let mut ex: Vec<(u64, u64, u64)> = extents.to_vec();
        ex.sort_unstable();
        let mut out = Vec::new();
        let mut pos = 0u64;
        for (off, at, count) in ex {
            let off = off * self.block;
            if off >= size {
                break;
            }
            if off > pos {
                out.push(Extent { offset: None, len: off - pos });
            } else if off < pos {
                continue;
            }
            let len = (count * self.block).min(size - off);
            out.push(Extent { offset: Some(at), len });
            pos = off + len;
        }
        if pos < size {
            out.push(Extent { offset: None, len: size - pos });
        }
        FileData::Extents(out)
    }

    /// Entries of a directory: (inode, name, live). Deleted entries are
    /// found in the free space of directory blocks.
    fn entries(&self, dir: &Inode) -> Vec<(u64, String, bool)> {
        let fork = self.data_fork(dir);
        if dir.format() == 1 {
            return self.shortform(fork);
        }
        let mut out = Vec::new();
        for (off, at, count) in self.extents(dir, Some(usize::MAX)) {
            // Data blocks only (leaf and free-index blocks come later).
            if off * self.block >= ROOT_LEAF_OFFSET {
                continue;
            }
            let mut pos = 0u64;
            while pos < count * self.block {
                if let Ok(b) = self.src.read_vec(at + pos, self.dir_block as usize) {
                    self.data_block(&b, &mut out);
                }
                pos += self.dir_block;
            }
        }
        out
    }

    fn shortform(&self, f: &[u8]) -> Vec<(u64, String, bool)> {
        let count = usize::from(f.first().copied().unwrap_or(0));
        let wide = f.get(1).copied().unwrap_or(0) > 0;
        let ino_len = if wide { 8 } else { 4 };
        let mut at = 2 + ino_len;
        let mut out = Vec::new();
        for _ in 0..count {
            let Some(&len) = f.get(at) else { break };
            let len = usize::from(len);
            let name_at = at + 3;
            let Some(name) = f.get(name_at..name_at + len) else { break };
            let ino_at = name_at + len + usize::from(self.ftype);
            let ino = if wide { be64(f, ino_at) } else { be32(f, ino_at).map(u64::from) };
            if let Some(ino) = ino {
                out.push((ino, String::from_utf8_lossy(name).to_string(), true));
            }
            at = ino_at + ino_len;
        }
        out
    }

    fn data_block(&self, b: &[u8], out: &mut Vec<(u64, String, bool)>) {
        let (header, end) = match &b[..4] {
            b"XDB3" | b"XD2B" => {
                // Block form: leaf entries and a tail at the end.
                let count = be32(b, b.len() - 8).unwrap_or(0) as usize;
                (if &b[..4] == b"XDB3" { 64 } else { 16 }, b.len().saturating_sub(8 + count * 8))
            }
            b"XDD3" => (64, b.len()),
            b"XD2D" => (16, b.len()),
            _ => return,
        };
        let mut at = header;
        while at + 8 <= end {
            if be16(b, at) == Some(0xFFFF) {
                // Free space: deleted entries may remain in it.
                let len = usize::from(be16(b, at + 2).unwrap_or(0));
                if len < 8 || at + len > end {
                    break;
                }
                self.freed(b, at, at + len, out);
                at += len;
                continue;
            }
            match self.entry(b, at, end, false) {
                Some((ino, name, size)) => {
                    out.push((ino, name, true));
                    at += size;
                }
                None => break,
            }
        }
    }

    /// A directory entry at `at`: (inode, name, size). For a freed entry
    /// the first four bytes (the inode number's high half) are gone.
    fn entry(&self, b: &[u8], at: usize, end: usize, freed: bool) -> Option<(u64, String, usize)> {
        let ino = if freed { u64::from(be32(b, at + 4)?) } else { be64(b, at)? };
        let len = usize::from(*b.get(at + 8)?);
        if len == 0 {
            return None;
        }
        let name = b.get(at + 9..at + 9 + len)?;
        let size = (9 + len + usize::from(self.ftype) + 2).div_ceil(8) * 8;
        if at + size > end || be16(b, at + size - 2)? as usize != at {
            return None;
        }
        if freed && (name.iter().any(|&c| c == 0 || c == b'/') || ino == 0) {
            return None;
        }
        Some((ino, String::from_utf8_lossy(name).to_string(), size))
    }

    fn freed(&self, b: &[u8], start: usize, end: usize, out: &mut Vec<(u64, String, bool)>) {
        let mut at = start;
        while at + 16 <= end {
            match self.entry(b, at, end, true) {
                Some((ino, name, size)) => {
                    out.push((ino, name, false));
                    at += size;
                }
                None => at += 8,
            }
        }
    }

    /// Every inode chunk: (first inode number, free mask), from the inode
    /// B-tree of each allocation group.
    fn inode_chunks(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        for ag in 0..self.ag_count {
            let Ok(agi) = self.src.read_vec(self.agb(ag, 0) + 2 * 512.min(self.block), 512) else { continue };
            if !agi.starts_with(b"XAGI") {
                continue;
            }
            let root = u64::from(be32(&agi, 20).unwrap_or(0));
            self.inobt(ag, root, &mut out, 0);
        }
        out
    }

    fn inobt(&self, ag: u64, agb: u64, out: &mut Vec<(u64, u64)>, depth: usize) {
        if depth > 8 {
            return;
        }
        let Ok(b) = self.src.read_vec(self.agb(ag, agb), self.block as usize) else { return };
        let header = if b.starts_with(b"IAB3") {
            56
        } else if b.starts_with(b"IABT") {
            16
        } else {
            return;
        };
        let level = be16(&b, 4).unwrap_or(0);
        let n = usize::from(be16(&b, 6).unwrap_or(0));
        let per_ag = self.ag_blk_log + self.inopb_log;
        if level == 0 {
            for k in 0..n {
                let r = header + k * 16;
                let (Some(start), Some(free)) = (be32(&b, r), be64(&b, r + 8)) else { break };
                out.push(((ag << per_ag) | u64::from(start), free));
            }
        } else {
            let max = (b.len() - header) / 8;
            for k in 0..n.min(max) {
                if let Some(ptr) = be32(&b, header + max * 4 + k * 4) {
                    self.inobt(ag, u64::from(ptr), out, depth + 1);
                }
            }
        }
    }

    /// Free space of every allocation group: byte ranges.
    fn free(&self) -> Vec<ByteRange> {
        let mut out = Vec::new();
        for ag in 0..self.ag_count {
            let Ok(agf) = self.src.read_vec(self.agb(ag, 0) + 512.min(self.block), 512) else { continue };
            if !agf.starts_with(b"XAGF") {
                continue;
            }
            let root = u64::from(be32(&agf, 16).unwrap_or(0));
            self.bnobt(ag, root, &mut out, 0);
        }
        normalize(out)
    }

    fn bnobt(&self, ag: u64, agb: u64, out: &mut Vec<ByteRange>, depth: usize) {
        if depth > 8 {
            return;
        }
        let Ok(b) = self.src.read_vec(self.agb(ag, agb), self.block as usize) else { return };
        let header = if b.starts_with(b"AB3B") {
            56
        } else if b.starts_with(b"ABTB") {
            16
        } else {
            return;
        };
        let level = be16(&b, 4).unwrap_or(0);
        let n = usize::from(be16(&b, 6).unwrap_or(0));
        if level == 0 {
            for k in 0..n {
                let r = header + k * 8;
                let (Some(start), Some(count)) = (be32(&b, r), be32(&b, r + 4)) else { break };
                let at = self.agb(ag, u64::from(start));
                out.push(at..at + u64::from(count) * self.block);
            }
        } else {
            let max = (b.len() - header) / 12;
            for k in 0..n.min(max) {
                if let Some(ptr) = be32(&b, header + max * 8 + k * 4) {
                    self.bnobt(ag, u64::from(ptr), out, depth + 1);
                }
            }
        }
    }
}

/// An inode item being read from the log: inode, fields, regions so far,
/// little-endian.
type LogItem = (u64, u32, Vec<Vec<u8>>, bool);

/// An inode as the log last recorded it with its data fork.
struct Logged {
    lsn: u64,
    /// The inode core, in the byte order of the machine that wrote it.
    core: Vec<u8>,
    little: bool,
    /// The data fork, as on disk (extent records or a short directory).
    fork: Vec<u8>,
}

impl Logged {
    fn u16(&self, at: usize) -> u16 {
        let b = [self.core.get(at).copied().unwrap_or(0), self.core.get(at + 1).copied().unwrap_or(0)];
        if self.little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) }
    }
    fn u32(&self, at: usize) -> u32 {
        let b: [u8; 4] = self.core.get(at..at + 4).and_then(|b| b.try_into().ok()).unwrap_or_default();
        if self.little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) }
    }
    fn u64(&self, at: usize) -> u64 {
        let b: [u8; 8] = self.core.get(at..at + 8).and_then(|b| b.try_into().ok()).unwrap_or_default();
        if self.little { u64::from_le_bytes(b) } else { u64::from_be_bytes(b) }
    }
    fn mode(&self) -> u16 {
        self.u16(2)
    }
    fn time(&self, at: usize, bigtime: bool) -> Option<NaiveDateTime> {
        let t = if bigtime && self.u64(120) & 8 != 0 {
            let ns = self.u64(at);
            chrono::DateTime::from_timestamp((ns / 1_000_000_000) as i64 - (1 << 31), (ns % 1_000_000_000) as u32)
        } else {
            chrono::DateTime::from_timestamp(i64::from(self.u32(at) as i32), self.u32(at + 4))
        };
        t.map(|d| d.naive_utc()).filter(|d| chrono::Datelike::year(d) > 1970)
    }
}

impl Xfs {
    /// The log's area on the disk.
    fn log_area(&self) -> Option<(u64, u64)> {
        let sb = self.src.read_vec(0, 512).ok()?;
        let start = be64(&sb, 48)?;
        let blocks = u64::from(be32(&sb, 96)?);
        (start != 0 && blocks > 0).then(|| (self.fsb(start), blocks * self.block))
    }

    /// Inodes recorded in the log with their data fork: every copy.
    fn logged(&self) -> Vec<(u64, Logged)> {
        let mut out = Vec::new();
        let Some((at, len)) = self.log_area() else { return out };
        let Ok(log) = self.src.read_vec(at, len as usize) else { return out };
        let total = log.len();
        let mut bb = 0usize;
        while bb * 512 + 512 <= total {
            let h = &log[bb * 512..bb * 512 + 512];
            if be32(h, 0) != Some(0xFEED_BABE) {
                bb += 1;
                continue;
            }
            let data_len = be32(h, 12).unwrap_or(0) as usize;
            let lsn = be64(h, 16).unwrap_or(0);
            let ops = be32(h, 40).unwrap_or(0) as usize;
            let blocks = data_len.div_ceil(512);
            let ext_headers = if be32(h, 8) == Some(2) { blocks.div_ceil(64).saturating_sub(1) } else { 0 };
            let start = (bb + 1 + ext_headers) * 512;
            if data_len == 0 || data_len > 256 << 10 {
                bb += 1;
                continue;
            }
            // The record's data (it may wrap around the end of the log),
            // with the first word of each block put back.
            let mut data: Vec<u8> = (0..blocks * 512).map(|i| log[(start + i) % total]).collect();
            for i in 0..blocks {
                let word = if i < 64 {
                    h.get(44 + i * 4..48 + i * 4)
                } else {
                    let e = bb + 1 + (i / 64 - 1);
                    let off = e * 512 % total + 4 + (i % 64) * 4;
                    log.get(off..off + 4)
                };
                if let Some(w) = word {
                    data[i * 512..i * 512 + 4].copy_from_slice(w);
                }
            }
            data.truncate(data_len);
            self.log_ops(&data, ops, lsn, &mut out);
            bb += 1 + ext_headers + blocks;
        }
        out
    }

    /// The operations of one log record: inode items (format, core, data
    /// fork), per transaction.
    fn log_ops(&self, data: &[u8], ops: usize, lsn: u64, out: &mut Vec<(u64, Logged)>) {
        // Per transaction: the inode item being read.
        let mut items: HashMap<u32, LogItem> = HashMap::new();
        let mut at = 0usize;
        for _ in 0..ops {
            let (Some(tid), Some(len)) = (be32(data, at), be32(data, at + 4)) else { break };
            let flags = data.get(at + 9).copied().unwrap_or(0);
            let Some(payload) = data.get(at + 12..at + 12 + len as usize) else { break };
            at += 12 + len as usize;
            // An inode log format item starts with its type, 0x123b.
            let le = payload.len() >= 24 && payload[..2] == [0x3b, 0x12];
            let be = payload.len() >= 24 && payload[..2] == [0x12, 0x3b];
            if le || be {
                let rd = |o: usize, n: usize| -> u64 {
                    let b = &payload[o..o + n];
                    let mut v = 0u64;
                    for i in 0..n {
                        let byte = if le { b[n - 1 - i] } else { b[i] };
                        v = v << 8 | u64::from(byte);
                    }
                    v
                };
                items.insert(tid, (rd(16, 8), rd(4, 4) as u32, vec![payload.to_vec()], le));
                continue;
            }
            let Some(item) = items.get_mut(&tid) else { continue };
            // A region continued from the previous operation.
            if flags & 0x08 != 0
                && let Some(last) = item.2.last_mut()
            {
                last.extend_from_slice(payload);
            } else {
                item.2.push(payload.to_vec());
            }
            // Format, core, then the data fork if it was logged.
            let (ino, fields, regions, little) = item;
            if regions.len() == 3 && *fields & 0x0E != 0 {
                let core = regions[1].clone();
                let l = Logged { lsn, little: *little, core, fork: regions[2].clone() };
                if l.mode() != 0 {
                    out.push((*ino, l));
                }
            }
        }
    }
}

/// A packed extent record: (file block, filesystem block, block count).
fn record(b: &[u8], at: usize) -> Option<(u64, u64, u64)> {
    let hi = be64(b, at)?;
    let lo = be64(b, at + 8)?;
    let count = lo & 0x1F_FFFF;
    if count == 0 {
        return None;
    }
    Some(((hi >> 9) & ((1 << 54) - 1), ((hi & 0x1FF) << 43) | (lo >> 21), count))
}

impl Volume for Xfs {
    fn kind(&self) -> FsKind {
        FsKind::Xfs
    }

    fn describe(&self) -> String {
        let label = if self.label.is_empty() { String::new() } else { format!("\"{}\", ", self.label) };
        format!(
            "XFS{}, {label}{} blocks, {} allocation groups",
            if self.v5 { " v5" } else { "" },
            crate::units::format_size(self.block),
            self.ag_count
        )
    }

    fn source(&self) -> &Source {
        &self.src
    }

    fn cluster_size(&self) -> u64 {
        self.block
    }

    fn scan_files(&self, live: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<DeletedFile>> {
        let root = self.inode(self.root).context("reading the root directory")?;
        let mut out = Vec::new();
        // Names of deleted entries by (low half of) inode number.
        let mut deleted_names: HashMap<u64, String> = HashMap::new();
        let mut seen = HashSet::from([self.root]);
        let mut dir_paths: HashMap<u64, String> = HashMap::from([(self.root, String::new())]);
        let mut stack = vec![(root, String::new())];
        let mut done = 0u64;
        while let Some((dir, path)) = stack.pop() {
            for (ino, name, alive) in self.entries(&dir) {
                if name == "." || name == ".." {
                    continue;
                }
                let full = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
                if !alive {
                    deleted_names.entry(ino & 0xFFFF_FFFF).or_insert(full);
                    continue;
                }
                let Some(i) = self.inode(ino) else { continue };
                match i.mode() & 0o170000 {
                    0o040000 if seen.insert(ino) => {
                        dir_paths.insert(ino, full.clone());
                        stack.push((i, full));
                    }
                    0o100000 if live => {
                        let n = if self.nrext64 {
                            be64(&i.0, 24).unwrap_or(0)
                        } else {
                            u64::from(be32(&i.0, 76).unwrap_or(0))
                        };
                        let ext = self.extents(&i, Some(n as usize));
                        out.push(DeletedFile {
                            id: ino,
                            path: full,
                            size: i.size(),
                            created: self.time(&i, 144).or_else(|| self.time(&i, 48)),
                            modified: self.time(&i, 40),
                            condition: Condition::Recoverable,
                            note: None,
                            data: self.file_data(&ext, i.size()),
                        });
                    }
                    _ => {}
                }
            }
            done += 1;
            progress(done, done + stack.len() as u64);
        }

        // Deleted files: inodes that are free now, from the log's copies
        // of them, or from extent lists left in the freed inodes.
        let free = self.free();
        let all_logged = self.logged();
        // The newest copy of each file.
        let mut logged: HashMap<u64, &Logged> = HashMap::new();
        for (ino, l) in &all_logged {
            if l.mode() & 0o170000 == 0o100000 && logged.get(ino).is_none_or(|o| o.lsn <= l.lsn) {
                logged.insert(*ino, l);
            }
        }
        let live_ids: HashSet<u64> = out.iter().map(|f| f.id).collect();
        // Names: small directories as the log recorded them, every copy.
        for (dir_ino, l) in &all_logged {
            if l.mode() & 0o170000 != 0o040000 || l.core.get(5) != Some(&1) {
                continue;
            }
            let dir = dir_paths.get(dir_ino).cloned();
            for (ino, name, _) in self.shortform(&l.fork) {
                let path = match &dir {
                    Some(d) if !d.is_empty() => format!("{d}/{name}"),
                    Some(_) => name,
                    None => format!("$Orphan/{name}"),
                };
                deleted_names.entry(ino & 0xFFFF_FFFF).or_insert(path);
            }
        }
        let condition = |ext: &[(u64, u64, u64)]| {
            let (mut ok, mut total) = (0u64, 0u64);
            for &(_, at, count) in ext {
                let r = at..at + count * self.block;
                total += r.end - r.start;
                let i = free.partition_point(|f| f.end <= r.start);
                if free.get(i).is_some_and(|f| f.start <= r.start && f.end >= r.end) {
                    ok += r.end - r.start;
                }
            }
            Condition::from_counts(ok, total)
        };
        let name = |ino: u64| {
            deleted_names.get(&(ino & 0xFFFF_FFFF)).cloned().unwrap_or_else(|| format!("$Orphan/inode {ino}"))
        };
        let mut done_ids = HashSet::new();
        for (first, mask) in self.inode_chunks() {
            for k in 0..64u64 {
                if mask & (1 << k) == 0 {
                    continue;
                }
                let ino = first + k;
                if live_ids.contains(&ino) {
                    continue;
                }
                if let Some(l) = logged.get(&ino) {
                    let ext: Vec<_> = (0..l.fork.len() / 16)
                        .filter_map(|k| record(&l.fork, k * 16))
                        .map(|(o, b, c)| (o, self.fsb(b), c))
                        .collect();
                    if !ext.is_empty() {
                        let size = l.u64(56);
                        out.push(DeletedFile {
                            id: ino,
                            path: name(ino),
                            size,
                            created: l.time(144, self.bigtime).or_else(|| l.time(48, self.bigtime)),
                            modified: l.time(40, self.bigtime),
                            condition: condition(&ext),
                            note: Some("from the journal".into()),
                            data: self.file_data(&ext, size),
                        });
                        done_ids.insert(ino);
                        continue;
                    }
                }
                // Older kernels left the extent list in the freed inode.
                let Some(i) = self.inode(ino) else { continue };
                if i.mode() != 0 || i.format() != 2 {
                    continue;
                }
                let ext = self.extents(&i, None);
                if ext.is_empty() {
                    continue;
                }
                let size = ext.iter().map(|e| (e.0 + e.2) * self.block).max().unwrap_or(0);
                out.push(DeletedFile {
                    id: ino,
                    path: name(ino),
                    size,
                    created: self.time(&i, 144),
                    modified: self.time(&i, 40),
                    condition: condition(&ext),
                    note: Some("size rounded up to whole blocks; XFS keeps no size for deleted files".into()),
                    data: self.file_data(&ext, size),
                });
            }
        }
        let _ = done_ids;
        Ok(out)
    }

    fn free_ranges(&self) -> Result<Vec<ByteRange>> {
        Ok(self.free())
    }
}
