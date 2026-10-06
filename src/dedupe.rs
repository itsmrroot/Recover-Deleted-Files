//! Finding the same file found more than once.
//!
//! The same content often turns up several times: a deleted file found
//! through its metadata *and* by the deep search, several old MFT records
//! pointing at the same clusters, or copies of one photo in different
//! folders. Users then sort through identical files by hand.
//!
//! Nothing is guessed:
//! 1. items whose data lies at exactly the same place on the disk are the
//!    same bytes, without reading anything;
//! 2. otherwise items of equal size and equal first and last 64 KiB are
//!    candidates, and only a full byte-by-byte comparison makes one a
//!    duplicate of another.
//!
//! Of each set of duplicates, the copy with a real name (file system) is
//! kept as the original, preferring intact files outside `$Orphan`.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::fs::{Condition, FileData};
use crate::progress::{Progress, Unit};
use crate::recover::{Found, ItemRef, Session};
use crate::source::{ReadAt, read_tolerant};

const SAMPLE: u64 = 64 << 10;
const CHUNK: usize = 1 << 20;

/// Where an item's bytes come from, in order.
enum Piece {
    /// Absolute disk offset.
    Disk(u64, u64),
    /// Sparse region (reads as zeros).
    Zero(u64),
    /// Small file kept inside the metadata.
    Mem(Vec<u8>),
}

impl Piece {
    fn len(&self) -> u64 {
        match self {
            Piece::Disk(_, n) | Piece::Zero(n) => *n,
            Piece::Mem(v) => v.len() as u64,
        }
    }
}

struct Content {
    pieces: Vec<Piece>,
    len: u64,
}

impl Content {
    /// The exact bytes `recover::save` would write for this item, or `None`
    /// for items whose content is unreliable or not directly addressable.
    fn of(session: &Session, found: &Found, r: ItemRef) -> Option<Content> {
        let pieces = match r {
            ItemRef::Carved(i) => {
                let c = found.carved.get(i)?;
                vec![Piece::Disk(c.offset, c.len)]
            }
            ItemRef::Fs(i) => {
                let f = found.fs.get(i)?;
                if !matches!(f.file.condition, Condition::Recoverable | Condition::Partial(_)) {
                    return None;
                }
                let start = session.partitions.get(f.partition)?.start;
                let mut left = f.file.size;
                let mut pieces = Vec::new();
                match &f.file.data {
                    FileData::Resident(v) => pieces.push(Piece::Mem(v[..v.len().min(left as usize)].to_vec())),
                    FileData::Extents(extents) => {
                        for e in extents {
                            if left == 0 {
                                break;
                            }
                            let n = e.len.min(left);
                            pieces.push(match e.offset {
                                Some(o) => Piece::Disk(start + o, n),
                                None => Piece::Zero(n),
                            });
                            left -= n;
                        }
                    }
                    // Compressed streams would have to be decompressed.
                    FileData::Compressed { .. } | FileData::Lost => return None,
                }
                pieces
            }
        };
        let len = pieces.iter().map(Piece::len).sum();
        (len > 0).then_some(Content { pieces, len })
    }

    /// Identifies the data by where it lies, when it all lies on the disk.
    fn location(&self) -> Option<Vec<(u64, u64)>> {
        self.pieces
            .iter()
            .map(|p| match p {
                Piece::Disk(o, n) => Some((*o, *n)),
                _ => None,
            })
            .collect()
    }

    /// Fills `buf` from logical offset `pos` (unreadable sectors read as zeros).
    fn read(&self, disk: &dyn ReadAt, mut pos: u64, buf: &mut [u8]) {
        let mut done = 0usize;
        let mut start = 0u64;
        for p in &self.pieces {
            if done == buf.len() {
                break;
            }
            let end = start + p.len();
            if pos < end {
                let within = pos - start;
                let n = ((end - pos) as usize).min(buf.len() - done);
                let dst = &mut buf[done..done + n];
                match p {
                    Piece::Disk(o, _) => {
                        read_tolerant(disk, o + within, dst);
                    }
                    Piece::Zero(_) => dst.fill(0),
                    Piece::Mem(v) => dst.copy_from_slice(&v[within as usize..within as usize + n]),
                }
                done += n;
                pos += n as u64;
            }
            start = end;
        }
        buf[done..].fill(0);
    }

