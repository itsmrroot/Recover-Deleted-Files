//! The file types added later (camera RAW from Fujifilm, Olympus and
//! Panasonic, Photoshop, FLAC, RTF, Outlook): each is laid out between junk
//! and must come back with its exact offset, length and type.

use std::sync::atomic::AtomicBool;

use wdfr::carve::{self, CarveOptions, Carved};
use wdfr::source::MemSource;

/// Deterministic junk without 0xFF or 0x00 bytes, so it never looks like
/// the start or the end of anything.
fn junk(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            match (x >> 24) as u8 {
                0xFF | 0x00 | b'{' | b'}' | b'I' | b'M' | b'F' | b'8' | b'f' | b'!' => 0x55,
                b => b,
            }
        })
        .collect()
}

/// A little-endian TIFF-style file with `magic`, one IFD of
/// (tag, value) LONG entries at offset `ifd`, padded to `len`.
fn tiff_like(magic: &[u8; 4], ifd: u32, tags: &[(u16, u32)], len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    v[..4].copy_from_slice(magic);
    v[4..8].copy_from_slice(&ifd.to_le_bytes());
    let mut o = ifd as usize;
    v[o..o + 2].copy_from_slice(&(tags.len() as u16).to_le_bytes());
    o += 2;
    for &(tag, value) in tags {
        v[o..o + 2].copy_from_slice(&tag.to_le_bytes());
        v[o + 2..o + 4].copy_from_slice(&4u16.to_le_bytes()); // LONG
        v[o + 4..o + 8].copy_from_slice(&1u32.to_le_bytes());
        v[o + 8..o + 12].copy_from_slice(&value.to_le_bytes());
        o += 12;
    }
    v
}

fn orf() -> Vec<u8> {
    // StripOffsets 0x200, StripByteCounts 0x300: ends at 0x500.
    tiff_like(b"IIRO", 8, &[(256, 4), (273, 0x200), (279, 0x300)], 0x500)
}

fn rw2() -> Vec<u8> {
    // Sensor 16 x 8, raw data at 0x100: 16 * 8 * 2 bytes, ends at 0x200.
    tiff_like(b"IIU\0", 0x18, &[(2, 16), (3, 8), (280, 0x100)], 0x200)
}

fn raf() -> Vec<u8> {
    let mut v = vec![0x20u8; 0x360];
    v[..16].copy_from_slice(b"FUJIFILMCCD-RAW ");
    for (at, value) in [(0x54, 0x100u32), (0x58, 0x40), (0x5C, 0x140), (0x60, 0x20), (0x64, 0x160), (0x68, 0x200)] {
        v[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }
    v[0x100..0x103].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
    v
}

fn psd_header(version: u16, channels: u16, height: u32, width: u32) -> Vec<u8> {
    let mut v = b"8BPS".to_vec();
    v.extend_from_slice(&version.to_be_bytes());
    v.extend_from_slice(&[0; 6]);
    v.extend_from_slice(&channels.to_be_bytes());
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&8u16.to_be_bytes()); // depth
    v.extend_from_slice(&3u16.to_be_bytes()); // RGB
    v.extend_from_slice(&0u32.to_be_bytes()); // colour mode data
    v.extend_from_slice(&4u32.to_be_bytes()); // image resources: 4 bytes
    v.extend_from_slice(b"8BIM");
    v.extend_from_slice(&0u32.to_be_bytes()); // layers and masks
    v
}

fn psd_raw() -> Vec<u8> {
    let mut v = psd_header(1, 3, 2, 3);
    v.extend_from_slice(&0u16.to_be_bytes());
    v.extend_from_slice(&[7u8; 18]); // 3 channels x 2 rows x 3 bytes
    v
}

fn psd_rle() -> Vec<u8> {
    let mut v = psd_header(1, 1, 2, 4);
    v.extend_from_slice(&1u16.to_be_bytes());
    v.extend_from_slice(&3u16.to_be_bytes());
    v.extend_from_slice(&3u16.to_be_bytes());
    v.extend_from_slice(&[0x03, 1, 2, 3, 0x7D, 9]); // two RLE rows
    v
}

fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |mut c, &b| {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
        }
        c
    })
}

fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |mut c, &b| {
        c ^= u16::from(b) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
        }
        c
    })
}

/// One FLAC frame of 16 mono samples (frame `n`), with valid CRCs.
fn flac_frame(n: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0xFF, 0xF8, 0x69, 0x08, n, 15];
    f.push(crc8(&f));
    f.extend_from_slice(payload);
    let crc = crc16(&f);
    f.extend_from_slice(&crc.to_be_bytes());
    f
}

fn flac() -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    v.extend_from_slice(&[0x80, 0, 0, 34]); // last metadata block: STREAMINFO
    v.extend_from_slice(&16u16.to_be_bytes());
    v.extend_from_slice(&16u16.to_be_bytes());
    v.extend_from_slice(&[0; 6]); // frame sizes unknown
    // 44100 Hz, 1 channel, 16 bits, 32 samples.
    let packed: u64 = (44100 << 44) | (15 << 36) | 32;
    v.extend_from_slice(&packed.to_be_bytes());
    v.extend_from_slice(&[0; 16]); // MD5
    v.extend(flac_frame(0, &junk(40, 7)));
    v.extend(flac_frame(1, &junk(40, 8)));
    v
}

fn rtf() -> Vec<u8> {
    b"{\\rtf1\\ansi{\\fonttbl{\\f0 Arial;}}{\\b bold \\{not a group\\}} plain text\\par}".to_vec()
}

fn pst() -> Vec<u8> {
    let mut v = vec![0x41u8; 2048];
    v[..4].copy_from_slice(b"!BDN");
    v[8..10].copy_from_slice(b"SM");
    v[10..12].copy_from_slice(&23u16.to_le_bytes()); // Unicode format
    v[0xB8..0xC0].copy_from_slice(&2048u64.to_le_bytes());
    v
}

#[test]
fn carves_the_newer_file_types() {
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("orf", orf()),
        ("rw2", rw2()),
        ("raf", raf()),
        ("psd", psd_raw()),
        ("psd", psd_rle()),
        ("flac", flac()),
        ("rtf", rtf()),
        ("pst", pst()),
    ];
    let mut disk = junk(4096, 1);
    let mut expected = Vec::new();
    for (i, (ext, f)) in files.iter().enumerate() {
        let pad = (512 - disk.len() % 512) % 512;
        disk.extend(junk(pad, i as u32 + 10));
        expected.push((disk.len() as u64, f.len() as u64, *ext));
        disk.extend_from_slice(f);
        disk.extend(junk(1500 + i * 300, i as u32 + 100));
    }
    let src = MemSource(disk);
    let opts = CarveOptions { formats: carve::all_formats(), step: 512, max_size: None };
    let mut found: Vec<Carved> = Vec::new();
    carve::carve(
        &src,
        std::slice::from_ref(&(0..src.0.len() as u64)),
        &opts,
        &AtomicBool::new(false),
        |c| {
            found.push(c.clone());
            Ok(())
        },
        |_| {},
    )
    .unwrap();
    let got: Vec<(u64, u64, &str)> = found.iter().map(|c| (c.offset, c.len, c.ext)).collect();
    assert_eq!(got, expected);
}

#[test]
fn an_rtf_without_its_closing_brace_is_not_reported() {
    let mut cut = rtf();
    cut.pop();
    let mut disk = cut;
    disk.resize(4096, 0);
    let src = MemSource(disk);
    let opts = CarveOptions { formats: carve::all_formats(), step: 512, max_size: None };
    let mut found = Vec::new();
    carve::carve(
        &src,
        std::slice::from_ref(&(0..4096)),
        &opts,
        &AtomicBool::new(false),
        |c| {
            found.push(c.ext);
            Ok(())
        },
        |_| {},
    )
    .unwrap();
    assert!(found.is_empty(), "{found:?}");
}
