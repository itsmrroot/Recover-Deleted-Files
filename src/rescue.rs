//! Bringing back names and folders that the partition table or the file
//! system no longer knows about:
//!
//! * **Lost partitions.** A partition deleted from the partition table (or
//!   a table that was wiped) still starts with its boot sector, and NTFS,
//!   FAT32 and exFAT keep a backup copy of it further on. Every boot sector
//!   that does not belong to a current partition is tried: when its file
//!   system opens, all its files are listed with their names.
//! * **Quick-formatted NTFS drives.** A quick format writes a new, almost
//!   empty file table, but the old MFT records stay in what is now free
//!   space. They still hold every file's name, folder, dates and data
//!   location. The old layout (where the volume started and its cluster
//!   size) is worked out by checking candidate layouts against the files'
//!   contents: a `.jpg` record must point at JPEG data.
//!
//! Both live in the space the deep search reads anyway, so [`Finder::look`]
//! is handed every block it reads: no extra pass over the disk.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::sync::Arc;

use crate::bytes::{le16, le32, le64, u8_at};
use crate::carve;
use crate::fs::ntfs::record::{Data, FileRecord, NonResident, apply_fixups, ref_record, ref_seq};
use crate::fs::ntfs::{FIRST_USER_RECORD, Node, data_from_fragments, dir_path_with};
use crate::fs::{self, Condition, DeletedFile, FileData, FsKind};
use crate::partition::{Partition, Scheme};
use crate::ranges::ByteRange;
use crate::recover::Session;
use crate::source::{ReadAt, Source, SubSource, read_tolerant};

const SECTOR: u64 = 512;
/// Records kept at most (a few hundred bytes each in memory).
const MAX_RECORDS: usize = 4_000_000;
/// Files whose contents decide an old layout.
const LAYOUT_SAMPLE: usize = 48;
/// Smallest volume worth opening.
const MIN_VOLUME: u64 = 64 * SECTOR;

/// A boot sector seen by the search.
struct Boot {
    offset: u64,
    kind: FsKind,
    bytes: Vec<u8>,
}

impl Boot {
    /// The volume's length, where in the volume this sector can be (the
    /// main copy first, then backups) and its cluster size.
    fn geometry(&self) -> Option<(u64, Vec<u64>, u64)> {
        let b = &self.bytes;
        match self.kind {
            FsKind::Ntfs => {
                let bps = u64::from(le16(b, 0x0B)?);
                let raw = u8_at(b, 0x0D)?;
                let spc = if raw > 0x80 { 1u64 << (256 - u32::from(raw)) } else { u64::from(raw) };
                let total = le64(b, 0x28)?;
                let cluster = bps * spc;
                if !matches!(bps, 512 | 1024 | 2048 | 4096) || !cluster.is_power_of_two() || cluster > 2 << 20 {
                    return None;
                }
                // The backup boot sector follows the last sector of the volume.
                Some(((total + 1) * bps, vec![0, total * bps], cluster))
            }
            FsKind::ExFat => {
                let bps = 1u64 << u8_at(b, 0x6C)?.min(12);
                let cluster = bps << u8_at(b, 0x6D)?.min(25);
                Some((le64(b, 0x48)? * bps, vec![0, 12 * bps], cluster))
            }
            FsKind::Ext => {
                let block = 1024u64 << le32(b, 24)?.min(6);
                let blocks = u64::from(le32(b, 4)?);
                let group = u64::from(le16(b, 0x5A)?);
                let (first, per_group) = (u64::from(le32(b, 20)?), u64::from(le32(b, 32)?));
                // Group 0's superblock is 1024 bytes in; backups start their group.
                let at = if group == 0 { 1024 } else { (group * per_group + first) * block };
                Some((blocks * block, vec![at], block))
            }
            FsKind::HfsPlus => {
                let block = u64::from(crate::bytes::be32(b, 40)?);
                let len = u64::from(crate::bytes::be32(b, 44)?) * block;
                // The volume header, and its copy 1024 bytes before the end.
                Some((len, vec![1024, len.checked_sub(1024)?], block))
            }
            FsKind::Apfs => {
                let block = u64::from(le32(b, 36)?);
                Some((le64(b, 40)? * block, vec![0], block))
            }
            _ => {
                let bps = u64::from(le16(b, 11)?);
                let cluster = bps * u64::from(u8_at(b, 13)?);
                let total = match le16(b, 19)? {
                    0 => u64::from(le32(b, 32)?),
                    n => u64::from(n),
                };
                let backup = if self.kind == FsKind::Fat32 { u64::from(le16(b, 0x32)?) * bps } else { 0 };
                let backups = if backup > 0 { vec![backup] } else { Vec::new() };
                Some((total * bps, backups, cluster))
            }
        }
    }
}

