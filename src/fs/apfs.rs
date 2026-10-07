//! APFS (Apple File System, macOS 10.13 and later).
//!
//! APFS never changes its metadata in place: every change writes new copies
//! of the tree nodes it touches, and the container keeps a ring of recent
//! checkpoints, each naming the file trees as they were then. The newest
//! checkpoint gives the files that exist; the older ones still describe
//! files deleted since, with their names, folders and data locations, as
//! long as their blocks were not reused.
//!
//! Encrypted volumes (FileVault) cannot be read without their password.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, ensure};

use super::{Condition, DeletedFile, Extent, FileData, FsKind, Volume};
use crate::bytes::{le16, le32, le64};
use crate::ranges::ByteRange;
use crate::source::Source;

const NX_MAGIC: &[u8; 4] = b"NXSB";
const APSB_MAGIC: &[u8; 4] = b"APSB";
const TYPE_NX_SUPERBLOCK: u32 = 0x1;
const TYPE_BTREE: u32 = 0x2;
const TYPE_BTREE_NODE: u32 = 0x3;
const TYPE_FS: u32 = 0xD;
const BTNODE_ROOT: u16 = 0x1;
const BTNODE_FIXED: u16 = 0x4;
const J_INODE: u64 = 3;
const J_FILE_EXTENT: u64 = 8;
const J_DIR_REC: u64 = 9;
const ROOT_DIR: u64 = 2;
/// Nodes read for one file tree at most (also guards against loops).
const MAX_NODES: usize = 1 << 20;

/// Whether the first block of a source is an APFS container superblock.
pub fn detect(b: &[u8]) -> bool {
    b.get(32..36) == Some(NX_MAGIC) && le32(b, 36).is_some_and(|s| s.is_power_of_two() && (4096..=65536).contains(&s))
}

/// The Fletcher-64 checksum APFS puts at the start of every object.
fn checksum_ok(b: &[u8]) -> bool {
    let (mut s1, mut s2) = (0u64, 0u64);
    for w in b[8..].as_chunks::<4>().0 {
        s1 = (s1 + u64::from(u32::from_le_bytes(*w))) % 0xFFFF_FFFF;
        s2 = (s2 + s1) % 0xFFFF_FFFF;
    }
    let c1 = 0xFFFF_FFFF - (s1 + s2) % 0xFFFF_FFFF;
    let c2 = 0xFFFF_FFFF - (s1 + c1) % 0xFFFF_FFFF;
    le64(b, 0) == Some(c2 << 32 | c1)
}

fn obj_type(b: &[u8]) -> u32 {
    le32(b, 24).unwrap_or(0) & 0xFFFF
}

/// A file as one version of a file tree describes it.
#[derive(Debug, Clone)]
struct Inode {
    stream: u64,
    size: u64,
    created: u64,
    modified: u64,
    regular: bool,
    compressed: bool,
}

/// What one version of a volume's file tree holds.
#[derive(Default)]
struct Tree {
    inodes: HashMap<u64, Inode>,
    /// file id -> (parent id, name)
    names: HashMap<u64, (u64, String)>,
    /// stream id -> (offset in the file, length, physical block)
    extents: HashMap<u64, Vec<(u64, u64, u64)>>,
}

struct Volume1 {
    name: String,
    encrypted: bool,
    /// Newest version first.
    trees: Vec<Tree>,
}

pub struct Apfs {
    src: Source,
    block: u64,
    blocks: u64,
    volumes: Vec<Volume1>,
}

