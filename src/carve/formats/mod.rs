//! Built-in carvable formats.

mod archive;
mod audio;
mod document;
mod image;
mod video;

use super::Format;

pub static ALL: &[&dyn Format] = &[
    &image::Jpeg,
    &image::Png,
    &image::Gif,
    &image::Bmp,
    &image::Tiff,
    &video::Bmff,
    &video::Matroska,
    &video::Riff,
    &video::Asf,
    &video::TransportStream,
    &audio::Mp3,
    &audio::Ogg,
    &document::Pdf,
    &document::Ole,
    &archive::Zip,
    &archive::SevenZip,
    &archive::Rar,
    &archive::Sqlite,
];

const fn crc32_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC32: [u32; 256] = crc32_table();

/// CRC-32 (IEEE 802.3), as used by ZIP, 7z, RAR and PNG.
pub fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, &b| CRC32[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8))
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32_check_value() {
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
    }
}
