//! HFS+ and APFS images made by macOS (hdiutil, then files copied in and
//! deleted on the mounted volume): see `tests/data`. The deleted files are
//! a 37 KB JPEG (`Photos/Holiday/beach.jpg`) and 20 KB of random bytes
//! (`Docs/report.bin`); `Docs/note.txt` still exists.

use std::sync::Arc;

use wdfr::carve::formats::crc32;
use wdfr::fs::{self, Condition, DeletedFile, FsKind, Volume};
use wdfr::source::{MemSource, Source};

mod common;

const BEACH: (usize, u32) = (37349, 0x1975_f044);
const REPORT: (usize, u32) = (20000, 0x10ac_da29);

fn open(name: &str, kind: FsKind) -> Box<dyn Volume> {
    let src: Source = Arc::new(MemSource(common::load_sectors(name)));
    assert_eq!(fs::detect(src.as_ref()), Some(kind));
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

fn deleted_and_existing(name: &str, kind: FsKind, note: &[u8]) {
    let vol = open(name, kind);
    let deleted = vol.scan_deleted(&mut |_, _| {}).unwrap();
    check(vol.as_ref(), &deleted, "Photos/Holiday/beach.jpg", BEACH);
    check(vol.as_ref(), &deleted, "Docs/report.bin", REPORT);
    assert_eq!(deleted.len(), 2, "{:?}", deleted.iter().map(|f| &f.path).collect::<Vec<_>>());
    // Existing files are listed only when asked (a lost partition).
    let all = vol.scan_files(true, &mut |_, _| {}).unwrap();
    let n = all.iter().find(|f| f.path == "Docs/note.txt").expect("note.txt");
    assert_eq!(content(vol.as_ref(), n), note);
}

#[test]
fn hfs_plus_deleted_files_come_back_from_old_catalog_nodes() {
    deleted_and_existing("hfsplus.sectors", FsKind::HfsPlus, b"hello hfs\n");
}

#[test]
fn apfs_deleted_files_come_back_from_earlier_checkpoints() {
    deleted_and_existing("apfs.sectors", FsKind::Apfs, b"hello apfs\n");
}

#[test]
fn a_lost_apfs_container_is_found_with_its_files() {
    let (session, found) = common::scan_lost(common::load_sectors("apfs.sectors"), 0);
    assert_eq!(common::found_content(&session, &found, "Docs/note.txt"), b"hello apfs\n");
    let photo = common::found_content(&session, &found, "Photos/Holiday/beach.jpg");
    assert_eq!((photo.len(), crc32(&photo)), BEACH);
}

#[test]
fn a_lost_hfs_plus_volume_is_found_through_its_alternate_header() {
    // The main volume header (1024 bytes in) is wiped.
    let (session, found) = common::scan_lost(common::load_sectors("hfsplus.sectors"), 2048);
    assert_eq!(common::found_content(&session, &found, "Docs/note.txt"), b"hello hfs\n");
}
