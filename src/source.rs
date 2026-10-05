//! Read-only access to the thing being recovered from: a raw device
//! (`\\.\C:`, `\\.\PhysicalDrive1`, `/dev/disk4`, `/dev/sdb`) or an image file.
//!
//! Design notes:
//! * Sources are **never opened for writing**. Recovering onto, or writing
//!   to, the damaged medium is the most common way people destroy the data
//!   they are trying to save.
//! * Raw devices only accept sector-aligned reads on Windows and macOS, so
//!   [`DiskSource`] transparently widens unaligned requests.
//! * Failing media is the norm in recovery work. [`read_tolerant`] retries a
//!   failed read sector by sector and zero-fills what cannot be read instead
//!   of aborting the whole job.

use std::fs::File;
use std::io;
use std::sync::Arc;

use anyhow::{Context, Result};

/// Smallest unit we retry individually when a read fails.
pub const SECTOR: usize = 512;

/// Alignment used for raw device I/O. 4096 is a multiple of every sector
/// size in practical use (512e, 4Kn), so it is always safe.
const DEVICE_ALIGN: u64 = 4096;

/// Positional, thread-safe, read-only byte source.
pub trait ReadAt: Send + Sync {
    /// Reads up to `buf.len()` bytes at `offset`; returns 0 at end of source.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    /// Total size in bytes.
    fn size(&self) -> u64;

    fn read_exact_at(&self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            match self.read_at(offset, buf) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    offset += n as u64;
                    buf = &mut buf[n..];
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn read_vec(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read_exact_at(offset, &mut v)?;
        Ok(v)
    }
}

pub type Source = Arc<dyn ReadAt>;

/// Fills `buf` from `offset`, never failing. Unreadable sectors and bytes
/// past the end of the source are zero-filled. Returns the number of bytes
/// that could not be read.
pub fn read_tolerant(src: &dyn ReadAt, offset: u64, buf: &mut [u8]) -> u64 {
    if src.read_exact_at(offset, buf).is_ok() {
        return 0;
    }
    let mut bad = 0u64;
    // Retry on sector boundaries so a single bad sector costs 512 bytes,
    // not the whole request.
    let mut pos = 0usize;
    while pos < buf.len() {
        let abs = offset + pos as u64;
        let to_boundary = SECTOR - (abs % SECTOR as u64) as usize;
        let n = to_boundary.min(buf.len() - pos);
        let chunk = &mut buf[pos..pos + n];
        // Keep whatever part of the sector is readable (the source may end
        // mid-sector); zero the rest.
        let mut got = 0usize;
        while got < n {
            match src.read_at(abs + got as u64, &mut chunk[got..]) {
                Ok(0) | Err(_) => break,
                Ok(k) => got += k,
            }
        }
        chunk[got..].fill(0);
        bad += (n - got) as u64;
        pos += n;
    }
    if bad > 0 {
        log::debug!("{bad} unreadable bytes near offset {offset:#x}");
    }
    bad
}

/// A raw device or image file.
pub struct DiskSource {
    file: File,
    size: u64,
    align: u64,
    path: String,
}

impl DiskSource {
    /// Opens `path` read-only. On Windows a bare drive letter (`E:` or `E`)
    /// is translated to the volume device `\\.\E:`.
    pub fn open(path: &str) -> Result<Self> {
        let path = normalize_device_path(path);
        let file = platform::open_readonly(&path).map_err(|e| {
            let hint = match e.kind() {
                std::io::ErrorKind::NotFound => " (no such drive or file)",
                std::io::ErrorKind::PermissionDenied => {
                    if cfg!(windows) {
                        " (run as Administrator to read drives)"
                    } else {
                        " (run with sudo to read drives)"
                    }
                }
                _ => "",
            };
            anyhow::Error::new(e).context(format!("cannot open {path}{hint}"))
        })?;
        let is_device = platform::is_device(&path, &file);
        let size = if is_device {
            platform::device_size(&file).with_context(|| format!("cannot determine size of {path}"))?
        } else {
            file.metadata()?.len()
        };
        anyhow::ensure!(size > 0, "{path} is empty");
        Ok(Self { file, size, align: if is_device { DEVICE_ALIGN } else { 1 }, path })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn is_device(&self) -> bool {
        self.align > 1
    }

    fn read_full_raw(&self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            match platform::pread(&self.file, buf, offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    offset += n as u64;
                    buf = &mut buf[n..];
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl ReadAt for DiskSource {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.size - offset) as usize;
        let end = offset + want as u64;
        let a = self.align;
        // Fast path: already aligned (or a plain file).
        if a == 1 || (offset.is_multiple_of(a) && (end.is_multiple_of(a) || end == self.size)) {
            return platform::pread(&self.file, &mut buf[..want], offset);
        }
        // Widen to alignment. The device size is always a multiple of the
        // physical sector size, so clamping the end to it stays aligned.
        let start = offset / a * a;
        let wide_end = end.div_ceil(a).saturating_mul(a).min(self.size);
        let mut tmp = vec![0u8; (wide_end - start) as usize];
        self.read_full_raw(start, &mut tmp)?;
        let skip = (offset - start) as usize;
        buf[..want].copy_from_slice(&tmp[skip..skip + want]);
        Ok(want)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// A window onto part of another source, e.g. one partition of a disk.
pub struct SubSource {
    inner: Source,
    start: u64,
    len: u64,
}

impl SubSource {
    pub fn new(inner: Source, start: u64, len: u64) -> Self {
        let len = len.min(inner.size().saturating_sub(start));
        Self { inner, start, len }
    }
}

impl ReadAt for SubSource {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(self.len - offset) as usize;
        self.inner.read_at(self.start + offset, &mut buf[..n])
    }

    fn size(&self) -> u64 {
        self.len
    }
}

/// In-memory source; used by tests and handy for small images.
pub struct MemSource(pub Vec<u8>);

impl ReadAt for MemSource {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let Ok(off) = usize::try_from(offset) else { return Ok(0) };
        if off >= self.0.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.0.len() - off);
        buf[..n].copy_from_slice(&self.0[off..off + n]);
        Ok(n)
    }

    fn size(&self) -> u64 {
        self.0.len() as u64
    }
}

/// `E:` / `e` -> `\\.\E:` on Windows; identity elsewhere.
pub fn normalize_device_path(path: &str) -> String {
    if cfg!(windows) {
        let t = path.trim_end_matches(['\\', '/']);
        let b = t.as_bytes();
        let is_letter = |c: u8| c.is_ascii_alphabetic();
        if (b.len() == 1 && is_letter(b[0])) || (b.len() == 2 && is_letter(b[0]) && b[1] == b':') {
            return format!(r"\\.\{}:", (b[0] as char).to_ascii_uppercase());
        }
    }
    path.to_string()
}

#[cfg(unix)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{FileExt, FileTypeExt};