/// An MFT record seen by the search.
struct Rec {
    offset: u64,
    /// The record's own number (NTFS 3.1 stores it in the header).
    no: u64,
    rec: FileRecord,
}

/// Collects boot sectors and MFT records from the blocks the deep search
/// reads.
#[derive(Default)]
pub struct Finder {
    boots: BTreeMap<u64, Boot>,
    records: BTreeMap<u64, Rec>,
}

/// A partition found by the search, and its files.
pub struct Rescued {
    pub partition: Partition,
    pub files: Vec<DeletedFile>,
}

impl Finder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Looks at a block of `disk` read at `pos`.
    pub fn look(&mut self, disk: &dyn ReadAt, pos: u64, block: &[u8]) {
        let end = pos + block.len() as u64;
        let mut abs = pos.div_ceil(SECTOR) * SECTOR;
        while abs + SECTOR <= end {
            let off = (abs - pos) as usize;
            let s = &block[off..off + SECTOR as usize];
            if s.starts_with(b"FILE") {
                self.record(disk, abs, &block[off..]);
            } else if s[510..512] == [0x55, 0xAA] {
                self.boot(abs, s);
            } else if let Some(kind) = superblock(s) {
                self.boots.entry(abs).or_insert_with(|| Boot { offset: abs, kind, bytes: s.to_vec() });
            }
            abs += SECTOR;
        }
    }

    fn record(&mut self, disk: &dyn ReadAt, abs: u64, rest: &[u8]) {
        if self.records.contains_key(&abs) || self.records.len() >= MAX_RECORDS {
            return;
        }
        let (Some(usa_off), Some(usa_count), Some(size)) = (le16(rest, 4), le16(rest, 6), le32(rest, 0x1C)) else {
            return;
        };
        let size = size as usize;
        // NTFS 3.1 (Windows XP and later): the record number is in the header.
        if usa_off != 0x30 || !matches!(size, 1024 | 2048 | 4096) || usize::from(usa_count) != size / 512 + 1 {
            return;
        }
        let mut buf = match rest.get(..size) {
            Some(b) => b.to_vec(),
            None => match disk.read_vec(abs, size) {
                Ok(v) => v,
                Err(_) => return,
            },
        };
        // A torn record is a stale fragment, not a record.
        if !apply_fixups(&mut buf) {
            return;
        }
        let (Some(no), Some(rec)) = (le32(&buf, 0x2C), FileRecord::parse(&buf)) else { return };
        let useful = rec.file_name.is_some() || (rec.base_ref != 0 && rec.data.is_some());
        if useful {
            self.records.insert(abs, Rec { offset: abs, no: u64::from(no), rec });
        }
    }

    fn boot(&mut self, abs: u64, s: &[u8]) {
        let kind = match &s[3..11] {
            b"NTFS    " => FsKind::Ntfs,
            b"EXFAT   " => FsKind::ExFat,
            _ => match fs::fat::detect_boot_sector(s) {
                Some(k) => k,
                None => return,
            },
        };
        self.boots.entry(abs).or_insert_with(|| Boot { offset: abs, kind, bytes: s.to_vec() });
    }

    /// Rebuilds what was found. `free` is the space no current file uses
    /// (whole disk, sorted), to judge whether the files' data is intact.
    pub fn finish(self, session: &Session, free: &[ByteRange]) -> Vec<Rescued> {
        let mut out = Vec::new();
        let lost = self.lost_partitions(session, free, &mut out);
        self.old_file_tables(session, free, &lost, &mut out);
        out
    }

    /// Opens every boot sector that no current partition starts with.
    /// Returns the ranges of the partitions found.
    fn lost_partitions(&self, session: &Session, free: &[ByteRange], out: &mut Vec<Rescued>) -> Vec<ByteRange> {
        let disk = &session.disk;
        let size = disk.size();
        let current: HashSet<u64> = session.partitions.iter().map(|p| p.start).collect();
        // Volume start -> (boot sector, volume length). A primary copy wins
        // over a backup that points at the same start.
        let mut candidates: BTreeMap<u64, (&Boot, u64)> = BTreeMap::new();
        for b in self.boots.values() {
            let Some((len, positions, _)) = b.geometry() else { continue };
            for (k, pos) in positions.iter().enumerate() {
                let Some(start) = b.offset.checked_sub(*pos) else { continue };
                if current.contains(&start) {
                    continue;
                }
                let e = candidates.entry(start).or_insert((b, len));
                if k == 0 {
                    *e = (b, len);
                }
            }
        }
        let mut found: Vec<ByteRange> = Vec::new();
        for (start, (boot, len)) in candidates {
            let len = len.min(size.saturating_sub(start));
            if len < MIN_VOLUME || found.iter().any(|r| r.contains(&start)) {
                continue;
            }
            let sub: Source = Arc::new(SubSource::new(disk.clone(), start, len));
            // The main copy may be gone: read the volume through its backup.
            let main = boot.geometry().and_then(|g| g.1.first().copied()).unwrap_or(0);
            let mut first = vec![0u8; SECTOR as usize];
            read_tolerant(sub.as_ref(), main, &mut first);
            let src: Source = if first == boot.bytes {
                sub
            } else {
                Arc::new(Patched { inner: sub, at: main, boot: boot.bytes.clone() })
            };
            let Ok(vol) = fs::open(src) else { continue };
            let Ok(mut files) = vol.scan_files(true, &mut |_, _| {}) else { continue };
            for f in &mut files {
                worsen(&mut f.condition, free, start, &f.data, f.size);
                add_note(f, "on a lost partition");
            }
            found.push(start..start + len);
            out.push(Rescued {
                partition: Partition {
                    index: out.len() + 1,
                    start,
                    len,
                    scheme: Scheme::Found,
                    kind: "lost partition".into(),
                    name: String::new(),
                    fs: Some(vol.kind()),
                    device: None,
                },
                files,
            });
        }
        found
    }

    /// Rebuilds old NTFS file tables from the records found in free space.
    fn old_file_tables(&self, session: &Session, free: &[ByteRange], lost: &[ByteRange], out: &mut Vec<Rescued>) {
        let disk = session.disk.as_ref();
        // Records of the partitions found above are already listed.
        let mut groups: BTreeMap<Option<usize>, Vec<&Rec>> = BTreeMap::new();
        for r in self.records.values() {
            if lost.iter().any(|l| l.contains(&r.offset)) {
                continue;
            }
            let host = session.partitions.iter().position(|p| p.range().contains(&r.offset));
            groups.entry(host).or_default().push(r);
        }
        for (host, recs) in groups {
            let host = host.map(|i| &session.partitions[i]);
            let Some((start, cluster)) = self.layout(disk, host, &recs) else {
                log::info!("old MFT records at {:#x}: volume layout unknown", recs[0].offset);
                continue;
            };
            let end = host.map_or(disk.size(), |h| h.start + h.len);
            let files = rebuild(&recs, start, cluster, free);
            if files.is_empty() {
                continue;
            }
            out.push(Rescued {
                partition: Partition {
                    index: out.len() + 1,
                    start,
                    len: end.saturating_sub(start),
                    scheme: Scheme::Found,
                    kind: "old file table".into(),
                    name: String::new(),
                    fs: Some(FsKind::Ntfs),
                    device: None,
                },
                files,
            });
        }
    }

    /// Where the volume of these records started and its cluster size:
    /// the layout under which the most sampled files point at content of
    /// their own type.
    fn layout(&self, disk: &dyn ReadAt, host: Option<&Partition>, recs: &[&Rec]) -> Option<(u64, u64)> {
        let lowest = recs.iter().map(|r| r.offset).min()?;
        // Candidate starts: the partition the records are in, and every NTFS
        // boot sector (or the start its backup points to) before them.
        let mut starts: Vec<(u64, Option<u64>)> = Vec::new();
        if let Some(h) = host {
            let mut bs = vec![0u8; SECTOR as usize];
            read_tolerant(disk, h.start, &mut bs);
            let boot = Boot { offset: h.start, kind: FsKind::Ntfs, bytes: bs };
            let cluster = (&boot.bytes[3..11] == b"NTFS    ").then(|| boot.geometry()).flatten().map(|g| g.2);
            starts.push((h.start, cluster));
        }
        for b in self.boots.values().filter(|b| b.kind == FsKind::Ntfs) {
            let Some((_, positions, cluster)) = b.geometry() else { continue };
            for s in positions.iter().filter_map(|d| b.offset.checked_sub(*d)) {
                if s <= lowest && host.is_none_or(|h| h.range().contains(&s)) && !starts.iter().any(|x| x.0 == s) {
                    starts.push((s, Some(cluster)));
                }
            }
        }
        if starts.is_empty() {
            return None;
        }
        // Files whose first cluster can be checked by their type.
        let sample: Vec<(u64, String)> = recs
            .iter()
            .filter(|r| r.rec.base_ref == 0 && !r.rec.is_dir())
            .filter_map(|r| {
                let name = &r.rec.file_name.as_ref()?.name;
                let ext = carve::normalize_ext(name.rsplit_once('.')?.1);
                let checkable = carve::all_formats().iter().any(|f| f.kinds().iter().any(|(e, _)| *e == ext));
                let Some(Data::NonResident(nr)) = &r.rec.data else { return None };
                let lcn = nr.runs.first()?.lcn?;
                (checkable && nr.start_vcn == 0).then_some((lcn, ext))
            })
            .take(LAYOUT_SAMPLE)
            .collect();
        let mut best: Option<(usize, u8, u64, u64)> = None;
        let mut head = vec![0u8; carve::HEAD_LEN];
        for &(start, boot_cluster) in &starts {
            for shift in 9..=16 {
                let cluster = 1u64 << shift;
                let score = sample
                    .iter()
                    .filter(|(lcn, ext)| {
                        let at = start + lcn * cluster;
                        at + head.len() as u64 <= disk.size() && {
                            read_tolerant(disk, at, &mut head);
                            carve::sniff_matches_ext(&head, ext)
                        }
                    })
                    .count();
                // On a tie: the cluster size the boot sector states, then
                // the most common one.
                let preference = if Some(cluster) == boot_cluster { 2 } else { u8::from(cluster == 4096) };
                if best.is_none_or(|b| (score, preference) > (b.0, b.1)) {
                    best = Some((score, preference, start, cluster));
                }
            }
        }
        let (score, _, start, cluster) = best?;
        // Files to check, and none of them where expected: the layout is not
        // known, and guessing would only produce garbage.
        if sample.len() >= 3 && score == 0 {
            return None;
        }
        Some((start, cluster))
    }
}

