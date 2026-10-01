//! Enumerates disks and volumes that can be used as a recovery source.

use crate::source::{DiskSource, ReadAt};

pub struct Device {
    pub path: String,
    pub size: Option<u64>,
    pub description: String,
}

fn probe(path: String, description: String) -> Device {
    let size = DiskSource::open(&path).ok().map(|d| d.size());
    Device { path, size, description }
}

#[cfg(windows)]
pub fn list() -> Vec<Device> {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    let mut out = Vec::new();
    // SAFETY: no arguments, returns a bitmask.
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) != 0 {
            let letter = char::from(b'A' + i as u8);
            out.push(probe(format!(r"\\.\{letter}:"), format!("volume {letter}:")));
        }
    }
    for n in 0..32 {
        let path = format!(r"\\.\PhysicalDrive{n}");
        let d = probe(path, format!("physical disk {n}"));
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
            probe(format!("/dev/r{n}"), if whole { "whole disk".into() } else { "partition".into() })
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
        out.push(probe(format!("/dev/{name}"), if partition { "partition".into() } else { "whole disk".into() }));
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
