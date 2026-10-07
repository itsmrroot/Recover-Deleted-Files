//! Rebuilding videos that were stored in pieces.
//!
//! A camera writes a long video into whatever free clusters the card has,
//! so after earlier files were deleted it ends up in pieces. The deep
//! search assumes a file is in one piece, so such videos either are not
//! found at all or come out with someone else's data inside.
//!
//! An MP4/MOV file's index (`moov`) records where every frame is in the
//! file, and every H.264/H.265 frame can be recognised (see
//! [`crate::carve::bmff::is_frame`]). So the video is followed frame by
//! frame from its start; when a frame is not where the index says, the
//! video continues elsewhere, at a cluster boundary: the disk ahead is
//! searched for the place where that frame, and the ones after it, are.
//! Cameras usually write the index at the very end, which is also found by
//! the search.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::NaiveDateTime;
use memchr::memmem;

use crate::bytes::be32;
use crate::carve::bmff::{self, Codec, Index};
use crate::carve::{Carved, Reader};
use crate::fs::Extent;
use crate::source::{ReadAt, read_tolerant};

const SECTOR: u64 = 512;
/// Largest index kept.
const MAX_MOOV: usize = 64 << 20;
const MAX_MOOVS: usize = 4096;
/// How far ahead on the disk the next piece of a video is looked for.
const WINDOW: u64 = 8 << 30;
/// Frames that must check out at a new piece before it is accepted.
const CONFIRM: usize = 3;
/// Frames checked in a video assumed to be in one piece.
const SPOT_CHECKS: usize = 16;

/// Collects where videos start, and their indexes, from the blocks the
/// deep search reads.
#[derive(Default)]
pub struct VideoFinder {
    starts: BTreeSet<u64>,
    moovs: BTreeMap<u64, Vec<u8>>,
}

/// A video put back together from its pieces.
pub struct Rebuilt {
    pub start: u64,
    pub ext: &'static str,
    /// Pieces on the disk, in order.
    pub extents: Vec<Extent>,
    pub size: u64,
    pub created: Option<NaiveDateTime>,
    /// Every frame was found.
    pub complete: bool,
    /// How much of the media data was found, in percent.
    pub percent: u8,
}

