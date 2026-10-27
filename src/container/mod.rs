//! Disk image files that are not a plain copy of the disk: virtual machine
//! disks (VHD, VHDX, VMDK, VDI, QCOW2) and forensic images (E01). Each is
//! read through a [`ReadAt`] that shows the disk as it was, so everything
//! else works on it unchanged. Never-written parts of a sparse image read as
//! zeros.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::source::{DiskSource, ReadAt, Source};

mod ewf;
mod qcow2;
mod vdi;
mod vhd;
mod vhdx;
mod vmdk;

/// What kind of file `path` is, if it is one of these formats.
pub fn kind(path: &str) -> Option<&'static str> {
    let file = DiskSource::open(path).ok()?;
    if file.is_device() {
        return None;
    }
    detect(&file, Path::new(path))
}

fn detect(file: &DiskSource, path: &Path) -> Option<&'static str> {
    let mut head = [0u8; 512];
    let n = file.read_at(0, &mut head).ok()?;
    let head = &head[..n];
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if head.starts_with(b"vhdxfile") {
        return Some("VHDX");
    }
    if head.starts_with(b"KDMV") || head.starts_with(b"# Disk DescriptorFile") || (ext == "vmdk" && descriptor(head)) {
        return Some("VMDK");
    }
    if head.starts_with(b"QFI\xfb") {
        return Some("QCOW2");
    }
    if head.starts_with(ewf::SIGNATURE) {
        return Some("E01");
    }
    if head.len() >= 0x44 && crate::bytes::le32(head, 0x40) == Some(vdi::SIGNATURE) {
        return Some("VDI");
    }
    // VHD keeps its footer at the end (and a copy at the start when dynamic).
    let size = file.size();
    let mut foot = [0u8; 512];
    if head.starts_with(b"conectix")
        || (size >= 512 && file.read_at(size - 512, &mut foot).is_ok() && foot.starts_with(b"conectix"))
    {
        return Some("VHD");
    }
    None
}

fn descriptor(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head);
    text.contains("createType") || text.contains("RW ") && text.contains("FLAT")
}

/// Opens `path`: through its format if it is one of these image files, as
/// it is otherwise (a drive, or a plain image).
pub fn open(path: &str) -> Result<(Source, String)> {
    let file = DiskSource::open(path)?;
    let shown = file.path().to_string();
    if file.is_device() {
        return Ok((Arc::new(file), shown));
    }
    let p = PathBuf::from(&shown);
    let Some(kind) = detect(&file, &p) else { return Ok((Arc::new(file), shown)) };
    let src: Source = match kind {
        "VHD" => Arc::new(vhd::Vhd::open(file, &p)?),
        "VHDX" => Arc::new(vhdx::Vhdx::open(file)?),
        "VMDK" => vmdk::open(file, &p)?,
        "QCOW2" => Arc::new(qcow2::Qcow2::open(file)?),
        "VDI" => Arc::new(vdi::Vdi::open(file)?),
        "E01" => Arc::new(ewf::Ewf::open(&p)?),
        _ => unreachable!(),
    };
    log::info!("{shown}: {kind} image, {} bytes", src.size());
    Ok((src, shown))
}

/// Opens a file next to `base` (a parent disk, an extent, a segment).
fn sibling(base: &Path, name: &str) -> Result<DiskSource> {
    let name = name.trim().trim_matches('"');
    let candidate = Path::new(name);
    let path = if candidate.is_absolute() && candidate.exists() {
        candidate.to_path_buf()
    } else {
        // Only the file name: the image may have been moved with its parts.
        let file = Path::new(&name.replace('\\', "/")).file_name().map(PathBuf::from).unwrap_or_default();
        base.parent().unwrap_or(Path::new(".")).join(file)
    };
    DiskSource::open(&path.to_string_lossy()).with_context(|| format!("{name} (needed by {})", base.display()))
}

/// A small cache of decompressed blocks, so that reading a compressed block
/// sector by sector does not decompress it every time.
struct BlockCache {
    blocks: Mutex<Vec<(u64, Arc<Vec<u8>>)>>,
    capacity: usize,
}

impl BlockCache {
    fn new(capacity: usize) -> Self {
        Self { blocks: Mutex::new(Vec::new()), capacity }
    }

    fn get(&self, key: u64, load: impl FnOnce() -> io::Result<Vec<u8>>) -> io::Result<Arc<Vec<u8>>> {
        if let Some(hit) = self.blocks.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == key) {
            return Ok(hit.1.clone());
        }
        let block = Arc::new(load()?);
        let mut blocks = self.blocks.lock().unwrap_or_else(|e| e.into_inner());
        if blocks.len() >= self.capacity {
            blocks.remove(0);
        }
        blocks.push((key, block.clone()));
        Ok(block)
    }
}

/// The disk of a block-mapped image, read block by block: `locate` says
/// where each piece of a block is.
trait Blocks: Send + Sync {
    fn disk_size(&self) -> u64;
    fn block_size(&self) -> u64;
    /// Fills `buf` from `offset` within block `index` (never past its end).
    fn read_in_block(&self, index: u64, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

fn read_blocks(b: &dyn Blocks, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let size = b.disk_size();
    if offset >= size || buf.is_empty() {
        return Ok(0);
    }
    let want = (buf.len() as u64).min(size - offset) as usize;
    let bs = b.block_size();
    let mut done = 0usize;
    while done < want {
        let pos = offset + done as u64;
        let (index, within) = (pos / bs, pos % bs);
        let n = ((bs - within) as usize).min(want - done);
        b.read_in_block(index, within, &mut buf[done..done + n])?;
        done += n;
    }
    Ok(want)
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// zlib (RFC 1950) or raw deflate (RFC 1951) data, decompressed to `len`
/// bytes at most.
fn inflate(data: &[u8], zlib_header: bool, len: usize) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::with_capacity(len);
    let r: Box<dyn Read> = if zlib_header {
        Box::new(flate2::read::ZlibDecoder::new(data))
    } else {
        Box::new(flate2::read::DeflateDecoder::new(data))
    };
    r.take(len as u64).read_to_end(&mut out)?;
    Ok(out)
}

/// CRC-32C (Castagnoli), used by VHDX and btrfs.
pub fn crc32c(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0x82F6_3B78 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8))
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32c_check_value() {
        assert_eq!(super::crc32c(b"123456789"), 0xE306_9283);
    }
}