    fn sample_hash(&self, disk: &dyn ReadAt) -> u64 {
        let mut h = DefaultHasher::new();
        let head = self.len.min(SAMPLE) as usize;
        let mut buf = vec![0u8; head];
        self.read(disk, 0, &mut buf);
        buf.hash(&mut h);
        if self.len > SAMPLE {
            let tail = self.len.min(SAMPLE) as usize;
            buf.resize(tail, 0);
            self.read(disk, self.len - tail as u64, &mut buf);
            buf.hash(&mut h);
        }
        h.finish()
    }

    fn same_bytes(&self, other: &Content, disk: &dyn ReadAt, cancel: &AtomicBool) -> bool {
        if self.len != other.len {
            return false;
        }
        let mut a = vec![0u8; CHUNK];
        let mut b = vec![0u8; CHUNK];
        let mut pos = 0u64;
        while pos < self.len {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let n = (self.len - pos).min(CHUNK as u64) as usize;
            self.read(disk, pos, &mut a[..n]);
            other.read(disk, pos, &mut b[..n]);
            if a[..n] != b[..n] {
                return false;
            }
            pos += n as u64;
        }
        true
    }
}

/// Which copy to keep: named, intact, in a known folder, found first.
fn preference(found: &Found, r: ItemRef) -> (u8, u8, u8, usize) {
    match r {
        ItemRef::Fs(i) => {
            let f = &found.fs[i].file;
            let damaged = u8::from(f.condition != Condition::Recoverable);
            let orphan = u8::from(f.path.starts_with("$Orphan"));
            (0, damaged, orphan, i)
        }
        ItemRef::Carved(i) => (1, 0, 0, i),
    }
}

