//! Original names for files from an emptied Recycle Bin.
//!
//! Since Windows Vista, deleting a file to the Recycle Bin moves it to
//! `$Recycle.Bin\<SID>\$R<id>.<ext>` and writes a small `$I<id>.<ext>`
//! next to it holding the original path, size and deletion time. Emptying
//! the bin deletes both, so a scan finds them as deleted files named
//! `$RXK3T9Q.jpg`. Both usually survive (the `$I` file is tiny and is kept
//! inside its NTFS record), so the original location can be restored. The
//! same layout is used on FAT and exFAT USB drives (`$RECYCLE.BIN`).
//!
//! `$I` layout: version (u64; 1 = Vista–8.1, 2 = Windows 10+), original
//! size (u64), deletion time (FILETIME), then the UTF-16 path — 260
//! characters in version 1, preceded by its length (u32, characters
//! including the terminating zero) in version 2.

use std::collections::HashMap;

use chrono::NaiveDateTime;

use super::{DeletedFile, filetime};
use crate::bytes::{le32, le64};

/// What an `$I` file says about its `$R` partner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinEntry {
    /// Volume-relative, `/`-separated (the drive letter is dropped).
    pub original_path: String,
    pub size: u64,
    pub deleted: Option<NaiveDateTime>,
}

/// Parses the content of an `$I` file.
pub fn parse_info(data: &[u8]) -> Option<BinEntry> {
    let version = le64(data, 0)?;
    let size = le64(data, 8)?;
    let deleted = filetime(le64(data, 16)?);
    let units: Vec<u16> = match version {
        1 => data.get(24..24 + 520)?.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect(),
        2 => {
            let chars = le32(data, 24)? as usize;
            if chars == 0 || chars > 32_768 {
                return None;
            }
            data.get(28..28 + chars * 2)?.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()
        }
        _ => return None,
    };
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    let path = String::from_utf16(&units[..end]).ok()?;
    let original_path = volume_relative(&path)?;
    Some(BinEntry { original_path, size, deleted })
}

/// `C:\Users\Ann\a.jpg` → `Users/Ann/a.jpg`.
fn volume_relative(path: &str) -> Option<String> {
    let p = path.replace('\\', "/");
    let rest = match p.as_bytes() {
        [d, b':', b'/', ..] if d.is_ascii_alphabetic() => &p[3..],
        _ => p.trim_start_matches('/'),
    };
    let rest = rest.trim_matches('/');
    (!rest.is_empty() && !rest.split('/').any(|c| c.is_empty() || c == "." || c == "..")).then(|| rest.to_string())
}

/// `<…>/$Recycle.Bin/<SID>/$R<id>.ext[/…]` → (bin folder, id, path inside a
/// deleted folder). Case-insensitive, as FAT stores `$RECYCLE.BIN`.
fn split_bin_path(path: &str, marker: char) -> Option<(&str, &str, &str)> {
    let lower = path.to_ascii_lowercase();
    let bin = lower.find("$recycle.bin/")?;
    let after_bin = bin + "$recycle.bin/".len();
    let sid_end = after_bin + path[after_bin..].find('/')?;
    let name_start = sid_end + 1;
    let rest = &path[name_start..];
    let (name, inner) = rest.split_once('/').unwrap_or((rest, ""));
    let mut chars = name.chars();
    let (Some('$'), Some(m)) = (chars.next(), chars.next()) else { return None };
    if !m.eq_ignore_ascii_case(&marker) || name.len() < 3 {
        return None;
    }
    Some((&path[..sid_end], &name[2..], inner))
}

