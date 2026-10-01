//! MFT FILE record parsing.

use crate::bytes::{le16, le32, le64, u8_at, utf16le};

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

pub const FLAG_IN_USE: u16 = 0x0001;
pub const FLAG_DIRECTORY: u16 = 0x0002;

const ATTR_FLAG_COMPRESSED: u16 = 0x0001;
const ATTR_FLAG_ENCRYPTED: u16 = 0x4000;

/// Lower 48 bits of a file reference are the record number.
#[inline]
pub fn ref_record(r: u64) -> u64 {
    r & 0x0000_FFFF_FFFF_FFFF
}

#[inline]
pub fn ref_seq(r: u64) -> u16 {
    (r >> 48) as u16
}

/// Applies the update sequence array ("fixups") in place. NTFS replaces the
/// last two bytes of every 512-byte stride with a check value; the originals
/// live in the array. Returns `false` if the record is torn (a stride's check
/// value does not match), in which case it is still restored best-effort.
pub fn apply_fixups(buf: &mut [u8]) -> bool {
    let (Some(usa_off), Some(usa_count)) = (le16(buf, 4), le16(buf, 6)) else {
        return false;
    };
    let (usa_off, usa_count) = (usize::from(usa_off), usize::from(usa_count));
    if usa_count < 2 || usa_off + usa_count * 2 > buf.len() {
        return false;
    }
    let stride = buf.len() / (usa_count - 1);
    if stride < 2 {
        return false;
    }
    let check = [buf[usa_off], buf[usa_off + 1]];
    let mut ok = true;
    for i in 1..usa_count {
        let end = i * stride;
        if end > buf.len() {
            return false;
        }
        if buf[end - 2..end] != check {
            ok = false;
        }
        let orig = [buf[usa_off + 2 * i], buf[usa_off + 2 * i + 1]];
        buf[end - 2..end].copy_from_slice(&orig);
    }
    ok
}

/// One data run, in clusters. `lcn == None` is a sparse run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub lcn: Option<u64>,
    pub len: u64,
}

/// Decodes a mapping-pairs array. Returns `None` on malformed input.
pub fn decode_runlist(b: &[u8]) -> Option<Vec<Run>> {
    let mut runs = Vec::new();
    let mut i = 0usize;
    let mut lcn: i64 = 0;
    while let Some(&h) = b.get(i) {
        if h == 0 {
            break;
        }
        let len_size = usize::from(h & 0x0F);
        let off_size = usize::from(h >> 4);
        i += 1;
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return None;
        }
        let len = crate::bytes::le_uint(b.get(i..i + len_size)?)?;
        i += len_size;
        if off_size == 0 {
            runs.push(Run { lcn: None, len });
        } else {
            let delta = crate::bytes::le_int(b.get(i..i + off_size)?)?;
            lcn = lcn.checked_add(delta)?;
            if lcn < 0 {
                return None;
            }
            runs.push(Run { lcn: Some(lcn as u64), len });
        }
        i += off_size;
    }
    Some(runs)
}

#[derive(Debug, Clone)]
pub struct FileName {
    pub parent: u64,
    pub name: String,
    pub namespace: u8,
    pub created: u64,
    pub modified: u64,
    pub real_size: u64,
}

impl FileName {
    /// Win32 / Win32+DOS names first, POSIX next, 8.3 DOS names last.
    fn rank(&self) -> u8 {
        match self.namespace {
            1 | 3 => 0,
            0 => 1,
            _ => 2,
        }
    }
}

/// A non-resident attribute fragment (one record's share of the run list).
#[derive(Debug, Clone)]
pub struct NonResident {
    pub start_vcn: u64,
    pub runs: Vec<Run>,
    /// Only meaningful on the fragment with `start_vcn == 0`.
    pub real_size: u64,
    pub compression_unit: u8,
    pub compressed: bool,
    pub encrypted: bool,
}

#[derive(Debug, Clone)]
pub enum Data {
    Resident(Vec<u8>),
    NonResident(NonResident),
}

#[derive(Debug, Clone)]
pub struct AttrListEntry {
    pub attr_type: u32,
    pub start_vcn: u64,
    pub record: u64,
}

/// The parts of a FILE record that recovery cares about.
#[derive(Debug, Clone, Default)]
pub struct FileRecord {
    pub flags: u16,
    pub seq: u16,
    pub base_ref: u64,
    pub file_name: Option<FileName>,
    pub si_created: u64,
    pub si_modified: u64,
    /// Unnamed `$DATA` stream (the file content).
    pub data: Option<Data>,
    /// Named `$DATA` streams (alternate data streams), stored in this record.
    pub streams: Vec<(String, Data)>,
    pub attr_list: Option<Vec<AttrListEntry>>,
    pub has_attr_list: bool,
    pub torn: bool,
}

