//! BitLocker drives: locked until their recovery key or password is given,
//! then read like any other drive. The volumes are from cryptsetup's tests.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use wdfr::filter::Filter;
use wdfr::fs::FsKind;
use wdfr::progress::Silent;
use wdfr::recover::{self, Method, ScanOptions, Session};
use wdfr::source::{MemSource, Source, SubSource};

mod common;

const START: usize = 1 << 20;

/// A disk with one MBR partition at 1 MiB holding `volume`.
fn disk_with(volume: Vec<u8>) -> Source {
    let mut disk = vec![0u8; START];
    let sectors = (volume.len() / 512) as u32;
    let e = &mut disk[446..462];
    e[4] = 0x07;
    e[8..12].copy_from_slice(&((START / 512) as u32).to_le_bytes());
    e[12..16].copy_from_slice(&sectors.to_le_bytes());
    disk[510] = 0x55;
    disk[511] = 0xAA;
    disk.extend(volume);
    Arc::new(MemSource(disk))
}

fn open(disk: &Source) -> Session {
    Session::new(disk.clone(), "test".into(), wdfr::partition::discover(disk))
}

#[test]
fn a_bitlocker_partition_opens_with_its_recovery_key() {
    let disk = disk_with(common::load_sectors("bitlocker-xts-128.sectors"));
    let s = open(&disk);
    let p = &s.partitions[0];
    assert!(p.locked(&s.disk), "{p:?}");
    assert_eq!(p.fs, None);

    let part: Source = Arc::new(SubSource::new(disk.clone(), p.start, p.len));
    assert!(wdfr::bitlocker::add_key(part.clone(), "wrong password").is_err());
    assert!(wdfr::bitlocker::add_key(part.clone(), "404558-436711-420860-678557-638220-018909-039941-695321").is_err());
    assert!(open(&disk).partitions[0].locked(&disk));

    wdfr::bitlocker::add_key(part, "235818-357951-253979-013365-241120-245575-342914-591910").unwrap();
    let s = open(&disk);
    let p = &s.partitions[0];
    assert!(!p.locked(&s.disk));
    assert_eq!(p.fs, Some(FsKind::Ntfs));
    // The whole disk reads decrypted inside the partition (the deep search
    // reads it this way), and unchanged outside it.
    let boot = s.disk.read_vec(START as u64, 512).unwrap();
    assert_eq!(&boot[3..11], b"NTFS    ");
    assert_eq!(s.disk.read_vec(0, 512).unwrap(), disk.read_vec(0, 512).unwrap());
    let across = s.disk.read_vec(START as u64 - 512, 1024).unwrap();
    assert_eq!(&across[512..], &boot[..]);

    let opts = ScanOptions {
        method: Method::All,
        partition: None,
        filter: Filter::default(),
        carve_all_space: false,
        step: 512,
        max_carve_size: None,
    };
    recover::scan(&s, &opts, &Silent, &AtomicBool::new(false)).unwrap();
}

#[test]
fn suspended_bitlocker_opens_without_a_key() {
    let disk: Source = Arc::new(MemSource(common::load_sectors("bitlocker-suspended.sectors")));
    let s = open(&disk);
    let p = &s.partitions[0];
    assert_eq!(p.kind, "BitLocker");
    assert_eq!(p.fs, Some(FsKind::Ntfs));
    assert!(!p.locked(&s.disk));
}