impl Apfs {
    pub fn open(src: Source) -> Result<Self> {
        let first = src.read_vec(0, 4096).context("reading the APFS container superblock")?;
        ensure!(detect(&first), "not an APFS container");
        let block = u64::from(le32(&first, 36).unwrap_or(4096));
        let blocks = le64(&first, 40).unwrap_or(0);
        let mut fs = Apfs { src, block, blocks, volumes: Vec::new() };
        // Every container superblock in the checkpoint ring, newest first.
        let base = le64(&first, 112).unwrap_or(0);
        let count = u64::from(le32(&first, 104).unwrap_or(0) & 0x7FFF_FFFF);
        let mut supers: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        for k in 0..count.min(4096) {
            if let Some(b) = fs.object(base + k)
                && obj_type(&b) == TYPE_NX_SUPERBLOCK
                && b.get(32..36) == Some(NX_MAGIC)
            {
                supers.insert(le64(&b, 16).unwrap_or(0), b);
            }
        }
        if supers.is_empty() && checksum_ok(&first) {
            supers.insert(le64(&first, 16).unwrap_or(0), first);
        }
        ensure!(!supers.is_empty(), "no valid APFS checkpoint");
        let mut by_index: BTreeMap<u32, Volume1> = BTreeMap::new();
        let mut cache = Cache::default();
        for (xid, nx) in supers.iter().rev() {
            fs.read_checkpoint(*xid, nx, &mut by_index, &mut cache);
        }
        fs.volumes = by_index.into_values().collect();
        ensure!(!fs.volumes.is_empty(), "no readable APFS volume");
        Ok(fs)
    }

    /// The object at block `b`, if its checksum is right.
    fn object(&self, b: u64) -> Option<Vec<u8>> {
        if b == 0 || (self.blocks > 0 && b >= self.blocks) {
            return None;
        }
        let v = self.src.read_vec(b * self.block, self.block as usize).ok()?;
        checksum_ok(&v).then_some(v)
    }

    /// The volumes of one checkpoint: each volume's file tree as it was.
    fn read_checkpoint(&self, xid: u64, nx: &[u8], volumes: &mut BTreeMap<u32, Volume1>, cache: &mut Cache) {
        let Some(omap) = self.omap(le64(nx, 160).unwrap_or(0), cache) else { return };
        let max = le32(nx, 180).unwrap_or(0).min(100) as usize;
        for i in 0..max {
            let oid = le64(nx, 184 + i * 8).unwrap_or(0);
            if oid == 0 {
                continue;
            }
            let Some(sb) = resolve(&omap, oid, xid).and_then(|p| self.object(p)) else { continue };
            if obj_type(&sb) != TYPE_FS || sb.get(32..36) != Some(APSB_MAGIC) {
                continue;
            }
            let index = le32(&sb, 36).unwrap_or(i as u32);
            let name_raw = sb.get(704..960).unwrap_or_default();
            let name =
                String::from_utf8_lossy(&name_raw[..name_raw.iter().position(|&c| c == 0).unwrap_or(0)]).into_owned();
            let encrypted = le64(&sb, 264).unwrap_or(0) & 1 == 0;
            let vol = volumes.entry(index).or_insert_with(|| Volume1 { name, encrypted, trees: Vec::new() });
            if encrypted {
                continue;
            }
            let vol_xid = le64(&sb, 16).unwrap_or(xid);
            let hashed = le64(&sb, 56).unwrap_or(0) & 0x9 != 0;
            let Some(vomap) = self.omap(le64(&sb, 128).unwrap_or(0), cache) else { continue };
            let mut tree = Tree::default();
            // Older trees share most nodes with newer ones (a node is never
            // changed, only replaced): each node is read once, so an older
            // tree holds just what changed since.
            let seen = cache.fs_nodes.entry(index).or_default();
            if let Some(root) = resolve(&vomap, le64(&sb, 136).unwrap_or(0), vol_xid) {
                self.fs_node(root, &vomap, vol_xid, hashed, &mut tree, seen);
            }
            if !tree.inodes.is_empty() {
                vol.trees.push(tree);
            }
        }
    }

