//! Cached random-access reader used by format parsers.

use memchr::memmem;

use crate::bytes;
use crate::source::{ReadAt, read_tolerant};

const WINDOW: usize = 1 << 20;
/// First refill size. Most candidates are rejected within a few KiB, so we
/// start small and double up to [`WINDOW`] for files that turn out real.
const INITIAL_WINDOW: usize = 64 << 10;

/// A view of the source starting at a candidate offset (position 0) and
/// limited to the format's maximum size. Reads are served from a cache
/// window of up to 1 MiB. Unreadable sectors read as zeros.
pub struct Reader<'a> {
    src: &'a dyn ReadAt,
    base: u64,
    limit: u64,
    buf: Vec<u8>,
    buf_pos: u64,
    window: usize,
}

impl<'a> Reader<'a> {
    pub fn new(src: &'a dyn ReadAt, base: u64, limit: u64) -> Self {
        let limit = limit.min(src.size().saturating_sub(base));
        Self { src, base, limit, buf: Vec::new(), buf_pos: 0, window: INITIAL_WINDOW }
    }

    /// Bytes available from the origin (format max size or source end).
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Exactly `len` bytes at `pos` (len <= 1 MiB), or `None` if that would
    /// cross the limit.
    pub fn bytes(&mut self, pos: u64, len: usize) -> Option<&[u8]> {
        debug_assert!(len <= WINDOW);
        let end = pos.checked_add(len as u64)?;
        if end > self.limit || len > WINDOW {
            return None;
        }
        let cached_end = self.buf_pos + self.buf.len() as u64;
        if pos < self.buf_pos || end > cached_end {
            let n = (self.limit - pos).min(self.window.max(len) as u64) as usize;
            self.window = (self.window * 2).min(WINDOW);
            self.buf.resize(n, 0);
            read_tolerant(self.src, self.base + pos, &mut self.buf);
            self.buf_pos = pos;
        }
        let s = (pos - self.buf_pos) as usize;
        Some(&self.buf[s..s + len])
    }

    pub fn u8(&mut self, pos: u64) -> Option<u8> {
        self.bytes(pos, 1).map(|b| b[0])
    }
    pub fn le16(&mut self, pos: u64) -> Option<u16> {
        bytes::le16(self.bytes(pos, 2)?, 0)
    }
    pub fn le32(&mut self, pos: u64) -> Option<u32> {
        bytes::le32(self.bytes(pos, 4)?, 0)
    }
    pub fn le64(&mut self, pos: u64) -> Option<u64> {
        bytes::le64(self.bytes(pos, 8)?, 0)
    }
    pub fn be16(&mut self, pos: u64) -> Option<u16> {
        bytes::be16(self.bytes(pos, 2)?, 0)
    }
    pub fn be32(&mut self, pos: u64) -> Option<u32> {
        bytes::be32(self.bytes(pos, 4)?, 0)
    }
    pub fn be64(&mut self, pos: u64) -> Option<u64> {
        bytes::be64(self.bytes(pos, 8)?, 0)
    }

    pub fn starts_with(&mut self, pos: u64, needle: &[u8]) -> bool {
        self.bytes(pos, needle.len()) == Some(needle)
    }

    /// Position of the first `needle` in `[from, to)`.
    pub fn find(&mut self, from: u64, needle: &[u8], to: u64) -> Option<u64> {
        let to = to.min(self.limit);
        let finder = memmem::Finder::new(needle);
        let mut cur = from;
        while cur < to && to - cur >= needle.len() as u64 {
            let len = (to - cur).min(WINDOW as u64) as usize;
            let chunk = self.bytes(cur, len)?;
            if let Some(i) = finder.find(chunk) {
                return Some(cur + i as u64);
            }
            if cur + len as u64 >= to {
                break;
            }
            // Overlap so a needle straddling the boundary is not missed.
            cur += (len - (needle.len() - 1)).max(1) as u64;
        }
        None
    }

    /// Position of the first byte equal to `b` in `[from, to)`.
    pub fn find_byte(&mut self, from: u64, b: u8, to: u64) -> Option<u64> {
        let to = to.min(self.limit);
        let mut cur = from;
        while cur < to {
            let len = (to - cur).min(WINDOW as u64) as usize;
            let chunk = self.bytes(cur, len)?;
            if let Some(i) = memchr::memchr(b, chunk) {
                return Some(cur + i as u64);
            }
            cur += len as u64;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemSource;

    #[test]
    fn find_across_window_boundary() {
        let mut data = vec![0u8; WINDOW * 2 + 100];
        data[WINDOW - 2..WINDOW + 2].copy_from_slice(b"ABCD");
        let src = MemSource(data);
        let mut r = Reader::new(&src, 0, u64::MAX);
        assert_eq!(r.find(0, b"ABCD", u64::MAX), Some(WINDOW as u64 - 2));
        assert_eq!(r.find(WINDOW as u64, b"ABCD", u64::MAX), None);
    }

    #[test]
    fn respects_limit() {
        let src = MemSource(vec![1u8; 100]);
        let mut r = Reader::new(&src, 10, 50);
        assert_eq!(r.limit(), 50);
        assert!(r.bytes(45, 5).is_some());
        assert!(r.bytes(46, 5).is_none());
    }
}