impl VideoFinder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Looks at a block of `disk` read at `pos`.
    pub fn look(&mut self, disk: &dyn ReadAt, pos: u64, block: &[u8]) {
        let end = pos + block.len() as u64;
        // A video starts with "ftyp" on a sector boundary.
        let mut abs = pos.div_ceil(SECTOR) * SECTOR;
        while abs + 16 <= end {
            let off = (abs - pos) as usize;
            if &block[off + 4..off + 8] == b"ftyp"
                && be32(block, off).is_some_and(|s| (16..=512).contains(&s))
                && is_video_brand(&block[off + 8..off + 12])
            {
                self.starts.insert(abs);
            }
            abs += SECTOR;
        }
        // An index can be anywhere.
        for i in memmem::find_iter(block, b"moov") {
            if i < 4 || self.moovs.len() >= MAX_MOOVS {
                continue;
            }
            let at = pos + i as u64 - 4;
            let Some(size) = be32(block, i - 4).map(|s| s as usize) else { continue };
            let first_child = block.get(i + 8..i + 12);
            if self.moovs.contains_key(&at)
                || !(64..=MAX_MOOV).contains(&size)
                || !matches!(first_child, Some(b"mvhd" | b"prfl" | b"iods" | b"udta" | b"trak") | None)
            {
                continue;
            }
            let bytes = match block.get(i - 4..i - 4 + size) {
                Some(b) => b.to_vec(),
                None => match disk.read_vec(at, size) {
                    Ok(b) => b,
                    Err(_) => continue,
                },
            };
            if bmff::parse_moov(&bytes).is_some_and(|ix| !ix.video_samples().is_empty()) {
                self.moovs.insert(at, bytes);
            }
        }
    }

    /// Rebuilds the videos that the deep search could not take in one
    /// piece. Videos in `carved` that turn out to be in pieces are replaced
    /// by their rebuilt version (removed from `carved`).
    /// `cluster_at(offset)` is the cluster size of the volume there.
    pub fn rebuild(
        self,
        disk: &dyn ReadAt,
        carved: &mut Vec<Carved>,
        cluster_at: &dyn Fn(u64) -> u64,
        cancel: &AtomicBool,
    ) -> Vec<Rebuilt> {
        let mut starts = self.starts.clone();
        // Videos found in one piece: keep them if their frames check out.
        let mut drop = Vec::new();
        for (i, c) in carved.iter().enumerate().filter(|(_, c)| c.format == "bmff" && is_video_ext(c.ext)) {
            if intact(disk, c.offset, c.len) {
                starts.remove(&c.offset);
            } else {
                starts.insert(c.offset);
                drop.push((i, c.offset));
            }
        }
        let mut out = Vec::new();
        let mut used_moovs = BTreeSet::new();
        for start in starts {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            if let Some(v) = self.rebuild_one(disk, start, cluster_at(start), &mut used_moovs) {
                out.push(v);
            }
        }
        // A carved video that was rebuilt is listed once, rebuilt.
        let rebuilt: BTreeSet<u64> = out.iter().map(|v| v.start).collect();
        let gone: BTreeSet<usize> = drop.iter().filter(|(_, o)| rebuilt.contains(o)).map(|(i, _)| *i).collect();
        let mut i = 0;
        carved.retain(|_| {
            let keep = !gone.contains(&i);
            i += 1;
            keep
        });
        out
    }

    fn rebuild_one(
        &self,
        disk: &dyn ReadAt,
        start: u64,
        cluster: u64,
        used_moovs: &mut BTreeSet<u64>,
    ) -> Option<Rebuilt> {
        // The boxes at the start: ftyp, perhaps moov, then mdat.
        let mut r = Reader::new(disk, start, 64 << 20);
        let brand: [u8; 4] = r.bytes(8, 4)?.try_into().ok()?;
        let mut pos = 0u64;
        let mut moov_first: Option<(u64, u64)> = None;
        let (mdat, header, mdat_size) = loop {
            let size32 = u64::from(r.be32(pos)?);
            let ty = r.bytes(pos + 4, 4)?.to_vec();
            let (size, header) = match size32 {
                1 => (r.be64(pos + 8)?, 16),
                s => (s, 8),
            };
            if &ty[..] == b"mdat" {
                break (pos, header, size);
            }
            if size < 8 || pos > 1 << 20 {
                return None;
            }
            if &ty[..] == b"moov" {
                moov_first = Some((pos, size));
            }
            pos += size;
        };
        let payload = mdat + header;

        // The index: inside the first piece, or found elsewhere on the disk.
        let (index, moov_tail) = match moov_first {
            Some((at, size)) => {
                let bytes = disk.read_vec(start + at, usize::try_from(size).ok().filter(|&s| s <= MAX_MOOV)?).ok()?;
                (bmff::parse_moov(&bytes)?, None)
            }
            None => {
                let mdat_end = (mdat_size > header).then(|| mdat + mdat_size);
                let (at, ix) = self.pick_moov(start, payload, mdat_end, used_moovs)?;
                let len = self.moovs[&at].len() as u64;
                (ix, Some((at, len)))
            }
        };
        let frames: Vec<(u64, u32, Codec)> =
            index.video_samples().into_iter().filter(|(o, s, _)| *o >= payload && *s > 0).collect();
        let (_, data_end) = index.data_range()?;
        let end = match moov_tail {
            // The media data ends where the index's data ends (or where
            // the mdat box says, if that is later).
            Some(_) => data_end.max(if mdat_size > header { mdat + mdat_size } else { 0 }),
            None => data_end,
        };
        if frames.len() < CONFIRM {
            return None;
        }

        // Follow the frames.
        let mut pieces: Vec<(u64, u64)> = vec![(0, start)]; // (offset in file, on disk)
        let mut good_until = payload;
        let mut length = end;
        let mut complete = true;
        let mut i = 0;
        while i < frames.len() {
            let (off, size, codec) = frames[i];
            if let Some(checked) = frame_checked(disk, &pieces, off, size, codec) {
                // The end of a frame is picture data: only trusted as far
                // as its last unit header.
                good_until = off + checked as u64;
                i += 1;
                continue;
            }
            // The video continues elsewhere from a cluster boundary after
            // the last frame that checked out, at the latest inside this
            // frame. Every such boundary is tried, the earliest first.
            let last = pieces.last()?.0;
            let first_boundary = (good_until.div_ceil(cluster) * cluster).max(last + cluster);
            let boundaries: Vec<u64> = (first_boundary..off + u64::from(size)).step_by(cluster as usize).collect();
            let next_piece = find_piece(disk, &pieces, start, cluster, &boundaries, &frames, i);
            match next_piece {
                Some(p) => pieces.push(p),
                None => {
                    // Not even the first frames are there: this is no video.
                    if i < CONFIRM {
                        return None;
                    }
                    (length, complete) = (first_boundary.min(end), false);
                    break;
                }
            }
        }
        if i < CONFIRM {
            return None;
        }

        let mut extents = Vec::with_capacity(pieces.len() + 1);
        for (k, &(off, disk_pos)) in pieces.iter().enumerate() {
            let next = pieces.get(k + 1).map_or(length, |p| p.0).min(length);
            if next > off {
                extents.push(Extent { offset: Some(disk_pos), len: next - off });
            }
        }
        let mut size = length;
        // An index stored after the media data is added at the end.
        if let Some((moov_at, moov_len)) = moov_tail {
            used_moovs.insert(moov_at);
            extents.push(Extent { offset: Some(moov_at), len: moov_len });
            size += moov_len;
        }
        // A whole video in one piece is the deep search's to report.
        let one_piece = pieces.len() == 1 && moov_tail.is_none_or(|(m, _)| m == start + length);
        if one_piece && complete {
            return None;
        }
        let ext = match &brand {
            b"qt  " => "mov",
            b"M4V " | b"M4VH" | b"M4VP" => "m4v",
            [b'3', b'g', ..] => "3gp",
            _ => "mp4",
        };
        let percent = (length.saturating_sub(payload) * 100 / end.saturating_sub(payload).max(1)).min(100) as u8;
        Some(Rebuilt { start, ext, extents, size, created: index.created, complete, percent })
    }

    /// The index that belongs to a video whose media data starts at
    /// `payload` (and ends at `mdat_end`, if known): its first chunk is
    /// right at the start of the media data. The nearest one after the
    /// video's start wins.
    fn pick_moov(&self, start: u64, payload: u64, mdat_end: Option<u64>, used: &BTreeSet<u64>) -> Option<(u64, Index)> {
        let mut best: Option<(u64, u64, Index)> = None;
        for (&at, bytes) in &self.moovs {
            if used.contains(&at) {
                continue;
            }
            let Some(ix) = bmff::parse_moov(bytes) else { continue };
            let Some((first, end)) = ix.data_range() else { continue };
            if first < payload || first > payload + (64 << 10) || mdat_end.is_some_and(|m| end > m) {
                continue;
            }
            // Written after the video's data: prefer the closest after its start.
            let distance = if at > start { at - start } else { u64::MAX / 2 + (start - at) };
            if best.as_ref().is_none_or(|b| distance < b.0) {
                best = Some((distance, at, ix));
            }
        }
        best.map(|(_, at, ix)| (at, ix))
    }
}

