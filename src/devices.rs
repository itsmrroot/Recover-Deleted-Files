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
    /// File system as the OS reports it (e.g. "APFS"), when known.
    pub fs: Option<String>,
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
    Device { path, size, description, label, fs: None, kind }
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
    macos::list().unwrap_or_else(macos::scan_dev)
}

/// macOS: drives as `diskutil` describes them, so they get their real names
/// ("Macintosh HD", a USB stick's label) instead of bare `/dev/diskNsM` nodes.
#[cfg(target_os = "macos")]
mod macos {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use serde_json::Value;

    use super::{Device, DeviceKind, natural_key, probe};

    /// Partitions that only hold macOS internals (boot loader, recovery
    /// system), never the user's files.
    const SYSTEM_PARTITIONS: &[&str] =
        &["EFI", "Apple_APFS_ISC", "Apple_APFS_Recovery", "Apple_Boot", "Apple_KernelCoreDump"];

    /// Runs `diskutil <verb> -plist [device]` and returns its output as JSON.
    fn diskutil(verb: &str, device: Option<&str>) -> Option<Value> {
        let out = Command::new("diskutil").args([verb, "-plist"]).args(device).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let mut plutil = Command::new("plutil")
            .args(["-convert", "json", "-o", "-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .ok()?;
        plutil.stdin.take()?.write_all(&out.stdout).ok()?;
        let json = plutil.wait_with_output().ok()?;
        serde_json::from_slice(&json.stdout).ok()
    }

    fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
        v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
    }

    /// A readable file-system name for a partition type.
    fn fs_name(content: &str) -> &str {
        match content {
            "Apple_APFS" | "Apple_APFS_Container" => "APFS",
            "Apple_HFS" | "Apple_HFSX" => "Mac OS Extended",
            "DOS_FAT_12" | "DOS_FAT_16" | "DOS_FAT_32" => "FAT",
            "Windows_NTFS" => "NTFS",
            "Microsoft Basic Data" | "Windows_FAT_32" => "Windows",
            other => other,
        }
    }

    pub fn list() -> Option<Vec<Device>> {
        let all = diskutil("list", None)?;
        let disks = all.get("AllDisksAndPartitions")?.as_array()?;
        // APFS containers are virtual disks built on a partition: name that
        // partition after the container's main volume ("Macintosh HD").
        let mut apfs_names: Vec<(String, String)> = Vec::new();
        for d in disks {
            let (Some(stores), Some(volumes)) =
                (d.get("APFSPhysicalStores").and_then(Value::as_array), d.get("APFSVolumes").and_then(Value::as_array))
            else {
                continue;
            };
            let Some(name) = volumes.iter().filter_map(|v| text(v, "VolumeName")).next() else { continue };
            for s in stores {
                if let Some(id) = text(s, "DeviceIdentifier") {
                    apfs_names.push((id.to_string(), name.to_string()));
                }
            }
        }

        let mut out = Vec::new();
        for d in disks {
            let Some(id) = text(d, "DeviceIdentifier") else { continue };
            // The virtual APFS disks are reached through their partition.
            if d.get("APFSPhysicalStores").is_some() || text(d, "Content") == Some("Apple_APFS_Container") {
                continue;
            }
            let info = diskutil("info", Some(id));
            let info = info.as_ref();
            let internal = info.and_then(|i| i.get("Internal")).and_then(Value::as_bool).unwrap_or(true);
            let removable = !internal
                || info.and_then(|i| i.get("RemovableMediaOrExternalDevice")).and_then(Value::as_bool).unwrap_or(false);
            let where_ = if internal { "internal" } else { "external" };
            let media = info.and_then(|i| text(i, "MediaName")).map(str::to_string);
            let partitions = d.get("Partitions").and_then(Value::as_array).cloned().unwrap_or_default();

            if partitions.is_empty() {
                // A card or stick formatted without a partition table.
                let label = text(d, "VolumeName").map(str::to_string).or(media);
                let fs = text(d, "Content").map(fs_name).unwrap_or("disk");
                let kind = if removable { DeviceKind::Removable } else { DeviceKind::Volume };
                let mut dev = probe(format!("/dev/r{id}"), format!("{fs}, {where_}"), kind, label);
                dev.fs = Some(fs.to_string());
                out.push(dev);
                continue;
            }
            out.push(probe(format!("/dev/r{id}"), format!("whole disk, {where_}"), DeviceKind::Disk, media));
            for p in &partitions {
                let Some(pid) = text(p, "DeviceIdentifier") else { continue };
                let content = text(p, "Content").unwrap_or("");
                if SYSTEM_PARTITIONS.contains(&content) {
                    continue;
                }
                let label = text(p, "VolumeName")
                    .map(str::to_string)
                    .or_else(|| apfs_names.iter().find(|(s, _)| s == pid).map(|(_, n)| n.clone()));
                let kind = if removable { DeviceKind::Removable } else { DeviceKind::Volume };
                let fs = fs_name(content);
                let mut dev = probe(format!("/dev/r{pid}"), format!("{fs}, {where_}"), kind, label);
                dev.fs = Some(fs.to_string());
                out.push(dev);
            }
        }
        Some(out)
    }

    /// Fallback when `diskutil` is unavailable: every disk node in /dev.
    pub fn scan_dev() -> Vec<Device> {
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
                let (desc, kind) =
                    if whole { ("whole disk", DeviceKind::Disk) } else { ("partition", DeviceKind::Volume) };
                probe(format!("/dev/r{n}"), desc.into(), kind, None)
            })
            .collect()
    }
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