/// Renames files from an emptied Recycle Bin to where they were deleted
/// from, and drops the `$I` bookkeeping files. `read` returns a deleted
/// file's content (only called for small `$I` files).
pub fn restore_names(files: &mut Vec<DeletedFile>, read: impl Fn(&DeletedFile) -> Option<Vec<u8>>) {
    // (bin folder, lower-case id) → entries; a bin emptied more than once can
    // leave several `$I` files with the same id.
    let mut info: HashMap<(String, String), Vec<BinEntry>> = HashMap::new();
    let mut is_info = vec![false; files.len()];
    for (i, f) in files.iter().enumerate() {
        let Some((bin, id, "")) = split_bin_path(&f.path, 'I') else { continue };
        is_info[i] = true;
        if f.size > 64 * 1024 {
            continue;
        }
        if let Some(entry) = read(f).as_deref().and_then(parse_info) {
            info.entry((bin.to_ascii_lowercase(), id.to_ascii_lowercase())).or_default().push(entry);
        }
    }

    for f in files.iter_mut() {
        let Some((bin, id, inner)) = split_bin_path(&f.path, 'R') else { continue };
        let Some(entries) = info.get(&(bin.to_ascii_lowercase(), id.to_ascii_lowercase())) else { continue };
        // A file must match its recorded size; for files inside a deleted
        // folder the `$I` size is the folder's total, so any entry will do.
        let entry = if inner.is_empty() { entries.iter().find(|e| e.size == f.size) } else { entries.first() };
        let Some(entry) = entry else { continue };
        let new_path =
            if inner.is_empty() { entry.original_path.clone() } else { format!("{}/{inner}", entry.original_path) };
        let when = entry.deleted.map(|d| format!(" on {}", d.format("%Y-%m-%d %H:%M"))).unwrap_or_default();
        let note = format!("from the emptied Recycle Bin (deleted{when})");
        f.note = Some(match f.note.take() {
            Some(n) => format!("{note}; {n}"),
            None => note,
        });
        f.path = new_path;
    }

    let mut i = 0;
    files.retain(|_| {
        let keep = !is_info[i];
        i += 1;
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{Condition, FileData};

    fn info_v2(path: &str, size: u64, ft: u64) -> Vec<u8> {
        let units: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let mut v = Vec::new();
        v.extend_from_slice(&2u64.to_le_bytes());
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&ft.to_le_bytes());
        v.extend_from_slice(&(units.len() as u32).to_le_bytes());
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    fn info_v1(path: &str, size: u64) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&1u64.to_le_bytes());
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        let mut units: Vec<u16> = path.encode_utf16().collect();
        units.resize(260, 0);
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    fn file(path: &str, size: u64, data: Vec<u8>) -> DeletedFile {
        DeletedFile {
            id: 0,
            path: path.into(),
            size,
            created: None,
            modified: None,
            condition: Condition::Recoverable,
            note: None,
            data: FileData::Resident(data),
        }
    }

    fn resident(f: &DeletedFile) -> Option<Vec<u8>> {
        match &f.data {
            FileData::Resident(v) => Some(v.clone()),
            _ => None,
        }
    }

    #[test]
    fn parses_both_versions() {
        // 2020-01-01 00:00:00 UTC
        let e = parse_info(&info_v2(r"C:\Users\Ann\Pictures\beach.jpg", 1234, 132_223_104_000_000_000)).unwrap();
        assert_eq!(e.original_path, "Users/Ann/Pictures/beach.jpg");
        assert_eq!(e.size, 1234);
        assert_eq!(e.deleted.unwrap().to_string(), "2020-01-01 00:00:00");
        let e = parse_info(&info_v1(r"D:\Work\report.docx", 99)).unwrap();
        assert_eq!(e.original_path, "Work/report.docx");
        assert_eq!(parse_info(&[0; 10]), None);
        assert_eq!(parse_info(&info_v2(r"C:\..\x", 1, 0)), None);
    }

    #[test]
    fn restores_files_and_folders_and_drops_info_files() {
        let bin = "$Recycle.Bin/S-1-5-21-1-2-3-1001";
        let mut files = vec![
            file(&format!("{bin}/$I1AB2CD.jpg"), 100, info_v2(r"C:\Users\Ann\beach.jpg", 5, 0)),
            file(&format!("{bin}/$R1AB2CD.jpg"), 5, b"photo".to_vec()),
            file(&format!("{bin}/$IFOLDER"), 100, info_v2(r"C:\Users\Ann\Trip", 900, 0)),
            file(&format!("{bin}/$RFOLDER/day1/a.jpg"), 3, b"abc".to_vec()),
            // No matching $I: left alone.
            file(&format!("{bin}/$RZZZZZZ.txt"), 1, b"x".to_vec()),
            file("Users/Ann/other.txt", 1, b"y".to_vec()),
        ];
        restore_names(&mut files, resident);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            ["Users/Ann/beach.jpg", "Users/Ann/Trip/day1/a.jpg", &format!("{bin}/$RZZZZZZ.txt"), "Users/Ann/other.txt"]
        );
        assert!(files[0].note.as_deref().unwrap().contains("Recycle Bin"));
    }

    #[test]
    fn fat_spelling_and_size_check() {
        let bin = "$RECYCLE.BIN/S-1-5-21-9";
        let mut files = vec![
            file(&format!("{bin}/$IAB12CD.JPG"), 100, info_v2(r"E:\DCIM\IMG_1.JPG", 4, 0)),
            file(&format!("{bin}/$RAB12CD.JPG"), 4, b"abcd".to_vec()),
            // Same id, wrong size: a stale `$I` must not rename it.
            file(&format!("{bin}/$IXY99ZZ.png"), 100, info_v2(r"E:\a.png", 999, 0)),
            file(&format!("{bin}/$RXY99ZZ.png"), 4, b"abcd".to_vec()),
        ];
        restore_names(&mut files, resident);
        assert_eq!(files[0].path, "DCIM/IMG_1.JPG");
        assert_eq!(files[1].path, format!("{bin}/$RXY99ZZ.png"));
        assert_eq!(files.len(), 2);
    }
}