/// Searches the disk for where the video continues: a boundary (one of
/// `boundaries`, offsets in the file) and the cluster start on the disk (on
/// the grid of `start`) from which frame `i` and the frames around the
/// boundary check out.
///
/// For all boundaries that share the unit header positioning the search,
/// the candidates lie on one grid, so the disk is read once for them.
fn find_piece(
    disk: &dyn ReadAt,
    pieces: &[(u64, u64)],
    start: u64,
    cluster: u64,
    boundaries: &[u64],
    frames: &[(u64, u32, Codec)],
    i: usize,
) -> Option<(u64, u64)> {
    let (off, size, codec) = frames[i];
    // (unit header offset in the file, bytes of its frame from there, codec,
    // the boundaries it positions)
    let mut groups: Vec<(u64, u64, Codec, Vec<u64>)> = Vec::new();
    let before: Vec<u64> = boundaries.iter().copied().filter(|&b| b <= off).collect();
    if !before.is_empty() {
        // The new piece starts before frame i: frame i is its first.
        groups.push((off, u64::from(size), codec, before));
    }
    let after: Vec<u64> = boundaries.iter().copied().filter(|&b| b > off).collect();
    if !after.is_empty() {
        match frames.get(i + 1) {
            // Frame i spans the boundary; the next frame is in the new piece.
            Some(&(o, s, c)) => groups.push((o, u64::from(s), c, after)),
            // The last frame spans it: its next unit after the boundary.
            None => {
                for b in after {
                    if let Some((p, room, c)) = next_unit(disk, pieces, b, &frames[i]) {
                        groups.push((p, room, c, vec![b]));
                    }
                }
            }
        }
    }
    let grid = start % cluster;
    const BLOCK: u64 = 8 << 20;
    let mut buf = vec![0u8; BLOCK as usize];
    for (probe, room, codec, bs) in groups {
        let n = codec.nal_len;
        // Usually after where the current piece would have gone on; a full
        // card makes the camera continue from its start.
        let from = mapped(pieces, bs[0]);
        let shift = probe - bs[bs.len() - 1];
        let ahead = (from + shift, (from + WINDOW + (probe - bs[0])).min(disk.size()));
        let behind = ((from + shift).saturating_sub(WINDOW), from + shift);
        // Positions of the unit header: one cluster apart, on the grid.
        let residue = (grid + probe) % cluster;
        for (lo, hi) in [ahead, behind] {
            let mut pos = lo.saturating_sub(residue).div_ceil(cluster) * cluster + residue;
            while pos < hi {
                let len = (hi - pos).min(BLOCK) as usize;
                read_tolerant(disk, pos, &mut buf[..len]);
                let mut k = 0usize;
                while k + n + 2 <= len {
                    let b = &buf[k..];
                    let unit = b[..n].iter().fold(0u64, |a, &x| a << 8 | u64::from(x));
                    if unit > 0 && unit + n as u64 <= room && b[n] & 0x80 == 0 {
                        let at = pos + k as u64;
                        if let Some(found) = confirm(disk, pieces, frames, probe, at, &bs) {
                            return Some(found);
                        }
                    }
                    k += cluster as usize;
                }
                pos += (len as u64).div_ceil(cluster) * cluster;
            }
        }
    }
    None
}