    pub fn open_readonly(path: &str) -> io::Result<File> {
        OpenOptions::new().read(true).open(path)
    }

    pub fn is_device(_path: &str, file: &File) -> bool {
        file.metadata().map(|m| m.file_type().is_block_device() || m.file_type().is_char_device()).unwrap_or(false)
    }

    pub fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        file.read_at(buf, offset)
    }

    #[cfg(target_os = "macos")]
    pub fn device_size(file: &File) -> io::Result<u64> {
        use std::os::fd::AsRawFd;
        // <sys/disk.h>: _IOR('d', 24, uint32_t) and _IOR('d', 25, uint64_t)
        const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
        const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;
        let fd = file.as_raw_fd();
        let mut block_size: u32 = 0;
        let mut block_count: u64 = 0;
        // SAFETY: both ioctls write exactly one integer of the given type.
        unsafe {
            if libc::ioctl(fd, DKIOCGETBLOCKSIZE, &mut block_size) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(fd, DKIOCGETBLOCKCOUNT, &mut block_count) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(u64::from(block_size) * block_count)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn device_size(file: &File) -> io::Result<u64> {
        use std::io::{Seek, SeekFrom};
        // Linux & BSD report block device sizes through seek-to-end.
        let mut f = file;
        f.seek(SeekFrom::End(0))
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::{FileExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};

    pub fn open_readonly(path: &str) -> io::Result<File> {
        // Volumes in use by Windows are opened with full sharing so we can
        // read them while mounted; we never request write access.
        OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE).open(path)
    }

    pub fn is_device(path: &str, _file: &File) -> bool {
        path.starts_with(r"\\.\") || path.starts_with(r"\\?\GLOBALROOT")
    }

    pub fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        file.seek_read(buf, offset)
    }

    pub fn device_size(file: &File) -> io::Result<u64> {
        let mut info = GET_LENGTH_INFORMATION { Length: 0 };
        let mut returned = 0u32;
        // SAFETY: the output buffer is a properly sized GET_LENGTH_INFORMATION.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as _,
                IOCTL_DISK_GET_LENGTH_INFO,
                std::ptr::null(),
                0,
                (&mut info as *mut GET_LENGTH_INFORMATION).cast(),
                std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        u64::try_from(info.Length).map_err(|_| io::Error::other("negative device length"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source with one unreadable sector, to exercise tolerant reads.
    struct Flaky(MemSource, u64);

    impl ReadAt for Flaky {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let end = offset + buf.len() as u64;
            if offset < self.1 + SECTOR as u64 && end > self.1 {
                return Err(io::Error::other("bad sector"));
            }
            self.0.read_at(offset, buf)
        }
        fn size(&self) -> u64 {
            self.0.size()
        }
    }

    #[test]
    fn tolerant_read_zero_fills_only_the_bad_sector() {
        let src = Flaky(MemSource(vec![7u8; 4096]), 1024);
        let mut buf = vec![0xAAu8; 4096];
        assert_eq!(read_tolerant(&src, 0, &mut buf), 512);
        assert!(buf[..1024].iter().all(|&b| b == 7));
        assert!(buf[1024..1536].iter().all(|&b| b == 0));
        assert!(buf[1536..].iter().all(|&b| b == 7));
    }

    #[test]
    fn tolerant_read_past_end_is_zero() {
        let src = MemSource(vec![1u8; 100]);
        let mut buf = vec![9u8; 200];
        let bad = read_tolerant(&src, 0, &mut buf);
        assert_eq!(bad, 100);
        assert!(buf[..100].iter().all(|&b| b == 1));
        assert!(buf[100..].iter().all(|&b| b == 0));
    }

    #[test]
    fn sub_source_is_clamped() {
        let src: Source = Arc::new(MemSource((0..=255u8).collect()));
        let sub = SubSource::new(src, 250, 100);
        assert_eq!(sub.size(), 6);
        let mut b = [0u8; 10];
        assert_eq!(sub.read_at(0, &mut b).unwrap(), 6);
        assert_eq!(&b[..6], &[250, 251, 252, 253, 254, 255]);
    }
}
