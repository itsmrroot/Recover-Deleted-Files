//! Writing recovered files safely: name sanitising (recovered names are
//! untrusted input — they must never escape the output directory), unique
//! naming, and the CSV report.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

const MAX_COMPONENT_BYTES: usize = 200;

/// Makes one path component safe on every OS (Windows being the strictest).
pub fn sanitize_component(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    if out.is_empty() || out == "." || out == ".." {
        out = "_".into();
    }
    let stem = out.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        out.insert(0, '_');
    }
    if out.len() > MAX_COMPONENT_BYTES {
        // Keep the extension, trim the stem on a char boundary.
        let ext = out.rfind('.').map(|i| out[i..].to_string()).filter(|e| e.len() <= 16).unwrap_or_default();
        let mut cut = MAX_COMPONENT_BYTES - ext.len();
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out = format!("{}{ext}", &out[..cut]);
    }
    out
}

/// Turns a recovered `/`-separated path into a relative, traversal-free path.
pub fn safe_relative_path(path: &str) -> PathBuf {
    let mut p = PathBuf::new();
    for comp in path.split(['/', '\\']).filter(|c| !c.is_empty()) {
        p.push(sanitize_component(comp));
    }
    if p.as_os_str().is_empty() {
        p.push("_");
    }
    p
}

/// Returns `path`, or `name (1).ext`, `name (2).ext`, ... if taken.
pub fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    (1..).map(|i| parent.join(format!("{stem} ({i}){ext}"))).find(|p| !p.exists()).expect("unbounded iterator")
}

/// Creates `path` (and parents) for writing, never overwriting.
pub fn create_unique(path: PathBuf) -> std::io::Result<(PathBuf, File)> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let path = unique_path(path);
    let f = File::options().write(true).create_new(true).open(&path)?;
    Ok((path, f))
}

#[derive(Debug, Serialize)]
pub struct ReportRow {
    pub method: &'static str,
    pub partition: String,
    pub original_path: String,
    pub recovered_path: String,
    pub size: u64,
    pub disk_offset: String,
    pub condition: String,
    pub modified: String,
    pub note: String,
    pub unreadable_bytes: u64,
}

pub struct Report {
    w: csv::Writer<BufWriter<File>>,
    path: PathBuf,
}

impl Report {
    pub fn create(dir: &Path) -> Result<Self> {
        let path = unique_path(dir.join("report.csv"));
        let f = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { w: csv::Writer::from_writer(BufWriter::new(f)), path })
    }

    pub fn add(&mut self, row: &ReportRow) -> Result<()> {
        self.w.serialize(row)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<PathBuf> {
        self.w.flush()?;
        Ok(self.path)
    }
}

/// Refuses to write recovered files onto the volume being recovered —
/// every byte written there may overwrite a deleted file.
pub fn ensure_not_on_source(source: &str, out: &Path) -> Result<()> {
    let out = fs::canonicalize(out).with_context(|| format!("resolving {}", out.display()))?;
    if same_volume(source, &out) {
        anyhow::bail!(
            "the output directory {} is on the volume being recovered ({source}).\n\
             Writing there can overwrite the very files you are trying to recover.\n\
             Choose a directory on another drive (or pass --allow-same-volume if you really mean it).",
            out.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn same_volume(source: &str, out: &Path) -> bool {
    // \\.\E:  vs  \\?\E:\recovered
    let src_letter = source
        .strip_prefix(r"\\.\")
        .and_then(|s| s.strip_suffix(':'))
        .filter(|s| s.len() == 1)
        .map(|s| s.to_ascii_uppercase());
    let out_s = out.to_string_lossy();
    let out_letter = out_s.trim_start_matches(r"\\?\").chars().next().map(|c| c.to_ascii_uppercase().to_string());
    src_letter.is_some() && src_letter == out_letter
}

#[cfg(unix)]
fn same_volume(source: &str, out: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(src), Ok(dst)) = (fs::metadata(source), fs::metadata(out)) else {
        return false;
    };
    let is_dev = {
        use std::os::unix::fs::FileTypeExt;
        src.file_type().is_block_device() || src.file_type().is_char_device()
    };
    // A block device's rdev equals the st_dev of files on the file system
    // mounted from it. For image files, refuse writing to the same file
    // system only if the image itself would be the output (never), so false.
    is_dev && src.rdev() == dst.dev()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_hostile_names() {
        assert_eq!(sanitize_component("a<b>c:d|e?f*g"), "a_b_c_d_e_f_g");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_component("trailing. "), "trailing");
        assert_eq!(sanitize_component("con.txt"), "_con.txt");
        assert_eq!(sanitize_component("COM1"), "_COM1");
        assert_eq!(sanitize_component("COMMON"), "COMMON");
        assert_eq!(sanitize_component("tab\there"), "tab_here");
        let long = format!("{}.jpg", "é".repeat(150));
        let s = sanitize_component(&long);
        assert!(s.len() <= MAX_COMPONENT_BYTES && s.ends_with(".jpg"));
    }

    #[test]
    fn paths_cannot_escape() {
        assert_eq!(safe_relative_path("../../etc/passwd"), PathBuf::from("_/_/etc/passwd"));
        assert_eq!(safe_relative_path("/abs/path"), PathBuf::from("abs/path"));
        assert_eq!(safe_relative_path("C:\\Windows\\x"), PathBuf::from("C_/Windows/x"));
        assert_eq!(safe_relative_path(""), PathBuf::from("_"));
    }

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir().join(format!("wdfr-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (p1, _) = create_unique(dir.join("a.txt")).unwrap();
        let (p2, _) = create_unique(dir.join("a.txt")).unwrap();
        assert_eq!(p1.file_name().unwrap(), "a.txt");
        assert_eq!(p2.file_name().unwrap(), "a (1).txt");
        fs::remove_dir_all(&dir).unwrap();
    }
}