/// With the unit header at offset `probe` of the file found at `at` on the
/// disk: the first boundary of `bs` for which the frames around it check
/// out, and where its piece starts.
fn confirm(
    disk: &dyn ReadAt,
    pieces: &[(u64, u64)],
    frames: &[(u64, u32, Codec)],
    probe: u64,
    at: u64,
    bs: &[u64],
) -> Option<(u64, u64)> {
    let mut trial = pieces.to_vec();
    trial.push((0, 0));
    for &b in bs {
        let piece = at.checked_sub(probe - b)?;
        *trial.last_mut()? = (b, piece);
        // Frames that span the boundary, and the frame starting at the probe.
        let first = frames.partition_point(|f| f.0 + u64::from(f.1) <= b);
        let spanning = frames[first..].iter().take_while(|f| f.0 < b);
        let at_probe = frames.binary_search_by_key(&probe, |f| f.0).ok().map(|j| &frames[j]);
        let mut checks = spanning.chain(at_probe).peekable();
        if checks.peek().is_some() && checks.all(|&(o, s, c)| frame_mapped(disk, &trial, o, s, c)) {
            return Some((b, piece));
        }
    }
    None
}

/// For the frame `f` that spans `boundary`: the offset of its first unit
/// header after the boundary, and the bytes of the frame from there,
/// worked out from the part before the boundary.
fn next_unit(
    disk: &dyn ReadAt,
    pieces: &[(u64, u64)],
    boundary: u64,
    f: &(u64, u32, Codec),
) -> Option<(u64, u64, Codec)> {
    let &(off, size, codec) = f;
    let end = off + u64::from(size);
    if off >= boundary {
        return None;
    }
    let mut known = vec![0u8; (boundary - off) as usize];
    read_tolerant(disk, mapped(pieces, off), &mut known);
    let n = codec.nal_len;
    let mut p = 0usize;
    while p + n <= known.len() {
        let unit = known[p..p + n].iter().fold(0usize, |a, &b| a << 8 | usize::from(b));
        p += n + unit;
    }
    let at = off + p as u64;
    (at >= boundary && at + n as u64 + 1 < end).then(|| (at, end - at, codec))
}

/// Where offset `x` of the file is on the disk.
fn mapped(pieces: &[(u64, u64)], x: u64) -> u64 {
    let p = pieces[pieces.partition_point(|p| p.0 <= x).saturating_sub(1)];
    p.1 + (x - p.0)
}

