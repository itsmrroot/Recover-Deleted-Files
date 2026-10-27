//! Videos whose recording was cut off before the index was written are
//! rebuilt with the help of another video from the same camera. The clips
//! were made with ffmpeg: H.264 with B-frames and PCM sound (MOV), and
//! H.265 with AAC sound (MP4).

use std::path::Path;

use wdfr::carve::bmff::{Index, parse_moov};
use wdfr::source::{MemSource, ReadAt};
use wdfr::video_repair::{Sound, repair_video};

fn data(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/video").join(name)).unwrap()
}

/// Top-level boxes: (type, start, size).
fn boxes(d: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= d.len() {
        let mut size = u32::from_be_bytes(d[i..i + 4].try_into().unwrap()) as usize;
        if size == 1 {
            size = u64::from_be_bytes(d[i + 8..i + 16].try_into().unwrap()) as usize;
        }
        out.push((d[i + 4..i + 8].try_into().unwrap(), i, size));
        i += size;
    }
    out
}

fn index(file: &[u8]) -> Index {
    let (_, at, size) = *boxes(file).iter().find(|b| &b.0 == b"moov").unwrap();
    parse_moov(&file[at..at + size]).unwrap()
}

/// Video frame sizes in file order, and the number of sound samples.
fn layout(i: &Index) -> (Vec<u32>, usize) {
    let video = i.video_samples().iter().map(|s| s.1).collect();
    let sound = i.tracks.iter().filter(|t| t.codec.is_none()).flat_map(|t| &t.chunks).map(|c| c.samples.len()).sum();
    (video, sound)
}

fn repair(original: &str, reference: &str) -> (wdfr::video_repair::Repaired, Vec<u8>, Vec<u8>) {
    let original = data(original);
    // The camera stopped before writing the index: the file ends where it
    // would have started.
    let (_, moov_at, _) = *boxes(&original).iter().find(|b| &b.0 == b"moov").unwrap();
    let damaged = MemSource(original[..moov_at].to_vec());
    let reference = MemSource(data(reference));
    let mut out = Vec::new();
    let r = repair_video(&damaged, &reference, &mut out, &|| false).unwrap();
    assert!(damaged.size() > 0);
    (r, original, out)
}

#[test]
fn h264_with_b_frames_and_pcm_sound_comes_back_frame_for_frame() {
    let (r, original, out) = repair("h264-pcm.mov", "h264-pcm-reference.mov");
    let (want_video, want_sound) = layout(&index(&original));
    let (video, sound) = layout(&index(&out));
    assert_eq!(r.frames, 90);
    assert_eq!(r.keyframes, 3);
    assert_eq!(video, want_video, "every frame, with its exact size");
    assert_eq!(r.sound, Sound::Restored);
    assert_eq!(sound, want_sound, "all the sound");
}

#[test]
fn h265_with_aac_comes_back_without_sound() {
    let (r, original, out) = repair("h265-aac.mp4", "h265-aac-reference.mp4");
    let (want_video, _) = layout(&index(&original));
    let (video, sound) = layout(&index(&out));
    assert_eq!(r.frames, 90);
    assert_eq!(video, want_video);
    assert!(matches!(r.sound, Sound::Dropped(_)), "{:?}", r.sound);
    assert_eq!(sound, 0);
}

#[test]
fn a_photo_or_another_codec_is_refused() {
    let damaged = MemSource(vec![0u8; 4096]);
    let reference = MemSource(data("h264-pcm.mov"));
    let mut out = Vec::new();
    assert!(repair_video(&damaged, &reference, &mut out, &|| false).is_err());
    let not_video = MemSource(b"\xFF\xD8\xFF\xE0 a JPEG".to_vec());
    assert!(repair_video(&damaged, &not_video, &mut out, &|| false).is_err());
}
