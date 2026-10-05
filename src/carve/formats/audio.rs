//! Audio: MP3 (MPEG audio frames, optional ID3 tags) and Ogg (Vorbis, Opus).

use crate::carve::{Category, Format, Hit, Reader};

const MIB: u64 = 1 << 20;

pub struct Mp3;

/// Length in bytes of the MPEG audio frame with header `h`, or `None` if
/// the header is invalid.
fn mpeg_frame_len(h: u32) -> Option<u64> {
    if h >> 21 != 0x7FF {
        return None;
    }
    let version = (h >> 19) & 3; // 3 = MPEG1, 2 = MPEG2, 0 = MPEG2.5
    let layer = (h >> 17) & 3; // 3 = I, 2 = II, 1 = III
    let br_idx = ((h >> 12) & 0xF) as usize;
    let sr_idx = ((h >> 10) & 3) as usize;
    let pad = u64::from((h >> 9) & 1);
    if version == 1 || layer == 0 || br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    const V1L1: [u64; 15] = [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448];
    const V1L2: [u64; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
    const V1L3: [u64; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const V2L1: [u64; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256];
    const V2L23: [u64; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let mpeg1 = version == 3;
    let kbps = match (mpeg1, layer) {
        (true, 3) => V1L1[br_idx],
        (true, 2) => V1L2[br_idx],
        (true, _) => V1L3[br_idx],
        (false, 3) => V2L1[br_idx],
        (false, _) => V2L23[br_idx],
    };
    let rates = match version {
        3 => [44100u64, 48000, 32000],
        2 => [22050, 24000, 16000],
        _ => [11025, 12000, 8000],
    };
    let sr = rates[sr_idx];
    let bps = kbps * 1000;
    Some(match layer {
        3 => (12 * bps / sr + pad) * 4,
        1 if !mpeg1 => 72 * bps / sr + pad,
        _ => 144 * bps / sr + pad,
    })
}

/// Bits that must stay constant across frames of one stream: sync,
/// version, layer and sample rate.
const STREAM_MASK: u32 = 0xFFFE_0C00;

impl Format for Mp3 {
    fn name(&self) -> &'static str {
        "mp3"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("mp3", Category::Audio)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"I\xFF"
    }
    fn max_size(&self) -> u64 {
        1024 * MIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        if h.starts_with(b"ID3") {
            return matches!(h.get(3), Some(2..=4)) && h.get(6..10).is_some_and(|s| s.iter().all(|&b| b < 0x80));
        }
        // Without a tag, demand two consecutive valid frames.
        let Some(h0) = crate::bytes::be32(h, 0) else { return false };
        let Some(len) = mpeg_frame_len(h0) else { return false };
        crate::bytes::be32(h, len as usize)
            .is_some_and(|h1| h1 & STREAM_MASK == h0 & STREAM_MASK && mpeg_frame_len(h1).is_some())
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let mut pos = 0u64;
        let tagged = r.starts_with(0, b"ID3");
        if tagged {
            let s = r.bytes(6, 4)?;
            let size = s.iter().fold(0u64, |a, &b| a << 7 | u64::from(b & 0x7F));
            let footer = if r.u8(5)? & 0x10 != 0 { 10 } else { 0 };
            pos = 10 + size + footer;
        }
        let first = r.be32(pos)?;
        let mut frames = 0u64;
        while let Some(h) = r.be32(pos) {
            if h & STREAM_MASK != first & STREAM_MASK {
                break;
            }
            let Some(len) = mpeg_frame_len(h) else { break };
            if pos + len > r.limit() {
                break;
            }
            pos += len;
            frames += 1;
        }
        if frames < if tagged { 3 } else { 8 } {
            return None;
        }
        if r.starts_with(pos, b"TAG") && pos + 128 <= r.limit() {
            pos += 128; // ID3v1
        }
        Some(Hit { len: pos, ext: "mp3" })
    }
}

pub struct Ogg;

impl Format for Ogg {
    fn name(&self) -> &'static str {
        "ogg"
    }
    fn kinds(&self) -> &'static [(&'static str, Category)] {
        &[("ogg", Category::Audio), ("opus", Category::Audio), ("ogv", Category::Video)]
    }
    fn first_bytes(&self) -> &'static [u8] {
        b"O"
    }
    fn max_size(&self) -> u64 {
        4096 * MIB
    }
    fn probe(&self, h: &[u8]) -> bool {
        // Version 0, and the first page must be a beginning-of-stream page.
        h.starts_with(b"OggS\0") && h.get(5).is_some_and(|f| f & 0x02 != 0)
    }
    fn measure(&self, r: &mut Reader) -> Option<Hit> {
        let first_payload = 27 + u64::from(r.u8(26)?);
        let ext = if r.starts_with(first_payload, b"OpusHead") {
            "opus"
        } else if r.starts_with(first_payload, b"\x80theora") {
            "ogv"
        } else {
            "ogg"
        };
        let mut pos = 0u64;
        let mut pages = 0;
        while r.starts_with(pos, b"OggS\0") {
            let nseg = u64::from(r.u8(pos + 26)?);
            let table = r.bytes(pos + 27, nseg as usize)?;
            let body: u64 = table.iter().map(|&b| u64::from(b)).sum();
            let flags = r.u8(pos + 5)?;
            let next = pos + 27 + nseg + body;
            if next > r.limit() {
                break;
            }
            pos = next;
            pages += 1;
            if flags & 0x04 != 0 && !r.starts_with(pos, b"OggS\0") {
                break; // end of (last) stream
            }
        }
        (pages >= 2).then_some(Hit { len: pos, ext })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_lengths() {
        // MPEG1 Layer III 128 kbps 44.1 kHz, no padding: 417 bytes.
        assert_eq!(mpeg_frame_len(0xFFFB_9000), Some(417));
        // Same with padding: 418.
        assert_eq!(mpeg_frame_len(0xFFFB_9200), Some(418));
        assert_eq!(mpeg_frame_len(0xFFFB_F000), None, "bad bitrate");
        assert_eq!(mpeg_frame_len(0x1234_5678), None);
    }
}