/// Whether the frame at offset `off` of the file checks out, read through
/// the pieces (a frame can span two).
fn frame_mapped(disk: &dyn ReadAt, pieces: &[(u64, u64)], off: u64, size: u32, codec: Codec) -> bool {
    frame_checked(disk, pieces, off, size, codec).is_some()
}

/// [`frame_mapped`], with how far into the frame the bytes were checked.
fn frame_checked(disk: &dyn ReadAt, pieces: &[(u64, u64)], off: u64, size: u32, codec: Codec) -> Option<usize> {
    let size = size as usize;
    if size > 64 << 20 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let mut done = 0usize;
    while done < size {
        let x = off + done as u64;
        let k = pieces.partition_point(|p| p.0 <= x).saturating_sub(1);
        let next = pieces.get(k + 1).map_or(u64::MAX, |p| p.0);
        let n = ((next - x) as usize).min(size - done);
        let at = pieces[k].1 + (x - pieces[k].0);
        if at + n as u64 > disk.size() || disk.read_exact_at(at, &mut buf[done..done + n]).is_err() {
            return None;
        }
        done += n;
    }
    bmff::checked_until(&buf, codec)
}

/// True when a video assumed to be in one piece has its frames where its
/// index says (checked at evenly spread frames), or cannot be checked.
pub fn intact(src: &dyn ReadAt, offset: u64, len: u64) -> bool {
    let mut r = Reader::new(src, offset, len);
    let Some((at, size)) = bmff::find_moov(&mut r) else { return true };
    let Ok(bytes) = src.read_vec(offset + at, usize::try_from(size).unwrap_or(usize::MAX).min(MAX_MOOV)) else {
        return true;
    };
    let Some(index) = bmff::parse_moov(&bytes) else { return true };
    let frames = index.video_samples();
    if frames.is_empty() {
        return true;
    }
    let step = frames.len().div_ceil(SPOT_CHECKS).max(1);
    frames
        .iter()
        .step_by(step)
        .chain(frames.last())
        .all(|&(off, size, codec)| off + u64::from(size) <= len && frame_mapped(src, &[(0, offset)], off, size, codec))
}

fn is_video_brand(b: &[u8]) -> bool {
    !matches!(
        b,
        b"heic"
            | b"heix"
            | b"heim"
            | b"heis"
            | b"hevc"
            | b"hevx"
            | b"mif1"
            | b"msf1"
            | b"avif"
            | b"avis"
            | b"crx "
            | b"M4A "
            | b"M4B "
            | b"M4P "
    ) && b.iter().all(|c| c.is_ascii_graphic() || *c == b' ')
}

