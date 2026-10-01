//! Bounds-checked little/big-endian readers for parsing on-disk structures.
//!
//! Everything we parse comes from a possibly damaged disk, so none of these
//! helpers may panic: out-of-range reads return `None`.

#[inline]
pub fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

#[inline]
fn array<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    b.get(off..off.checked_add(N)?)?.try_into().ok()
}

#[inline]
pub fn le16(b: &[u8], off: usize) -> Option<u16> {
    array(b, off).map(u16::from_le_bytes)
}

#[inline]
pub fn le32(b: &[u8], off: usize) -> Option<u32> {
    array(b, off).map(u32::from_le_bytes)
}

#[inline]
pub fn le64(b: &[u8], off: usize) -> Option<u64> {
    array(b, off).map(u64::from_le_bytes)
}

#[inline]
pub fn be16(b: &[u8], off: usize) -> Option<u16> {
    array(b, off).map(u16::from_be_bytes)
}

#[inline]
pub fn be32(b: &[u8], off: usize) -> Option<u32> {
    array(b, off).map(u32::from_be_bytes)
}

#[inline]
pub fn be64(b: &[u8], off: usize) -> Option<u64> {
    array(b, off).map(u64::from_be_bytes)
}

/// Reads an unsigned little-endian integer of 1..=8 bytes.
pub fn le_uint(b: &[u8]) -> Option<u64> {
    if b.is_empty() || b.len() > 8 {
        return None;
    }
    Some(b.iter().rev().fold(0u64, |acc, &x| (acc << 8) | u64::from(x)))
}

/// Reads a sign-extended little-endian integer of 1..=8 bytes.
pub fn le_int(b: &[u8]) -> Option<i64> {
    let v = le_uint(b)?;
    let bits = b.len() * 8;
    if bits == 64 {
        return Some(v as i64);
    }
    let shift = 64 - bits;
    Some(((v << shift) as i64) >> shift)
}

/// Decodes UTF-16LE, stopping at the first NUL. Invalid surrogates are
/// replaced rather than rejected: a slightly mangled file name is still far
/// more useful to the user than none.
pub fn utf16le(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_are_checked() {
        let b = [1u8, 2, 3];
        assert_eq!(le16(&b, 1), Some(0x0302));
        assert_eq!(le16(&b, 2), None);
        assert_eq!(le32(&b, usize::MAX), None);
    }

    #[test]
    fn signed_varints() {
        assert_eq!(le_int(&[0xFF]), Some(-1));
        assert_eq!(le_int(&[0x00, 0x80]), Some(-32768));
        assert_eq!(le_int(&[0x34, 0x12]), Some(0x1234));
        assert_eq!(le_uint(&[0x34, 0x12, 0x00]), Some(0x1234));
        assert_eq!(le_uint(&[]), None);
    }

    #[test]
    fn utf16_stops_at_nul() {
        let b = [b'h', 0, b'i', 0, 0, 0, b'x', 0];
        assert_eq!(utf16le(&b), "hi");
    }
}
