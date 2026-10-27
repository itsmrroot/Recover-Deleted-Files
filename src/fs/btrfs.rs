//! btrfs (Synology and some other NAS drives, Fedora, openSUSE).
//!
//! Everything is a B-tree of keyed items, stored in metadata blocks
//! ("nodes") at logical addresses that the chunk tree maps to the disk. A
//! file is an inode item, inode references (its names) and extent items
//! (where its data is, possibly compressed or stored inline).
//!
//! btrfs never overwrites metadata in place: every change writes new
//! nodes. The nodes from before a file was deleted stay on the disk until
//! their space is reused, so the metadata areas are read node by node, and
//! files that older nodes describe but the current trees do not are the
//! deleted ones. Data extents are never changed in place either, so one
//! still recorded in the extent tree is intact.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use chrono::NaiveDateTime;

use super::compress::{self, Compression};
use super::{Condition, DeletedFile, Extent, FileData, FsKind, Packed, Volume};
use crate::bytes::{le16, le32, le64};
use crate::ranges::{ByteRange, normalize, subtract};
use crate::source::{ReadAt, Source};

const SUPER: u64 = 0x10000;
const MAGIC: &[u8; 8] = b"_BHRfS_M";

// Item types.
const INODE_ITEM: u8 = 1;
const INODE_REF: u8 = 12;
const EXTENT_DATA: u8 = 108;
const ROOT_ITEM: u8 = 132;
const ROOT_REF: u8 = 156;
const EXTENT_ITEM: u8 = 168;
const CHUNK_ITEM: u8 = 228;

const FS_TREE: u64 = 5;
const EXTENT_TREE: u64 = 2;
const FIRST_FREE: u64 = 256;
const LAST_FREE: u64 = u64::MAX - 256;
const ROOT_DIR: u64 = 256;

// Block group types.
const DATA: u64 = 1;
const SYSTEM: u64 = 2;
const METADATA: u64 = 4;