    /// An object map: virtual object id -> (transaction, block), newest
    /// mappings included.
    fn omap(&self, block: u64, cache: &mut Cache) -> Option<HashMap<u64, Vec<(u64, u64)>>> {
        let om = self.object(block)?;
        let tree = le64(&om, 48)?;
        let mut out: HashMap<u64, Vec<(u64, u64)>> = HashMap::new();
        let mut stack = vec![tree];
        let mut seen = HashSet::new();
        while let Some(b) = stack.pop() {
            if !seen.insert(b) || seen.len() > MAX_NODES {
                continue;
            }
            // Object map nodes are shared between checkpoints too.
            let node = cache.omap_nodes.entry(b).or_insert_with(|| {
                let Some(node) = self.object(b) else { return OmapNode::Leaf(Vec::new()) };
                let leaf = le16(&node, 34) == Some(0);
                let pairs = self.entries(&node, 16, if leaf { 16 } else { 8 });
                if leaf {
                    OmapNode::Leaf(
                        pairs
                            .iter()
                            .map(|(k, v)| (le64(k, 0).unwrap_or(0), le64(k, 8).unwrap_or(0), le64(v, 8).unwrap_or(0)))
                            .collect(),
                    )
                } else {
                    OmapNode::Index(pairs.iter().filter_map(|(_, v)| le64(v, 0)).collect())
                }
            });
            match node {
                OmapNode::Leaf(entries) => {
                    for &(oid, xid, paddr) in entries.iter() {
                        out.entry(oid).or_default().push((xid, paddr));
                    }
                }
                OmapNode::Index(children) => stack.extend(children.iter().copied()),
            }
        }
        Some(out)
    }

    /// The (key, value) pairs of a B-tree node.
    fn entries<'a>(&self, node: &'a [u8], fixed_k: usize, fixed_v: usize) -> Vec<(&'a [u8], &'a [u8])> {
        let ty = obj_type(node);
        if ty != TYPE_BTREE && ty != TYPE_BTREE_NODE {
            return Vec::new();
        }
        let flags = le16(node, 32).unwrap_or(0);
        let n = le32(node, 36).unwrap_or(0) as usize;
        let (toc_off, toc_len) = (usize::from(le16(node, 40).unwrap_or(0)), usize::from(le16(node, 42).unwrap_or(0)));
        let toc = 56 + toc_off;
        let keys = toc + toc_len;
        let vals_end = node.len() - if flags & BTNODE_ROOT != 0 { 40 } else { 0 };
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let (ko, kl, vo, vl) = if flags & BTNODE_FIXED != 0 {
                let e = toc + i * 4;
                (le16(node, e), Some(fixed_k as u16), le16(node, e + 2), Some(fixed_v as u16))
            } else {
                let e = toc + i * 8;
                (le16(node, e), le16(node, e + 2), le16(node, e + 4), le16(node, e + 6))
            };
            let (Some(ko), Some(kl), Some(vo), Some(vl)) = (ko, kl, vo, vl) else { break };
            let (ko, kl, vo, vl) = (usize::from(ko), usize::from(kl), usize::from(vo), usize::from(vl));
            let (Some(k), Some(v)) =
                (node.get(keys + ko..keys + ko + kl), vals_end.checked_sub(vo).and_then(|s| node.get(s..s + vl)))
            else {
                continue;
            };
            out.push((k, v));
        }
        out
    }

    /// Walks a file tree from virtual node `oid`, collecting its records.
    fn fs_node(
        &self,
        block: u64,
        omap: &HashMap<u64, Vec<(u64, u64)>>,
        xid: u64,
        hashed: bool,
        tree: &mut Tree,
        seen: &mut HashSet<u64>,
    ) {
        if !seen.insert(block) || seen.len() > MAX_NODES {
            return;
        }
        let Some(node) = self.object(block) else { return };
        if le16(&node, 34) != Some(0) {
            for (_, v) in self.entries(&node, 0, 0) {
                if let Some(child) = le64(v, 0).and_then(|oid| resolve(omap, oid, xid)) {
                    self.fs_node(child, omap, xid, hashed, tree, seen);
                }
            }
            return;
        }
        for (k, v) in self.entries(&node, 0, 0) {
            let Some(hdr) = le64(k, 0) else { continue };
            let (id, ty) = (hdr & 0x0FFF_FFFF_FFFF_FFFF, hdr >> 60);
            match ty {
                J_INODE => {
                    if let Some(i) = parse_inode(v) {
                        tree.inodes.insert(id, i);
                    }
                }
                J_DIR_REC => {
                    let name = if hashed {
                        let len = (le32(k, 8).unwrap_or(0) & 0x3FF) as usize;
                        k.get(12..12 + len)
                    } else {
                        let len = usize::from(le16(k, 8).unwrap_or(0));
                        k.get(10..10 + len)
                    };
                    let (Some(name), Some(file)) = (name, le64(v, 0)) else { continue };
                    let name = String::from_utf8_lossy(name).trim_end_matches('\0').replace('/', ":");
                    tree.names.insert(file, (id, name));
                }
                J_FILE_EXTENT => {
                    let (Some(logical), Some(len), Some(phys)) = (le64(k, 8), le64(v, 0), le64(v, 8)) else {
                        continue;
                    };
                    tree.extents.entry(id).or_default().push((logical, len & 0x00FF_FFFF_FFFF_FFFF, phys));
                }
                _ => {}
            }
        }
    }

    fn data(&self, tree: &Tree, inode: &Inode) -> FileData {
        let Some(list) = tree.extents.get(&inode.stream) else {
            return if inode.size == 0 { FileData::Resident(Vec::new()) } else { FileData::Lost };
        };
        let mut list = list.clone();
        list.sort_unstable();
        let mut extents = Vec::new();
        let mut next = 0u64;
        for (logical, len, phys) in list {
            if logical < next {
                continue;
            }
            if logical > next {
                extents.push(Extent { offset: None, len: logical - next });
            }
            // Block 0 marks a hole.
            extents.push(Extent { offset: (phys != 0).then(|| phys * self.block), len });
            next = logical + len;
        }
        FileData::Extents(extents)
    }
}

