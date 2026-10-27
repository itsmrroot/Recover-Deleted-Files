//! RAID: Linux software RAID (md, used by Synology, QNAP and most NAS
//! drives) found from its superblocks, and hand-described hardware RAID.
//!
//! Each md member carries a superblock (version 0.90 at the end, 1.0 at
//! the end, 1.1 at the start, 1.2 at 4 KiB) giving the array's UUID, level,
//! chunk size, layout, and this member's position. The arrays are put back
//! together by reading the members as the md driver would: linear,
//! RAID 0, 1, 4, 5, 6 and 10 (near copies). RAID 5 (and 6 with one disk
//! gone) is read through its parity when a member is missing.

use std::io;
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::bytes::{le32, le64};
use crate::source::{ReadAt, Source};

const MAGIC: u32 = 0xA92B_4EFC;

/// What a member's superblock says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Super {
    pub uuid: [u8; 16],
    /// The array's name (`hostname:name` for v1, empty for v0.90).
    pub name: String,
    /// -1 linear, 0, 1, 4, 5, 6, 10.
    pub level: i32,
    pub layout: u32,
    /// Bytes.
    pub chunk: u64,
    pub disks: u32,
    /// This member's position; `None` for a spare or a failed member.
    pub role: Option<u32>,
    /// Where the data starts on the member, and how much of it is used.
    pub data_offset: u64,
    pub data_size: u64,
    pub events: u64,
}

/// Reads the md superblock of `src`, if it is an array member.
pub fn superblock(src: &dyn ReadAt) -> Option<Super> {
    let size = src.size();
    let sectors = size / 512;
    // v1.1, v1.2, then v1.0 at the end.
    let mut places = vec![0u64, 4096];
    if sectors > 16 {
        places.push(((sectors - 16) & !7) * 512);
    }
    for at in places {
        if let Some(s) = v1(src, at) {
            return Some(s);
        }
    }
    // v0.90: 64 KiB from the end, aligned to 64 KiB.
    if sectors >= 128 {
        let at = ((sectors & !127) - 128) * 512;
        return v090(src, at);
    }
    None
}

fn v1(src: &dyn ReadAt, at: u64) -> Option<Super> {
    let b = src.read_vec(at, 1024).ok()?;
    if le32(&b, 0)? != MAGIC || le32(&b, 4)? != 1 {
        return None;
    }
    let raw_level = le32(&b, 72)? as i32;
    let disks = le32(&b, 92)?;
    let data_offset = le64(&b, 128)? * 512;
    let data_size = le64(&b, 136)? * 512;
    let used = le64(&b, 80)? * 512;
    let dev_number = le32(&b, 160)? as usize;
    let role =
        crate::bytes::le16(&b, 256 + dev_number * 2).filter(|&r| r < 0xFFFE && u32::from(r) < disks).map(u32::from);
    let name = String::from_utf8_lossy(&b[32..64]).trim_end_matches('\0').to_string();
    Some(Super {
        uuid: b[16..32].try_into().ok()?,
        name,
        level: raw_level,
        layout: le32(&b, 76)?,
        chunk: u64::from(le32(&b, 88)?) * 512,
        disks,
        role,
        data_offset,
        data_size: if used > 0 { used } else { data_size },
        events: le64(&b, 200)?,
    })
}

fn v090(src: &dyn ReadAt, at: u64) -> Option<Super> {
    let b = src.read_vec(at, 4096).ok()?;
    if le32(&b, 0)? != MAGIC || le32(&b, 4)? != 0 {
        return None;
    }
    let mut uuid = [0u8; 16];
    for (i, off) in [20usize, 52, 56, 60].iter().enumerate() {
        uuid[i * 4..i * 4 + 4].copy_from_slice(&b[*off..off + 4]);
    }
    let disks = le32(&b, 40)?;
    // this_disk: number, major, minor, raid_disk, state.
    let raid_disk = le32(&b, 3968 + 12)?;
    let state = le32(&b, 3968 + 16)?;
    let faulty = state & 1 != 0;
    let events = (u64::from(le32(&b, 128 + 7 * 4)?) << 32) | u64::from(le32(&b, 128 + 8 * 4)?);
    Some(Super {
        uuid,
        name: String::new(),
        level: le32(&b, 28)? as i32,
        layout: le32(&b, 256)?,
        chunk: u64::from(le32(&b, 260)?),
        disks,
        role: (!faulty && raid_disk < disks).then_some(raid_disk),
        data_offset: 0,
        data_size: u64::from(le32(&b, 32)?) * 1024,
        events,
    })
}

// ----------------------------------------------------------------------
// Reading an array

