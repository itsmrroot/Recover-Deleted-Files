//! Drive health, as the drive itself reports it (S.M.A.R.T., or the NVMe
//! health log). A drive that reports problems should be copied to an image
//! once, before anything else reads it (see [`crate::imaging`]).
//!
//! * macOS: `diskutil info` (its SMART status, and the NVMe health values).
//! * Windows: the disk driver's failure prediction, and the NVMe health log.
//! * Linux: `smartctl` when it is installed; the NVMe health log otherwise.
//!
//! USB enclosures and card readers usually pass none of this on: then the
//! health is unknown, and read errors during a scan are the only sign.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Health {
    #[default]
    Unknown,
    Good,
    /// Worn, or has had errors: copy it soon.
    Warning(String),
    /// The drive predicts its own failure: copy it now.
    Failing(String),
}

impl Health {
    pub fn is_bad(&self) -> bool {
        matches!(self, Health::Warning(_) | Health::Failing(_))
    }
}

/// What NVMe drives report in their health log (also used by macOS).
fn nvme(critical: u8, spare: u8, spare_threshold: u8, used: u8, media_errors: u128) -> Health {
    if critical & 0x0F != 0 || (spare_threshold > 0 && spare < spare_threshold) {
        let mut why = Vec::new();
        if critical & 0x04 != 0 {
            why.push("its reliability is degraded");
        }
        if critical & 0x08 != 0 {
            why.push("it has become read-only");
        }
        if critical & 0x01 != 0 || spare < spare_threshold {
            why.push("its spare capacity is used up");
        }
        if critical & 0x02 != 0 {
            why.push("it is too hot");
        }
        return Health::Failing(why.join(", "));
    }
    if media_errors > 0 {
        return Health::Warning(format!("{media_errors} media errors"));
    }
    if used >= 100 {
        return Health::Warning(format!("worn out ({used} % of its rated life used)"));
    }
    Health::Good
}

