//! Restarting with administrator rights on macOS and Linux.
//!
//! Reading drives needs root there, but an installed app is started by
//! double-clicking, not with `sudo`. The app restarts itself through the
//! system's password dialog (`osascript` on macOS, `pkexec` on Linux) and
//! hands the new instance the current settings and the real user, so that
//! recovered files can be given back to that user instead of root.
//! (On Windows the manifest already asks for administrator rights.)

use std::path::{Path, PathBuf};
use std::process::Child;
#[cfg(unix)]
use std::process::Command;
use std::time::{Duration, Instant};

/// The settings of the instance that asked for the restart, as JSON.
pub const SETTINGS_ENV: &str = "WDFR_SETTINGS";
/// `uid:gid` of the user who asked for the restart.
#[cfg(unix)]
const OWNER_ENV: &str = "WDFR_OWNER";
/// A file the new instance creates once its window is up.
const READY_ENV: &str = "WDFR_READY_FILE";

/// Whether this system can restart the app with administrator rights.
pub fn supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

pub fn is_root() -> bool {
    #[cfg(unix)]
    // SAFETY: geteuid has no preconditions and cannot fail.
    return unsafe { libc::geteuid() } == 0;
    #[cfg(not(unix))]
    false
}

/// A restart waiting for the password dialog and the new window.
pub struct Restart {
    child: Child,
    ready: PathBuf,
    /// When the password dialog closed successfully (macOS) — the new
    /// window should follow shortly.
    authorised: Option<Instant>,
}

pub enum Status {
    Waiting,
    /// The new instance is up: this one can close.
    Started,
    /// Cancelled, or the new instance did not start.
    Failed,
}

impl Restart {
    pub fn poll(&mut self) -> Status {
        if self.ready.exists() {
            let _ = std::fs::remove_file(&self.ready);
            return Status::Started;
        }
        if let Some(t) = self.authorised {
            return if t.elapsed() > Duration::from_secs(30) { Status::Failed } else { Status::Waiting };
        }
        match self.child.try_wait() {
            // osascript returns once the new instance is launched; pkexec only
            // returns when it has failed or the new instance has quit.
            Ok(Some(s)) if s.success() && cfg!(target_os = "macos") => {
                self.authorised = Some(Instant::now());
                Status::Waiting
            }
            Ok(Some(_)) | Err(_) => Status::Failed,
            Ok(None) => Status::Waiting,
        }
    }
}

/// Shell-quotes `s` for `sh -c`.
#[cfg(target_os = "macos")]
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Starts a copy of the app as root, after the system asks for the password.
#[cfg(unix)]
pub fn restart(settings_json: &str) -> std::io::Result<Restart> {
    // Inside an AppImage the binary lives on a FUSE mount root cannot read:
    // start the AppImage itself.
    let exe = std::env::var_os("APPIMAGE").map(PathBuf::from).map_or_else(std::env::current_exe, Ok)?;
    let ready = std::env::temp_dir().join(format!("wdfr-ready-{}", std::process::id()));
    let _ = std::fs::remove_file(&ready);
    // SAFETY: getuid/getgid have no preconditions and cannot fail.
    let owner = unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) };
    let vars =
        [(OWNER_ENV, owner), (READY_ENV, ready.display().to_string()), (SETTINGS_ENV, settings_json.to_string())];

    #[cfg(target_os = "macos")]
    let child = {
        let mut cmd: String = vars.iter().map(|(k, v)| format!("{k}={} ", quote(v))).collect();
        cmd.push_str(&quote(&exe.display().to_string()));
        cmd.push_str(" >/dev/null 2>&1 &");
        Command::new("osascript")
            .args(["-e", "on run argv", "-e", "do shell script (item 1 of argv) with administrator privileges"])
            .args(["-e", "end run", &cmd])
            .spawn()?
    };
    #[cfg(not(target_os = "macos"))]
    let child = {
        // pkexec clears the environment: pass on what a window needs.
        let mut cmd = Command::new("pkexec");
        cmd.arg("env");
        for (k, v) in &vars {
            cmd.arg(format!("{k}={v}"));
        }
        for k in ["DISPLAY", "XAUTHORITY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "LANG"] {
            if let Some(v) = std::env::var_os(k) {
                cmd.arg(format!("{k}={}", v.to_string_lossy()));
            }
        }
        cmd.arg(exe).spawn()?
    };
    Ok(Restart { child, ready, authorised: None })
}

#[cfg(not(unix))]
pub fn restart(_settings_json: &str) -> std::io::Result<Restart> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "not needed on this system"))
}

/// Tells the instance that asked for this restart that the window is up.
pub fn announce_ready() {
    if let Some(path) = std::env::var_os(READY_ENV) {
        let _ = std::fs::write(path, b"");
    }
}

/// The user running the app as root through a restart or `sudo`.
#[cfg(unix)]
fn owner() -> Option<(u32, u32)> {
    if !is_root() {
        return None;
    }
    let pair = std::env::var(OWNER_ENV)
        .ok()
        .and_then(|v| v.split_once(':').map(|(u, g)| (u.to_string(), g.to_string())))
        .or_else(|| Some((std::env::var("SUDO_UID").ok()?, std::env::var("SUDO_GID").ok()?)))?;
    Some((pair.0.parse().ok()?, pair.1.parse().ok()?))
}

/// The home folder of the real user (not root's), for the default
/// destination on the Desktop.
pub fn user_home() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        let (uid, _) = owner()?;
        // SAFETY: getpwuid returns NULL or a pointer to a static record that
        // stays valid until the next call; the directory is copied out at once.
        unsafe {
            let pw = libc::getpwuid(uid);
            if pw.is_null() || (*pw).pw_dir.is_null() {
                return None;
            }
            let dir = std::ffi::CStr::from_ptr((*pw).pw_dir);
            Some(PathBuf::from(std::ffi::OsStr::from_encoded_bytes_unchecked(dir.to_bytes())))
        }
    }
    #[cfg(not(unix))]
    None
}

/// Gives recovered files to the real user, so they can move and delete them
/// without a password.
pub fn give_back(path: &Path) {
    #[cfg(unix)]
    if let Some((uid, gid)) = owner() {
        fn walk(p: &Path, uid: u32, gid: u32) {
            let _ = std::os::unix::fs::lchown(p, Some(uid), Some(gid));
            if p.is_dir() && !p.is_symlink() {
                for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                    walk(&e.path(), uid, gid);
                }
            }
        }
        walk(path, uid, gid);
    }
    #[cfg(not(unix))]
    let _ = path;
}
