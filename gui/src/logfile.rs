//! The app's log file, next to its settings: release builds on Windows have
//! no console, so without it a failed start leaves no trace at all.
//!
//! * Windows: `%APPDATA%\wdfr\data\wdfr-gui.log`
//! * macOS:   `~/Library/Application Support/wdfr/wdfr-gui.log`
//! * Linux:   `~/.local/share/wdfr/wdfr-gui.log`

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Settings and the log live under this name (the viewport's app id).
pub const APP_ID: &str = "wdfr";
/// A longer log is started afresh.
const MAX_SIZE: u64 = 1 << 20;

struct FileLog {
    file: Mutex<File>,
}

impl log::Log for FileLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        // The app's own messages, and only warnings from the libraries
        // (graphics drivers are chatty).
        m.level() <= log::Level::Warn || m.target().starts_with("wdfr")
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} {}: {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            r.level(),
            r.target(),
            r.args()
        );
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
    }
}

/// Where the log is written.
pub fn path() -> Option<PathBuf> {
    eframe::storage_dir(APP_ID).map(|d| d.join("wdfr-gui.log"))
}

/// Starts logging to the file, and records panics in it. Returns its path.
pub fn init() -> Option<PathBuf> {
    let path = path()?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    let big = std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_SIZE);
    let file = OpenOptions::new().create(true).append(!big).write(true).truncate(big).open(&path).ok()?;
    log::set_boxed_logger(Box::new(FileLog { file: Mutex::new(file) })).ok()?;
    log::set_max_level(log::LevelFilter::Info);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(target: "wdfr::panic", "{info}\n{}", std::backtrace::Backtrace::force_capture());
        log::logger().flush();
        default_hook(info);
    }));
    log::info!(
        "Deleted Files Recovery {} on {} {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    Some(path)
}
