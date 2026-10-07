//! Checks that a found file is intact before it is saved.
//!
//! A file passes when its own structure — the JPEG marker chain, the PNG
//! chunks, the MP4 boxes, the ZIP central directory, ... — can be followed
//! from its start to its end within its size, using the same parsers as
//! the deep search. Data that was cut short or overwritten by other files
//! breaks that structure. Photos must also contain no block of zeros, which
//! is what an erased or never-written part looks like. Types without a
//! parser (plain text, ...) are left unchecked.
//!
//! Only what the parsers need is read, so even large videos are checked in
//! a moment.

use std::io;

use crate::carve::{self, Category, HEAD_LEN, Reader};
use crate::fs::{self, DeletedFile, Extent, FileData};
use crate::recover::{Item, Session};
use crate::source::{ReadAt, read_tolerant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Verdict {
    /// The file's structure is complete: it should open.
    Verified,
    /// The structure is broken or cut short: it may not open.
    Damaged,
    /// No check exists for this type, or the file could not be read.
    Unchecked,
}

/// Compressed NTFS files are unpacked in memory to be checked, up to this size.
const MAX_UNPACK: u64 = 64 << 20;
/// Images are searched for zero blocks up to this size.
const MAX_ZERO_SCAN: u64 = 256 << 20;
const ZERO_BLOCK: usize = 4096;

/// Checks one found file.
pub fn check(session: &Session, item: Item) -> Verdict {
    match item {
        Item::Carved(c) => {
            let view = Slice { src: session.disk.as_ref(), offset: c.offset, len: c.len };
            check_data(c.ext, &view)
        }
        Item::Fs(f) => {
            let Some(p) = session.partition(f.partition) else { return Verdict::Unchecked };
            let vol = p.source(&session.disk);
            let Some(ext) = f.file.name().rsplit_once('.').map(|(_, e)| e) else { return Verdict::Unchecked };
            check_file(vol.as_ref(), &f.file, ext)
        }
    }
}

/// Checks a file found through file-system metadata on volume `vol`.
pub fn check_file(vol: &dyn ReadAt, file: &DeletedFile, ext: &str) -> Verdict {
    match &file.data {
        FileData::Lost => Verdict::Unchecked,
        FileData::Resident(bytes) => {
            let n = bytes.len().min(file.size as usize);
            check_data(ext, &Bytes(&bytes[..n]))
        }
        FileData::Extents(extents) => check_data(ext, &FileView::new(vol, extents, file.size)),
        FileData::Compressed { .. } => {
            if file.size > MAX_UNPACK {
                return Verdict::Unchecked;
            }
            let mut out = Vec::with_capacity(file.size as usize);
            match fs::extract(vol, file, &mut out) {
                Ok(_) => check_data(ext, &Bytes(&out)),
                Err(_) => Verdict::Unchecked,
            }
        }
    }
}

/// Checks the content `data` of a file with extension `ext`.
pub fn check_data(ext: &str, data: &dyn ReadAt) -> Verdict {
    let ext = carve::normalize_ext(ext);
    let formats: Vec<_> =
        carve::all_formats().into_iter().filter(|f| f.kinds().iter().any(|(e, _)| *e == ext)).collect();
    if formats.is_empty() {
        return Verdict::Unchecked;
    }
    let size = data.size();
    if size == 0 {
        return Verdict::Damaged;
    }
    let mut head = vec![0u8; (size as usize).min(HEAD_LEN)];
    read_tolerant(data, 0, &mut head);
    let intact = formats.iter().any(|f| {
        f.probe(&head) && {
            let mut r = Reader::new(data, 0, size);
            f.measure(&mut r).is_some_and(|hit| hit.len > 0 && hit.len <= size)
        }
    });
    if !intact {
        return Verdict::Damaged;
    }
    if carve::category_for_ext(&ext) == Some(Category::Image) && has_zero_block(data) {
        return Verdict::Damaged;
    }
    Verdict::Verified
}

/// True when an aligned 4 KiB block of the data is all zeros. Compressed
/// image data never looks like that; erased or never-written space does.
fn has_zero_block(data: &dyn ReadAt) -> bool {
    let size = data.size().min(MAX_ZERO_SCAN);
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < size {
        let n = ((size - pos) as usize).min(buf.len());
        read_tolerant(data, pos, &mut buf[..n]);
        // Only whole blocks: a short last block is too small to judge.
        if buf[..n].as_chunks::<ZERO_BLOCK>().0.iter().any(|b| b.iter().all(|&x| x == 0)) {
            return true;
        }
        pos += n as u64;
    }
    false
}

/// A file's content, read through its extents on the volume.
struct FileView<'a> {
    vol: &'a dyn ReadAt,
    /// (logical start, extent), in file order.
    extents: Vec<(u64, Extent)>,
    size: u64,
}

