//! NAS drives, end to end: a deleted file comes back through Linux software
//! RAID and LVM. The images were made with mdadm, lvm and mkfs.ext4 in a
//! Linux VM: a Synology-style disk (GPT; a system RAID 1 with metadata
//! 0.90; a data RAID 1 holding LVM `vg1000/lv` with ext4), and three disks
//! of a RAID 5 with ext4. On each, `gone.txt` was written and deleted.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use wdfr::filter::Filter;
use wdfr::progress::Silent;
use wdfr::recover::{self, Item, Method, ScanOptions, Session};

fn unpack(names: &[&str]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wdfr-nas-{}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in names {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/nas").join(format!("{name}.gz"));
        let mut v = Vec::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&p).unwrap()).read_to_end(&mut v).unwrap();
        std::fs::write(dir.join(name), v).unwrap();
    }
    dir
}

fn gone() -> Vec<u8> {
    b"Deleted Files Recovery gone ".repeat(50000 / 30 + 1)
}

/// Scans `session` and returns the content of the deleted `gone.txt`.
fn recover_gone(session: &Session) -> Vec<u8> {
    let opts = ScanOptions {
        method: Method::Fs,
        partition: None,
        filter: Filter::default(),
        carve_all_space: false,
        step: 512,
        max_carve_size: None,
    };
    let found = recover::scan(session, &opts, &Silent, &AtomicBool::new(false)).unwrap();
    let f = found.fs.iter().find(|f| f.file.path.ends_with("gone.txt")).expect("gone.txt is found");
    recover::read_item(session, Item::Fs(f), 1 << 20).unwrap().unwrap()
}

#[test]
fn a_synology_style_disk_shows_its_lvm_volume_and_its_deleted_files() {
    let dir = unpack(&["syno.img"]);
    let s = Session::open(dir.join("syno.img").to_str().unwrap()).unwrap();
    let lv = s.partitions.iter().find(|p| p.kind == "LVM").expect("the LVM volume inside the RAID 1");
    assert_eq!(lv.name, "vg1000/lv");
    assert_eq!(lv.fs, Some(wdfr::fs::FsKind::Ext));
    assert_eq!(recover_gone(&s), gone());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_raid5_of_three_disks_is_read_whole_and_with_a_disk_missing() {
    let names = ["r5fs-0.img", "r5fs-1.img", "r5fs-2.img"];
    let dir = unpack(&names);
    let path = |n: &[&str]| {
        let disks: Vec<String> = n.iter().map(|f| dir.join(f).to_string_lossy().to_string()).collect();
        format!("raid:auto|{}", disks.join("|"))
    };
    let s = Session::open(&path(&names)).unwrap();
    assert_eq!(s.partitions.len(), 1);
    assert!(s.partitions[0].kind.starts_with("RAID 5"), "{}", s.partitions[0].kind);
    assert_eq!(recover_gone(&s), gone());
    // The same with the middle disk gone: rebuilt from parity.
    let s = Session::open(&path(&[names[0], names[2]])).unwrap();
    assert_eq!(recover_gone(&s), gone());
    let _ = std::fs::remove_dir_all(dir);
}
