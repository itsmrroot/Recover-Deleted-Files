//! Windows shadow copies ("Previous Versions", System Restore points).
//!
//! Windows keeps read-only snapshots of a drive. A file deleted since a
//! snapshot was taken is still complete in it — whatever has happened to
//! its clusters on the drive since. Each shadow copy is opened as a volume
//! of its own (`\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN`); its files
//! that the drive no longer has are the ones deleted since.

use chrono::NaiveDateTime;
use serde::Deserialize;

/// One shadow copy of a drive.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowCopy {
    /// The device to open it with.
    pub device: String,
    pub created: Option<NaiveDateTime>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(rename = "DeviceObject")]
    device: Option<String>,
    #[serde(rename = "Created")]
    created: Option<String>,
}

/// Parses the JSON that [`QUERY`] prints: one object, or an array of them.
pub fn parse(json: &str) -> Vec<ShadowCopy> {
    let json = json.trim();
    if json.is_empty() {
        return Vec::new();
    }
    let items: Vec<Item> = if json.starts_with('[') {
        serde_json::from_str(json).unwrap_or_default()
    } else {
        serde_json::from_str(json).map(|i| vec![i]).unwrap_or_default()
    };
    let mut out: Vec<ShadowCopy> = items
        .into_iter()
        .filter_map(|i| {
            let device = i.device.filter(|d| d.contains("HarddiskVolumeShadowCopy"))?;
            let created = i.created.as_deref().and_then(|c| {
                // "2026-10-01T12:00:00.0000000+02:00" (round-trip format)
                chrono::DateTime::parse_from_rfc3339(c).ok().map(|d| d.naive_local())
            });
            Some(ShadowCopy { device, created })
        })
        .collect();
    // Newest first.
    out.sort_by_key(|a| std::cmp::Reverse(a.created));
    out
}

/// The drive letter of a volume path (`\\.\C:`, `C:`), upper case.
pub fn drive_letter(source: &str) -> Option<char> {
    let s = source.trim_start_matches(r"\\.\").trim_start_matches(r"\\?\").trim_end_matches(['\\', '/']);
    let mut c = s.chars();
    match (c.next(), c.next(), c.next()) {
        (Some(l), Some(':'), None) if l.is_ascii_alphabetic() => Some(l.to_ascii_uppercase()),
        (Some(l), None, None) if l.is_ascii_alphabetic() => Some(l.to_ascii_uppercase()),
        _ => None,
    }
}

/// PowerShell that lists the shadow copies of drive `{L}:` as JSON, in a
/// form that does not depend on the language of Windows.
pub const QUERY: &str = "$v = Get-CimInstance Win32_Volume | Where-Object { $_.DriveLetter -eq '{L}:' }; \
     if ($v) { Get-CimInstance Win32_ShadowCopy | Where-Object { $_.VolumeName -eq $v.DeviceID } | \
     Select-Object DeviceObject, @{n='Created';e={$_.InstallDate.ToString('o')}} | ConvertTo-Json -Compress }";

/// The shadow copies of the drive `source` is (Windows only; needs
/// administrator rights, which the app has there).
pub fn list(source: &str) -> Vec<ShadowCopy> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let Some(letter) = drive_letter(source) else { return Vec::new() };
        let script = QUERY.replace("{L}", &letter.to_string());
        let out = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(o) if o.status.success() => parse(&String::from_utf8_lossy(&o.stdout)),
            Ok(o) => {
                log::info!("listing shadow copies failed: {}", String::from_utf8_lossy(&o.stderr));
                Vec::new()
            }
            Err(e) => {
                log::info!("listing shadow copies failed: {e}");
                Vec::new()
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = source;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_copies_are_read_from_powershell() {
        let one = r#"{"DeviceObject":"\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy3","Created":"2026-10-01T12:00:00.0000000+02:00"}"#;
        let v = parse(one);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].device, r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3");
        assert_eq!(v[0].created.unwrap().to_string(), "2026-10-01 12:00:00");
        let many = r#"[{"DeviceObject":"\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy1","Created":"2026-09-01T08:00:00.0000000+00:00"},
                       {"DeviceObject":"\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy2","Created":"2026-09-20T08:00:00.0000000+00:00"}]"#;
        let v = parse(many);
        assert_eq!(v.len(), 2);
        assert!(v[0].device.ends_with("Copy2"), "newest first");
        assert!(parse("").is_empty());
        assert!(parse("garbage").is_empty());
    }

    #[test]
    fn drive_letters() {
        assert_eq!(drive_letter(r"\\.\C:"), Some('C'));
        assert_eq!(drive_letter("d:"), Some('D'));
        assert_eq!(drive_letter(r"\\.\PhysicalDrive1"), None);
        assert_eq!(drive_letter("/dev/disk4"), None);
    }
}
