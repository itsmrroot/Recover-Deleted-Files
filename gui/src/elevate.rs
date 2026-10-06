//! Getting administrator rights to read drives on macOS and Linux.
//!
//! Reading drives needs root there, but an installed app is started by
//! double-clicking, not with `sudo`.
//!
//! * macOS: the app keeps running as the user and starts a small helper as
//!   root through the system's password dialog. The helper has no window;
//!   it only opens drives read-only and passes the open files back over a
//!   Unix socket, as Apple's own `authopen` does. (A whole app running as
//!   root is cut off from the user's login session — the Dock, input
//!   methods, Spotlight — and macOS keeps retrying them, which is heard as
//!   constant clicking.)
//! * Linux: the app restarts itself through `pkexec` and hands the new
//!   instance the current settings and the real user, so that recovered
//!   files can be given back to that user instead of root.
//!
//! (On Windows the manifest already asks for administrator rights.)

use std::path::{Path, PathBuf};
use std::process::Child;
#[cfg(unix)]
use std::process::Command;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

/// The settings of the instance that asked for the restart, as JSON.
pub const SETTINGS_ENV: &str = "WDFR_SETTINGS";
/// `uid:gid` of the user who asked for the restart.
#[cfg(unix)]
const OWNER_ENV: &str = "WDFR_OWNER";
/// A file the new instance creates once its window is up.
const READY_ENV: &str = "WDFR_READY_FILE";

/// Whether this system can get administrator rights from inside the app.
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

/// Whether drives can already be read through the helper (macOS).
pub fn has_drive_access() -> bool {
    #[cfg(target_os = "macos")]
    return helper::connected();
    #[cfg(not(target_os = "macos"))]
    false
}

/// Runs the drive helper if this process was started as one (macOS), and
/// returns its exit code.
pub fn run_drive_helper() -> Option<i32> {
    #[cfg(target_os = "macos")]
    return helper::run_if_requested();
    #[cfg(not(target_os = "macos"))]
    None
}

/// Opens System Settings at Privacy & Security → Full Disk Access (macOS).
pub fn open_full_disk_access_settings() {
    #[cfg(target_os = "macos")]
    {
        // macOS 13 and later, then the older System Preferences.
        let urls = [
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
        ];
        for url in urls {
            if Command::new("open").arg(url).status().is_ok_and(|s| s.success()) {
                break;
            }
        }
    }
}

/// A request for administrator rights, waiting for the password dialog.
pub struct Restart {
    child: Child,
    /// When the password dialog closed successfully (macOS) — the helper
    /// should connect shortly.
    #[cfg(target_os = "macos")]
    authorised: Option<Instant>,
    #[cfg(target_os = "macos")]
    pending: helper::Pending,
    #[cfg(target_os = "linux")]
    ready: PathBuf,
}

pub enum Status {
    Waiting,
    /// Drives can now be read (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Granted,
    /// The new instance is up: this one can close (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Started,
    /// Cancelled, or the helper or new instance did not start.
    Failed,
}

