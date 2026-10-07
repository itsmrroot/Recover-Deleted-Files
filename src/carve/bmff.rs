//! The sample index of an MP4/MOV file (`moov`), and a test that a piece
//! of data really is the video frame the index says should be there.
//!
//! Cameras and phones store H.264/H.265 video as length-prefixed NAL units:
//! every frame is a chain of `<length><unit>` that ends exactly at the
//! frame's size, each unit starting with a valid header. Random data or
//! another file almost never passes that test, which makes it a reliable
//! way to tell where a video's data really is (see `crate::fragments`) and
//! whether a video is intact (see `crate::verify`).

use chrono::NaiveDateTime;

use crate::bytes::{be32, be64};

/// One chunk of a track: where it is in the file, and the sizes of the
/// samples (frames) stored in it back to back.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub offset: u64,
    pub samples: Vec<u32>,
}

/// How a video track's frames can be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codec {
    /// Bytes of each NAL unit's length prefix (1, 2 or 4).
    pub nal_len: usize,
    pub hevc: bool,
}

#[derive(Debug, Clone)]
pub struct Track {
    /// Set for H.264/H.265 video, whose frames can be checked.
    pub codec: Option<Codec>,
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, Default)]
pub struct Index {
    pub tracks: Vec<Track>,
    pub created: Option<NaiveDateTime>,
}

impl Index {
    /// Where the media data starts and ends (first chunk, end of the last).
    pub fn data_range(&self) -> Option<(u64, u64)> {
        let chunks = self.tracks.iter().flat_map(|t| &t.chunks);
        let start = chunks.clone().map(|c| c.offset).min()?;
        let end = chunks.map(|c| c.offset + c.samples.iter().map(|&s| u64::from(s)).sum::<u64>()).max()?;
        Some((start, end))
    }

    /// Every checkable video frame: (offset in the file, size, codec), in
    /// file order.
    pub fn video_samples(&self) -> Vec<(u64, u32, Codec)> {
        let mut out = Vec::new();
        for t in &self.tracks {
            let Some(codec) = t.codec else { continue };
            for c in &t.chunks {
                let mut at = c.offset;
                for &s in &c.samples {
                    out.push((at, s, codec));
                    at += u64::from(s);
                }
            }
        }
        out.sort_unstable_by_key(|s| s.0);
        out
    }
}

/// The boxes inside `b[start..end]`: (type, body start, end).
fn children(b: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 8 <= end {
        let Some(size32) = be32(b, pos) else { break };
        let ty: [u8; 4] = b[pos + 4..pos + 8].try_into().unwrap_or_default();
        let (size, header) = match size32 {
            1 => match be64(b, pos + 8) {
                Some(s) => (s as usize, 16),
                None => break,
            },
            0 => (end - pos, 8),
            s => (s as usize, 8),
        };
        if size < header || pos + size > end {
            break;
        }
        out.push((ty, pos + header, pos + size));
        pos += size;
    }
    out
}

fn child(b: &[u8], start: usize, end: usize, ty: &[u8; 4]) -> Option<(usize, usize)> {
    children(b, start, end).into_iter().find(|c| &c.0 == ty).map(|c| (c.1, c.2))
}

/// Reads the sample index from a whole `moov` box (header included).
pub fn parse_moov(moov: &[u8]) -> Option<Index> {
    let (_, body, end) = *children(moov, 0, moov.len()).first().filter(|c| &c.0 == b"moov")?;
    let mut index = Index::default();
    for (ty, s, e) in children(moov, body, end) {
        match &ty {
            b"mvhd" => index.created = mvhd_date(&moov[s..e]),
            b"trak" => {
                if let Some(t) = parse_trak(moov, s, e) {
                    index.tracks.push(t);
                }
            }
            _ => {}
        }
    }
    (!index.tracks.is_empty()).then_some(index)
}

fn mvhd_date(v: &[u8]) -> Option<NaiveDateTime> {
    let secs = if v.first()? == &1 { be64(v, 4)? } else { u64::from(be32(v, 4)?) };
    // Seconds since 1904-01-01.
    let unix = secs.checked_sub(2_082_844_800)?;
    let t = chrono::DateTime::from_timestamp(unix as i64, 0)?.naive_utc();
    (1990..=2100).contains(&chrono::Datelike::year(&t)).then_some(t)
}

