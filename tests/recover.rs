//! The full pipeline on an image file: scan (file system + deep search)
//! then save, in both output layouts, plus the one-shot `run`.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use wdfr::filter::Filter;
use wdfr::progress::Silent;
use wdfr::recover::{self, Item, Layout, Method, Options, SaveOptions, ScanOptions, Session};

mod common;

use common::*;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("wdfr-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A FAT16 image with one deleted photo (found via metadata) and one PNG
/// with no directory entry at all (found only by the deep search).
fn image(dir: &Path) -> (PathBuf, Vec<u8>, Vec<u8>) {
    let mut img = Image::new();
    let photo = jpeg();
    let mut entries = long_entries("Holiday.jpg", b"HOLIDAY JPG", 0x20, 10, photo.len() as u32);
    delete(&mut entries);
    img.d[ROOT_OFF..ROOT_OFF + entries.len()].copy_from_slice(&entries);
    img.cluster(10)[..photo.len()].copy_from_slice(&photo);
    let orphan = png();
    img.cluster(50)[..orphan.len()].copy_from_slice(&orphan);
    let path = dir.join("card.img");
    std::fs::write(&path, &img.d).unwrap();
    (path, photo, orphan)
}

fn scan_opts() -> ScanOptions {
    ScanOptions {
        method: Method::All,
        partition: None,
        filter: Filter::default(),
        carve_all_space: false,
        step: 512,
        max_carve_size: None,
    }
}

fn save_opts(out: PathBuf, layout: Layout) -> SaveOptions {
    SaveOptions { out, layout, restore_dates: true, write_report: true, allow_same_volume: false }
}

#[test]
fn scan_then_save_selected_files() {
    let tmp = TempDir::new("pipeline");
    let (path, photo, orphan) = image(&tmp.0);
    let session = Session::open(path.to_str().unwrap()).unwrap();
    let never = AtomicBool::new(false);

    let found = recover::scan(&session, &scan_opts(), &Silent, &never).unwrap();
    assert_eq!(found.fs.len(), 1);
    assert_eq!(found.fs[0].file.path, "Holiday.jpg");
    // The photo was recovered from metadata, so the deep search must not
    // report it a second time; only the orphan PNG is new.
    assert_eq!(found.carved.len(), 1);
    assert_eq!(found.carved[0].ext, "png");

    let items: Vec<Item> = found.fs.iter().map(Item::Fs).chain(found.carved.iter().map(Item::Carved)).collect();

    // Previews read the same bytes that saving writes.
    assert_eq!(recover::read_item(&session, items[0], 1 << 20).unwrap().unwrap(), photo);
    assert_eq!(recover::read_item(&session, items[0], 10).unwrap(), None);

    let out = tmp.0.join("original");
    let sum = recover::save(&session, &items, &save_opts(out.clone(), Layout::Original), &Silent, &never).unwrap();
    assert_eq!((sum.fs_files, sum.carved_files, sum.failures), (1, 1, 0));
    assert_eq!(std::fs::read(out.join("volume_FAT16/Holiday.jpg")).unwrap(), photo);
    let carved = out.join("carved/images").join(recover::carved_name(&found.carved[0]));
    assert_eq!(std::fs::read(carved).unwrap(), orphan);
    assert!(sum.report.unwrap().ends_with("report.csv"));

    // Saving only a selection, sorted by type.
    let out = tmp.0.join("bytype");
    let sum = recover::save(&session, &items[..1], &save_opts(out.clone(), Layout::ByType), &Silent, &never).unwrap();
    assert_eq!((sum.fs_files, sum.carved_files), (1, 0));
    assert_eq!(std::fs::read(out.join("images/Holiday.jpg")).unwrap(), photo);
}

#[test]
fn run_recovers_everything() {
    let tmp = TempDir::new("run");
    let (path, photo, orphan) = image(&tmp.0);
    let session = Session::open(path.to_str().unwrap()).unwrap();
    let out = tmp.0.join("out");
    let opts = Options {
        scan: scan_opts(),
        save: save_opts(out.clone(), Layout::ByType),
        include_overwritten: false,
        keep_duplicates: false,
    };
    let sum = recover::run(&session, &opts, &Silent, &AtomicBool::new(false)).unwrap();
    assert_eq!((sum.fs_files, sum.carved_files), (1, 1));
    let mut names: Vec<_> = std::fs::read_dir(out.join("images")).unwrap().map(|e| e.unwrap().path()).collect();
    names.sort();
    let contents: Vec<Vec<u8>> = names.iter().map(|p| std::fs::read(p).unwrap()).collect();
    assert!(contents.contains(&photo) && contents.contains(&orphan));
}

#[test]
fn saved_scans_reopen_identically_and_only_on_the_same_source() {
    let tmp = TempDir::new("saved");
    let (path, photo, orphan) = image(&tmp.0);
    let session = Session::open(path.to_str().unwrap()).unwrap();
    let found = recover::scan(&session, &scan_opts(), &Silent, &AtomicBool::new(false)).unwrap();

    let file = tmp.0.join("card.wdfrscan");
    wdfr::saved::save(&file, &session, &found).unwrap();
    let saved = wdfr::saved::load(&file).unwrap();
    assert_eq!(saved.source, session.path);
    let reopened = Session::open(&saved.source).unwrap();
    let again = saved.into_found(&reopened).unwrap();

    assert_eq!(again.fs.len(), found.fs.len());
    assert_eq!(again.fs[0].file.path, found.fs[0].file.path);
    assert_eq!(again.fs[0].file.condition, found.fs[0].file.condition);
    assert_eq!(again.carved.len(), found.carved.len());
    assert_eq!(recover::carved_name(&again.carved[0]), recover::carved_name(&found.carved[0]));
    // The reopened results read exactly the same bytes.
    assert_eq!(recover::read_item(&reopened, Item::Fs(&again.fs[0]), 1 << 20).unwrap().unwrap(), photo);
    assert_eq!(recover::read_item(&reopened, Item::Carved(&again.carved[0]), 1 << 20).unwrap().unwrap(), orphan);

    // A different source is refused rather than read at the wrong places.
    let other = tmp.0.join("other.img");
    std::fs::write(&other, vec![0u8; 1 << 20]).unwrap();
    let other = Session::open(other.to_str().unwrap()).unwrap();
    let err = wdfr::saved::load(&file).unwrap().into_found(&other).unwrap_err();
    assert!(err.to_string().contains("another drive"), "{err}");

    // Not a saved scan at all.
    std::fs::write(tmp.0.join("junk.wdfrscan"), b"hello").unwrap();
    assert!(wdfr::saved::load(&tmp.0.join("junk.wdfrscan")).is_err());
}
