//! Signature-based file carving.
//!
//! When file-system metadata is gone (formatted card, overwritten MFT,
//! unknown file system) files can still be found by their content. Each
//! [`Format`] recognises a file's header and then *parses the structure* to
//! find its exact length (PNG chunk chain, MP4 box tree, ZIP central
//! directory, ...), which gives far fewer broken or bloated results than
//! the naive "header ... footer" approach.
//!
//! Carving assumes a file is stored contiguously, which holds for most
//! media written once by cameras and phones.

pub mod formats;
pub mod meta;
mod reader;
mod scan;

use std::fmt;
use std::str::FromStr;

use serde::Serialize;

pub use reader::Reader;
pub use scan::{CarveOptions, CarveStats, Carved, carve, carve_with_blocks};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, serde::Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Image,
    Video,
    Audio,
    Document,
    Archive,
    Database,
}

impl Category {
    pub const ALL: [Category; 6] =
        [Category::Image, Category::Video, Category::Audio, Category::Document, Category::Archive, Category::Database];

    pub fn dir_name(self) -> &'static str {
        match self {
            Category::Image => "images",
            Category::Video => "videos",
            Category::Audio => "audio",
            Category::Document => "documents",
            Category::Archive => "archives",
            Category::Database => "databases",
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Category::Image => "image",
            Category::Video => "video",
            Category::Audio => "audio",
            Category::Document => "document",
            Category::Archive => "archive",
            Category::Database => "database",
        })
    }
}

impl FromStr for Category {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim().to_ascii_lowercase();
        Category::ALL.into_iter().find(|c| c.to_string() == s || c.dir_name() == s).ok_or_else(|| {
            format!("unknown category {s:?} (expected one of: image, video, audio, document, archive, database)")
        })
    }
}

/// Result of measuring a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub len: u64,
    pub ext: &'static str,
}

/// A carvable file format.
pub trait Format: Send + Sync {
    /// Short identifier, e.g. `"jpeg"`.
    fn name(&self) -> &'static str;

    /// Every (extension, category) this format can produce.
    fn kinds(&self) -> &'static [(&'static str, Category)];

    /// Bytes a file of this format can start with (empty = any). Lets the
    /// scanner skip `probe` calls at the vast majority of offsets.
    fn first_bytes(&self) -> &'static [u8];

    /// Upper bound on file size, limiting how far `measure` may read.
    fn max_size(&self) -> u64;

    /// Cheap check on the first bytes at a candidate offset. `head` holds
    /// at least [`HEAD_LEN`] bytes unless the source ends sooner.
    fn probe(&self, head: &[u8]) -> bool;

    /// Parses the structure starting at the reader's origin and returns the
    /// exact file length, or `None` if this is not a valid file.
    fn measure(&self, r: &mut Reader) -> Option<Hit>;

    fn category_of(&self, ext: &str) -> Category {
        self.kinds().iter().find(|(e, _)| *e == ext).map_or(self.kinds()[0].1, |k| k.1)
    }
}

/// Bytes handed to [`Format::probe`].
pub const HEAD_LEN: usize = 1024;

/// Every built-in format.
pub fn all_formats() -> Vec<&'static dyn Format> {
    formats::ALL.to_vec()
}

/// Canonical spelling of an extension for filtering (`JPEG` -> `jpg`).
pub fn normalize_ext(ext: &str) -> String {
    let e = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    match e.as_str() {
        "jpeg" | "jpe" | "jfif" => "jpg".into(),
        "tiff" => "tif".into(),
        "heif" => "heic".into(),
        "mpeg" => "mpg".into(),
        "htm" => "html".into(),
        _ => e,
    }
}

/// Best-effort category for an arbitrary file extension (used to filter
/// files recovered through file-system metadata).
pub fn category_for_ext(ext: &str) -> Option<Category> {
    let e = normalize_ext(ext);
    for f in formats::ALL {
        if let Some((_, c)) = f.kinds().iter().find(|(x, _)| *x == e) {
            return Some(*c);
        }
    }
    use Category::*;
    Some(match e.as_str() {
        "png" | "gif" | "bmp" | "webp" | "heic" | "avif" | "psd" | "svg" | "ico" | "raw" | "orf" | "rw2" | "raf"
        | "srw" | "pef" | "jxl" => Image,
        "mkv" | "webm" | "avi" | "wmv" | "flv" | "m4v" | "3gp" | "mts" | "m2ts" | "ts" | "mpg" | "vob" | "mod"
        | "mov" | "mp4" => Video,
        "mp3" | "wav" | "flac" | "ogg" | "opus" | "m4a" | "aac" | "wma" | "aiff" | "aif" | "amr" | "mid" => Audio,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "rtf" | "txt" | "csv"
        | "epub" | "md" | "pages" | "numbers" | "key" | "html" | "xml" | "json" | "msg" | "eml" | "pst" | "ost" => {
            Document
        }
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "cab" | "iso" | "jar" | "apk" => Archive,
        "sqlite" | "db" | "sqlite3" | "mdb" | "accdb" => Database,
        _ => return None,
    })
}

/// True when `head` starts with a signature of a format that can produce
/// files with extension `ext`. Used to validate guessed cluster locations.
pub fn sniff_matches_ext(head: &[u8], ext: &str) -> bool {
    let e = normalize_ext(ext);
    formats::ALL.iter().filter(|f| f.kinds().iter().any(|(x, _)| *x == e)).any(|f| f.probe(head))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_categories() {
        assert_eq!(category_for_ext("JPEG"), Some(Category::Image));
        assert_eq!(category_for_ext(".mov"), Some(Category::Video));
        assert_eq!(category_for_ext("docx"), Some(Category::Document));
        assert_eq!(category_for_ext("weird"), None);
        assert_eq!("videos".parse::<Category>(), Ok(Category::Video));
    }

    #[test]
    fn format_names_are_unique() {
        let mut names: Vec<_> = all_formats().iter().map(|f| f.name()).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len());
    }
}
