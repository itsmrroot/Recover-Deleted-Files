//! Progress reporting, decoupled from how it is displayed.
//!
//! The recovery engine reports what it is doing through [`Progress`]; the
//! command line renders it as terminal progress bars, the desktop app as
//! widgets. Every method has a no-op default.

use crate::carve::{Carved, Category};
use crate::recover::FsFound;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Items,
    Bytes,
}

pub trait Progress: Sync {
    /// A new task starts (e.g. "Reading NTFS metadata", "Deep search").
    fn begin(&self, _task: &str, _total: u64, _unit: Unit) {}
    /// Absolute position (the total may still be growing).
    fn set(&self, _done: u64, _total: u64) {}
    /// Relative advance.
    fn inc(&self, _n: u64) {}
    /// The item currently being worked on (a file name, a counter, ...).
    fn item(&self, _name: &str) {}
    /// A recoverable file was found.
    fn found(&self, _category: Option<Category>) {}
    /// During a scan: a file found through file-system metadata, in the
    /// order it is added to `Found::fs` (so results can be shown live).
    fn file_found(&self, _file: &FsFound) {}
    /// During a scan: a file found by its content, in the order it is added
    /// to `Found::carved`.
    fn carved_found(&self, _file: &Carved) {}
    /// The current task finished.
    fn end(&self) {}
    fn warn(&self, msg: &str) {
        log::warn!("{msg}");
    }
    fn error(&self, msg: &str) {
        log::error!("{msg}");
    }
}

/// Discards all progress (tests, scripting).
pub struct Silent;

impl Progress for Silent {}