/// The files of one old file table, with paths rebuilt from their records.
fn rebuild(recs: &[&Rec], start: u64, cluster: u64, free: &[ByteRange]) -> Vec<DeletedFile> {
    let mut nodes: HashMap<u64, Node> = HashMap::new();
    let mut extensions: HashMap<u64, Vec<(u16, NonResident)>> = HashMap::new();
    for r in recs {
        if r.rec.base_ref != 0 {
            if let Some(Data::NonResident(nr)) = &r.rec.data {
                extensions.entry(ref_record(r.rec.base_ref)).or_default().push((ref_seq(r.rec.base_ref), nr.clone()));
            }
        } else if let Some(f) = &r.rec.file_name {
            nodes.entry(r.no).or_insert(Node {
                seq: r.rec.seq,
                in_use: r.rec.in_use(),
                parent: f.parent,
                name: f.name.clone().into_boxed_str(),
            });
        }
    }
    let mut cache = HashMap::new();
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for r in recs {
        let rec = &r.rec;
        let Some(fname) = rec.file_name.as_ref() else { continue };
        if rec.base_ref != 0 || rec.is_dir() || r.no < FIRST_USER_RECORD || !seen.insert(r.no) {
            continue;
        }
        let (data, size, note) = match &rec.data {
            Some(Data::Resident(v)) => (FileData::Resident(v.clone()), v.len() as u64, None),
            other => {
                let mut frags: Vec<NonResident> = match other {
                    Some(Data::NonResident(nr)) => vec![nr.clone()],
                    _ => Vec::new(),
                };
                if let Some(ext) = extensions.get(&r.no) {
                    frags.extend(
                        ext.iter()
                            .filter(|(seq, _)| *seq == rec.seq || seq.wrapping_add(1) == rec.seq)
                            .map(|(_, nr)| nr.clone()),
                    );
                }
                data_from_fragments(cluster, frags, fname.real_size)
            }
        };
        let mut condition =
            if matches!(data, FileData::Lost) { Condition::Overwritten } else { Condition::Recoverable };
        worsen(&mut condition, free, start, &data, size);
        let dir = dir_path_with(&|no| nodes.get(&no), fname.parent, &mut cache);
        let path = if dir.is_empty() { fname.name.clone() } else { format!("{dir}/{}", fname.name) };
        let pick = |si: u64, fnt: u64| fs::filetime(if si != 0 { si } else { fnt });
        let mut file = DeletedFile {
            id: r.no,
            path,
            size,
            created: pick(rec.si_created, fname.created),
            modified: pick(rec.si_modified, fname.modified),
            condition,
            note,
            data,
        };
        add_note(&mut file, "from an old file table (formatted drive)");
        files.push(file);
    }
    files
}