impl<'a> FileView<'a> {
    fn new(vol: &'a dyn ReadAt, extents: &[Extent], size: u64) -> Self {
        let mut start = 0u64;
        let extents = extents
            .iter()
            .map(|e| {
                let s = start;
                start += e.len;
                (s, *e)
            })
            .collect();
        Self { vol, extents, size }
    }
}

impl ReadAt for FileView<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }
        // The extent holding `offset` (they are sorted by logical start).
        let i = self.extents.partition_point(|(s, _)| *s <= offset).saturating_sub(1);
        let Some(&(start, e)) = self.extents.get(i) else { return Ok(0) };
        let within = offset - start;
        if within >= e.len {
            // Past the last extent: the rest of the file was never stored.
            let n = buf.len().min((self.size - offset) as usize);
            buf[..n].fill(0);
            return Ok(n);
        }
        let n = buf.len().min((e.len - within) as usize).min((self.size - offset) as usize);
        match e.offset {
            Some(o) => self.vol.read_at(o + within, &mut buf[..n]),
            None => {
                buf[..n].fill(0);
                Ok(n)
            }
        }
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// `len` bytes of `src` from `offset`.
struct Slice<'a> {
    src: &'a dyn ReadAt,
    offset: u64,
    len: u64,
}

impl ReadAt for Slice<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let n = buf.len().min((self.len - offset) as usize);
        self.src.read_at(self.offset + offset, &mut buf[..n])
    }

    fn size(&self) -> u64 {
        self.len
    }
}

struct Bytes<'a>(&'a [u8]);

impl ReadAt for Bytes<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let Some(rest) = self.0.get(offset as usize..) else { return Ok(0) };
        let n = buf.len().min(rest.len());
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }

    fn size(&self) -> u64 {
        self.0.len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemSource;

    /// A minimal PNG: signature, IHDR, IEND.
    fn png() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
        v.extend_from_slice(&[0; 4]);
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(b"IEND");
        v.extend_from_slice(&[0xAE, 0x42, 0x60, 0x82]);
        v
    }

    #[test]
    fn whole_files_pass_and_cut_ones_do_not() {
        assert_eq!(check_data("png", &Bytes(&png())), Verdict::Verified);
        let cut = &png()[..40];
        assert_eq!(check_data("png", &Bytes(cut)), Verdict::Damaged);
        // Something else under a .png name.
        assert_eq!(check_data("PNG", &Bytes(b"not an image at all, just text")), Verdict::Damaged);
        assert_eq!(check_data("txt", &Bytes(b"hello")), Verdict::Unchecked);
    }

    #[test]
    fn images_with_a_block_of_zeros_are_damaged() {
        // A PNG whose (private) chunk holds 8 KiB of zeros.
        let mut v = png();
        let iend = v.split_off(v.len() - 12);
        v.extend_from_slice(&8192u32.to_be_bytes());
        v.extend_from_slice(b"zzZz");
        v.extend(vec![0u8; 8192]);
        v.extend_from_slice(&[0; 4]);
        v.extend(iend);
        assert_eq!(check_data("png", &Bytes(&v)), Verdict::Damaged);
    }

    #[test]
    fn files_are_read_through_their_extents() {
        // The PNG split in two pieces, stored in reverse order on the volume.
        let p = png();
        let mut disk = vec![0x11u8; 4096];
        disk[2048..2048 + 20].copy_from_slice(&p[..20]);
        disk[100..100 + p.len() - 20].copy_from_slice(&p[20..]);
        let vol = MemSource(disk);
        let file = DeletedFile {
            id: 0,
            path: "a.png".into(),
            size: p.len() as u64,
            created: None,
            modified: None,
            condition: fs::Condition::Recoverable,
            note: None,
            data: FileData::Extents(vec![
                Extent { offset: Some(2048), len: 20 },
                Extent { offset: Some(100), len: p.len() as u64 - 20 },
            ]),
        };
        assert_eq!(check_file(&vol, &file, "png"), Verdict::Verified);
        // The same file pointing at the wrong place.
        let wrong = DeletedFile { data: FileData::Extents(vec![Extent { offset: Some(0), len: 64 }]), ..file };
        assert_eq!(check_file(&vol, &wrong, "png"), Verdict::Damaged);
    }
}