fn is_video_ext(ext: &str) -> bool {
    matches!(ext, "mp4" | "mov" | "m4v" | "3gp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::bmff::testing::mp4;
    use crate::source::MemSource;

    const CLUSTER: usize = 4096;

    /// Junk that is never a frame.
    fn junk(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| ((i as u8).wrapping_mul(7).wrapping_add(seed)) | 0x80).collect()
    }

    /// Lays `file` out on a disk in clusters, in the order given by
    /// `order` (cluster numbers of the disk), with junk everywhere else.
    fn scatter(file: &[u8], order: &[usize], disk_clusters: usize) -> Vec<u8> {
        let mut disk = junk(disk_clusters * CLUSTER, 3);
        for (k, chunk) in file.chunks(CLUSTER).enumerate() {
            let at = order[k] * CLUSTER;
            disk[at..at + chunk.len()].copy_from_slice(chunk);
        }
        disk
    }

    fn found(disk: &MemSource) -> VideoFinder {
        let mut f = VideoFinder::new();
        // The deep search hands over the disk in blocks.
        for (k, block) in disk.0.chunks(64 << 10).enumerate() {
            f.look(disk, (k * (64 << 10)) as u64, block);
        }
        f
    }

    fn content(disk: &MemSource, v: &Rebuilt) -> Vec<u8> {
        let mut out = Vec::new();
        for e in &v.extents {
            let o = e.offset.unwrap() as usize;
            out.extend_from_slice(&disk.0[o..o + e.len as usize]);
        }
        out
    }

    #[test]
    fn a_video_in_three_pieces_is_put_back_together() {
        // 40 frames of various sizes, about 40 clusters.
        let sizes: Vec<usize> = (0..40).map(|i| 2500 + (i * 977) % 3000).collect();
        let file = mp4(&sizes);
        let n = file.len().div_ceil(CLUSTER);
        // Clusters 10.., then a gap of other data, then 70.., then 130..
        let a = n / 3;
        let b = 2 * n / 3;
        let order: Vec<usize> = (0..n)
            .map(|k| {
                if k < a {
                    10 + k
                } else if k < b {
                    70 + k - a
                } else {
                    130 + k - b
                }
            })
            .collect();
        let disk = MemSource(scatter(&file, &order, 200));
        let mut carved = Vec::new();
        let videos = found(&disk).rebuild(&disk, &mut carved, &|_| CLUSTER as u64, &AtomicBool::new(false));
        assert_eq!(videos.len(), 1);
        let v = &videos[0];
        assert!(v.complete);
        assert_eq!(v.start, 10 * CLUSTER as u64);
        assert_eq!(v.created.unwrap().to_string(), "2024-08-21 18:45:03");
        assert_eq!(v.size, file.len() as u64);
        assert_eq!(content(&disk, v), file);
    }

    #[test]
    fn frames_larger_than_clusters_in_many_pieces() {
        // 60 frames of 5-17 KiB: every break falls inside a frame.
        let sizes: Vec<usize> = (0..60).map(|i| 5000 + (i * 7919) % 12000).collect();
        let file = mp4(&sizes);
        let n = file.len().div_ceil(CLUSTER);
        // Pieces of 7..23 clusters, each after a gap of other data, the
        // last piece (with the index) before the first one on the disk.
        let mut order = Vec::with_capacity(n);
        let (mut at, mut k, mut len) = (400usize, 0usize, 7usize);
        while k < n {
            let take = len.min(n - k);
            if k + take == n {
                at = 5; // the end of the video was written to the start of the disk
            }
            order.extend(at..at + take);
            k += take;
            at += take + 3 + len % 5;
            len = 7 + (len * 5) % 17;
        }
        let disk = MemSource(scatter(&file, &order, 1200));
        let mut carved = Vec::new();
        let videos = found(&disk).rebuild(&disk, &mut carved, &|_| CLUSTER as u64, &AtomicBool::new(false));
        assert_eq!(videos.len(), 1);
        let v = &videos[0];
        assert!(v.complete, "{} pieces", v.extents.len());
        assert_eq!(content(&disk, v), file);
    }

    #[test]
    fn a_video_in_one_piece_is_left_to_the_deep_search() {
        let file = mp4(&[3000; 12]);
        let order: Vec<usize> = (0..file.len().div_ceil(CLUSTER)).map(|k| 5 + k).collect();
        let disk = MemSource(scatter(&file, &order, 40));
        let mut carved = Vec::new();
        let videos = found(&disk).rebuild(&disk, &mut carved, &|_| CLUSTER as u64, &AtomicBool::new(false));
        assert!(videos.is_empty());
        assert!(intact(&disk, 5 * CLUSTER as u64, file.len() as u64));
    }

    #[test]
    fn a_video_whose_rest_is_gone_is_kept_as_far_as_it_goes() {
        let sizes = vec![3000; 30];
        let file = mp4(&sizes);
        let n = file.len().div_ceil(CLUSTER);
        // Only the first half and the index (last clusters) survive.
        let mut disk = junk(120 * CLUSTER, 9);
        for k in 0..n / 2 {
            let src = &file[k * CLUSTER..((k + 1) * CLUSTER).min(file.len())];
            disk[(20 + k) * CLUSTER..(20 + k) * CLUSTER + src.len()].copy_from_slice(src);
        }
        let tail = &file[(n - 2) * CLUSTER..];
        disk[90 * CLUSTER..90 * CLUSTER + tail.len()].copy_from_slice(tail);
        let disk = MemSource(disk);
        let mut carved = Vec::new();
        let videos = found(&disk).rebuild(&disk, &mut carved, &|_| CLUSTER as u64, &AtomicBool::new(false));
        assert_eq!(videos.len(), 1);
        assert!(!videos[0].complete);
        assert!(videos[0].size < file.len() as u64);
    }
}
