//! LZNT1 decompression (NTFS file compression, MS-XCA §2.5).

/// Decompresses `input` (one compression unit) and appends to `out`.
/// Corrupt input is decoded as far as possible; it never panics.
pub fn decompress(input: &[u8], out: &mut Vec<u8>) {
    const CHUNK: usize = 4096;
    let mut i = 0usize;
    while i + 2 <= input.len() {
        let header = u16::from_le_bytes([input[i], input[i + 1]]);
        i += 2;
        if header == 0 {
            break;
        }
        let size = usize::from(header & 0x0FFF) + 1;
        let compressed = header & 0x8000 != 0;
        let end = (i + size).min(input.len());
        let chunk = &input[i..end];
        i = end;

        let chunk_start = out.len();
        if !compressed {
            out.extend_from_slice(chunk);
        } else {
            decompress_chunk(chunk, out, chunk_start);
        }
        // Every chunk but the last represents a full 4 KiB of output.
        if i + 2 <= input.len() && input[i] | input[i + 1] != 0 {
            let produced = out.len() - chunk_start;
            if produced < CHUNK {
                out.resize(chunk_start + CHUNK, 0);
            }
        }
    }
}

fn decompress_chunk(chunk: &[u8], out: &mut Vec<u8>, chunk_start: usize) {
    let mut i = 0usize;
    while i < chunk.len() {
        let flags = chunk[i];
        i += 1;
        for bit in 0..8 {
            if i >= chunk.len() {
                return;
            }
            if flags >> bit & 1 == 0 {
                out.push(chunk[i]);
                i += 1;
                continue;
            }
            if i + 2 > chunk.len() {
                return;
            }
            let token = u16::from_le_bytes([chunk[i], chunk[i + 1]]);
            i += 2;
            let pos = out.len() - chunk_start;
            if pos == 0 {
                return; // back-reference before any data: corrupt
            }
            // The offset/length split depends on the position in the chunk.
            let mut len_bits = 12u32;
            let mut p = pos - 1;
            while p >= 0x10 {
                len_bits -= 1;
                p >>= 1;
            }
            let length = usize::from(token & ((1u16 << len_bits) - 1)) + 3;
            let offset = usize::from(token >> len_bits) + 1;
            if offset > pos {
                return;
            }
            let from = out.len() - offset;
            for k in 0..length {
                let b = out[from + k];
                out.push(b);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_back_reference() {
        let input = [0x05, 0xB0, 0x08, b'a', b'b', b'c', 0x06, 0x20, 0, 0];
        let mut out = Vec::new();
        decompress(&input, &mut out);
        assert_eq!(out, b"abcabcabcabc");
    }

    #[test]
    fn uncompressed_chunk() {
        let mut input = vec![0x03, 0x30];
        input.extend_from_slice(b"wxyz");
        let mut out = Vec::new();
        decompress(&input, &mut out);
        assert_eq!(out, b"wxyz");
    }

    #[test]
    fn corrupt_input_does_not_panic() {
        let input = [0xFF, 0xBF, 0xFF, 0xFF, 0xFF, 0x01];
        let mut out = Vec::new();
        decompress(&input, &mut out);
    }
}