fn parse_trak(b: &[u8], s: usize, e: usize) -> Option<Track> {
    let (ms, me) = child(b, s, e, b"mdia")?;
    let (hs, _) = child(b, ms, me, b"hdlr")?;
    let video = b.get(hs + 8..hs + 12)? == b"vide";
    let (ns, ne) = child(b, ms, me, b"minf")?;
    let (ts, te) = child(b, ns, ne, b"stbl")?;
    let codec = if video { child(b, ts, te, b"stsd").and_then(|(ds, de)| codec(b, ds, de)) } else { None };

    let (offsets, sizes, stsc) = {
        let offsets: Vec<u64> = if let Some((cs, _)) = child(b, ts, te, b"stco") {
            let n = be32(b, cs + 4)? as usize;
            (0..n).map(|i| be32(b, cs + 8 + i * 4).map(u64::from)).collect::<Option<_>>()?
        } else {
            let (cs, _) = child(b, ts, te, b"co64")?;
            let n = be32(b, cs + 4)? as usize;
            (0..n).map(|i| be64(b, cs + 8 + i * 8)).collect::<Option<_>>()?
        };
        let (zs, _) = child(b, ts, te, b"stsz")?;
        let (uniform, count) = (be32(b, zs + 4)?, be32(b, zs + 8)? as usize);
        let sizes: Vec<u32> = if uniform != 0 {
            vec![uniform; count]
        } else {
            (0..count).map(|i| be32(b, zs + 12 + i * 4)).collect::<Option<_>>()?
        };
        let (ss, _) = child(b, ts, te, b"stsc")?;
        let n = be32(b, ss + 4)? as usize;
        let stsc: Vec<(u32, u32)> =
            (0..n).map(|i| Some((be32(b, ss + 8 + i * 12)?, be32(b, ss + 12 + i * 12)?))).collect::<Option<_>>()?;
        (offsets, sizes, stsc)
    };
    // stsc: from chunk `first` (1-based) on, `per` samples a chunk.
    let mut chunks = Vec::with_capacity(offsets.len());
    let mut next = 0usize;
    for (i, &offset) in offsets.iter().enumerate() {
        let chunk_no = i as u32 + 1;
        let per = stsc.iter().rev().find(|(first, _)| *first <= chunk_no).map_or(0, |e| e.1) as usize;
        let end = (next + per).min(sizes.len());
        chunks.push(Chunk { offset, samples: sizes[next..end].to_vec() });
        next = end;
    }
    Some(Track { codec, chunks })
}

/// H.264 (`avc1`/`avc3` with `avcC`) or H.265 (`hvc1`/`hev1` with `hvcC`).
fn codec(b: &[u8], s: usize, e: usize) -> Option<Codec> {
    // stsd: version/flags, entry count, then sample entries; a visual
    // sample entry has 78 bytes before its child boxes.
    let (ty, es, ee) = *children(b, s + 8, e).first()?;
    let hevc = match &ty {
        b"avc1" | b"avc3" => false,
        b"hvc1" | b"hev1" => true,
        _ => return None,
    };
    let (cs, _) = child(b, es + 78, ee, if hevc { b"hvcC" } else { b"avcC" })?;
    let nal_len = usize::from(b.get(cs + if hevc { 21 } else { 4 })? & 3) + 1;
    (nal_len != 3).then_some(Codec { nal_len, hevc })
}

/// True when `data` is one frame: length-prefixed NAL units with valid
/// headers, ending exactly at the end of the data.
pub fn is_frame(data: &[u8], codec: Codec) -> bool {
    checked_until(data, codec).is_some()
}

/// For a frame, how far into it the bytes were actually checked: up to the
/// end of its last unit's header. What follows is picture data, which
/// could be anything.
pub fn checked_until(data: &[u8], codec: Codec) -> Option<usize> {
    let n = codec.nal_len;
    let header = if codec.hevc { 2 } else { 1 };
    let mut p = 0usize;
    let mut last = None;
    while p < data.len() {
        let len = data.get(p..p + n)?.iter().fold(0usize, |a, &b| a << 8 | usize::from(b));
        last = Some(p + n + header);
        p += n;
        if len == 0 || p + len > data.len() || !nal_header_ok(&data[p..p + len], codec.hevc) {
            return None;
        }
        p += len;
    }
    last
}

fn nal_header_ok(unit: &[u8], hevc: bool) -> bool {
    let Some(&h) = unit.first() else { return false };
    if h & 0x80 != 0 {
        return false; // forbidden_zero_bit
    }
    if hevc {
        // Types 0..40 are defined; the temporal id is never 0.
        let ty = (h >> 1) & 0x3F;
        unit.get(1).is_some_and(|&b| b & 7 != 0) && ty <= 40
    } else {
        // Types 1..23 are defined.
        (1..=23).contains(&(h & 0x1F))
    }
}

/// The position of the top-level `moov` box in `head` (the start of a file).
pub fn find_moov(r: &mut crate::carve::Reader) -> Option<(u64, u64)> {
    let mut pos = 0u64;
    while pos + 8 <= r.limit() {
        let size32 = u64::from(r.be32(pos)?);
        let ty = r.bytes(pos + 4, 4)?.to_vec();
        let size = match size32 {
            1 => r.be64(pos + 8)?,
            0 => r.limit() - pos,
            s => s,
        };
        if size < 8 {
            return None;
        }
        if ty == b"moov" {
            return Some((pos, size));
        }
        pos += size;
    }
    None
}