/// The health of the drive holding `path` (a disk, a partition or a volume).
pub fn check(path: &str) -> Health {
    let h = platform::check(path);
    log::info!("{path}: health {h:?}");
    h
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::{Health, nvme};

    pub fn check(path: &str) -> Health {
        // /dev/rdisk4s1 → disk4s1; diskutil answers for the whole drive.
        let id = path.trim_start_matches("/dev/").trim_start_matches('r');
        let Some(info) = diskutil_info(id) else { return Health::Unknown };
        from_info(&info)
    }

    pub fn from_info(info: &serde_json::Value) -> Health {
        let status = info.get("SMARTStatus").and_then(|s| s.as_str()).unwrap_or("");
        if status == "Failing" {
            return Health::Failing("S.M.A.R.T.".into());
        }
        if let Some(n) = info.get("SMARTDeviceSpecificKeysMayVaryNotGuaranteed") {
            let num = |k: &str| n.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
            let errors = u128::from(num("MEDIA_ERRORS_0")) | (u128::from(num("MEDIA_ERRORS_1")) << 64);
            let threshold = num("AVAILABLE_SPARE_THRESHOLD").min(255) as u8;
            let h = nvme(
                0,
                num("AVAILABLE_SPARE").min(255) as u8,
                threshold,
                num("PERCENTAGE_USED").min(255) as u8,
                errors,
            );
            if h != Health::Good {
                return h;
            }
        }
        if status == "Verified" { Health::Good } else { Health::Unknown }
    }

    fn diskutil_info(id: &str) -> Option<serde_json::Value> {
        let out = Command::new("diskutil").args(["info", "-plist", id]).output().ok()?;
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
        serde_json::from_slice(&plutil.wait_with_output().ok()?.stdout).ok()
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::{Health, nvme};

    const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002D_1080;
    const IOCTL_STORAGE_PREDICT_FAILURE: u32 = 0x002D_1100;
    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
    const FILE_SHARE_READ_WRITE: u32 = 3;

    fn open(path: &str) -> Option<std::fs::File> {
        // No access rights are needed to ask the driver.
        std::fs::OpenOptions::new().access_mode(0).share_mode(FILE_SHARE_READ_WRITE).open(path).ok()
    }

    fn ioctl(f: &std::fs::File, code: u32, input: &[u8], output: &mut [u8]) -> bool {
        let mut returned = 0u32;
        // SAFETY: valid handle and buffers of the given lengths.
        unsafe {
            DeviceIoControl(
                f.as_raw_handle() as _,
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            ) != 0
        }
    }

    pub fn check(path: &str) -> Health {
        let path = crate::source::normalize_device_path(path);
        let Some(f) = open(&path) else { return Health::Unknown };
        // A volume (\\.\C:) lives on a physical drive: ask that.
        let mut number = [0u8; 12];
        let drive = if ioctl(&f, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut number) {
            let n = u32::from_le_bytes([number[4], number[5], number[6], number[7]]);
            open(&format!(r"\\.\PhysicalDrive{n}")).unwrap_or(f)
        } else {
            f
        };
        let nvme = nvme_health(&drive);
        if nvme.is_bad() {
            return nvme;
        }
        // STORAGE_PREDICT_FAILURE: PredictFailure, then vendor data.
        let mut predict = [0u8; 4 + 512];
        if ioctl(&drive, IOCTL_STORAGE_PREDICT_FAILURE, &[], &mut predict) {
            if u32::from_le_bytes([predict[0], predict[1], predict[2], predict[3]]) != 0 {
                return Health::Failing("S.M.A.R.T.".into());
            }
            return Health::Good;
        }
        nvme
    }

    /// The NVMe health log (log page 2), through the storage driver.
    fn nvme_health(drive: &std::fs::File) -> Health {
        // STORAGE_PROPERTY_QUERY { PropertyId = StorageDeviceProtocolSpecificProperty,
        // QueryType = standard, STORAGE_PROTOCOL_SPECIFIC_DATA { NVMe, log page,
        // page 2, offset 40, length 512 } } followed by room for the page.
        let mut q = vec![0u8; 8 + 40 + 512];
        let put = |q: &mut Vec<u8>, at: usize, v: u32| q[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut q, 0, 50);
        put(&mut q, 4, 0);
        put(&mut q, 8, 3); // ProtocolTypeNvme
        put(&mut q, 12, 2); // NVMeDataTypeLogPage
        put(&mut q, 16, 2); // health information
        put(&mut q, 24, 40);
        put(&mut q, 28, 512);
        let mut out = vec![0u8; q.len()];
        if !ioctl(drive, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut out) {
            return Health::Unknown;
        }
        // STORAGE_PROTOCOL_DATA_DESCRIPTOR: version, size, then the data
        // block, whose offset field points at the page.
        let offset = u32::from_le_bytes([out[24], out[25], out[26], out[27]]) as usize;
        let Some(page) = out.get(8 + offset..8 + offset + 512) else { return Health::Unknown };
        let errors = u128::from_le_bytes(page[160..176].try_into().unwrap_or_default());
        nvme(page[0], page[3], page[4], page[5], errors)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::os::fd::AsRawFd;

    use super::{Health, nvme};

    /// /dev/sda1 → /dev/sda, /dev/nvme0n1p2 → /dev/nvme0n1,
    /// /dev/mmcblk0p1 → /dev/mmcblk0.
    fn whole_disk(path: &str) -> String {
        let digits = path.trim_end_matches(|c: char| c.is_ascii_digit());
        // Names ending in a digit number their partitions with "p".
        if let Some(base) = digits.strip_suffix('p')
            && base.ends_with(|c: char| c.is_ascii_digit())
            && digits.len() < path.len()
        {
            return base.to_string();
        }
        if path.contains("nvme") || path.contains("mmcblk") || path.contains("loop") {
            return path.to_string();
        }
        digits.to_string()
    }

    pub fn check(path: &str) -> Health {
        let disk = whole_disk(path);
        if let Some(h) = smartctl(&disk) {
            return h;
        }
        if disk.contains("nvme") {
            return nvme_log(&disk);
        }
        Health::Unknown
    }

    fn smartctl(disk: &str) -> Option<Health> {
        let out = std::process::Command::new("smartctl").args(["-H", "-j", disk]).output().ok()?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        match v.get("smart_status")?.get("passed")?.as_bool()? {
            true => Some(Health::Good),
            false => Some(Health::Failing("S.M.A.R.T.".into())),
        }
    }

    /// NVMe Get Log Page (health information), as root.
    fn nvme_log(disk: &str) -> Health {
        #[repr(C)]
        #[derive(Default)]
        struct AdminCmd {
            opcode: u8,
            flags: u8,
            rsvd1: u16,
            nsid: u32,
            cdw2: u32,
            cdw3: u32,
            metadata: u64,
            addr: u64,
            metadata_len: u32,
            data_len: u32,
            cdw10: u32,
            cdw11: u32,
            cdw12: u32,
            cdw13: u32,
            cdw14: u32,
            cdw15: u32,
            timeout_ms: u32,
            result: u32,
        }
        const NVME_IOCTL_ADMIN_CMD: libc::c_ulong = 0xC048_4E41;
        let Ok(f) = std::fs::File::open(disk) else { return Health::Unknown };
        let mut page = [0u8; 512];
        let mut cmd = AdminCmd {
            opcode: 0x02,
            nsid: 0xFFFF_FFFF,
            addr: page.as_mut_ptr() as u64,
            data_len: 512,
            cdw10: 0x02 | (127 << 16),
            ..AdminCmd::default()
        };
        // SAFETY: a valid descriptor and a command whose buffer lives
        // until the call returns.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), NVME_IOCTL_ADMIN_CMD as _, &mut cmd) };
        if r != 0 {
            return Health::Unknown;
        }
        let errors = u128::from_le_bytes(page[160..176].try_into().unwrap_or_default());
        nvme(page[0], page[3], page[4], page[5], errors)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn whole_disks() {
            assert_eq!(super::whole_disk("/dev/sda1"), "/dev/sda");
            assert_eq!(super::whole_disk("/dev/sdb"), "/dev/sdb");
            assert_eq!(super::whole_disk("/dev/nvme0n1p2"), "/dev/nvme0n1");
            assert_eq!(super::whole_disk("/dev/nvme0n1"), "/dev/nvme0n1");
            assert_eq!(super::whole_disk("/dev/mmcblk0p1"), "/dev/mmcblk0");
            assert_eq!(super::whole_disk("/dev/mmcblk0"), "/dev/mmcblk0");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvme_health_values() {
        assert_eq!(nvme(0, 100, 10, 3, 0), Health::Good);
        assert!(matches!(nvme(0, 100, 10, 3, 2), Health::Warning(_)));
        assert!(matches!(nvme(0, 100, 10, 120, 0), Health::Warning(_)));
        assert!(matches!(nvme(0x04, 100, 10, 3, 0), Health::Failing(_)));
        assert!(matches!(nvme(0, 5, 10, 3, 0), Health::Failing(_)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reports() {
        use serde_json::json;
        let good = json!({"SMARTStatus": "Verified", "SMARTDeviceSpecificKeysMayVaryNotGuaranteed":
            {"MEDIA_ERRORS_0": 0, "AVAILABLE_SPARE": 100, "AVAILABLE_SPARE_THRESHOLD": 99, "PERCENTAGE_USED": 0}});
        assert_eq!(platform::from_info(&good), Health::Good);
        assert!(matches!(platform::from_info(&json!({"SMARTStatus": "Failing"})), Health::Failing(_)));
        assert_eq!(platform::from_info(&json!({"SMARTStatus": "Not Supported"})), Health::Unknown);
        let errors =
            json!({"SMARTStatus": "Verified", "SMARTDeviceSpecificKeysMayVaryNotGuaranteed": {"MEDIA_ERRORS_0": 3}});
        assert!(matches!(platform::from_info(&errors), Health::Warning(_)));
    }
}
