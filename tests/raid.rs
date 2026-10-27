//! Linux software RAID and LVM, read like the Linux kernel reads them. The
//! member disks were made by mdadm and lvm in a Linux VM; every 4 KiB block
//! of each array (or volume) holds its own number, so any misplaced chunk
//! shows.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use wdfr::raid::{self, Super};
use wdfr::source::{MemSource, ReadAt, Source};

fn member(name: &str) -> Source {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/raid").join(format!("{name}.img.gz"));
    let mut v = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&p).unwrap()).read_to_end(&mut v).unwrap();
    Arc::new(MemSource(v))
}

fn expected_size(name: &str) -> u64 {
    let sizes =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/raid/sizes.txt")).unwrap();
    sizes.lines().find_map(|l| l.strip_prefix(&format!("{name} "))).unwrap().parse().unwrap()
}

/// Every 4 KiB block holds its number (u64, little-endian) 512 times.
fn check_pattern(what: &str, src: &dyn ReadAt) {
    let mut block = vec![0u8; 4096];
    for i in 0..src.size() / 4096 {
        src.read_exact_at(i * 4096, &mut block).unwrap_or_else(|e| panic!("{what}: block {i}: {e}"));
        let want = i.to_le_bytes();
        assert!(block.chunks(8).all(|c| c == want), "{what}: block {i} holds {:?}", &block[..8]);
    }
}

fn members(name: &str, count: usize) -> Vec<(Source, Super)> {
    (0..count)
        .map(|i| {
            let src = member(&format!("{name}-{i}"));
            let sb = raid::superblock(src.as_ref()).unwrap_or_else(|| panic!("{name}-{i}: no superblock"));
            (src, sb)
        })
        .collect()
}

#[test]
fn md_arrays_of_every_level_read_back_exactly() {
    let arrays =
        [("r0", 2, 0), ("r1", 2, 1), ("r5", 3, 5), ("r5ra", 3, 5), ("r6", 4, 6), ("r10", 4, 10), ("r5old", 3, 5)];
    for (name, count, level) in arrays {
        let m = members(name, count);
        assert!(m.iter().all(|(_, s)| s.level == level && s.disks == count as u32), "{name}");
        let mut arrays = raid::assemble(m);
        assert_eq!(arrays.len(), 1, "{name}");
        let a = arrays.remove(0).unwrap();
        assert_eq!(a.found, count);
        assert_eq!(a.source.size(), expected_size(name), "{name}: size");
        check_pattern(name, a.source.as_ref());
    }
}

#[test]
fn redundant_arrays_read_back_with_a_disk_missing() {
    for (name, count) in [("r1", 2), ("r5", 3), ("r5ra", 3), ("r6", 4), ("r10", 4), ("r5old", 3)] {
        for gone in 0..count {
            let mut m = members(name, count);
            m.remove(gone);
            let a = raid::assemble(m).remove(0).unwrap();
            assert_eq!(a.found, count - 1);
            assert_eq!(a.source.size(), expected_size(name));
            check_pattern(&format!("{name} without disk {gone}"), a.source.as_ref());
        }
    }
    // RAID 0 cannot do without a disk.
    let mut m = members("r0", 2);
    m.pop();
    assert!(raid::assemble(m).remove(0).is_err());
}

#[test]
fn lvm_volumes_on_raid_read_back_exactly() {
    // Two RAID 1 arrays, each a physical volume of one volume group with a
    // plain and a striped (across both arrays) logical volume.
    let arrays = raid::assemble(members("lvm", 4));
    assert_eq!(arrays.len(), 2);
    let pvs: Vec<_> = arrays
        .into_iter()
        .map(|a| {
            let a = a.unwrap();
            let pv = wdfr::lvm::pv(a.source.as_ref()).expect("an LVM physical volume");
            (a.source, pv)
        })
        .collect();
    let lvs = wdfr::lvm::volumes(&pvs);
    let names: Vec<&str> = lvs.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["plain", "striped"]);
    for lv in &lvs {
        assert_eq!(lv.vg, "vg1000");
        assert_eq!(lv.source.size(), expected_size(&lv.name), "{}", lv.name);
        check_pattern(&lv.name, lv.source.as_ref());
    }
    // The plain volume is one piece of the first array.
    let (src, pv) = &pvs[0];
    let (at, len) = wdfr::lvm::contiguous(pv, "plain").unwrap();
    assert_eq!(len, expected_size("plain"));
    check_pattern("plain in place", &wdfr::source::SubSource::new(src.clone(), at, len));
    assert_eq!(wdfr::lvm::contiguous(pv, "striped"), None);
}
