//! ext2/3/4 images made by Linux itself (mkfs, then files copied in and
//! deleted on a mounted volume): see `tests/data`. The deleted files are a
//! 37 KB JPEG (`Photos/Holiday/beach.jpg`) and 20 KB of random bytes
//! (`Docs/report.bin`); `Docs/note.txt` still exists.

use std::sync::Arc;

use wdfr::carve::formats::crc32;
use wdfr::fs::{self, Condition, DeletedFile, FsKind, Volume};
use wdfr::source::{MemSource, Source};

mod common;

const BEACH: (usize, u32) = (37349, 0x1975_f044);
const REPORT: (usize, u32) = (20000, 0x10ac_da29);

fn open(name: &str) -> Box<dyn Volume> {
    let src: Source = Arc::new(MemSource(common::load_sectors(name)));
    assert_eq!(fs::detect(src.as_ref()), Some(FsKind::Ext));
    fs::open(src).unwrap()
}

fn content(vol: &dyn Volume, f: &DeletedFile) -> Vec<u8> {
    let mut out = Vec::new();
    fs::extract(vol.source().as_ref(), f, &mut out).unwrap();
    out
}

fn check(vol: &dyn Volume, files: &[DeletedFile], path: &str, (len, crc): (usize, u32)) {
    let f = files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path} not found in {:?}", files.iter().map(|f| &f.path).collect::<Vec<_>>()));
    assert_eq!(f.condition, Condition::Recoverable, "{path}");
    let data = content(vol, f);
    assert_eq!((data.len(), crc32(&data)), (len, crc), "{path}");
}

#[test]
fn deleted_files_come_back_from_the_ext4_journal() {
    let vol = open("ext4.sectors");
    let files = vol.scan_deleted(&mut |_, _| {}).unwrap();
    check(vol.as_ref(), &files, "Photos/Holiday/beach.jpg", BEACH);
    check(vol.as_ref(), &files, "Docs/report.bin", REPORT);
    assert_eq!(files.len(), 2, "{:?}", files.iter().map(|f| &f.path).collect::<Vec<_>>());
}

#[test]
fn deleted_files_come_back_from_the_ext3_journal() {
    let vol = open("ext3.sectors");
    let files = vol.scan_deleted(&mut |_, _| {}).unwrap();
    check(vol.as_ref(), &files, "Photos/Holiday/beach.jpg", BEACH);
    check(vol.as_ref(), &files, "Docs/report.bin", REPORT);
}

#[test]
fn existing_files_are_listed_for_lost_partitions() {
    // ext2 has no journal, and Linux wipes a deleted file's name and block
    // list: only the deep search can find those. The files that exist are
    // listed when a lost partition is recovered.
    let vol = open("ext2.sectors");
    assert!(vol.scan_deleted(&mut |_, _| {}).unwrap().is_empty());
    let all = vol.scan_files(true, &mut |_, _| {}).unwrap();
    let note = all.iter().find(|f| f.path == "Docs/note.txt").expect("note.txt");
    assert_eq!(content(vol.as_ref(), note), b"hello ext2\n");
    // Free space covers the deleted photo's data, which carving then finds.
    assert!(!vol.free_ranges().unwrap().is_empty());
}

#[test]
fn a_lost_ext4_partition_is_found_with_its_files() {
    let (session, found) = common::scan_lost(common::load_sectors("ext4.sectors"), 0);
    assert_eq!(common::found_content(&session, &found, "Docs/note.txt"), b"hello ext4\n");
    let photo = common::found_content(&session, &found, "Photos/Holiday/beach.jpg");
    assert_eq!((photo.len(), crc32(&photo)), BEACH);
}