/// Lowers `condition` by how much of the file's data (volume at `start`)
/// lies in space that current files use.
fn worsen(condition: &mut Condition, free: &[ByteRange], start: u64, data: &FileData, size: u64) {
    let (FileData::Extents(extents) | FileData::Compressed { extents, .. }) = data else { return };
    let (mut total, mut ok) = (0u64, 0u64);
    let mut left = size;
    for e in extents {
        if left == 0 {
            break;
        }
        let n = e.len.min(left);
        left -= n;
        let Some(o) = e.offset else { continue };
        let (s, end) = (start + o, start + o + n);
        total += n;
        // Free ranges are sorted and disjoint.
        let first = free.partition_point(|r| r.end <= s);
        for r in &free[first..] {
            if r.start >= end {
                break;
            }
            ok += end.min(r.end) - s.max(r.start);
        }
    }
    let here = Condition::from_counts(ok / SECTOR, total / SECTOR);
    let rank = |c: &Condition| match c {
        Condition::Recoverable => 0,
        Condition::Partial(p) => 200 - u16::from(*p),
        Condition::Overwritten => 300,
        Condition::Erased => 400,
    };
    if rank(&here) > rank(condition) {
        *condition = here;
    }
}

fn add_note(f: &mut DeletedFile, note: &str) {
    f.note = Some(match f.note.take() {
        Some(n) => format!("{note}; {n}"),
        None => note.to_string(),
    });
}