pub fn detect(src: &dyn ReadAt) -> bool {
    let mut m = [0u8; 8];
    src.read_exact_at(SUPER + 0x40, &mut m).is_ok() && &m == MAGIC
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Key {
    objectid: u64,
    ty: u8,
    offset: u64,
}

fn key(b: &[u8], at: usize) -> Option<Key> {
    Some(Key { objectid: le64(b, at)?, ty: *b.get(at + 8)?, offset: le64(b, at + 9)? })
}

#[derive(Debug, Clone, Copy)]
struct Chunk {
    logical: u64,
    length: u64,
    physical: u64,
    kind: u64,
}

pub struct Btrfs {
    src: Source,
    nodesize: u64,
    sectorsize: u64,
    fsid: [u8; 16],
    csum_crc32c: bool,
    label: String,
    total: u64,
    chunks: Vec<Chunk>,
    root_tree: (u64, u8),
}

/// What the trees say about one file system tree (a subvolume).
#[derive(Default)]
struct Items {
    /// ino → (generation of the item, its data), per item type.
    inodes: HashMap<u64, (u64, Vec<u8>)>,
    refs: HashMap<u64, Vec<(u64, String)>>,
    extents: HashMap<u64, BTreeMap<u64, (u64, Vec<u8>)>>,
}

impl Items {
    fn add(&mut self, k: Key, generation: u64, data: &[u8]) {
        match k.ty {
            INODE_ITEM => {
                let e = self.inodes.entry(k.objectid).or_insert((0, Vec::new()));
                if generation >= e.0 {
                    *e = (generation, data.to_vec());
                }
            }
            INODE_REF => {
                // index u64, name length u16, name (possibly several).
                let mut at = 0usize;
                while at + 10 <= data.len() {
                    let len = usize::from(le16(data, at + 8).unwrap_or(0));
                    let Some(name) = data.get(at + 10..at + 10 + len) else { break };
                    let entry = (k.offset, String::from_utf8_lossy(name).to_string());
                    let refs = self.refs.entry(k.objectid).or_default();
                    if !refs.contains(&entry) {
                        refs.push(entry);
                    }
                    at += 10 + len;
                }
            }
            EXTENT_DATA => {
                let e = self.extents.entry(k.objectid).or_default().entry(k.offset).or_insert((0, Vec::new()));
                if generation >= e.0 {
                    *e = (generation, data.to_vec());
                }
            }
            _ => {}
        }
    }
}

impl Btrfs {
    pub fn open(src: Source) -> Result<Self> {
        let sb = src.read_vec(SUPER, 4096).context("reading the btrfs superblock")?;
        anyhow::ensure!(&sb[0x40..0x48] == MAGIC, "not btrfs");
        let num_devices = le64(&sb, 0x88).unwrap_or(1);
        if num_devices > 1 {
            bail!("this btrfs volume spans {num_devices} drives; only single-drive volumes can be read");
        }
        let nodesize = u64::from(le32(&sb, 0x94).unwrap_or(16384));
        let sectorsize = u64::from(le32(&sb, 0x90).unwrap_or(4096));
        anyhow::ensure!((4096..=65536).contains(&nodesize), "damaged btrfs superblock");
        let label = String::from_utf8_lossy(&sb[0x12B..0x22B]).trim_end_matches('\0').to_string();
        let mut fs = Self {
            nodesize,
            sectorsize,
            fsid: sb[0x20..0x30].try_into()?,
            csum_crc32c: le16(&sb, 0xC4) == Some(0),
            label,
            total: le64(&sb, 0x70).unwrap_or(0),
            chunks: Vec::new(),
            root_tree: (le64(&sb, 0x50).unwrap_or(0), sb[0xC6]),
            src,
        };
        // The chunks needed to read the chunk tree are in the superblock.
        let n = le32(&sb, 0xA0).unwrap_or(0) as usize;
        let arr = sb.get(0x32B..0x32B + n.min(2048)).unwrap_or_default();
        let mut at = 0usize;
        while at + 17 + 48 <= arr.len() {
            let k = key(arr, at).context("damaged btrfs chunk array")?;
            let stripes = usize::from(le16(arr, at + 17 + 44).unwrap_or(0));
            if k.ty == CHUNK_ITEM {
                fs.add_chunk(k.offset, &arr[at + 17..]);
            }
            at += 17 + 48 + stripes * 32;
        }
        // Then every chunk, from the chunk tree.
        let chunk_root = (le64(&sb, 0x58).unwrap_or(0), sb[0xC7]);
        let mut found = Vec::new();
        fs.walk(chunk_root.0, chunk_root.1, &mut |k, _, d| {
            if k.ty == CHUNK_ITEM {
                found.push((k.offset, d.to_vec()));
            }
        });
        for (logical, d) in found {
            fs.add_chunk(logical, &d);
        }
        anyhow::ensure!(!fs.chunks.is_empty(), "damaged btrfs: no chunks");
        Ok(fs)
    }

    fn add_chunk(&mut self, logical: u64, c: &[u8]) {
        let (Some(length), Some(kind), Some(physical)) = (le64(c, 0), le64(c, 24), le64(c, 48 + 8)) else { return };
        if !self.chunks.iter().any(|x| x.logical == logical) {
            self.chunks.push(Chunk { logical, length, physical, kind });
            self.chunks.sort_by_key(|c| c.logical);
        }
    }

    fn physical(&self, logical: u64) -> Option<u64> {
        let c = self.chunks.iter().find(|c| (c.logical..c.logical + c.length).contains(&logical))?;
        Some(c.physical + (logical - c.logical))
    }

    /// The node at `logical`, if it is a valid node of this file system.
    fn node(&self, logical: u64) -> Option<Vec<u8>> {
        let at = self.physical(logical)?;
        let n = self.src.read_vec(at, self.nodesize as usize).ok()?;
        self.valid(&n, logical).then_some(n)
    }

    fn valid(&self, n: &[u8], logical: u64) -> bool {
        if n.get(32..48) != Some(&self.fsid[..]) || le64(n, 48) != Some(logical) {
            return false;
        }
        !self.csum_crc32c || le32(n, 0) == Some(crate::container::crc32c(&n[32..]))
    }

    /// Calls `f(key, generation, data)` for every item of the tree at
    /// `root` (a logical address).
    fn walk(&self, root: u64, level: u8, f: &mut dyn FnMut(Key, u64, &[u8])) {
        let mut stack = vec![(root, level)];
        let mut seen = HashSet::new();
        while let Some((at, _)) = stack.pop() {
            if !seen.insert(at) || seen.len() > 1_000_000 {
                continue;
            }
            let Some(n) = self.node(at) else { continue };
            leaf_items(&n, f, &mut |child| stack.push((child, 0)));
        }
    }

    /// The file system trees (the top one and each subvolume): id, root
    /// node, and the subvolume's path.
    fn trees(&self) -> Vec<(u64, u64, u8, String)> {
        let mut roots = Vec::new();
        let mut names: HashMap<u64, (u64, String)> = HashMap::new();
        self.walk(self.root_tree.0, self.root_tree.1, &mut |k, _, d| match k.ty {
            ROOT_ITEM if k.objectid == FS_TREE || (FIRST_FREE..=LAST_FREE).contains(&k.objectid) => {
                if let (Some(at), Some(&level)) = (le64(d, 176), d.get(238)) {
                    roots.push((k.objectid, at, level));
                }
            }
            ROOT_REF => {
                // dirid, sequence, name length, name.
                let len = usize::from(le16(d, 16).unwrap_or(0));
                if let Some(name) = d.get(18..18 + len) {
                    names.insert(k.offset, (k.objectid, String::from_utf8_lossy(name).to_string()));
                }
            }
            _ => {}
        });
        roots.sort_by_key(|r| r.0);
        roots.dedup_by_key(|r| r.0);
        roots
            .into_iter()
            .map(|(id, at, level)| {
                let mut path = Vec::new();
                let mut cur = id;
                while let Some((parent, name)) = names.get(&cur) {
                    path.push(name.clone());
                    cur = *parent;
                    if path.len() > 64 {
                        break;
                    }
                }
                path.reverse();
                (id, at, level, path.join("/"))
            })
            .collect()
    }

    /// Items of every file system tree found in old nodes, by tree.
    fn old_items(&self, trees: &HashSet<u64>, progress: &mut dyn FnMut(u64, u64)) -> HashMap<u64, Items> {
        let mut out: HashMap<u64, Items> = HashMap::new();
        let meta: Vec<&Chunk> = self.chunks.iter().filter(|c| c.kind & (METADATA | SYSTEM) != 0).collect();
        let total: u64 = meta.iter().map(|c| c.length).sum();
        let mut done = 0u64;
        for c in meta {
            let mut off = 0u64;
            // Read a chunk in pieces of many nodes.
            let piece = self.nodesize * 64;
            while off < c.length {
                let len = piece.min(c.length - off);
                let Ok(buf) = self.src.read_vec(c.physical + off, len as usize) else {
                    off += len;
                    continue;
                };
                for (i, n) in buf.chunks_exact(self.nodesize as usize).enumerate() {
                    let logical = c.logical + off + i as u64 * self.nodesize;
                    // Leaves of file system trees only.
                    if n.get(100) != Some(&0) || !trees.contains(&le64(n, 88).unwrap_or(0)) || !self.valid(n, logical) {
                        continue;
                    }
                    let owner = le64(n, 88).unwrap_or(0);
                    let items = out.entry(owner).or_default();
                    leaf_items(n, &mut |k, g, d| items.add(k, g, d), &mut |_| {});
                }
                off += len;
                done += len;
                progress(done, total);
            }
        }
        out
    }

    /// Data extents in use: (logical start, length).
    fn allocated(&self) -> Vec<(u64, u64)> {
        let mut extent_root = None;
        self.walk(self.root_tree.0, self.root_tree.1, &mut |k, _, d| {
            if k.ty == ROOT_ITEM && k.objectid == EXTENT_TREE {
                extent_root = le64(d, 176).zip(d.get(238).copied());
            }
        });
        let mut out = Vec::new();
        if let Some((at, level)) = extent_root {
            self.walk(at, level, &mut |k, _, _| {
                if k.ty == EXTENT_ITEM {
                    out.push((k.objectid, k.offset));
                }
            });
        }
        out.sort_unstable();
        out
    }

    /// A file's data from its extent items (file offset → item).
    fn data(&self, items: &BTreeMap<u64, (u64, Vec<u8>)>, size: u64) -> Option<FileData> {
        let mut plain: Vec<Extent> = Vec::new();
        let mut packed: Vec<Packed> = Vec::new();
        let mut pos = 0u64;
        let mut compressed = false;
        for (&file_off, (_, d)) in items {
            if file_off >= size {
                break;
            }
            if file_off > pos {
                plain.push(Extent { offset: None, len: file_off - pos });
                packed.push(Packed {
                    offset: None,
                    stored: 0,
                    method: Compression::None,
                    skip: 0,
                    len: file_off - pos,
                });
            } else if file_off < pos {
                continue;
            }
            let method = match d.get(16).copied().unwrap_or(0) {
                0 => Compression::None,
                1 => Compression::Zlib,
                2 => Compression::Lzo,
                3 => Compression::Zstd,
                _ => return None,
            };
            if d.get(17).copied().unwrap_or(0) != 0 {
                return None; // encrypted
            }
            match d.get(20).copied().unwrap_or(0) {
                // Inline: the data is in the item (small files).
                0 => {
                    let ram = le64(d, 8).unwrap_or(0) as usize;
                    let bytes = compress::decompress(method, d.get(21..)?, ram).ok()?;
                    let mut v = bytes;
                    v.truncate((size - file_off) as usize);
                    if items.len() == 1 && file_off == 0 {
                        return Some(FileData::Resident(v));
                    }
                    return None;
                }
                1 | 2 => {
                    let disk = le64(d, 21).unwrap_or(0);
                    let stored = le64(d, 29).unwrap_or(0);
                    let skip = le64(d, 37).unwrap_or(0);
                    let len = le64(d, 45).unwrap_or(0).min(size - file_off);
                    let prealloc = d[20] == 2;
                    if disk == 0 || prealloc {
                        plain.push(Extent { offset: None, len });
                        packed.push(Packed { offset: None, stored: 0, method: Compression::None, skip: 0, len });
                    } else {
                        let at = self.physical(disk)?;
                        compressed |= method != Compression::None;
                        plain.push(Extent { offset: Some(at + skip), len });
                        let p = if method == Compression::None {
                            Packed { offset: Some(at + skip), stored: len, method, skip: 0, len }
                        } else {
                            Packed { offset: Some(at), stored, method, skip, len }
                        };
                        packed.push(p);
                    }
                    pos = file_off + len;
                }
                _ => return None,
            }
        }
        if pos < size {
            plain.push(Extent { offset: None, len: size - pos });
            packed.push(Packed { offset: None, stored: 0, method: Compression::None, skip: 0, len: size - pos });
        }
        Some(if compressed { FileData::Packed(packed) } else { FileData::Extents(plain) })
    }
}

/// Calls `item(key, generation, data)` for each item of a leaf, or
/// `child(logical)` for each child of an internal node.
fn leaf_items(n: &[u8], item: &mut dyn FnMut(Key, u64, &[u8]), child: &mut dyn FnMut(u64)) {
    let count = le32(n, 96).unwrap_or(0) as usize;
    let level = n.get(100).copied().unwrap_or(0);
    let generation = le64(n, 80).unwrap_or(0);
    for i in 0..count.min(n.len() / 25) {
        if level == 0 {
            let at = 101 + i * 25;
            let (Some(k), Some(off), Some(size)) = (key(n, at), le32(n, at + 17), le32(n, at + 21)) else { break };
            let (s, e) = (101 + off as usize, 101 + off as usize + size as usize);
            if let Some(d) = n.get(s..e) {
                item(k, generation, d);
            }
        } else {
            let at = 101 + i * 33;
            if let Some(ptr) = le64(n, at + 17) {
                child(ptr);
            }
        }
    }
}

fn time(inode: &[u8], at: usize) -> Option<NaiveDateTime> {
    let secs = le64(inode, at)? as i64;
    let nanos = le32(inode, at + 8)?;
    (secs > 0).then(|| chrono::DateTime::from_timestamp(secs, nanos).map(|d| d.naive_utc())).flatten()
}

fn is_file(inode: &[u8]) -> bool {
    le32(inode, 52).is_some_and(|m| m & 0o170000 == 0o100000)
}

/// The path of `ino` from its names, in `items` and then `older`.
fn path_of(ino: u64, items: &Items, older: Option<&Items>) -> Option<String> {
    let mut parts = Vec::new();
    let mut cur = ino;
    for _ in 0..256 {
        if cur == ROOT_DIR {
            parts.reverse();
            return Some(parts.join("/"));
        }
        let refs = items.refs.get(&cur).or_else(|| older.and_then(|o| o.refs.get(&cur)))?;
        let (parent, name) = refs.first()?;
        parts.push(name.clone());
        cur = *parent;
    }
    None
}

impl Volume for Btrfs {
    fn kind(&self) -> FsKind {
        FsKind::Btrfs
    }

    fn describe(&self) -> String {
        let label = if self.label.is_empty() { String::new() } else { format!("\"{}\", ", self.label) };
        format!(
            "btrfs, {label}{} nodes, {} sectors",
            crate::units::format_size(self.nodesize),
            crate::units::format_size(self.sectorsize)
        )
    }

    fn source(&self) -> &Source {
        &self.src
    }

    fn cluster_size(&self) -> u64 {
        self.sectorsize
    }

    fn scan_files(&self, live: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<DeletedFile>> {
        let trees = self.trees();
        anyhow::ensure!(!trees.is_empty(), "damaged btrfs: no file system tree");
        let ids: HashSet<u64> = trees.iter().map(|t| t.0).collect();
        let mut current: HashMap<u64, Items> = HashMap::new();
        for &(id, at, level, _) in &trees {
            let items = current.entry(id).or_default();
            self.walk(at, level, &mut |k, g, d| items.add(k, g, d));
        }
        let old = self.old_items(&ids, progress);
        let allocated = self.allocated();
        let mut out = Vec::new();
        let empty = Items::default();
        for (id, _, _, sub) in &trees {
            let now = current.get(id).unwrap_or(&empty);
            let prefix = |p: String| if sub.is_empty() { p } else { format!("{sub}/{p}") };
            let file = |ino: u64, inode: &[u8], extents: Option<&BTreeMap<u64, (u64, Vec<u8>)>>, path: String| {
                let size = le64(inode, 16).unwrap_or(0);
                let data = match extents {
                    Some(e) => self.data(e, size)?,
                    None if size == 0 => FileData::Resident(Vec::new()),
                    None => return None,
                };
                Some(DeletedFile {
                    id: ino,
                    path,
                    size,
                    created: time(inode, 148),
                    modified: time(inode, 136),
                    condition: Condition::Recoverable,
                    note: None,
                    data,
                })
            };
            if live {
                for (&ino, (_, inode)) in &now.inodes {
                    if !is_file(inode) {
                        continue;
                    }
                    let Some(path) = path_of(ino, now, None) else { continue };
                    if let Some(f) = file(ino, inode, now.extents.get(&ino), prefix(path)) {
                        out.push(f);
                    }
                }
            }
            let Some(old) = old.get(id) else { continue };
            for (&ino, (_, inode)) in &old.inodes {
                if now.inodes.contains_key(&ino) || !is_file(inode) {
                    continue;
                }
                let path = match path_of(ino, old, Some(now)) {
                    Some(p) => prefix(p),
                    None => {
                        let name = old.refs.get(&ino).and_then(|r| r.first()).map(|r| r.1.clone());
                        format!("$Orphan/{}", name.unwrap_or_else(|| format!("inode {ino}")))
                    }
                };
                let Some(mut f) = file(ino, inode, old.extents.get(&ino), path) else { continue };
                f.condition = condition(old.extents.get(&ino), &allocated);
                f.note = Some("from older copies of the file system's metadata".into());
                out.push(f);
            }
        }
        Ok(out)
    }

    fn free_ranges(&self) -> Result<Vec<ByteRange>> {
        // Unused space: outside every chunk, and in data chunks between
        // the extents in use.
        let size = self.src.size().min(self.total.max(self.src.size()));
        let chunks: Vec<ByteRange> = self.chunks.iter().map(|c| c.physical..c.physical + c.length).collect();
        let mut free = subtract(std::slice::from_ref(&(0..size)), &normalize(chunks));
        let used: Vec<ByteRange> =
            self.allocated().into_iter().filter_map(|(l, len)| self.physical(l).map(|p| p..p + len)).collect();
        let data: Vec<ByteRange> =
            self.chunks.iter().filter(|c| c.kind & DATA != 0).map(|c| c.physical..c.physical + c.length).collect();
        free.extend(subtract(&normalize(data), &normalize(used)));
        Ok(normalize(free))
    }
}

/// A deleted file's condition: intact if each of its data extents is
/// still recorded as it was (data extents are never rewritten), likely
/// overwritten if other extents now use its space.
fn condition(extents: Option<&BTreeMap<u64, (u64, Vec<u8>)>>, allocated: &[(u64, u64)]) -> Condition {
    let Some(extents) = extents else { return Condition::Recoverable };
    let (mut total, mut ok) = (0u64, 0u64);
    for (_, d) in extents.values() {
        if d.get(20) == Some(&0) {
            continue; // inline: kept in the old node itself
        }
        let (Some(disk), Some(len)) = (le64(d, 21), le64(d, 29)) else { continue };
        if disk == 0 {
            continue;
        }
        total += len;
        let i = allocated.partition_point(|a| a.0 + a.1 <= disk);
        let same = allocated.get(i).is_some_and(|a| a.0 == disk && a.1 == len);
        let overlaps = allocated.get(i).is_some_and(|a| a.0 < disk + len);
        if same || !overlaps {
            ok += len;
        }
    }
    Condition::from_counts(ok, total)
}