/// Maps every duplicate to the copy it duplicates.
pub fn find(
    session: &Session,
    found: &Found,
    progress: &dyn Progress,
    cancel: &AtomicBool,
) -> HashMap<ItemRef, ItemRef> {
    let mut items: Vec<(ItemRef, Content)> = (0..found.fs.len())
        .map(ItemRef::Fs)
        .chain((0..found.carved.len()).map(ItemRef::Carved))
        .filter_map(|r| Content::of(session, found, r).map(|c| (r, c)))
        .collect();
    items.sort_by_key(|(r, _)| preference(found, *r));
    let disk = session.disk.as_ref();
    let mut dup: HashMap<ItemRef, ItemRef> = HashMap::new();

    // 1. Same place on the disk: same bytes.
    let mut by_location: HashMap<Vec<(u64, u64)>, ItemRef> = HashMap::new();
    for (r, c) in &items {
        if let Some(loc) = c.location() {
            match by_location.get(&loc) {
                Some(orig) => {
                    dup.insert(*r, *orig);
                }
                None => {
                    by_location.insert(loc, *r);
                }
            }
        }
    }

    // 2. Same size, same samples, then a full comparison.
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (k, (r, c)) in items.iter().enumerate() {
        if !dup.contains_key(r) {
            by_size.entry(c.len).or_default().push(k);
        }
    }
    let groups: Vec<Vec<usize>> = by_size.into_values().filter(|g| g.len() > 1).collect();
    let total: u64 = groups.iter().map(|g| g.len() as u64).sum();
    if total == 0 {
        return dup;
    }
    progress.begin("Checking for duplicates", total, Unit::Items);
    for group in groups {
        let mut by_sample: HashMap<u64, Vec<usize>> = HashMap::new();
        for k in group {
            if cancel.load(Ordering::Relaxed) {
                progress.end();
                return dup;
            }
            by_sample.entry(items[k].1.sample_hash(disk)).or_default().push(k);
            progress.inc(1);
        }
        for mut members in by_sample.into_values().filter(|m| m.len() > 1) {
            // Keep the preferred copy first.
            members.sort_unstable();
            let mut originals: Vec<usize> = Vec::new();
            for k in members {
                let original = originals.iter().copied().find(|&o| items[o].1.same_bytes(&items[k].1, disk, cancel));
                match original {
                    Some(o) => {
                        dup.insert(items[k].0, items[o].0);
                    }
                    None => originals.push(k),
                }
            }
        }
    }
    progress.end();
    dup
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::carve::{Carved, Category};
    use crate::fs::{DeletedFile, Extent};
    use crate::partition::{Partition, Scheme};
    use crate::progress::Silent;
    use crate::recover::FsFound;
    use crate::source::MemSource;

    const PART: u64 = 1 << 20;

    fn session(disk: Vec<u8>) -> Session {
        let len = disk.len() as u64 - PART;
        Session {
            disk: Arc::new(MemSource(disk)),
            path: "test".into(),
            partitions: vec![Partition {
                index: 1,
                start: PART,
                len,
                scheme: Scheme::Mbr,
                kind: "0x07".into(),
                name: String::new(),
                fs: None,
            }],
        }
    }

    fn fs_file(path: &str, offset: u64, size: u64, condition: Condition) -> FsFound {
        FsFound {
            partition: 0,
            file: DeletedFile {
                id: 0,
                path: path.into(),
                size,
                created: None,
                modified: None,
                condition,
                note: None,
                data: FileData::Extents(vec![Extent { offset: Some(offset), len: size.div_ceil(512) * 512 }]),
            },
        }
    }

    fn carved(offset: u64, len: u64) -> Carved {
        Carved { offset, len, ext: "jpg", category: Category::Image, format: "jpeg", title: None, date: None }
    }

    fn noise(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| ((i * 7 + usize::from(seed) * 13) % 251) as u8).collect()
    }

    #[test]
    fn same_place_on_disk_is_the_same_file() {
        let mut disk = vec![0u8; 4 << 20];
        disk[PART as usize + 4096..PART as usize + 4096 + 5000].copy_from_slice(&noise(5000, 1));
        let s = session(disk);
        let found = Found {
            fs: vec![fs_file("Photos/a.jpg", 4096, 5000, Condition::Recoverable)],
            carved: vec![carved(PART + 4096, 5000)],
            ..Found::default()
        };
        let dup = find(&s, &found, &Silent, &AtomicBool::new(false));
        // The named copy is the one kept.
        assert_eq!(dup.get(&ItemRef::Carved(0)), Some(&ItemRef::Fs(0)));
        assert_eq!(dup.len(), 1);
    }

    #[test]
    fn identical_copies_are_compared_in_full() {
        let len = 300_000usize;
        let a = noise(len, 2);
        let mut b = a.clone();
        // Same size, same first and last 64 KiB, different middle.
        b[len / 2] ^= 0xFF;
        let mut disk = vec![0u8; 4 << 20];
        let at = |k: usize| 2 * PART as usize + k * 400_000;
        disk[at(0)..at(0) + len].copy_from_slice(&a);
        disk[at(1)..at(1) + len].copy_from_slice(&a);
        disk[at(2)..at(2) + len].copy_from_slice(&b);
        let s = session(disk);
        let found = Found {
            carved: vec![
                carved(at(0) as u64, len as u64),
                carved(at(1) as u64, len as u64),
                carved(at(2) as u64, len as u64),
            ],
            ..Found::default()
        };
        let dup = find(&s, &found, &Silent, &AtomicBool::new(false));
        assert_eq!(dup.get(&ItemRef::Carved(1)), Some(&ItemRef::Carved(0)));
        assert_eq!(dup.get(&ItemRef::Carved(2)), None, "differs in the middle");
        assert_eq!(dup.len(), 1);
    }

    #[test]
    fn named_intact_copies_are_preferred_and_overwritten_files_ignored() {
        let mut disk = vec![0u8; 4 << 20];
        let data = noise(9000, 3);
        for off in [0usize, 20_000, 40_000] {
            disk[PART as usize + off..PART as usize + off + data.len()].copy_from_slice(&data);
        }
        let s = session(disk);
        let found = Found {
            fs: vec![
                fs_file("$Orphan/x.jpg", 0, 9000, Condition::Recoverable),
                fs_file("Photos/x.jpg", 20_000, 9000, Condition::Recoverable),
                fs_file("Old/x.jpg", 40_000, 9000, Condition::Overwritten),
            ],
            ..Found::default()
        };
        let dup = find(&s, &found, &Silent, &AtomicBool::new(false));
        assert_eq!(dup.get(&ItemRef::Fs(0)), Some(&ItemRef::Fs(1)));
        // Overwritten content is not trusted to be anything.
        assert_eq!(dup.get(&ItemRef::Fs(2)), None);
        assert_eq!(dup.len(), 1);
    }
}
