//! Repairing MP4/MOV videos whose index is missing.
//!
//! When a camera or phone stops recording abruptly (battery, full card, a
//! crash), the video data is on the card but the index (`moov`) that says
//! where each frame is was never written, and no player opens the file.
//! The index can be rebuilt with the help of a good video recorded with
//! the same camera and settings (the "reference"): it gives the codec
//! settings, the frame rate and the track layout.
//!
//! The damaged file's media data is walked frame by frame. H.264/H.265
//! frames are chains of length-prefixed NAL units, and a new frame starts
//! at an access-unit delimiter, a parameter set or the first slice of a
//! picture. Keyframes are recognised by their NAL type, and the display
//! order of B-frames is worked out from each picture's order count.
//!
//! Uncompressed (PCM) sound, between the video chunks, is restored too.
//! Compressed sound (AAC) cannot be split into its packets without
//! decoding it, so such videos come back without sound.
//!
//! The result is the reference's `ftyp`, the damaged file's media data as
//! it is, and a new `moov`.

use std::io::Write;

use anyhow::{Context, Result, bail};

use crate::bytes::{be16, be32, be64};
use crate::source::ReadAt;

/// What a repair produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Repaired {
    pub frames: usize,
    pub keyframes: usize,
    pub seconds: f64,
    pub sound: Sound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sound {
    Restored,
    /// The reference has sound this repair cannot restore (e.g. AAC).
    Dropped(String),
    /// The reference has no sound.
    None,
}

// ----------------------------------------------------------------------
// Boxes

/// Top-level boxes of a file: (type, start, header length, total length).
fn top_boxes(src: &dyn ReadAt) -> Vec<([u8; 4], u64, u64, u64)> {
    let size = src.size();
    let mut out = Vec::new();
    let mut pos = 0u64;
    while pos + 8 <= size && out.len() < 1024 {
        let mut h = [0u8; 16];
        let n = src.read_at(pos, &mut h).unwrap_or(0);
        if n < 8 {
            break;
        }
        let ty: [u8; 4] = h[4..8].try_into().unwrap_or_default();
        if !ty.iter().all(|c| c.is_ascii_alphanumeric() || *c == b' ' || *c == b'-' || *c == 0xA9) {
            break;
        }
        let (len, hl) = match u64::from(be32(&h, 0).unwrap_or(0)) {
            1 if n >= 16 => (be64(&h, 8).unwrap_or(0), 16),
            0 => (size - pos, 8),
            l => (l, 8),
        };
        // A box that runs past the end: the file was cut off inside it.
        let len = if len < hl || pos + len > size { size - pos } else { len };
        out.push((ty, pos, hl, len));
        pos += len;
    }
    out
}

/// The child boxes of a box body: (type, body start, end) relative to `b`.
fn children(b: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= b.len() {
        let size = be32(b, pos).unwrap_or(0) as usize;
        let ty: [u8; 4] = b[pos + 4..pos + 8].try_into().unwrap_or_default();
        let (size, hl) = match size {
            1 => (be64(b, pos + 8).unwrap_or(0) as usize, 16),
            0 => (b.len() - pos, 8),
            s => (s, 8),
        };
        if size < hl || pos + size > b.len() {
            break;
        }
        out.push((ty, pos + hl, pos + size));
        pos += size;
    }
    out
}

/// The whole box (header included) of the first child of type `ty`.
fn child_box<'a>(b: &'a [u8], ty: &[u8; 4]) -> Option<&'a [u8]> {
    let (_, s, e) = *children(b).iter().find(|c| &c.0 == ty)?;
    let hl = if be32(b, s - 8) == Some(1) { 16 } else { 8 };
    b.get(s - hl..e)
}

fn child_body<'a>(b: &'a [u8], ty: &[u8; 4]) -> Option<&'a [u8]> {
    let (_, s, e) = *children(b).iter().find(|c| &c.0 == ty)?;
    b.get(s..e)
}

fn mk(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 8);
    v.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    v.extend_from_slice(ty);
    v.extend_from_slice(body);
    v
}

fn full(ty: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version, 0, 0, 0];
    b.extend_from_slice(body);
    mk(ty, &b)
}

/// A copy of an mvhd/mdhd/tkhd box with its duration replaced.
fn with_duration(boxed: &[u8], duration: u64) -> Vec<u8> {
    let mut b = boxed.to_vec();
    let ty = &b[4..8];
    let v1 = b.get(8) == Some(&1);
    // Offsets from the start of the box (8-byte header, 4 bytes version/flags).
    let at = match (ty, v1) {
        (b"tkhd", false) => 8 + 4 + 16,
        (b"tkhd", true) => 8 + 4 + 28,
        (_, false) => 8 + 4 + 12,
        (_, true) => 8 + 4 + 20,
    };
    if v1 {
        if let Some(d) = b.get_mut(at..at + 8) {
            d.copy_from_slice(&duration.to_be_bytes());
        }
    } else if let Some(d) = b.get_mut(at..at + 4) {
        d.copy_from_slice(&(duration.min(u64::from(u32::MAX)) as u32).to_be_bytes());
    }
    b
}