/// How RAID 5/6 place their parity (md's names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    LeftAsymmetric,
    RightAsymmetric,
    LeftSymmetric,
    RightSymmetric,
    /// RAID 4: parity on the last disk.
    Parity0,
}

impl Layout {
    fn md(code: u32) -> Self {
        match code {
            0 => Layout::LeftAsymmetric,
            1 => Layout::RightAsymmetric,
            3 => Layout::RightSymmetric,
            4 | 5 => Layout::Parity0,
            _ => Layout::LeftSymmetric,
        }
    }
}

/// An array's description: enough to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Geometry {
    pub level: i32,
    pub chunk: u64,
    pub layout: Layout,
    /// RAID 10: copies next to each other.
    pub near: u32,
}

/// One member: its source and where its data starts.
#[derive(Clone)]
pub struct Disk {
    pub src: Source,
    pub data_offset: u64,
}

/// An array read as one disk.
pub struct Array {
    g: Geometry,
    /// By position; `None` where a member is missing.
    disks: Vec<Option<Disk>>,
    /// Bytes used on each member.
    per_disk: u64,
    size: u64,
}

impl Array {
    pub fn new(g: Geometry, disks: Vec<Option<Disk>>, per_disk: u64) -> Result<Self> {
        let n = disks.len() as u64;
        anyhow::ensure!(n > 0, "no disks");
        let missing = disks.iter().filter(|d| d.is_none()).count() as u64;
        let chunk = g.chunk.max(512);
        let per_disk_chunks = per_disk / chunk * chunk;
        let size = match g.level {
            -1 => bail!("use Array::linear"),
            0 => per_disk_chunks * n,
            1 => per_disk,
            4 | 5 => per_disk_chunks * (n - 1),
            6 => per_disk_chunks * (n - 2),
            10 => per_disk_chunks * n / u64::from(g.near.max(1)),
            l => bail!("RAID level {l} is not supported"),
        };
        let tolerated = match g.level {
            1 => n - 1,
            4..=6 => 1,
            10 => u64::from(g.near.max(1)) - 1,
            _ => 0,
        };
        if missing > tolerated {
            bail!("{missing} of the {n} disks of this RAID {} are missing; it cannot be read", g.level);
        }
        Ok(Self { g, disks, per_disk, size })
    }

    fn disk(&self, i: usize) -> Option<&Disk> {
        self.disks.get(i).and_then(Option::as_ref)
    }

    fn read_disk(&self, i: usize, at: u64, buf: &mut [u8]) -> io::Result<bool> {
        match self.disk(i) {
            Some(d) => d.src.read_exact_at(d.data_offset + at, buf).map(|_| true),
            None => Ok(false),
        }
    }

    /// RAID 4/5/6: the disks holding parity (P, and Q for RAID 6) and the
    /// disk of data chunk `d` in stripe `s`.
    fn place(&self, s: u64, d: u64) -> (usize, Option<usize>, usize) {
        let n = self.disks.len() as u64;
        let raid6 = self.g.level == 6;
        let pd = match self.g.layout {
            Layout::LeftAsymmetric | Layout::LeftSymmetric => n - 1 - s % n,
            Layout::RightAsymmetric | Layout::RightSymmetric => s % n,
            Layout::Parity0 => n - 1 - u64::from(raid6),
        };
        let qd = raid6.then(|| match self.g.layout {
            Layout::Parity0 => n - 1,
            Layout::LeftAsymmetric | Layout::RightAsymmetric => {
                if pd == n - 1 {
                    0
                } else {
                    pd + 1
                }
            }
            _ => (pd + 1) % n,
        });
        let parity = 1 + u64::from(raid6);
        let dd = match self.g.layout {
            Layout::LeftSymmetric | Layout::RightSymmetric => (pd + parity + d) % n,
            Layout::Parity0 => d,
            _ => {
                // Asymmetric: data in disk order, skipping the parity disks.
                let mut dd = d;
                if raid6 && pd == n - 1 {
                    dd += 1; // Q is on disk 0
                } else if dd >= pd {
                    dd += parity;
                }
                dd
            }
        };
        (pd as usize, qd.map(|q| q as usize), dd as usize)
    }

