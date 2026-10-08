//! Repairing damaged JPEG photos.
//!
//! Two kinds of damage are common after recovery, and fixable:
//!
//! * **The end is missing** (the file was cut short, or its last clusters
//!   were reused): the picture data is closed properly, so viewers show
//!   everything that is left instead of refusing the file.
//! * **The beginning is gone** (its first clusters were overwritten), but
//!   the picture data survives. A camera writes the same header (sizes,
//!   colour tables, compression tables) for every photo taken with the
//!   same settings, so the header of a good photo from the same camera is
//!   put in front of the surviving picture data.
//!
//! The result is not checked here: the caller decodes it.

/// A repaired photo, and what was done to it.
pub struct Repaired {
    pub bytes: Vec<u8>,
    pub fixes: Vec<Fix>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix {
    /// The missing end of the picture data was closed.
    ClosedEnd,
    /// The header of a photo from the same camera was used.
    NewHeader,
}

const SOI: [u8; 2] = [0xFF, 0xD8];
const EOI: [u8; 2] = [0xFF, 0xD9];

/// Repairs `damaged`, with the header of `reference` (a good photo from the
/// same camera) if given. `None` if there is nothing to repair with.
pub fn repair_jpeg(damaged: &[u8], reference: Option<&[u8]>) -> Option<Repaired> {
    let mut fixes = Vec::new();
    let mut out: Vec<u8> = match (header_end(damaged), reference) {
        // The photo's own header is intact.
        (Some(_), _) => damaged.to_vec(),
        // The header is gone: take the reference's, and the picture data
        // from where the reference's started.
        (None, Some(r)) => {
            let at = header_end(r)?;
            let data = damaged.get(at..).filter(|d| !d.is_empty())?;
            fixes.push(Fix::NewHeader);
            let mut v = r[..at].to_vec();
            v.extend_from_slice(data);
            v
        }
        (None, None) => return None,
    };
    // The end: the picture data must end with the end-of-image marker.
    let body = header_end(&out)?;
    if !out[body..].ends_with(&EOI) {
        // Drop what the cut left behind: zeros, or a dangling 0xFF.
        while out.len() > body && matches!(out.last(), Some(0x00 | 0xFF)) {
            out.pop();
        }
        // An end marker inside, followed by leftovers: cut there.
        if let Some(i) = out[body..].windows(2).rposition(|w| w == EOI) {
            out.truncate(body + i);
        }
        out.extend_from_slice(&EOI);
        fixes.push(Fix::ClosedEnd);
    }
    (!fixes.is_empty()).then_some(Repaired { bytes: out, fixes })
}

/// Where the picture data starts: just after the header of the first scan
/// (`FF DA` and its length), if the file starts like a JPEG.
fn header_end(b: &[u8]) -> Option<usize> {
    if !b.starts_with(&SOI) {
        return None;
    }
    let mut pos = 2usize;
    for _ in 0..1024 {
        if *b.get(pos)? != 0xFF {
            return None;
        }
        let marker = *b.get(pos + 1)?;
        let len = usize::from(u16::from_be_bytes([*b.get(pos + 2)?, *b.get(pos + 3)?]));
        if len < 2 {
            return None;
        }
        pos += 2 + len;
        if marker == 0xDA {
            return (pos <= b.len()).then_some(pos);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG-shaped file: SOI, a quantisation table, a frame header, a
    /// scan header, `scan` bytes of picture data, EOI.
    fn photo(scan: &[u8]) -> Vec<u8> {
        let mut v = SOI.to_vec();
        v.extend_from_slice(&[0xFF, 0xDB, 0, 67, 0]);
        v.extend((0..64).map(|i| i as u8 + 1));
        v.extend_from_slice(&[0xFF, 0xC0, 0, 11, 8, 0, 16, 0, 16, 1, 1, 0x11, 0]);
        v.extend_from_slice(&[0xFF, 0xDA, 0, 8, 1, 1, 0, 0, 63, 0]);
        v.extend_from_slice(scan);
        v.extend_from_slice(&EOI);
        v
    }

    #[test]
    fn a_cut_photo_gets_its_end_back() {
        let whole = photo(&[0x12; 500]);
        let mut cut = whole[..whole.len() - 200].to_vec();
        cut.extend(vec![0u8; 300]); // what follows on the disk
        let r = repair_jpeg(&cut, None).unwrap();
        assert_eq!(r.fixes, vec![Fix::ClosedEnd]);
        assert!(r.bytes.ends_with(&EOI));
        assert_eq!(&r.bytes[..r.bytes.len() - 2], &whole[..whole.len() - 200]);
        // A whole photo needs nothing.
        assert!(repair_jpeg(&whole, None).is_none());
    }

    #[test]
    fn a_photo_without_its_header_gets_one_from_the_same_camera() {
        let original = photo(&[0x34; 900]);
        let reference = photo(&[0x56; 300]);
        let mut damaged = original.clone();
        damaged[..60].fill(0); // the first cluster was overwritten
        assert!(repair_jpeg(&damaged, None).is_none());
        let r = repair_jpeg(&damaged, Some(&reference)).unwrap();
        assert_eq!(r.fixes, vec![Fix::NewHeader]);
        assert_eq!(r.bytes, original);
    }
}