fn timescale(boxed: &[u8]) -> Option<u32> {
    // mvhd/mdhd: after version/flags and two (v0: 4-byte, v1: 8-byte) dates.
    let at = if boxed.get(8) == Some(&1) { 8 + 4 + 16 } else { 8 + 4 + 8 };
    be32(boxed, at).filter(|&t| t > 0)
}

// ----------------------------------------------------------------------
// Bit reading (Exp-Golomb), for parameter sets and slice headers.

struct Bits {
    data: Vec<u8>,
    pos: usize,
}

impl Bits {
    /// The payload of a NAL unit without its header and emulation
    /// prevention bytes (00 00 03).
    fn new(nal: &[u8], header: usize) -> Self {
        let mut data = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in nal.iter().skip(header) {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            data.push(b);
        }
        Self { data, pos: 0 }
    }

    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.u(zeros)?)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k & 1 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.data.len() * 8).then_some(())
    }
}

/// What is needed from the sequence (and picture) parameters to read a
/// picture's order count from its slice header.
#[derive(Debug, Clone, Copy, Default)]
struct Params {
    hevc: bool,
    // H.264
    log2_frame_num: u32,
    poc_type: u32,
    log2_poc_lsb: u32,
    frame_mbs_only: bool,
    separate_colour_plane: bool,
    // H.265 (from the picture parameters)
    dependent_slices: bool,
    output_flag: bool,
    extra_slice_bits: u32,
    known: bool,
}

