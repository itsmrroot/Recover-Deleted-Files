//! Builds a small but structurally faithful NTFS volume in memory (boot
//! sector, $MFT with update sequence fixups, $Bitmap, directory and file
//! records with data runs) and checks deleted-file recovery end to end.

use std::sync::Arc;

use wdfr::fs::{self, Condition, DeletedFile, FsKind, Volume};
use wdfr::source::{MemSource, Source};

const SECTOR: usize = 512;
const CLUSTER: usize = 512;
const RECORD: usize = 1024;
const TOTAL_CLUSTERS: usize = 4096;
const MFT_LCN: usize = 16;
const MFT_RECORDS: usize = 48;
const BITMAP_LCN: usize = 120;

const IN_USE: u16 = 1;
const DIR: u16 = 2;

fn fref(record: u64, seq: u16) -> u64 {
    record | (u64::from(seq) << 48)
}

/// Encodes absolute (LCN, length) runs as an NTFS mapping-pairs array.
fn runlist(runs: &[(Option<i64>, u64)]) -> Vec<u8> {
    fn min_bytes_signed(v: i64) -> usize {
        (1..=8)
            .find(|&n| {
                let bits = n * 8;
                let lo = -(1i128 << (bits - 1));
                let hi = (1i128 << (bits - 1)) - 1;
                (lo..=hi).contains(&i128::from(v))
            })
            .unwrap()
    }
    fn min_bytes_unsigned(v: u64) -> usize {
        (1..=8).find(|&n| n == 8 || v < 1u64 << (n * 8)).unwrap()
    }
    let mut out = Vec::new();
    let mut prev = 0i64;
    for &(lcn, len) in runs {
        let ls = min_bytes_unsigned(len);
        match lcn {
            None => {
                out.push(ls as u8);
                out.extend_from_slice(&len.to_le_bytes()[..ls]);
            }
            Some(l) => {
                let delta = l - prev;
                prev = l;
                let os = min_bytes_signed(delta);
                out.push(((os as u8) << 4) | ls as u8);
                out.extend_from_slice(&len.to_le_bytes()[..ls]);
                out.extend_from_slice(&delta.to_le_bytes()[..os]);
            }
        }
    }
    out.push(0);
    out
}

struct Rec {
    b: Vec<u8>,
    off: usize,
}