impl Restart {
    #[cfg(target_os = "macos")]
    pub fn poll(&mut self) -> Status {
        match self.pending.accept() {
            Ok(true) => return Status::Granted,
            Ok(false) => {}
            Err(_) => return Status::Failed,
        }
        if let Some(t) = self.authorised {
            return if t.elapsed() > Duration::from_secs(30) { Status::Failed } else { Status::Waiting };
        }
        // osascript returns once the helper is launched.
        match self.child.try_wait() {
            Ok(Some(s)) if s.success() => {
                self.authorised = Some(Instant::now());
                Status::Waiting
            }
            Ok(Some(_)) | Err(_) => Status::Failed,
            Ok(None) => Status::Waiting,
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn poll(&mut self) -> Status {
        #[cfg(target_os = "linux")]
        if self.ready.exists() {
            let _ = std::fs::remove_file(&self.ready);
            return Status::Started;
        }
        // pkexec only returns when it has failed or the new instance has quit.
        match self.child.try_wait() {
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

/// Asks for the password and starts the drive helper as root.
#[cfg(target_os = "macos")]
pub fn restart(_settings_json: &str) -> std::io::Result<Restart> {
    let (socket, pending) = helper::Pending::listen()?;
    let exe = std::env::current_exe()?;
    let cmd = format!(
        "{} {} {} >/dev/null 2>&1 &",
        quote(&exe.display().to_string()),
        helper::ARG,
        quote(&socket.display().to_string())
    );
    let child = Command::new("osascript")
        .args(["-e", "on run argv", "-e", "do shell script (item 1 of argv) with administrator privileges"])
        .args(["-e", "end run", &cmd])
        .spawn()?;
    Ok(Restart { child, authorised: None, pending })
}

/// Starts a copy of the app as root, after the system asks for the password.
#[cfg(target_os = "linux")]
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
    let child = cmd.arg(exe).spawn()?;
    Ok(Restart { child, ready })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn restart(_settings_json: &str) -> std::io::Result<Restart> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "not needed on this system"))
}

/// macOS: the root helper that opens drives for the app.
#[cfg(target_os = "macos")]
mod helper {
    use std::fs::File;
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Command-line argument that starts the helper, followed by the socket.
    pub const ARG: &str = "--drive-helper";

    /// The app's connection to the helper.
    static CONNECTION: Mutex<Option<UnixStream>> = Mutex::new(None);

    pub fn connected() -> bool {
        CONNECTION.lock().is_ok_and(|c| c.is_some())
    }

    /// Only whole disks and partitions (`/dev/disk4`, `/dev/rdisk4s2`) are
    /// opened, never any other file.
    fn is_drive(path: &str) -> bool {
        let Some(rest) = path.strip_prefix("/dev/rdisk").or_else(|| path.strip_prefix("/dev/disk")) else {
            return false;
        };
        rest.starts_with(|c: char| c.is_ascii_digit()) && rest.chars().all(|c| c.is_ascii_digit() || c == 's')
    }

    pub fn run_if_requested() -> Option<i32> {
        let mut args = std::env::args_os().skip(1);
        if args.next()? != ARG {
            return None;
        }
        let socket = PathBuf::from(args.next()?);
        Some(if serve(&socket).is_ok() { 0 } else { 1 })
    }

    /// Answers each path the app sends (one per line) with an open,
    /// read-only file or an error number, until the app quits.
    fn serve(socket: &Path) -> io::Result<()> {
        if !super::is_root() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let stream = UnixStream::connect(socket)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let path = line.trim_end_matches('\n');
            let opened = if is_drive(path) { File::open(path) } else { Err(io::ErrorKind::PermissionDenied.into()) };
            match opened {
                Ok(f) => send(&stream, 0, Some(f.as_raw_fd()))?,
                Err(e) => send(&stream, e.raw_os_error().unwrap_or(libc::EIO), None)?,
            }
        }
    }

    /// Sends an error number (0 for success) and, with it, an open file.
    fn send(stream: &UnixStream, errno: i32, fd: Option<RawFd>) -> io::Result<()> {
        let payload = errno.to_ne_bytes();
        let mut iov = libc::iovec { iov_base: payload.as_ptr() as *mut _, iov_len: payload.len() };
        // u32s keep the control buffer aligned for `cmsghdr`.
        let mut control = [0u32; 16];
        // SAFETY: `msg` points at live buffers for the whole call; the control
        // buffer is large enough for one descriptor (CMSG_SPACE(4) = 16).
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            if let Some(fd) = fd {
                msg.msg_control = control.as_mut_ptr().cast();
                msg.msg_controllen = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as _;
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
            }
            if libc::sendmsg(stream.as_raw_fd(), &msg, 0) != payload.len() as isize {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Receives what [`send`] sent. The outer error means the connection
    /// failed; the inner one that the drive could not be opened.
    fn receive(stream: &UnixStream) -> io::Result<io::Result<File>> {
        let mut payload = [0u8; 4];
        let mut iov = libc::iovec { iov_base: payload.as_mut_ptr().cast(), iov_len: payload.len() };
        let mut control = [0u32; 16];
        let mut file = None;
        // SAFETY: as in `send`; a received descriptor is owned by nobody else
        // and is wrapped in a `File` at once.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = size_of_val(&control) as _;
            let n = libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_WAITALL);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let fd = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<RawFd>());
                    file = Some(File::from_raw_fd(fd));
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
            if n as usize != payload.len() {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(match (i32::from_ne_bytes(payload), file) {
            (0, Some(f)) => Ok(f),
            (0, None) => Err(io::Error::from_raw_os_error(libc::EIO)),
            (errno, _) => Err(io::Error::from_raw_os_error(errno)),
        })
    }

    /// Opens `path` through the helper, for `wdfr::source::DiskSource`.
    fn open(path: &str) -> io::Result<File> {
        let denied = || io::Error::from(io::ErrorKind::PermissionDenied);
        if path.contains('\n') {
            return Err(denied());
        }
        let mut conn = CONNECTION.lock().map_err(|_| denied())?;
        let Some(stream) = conn.as_mut() else { return Err(denied()) };
        match writeln!(stream, "{path}").and_then(|()| receive(stream)) {
            Ok(result) => result,
            Err(_) => {
                // The helper is gone: the user has to allow access again.
                *conn = None;
                Err(denied())
            }
        }
    }

    /// The app's end of the socket, until the helper connects.
    pub struct Pending {
        listener: UnixListener,
        dir: PathBuf,
    }

    impl Pending {
        /// Listens on a socket in a folder only this user can enter.
        pub fn listen() -> io::Result<(PathBuf, Self)> {
            let dir = std::env::temp_dir().join(format!("wdfr-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
            let socket = dir.join("drives");
            let listener = UnixListener::bind(&socket)?;
            listener.set_nonblocking(true)?;
            Ok((socket, Self { listener, dir }))
        }

        /// Takes the helper's connection once it has arrived.
        pub fn accept(&self) -> io::Result<bool> {
            let stream = match self.listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return Err(e),
            };
            stream.set_nonblocking(false)?;
            // Only a helper running as root is accepted.
            let (mut uid, mut gid) = (u32::MAX, u32::MAX);
            // SAFETY: the descriptor is valid; both outputs are plain integers.
            if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 || uid != 0 {
                return Ok(false);
            }
            *CONNECTION.lock().map_err(|_| io::Error::other("lock"))? = Some(stream);
            wdfr::source::set_device_opener(Box::new(open));
            Ok(true)
        }
    }

    impl Drop for Pending {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::unix::fs::FileTypeExt;

        use super::*;

        #[test]
        fn only_drives_are_opened() {
            for ok in ["/dev/disk0", "/dev/rdisk4", "/dev/rdisk4s2", "/dev/disk12s10"] {
                assert!(is_drive(ok), "{ok}");
            }
            for bad in ["/dev/disk", "/dev/rdisks1", "/etc/sudoers", "/dev/disk4/../../etc", "/dev/null", "/dev/disk1 "]
            {
                assert!(!is_drive(bad), "{bad}");
            }
        }

        #[test]
        fn files_pass_through_the_socket() {
            let (a, b) = UnixStream::pair().unwrap();
            let f = File::open("/dev/null").unwrap();
            send(&a, 0, Some(f.as_raw_fd())).unwrap();
            let got = receive(&b).unwrap().unwrap();
            assert!(got.metadata().unwrap().file_type().is_char_device());
            send(&a, libc::ENOENT, None).unwrap();
            let err = receive(&b).unwrap().unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
            drop(a);
            assert!(receive(&b).is_err());
        }
    }
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