/// What reading the checkpoints shares.
#[derive(Default)]
struct Cache {
    omap_nodes: HashMap<u64, OmapNode>,
    /// File-tree nodes already read, per volume.
    fs_nodes: HashMap<u32, HashSet<u64>>,
}

enum OmapNode {
    /// (object id, transaction, block)
    Leaf(Vec<(u64, u64, u64)>),
    Index(Vec<u64>),
}

/// The block holding virtual object `oid` as of transaction `xid`.
fn resolve(omap: &HashMap<u64, Vec<(u64, u64)>>, oid: u64, xid: u64) -> Option<u64> {
    omap.get(&oid)?.iter().filter(|(x, _)| *x <= xid).max_by_key(|(x, _)| *x).map(|(_, b)| *b)
}

fn parse_inode(v: &[u8]) -> Option<Inode> {
    let mode = le16(v, 80)?;
    let mut size = 0;
    // Extended fields: a count, then (type, flags, size) each, then the data,
    // each piece padded to 8 bytes.
    if let (Some(n), Some(_)) = (le16(v, 92), le16(v, 94)) {
        let n = usize::from(n);
        let mut data = 96 + n * 4;
        for i in 0..n {
            let (Some(&ty), Some(len)) = (v.get(96 + i * 4), le16(v, 96 + i * 4 + 2)) else { break };
            if ty == 8 {
                size = le64(v, data).unwrap_or(0); // the data stream
            }
            data += usize::from(len).div_ceil(8) * 8;
        }
    }
    let bsd = le32(v, 68).unwrap_or(0);
    Some(Inode {
        stream: le64(v, 8)?,
        created: le64(v, 16)?,
        modified: le64(v, 24)?,
        size,
        regular: mode & 0xF000 == 0x8000,
        // UF_COMPRESSED: the data lives in an extended attribute.
        compressed: bsd & 0x20 != 0,
    })
}

