//! Builds a FAT16 volume in memory and checks deleted-file recovery,
//! including long names, fragmentation around live files, and choosing the
//! right owner of a reused directory cluster.

use wdfr::fs::{self, Condition, DeletedFile, FsKind};

mod common;

use common::*;

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