#[cfg(test)]
pub mod testing {
    //! Builds small but real MP4 files: an H.264 track whose frames are
    //! valid length-prefixed NAL units.

    /// A box: size, type, body.
    pub fn bx(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(ty);
        b.extend_from_slice(body);
        b
    }

    /// A frame of `len` bytes: NAL units of up to 300 bytes, filled with
    /// `seed`-dependent bytes.
    pub fn frame(len: usize, seed: u8) -> Vec<u8> {
        let mut f = Vec::with_capacity(len);
        let mut left = len;
        let mut i = 0u8;
        while left > 0 {
            let unit = (left - 4).min(300);
            f.extend_from_slice(&(unit as u32).to_be_bytes());
            f.push(if i == 0 { 0x65 } else { 0x41 }); // IDR slice, then slices
            f.extend((1..unit).map(|k| (k as u8).wrapping_mul(13).wrapping_add(seed) | 1));
            left -= 4 + unit;
            i = i.wrapping_add(1);
        }
        f
    }

    /// `moov` for one H.264 track whose chunks (one frame each) are at
    /// `offsets` with sizes `sizes`, recorded on 2024-08-21 18:45:03 UTC.
    pub fn moov(offsets: &[u32], sizes: &[u32]) -> Vec<u8> {
        let mut mvhd = vec![0u8; 100];
        mvhd[4..8].copy_from_slice(&(1_724_265_903u32 + 2_082_844_800).to_be_bytes());
        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"vide");
        let mut avcc = vec![1, 0x64, 0, 0x28, 0xFF]; // NAL length size 4
        avcc.extend_from_slice(&[0xE0, 0]);
        let mut entry = vec![0u8; 78];
        entry.extend(bx(b"avcC", &avcc));
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(bx(b"avc1", &entry));
        let mut stco = vec![0, 0, 0, 0];
        stco.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        offsets.iter().for_each(|o| stco.extend_from_slice(&o.to_be_bytes()));
        let mut stsz = vec![0, 0, 0, 0, 0, 0, 0, 0];
        stsz.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        sizes.iter().for_each(|s| stsz.extend_from_slice(&s.to_be_bytes()));
        let mut stsc = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsc.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]);
        let stbl = [bx(b"stsd", &stsd), bx(b"stco", &stco), bx(b"stsz", &stsz), bx(b"stsc", &stsc)].concat();
        let minf = bx(b"stbl", &stbl);
        let mdia = [bx(b"hdlr", &hdlr), bx(b"minf", &minf)].concat();
        let trak = bx(b"mdia", &mdia);
        bx(b"moov", &[bx(b"mvhd", &mvhd), bx(b"trak", &trak)].concat())
    }

    /// A whole MP4 (ftyp, mdat, moov at the end) with `frames` frames of
    /// the given sizes.
    pub fn mp4(frames: &[usize]) -> Vec<u8> {
        let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomavc1mp41");
        let data: Vec<u8> = frames.iter().enumerate().flat_map(|(i, &n)| frame(n, i as u8)).collect();
        let mdat_start = ftyp.len() as u32 + 8;
        let mut offsets = Vec::new();
        let mut at = mdat_start;
        for &n in frames {
            offsets.push(at);
            at += n as u32;
        }
        let sizes: Vec<u32> = frames.iter().map(|&n| n as u32).collect();
        [ftyp, bx(b"mdat", &data), moov(&offsets, &sizes)].concat()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn reads_the_index_and_recognises_frames() {
        let file = mp4(&[700, 1200, 500]);
        let moov_at = file.len() - moov(&[0; 3], &[0; 3]).len();
        let index = parse_moov(&file[moov_at..]).unwrap();
        assert_eq!(index.created.unwrap().to_string(), "2024-08-21 18:45:03");
        let frames = index.video_samples();
        assert_eq!(frames.len(), 3);
        for (offset, size, codec) in &frames {
            let data = &file[*offset as usize..*offset as usize + *size as usize];
            assert!(is_frame(data, *codec));
        }
        let (start, end) = index.data_range().unwrap();
        assert_eq!((start, end), (frames[0].0, moov_at as u64));
        // Shifted by one byte, or cut short, it is no frame.
        let (o, s, c) = frames[1];
        assert!(!is_frame(&file[o as usize + 1..(o + u64::from(s)) as usize + 1], c));
        assert!(!is_frame(&file[o as usize..(o + u64::from(s)) as usize - 1], c));
        assert!(!is_frame(&[0x37; 700], c));
    }
}
