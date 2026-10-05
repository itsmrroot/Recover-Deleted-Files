//! Shared test helpers: an in-memory FAT16 image builder.

#![allow(dead_code)]

use std::sync::Arc;

use wdfr::source::{MemSource, Source};

pub const BPS: usize = 512;
pub const SPC: usize = 2;
pub const CLUSTER: usize = BPS * SPC;
pub const RESERVED: usize = 1;
pub const FATS: usize = 2;
pub const FAT_SECTORS: usize = 17;
pub const ROOT_ENTRIES: usize = 512;
pub const CLUSTERS: usize = 4100;

pub const ROOT_OFF: usize = (RESERVED + FATS * FAT_SECTORS) * BPS;
pub const DATA_OFF: usize = ROOT_OFF + ROOT_ENTRIES * 32;
pub const EOC: u16 = 0xFFFF;

pub struct Image {
    pub d: Vec<u8>,
}

pub fn sfn_checksum(name: &[u8; 11]) -> u8 {
    name.iter().fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c))
}

pub fn short_entry(name: &[u8; 11], attr: u8, cluster: u16, size: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(name);
    e[11] = attr;
    e[22..24].copy_from_slice(&0x6000u16.to_le_bytes()); // 12:00:00
    e[24..26].copy_from_slice(&(((2024 - 1980) << 9 | 5 << 5 | 17) as u16).to_le_bytes());
    e[26..28].copy_from_slice(&cluster.to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

/// LFN entries (disk order) followed by the short entry.
pub fn long_entries(long: &str, short: &[u8; 11], attr: u8, cluster: u16, size: u32) -> Vec<u8> {
    let units: Vec<u16> = long.encode_utf16().collect();
    let parts = units.len().div_ceil(13);
    let chk = sfn_checksum(short);
    let mut out = Vec::new();
    for k in (0..parts).rev() {
        let mut e = [0u8; 32];
        e[0] = (k + 1) as u8 | if k + 1 == parts { 0x40 } else { 0 };
        e[11] = 0x0F;
        e[13] = chk;
        let mut chunk: Vec<u16> = units[k * 13..units.len().min(k * 13 + 13)].to_vec();
        if chunk.len() < 13 {
            chunk.push(0);
        }
        chunk.resize(13, 0xFFFF);
        for (i, o) in [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].into_iter().enumerate() {
            e[o..o + 2].copy_from_slice(&chunk[i].to_le_bytes());
        }
        out.extend_from_slice(&e);
    }
    out.extend_from_slice(&short_entry(short, attr, cluster, size));
    out
}

/// Marks an entry list (LFN + short) deleted the way FAT does.
pub fn delete(entries: &mut [u8]) {
    for e in entries.as_chunks_mut::<32>().0 {
        e[0] = 0xE5;
    }
}

impl Image {
    pub fn new() -> Self {
        let total_sectors = (DATA_OFF / BPS) + CLUSTERS * SPC;
        let mut d = vec![0u8; total_sectors * BPS];
        let bs = &mut d[..BPS];
        bs[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
        bs[3..11].copy_from_slice(b"MSWIN4.1");
        bs[11..13].copy_from_slice(&(BPS as u16).to_le_bytes());
        bs[13] = SPC as u8;
        bs[14..16].copy_from_slice(&(RESERVED as u16).to_le_bytes());
        bs[16] = FATS as u8;
        bs[17..19].copy_from_slice(&(ROOT_ENTRIES as u16).to_le_bytes());
        bs[19..21].copy_from_slice(&(total_sectors as u16).to_le_bytes());
        bs[21] = 0xF8;
        bs[22..24].copy_from_slice(&(FAT_SECTORS as u16).to_le_bytes());
        bs[510] = 0x55;
        bs[511] = 0xAA;
        let mut img = Image { d };
        img.set_fat(0, 0xFFF8);
        img.set_fat(1, EOC);
        img
    }

    pub fn set_fat(&mut self, cl: usize, v: u16) {
        for f in 0..FATS {
            let o = (RESERVED + f * FAT_SECTORS) * BPS + cl * 2;
            self.d[o..o + 2].copy_from_slice(&v.to_le_bytes());
        }
    }

    pub fn cluster(&mut self, cl: usize) -> &mut [u8] {
        let o = DATA_OFF + (cl - 2) * CLUSTER;
        &mut self.d[o..o + CLUSTER]
    }

    pub fn fill(&mut self, cl: usize, seed: u8) {
        for (i, b) in self.cluster(cl).iter_mut().enumerate() {
            *b = seed ^ (i as u8);
        }
    }

    pub fn build(self) -> Source {
        Arc::new(MemSource(self.d))
    }
}

/// A minimal valid baseline JPEG (with byte stuffing and a restart marker).
pub fn jpeg() -> Vec<u8> {
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

/// A minimal PNG (CRCs are not checked by the carver).
pub fn png() -> Vec<u8> {
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
