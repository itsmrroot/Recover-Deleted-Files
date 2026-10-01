//! Carving end to end: minimal but valid files of several formats are laid
//! out between random-looking junk, and every one must come back with its
//! exact offset, length and type — and nothing else may be reported.

use std::sync::atomic::AtomicBool;

use wdfr::carve::{self, CarveOptions, Carved};
use wdfr::source::MemSource;

/// Deterministic junk that never accidentally contains a signature.
fn junk(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            // Avoid bytes that start signatures we test for.
            match (x >> 24) as u8 {
                0xFF | 0x89 | b'G' | b'%' | b'P' | 0x00 | 0x1A | b'R' | b'S' | b'I' | 0xD0 | b'7' | 0x30 | b'O'
                | b'B' | b'M' => 0x55,
                b => b,
            }
        })
        .collect()
}

fn jpeg() -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
    v.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    v.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
    v.extend_from_slice(&[1u8; 64]);
    v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x10, 0x01, 0x01, 0x11, 0x00]);
    v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
    // Entropy-coded data with byte stuffing and a restart marker.
    v.extend_from_slice(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0x78]);
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

fn png() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    let chunk = |v: &mut Vec<u8>, ty: &[u8], data: &[u8]| {
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(ty);
        v.extend_from_slice(data);
        v.extend_from_slice(&[0, 0, 0, 0]);
    };
    chunk(&mut v, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]);
    chunk(&mut v, b"IDAT", &[0x78, 0x9C, 1, 2, 3]);
    chunk(&mut v, b"IEND", &[]);
    v
}

fn gif() -> Vec<u8> {
    let mut v = b"GIF89a".to_vec();
    v.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0]); // 1x1, 2-colour table
    v.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
    v.extend_from_slice(&[0x21, 0xF9, 4, 0, 0, 0, 0, 0]); // graphic control ext
    v.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0]);
    v.extend_from_slice(&[2, 2, 0x4C, 0x01, 0]); // LZW data
    v.push(0x3B);
    v
}

fn pdf() -> Vec<u8> {
    let mut v = b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n".to_vec();
    // Incremental update with a second %%EOF.
    v.extend_from_slice(b"2 0 obj << >> endobj\nstartxref\n9\n%%EOF\n");
    v
}

fn mp4() -> Vec<u8> {
    let bx = |ty: &[u8], body: &[u8]| {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(ty);
        b.extend_from_slice(body);
        b
    };
    let mut v = bx(b"ftyp", b"isom\0\0\x02\0isomiso2mp41");
    v.extend(bx(b"moov", &[7u8; 100]));
    v.extend(bx(b"mdat", &[9u8; 3000]));
    v
}

/// A ZIP with one stored entry whose central directory says it is a DOCX.
fn docx() -> Vec<u8> {
    let name = b"word/document.xml";
    let data = b"<w:document/>";
    let mut v = Vec::new();
    v.extend_from_slice(b"PK\x03\x04");
    v.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    v.extend_from_slice(&0u32.to_le_bytes()); // crc (unchecked)
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&(name.len() as u16).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(name);
    v.extend_from_slice(data);
    let cd_off = v.len() as u32;
    v.extend_from_slice(b"PK\x01\x02");
    v.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&(name.len() as u16).to_le_bytes());
    v.extend_from_slice(&[0u8; 12]);
    v.extend_from_slice(&0u32.to_le_bytes()); // local header offset
    v.extend_from_slice(name);
    let cd_size = v.len() as u32 - cd_off;
    v.extend_from_slice(b"PK\x05\x06\0\0\0\0\x01\0\x01\0");
    v.extend_from_slice(&cd_size.to_le_bytes());
    v.extend_from_slice(&cd_off.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v
}

fn sqlite() -> Vec<u8> {
    let mut v = vec![0u8; 3 * 1024];
    v[..16].copy_from_slice(b"SQLite format 3\0");
    v[16..18].copy_from_slice(&1024u16.to_be_bytes());
    v[24..28].copy_from_slice(&5u32.to_be_bytes());
    v[28..32].copy_from_slice(&3u32.to_be_bytes());
    v[92..96].copy_from_slice(&5u32.to_be_bytes());
    v
}

#[test]
fn carves_exact_files_from_junk() {
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("jpg", jpeg()),
        ("png", png()),
        ("gif", gif()),
        ("pdf", pdf()),
        ("mp4", mp4()),
        ("docx", docx()),
        ("sqlite", sqlite()),
    ];
    let mut disk = junk(4096, 1);
    let mut expected = Vec::new();
    for (i, (ext, f)) in files.iter().enumerate() {
        let pad = (512 - disk.len() % 512) % 512;
        disk.extend(junk(pad, i as u32 + 10));
        expected.push((disk.len() as u64, f.len() as u64, *ext));
        disk.extend_from_slice(f);
        disk.extend(junk(2000 + i * 300, i as u32 + 100));
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
fn rejects_truncated_files() {
    // A JPEG cut off before its end marker must not be reported.
    let mut j = jpeg();
    j.truncate(j.len() - 2);
    let mut disk = j;
    disk.extend(vec![0u8; 4096]);
    let src = MemSource(disk);
    let opts = CarveOptions { formats: carve::all_formats(), step: 512, max_size: Some(4096) };
    let mut n = 0;
    carve::carve(
        &src,
        std::slice::from_ref(&(0..src.0.len() as u64)),
        &opts,
        &AtomicBool::new(false),
        |_| {
            n += 1;
            Ok(())
        },
        |_| {},
    )
    .unwrap();
    assert_eq!(n, 0);
}
