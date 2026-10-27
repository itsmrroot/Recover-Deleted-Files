//! Virtual machine disks and forensic images read exactly like the disk
//! they hold. The images were made from one 6 MiB test disk with qemu-img
//! (VHD, VHDX, VMDK, VDI, QCOW2, snapshots of QCOW2 and VMDK) and
//! ewfacquire (E01, compressed and split into segments).

use std::io::Read;
use std::path::{Path, PathBuf};

const MIB: usize = 1024 * 1024;

/// Unpacks every test image into a folder of its own (names matter: the
/// images refer to each other).
fn unpack() -> (PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("wdfr-containers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/containers");
    for e in std::fs::read_dir(&data).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_str().unwrap().strip_suffix(".gz").unwrap().to_string();
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&p).unwrap()).read_to_end(&mut out).unwrap();
        std::fs::write(dir.join(name), out).unwrap();
    }
    let raw = std::fs::read(dir.join("raw.img")).unwrap();
    (dir, raw)
}

fn read_all(path: &Path) -> (String, Vec<u8>) {
    let kind = wdfr::container::kind(path.to_str().unwrap()).unwrap_or("none").to_string();
    let (src, _) = wdfr::container::open(path.to_str().unwrap()).unwrap();
    let mut v = vec![0u8; src.size() as usize];
    // Odd-sized reads, so that reads start and end inside blocks.
    let mut pos = 0;
    while pos < v.len() {
        let n = 7777.min(v.len() - pos);
        src.read_exact_at(pos as u64, &mut v[pos..pos + n]).unwrap();
        pos += n;
    }
    (kind, v)
}

#[test]
fn every_image_format_reads_back_the_disk_it_holds() {
    let (dir, raw) = unpack();
    let cases = [
        ("dynamic.vhd", "VHD"),
        ("fixed.vhd", "VHD"),
        ("disk.vhdx", "VHDX"),
        ("sparse.vmdk", "VMDK"),
        ("stream.vmdk", "VMDK"),
        ("split.vmdk", "VMDK"),
        ("flat.vmdk", "VMDK"),
        ("disk.vdi", "VDI"),
        ("plain.qcow2", "QCOW2"),
        ("zlib.qcow2", "QCOW2"),
        ("zstd.qcow2", "QCOW2"),
        ("comp.E01", "E01"),
        ("split.E01", "E01"),
    ];
    for (name, want) in cases {
        let (kind, disk) = read_all(&dir.join(name));
        assert_eq!(kind, want, "{name}");
        // VHD rounds the disk up to whole cylinders: zeros after it.
        assert!(disk.len() >= raw.len(), "{name}: {} bytes", disk.len());
        assert!(disk[..raw.len()] == raw[..], "{name}: different contents");
        assert!(disk[raw.len()..].iter().all(|&b| b == 0), "{name}: data after the disk");
    }
    // Snapshots: their own changes over the parent's contents.
    for (name, at, byte) in [("child.qcow2", 2 * MIB, 0xAB), ("child.vmdk", 3 * MIB, 0xCD)] {
        let (_, disk) = read_all(&dir.join(name));
        let mut want = raw.clone();
        want[at..at + 65536].fill(byte);
        assert!(disk == want, "{name}: different contents");
    }
    // A plain image is not a container.
    assert_eq!(wdfr::container::kind(dir.join("raw.img").to_str().unwrap()), None);
    let _ = std::fs::remove_dir_all(&dir);
}
