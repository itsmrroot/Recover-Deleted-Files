//! Decompressing file data that a file system stored compressed (btrfs:
//! zlib, LZO, zstd). LZO is decoded here (LZO1X, as in the Linux kernel's
//! `lzo1x_decompress_safe`); zlib and zstd come from libraries.

use std::io::{self, Read};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Compression {
    None,
    Zlib,
    Lzo,
    Zstd,
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("damaged {what} data"))
}

/// Decompresses `data` (one btrfs extent) to at most `max` bytes.
pub fn decompress(method: Compression, data: &[u8], max: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(max.min(1 << 20));
    match method {
        Compression::None => out.extend_from_slice(&data[..data.len().min(max)]),
        Compression::Zlib => {
            flate2::read::ZlibDecoder::new(data).take(max as u64).read_to_end(&mut out)?;
        }
        Compression::Zstd => {
            let mut d = ruzstd::decoding::StreamingDecoder::new(data).map_err(|_| bad("zstd"))?;
            (&mut d).take(max as u64).read_to_end(&mut out)?;
        }
        Compression::Lzo => btrfs_lzo(data, max, &mut out)?,
    }
    Ok(out)
}

/// btrfs's LZO framing: the total length, then segments (length + LZO1X
/// data) of one 4 KiB page each; a segment header never straddles a page.
fn btrfs_lzo(data: &[u8], max: usize, out: &mut Vec<u8>) -> io::Result<()> {
    const PAGE: usize = 4096;
    let le32 = |at: usize| data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let total = le32(0).ok_or_else(|| bad("LZO"))?.min(data.len());
    let mut at = 4usize;
    while at + 4 <= total && out.len() < max {
        if PAGE - at % PAGE < 4 {
            at = at.div_ceil(PAGE) * PAGE;
            continue;
        }
        let len = le32(at).ok_or_else(|| bad("LZO"))?;
        at += 4;
        let Some(seg) = data.get(at..at + len) else { break };
        // Each segment is a whole page, even the last (trimmed below). A
        // damaged one ends the data: what came before it is kept.
        if lzo1x(seg, out, PAGE).is_err() {
            break;
        }
        at += len;
    }
    out.truncate(max);
    Ok(())
}

/// LZO1X: appends at most `limit` decompressed bytes to `out`.
pub fn lzo1x(input: &[u8], out: &mut Vec<u8>, limit: usize) -> io::Result<()> {
    let start = out.len();
    let end = start + limit;
    let mut ip = 0usize;
    let get = |ip: &mut usize| -> io::Result<usize> {
        let b = *input.get(*ip).ok_or_else(|| bad("LZO"))?;
        *ip += 1;
        Ok(usize::from(b))
    };
    let literals = |out: &mut Vec<u8>, ip: &mut usize, n: usize| -> io::Result<()> {
        let s = input.get(*ip..*ip + n).ok_or_else(|| bad("LZO"))?;
        if out.len() + n > end {
            return Err(bad("LZO"));
        }
        out.extend_from_slice(s);
        *ip += n;
        Ok(())
    };
    let copy_match = |out: &mut Vec<u8>, back: usize, n: usize| -> io::Result<()> {
        if back == 0 || back > out.len() - start || out.len() + n > end {
            return Err(bad("LZO"));
        }
        let from = out.len() - back;
        for i in 0..n {
            let b = out[from + i];
            out.push(b);
        }
        Ok(())
    };
    // A run of zero bytes extends a length: each zero adds 255.
    let extend = |ip: &mut usize, base: usize| -> io::Result<usize> {
        let mut t = base;
        while input.get(*ip) == Some(&0) {
            t += 255;
            *ip += 1;
        }
        Ok(t + get(ip)?)
    };

    let mut state;
    let first = *input.first().ok_or_else(|| bad("LZO"))?;
    if first > 17 {
        ip = 1;
        let t = usize::from(first) - 17;
        literals(out, &mut ip, t)?;
        state = if t < 4 { t } else { 4 };
    } else {
        state = 0;
    }
    loop {
        let t = get(&mut ip)?;
        let (back, len, next);
        if t < 16 {
            if state == 0 {
                let n = if t == 0 { extend(&mut ip, 15)? } else { t };
                literals(out, &mut ip, n + 3)?;
                state = 4;
                continue;
            } else if state != 4 {
                next = t & 3;
                back = 1 + (t >> 2) + (get(&mut ip)? << 2);
                len = 2;
            } else {
                next = t & 3;
                back = 1 + 0x0800 + (t >> 2) + (get(&mut ip)? << 2);
                len = 3;
            }
        } else if t >= 64 {
            next = t & 3;
            back = 1 + ((t >> 2) & 7) + (get(&mut ip)? << 3);
            len = (t >> 5) + 1;
        } else if t >= 32 {
            let n = t & 31;
            len = if n == 0 { extend(&mut ip, 31)? } else { n } + 2;
            let d = get(&mut ip)? | (get(&mut ip)? << 8);
            back = 1 + (d >> 2);
            next = d & 3;
        } else {
            let far = (t & 8) << 11;
            let n = t & 7;
            len = if n == 0 { extend(&mut ip, 7)? } else { n } + 2;
            let d = get(&mut ip)? | (get(&mut ip)? << 8);
            let dist = far + (d >> 2);
            if dist == 0 {
                return Ok(()); // end of stream
            }
            back = dist + 0x4000;
            next = d & 3;
        }
        copy_match(out, back, len)?;
        state = next;
        literals(out, &mut ip, next)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lzo1x_literals_and_matches() {
        // "abcabcabcabc": 3 literals (first byte 17 + 3), then a match of 9
        // bytes 3 back (M2: length 9 is too long; M3 code 0b001 with
        // length-2 = 7), then the end marker.
        let data = [20, b'a', b'b', b'c', 0x20 | 7, (2 << 2) as u8, 0, 0x11, 0, 0];
        let mut out = Vec::new();
        lzo1x(&data, &mut out, 64).unwrap();
        assert_eq!(out, b"abcabcabcabc");
    }
}