fn h264_sps(nal: &[u8], p: &mut Params) -> Option<()> {
    let mut b = Bits::new(nal, 1);
    let profile = b.u(8)?;
    b.skip(16)?; // constraint flags, level
    b.ue()?; // sps id
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        let chroma = b.ue()?;
        if chroma == 3 {
            p.separate_colour_plane = b.u(1)? == 1;
        }
        b.ue()?; // bit depth luma
        b.ue()?; // bit depth chroma
        b.u(1)?; // qpprime
        if b.u(1)? == 1 {
            // Scaling matrices.
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if b.u(1)? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    p.log2_frame_num = b.ue()? + 4;
    p.poc_type = b.ue()?;
    match p.poc_type {
        0 => p.log2_poc_lsb = b.ue()? + 4,
        1 => {
            b.u(1)?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?; // max ref frames
    b.u(1)?; // gaps allowed
    b.ue()?; // width
    b.ue()?; // height
    p.frame_mbs_only = b.u(1)? == 1;
    p.known = true;
    Some(())
}

fn h265_sps(nal: &[u8], p: &mut Params) -> Option<()> {
    let mut b = Bits::new(nal, 2);
    b.u(4)?; // vps id
    let sub_layers = b.u(3)?;
    b.u(1)?;
    // profile_tier_level
    b.skip(96)?;
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((b.u(1)? == 1, b.u(1)? == 1));
    }
    if sub_layers > 0 {
        b.skip(2 * (8 - sub_layers as usize))?;
    }
    for (profile, level) in present {
        if profile {
            b.skip(88)?;
        }
        if level {
            b.skip(8)?;
        }
    }
    b.ue()?; // sps id
    if b.ue()? == 3 {
        p.separate_colour_plane = b.u(1)? == 1;
    }
    b.ue()?; // width
    b.ue()?; // height
    if b.u(1)? == 1 {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    b.ue()?; // bit depths
    b.ue()?;
    p.log2_poc_lsb = b.ue()? + 4;
    p.hevc = true;
    p.known = true;
    Some(())
}

fn h265_pps(nal: &[u8], p: &mut Params) -> Option<()> {
    let mut b = Bits::new(nal, 2);
    b.ue()?;
    b.ue()?;
    p.dependent_slices = b.u(1)? == 1;
    p.output_flag = b.u(1)? == 1;
    p.extra_slice_bits = b.u(3)?;
    Some(())
}

// ----------------------------------------------------------------------
// The reference

struct RefTrack {
    /// tkhd, mdhd, hdlr, the media header (vmhd/smhd), dinf, stsd.
    tkhd: Vec<u8>,
    mdhd: Vec<u8>,
    hdlr: Vec<u8>,
    media_header: Vec<u8>,
    dinf: Vec<u8>,
    stsd: Vec<u8>,
    timescale: u32,
    /// Duration of one sample (the first stts entry).
    delta: u32,
}

struct Video {
    track: RefTrack,
    nal_len: usize,
    params: Params,
}

struct Audio {
    track: RefTrack,
    /// Bytes of a chunk in the reference (cameras write them all alike).
    chunk: u64,
    /// Bytes of one sample, and the stsz value to write.
    bytes_per_sample: u64,
    stsz_size: u32,
}

struct Reference {
    ftyp: Vec<u8>,
    mvhd: Vec<u8>,
    video: Video,
    audio: Result<Option<Audio>, String>,
    /// Where the media data starts in the reference (a damaged file whose
    /// header is gone most likely starts its data there too).
    data_start: u64,
}

fn read_box(src: &dyn ReadAt, at: u64, len: u64) -> Result<Vec<u8>> {
    anyhow::ensure!(len < 256 << 20, "box too large");
    Ok(src.read_vec(at, len as usize)?)
}

fn reference(src: &dyn ReadAt) -> Result<Reference> {
    let boxes = top_boxes(src);
    let ftyp = match boxes.iter().find(|b| &b.0 == b"ftyp") {
        Some(b) => read_box(src, b.1, b.3)?,
        None => bail!("the reference is not an MP4 or MOV video"),
    };
    let moov = boxes.iter().find(|b| &b.0 == b"moov").context("the reference video has no index either")?;
    let moov = read_box(src, moov.1, moov.3)?;
    let data_start = boxes.iter().find(|b| &b.0 == b"mdat").map_or(0, |b| b.1 + b.2);
    let body = &moov[8..];
    let mvhd = child_box(body, b"mvhd").context("damaged reference index")?.to_vec();
    let (mut video, mut audio) = (None, Ok(None));
    for (ty, s, e) in children(body) {
        if &ty != b"trak" {
            continue;
        }
        let trak = &body[s..e];
        let Some(t) = ref_track(trak) else { continue };
        let handler = t.hdlr.get(16..20).unwrap_or_default().to_vec();
        let entry = t.stsd.get(20..24).unwrap_or_default().to_vec();
        match handler.as_slice() {
            b"vide" if video.is_none() => {
                let stsd_body = &t.stsd[16..];
                let hevc = matches!(entry.as_slice(), b"hvc1" | b"hev1");
                if !hevc && !matches!(entry.as_slice(), b"avc1" | b"avc3") {
                    bail!("only H.264 and H.265 videos can be repaired");
                }
                // The visual sample entry has 78 bytes before its boxes.
                let entry_body = children(stsd_body).first().map(|c| &stsd_body[c.1..c.2]).unwrap_or_default();
                let config = entry_body
                    .get(78..)
                    .and_then(|b| child_body(b, if hevc { b"hvcC" } else { b"avcC" }))
                    .context("the reference video has no codec settings")?;
                let mut params = Params { hevc, ..Params::default() };
                let nal_len = parameter_sets(config, hevc, &mut params);
                video = Some(Video { track: t, nal_len, params });
            }
            b"soun" if matches!(audio, Ok(None)) => audio = pcm(t, &entry, trak),
            _ => {}
        }
    }
    let video = video.context("the reference has no video")?;
    Ok(Reference { ftyp, mvhd, video, audio, data_start })
}

fn ref_track(trak: &[u8]) -> Option<RefTrack> {
    let tkhd = child_box(trak, b"tkhd")?.to_vec();
    let mdia = child_body(trak, b"mdia")?;
    let mdhd = child_box(mdia, b"mdhd")?.to_vec();
    let hdlr = child_box(mdia, b"hdlr")?.to_vec();
    let minf = child_body(mdia, b"minf")?;
    let media_header = child_box(minf, b"vmhd").or_else(|| child_box(minf, b"smhd"))?.to_vec();
    let dinf = child_box(minf, b"dinf")?.to_vec();
    let stbl = child_body(minf, b"stbl")?;
    let stsd = child_box(stbl, b"stsd")?.to_vec();
    let delta = child_body(stbl, b"stts").and_then(|t| be32(t, 12)).unwrap_or(1);
    Some(RefTrack { timescale: timescale(&mdhd)?, tkhd, mdhd, hdlr, media_header, dinf, stsd, delta })
}

/// Reads the parameter sets out of avcC/hvcC. Returns the NAL length size.
fn parameter_sets(c: &[u8], hevc: bool, p: &mut Params) -> usize {
    if hevc {
        let nal_len = usize::from(c.get(21).copied().unwrap_or(3) & 3) + 1;
        let arrays = c.get(22).copied().unwrap_or(0);
        let mut at = 23usize;
        for _ in 0..arrays {
            let ty = c.get(at).copied().unwrap_or(0) & 0x3F;
            let n = be16(c, at + 1).unwrap_or(0);
            at += 3;
            for _ in 0..n {
                let len = usize::from(be16(c, at).unwrap_or(0));
                if let Some(nal) = c.get(at + 2..at + 2 + len) {
                    match ty {
                        33 => _ = h265_sps(nal, p),
                        34 => _ = h265_pps(nal, p),
                        _ => {}
                    }
                }
                at += 2 + len;
            }
        }
        nal_len
    } else {
        let nal_len = usize::from(c.get(4).copied().unwrap_or(3) & 3) + 1;
        let sps = c.get(5).copied().unwrap_or(0) & 0x1F;
        let mut at = 6usize;
        for _ in 0..sps {
            let len = usize::from(be16(c, at).unwrap_or(0));
            if let Some(nal) = c.get(at + 2..at + 2 + len) {
                let _ = h264_sps(nal, p);
            }
            at += 2 + len;
        }
        nal_len
    }
}

/// The reference's sound, if it is uncompressed.
fn pcm(t: RefTrack, entry: &[u8], trak: &[u8]) -> Result<Option<Audio>, String> {
    const PCM: [&[u8; 4]; 9] = [b"sowt", b"twos", b"lpcm", b"in24", b"in32", b"fl32", b"fl64", b"raw ", b"ipcm"];
    if !PCM.iter().any(|k| k.as_slice() == entry) {
        return Err(String::from_utf8_lossy(entry).trim().to_string());
    }
    let stbl = child_body(trak, b"mdia").and_then(|m| child_body(m, b"minf")).and_then(|m| child_body(m, b"stbl"));
    let stsz = stbl.and_then(|s| child_body(s, b"stsz")).and_then(|z| be32(z, 4)).unwrap_or(0);
    // stsc: entry count, then (first chunk, samples per chunk, description).
    // The last entry is the steady state (the first chunks often differ).
    let per_chunk = stbl
        .and_then(|s| child_body(s, b"stsc"))
        .and_then(|c| {
            let n = be32(c, 4)? as usize;
            be32(c, 8 + n.checked_sub(1)? * 12 + 4)
        })
        .unwrap_or(0);
    // Sound sample entry: 8 bytes, then version, …, channels at 16, bits at 18.
    let e = &t.stsd[16..];
    let channels = u64::from(be16(e, 8 + 16).unwrap_or(0));
    let bits = u64::from(be16(e, 8 + 18).unwrap_or(0));
    let frame = channels * bits / 8;
    let bytes_per_sample = if u64::from(stsz) >= frame && stsz > 1 { u64::from(stsz) } else { frame };
    if bytes_per_sample == 0 {
        return Err("PCM".into());
    }
    let chunk = u64::from(per_chunk) * bytes_per_sample;
    Ok(Some(Audio { track: t, chunk, bytes_per_sample, stsz_size: stsz.max(1) }))
}

// ----------------------------------------------------------------------
// Walking the damaged file

struct Frame {
    offset: u64,
    size: u32,
    key: bool,
    /// (picture order count, keyframe group) for the display order.
    order: Option<(u64, i64)>,
}

/// The media data of the damaged file: from after its `mdat` header to
/// the end of that box (or of the file).
fn data_range(src: &dyn ReadAt, fallback: u64) -> (u64, u64) {
    let size = src.size();
    let boxes = top_boxes(src);
    if let Some(m) = boxes.iter().find(|b| &b.0 == b"mdat") {
        return (m.1 + m.2, (m.1 + m.3).min(size));
    }
    // No header left: look for one near the start, else assume the
    // reference's layout.
    if let Ok(head) = src.read_vec(0, size.min(1 << 20) as usize)
        && let Some(i) = memchr::memmem::find(&head, b"mdat")
        && i >= 4
    {
        return ((i + 4) as u64, size);
    }
    (fallback.min(size), size)
}

/// Whether a NAL unit is the first of a new frame, given what came before.
fn starts_frame(nal: &[u8], hevc: bool, seen_picture: bool) -> bool {
    if !seen_picture {
        return false;
    }
    if hevc {
        let ty = (nal[0] >> 1) & 0x3F;
        match ty {
            0..=31 => nal.get(2).is_some_and(|b| b & 0x80 != 0),
            35 | 32 | 33 | 34 | 39 | 41..=44 | 48..=55 => true,
            _ => false,
        }
    } else {
        match nal[0] & 0x1F {
            1 | 5 => nal.get(1).is_some_and(|b| b & 0x80 != 0),
            6..=9 | 14..=18 => true,
            _ => false,
        }
    }
}

fn is_picture(nal: &[u8], hevc: bool) -> bool {
    if hevc { (nal[0] >> 1) & 0x3F <= 31 } else { matches!(nal[0] & 0x1F, 1 | 5) }
}

fn is_key(nal: &[u8], hevc: bool) -> bool {
    if hevc { (16..=21).contains(&((nal[0] >> 1) & 0x3F)) } else { nal[0] & 0x1F == 5 }
}

fn header_ok(nal: &[u8], hevc: bool) -> bool {
    let Some(&h) = nal.first() else { return false };
    if h & 0x80 != 0 {
        return false;
    }
    if hevc {
        nal.get(1).is_some_and(|&b| b & 7 != 0) && (h >> 1) & 0x3F <= 55 && h & 1 == 0 && nal[1] >> 3 == 0
    } else {
        (1..=23).contains(&(h & 0x1F))
    }
}

/// Largest NAL unit believed (a 4K keyframe is a few MB).
const MAX_NAL: u64 = 16 << 20;

/// NAL unit types that may appear in a camera's video track, with the
/// header fields the standards require of them; `len` is the unit's size.
fn type_ok(nal: &[u8], hevc: bool, len: u64) -> bool {
    if hevc {
        // Single-layer streams only: nuh_layer_id is 0.
        let layer = ((nal[0] & 1) << 5) | (nal.get(1).copied().unwrap_or(0) >> 3);
        layer == 0
            && match (nal[0] >> 1) & 0x3F {
                // End of sequence / bitstream: the header only.
                36 | 37 => len == 2,
                0..=9 | 16..=21 | 32..=35 | 38..=40 => len > 2,
                _ => false,
            }
    } else {
        let reference = (nal[0] >> 5) & 3 != 0;
        match nal[0] & 0x1F {
            1 => len > 1,
            // IDR pictures and parameter sets are always referenced.
            5 | 7 | 8 => reference && len > 1,
            // SEI, delimiters and filler never are.
            6 | 9 | 12 => !reference && len > 1,
            // End of sequence / stream: the header only.
            10 | 11 => !reference && len == 1,
            _ => false,
        }
    }
}

/// The NAL unit at `at`: its payload length and its first `peek` bytes, if
/// it looks like one (a plausible length, a valid header, and — as every
/// NAL unit ends with a stop bit — a last byte that is not zero).
fn nal_at(src: &dyn ReadAt, at: u64, end: u64, n: usize, hevc: bool, peek: usize) -> Option<(u64, Vec<u8>)> {
    let mut len = [0u8; 4];
    src.read_exact_at(at, &mut len[..n]).ok()?;
    let l = len[..n].iter().fold(0u64, |a, &b| a << 8 | u64::from(b));
    if !(1..=MAX_NAL).contains(&l) || at + n as u64 + l > end {
        return None;
    }
    let head = src.read_vec(at + n as u64, (l as usize).min(peek)).ok()?;
    if !header_ok(&head, hevc) || !type_ok(&head, hevc, l) {
        return None;
    }
    let mut last = [0u8];
    src.read_exact_at(at + n as u64 + l - 1, &mut last).ok()?;
    (last[0] != 0).then_some((l, head))
}

/// What a picture's first slice header says.
#[derive(Debug, Clone, Copy)]
struct Slice {
    pps: u32,
    slice_type: u32,
    /// H.264 frame_num.
    frame_num: Option<u32>,
    poc_lsb: Option<u32>,
    idr: bool,
    reference: bool,
}

/// The header of the first slice of a picture (`None` for other slices).
fn slice(nal: &[u8], p: &Params) -> Option<Slice> {
    if p.hevc {
        let ty = (nal[0] >> 1) & 0x3F;
        let mut b = Bits::new(nal, 2);
        if b.u(1)? != 1 {
            return None;
        }
        if (16..=23).contains(&ty) {
            b.u(1)?;
        }
        let pps = b.ue()?;
        b.skip(p.extra_slice_bits as usize)?;
        let slice_type = b.ue()?;
        if p.output_flag {
            b.u(1)?;
        }
        if p.separate_colour_plane {
            b.u(2)?;
        }
        let idr = ty == 19 || ty == 20;
        let poc_lsb = if idr || !p.known { None } else { Some(b.u(p.log2_poc_lsb)?) };
        // Even types below 16 are pictures nothing refers to.
        let reference = !(ty < 16 && ty.is_multiple_of(2));
        Some(Slice { pps, slice_type, frame_num: None, poc_lsb, idr, reference })
    } else {
        let mut b = Bits::new(nal, 1);
        if b.ue()? != 0 {
            return None;
        }
        let slice_type = b.ue()?;
        let pps = b.ue()?;
        if !p.known {
            return Some(Slice { pps, slice_type, frame_num: None, poc_lsb: None, idr: false, reference: true });
        }
        if p.separate_colour_plane {
            b.u(2)?;
        }
        let frame_num = b.u(p.log2_frame_num)?;
        if !p.frame_mbs_only && b.u(1)? == 1 {
            b.u(1)?;
        }
        let idr = nal[0] & 0x1F == 5;
        if idr {
            b.ue()?;
        }
        let poc_lsb = if p.poc_type == 0 { Some(b.u(p.log2_poc_lsb)?) } else { None };
        Some(Slice { pps, slice_type, frame_num: Some(frame_num), poc_lsb, idr, reference: nal[0] & 0x60 != 0 })
    }
}

/// What the next picture is expected to look like, from the ones before.
#[derive(Debug, Default, Clone, Copy)]
struct Expect {
    pps: Option<u32>,
    ref_frame_num: Option<u32>,
    poc_lsb: Option<u32>,
}

impl Expect {
    /// Whether `s` can be the next picture. Random data passes this very
    /// rarely, which tells video from the sound and data around it.
    fn fits(&self, s: &Slice, p: &Params) -> bool {
        if s.slice_type > 9 || (p.hevc && s.slice_type > 2) || s.pps > 63 {
            return false;
        }
        if self.pps.is_some_and(|pps| pps != s.pps) {
            return false;
        }
        if let (Some(f), Some(prev)) = (s.frame_num, self.ref_frame_num) {
            let max = 1u32 << p.log2_frame_num;
            let ok = if s.idr { f == 0 } else { f == prev || f == (prev + 1) % max };
            if !ok {
                return false;
            }
        }
        if p.hevc
            && !s.idr
            && let (Some(l), Some(prev)) = (s.poc_lsb, self.poc_lsb)
        {
            let max = 1i64 << p.log2_poc_lsb;
            let d = (i64::from(l) - i64::from(prev)).rem_euclid(max);
            if d > 64 && d < max - 64 {
                return false;
            }
        }
        true
    }

    fn update(&mut self, s: &Slice) {
        self.pps = Some(s.pps);
        if s.reference {
            self.ref_frame_num = s.frame_num.or(self.ref_frame_num);
        }
        self.poc_lsb = if s.idr { Some(0) } else { s.poc_lsb.or(self.poc_lsb) };
    }
}

/// Whether a NAL unit can follow the first slice of a picture within the
/// same frame: another slice of that picture, or filler and end markers.
fn continues_picture(nal: &[u8], p: &Params, expect: &Expect) -> bool {
    let same_pps = |pps: u32| expect.pps.is_none_or(|e| e == pps);
    if p.hevc {
        let ty = (nal[0] >> 1) & 0x3F;
        match ty {
            36..=38 | 40 => true,
            0..=9 | 16..=21 => {
                let mut b = Bits::new(nal, 2);
                if b.u(1) != Some(0) {
                    return false;
                }
                if (16..=23).contains(&ty) && b.u(1).is_none() {
                    return false;
                }
                b.ue().is_some_and(same_pps)
            }
            _ => false,
        }
    } else {
        match nal[0] & 0x1F {
            10..=12 => true,
            1 | 5 => {
                let mut b = Bits::new(nal, 1);
                b.ue().is_some_and(|mb| mb > 0) && b.ue().is_some_and(|t| t <= 9) && b.ue().is_some_and(same_pps)
            }
            _ => false,
        }
    }
}

/// Whether a frame starts at `at`: NAL units up to the first picture,
/// which must fit after the pictures before it.
fn video_starts(src: &dyn ReadAt, at: u64, end: u64, v: &Video, params: &Params, expect: &Expect) -> bool {
    let hevc = params.hevc;
    let mut pos = at;
    for i in 0..8 {
        let Some((l, head)) = nal_at(src, pos, end, v.nal_len, hevc, 64) else { return false };
        if is_picture(&head, hevc) {
            return slice(&head, params).is_some_and(|s| expect.fits(&s, params));
        }
        if i == 0 && !starts_frame(&head, hevc, true) {
            return false;
        }
        pos += v.nal_len as u64 + l;
    }
    false
}

/// Where the video continues after a gap that starts at `gap`: first after
/// whole uncompressed sound chunks (their size is known exactly), then
/// byte by byte, nearest first.
#[allow(clippy::too_many_arguments)]
fn next_run(
    src: &dyn ReadAt,
    gap: u64,
    end: u64,
    v: &Video,
    params: &Params,
    expect: &Expect,
    steps: &[u64],
) -> Option<u64> {
    // Nearest first, so that no frame is skipped.
    let mut candidates: Vec<u64> = steps
        .iter()
        .filter(|&&s| s > 0)
        .flat_map(|&s| (1..=4u64).map(move |m| gap + m * s))
        .filter(|&at| at < end)
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    if let Some(&at) = candidates.iter().find(|&&at| video_starts(src, at, end, v, params, expect)) {
        return Some(at);
    }
    const STEP: usize = 1 << 20;
    let n = v.nal_len;
    let mut base = gap + 1;
    while base < end {
        let len = ((end - base) as usize).min(STEP + 8);
        let buf = src.read_vec(base, len).ok()?;
        for i in 0..buf.len().saturating_sub(n + 2).min(STEP) {
            // A cheap first test: a small length and a valid header.
            let l = buf[i..i + n].iter().fold(0u64, |a, &b| a << 8 | u64::from(b));
            if !(2..=MAX_NAL).contains(&l) || !header_ok(&buf[i + n..], params.hevc) {
                continue;
            }
            if video_starts(src, base + i as u64, end, v, params, expect) {
                return Some(base + i as u64);
            }
        }
        base += STEP as u64;
    }
    None
}

/// The frames, and the gaps between runs of them (other tracks' chunks).
fn walk(
    src: &dyn ReadAt,
    start: u64,
    end: u64,
    v: &Video,
    sound_chunk: u64,
    cancel: &dyn Fn() -> bool,
) -> (Vec<Frame>, Vec<(u64, u64)>) {
    let hevc = v.params.hevc;
    let n = v.nal_len as u64;
    let mut params = v.params;
    let mut expect = Expect::default();
    let mut frames: Vec<Frame> = Vec::new();
    let mut gaps = Vec::new();
    let mut pos = start;
    let mut cur: Option<Frame> = None;
    let mut seen_picture = false;
    let (mut group, mut prev_lsb, mut msb) = (0i64, 0u32, 0i64);
    let finish = |cur: &mut Option<Frame>, frames: &mut Vec<Frame>| {
        if let Some(f) = cur.take().filter(|f| f.size > 0) {
            frames.push(f);
        }
    };
    while pos < end && !cancel() {
        let nal = nal_at(src, pos, end, v.nal_len, hevc, 64);
        // A new picture must fit after the ones before it; anything else
        // ends the run of video.
        let accepted = nal.filter(|(_, head)| {
            if cur.is_none() || starts_frame(head, hevc, seen_picture) {
                // A new frame: it must lead to a picture that fits.
                video_starts(src, pos, end, v, &params, &expect)
            } else if seen_picture {
                continues_picture(head, &params, &expect)
            } else {
                // Between the start of the frame and its picture, already
                // checked when the frame started.
                true
            }
        });
        let Some((l, head)) = accepted else {
            finish(&mut cur, &mut frames);
            seen_picture = false;
            match next_run(src, pos, end, v, &params, &expect, &[sound_chunk]) {
                Some(p) => {
                    gaps.push((pos, p));
                    pos = p;
                }
                None => {
                    gaps.push((pos, end));
                    break;
                }
            }
            continue;
        };
        if starts_frame(&head, hevc, seen_picture) {
            finish(&mut cur, &mut frames);
            seen_picture = false;
        }
        // In-band parameter sets (avc3/hev1) override the reference's.
        let ty = if hevc { (head[0] >> 1) & 0x3F } else { head[0] & 0x1F };
        if (!hevc && ty == 7) || (hevc && (ty == 33 || ty == 34)) {
            let full = src.read_vec(pos + n, l.min(4096) as usize).unwrap_or_default();
            let _ = match (hevc, ty) {
                (false, _) => h264_sps(&full, &mut params),
                (true, 33) => h265_sps(&full, &mut params),
                _ => h265_pps(&full, &mut params),
            };
        }
        let f = cur.get_or_insert(Frame { offset: pos, size: 0, key: false, order: None });
        f.size += (n + l) as u32;
        if is_picture(&head, hevc) && !seen_picture {
            seen_picture = true;
            f.key = is_key(&head, hevc);
            let s = slice(&head, &params);
            if let Some(s) = &s {
                expect.update(s);
            }
            // Display order: restart at IDR pictures, follow the LSB.
            f.order = match s {
                Some(s) if s.idr => {
                    group += 1;
                    prev_lsb = s.poc_lsb.unwrap_or(0);
                    msb = 0;
                    Some((group as u64, i64::from(prev_lsb)))
                }
                Some(Slice { poc_lsb: Some(lsb), .. }) if params.known => {
                    let max = 1i64 << params.log2_poc_lsb;
                    let (l, p) = (i64::from(lsb), i64::from(prev_lsb));
                    if l < p && p - l >= max / 2 {
                        msb += max;
                    } else if l > p && l - p > max / 2 {
                        msb -= max;
                    }
                    prev_lsb = lsb;
                    Some((group as u64, msb + l))
                }
                _ => None,
            };
        }
        pos += n + l;
    }
    finish(&mut cur, &mut frames);
    (frames, gaps)
}

// ----------------------------------------------------------------------
// Writing the new index

fn run_length(values: impl IntoIterator<Item = u32>) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for v in values {
        match out.last_mut() {
            Some((n, last)) if *last == v => *n += 1,
            _ => out.push((1, v)),
        }
    }
    out
}

fn table(entries: &[(u32, u32)]) -> Vec<u8> {
    let mut b = (entries.len() as u32).to_be_bytes().to_vec();
    for (a, c) in entries {
        b.extend_from_slice(&a.to_be_bytes());
        b.extend_from_slice(&c.to_be_bytes());
    }
    b
}

/// A track box from the reference's descriptions and new sample tables.
#[allow(clippy::too_many_arguments)]
fn trak(t: &RefTrack, movie_duration: u64, media_duration: u64, stbl: Vec<u8>, edts: Option<Vec<u8>>) -> Vec<u8> {
    let minf = mk(b"minf", &[t.media_header.clone(), t.dinf.clone(), mk(b"stbl", &stbl)].concat());
    let mdia = mk(b"mdia", &[with_duration(&t.mdhd, media_duration), t.hdlr.clone(), minf].concat());
    let mut body = with_duration(&t.tkhd, movie_duration);
    if let Some(e) = edts {
        body.extend(e);
    }
    body.extend(mdia);
    mk(b"trak", &body)
}

fn chunk_offsets(offsets: &[u64]) -> Vec<u8> {
    let mut b = (offsets.len() as u32).to_be_bytes().to_vec();
    for o in offsets {
        b.extend_from_slice(&o.to_be_bytes());
    }
    full(b"co64", 0, &b)
}

/// Repairs `damaged` with the index of `reference` (a good video from the
/// same camera), writing the repaired video to `out`.
pub fn repair_video(
    damaged: &dyn ReadAt,
    reference_video: &dyn ReadAt,
    out: &mut dyn Write,
    cancel: &dyn Fn() -> bool,
) -> Result<Repaired> {
    let r = reference(reference_video)?;
    let v = &r.video;
    let (start, end) = data_range(damaged, r.data_start);
    let sound_chunk = r.audio.as_ref().ok().and_then(|a| a.as_ref()).map_or(0, |a| a.chunk);
    let (frames, gaps) = walk(damaged, start, end, v, sound_chunk, cancel);
    if frames.len() < 2 {
        bail!("no video frames were found in this file, or it was recorded with other settings than the reference");
    }
    let movie_scale = timescale(&r.mvhd).unwrap_or(1000);

    // The new file: ftyp, a 64-bit mdat with the media data as it is, moov.
    let data_out = r.ftyp.len() as u64 + 16;
    let place = |o: u64| data_out + (o - start);

    // Video sample tables.
    let delta = v.track.delta.max(1);
    let sizes: Vec<u32> = frames.iter().map(|f| f.size).collect();
    let mut stbl = v.track.stsd.clone();
    stbl.extend(full(b"stts", 0, &table(&[(frames.len() as u32, delta)])));
    // Display order from the picture order counts, where known.
    let mut ctts = None;
    let mut shift = 0i64;
    if frames.iter().all(|f| f.order.is_some()) {
        let mut by_display: Vec<usize> = (0..frames.len()).collect();
        by_display.sort_by_key(|&i| frames[i].order);
        let mut display = vec![0i64; frames.len()];
        for (rank, &i) in by_display.iter().enumerate() {
            display[i] = rank as i64;
        }
        shift = (0..frames.len()).map(|i| i as i64 - display[i]).max().unwrap_or(0).max(0);
        if display.iter().enumerate().any(|(i, &d)| d != i as i64) {
            let offsets = (0..frames.len()).map(|i| ((display[i] + shift - i as i64) * i64::from(delta)) as u32);
            ctts = Some(full(b"ctts", 0, &table(&run_length(offsets))));
        }
    }
    if let Some(c) = &ctts {
        stbl.extend_from_slice(c);
    }
    let keys: Vec<u32> = frames.iter().enumerate().filter(|(_, f)| f.key).map(|(i, _)| i as u32 + 1).collect();
    if keys.len() < frames.len() {
        let mut b = (keys.len() as u32).to_be_bytes().to_vec();
        keys.iter().for_each(|k| b.extend_from_slice(&k.to_be_bytes()));
        stbl.extend(full(b"stss", 0, &b));
    }
    stbl.extend(full(b"stsc", 0, &{
        let mut b = 1u32.to_be_bytes().to_vec();
        [1u32, 1, 1].iter().for_each(|x| b.extend_from_slice(&x.to_be_bytes()));
        b
    }));
    let mut z = vec![0u8; 4];
    z.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    sizes.iter().for_each(|s| z.extend_from_slice(&s.to_be_bytes()));
    stbl.extend(full(b"stsz", 0, &z));
    stbl.extend(chunk_offsets(&frames.iter().map(|f| place(f.offset)).collect::<Vec<_>>()));

    let media = frames.len() as u64 * u64::from(delta);
    let seconds = media as f64 / f64::from(v.track.timescale);
    let movie = (seconds * f64::from(movie_scale)).round() as u64;
    // B-frames: the edit list starts the movie at the first picture shown.
    let edts = (shift > 0).then(|| {
        let mut e = 1u32.to_be_bytes().to_vec();
        e.extend_from_slice(&(movie.min(u64::from(u32::MAX)) as u32).to_be_bytes());
        e.extend_from_slice(&((shift * i64::from(delta)) as u32).to_be_bytes());
        e.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        mk(b"edts", &full(b"elst", 0, &e))
    });
    let mut traks = trak(&v.track, movie, media, stbl, edts);

    // Sound, in the gaps between the runs of video.
    let sound = match &r.audio {
        Ok(Some(a)) => {
            let chunks: Vec<(u64, u32)> =
                gaps.iter().map(|&(s, e)| (s, ((e - s) / a.bytes_per_sample) as u32)).filter(|&(_, n)| n > 0).collect();
            if chunks.is_empty() {
                Sound::Dropped("no sound was found between the frames".into())
            } else {
                let total: u64 = chunks.iter().map(|c| u64::from(c.1)).sum();
                let mut stbl = a.track.stsd.clone();
                stbl.extend(full(b"stts", 0, &table(&[(total as u32, a.track.delta.max(1))])));
                let mut stsc = Vec::new();
                let mut last = None;
                for (i, &(_, n)) in chunks.iter().enumerate() {
                    if last != Some(n) {
                        stsc.push((i as u32 + 1, n));
                        last = Some(n);
                    }
                }
                let mut b = (stsc.len() as u32).to_be_bytes().to_vec();
                for (first, n) in stsc {
                    [first, n, 1].iter().for_each(|x| b.extend_from_slice(&x.to_be_bytes()));
                }
                stbl.extend(full(b"stsc", 0, &b));
                let mut z = a.stsz_size.to_be_bytes().to_vec();
                z.extend_from_slice(&(total as u32).to_be_bytes());
                stbl.extend(full(b"stsz", 0, &z));
                stbl.extend(chunk_offsets(&chunks.iter().map(|c| place(c.0)).collect::<Vec<_>>()));
                let media = total * u64::from(a.track.delta.max(1));
                traks.extend(trak(&a.track, movie, media, stbl, None));
                Sound::Restored
            }
        }
        Ok(None) => Sound::None,
        Err(kind) => Sound::Dropped(format!("{kind} sound cannot be restored")),
    };

    let moov = mk(b"moov", &[with_duration(&r.mvhd, movie), traks].concat());
    out.write_all(&r.ftyp)?;
    out.write_all(&1u32.to_be_bytes())?;
    out.write_all(b"mdat")?;
    out.write_all(&(16 + (end - start)).to_be_bytes())?;
    let mut pos = start;
    let mut buf = vec![0u8; 1 << 20];
    while pos < end {
        let n = ((end - pos) as usize).min(buf.len());
        crate::source::read_tolerant(damaged, pos, &mut buf[..n]);
        out.write_all(&buf[..n])?;
        pos += n as u64;
    }
    out.write_all(&moov)?;
    Ok(Repaired { frames: frames.len(), keyframes: keys.len(), seconds, sound })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // 1 → 0, 010 → 1, 011 → 2, 00100 → 3
        let mut b = Bits { data: vec![0b1010_0110, 0b0100_0000], pos: 0 };
        assert_eq!(b.ue(), Some(0));
        assert_eq!(b.ue(), Some(1));
        assert_eq!(b.ue(), Some(2));
        assert_eq!(b.ue(), Some(3));
        // Emulation prevention bytes are removed.
        assert_eq!(Bits::new(&[0x65, 0, 0, 3, 1], 1).data, vec![0, 0, 1]);
    }

    #[test]
    fn run_lengths() {
        assert_eq!(run_length([5, 5, 5, 2, 2, 5]), vec![(3, 5), (2, 2), (1, 5)]);
    }
}