impl FileRecord {
    pub fn in_use(&self) -> bool {
        self.flags & FLAG_IN_USE != 0
    }

    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_DIRECTORY != 0
    }

    /// Parses a record whose fixups have already been applied.
    /// Returns `None` if it is not a FILE record.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.get(0..4)? != b"FILE" {
            return None;
        }
        let mut rec = FileRecord {
            seq: le16(buf, 0x10)?,
            flags: le16(buf, 0x16)?,
            base_ref: le64(buf, 0x20)?,
            ..Default::default()
        };
        let used = (le32(buf, 0x18)? as usize).min(buf.len());
        let mut off = usize::from(le16(buf, 0x14)?);
        while off + 16 <= used {
            let ty = le32(buf, off)?;
            if ty == ATTR_END {
                break;
            }
            let len = le32(buf, off + 4)? as usize;
            if len < 16 || off + len > used {
                break;
            }
            rec.parse_attr(ty, &buf[off..off + len]);
            off += len;
        }
        Some(rec)
    }

    fn parse_attr(&mut self, ty: u32, a: &[u8]) -> Option<()> {
        let non_resident = u8_at(a, 8)? != 0;
        let name_len = u8_at(a, 9)?;
        let flags = le16(a, 0x0C)?;
        match ty {
            ATTR_STANDARD_INFORMATION if !non_resident => {
                let v = resident_value(a)?;
                self.si_created = le64(v, 0)?;
                self.si_modified = le64(v, 8)?;
            }
            ATTR_FILE_NAME if !non_resident => {
                let v = resident_value(a)?;
                let n = usize::from(u8_at(v, 0x40)?);
                let fname = FileName {
                    parent: le64(v, 0)?,
                    created: le64(v, 0x08)?,
                    modified: le64(v, 0x10)?,
                    real_size: le64(v, 0x30)?,
                    namespace: u8_at(v, 0x41)?,
                    name: utf16le(v.get(0x42..0x42 + n * 2)?),
                };
                if self.file_name.as_ref().is_none_or(|cur| fname.rank() < cur.rank()) {
                    self.file_name = Some(fname);
                }
            }
            ATTR_ATTRIBUTE_LIST => {
                self.has_attr_list = true;
                if !non_resident {
                    self.attr_list = Some(parse_attr_list(resident_value(a)?));
                }
            }
            ATTR_DATA => {
                let data = if non_resident {
                    let runlist_off = usize::from(le16(a, 0x20)?);
                    Data::NonResident(NonResident {
                        start_vcn: le64(a, 0x10)?,
                        runs: decode_runlist(a.get(runlist_off..)?)?,
                        real_size: le64(a, 0x30)?,
                        compression_unit: u8_at(a, 0x22)?,
                        compressed: flags & ATTR_FLAG_COMPRESSED != 0,
                        encrypted: flags & ATTR_FLAG_ENCRYPTED != 0,
                    })
                } else {
                    Data::Resident(resident_value(a)?.to_vec())
                };
                if name_len == 0 {
                    if self.data.is_none() {
                        self.data = Some(data);
                    }
                } else {
                    let off = usize::from(le16(a, 0x0A)?);
                    let name = utf16le(a.get(off..off + usize::from(name_len) * 2)?);
                    self.streams.push((name, data));
                }
            }
            _ => {}
        }
        Some(())
    }
}

fn resident_value(a: &[u8]) -> Option<&[u8]> {
    let len = le32(a, 0x10)? as usize;
    let off = usize::from(le16(a, 0x14)?);
    a.get(off..off.checked_add(len)?)
}

pub fn parse_attr_list(v: &[u8]) -> Vec<AttrListEntry> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 26 <= v.len() {
        let (Some(ty), Some(len)) = (le32(v, off), le16(v, off + 4)) else { break };
        let len = usize::from(len);
        if len < 26 {
            break;
        }
        if let (Some(vcn), Some(r)) = (le64(v, off + 8), le64(v, off + 16)) {
            out.push(AttrListEntry { attr_type: ty, start_vcn: vcn, record: r });
        }
        off += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runlist_decoding() {
        // 0x21: 1-byte length, 2-byte offset. Second run negative delta.
        // Third run sparse.
        let b = [0x21, 0x10, 0x00, 0x01, 0x11, 0x08, 0xF0, 0x01, 0x04, 0x00];
        let runs = decode_runlist(&b).unwrap();
        assert_eq!(
            runs,
            vec![
                Run { lcn: Some(0x100), len: 0x10 },
                Run { lcn: Some(0x100 - 0x10), len: 0x08 },
                Run { lcn: None, len: 4 },
            ]
        );
        assert!(decode_runlist(&[0x21, 0x10]).is_none());
        assert!(decode_runlist(&[0x11, 0x01, 0x80]).is_none(), "negative LCN");
    }

    #[test]
    fn fixups_restore_and_detect_tears() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        rec[6..8].copy_from_slice(&3u16.to_le_bytes());
        rec[0x30..0x32].copy_from_slice(&[0xAB, 0xCD]);
        rec[0x32..0x34].copy_from_slice(&[1, 2]);
        rec[0x34..0x36].copy_from_slice(&[3, 4]);
        rec[510..512].copy_from_slice(&[0xAB, 0xCD]);
        rec[1022..1024].copy_from_slice(&[0xAB, 0xCD]);
        let mut good = rec.clone();
        assert!(apply_fixups(&mut good));
        assert_eq!(&good[510..512], &[1, 2]);
        assert_eq!(&good[1022..1024], &[3, 4]);

        rec[1022] = 0;
        assert!(!apply_fixups(&mut rec));
    }
}