    fn read_chunked(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let chunk = self.g.chunk;
        let n = self.disks.len() as u64;
        let c = offset / chunk;
        let within = offset % chunk;
        let len = (buf.len() as u64).min(chunk - within) as usize;
        let buf = &mut buf[..len];
        match self.g.level {
            0 => {
                let (dev, row) = ((c % n) as usize, c / n);
                if !self.read_disk(dev, row * chunk + within, buf)? {
                    return Err(missing());
                }
            }
            10 => {
                let near = u64::from(self.g.near.max(1));
                let v = c * near;
                let mut done = false;
                for k in 0..near {
                    let (dev, row) = (((v + k) % n) as usize, (v + k) / n);
                    if self.read_disk(dev, row * chunk + within, buf)? {
                        done = true;
                        break;
                    }
                }
                if !done {
                    return Err(missing());
                }
            }
            _ => {
                let data = n - 1 - u64::from(self.g.level == 6);
                let (s, d) = (c / data, c % data);
                let (pd, qd, dd) = self.place(s, d);
                let at = s * chunk + within;
                if !self.read_disk(dd, at, buf)? {
                    // XOR of everything else in the stripe, P included.
                    buf.fill(0);
                    let mut other = vec![0u8; len];
                    for i in 0..n as usize {
                        if i == dd || Some(i) == qd {
                            continue;
                        }
                        if !self.read_disk(i, at, &mut other)? {
                            return Err(missing());
                        }
                        buf.iter_mut().zip(&other).for_each(|(b, o)| *b ^= o);
                    }
                    let _ = pd;
                }
            }
        }
        Ok(len)
    }
}

fn missing() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "this part of the RAID is on a missing disk")
}

impl ReadAt for Array {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let max = (buf.len() as u64).min(self.size - offset) as usize;
        let buf = &mut buf[..max];
        if self.g.level == 1 {
            for i in 0..self.disks.len() {
                if self.read_disk(i, offset, buf)? {
                    return Ok(max);
                }
            }
            return Err(missing());
        }
        let _ = self.per_disk;
        self.read_chunked(offset, buf)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// Members one after another (md "linear", or JBOD).
pub fn linear(disks: Vec<(Disk, u64)>) -> Source {
    let mut extents = Vec::new();
    let mut at = 0u64;
    for (d, len) in disks {
        extents.push((at, Arc::new(crate::source::SubSource::new(d.src, d.data_offset, len)) as Source));
        at += len;
    }
    Arc::new(crate::source::Concat::new(extents))
}

/// An md array put together from its members: (array name, level, member
/// count found / expected, the array).
pub struct Assembled {
    pub name: String,
    pub level: i32,
    pub found: usize,
    pub disks: usize,
    pub source: Source,
}

type Members = Vec<(Source, Super)>;

/// Groups md members by array and assembles each array that can be read.
pub fn assemble(members: Vec<(Source, Super)>) -> Vec<Result<Assembled>> {
    let mut groups: Vec<([u8; 16], Members)> = Vec::new();
    for (src, sb) in members {
        match groups.iter_mut().find(|g| g.0 == sb.uuid) {
            Some(g) => g.1.push((src, sb)),
            None => groups.push((sb.uuid, vec![(src, sb)])),
        }
    }
    groups
        .into_iter()
        .map(|(_, members)| {
            // The newest copy of each position (stale members of a
            // replaced disk are ignored).
            let newest = members.iter().map(|(_, s)| s.events).max().unwrap_or(0);
            let first = members[0].1.clone();
            let n = first.disks as usize;
            let mut disks: Vec<Option<(Disk, u64)>> = vec![None; n];
            for (src, sb) in &members {
                let Some(role) = sb.role else { continue };
                if sb.events + 1 < newest {
                    continue;
                }
                if let Some(slot) = disks.get_mut(role as usize)
                    && slot.is_none()
                {
                    *slot = Some((Disk { src: src.clone(), data_offset: sb.data_offset }, sb.data_size));
                }
            }
            let found = disks.iter().filter(|d| d.is_some()).count();
            let source: Source = if first.level == -1 {
                let all: Option<Vec<(Disk, u64)>> = disks.into_iter().collect();
                let Some(all) = all else { bail!("a member of this linear array is missing") };
                linear(all)
            } else {
                let per_disk = disks.iter().flatten().map(|d| d.1).min().unwrap_or(0);
                let near = if first.level == 10 { first.layout & 0xFF } else { 0 };
                if first.level == 10 && (first.layout >> 8) & 0xFF > 1 {
                    bail!("RAID 10 with far copies is not supported");
                }
                // RAID 4 keeps its parity on the last disk whatever the layout says.
                let layout = if first.level == 4 { Layout::Parity0 } else { Layout::md(first.layout) };
                let g = Geometry { level: first.level, chunk: first.chunk, layout, near };
                Arc::new(Array::new(g, disks.into_iter().map(|d| d.map(|d| d.0)).collect(), per_disk)?)
            };
            let name = first.name.rsplit(':').next().unwrap_or_default().to_string();
            Ok(Assembled { name, level: first.level, found, disks: n, source })
        })
        .collect()
}

// ----------------------------------------------------------------------
// Several drives as one source: `raid:…` paths

/// Disks given for a hand-described array: a path, or `missing`.
const MISSING: &str = "missing";

/// What a `raid:` path asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    /// Linux/NAS software RAID: found from the superblocks.
    Auto(Vec<String>),
    /// Hardware RAID, described by hand. `None` disks are missing.
    Manual { geometry: Geometry, disks: Vec<Option<String>> },
    /// Disks one after another (JBOD, spanned).
    Span(Vec<String>),
}

