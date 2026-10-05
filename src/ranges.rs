//! Byte-range set arithmetic, used to decide which parts of a disk to carve.

use std::ops::Range;

pub type ByteRange = Range<u64>;

/// Sorts, drops empty ranges and merges overlapping/adjacent ones.
pub fn normalize(mut v: Vec<ByteRange>) -> Vec<ByteRange> {
    v.retain(|r| r.start < r.end);
    v.sort_by_key(|r| r.start);
    let mut out: Vec<ByteRange> = Vec::with_capacity(v.len());
    for r in v {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// `base` minus `remove`. Both inputs must be normalized.
pub fn subtract(base: &[ByteRange], remove: &[ByteRange]) -> Vec<ByteRange> {
    let mut out = Vec::new();
    let mut j = 0;
    for r in base {
        let mut cur = r.start;
        while j < remove.len() && remove[j].end <= cur {
            j += 1;
        }
        let mut k = j;
        while k < remove.len() && remove[k].start < r.end {
            if remove[k].start > cur {
                out.push(cur..remove[k].start);
            }
            cur = cur.max(remove[k].end);
            k += 1;
        }
        if cur < r.end {
            out.push(cur..r.end);
        }
    }
    out
}

pub fn total(v: &[ByteRange]) -> u64 {
    v.iter().map(|r| r.end - r.start).sum()
}

/// Runs of clear bits in an allocation bitmap (bit *i* = byte i/8, LSB
/// first — the layout used by NTFS `$Bitmap` and the exFAT allocation
/// bitmap). Only the first `count` bits are considered.
pub fn zero_bit_runs(bitmap: &[u8], count: u64) -> Vec<Range<u64>> {
    let mut runs = Vec::new();
    let mut start: Option<u64> = None;
    let count = count.min(bitmap.len() as u64 * 8);
    let mut i = 0u64;
    while i < count {
        let byte = bitmap[(i / 8) as usize];
        // Whole-byte fast paths when aligned.
        if i.is_multiple_of(8) && i + 8 <= count && (byte == 0x00 || byte == 0xFF) {
            if byte == 0x00 {
                start.get_or_insert(i);
            } else if let Some(s) = start.take() {
                runs.push(s..i);
            }
            i += 8;
            continue;
        }
        if byte >> (i % 8) & 1 == 0 {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            runs.push(s..i);
        }
        i += 1;
    }
    if let Some(s) = start {
        runs.push(s..count);
    }
    runs
}

#[inline]
pub fn bit_is_set(bitmap: &[u8], i: u64) -> Option<bool> {
    bitmap.get((i / 8) as usize).map(|b| b >> (i % 8) & 1 == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_merges() {
        assert_eq!(normalize(vec![5..10, 0..3, 3..4, 8..12, 20..20]), vec![0..4, 5..12]);
    }

    #[test]
    fn subtract_works() {
        let base = vec![0..100, 200..300];
        let rm = vec![10..20, 90..210, 250..260];
        assert_eq!(subtract(&base, &rm), vec![0..10, 20..90, 210..250, 260..300]);
        assert_eq!(subtract(&base, &[]), base);
        let r: Vec<ByteRange> = std::iter::once(0..10).collect();
        assert!(subtract(&r, &r).is_empty());
    }

    #[test]
    fn zero_runs() {
        // bits: 0..8 set except bit 2, then 8..24 clear, then bits 24.. set
        let bm = [0b1111_1011u8, 0x00, 0x00, 0xFF];
        assert_eq!(zero_bit_runs(&bm, 32), vec![2..3, 8..24]);
        assert_eq!(zero_bit_runs(&bm, 12), vec![2..3, 8..12]);
        assert_eq!(bit_is_set(&bm, 2), Some(false));
        assert_eq!(bit_is_set(&bm, 3), Some(true));
    }
}
