//! Enumerates disks and volumes that can be used as a recovery source.

use crate::source::{DiskSource, ReadAt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// A mounted volume / partition (e.g. `E:`).
    Volume,
    /// A removable volume (USB stick, memory card).
    Removable,
    /// A whole physical disk.
    Disk,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub path: String,
    /// `None` when the device cannot be opened (usually missing rights).
    pub size: Option<u64>,
    pub description: String,
    /// Volume label, when the OS knows one.
    pub label: Option<String>,
    pub kind: DeviceKind,
}

impl Device {
    /// Name as a person would say it: "USB Drive (E:)", "Local Disk (C:)".
    pub fn display_name(&self) -> String {
        #[cfg(windows)]
        if let Some(letter) = self.path.strip_prefix(r"\\.\").and_then(|s| s.strip_suffix(':')) {
            let name = self.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| {
                if self.kind == DeviceKind::Removable { "USB Drive".into() } else { "Local Disk".into() }
            });
            return format!("{name} ({letter}:)");
        }
        match &self.label {
            Some(l) if !l.is_empty() => format!("{l} ({})", self.path),
            _ => self.path.clone(),
        }
    }
}

fn probe(path: String, description: String, kind: DeviceKind, label: Option<String>) -> Device {
    let size = DiskSource::open(&path).ok().map(|d| d.size());
    Device { path, size, description, label, kind }
}

#[cfg(windows)]
pub fn list() -> Vec<Device> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
    // <winbase.h> drive types.
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_RAMDISK: u32 = 6;

    let mut out = Vec::new();
    // SAFETY: no arguments, returns a bitmask.
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = char::from(b'A' + i as u8);
        let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        // SAFETY: `root` is a NUL-terminated wide string.
        let ty = unsafe { GetDriveTypeW(root.as_ptr()) };
        // Network shares and optical drives cannot be read as raw volumes.
        if !matches!(ty, DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_RAMDISK) {
            continue;
        }
        let mut name = [0u16; 261];
        // SAFETY: the buffer length matches; the other outputs are optional.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        let label = (ok != 0).then(|| {
            let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            String::from_utf16_lossy(&name[..n])
        });
        let kind = if ty == DRIVE_REMOVABLE { DeviceKind::Removable } else { DeviceKind::Volume };
        let desc = if kind == DeviceKind::Removable { "removable volume" } else { "volume" };
        out.push(probe(format!(r"\\.\{letter}:"), format!("{desc} {letter}:"), kind, label));
    }
    for n in 0..32 {
        let path = format!(r"\\.\PhysicalDrive{n}");
        let d = probe(path, format!("physical disk {n}"), DeviceKind::Disk, Some(format!("Disk {n}")));
        if d.size.is_some() {
            out.push(d);
        }
    }
    out
}

#[cfg(target_os = "macos")]
pub fn list() -> Vec<Device> {
    let mut names: Vec<String> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("disk") && n[4..].chars().next().is_some_and(|c| c.is_ascii_digit()))
        .collect();
    names.sort_by_key(|n| natural_key(n));
    names
        .into_iter()
        .map(|n| {
            let whole = !n[4..].contains('s');
            // The raw (r) node bypasses the buffer cache and is much faster.
            let (desc, kind) = if whole { ("whole disk", DeviceKind::Disk) } else { ("partition", DeviceKind::Volume) };
            probe(format!("/dev/r{n}"), desc.into(), kind, None)
        })
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn list() -> Vec<Device> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/sys/class/block").into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
            continue;
        }
        let partition = e.path().join("partition").exists();
        let removable = std::fs::read_to_string(e.path().join("removable")).is_ok_and(|v| v.trim() == "1");
        let (desc, kind) = if partition {
            ("partition", DeviceKind::Volume)
        } else if removable {
            ("removable disk", DeviceKind::Removable)
        } else {
            ("whole disk", DeviceKind::Disk)
        };
        out.push(probe(format!("/dev/{name}"), desc.into(), kind, None));
    }
    out.sort_by_key(|d| natural_key(&d.path));
    out
}

/// Sort key that orders `disk10` after `disk2`.
#[allow(dead_code)]
fn natural_key(s: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            if !num.is_empty() {
                out.push((std::mem::take(&mut text), num.parse().unwrap_or(0)));
                num.clear();
            }
            text.push(c);
        }
    }
    out.push((text, num.parse().unwrap_or(0)));
    out
}