/// The superblock of ext, HFS+ or APFS at the start of `s`, if it is one.
fn superblock(s: &[u8]) -> Option<FsKind> {
    if fs::ext::detect(s) {
        Some(FsKind::Ext)
    } else if fs::hfsplus::detect(s) {
        Some(FsKind::HfsPlus)
    } else if fs::apfs::detect(s) {
        Some(FsKind::Apfs)
    } else {
        None
    }
}

/// A volume read with its boot sector (or superblock) replaced by a backup
/// copy at offset `at`.
struct Patched {
    inner: Source,
    at: u64,
    boot: Vec<u8>,
}

impl ReadAt for Patched {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read_at(offset, buf)?;
        let (from, to) = (self.at, self.at + self.boot.len() as u64);
        let (s, e) = (offset.max(from), (offset + n as u64).min(to));
        if s < e {
            buf[(s - offset) as usize..(e - offset) as usize]
                .copy_from_slice(&self.boot[(s - from) as usize..(e - from) as usize]);
        }
        Ok(n)
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_follow_the_space_now_in_use() {
        let free = vec![0..4096, 8192..16384];
        let data = FileData::Extents(vec![fs::Extent { offset: Some(0), len: 8192 }]);
        let mut c = Condition::Recoverable;
        worsen(&mut c, &free, 0, &data, 8192);
        assert_eq!(c, Condition::Partial(50));
        let mut c = Condition::Recoverable;
        worsen(&mut c, &free, 8192, &data, 8192);
        assert_eq!(c, Condition::Recoverable);
        // Never better than it was.
        let mut c = Condition::Overwritten;
        worsen(&mut c, &free, 8192, &data, 8192);
        assert_eq!(c, Condition::Overwritten);
    }
}