impl Spec {
    /// Parses what follows `raid:`: `auto|disk|disk…`, `span|disk|…`, or
    /// `<level>,<chunk KiB>,<layout>|disk|missing|disk…`.
    pub fn parse(spec: &str) -> Result<Self> {
        let mut parts = spec.split('|');
        let head = parts.next().unwrap_or_default();
        let disks: Vec<String> = parts.map(str::to_string).collect();
        anyhow::ensure!(!disks.is_empty(), "no drives given for the RAID");
        match head {
            "auto" => Ok(Spec::Auto(disks)),
            "span" => Ok(Spec::Span(disks)),
            _ => {
                let f: Vec<&str> = head.split(',').collect();
                let level: i32 =
                    f.first().and_then(|l| l.parse().ok()).ok_or_else(|| anyhow::anyhow!("bad RAID level"))?;
                let chunk = f.get(1).and_then(|c| c.parse::<u64>().ok()).unwrap_or(64) * 1024;
                let layout = match f.get(2).copied().unwrap_or("left-symmetric") {
                    "left-asymmetric" => Layout::LeftAsymmetric,
                    "right-asymmetric" => Layout::RightAsymmetric,
                    "right-symmetric" => Layout::RightSymmetric,
                    "parity-last" => Layout::Parity0,
                    _ => Layout::LeftSymmetric,
                };
                let near = if level == 10 { 2 } else { 0 };
                let disks = disks.into_iter().map(|d| (d != MISSING).then_some(d)).collect();
                Ok(Spec::Manual { geometry: Geometry { level, chunk, layout, near }, disks })
            }
        }
    }

    /// The `raid:` path for this description.
    pub fn path(&self) -> String {
        let (head, disks): (String, Vec<String>) = match self {
            Spec::Auto(d) => ("auto".into(), d.clone()),
            Spec::Span(d) => ("span".into(), d.clone()),
            Spec::Manual { geometry: g, disks } => {
                let layout = match g.layout {
                    Layout::LeftAsymmetric => "left-asymmetric",
                    Layout::RightAsymmetric => "right-asymmetric",
                    Layout::RightSymmetric => "right-symmetric",
                    Layout::Parity0 => "parity-last",
                    Layout::LeftSymmetric => "left-symmetric",
                };
                (
                    format!("{},{},{layout}", g.level, g.chunk / 1024),
                    disks.iter().map(|d| d.clone().unwrap_or_else(|| MISSING.into())).collect(),
                )
            }
        };
        format!("raid:{head}|{}", disks.join("|"))
    }
}