impl Rec {
    fn new(seq: u16, flags: u16, base: u64) -> Self {
        let mut b = vec![0u8; RECORD];
        b[0..4].copy_from_slice(b"FILE");
        b[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // USA offset
        b[6..8].copy_from_slice(&3u16.to_le_bytes()); // USA count: 1 + 2 sectors
        b[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
        b[0x12..0x14].copy_from_slice(&1u16.to_le_bytes());
        b[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        b[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        b[0x1C..0x20].copy_from_slice(&(RECORD as u32).to_le_bytes());
        b[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        Rec { b, off: 0x38 }
    }

    fn push_attr(&mut self, a: Vec<u8>) -> &mut Self {
        let len = a.len();
        self.b[self.off..self.off + len].copy_from_slice(&a);
        self.off += len;
        self
    }

    fn resident(&mut self, ty: u32, value: &[u8]) -> &mut Self {
        let len = (0x18 + value.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a[0x18..0x18 + value.len()].copy_from_slice(value);
        self.push_attr(a)
    }

    #[allow(clippy::too_many_arguments)]
    fn non_resident(
        &mut self,
        ty: u32,
        flags: u16,
        cu: u8,
        start_vcn: u64,
        runs: &[u8],
        clusters: u64,
        real: u64,
    ) -> &mut Self {
        let len = (0x40 + runs.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[0x0A..0x0C].copy_from_slice(&0x40u16.to_le_bytes());
        a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
        a[0x10..0x18].copy_from_slice(&start_vcn.to_le_bytes());
        a[0x18..0x20].copy_from_slice(&(start_vcn + clusters - 1).to_le_bytes());
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        a[0x22] = cu;
        let alloc = clusters * CLUSTER as u64;
        a[0x28..0x30].copy_from_slice(&alloc.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&real.to_le_bytes());
        a[0x38..0x40].copy_from_slice(&real.to_le_bytes());
        a[0x40..0x40 + runs.len()].copy_from_slice(runs);
        self.push_attr(a)
    }

    fn file_name(&mut self, parent: u64, name: &str) -> &mut Self {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42 + units.len() * 2];
        v[0..8].copy_from_slice(&parent.to_le_bytes());
        // 2024-02-29 12:00:00 UTC (a leap day, as a nod to DFTT #7)
        let ft: u64 = 133_536_816_000_000_000;
        for o in [0x08, 0x10, 0x18, 0x20] {
            v[o..o + 8].copy_from_slice(&ft.to_le_bytes());
        }
        v[0x40] = units.len() as u8;
        v[0x41] = 1; // Win32 namespace
        for (i, u) in units.iter().enumerate() {
            v[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        self.resident(0x30, &v)
    }

    /// Terminates the attribute list and applies the update sequence array
    /// the way NTFS writes records to disk.
    fn finish(&mut self) -> Vec<u8> {
        let mut b = self.b.clone();
        b[self.off..self.off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let used = (self.off + 8) as u32;
        b[0x18..0x1C].copy_from_slice(&used.to_le_bytes());
        let usn = [0x07, 0x00];
        b[0x30..0x32].copy_from_slice(&usn);
        for i in 0..2 {
            let end = (i + 1) * SECTOR;
            let orig = [b[end - 2], b[end - 1]];
            b[0x32 + i * 2..0x34 + i * 2].copy_from_slice(&orig);
            b[end - 2..end].copy_from_slice(&usn);
        }
        b
    }
}

struct Image {
    d: Vec<u8>,
    bitmap: Vec<u8>,
}

impl Image {
    fn new() -> Self {
        let mut d = vec![0u8; TOTAL_CLUSTERS * CLUSTER];
        let bs = &mut d[..SECTOR];
        bs[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        bs[3..11].copy_from_slice(b"NTFS    ");
        bs[0x0B..0x0D].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        bs[0x0D] = (CLUSTER / SECTOR) as u8;
        bs[0x28..0x30].copy_from_slice(&(TOTAL_CLUSTERS as u64).to_le_bytes());
        bs[0x30..0x38].copy_from_slice(&(MFT_LCN as u64).to_le_bytes());
        bs[0x38..0x40].copy_from_slice(&8u64.to_le_bytes());
        bs[0x40] = 0xF6; // 2^10 = 1024-byte records
        bs[510] = 0x55;
        bs[511] = 0xAA;
        let mut img = Image { d, bitmap: vec![0u8; TOTAL_CLUSTERS / 8] };
        // Metadata region (boot, MFT, bitmap) is allocated.
        img.allocate(0, BITMAP_LCN + 1);
        img
    }

    fn allocate(&mut self, lcn: usize, n: usize) {
        for c in lcn..lcn + n {
            self.bitmap[c / 8] |= 1 << (c % 8);
        }
    }

    fn write_clusters(&mut self, lcn: usize, data: &[u8]) {
        let o = lcn * CLUSTER;
        self.d[o..o + data.len()].copy_from_slice(data);
    }

    fn put(&mut self, no: usize, rec: Vec<u8>) {
        let o = MFT_LCN * CLUSTER + no * RECORD;
        self.d[o..o + RECORD].copy_from_slice(&rec);
    }

    fn build(mut self) -> Source {
        let mft_clusters = (MFT_RECORDS * RECORD / CLUSTER) as u64;
        let mft_runs = runlist(&[(Some(MFT_LCN as i64), mft_clusters)]);
        let rec0 = Rec::new(1, IN_USE, 0)
            .file_name(fref(5, 5), "$MFT")
            .non_resident(0x80, 0, 0, 0, &mft_runs, mft_clusters, (MFT_RECORDS * RECORD) as u64)
            .finish();
        self.put(0, rec0);
        let root = Rec::new(5, IN_USE | DIR, 0).file_name(fref(5, 5), ".").finish();
        self.put(5, root);
        let bm_runs = runlist(&[(Some(BITMAP_LCN as i64), 1)]);
        let rec6 = Rec::new(6, IN_USE, 0)
            .file_name(fref(5, 5), "$Bitmap")
            .non_resident(0x80, 0, 0, 0, &bm_runs, 1, (TOTAL_CLUSTERS / 8) as u64)
            .finish();
        self.put(6, rec6);
        let bitmap = self.bitmap.clone();
        self.write_clusters(BITMAP_LCN, &bitmap);
        Arc::new(MemSource(self.d))
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn find<'a>(files: &'a [DeletedFile], path: &str) -> &'a DeletedFile {
    files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path} not found in {:?}", files.iter().map(|f| &f.path).collect::<Vec<_>>()))
}

fn content(vol: &dyn Volume, f: &DeletedFile) -> Vec<u8> {
    let mut out = Vec::new();
    fs::extract(vol.source().as_ref(), f, &mut out).unwrap();
    out
}

#[test]
fn recovers_deleted_ntfs_files() {
    let mut img = Image::new();

    // 30: live directory "Photos"
    img.put(30, Rec::new(1, IN_USE | DIR, 0).file_name(fref(5, 5), "Photos").finish());

    // 31: deleted, fragmented file in Photos (runs 200-201 and 300-302).
    let beach = pattern(2300, 1);
    img.write_clusters(200, &beach[..1024]);
    img.write_clusters(300, &beach[1024..]);
    let runs = runlist(&[(Some(200), 2), (Some(300), 3)]);
    img.put(
        31,
        Rec::new(2, 0, 0)
            .file_name(fref(30, 1), "beach.jpg")
            .non_resident(0x80, 0, 0, 0, &runs, 5, beach.len() as u64)
            .finish(),
    );

    // 32: deleted resident file in the root.
    img.put(32, Rec::new(4, 0, 0).file_name(fref(5, 5), "note.txt").resident(0x80, b"hello ntfs").finish());

    // 34: deleted directory "Old" (seq bumped 1 -> 2 on delete);
    // 33: file inside it whose clusters were reallocated since.
    img.put(34, Rec::new(2, DIR, 0).file_name(fref(5, 5), "Old").finish());
    img.allocate(400, 3);
    let runs = runlist(&[(Some(400), 3)]);
    img.put(33, Rec::new(2, 0, 0).file_name(fref(34, 1), "x.bin").non_resident(0x80, 0, 0, 0, &runs, 3, 1500).finish());

    // 36: record reused by an unrelated live file; 35 still points at the
    // old incarnation of 36, so its directory is unknown.
    img.put(36, Rec::new(9, IN_USE, 0).file_name(fref(5, 5), "new.txt").finish());
    img.put(35, Rec::new(2, 0, 0).file_name(fref(36, 1), "lost.dat").resident(0x80, b"orphan").finish());

    // 37: LZNT1-compressed file: one 16-cluster unit, 1 cluster stored.
    let comp = [0x05, 0xB0, 0x08, b'a', b'b', b'c', 0x06, 0x20, 0, 0];
    img.write_clusters(500, &comp);
    let runs = runlist(&[(Some(500), 1), (None, 15)]);
    img.put(
        37,
        Rec::new(2, 0, 0).file_name(fref(5, 5), "packed.txt").non_resident(0x80, 0x0001, 4, 0, &runs, 16, 12).finish(),
    );

    // 38: deleted base record with an $ATTRIBUTE_LIST; its $DATA lives in
    // extension record 39 (whose base reference carries the pre-delete
    // sequence number).
    let big = pattern(3000, 9);
    img.write_clusters(600, &big);
    img.put(38, Rec::new(3, 0, 0).file_name(fref(5, 5), "big.mov").resident(0x20, &[0u8; 32]).finish());
    let runs = runlist(&[(Some(600), 6)]);
    img.put(39, Rec::new(1, 0, fref(38, 2)).non_resident(0x80, 0, 0, 0, &runs, 6, big.len() as u64).finish());

    let src = img.build();
    assert_eq!(fs::detect(src.as_ref()), Some(FsKind::Ntfs));
    let vol = fs::open(src).unwrap();
    let files = vol.scan_deleted(&mut |_, _| {}).unwrap();

    let f = find(&files, "Photos/beach.jpg");
    assert_eq!(f.size, 2300);
    assert_eq!(f.condition, Condition::Recoverable);
    assert_eq!(f.modified.unwrap().to_string(), "2024-02-29 12:00:00");
    assert_eq!(content(vol.as_ref(), f), beach);

    let f = find(&files, "note.txt");
    assert_eq!(content(vol.as_ref(), f), b"hello ntfs");

    let f = find(&files, "Old/x.bin");
    assert_eq!(f.condition, Condition::Overwritten);

    let f = find(&files, "$Orphan/lost.dat");
    assert_eq!(content(vol.as_ref(), f), b"orphan");

    let f = find(&files, "packed.txt");
    assert_eq!(content(vol.as_ref(), f), b"abcabcabcabc");

    let f = find(&files, "big.mov");
    assert_eq!(f.size, 3000);
    assert_eq!(content(vol.as_ref(), f), big);

    // Live files and system records must not be reported.
    assert!(files.iter().all(|f| f.path != "new.txt" && !f.path.starts_with('$') || f.path.starts_with("$Orphan")));

    // Unallocated space covers deleted data but not the reused clusters.
    let free = vol.free_ranges().unwrap();
    let covers = |off: u64| free.iter().any(|r| r.contains(&off));
    assert!(covers(200 * CLUSTER as u64));
    assert!(!covers(400 * CLUSTER as u64));
    assert!(!covers(MFT_LCN as u64 * CLUSTER as u64));
}
