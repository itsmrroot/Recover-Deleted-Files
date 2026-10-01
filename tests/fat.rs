//! Builds a FAT16 volume in memory and checks deleted-file recovery,
//! including long names, fragmentation around live files, and choosing the
//! right owner of a reused directory cluster.

use std::sync::Arc;

use wdfr::fs::{self, Condition, DeletedFile, FsKind};
use wdfr::source::{MemSource, Source};

const BPS: usize = 512;
const SPC: usize = 2;
const CLUSTER: usize = BPS * SPC;
const RESERVED: usize = 1;
const FATS: usize = 2;
const FAT_SECTORS: usize = 17;
const ROOT_ENTRIES: usize = 512;
const CLUSTERS: usize = 4100;

const ROOT_OFF: usize = (RESERVED + FATS * FAT_SECTORS) * BPS;
const DATA_OFF: usize = ROOT_OFF + ROOT_ENTRIES * 32;
const EOC: u16 = 0xFFFF;

struct Image {
    d: Vec<u8>,
}

fn sfn_checksum(name: &[u8; 11]) -> u8 {
    name.iter().fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c))
}

fn short_entry(name: &[u8; 11], attr: u8, cluster: u16, size: u32) -> [u8; 32] {
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
fn long_entries(long: &str, short: &[u8; 11], attr: u8, cluster: u16, size: u32) -> Vec<u8> {
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
fn delete(entries: &mut [u8]) {
    for e in entries.as_chunks_mut::<32>().0 {
        e[0] = 0xE5;
    }
}

impl Image {
    fn new() -> Self {
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

    fn set_fat(&mut self, cl: usize, v: u16) {
        for f in 0..FATS {
            let o = (RESERVED + f * FAT_SECTORS) * BPS + cl * 2;
            self.d[o..o + 2].copy_from_slice(&v.to_le_bytes());
        }
    }

    fn cluster(&mut self, cl: usize) -> &mut [u8] {
        let o = DATA_OFF + (cl - 2) * CLUSTER;
        &mut self.d[o..o + CLUSTER]
    }

    fn fill(&mut self, cl: usize, seed: u8) {
        for (i, b) in self.cluster(cl).iter_mut().enumerate() {
            *b = seed ^ (i as u8);
        }
    }

    fn build(self) -> Source {
        Arc::new(MemSource(self.d))
    }
}

fn find<'a>(files: &'a [DeletedFile], path: &str) -> &'a DeletedFile {
    files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path} missing; have {:?}", files.iter().map(|f| &f.path).collect::<Vec<_>>()))
}

#[test]
fn recovers_deleted_fat_files() {
    let mut img = Image::new();
    let mut root = Vec::new();

    // Live directory LIVE at cluster 2.
    root.extend_from_slice(&short_entry(b"LIVE       ", 0x10, 2, 0));
    img.set_fat(2, EOC);

    // A live file occupying cluster 12.
    root.extend_from_slice(&short_entry(b"KEEP    BIN", 0x20, 12, 1000));
    img.set_fat(12, EOC);

    // Deleted 4-cluster video written around the live file: 10, 11, 13, 14.
    let mut video = long_entries("Holiday Video 2024.mp4", b"HOLIDA~1MP4", 0x20, 10, 3 * 1024 + 100);
    delete(&mut video);
    root.extend_from_slice(&video);
    for (cl, seed) in [(10, 1), (11, 2), (13, 3), (14, 4)] {
        img.fill(cl, seed);
    }
    img.fill(12, 0xEE);

    // Deleted short-name-only file: first character is lost.
    let mut report = short_entry(b"REPORT  PDF", 0x20, 40, 2000);
    delete(&mut report);
    root.extend_from_slice(&report);
    img.fill(40, 7);
    img.fill(41, 8);

    // A deleted root-level directory GHOST whose cluster (30) was later
    // reused for LIVE/SUB: SUB's ".." points to LIVE, so GHOST is rejected.
    let mut ghost = short_entry(b"GHOST      ", 0x10, 30, 0);
    delete(&mut ghost);
    root.extend_from_slice(&ghost);

    let root_bytes = root.clone();
    img.d[ROOT_OFF..ROOT_OFF + root_bytes.len()].copy_from_slice(&root_bytes);

    // LIVE contents: ".", "..", deleted directory SUB at 30.
    let mut live = Vec::new();
    live.extend_from_slice(&short_entry(b".          ", 0x10, 2, 0));
    live.extend_from_slice(&short_entry(b"..         ", 0x10, 0, 0));
    let mut sub = short_entry(b"SUB        ", 0x10, 30, 0);
    delete(&mut sub);
    live.extend_from_slice(&sub);
    img.cluster(2)[..live.len()].copy_from_slice(&live);

    // SUB contents (cluster 30, freed): ".", ".." -> 2, deleted PIC.JPG.
    let mut subdir = Vec::new();
    subdir.extend_from_slice(&short_entry(b".          ", 0x10, 30, 0));
    subdir.extend_from_slice(&short_entry(b"..         ", 0x10, 2, 0));
    let mut pic = long_entries("beach.jpg", b"BEACH   JPG", 0x20, 31, 900);
    delete(&mut pic);
    subdir.extend_from_slice(&pic);
    img.cluster(30)[..subdir.len()].copy_from_slice(&subdir);
    img.fill(31, 9);

    let src = img.build();
    assert_eq!(fs::detect(src.as_ref()), Some(FsKind::Fat16));
    let vol = fs::open(src.clone()).unwrap();
    let files = vol.scan_deleted(&mut |_, _| {}).unwrap();

    let read = |f: &DeletedFile| {
        let mut v = Vec::new();
        fs::extract(src.as_ref(), f, &mut v).unwrap();
        v
    };
    let cluster_bytes = |seed: u8| (0..CLUSTER).map(|i| seed ^ (i as u8)).collect::<Vec<u8>>();

    // Long name recovered (incl. first character), fragmented around KEEP.BIN.
    let v = find(&files, "Holiday Video 2024.mp4");
    assert_eq!(v.condition, Condition::Recoverable);
    assert_eq!(v.modified.unwrap().to_string(), "2024-05-17 12:00:00");
    let mut want: Vec<u8> = [1, 2, 3].iter().flat_map(|&s| cluster_bytes(s)).collect();
    want.extend_from_slice(&cluster_bytes(4)[..100]);
    assert_eq!(read(v), want);

    let r = find(&files, "_EPORT.PDF");
    let mut want = cluster_bytes(7);
    want.extend_from_slice(&cluster_bytes(8)[..2000 - 1024]);
    assert_eq!(read(r), want);

    // SUB has no long name, so deletion destroyed its first character.
    let p = find(&files, "LIVE/_UB/beach.jpg");
    assert_eq!(read(p), cluster_bytes(9)[..900].to_vec());
    assert!(files.iter().all(|f| !f.path.starts_with("GHOST")));

    // Live files are not reported.
    assert!(files.iter().all(|f| f.path != "KEEP.BIN"));
}