fn apfs_time(ns: u64) -> Option<chrono::NaiveDateTime> {
    (ns != 0)
        .then(|| chrono::DateTime::from_timestamp((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32))
        .flatten()
        .map(|d| d.naive_utc())
}

impl Volume for Apfs {
    fn kind(&self) -> FsKind {
        FsKind::Apfs
    }

    fn describe(&self) -> String {
        let names: Vec<String> = self
            .volumes
            .iter()
            .map(|v| if v.encrypted { format!("{} (encrypted)", v.name) } else { v.name.clone() })
            .collect();
        format!("APFS, {} blocks, volumes: {}", crate::units::format_size(self.block), names.join(", "))
    }

    fn source(&self) -> &Source {
        &self.src
    }

    fn cluster_size(&self) -> u64 {
        self.block
    }

    fn scan_files(&self, live: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<DeletedFile>> {
        let several = self.volumes.len() > 1;
        let mut out = Vec::new();
        for (n, vol) in self.volumes.iter().enumerate() {
            progress(n as u64, self.volumes.len() as u64);
            let Some(newest) = vol.trees.first() else { continue };
            // Names and folders from every version, the newest winning.
            let mut names: HashMap<u64, (u64, String)> = HashMap::new();
            for t in vol.trees.iter().rev() {
                names.extend(t.names.iter().map(|(k, v)| (*k, v.clone())));
            }
            let prefix = if several { format!("{}/", vol.name.replace('/', ":")) } else { String::new() };
            let path = |id: u64| {
                let mut parts = Vec::new();
                let mut at = id;
                for _ in 0..256 {
                    if at == ROOT_DIR {
                        break;
                    }
                    match names.get(&at) {
                        Some((parent, name)) => {
                            parts.push(name.clone());
                            at = *parent;
                        }
                        None => {
                            parts.push(format!("$Orphan/{at}"));
                            break;
                        }
                    }
                }
                parts.reverse();
                format!("{prefix}{}", parts.join("/"))
            };
            // Blocks the existing files use: deleted data there is gone.
            let mut used: Vec<ByteRange> = Vec::new();
            for (id, inode) in &newest.inodes {
                if !inode.regular {
                    continue;
                }
                let data = self.data(newest, inode);
                if let FileData::Extents(e) = &data {
                    used.extend(e.iter().filter_map(|x| x.offset.map(|o| o..o + x.len)));
                }
                if live {
                    out.push(DeletedFile {
                        id: *id,
                        path: path(*id),
                        size: inode.size,
                        created: apfs_time(inode.created),
                        modified: apfs_time(inode.modified),
                        condition: Condition::Recoverable,
                        note: inode.compressed.then(|| "compressed by macOS: may not open".to_string()),
                        data,
                    });
                }
            }
            let used = crate::ranges::normalize(used);
            // Deleted: in an older version only.
            let mut done = HashSet::new();
            for tree in vol.trees.iter().skip(1) {
                for (id, inode) in &tree.inodes {
                    if !inode.regular || newest.inodes.contains_key(id) || !done.insert(*id) {
                        continue;
                    }
                    let data = self.data(tree, inode);
                    let condition = match &data {
                        FileData::Lost => Condition::Overwritten,
                        FileData::Extents(e) => {
                            let (mut total, mut taken) = (0u64, 0u64);
                            for x in e.iter().filter_map(|x| x.offset.map(|o| o..o + x.len)) {
                                total += x.end - x.start;
                                for u in &used {
                                    if u.start < x.end && x.start < u.end {
                                        taken += x.end.min(u.end) - x.start.max(u.start);
                                    }
                                }
                            }
                            Condition::from_counts((total - taken) / self.block, total / self.block)
                        }
                        _ => Condition::Recoverable,
                    };
                    out.push(DeletedFile {
                        id: *id,
                        path: path(*id),
                        size: inode.size,
                        created: apfs_time(inode.created),
                        modified: apfs_time(inode.modified),
                        condition,
                        note: Some("from an earlier checkpoint".into()),
                        data,
                    });
                }
            }
        }
        Ok(out)
    }

    fn free_ranges(&self) -> Result<Vec<ByteRange>> {
        anyhow::bail!("the APFS space manager is not read: the whole container is searched")
    }
}
