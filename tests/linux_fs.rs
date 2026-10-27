//! btrfs and XFS, made by Linux: every file comes back, deleted or not,
//! byte for byte (checked against the SHA-256 of each file taken before
//! anything was deleted). The btrfs image has a subvolume and files
//! compressed with zstd, LZO and zlib.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use wdfr::source::{MemSource, Source};

mod common;

fn image(name: &str) -> Source {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux").join(format!("{name}.img.gz"));
    let mut v = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&p).unwrap()).read_to_end(&mut v).unwrap();
    Arc::new(MemSource(v))
}

fn hashes(name: &str) -> HashMap<String, String> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux").join(format!("{name}.sha256"));
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| {
            let (hash, path) = l.split_once("  ").unwrap();
            (path.trim_start_matches("./").to_string(), hash.to_string())
        })
        .collect()
}

/// Every file of the image, with its content's SHA-256: (live, deleted).
fn files(name: &str) -> (HashMap<String, String>, HashMap<String, String>) {
    let src = image(name);
    let vol = wdfr::fs::open(src.clone()).unwrap();
    let mut live = HashMap::new();
    let mut deleted = HashMap::new();
    for f in vol.scan_files(true, &mut |_, _| {}).unwrap() {
        let mut out = Vec::new();
        wdfr::fs::extract(src.as_ref(), &f, &mut out).unwrap();
        let hash: String = Sha256::digest(&out).iter().map(|b| format!("{b:02x}")).collect();
        if f.note.is_some() {
            deleted.insert(f.path, hash)
        } else {
            live.insert(f.path, hash)
        };
    }
    (live, deleted)
}

#[test]
fn btrfs_files_live_and_deleted_compressed_or_not() {
    let want = hashes("btrfs");
    let (live, deleted) = files("btrfs");
    for (path, hash) in &want {
        let gone = path.starts_with("gone");
        let got = if gone { deleted.get(path) } else { live.get(path) };
        assert_eq!(got, Some(hash), "{path} (deleted: {gone})");
    }
    assert_eq!(deleted.len(), 2, "{deleted:?}");
}

#[test]
fn xfs_files_live_and_deleted_from_the_journal() {
    let want = hashes("xfs");
    let (live, deleted) = files("xfs");
    for (path, hash) in &want {
        let gone = path.starts_with("gone");
        let got = if gone { deleted.get(path) } else { live.get(path) };
        assert_eq!(got, Some(hash), "{path} (deleted: {gone})");
    }
    assert_eq!(deleted.len(), 2, "{deleted:?}");
}

#[test]
fn a_lost_btrfs_partition_is_found_with_its_files() {
    let volume = image("btrfs").read_vec(0, 128 << 20).unwrap();
    let (session, found) = common::scan_lost(volume, 0);
    let p = session.found_partitions();
    assert!(p.iter().any(|p| p.fs == Some(wdfr::fs::FsKind::Btrfs)), "{p:?}");
    let names: Vec<&str> = found.fs.iter().map(|f| f.file.path.as_str()).collect();
    assert!(names.contains(&"gone.bin") && names.contains(&"share/shared.txt"), "{names:?}");
}