/// Opens a `raid:` source (see [`Spec::parse`]): the disk to scan, and the
/// volumes on it.
pub fn open_spec(spec: &str) -> Result<(Source, Vec<crate::partition::Partition>)> {
    use crate::partition::discover;
    let open = |p: &str| crate::container::open(p).map(|(s, _)| s);
    match Spec::parse(spec)? {
        Spec::Manual { geometry, disks } => {
            let sources: Vec<Option<Source>> =
                disks.iter().map(|d| d.as_deref().map(open).transpose()).collect::<Result<_>>()?;
            let per_disk = sources.iter().flatten().map(|s| s.size()).min().unwrap_or(0);
            let disks = sources.into_iter().map(|s| s.map(|src| Disk { src, data_offset: 0 })).collect();
            let array: Source = Arc::new(Array::new(geometry, disks, per_disk)?);
            let parts = discover(&array);
            Ok((array, parts))
        }
        Spec::Span(disks) => {
            let disks = disks
                .iter()
                .map(|d| open(d).map(|src| (Disk { src: src.clone(), data_offset: 0 }, src.size())))
                .collect::<Result<Vec<_>>>()?;
            let disk = linear(disks);
            let parts = discover(&disk);
            Ok((disk, parts))
        }
        Spec::Auto(paths) => {
            // Members and physical volumes: the drives themselves and their
            // partitions.
            let mut members = Vec::new();
            let mut pvs = Vec::new();
            for p in &paths {
                let disk = open(p)?;
                let mut candidates = vec![disk.clone()];
                for part in discover(&disk) {
                    if part.start > 0 || part.len < disk.size() {
                        candidates.push(Arc::new(crate::source::SubSource::new(disk.clone(), part.start, part.len)));
                    }
                }
                for c in candidates {
                    if let Some(sb) = superblock(c.as_ref()) {
                        members.push((c, sb));
                    } else if let Some(pv) = crate::lvm::pv(c.as_ref()) {
                        pvs.push((c, pv));
                    }
                }
            }
            anyhow::ensure!(!members.is_empty() || !pvs.is_empty(), "no Linux RAID or LVM was found on these drives");
            // Every readable array, and the volumes on it.
            let mut volumes: Vec<(String, Source)> = Vec::new();
            for a in assemble(members) {
                let a = match a {
                    Ok(a) => a,
                    Err(e) => {
                        log::warn!("RAID: {e:#}");
                        continue;
                    }
                };
                let label = if a.level == -1 { "linear".to_string() } else { format!("RAID {}", a.level) };
                // mdadm names an array after its number when it has no name.
                let label = match a.name.as_str() {
                    "" => label,
                    n if n.chars().all(|c| c.is_ascii_digit()) => format!("{label} (md{n})"),
                    n => format!("{label} \"{n}\""),
                };
                if let Some(pv) = crate::lvm::pv(a.source.as_ref()) {
                    pvs.push((a.source, pv));
                } else if crate::fs::detect(a.source.as_ref()).is_some() {
                    volumes.push((label, a.source));
                } else {
                    for part in discover(&a.source) {
                        let sub: Source =
                            Arc::new(crate::source::SubSource::new(a.source.clone(), part.start, part.len));
                        volumes.push((format!("{label} #{}", part.index), sub));
                    }
                }
            }
            for lv in crate::lvm::volumes(&pvs) {
                volumes.push((format!("LVM {}/{}", lv.vg, lv.name), lv.source));
            }
            anyhow::ensure!(!volumes.is_empty(), "the RAID could not be put together from these drives");
            // One virtual disk: the volumes one after another, each on a
            // 1 MiB boundary, each a partition.
            const ALIGN: u64 = 1 << 20;
            let mut extents = Vec::new();
            let mut parts = Vec::new();
            let mut at = 0u64;
            for (i, (name, src)) in volumes.into_iter().enumerate() {
                let len = src.size();
                parts.push(crate::partition::Partition {
                    index: i + 1,
                    start: at,
                    len,
                    scheme: crate::partition::Scheme::Gpt,
                    kind: name,
                    name: String::new(),
                    fs: crate::fs::detect(src.as_ref()),
                    device: None,
                });
                extents.push((at, src));
                at = (at + len).div_ceil(ALIGN) * ALIGN;
            }
            Ok((Arc::new(crate::source::Concat::new(extents)), parts))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_of(level: i32, layout: Layout, n: usize, s: u64, d: u64) -> (usize, Option<usize>, usize) {
        let g = Geometry { level, chunk: 512, layout, near: 0 };
        let src: Source = Arc::new(crate::source::MemSource(vec![0; 4096]));
        let disks = (0..n).map(|_| Some(Disk { src: src.clone(), data_offset: 0 })).collect();
        Array::new(g, disks, 4096).unwrap().place(s, d)
    }

    #[test]
    fn specs_round_trip() {
        for path in ["raid:auto|/dev/sdb|/dev/sdc", "raid:5,64,left-symmetric|a.img|missing|c.img", "raid:span|x|y"] {
            assert_eq!(Spec::parse(path.strip_prefix("raid:").unwrap()).unwrap().path(), path);
        }
        assert!(Spec::parse("auto").is_err());
    }

    #[test]
    fn raid5_parity_rotates() {
        // left-symmetric, 3 disks: P on 2, 1, 0; data follows the parity.
        assert_eq!(layout_of(5, Layout::LeftSymmetric, 3, 0, 0), (2, None, 0));
        assert_eq!(layout_of(5, Layout::LeftSymmetric, 3, 1, 0), (1, None, 2));
        assert_eq!(layout_of(5, Layout::LeftSymmetric, 3, 2, 1), (0, None, 2));
        // left-asymmetric: data in disk order around the parity.
        assert_eq!(layout_of(5, Layout::LeftAsymmetric, 3, 1, 0), (1, None, 0));
        assert_eq!(layout_of(5, Layout::LeftAsymmetric, 3, 1, 1), (1, None, 2));
    }
}
