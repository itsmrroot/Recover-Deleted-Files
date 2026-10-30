//! Checking GitHub for a newer release and installing it.
//!
//! The installer for this system and this kind of installation is
//! downloaded, checked against the SHA-256 digest GitHub publishes, and
//! installed after the app has closed; the new version then starts:
//!
//! * Windows (installed with setup): the setup runs silently (`/S`) and
//!   starts the app again (`/R`).
//! * macOS (.app): the bundle is replaced from the disk image.
//! * Linux: an AppImage replaces itself; .deb and .rpm installs go through
//!   the package manager after the system's password dialog.
//!
//! Portable copies only get a link to the download page.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use eframe::egui::{self, Align, Layout, RichText, Ui};
use egui_phosphor::regular as icon;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use wdfr::progress::{Progress, Unit};
use wdfr::units::format_size;

use crate::i18n::{icon_label, tr, trf, trl, trlf};
use crate::jobs::Job;
use crate::theme::{self, Palette};

const LATEST: &str = "https://api.github.com/repos/itsmrroot/Recover-Deleted-Files--Release/releases/latest";

/// The version of this program.
pub fn current() -> &'static str {
    // Development aid: pretend to be an older version to try updating.
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("WDFR_PRETEND_VERSION") {
        return Box::leak(v.into_boxed_str());
    }
    env!("CARGO_PKG_VERSION")
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    /// The release notes (Markdown).
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    size: u64,
    browser_download_url: String,
    /// `sha256:<hex>`.
    digest: Option<String>,
}

/// A newer release.
#[derive(Clone)]
pub struct Release {
    /// Without the leading `v`.
    pub version: String,
    pub page: String,
    /// What is new, as GitHub shows it (Markdown).
    notes: String,
    /// The installer for this system, if it can be installed from the app
    /// (and has already been published).
    asset: Option<Asset>,
}

#[derive(Clone)]
struct Asset {
    name: String,
    url: String,
    size: u64,
    sha256: Option<String>,
}

impl Release {
    /// Whether "Update" can install it, rather than open the download page.
    pub fn installable(&self) -> bool {
        self.asset.is_some()
    }
}

/// How this copy of the app was installed.
#[derive(Clone, Debug, PartialEq)]
// Each system uses only some of these.
#[allow(dead_code)]
enum Kind {
    /// Windows, installed by the setup program into this folder.
    Setup,
    /// macOS, this .app bundle.
    Bundle(PathBuf),
    /// Linux, this AppImage file.
    AppImage(PathBuf),
    Deb,
    Rpm,
    /// Unpacked from an archive: updated by hand.
    Portable,
}

fn kind() -> Kind {
    let Ok(exe) = std::env::current_exe() else { return Kind::Portable };
    #[cfg(windows)]
    if exe.parent().is_some_and(|d| d.join("uninstall.exe").is_file()) {
        return Kind::Setup;
    }
    #[cfg(target_os = "macos")]
    if let Some(app) = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app"))
        && exe.parent().is_some_and(|d| d.ends_with("Contents/MacOS"))
    {
        return Kind::Bundle(app.to_path_buf());
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(file) = std::env::var_os("APPIMAGE") {
            return Kind::AppImage(PathBuf::from(file));
        }
        let owned_by = |cmd: &str, arg: &str| {
            std::process::Command::new(cmd)
                .args([arg, &exe.display().to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if owned_by("dpkg-query", "-S") {
            return Kind::Deb;
        }
        if owned_by("rpm", "-qf") {
            return Kind::Rpm;
        }
    }
    let _ = exe;
    Kind::Portable
}

/// Name of the release file that updates `kind` on this system.
fn asset_name(kind: &Kind) -> Option<String> {
    let arch = |x64: &'static str, arm: &'static str| if cfg!(target_arch = "aarch64") { arm } else { x64 };
    Some(match kind {
        Kind::Setup => format!("wdfr-windows-{}-setup.exe", arch("x64", "arm64")),
        Kind::Bundle(_) => format!("wdfr-macos-{}.dmg", arch("intel", "apple-silicon")),
        Kind::AppImage(_) => format!("wdfr-linux-{}.AppImage", arch("x86_64", "arm64")),
        Kind::Deb => format!("wdfr-linux-{}.deb", arch("x86_64", "arm64")),
        Kind::Rpm => format!("wdfr-linux-{}.rpm", arch("x86_64", "arm64")),
        Kind::Portable => return None,
    })
}

/// `1.2.3` or `v1.2.3` as numbers; anything after the numbers is ignored.
fn parse_version(v: &str) -> Option<[u64; 3]> {
    let mut parts = v.trim().trim_start_matches('v').split(['.', '-', '+']);
    let mut out = [0; 3];
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    Some(out)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(format!("wdfr-gui/{}", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .into()
}

/// GitHub refused because this network asked too often (without signing in, it
/// answers 60 requests an hour per address). `until` is when it answers again, as a Unix time.
#[derive(Debug)]
struct RateLimited {
    until: Option<i64>,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitHub allows only a limited number of requests per hour from one network")
    }
}

impl std::error::Error for RateLimited {}

/// Is an answer "too many requests", and until when? `Some(None)`: yes, but GitHub did not say
/// until when. The header values are as GitHub sends them.
fn rate_limited(
    status: u16,
    remaining: Option<&str>,
    retry_after: Option<&str>,
    reset: Option<&str>,
    now: i64,
) -> Option<Option<i64>> {
    if !matches!(status, 403 | 429) || (remaining != Some("0") && retry_after.is_none() && status != 429) {
        return None;
    }
    let seconds = retry_after.and_then(|s| s.trim().parse::<i64>().ok());
    Some(reset.and_then(|s| s.trim().parse::<i64>().ok()).or(seconds.map(|s| now + s)))
}

/// The sentence for a refused check, with the time of day (local) when GitHub answers again.
fn rate_limit_text(until: Option<i64>) -> String {
    let at = until
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string());
    match at {
        Some(time) => trf(
            "GitHub allows only a limited number of update checks per hour from one network. Try again after {time}.",
            &[("time", &time)],
        ),
        None => {
            tr("GitHub allows only a limited number of update checks per hour from one network. Try again in a while.")
                .to_string()
        }
    }
}

/// Asks GitHub for the latest release. `None` means this is the newest.
pub fn check() -> Result<Option<Release>> {
    // Development aid: WDFR_UPDATE_URL asks another address (a stand-in for GitHub's answers).
    #[cfg(debug_assertions)]
    let url = std::env::var("WDFR_UPDATE_URL").unwrap_or_else(|_| LATEST.to_string());
    #[cfg(not(debug_assertions))]
    let url = LATEST;
    // The status is read here, not turned into an error, to tell "too many requests" from the rest.
    let mut response = agent()
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .context("could not reach GitHub")?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let now = chrono::Utc::now().timestamp();
        if let Some(until) = rate_limited(
            status,
            header("x-ratelimit-remaining").as_deref(),
            header("retry-after").as_deref(),
            header("x-ratelimit-reset").as_deref(),
            now,
        ) {
            return Err(RateLimited { until }.into());
        }
        bail!("GitHub answered with status {status}");
    }
    let text = response.body_mut().read_to_string()?;
    let gh: GhRelease = serde_json::from_str(&text).context("unexpected answer from GitHub")?;
    let version = gh.tag_name.trim_start_matches('v').to_string();
    if gh.draft || gh.prerelease || !is_newer(&version, current()) {
        return Ok(None);
    }
    let asset = asset_name(&kind()).and_then(|name| {
        let a = gh.assets.into_iter().find(|a| a.name == name)?;
        Some(Asset {
            name: a.name,
            url: a.browser_download_url,
            size: a.size,
            sha256: a.digest.and_then(|d| d.strip_prefix("sha256:").map(str::to_ascii_lowercase)),
        })
    });
    let notes = gh.body.unwrap_or_default().chars().take(4000).collect();
    Ok(Some(Release { version, page: gh.html_url, notes, asset }))
}

/// Downloads the installer of `release` into a new temporary folder and
/// checks it. Returns the file and its SHA-256, which [`install`] checks
/// again right before running it.
pub fn download(release: &Release, progress: &dyn Progress, cancel: &AtomicBool) -> Result<(PathBuf, String)> {
    let Some(asset) = &release.asset else { bail!("no installer for this system") };
    // GitHub publishes a checksum for every file; without one there is
    // nothing to check the download against.
    ensure!(asset.sha256.is_some(), "the update has no checksum");
    let dir = std::env::temp_dir().join(format!("wdfr-update-{}", release.version));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&asset.name);

    progress.begin("Downloading the update", asset.size, Unit::Bytes);
    let mut response = agent().get(&asset.url).call().context("could not download the update")?;
    let mut body = response.body_mut().with_config().limit(asset.size + 1).reader();
    let mut file = File::create(&path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0; 256 * 1024];
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let n = body.read(&mut buf).context("the download was interrupted")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        hash.update(&buf[..n]);
        total += n as u64;
        progress.inc(n as u64);
    }
    file.sync_all()?;
    drop(file);
    ensure!(total == asset.size, "the download is incomplete");
    let want = asset.sha256.clone().unwrap_or_default();
    let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    ensure!(got == want, "the download is damaged (checksum mismatch)");
    progress.end();
    Ok((path, want))
}

/// The SHA-256 of what `file` reads, in hex.
#[cfg_attr(not(windows), allow(dead_code))]
fn sha256_hex(file: &mut impl Read) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buf = vec![0; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Shell-quotes `s` for `sh`.
#[cfg(unix)]
fn quote(s: &Path) -> String {
    format!("'{}'", s.display().to_string().replace('\'', r"'\''"))
}

/// Runs `script` with `sh` once this process has exited, detached from it.
#[cfg(unix)]
fn after_exit(script: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let pid = std::process::id();
    let script = format!("while kill -0 {pid} 2>/dev/null; do sleep 0.3; done\n{script}");
    std::process::Command::new("/bin/sh")
        .args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .context("could not start the installer")?;
    Ok(())
}

/// Starts installing the downloaded `file`. On success the app must close
/// right away: the installation continues without it and starts the new
/// version.
pub fn install(file: &Path, sha256: &str) -> Result<()> {
    let _ = sha256;
    match kind() {
        #[cfg(windows)]
        Kind::Setup => {
            use std::os::windows::fs::OpenOptionsExt;
            use std::os::windows::process::CommandExt;
            // DETACHED_PROCESS: the setup outlives the app it replaces.
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const FILE_SHARE_READ: u32 = 1;
            // The setup runs from the user's temporary folder, perhaps long
            // after the download ("Update later"): held open so that nothing
            // can change it (others may only read), and checked again, until
            // it has started.
            let mut held = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(file)
                .context("could not open the downloaded setup")?;
            ensure!(sha256_hex(&mut held)? == sha256, "the setup changed after it was downloaded");
            std::process::Command::new(file)
                .args(["/S", "/R"])
                .creation_flags(DETACHED_PROCESS)
                .spawn()
                .context("could not start the setup")?;
            drop(held);
            Ok(())
        }
        #[cfg(target_os = "macos")]
        Kind::Bundle(app) => {
            let parent = app.parent().context("unexpected app location")?;
            if !writable(parent) {
                // No rights to replace the app: let the user drag it over.
                std::process::Command::new("open").arg(file).spawn()?;
                return Ok(());
            }
            let (dmg, app, staged) = (quote(file), quote(&app), quote(&app.with_extension("app.new")));
            // The disk image shows the licence, which `yes` accepts. The old
            // app is only removed once the new one is fully copied; if
            // anything fails, the disk image is opened for the user.
            after_exit(&format!(
                r#"mnt=$(mktemp -d) || exit 1
fail() {{ hdiutil detach "$mnt" -quiet 2>/dev/null; open {dmg}; exit 1; }}
yes | PAGER=cat hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mnt" {dmg} >/dev/null 2>&1 || fail
src=$(ls -d "$mnt"/*.app | head -n 1)
[ -n "$src" ] || fail
rm -rf {staged}
ditto "$src" {staged} || fail
hdiutil detach "$mnt" -quiet
rm -rf {app} && mv {staged} {app} && open {app}"#
            ))
        }
        #[cfg(target_os = "linux")]
        Kind::AppImage(path) => {
            use std::os::unix::fs::PermissionsExt;
            let dir = path.parent().context("unexpected AppImage location")?;
            ensure!(writable(dir), "no permission to replace {}", path.display());
            // Copied next to it first, so that the switch is a rename.
            let staged = dir.join(format!(".{}.new", path.file_name().unwrap_or_default().to_string_lossy()));
            std::fs::copy(file, &staged)?;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
            std::fs::rename(&staged, &path)?;
            after_exit(&quote(&path))
        }
        #[cfg(target_os = "linux")]
        k @ (Kind::Deb | Kind::Rpm) => {
            use std::os::unix::fs::PermissionsExt;
            // The package manager reads it as root and as its own user.
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644))?;
            if let Some(dir) = file.parent() {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
            }
            let install = if k == Kind::Deb {
                "apt-get install -y --allow-downgrades \"$1\""
            } else {
                "if command -v dnf >/dev/null; then dnf install -y \"$1\"; \
                 elif command -v zypper >/dev/null; then zypper --non-interactive install --allow-unsigned-rpm \"$1\"; \
                 else rpm -U \"$1\"; fi"
            };
            let sudo = if crate::elevate::is_root() { "" } else { "pkexec " };
            let exe = std::env::current_exe()?;
            after_exit(&format!(
                "{sudo}/bin/sh -c {} sh {} >/dev/null 2>&1; {} &",
                quote(Path::new(install)),
                quote(file),
                quote(&exe)
            ))
        }
        _ => bail!("this copy of the app has to be updated by hand"),
    }
}

/// Whether this user may create files in `dir`.
#[cfg(unix)]
fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    // SAFETY: `c` is a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

// ---------------------------------------------------------------------------
// In the app

/// Update checks and installs, and what the app shows of them.
#[derive(Default)]
pub struct Updater {
    checking: Option<(Job<Option<Release>>, bool)>,
    /// The newer release found, if any.
    release: Option<Release>,
    /// Checked: this is the newest version.
    latest: bool,
    download: Option<Job<(PathBuf, String)>>,
    /// The update is downloaded and checked: the file and its SHA-256. It is
    /// installed when the user chooses to restart; until then it just waits.
    ready: Option<(PathBuf, String)>,
    /// The installation has started: the app must close now.
    restart: bool,
    error: Option<String>,
    dialog: bool,
}

impl Updater {
    /// Asks GitHub in the background. A `manual` check shows its result; a
    /// check at start only shows the update button if there is one.
    pub fn check(&mut self, ctx: &egui::Context, manual: bool) {
        // Not while an update is downloaded and waiting: it is for this release.
        if self.checking.is_some() || self.download.is_some() || self.ready.is_some() {
            return;
        }
        self.error = None;
        self.latest = false;
        self.checking = Some((Job::spawn(ctx, |_, _| check()), manual));
    }

    /// Takes in finished work. Returns true when the app must close so that
    /// the update can be installed.
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        if let Some((job, manual)) = &self.checking
            && let Some(result) = job.poll()
        {
            let manual = *manual;
            self.checking = None;
            match result {
                Ok(Some(r)) => {
                    self.release = Some(r);
                    self.dialog |= manual;
                }
                Ok(None) => self.latest = true,
                // Too many checks from this network: said as it is, not as "could not reach".
                Err(e) if manual && e.downcast_ref::<RateLimited>().is_some() => {
                    let until = e.downcast_ref::<RateLimited>().and_then(|r| r.until);
                    self.error = Some(rate_limit_text(until))
                }
                Err(e) if manual => {
                    self.error = Some(trf("Could not check for updates: {error}", &[("error", &format!("{e:#}"))]))
                }
                Err(e) => log::info!("update check failed: {e:#}"),
            }
        }
        if let Some(job) = &self.download {
            match job.poll() {
                None => ctx.request_repaint_after(Duration::from_millis(150)),
                Some(result) => {
                    let cancelled = job.stopping();
                    self.download = None;
                    match result {
                        // Downloaded: nothing is installed yet. The user is asked first, since
                        // installing closes the app (and a running scan with it).
                        Ok(file) => {
                            self.ready = Some(file);
                            self.dialog = true;
                        }
                        Err(_) if cancelled => {}
                        Err(e) => {
                            self.error =
                                Some(trf("The update could not be installed: {error}", &[("error", &format!("{e:#}"))]))
                        }
                    }
                }
            }
        }
        std::mem::take(&mut self.restart)
    }

    /// Development aid: "Update now" for the update tour.
    #[cfg(debug_assertions)]
    pub fn update_now(&mut self, ctx: &egui::Context) {
        if let Some(r) = self.release.clone()
            && self.download.is_none()
        {
            self.download = Some(Job::spawn(ctx, move |progress, cancel| download(&r, progress, cancel)));
        }
    }

    /// Development aid: opens the dialog for the screenshot tour.
    #[cfg(debug_assertions)]
    pub fn show_dialog(&mut self) {
        self.dialog = true;
    }

    /// The version a newer release has, once one was found.
    pub fn available(&self) -> Option<&str> {
        self.release.as_ref().map(|r| r.version.as_str())
    }

    /// The button that announces an update, for the top bar or the sidebar.
    pub fn button(&mut self, ui: &mut Ui, p: &Palette) {
        let Some(version) = self.available() else { return };
        // Downloaded and waiting for the restart ("Update later" was chosen).
        let label = if self.ready.is_some() {
            format!("{}  {}", icon::ARROW_CIRCLE_UP, tr("Restart to update"))
        } else {
            format!("{}  {}", icon::ARROW_CIRCLE_UP, trf("Update to {version}", &[("version", &version)]))
        };
        if theme::primary_button(ui, p, &label, true).clicked() {
            self.dialog = true;
        }
    }

    /// "Check for updates" with its result, for the About page.
    pub fn status(&mut self, ui: &mut Ui, p: &Palette) {
        ui.horizontal_wrapped(|ui| {
            let busy = self.checking.is_some();
            if ui
                .add_enabled(!busy, egui::Button::new(icon_label(icon::ARROWS_CLOCKWISE, "Check for updates")))
                .clicked()
            {
                self.check(ui.ctx(), true);
            }
            if busy {
                ui.spinner();
                ui.label(RichText::new(tr("Checking for updates…")).color(p.weak));
            } else if let Some(version) = self.available() {
                let text = trf("Version {version} is available.", &[("version", &version)]);
                if ui.link(RichText::new(text).color(p.accent)).clicked() {
                    self.dialog = true;
                }
            } else if self.latest {
                ui.label(
                    RichText::new(format!("{}  {}", icon::CHECK_CIRCLE, tr("You have the latest version.")))
                        .color(p.success),
                );
            } else if let Some(e) = &self.error
                && !self.dialog
            {
                ui.label(RichText::new(e).color(p.danger));
            }
        });
    }

    /// The update dialog, while open. `busy`: a scan or save is running.
    pub fn dialog(&mut self, ctx: &egui::Context, p: &Palette, busy: bool) {
        if !self.dialog {
            return;
        }
        let Some(release) = self.release.clone() else {
            self.dialog = false;
            return;
        };
        let modal = egui::Modal::new(egui::Id::new("update")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::ARROW_CIRCLE_UP).size(26.0).color(p.accent));
                let title = if self.ready.is_some() { tr("Update ready") } else { tr("Update available") };
                ui.label(theme::semibold(title, 18.0).color(p.text));
            });
            ui.add_space(8.0);
            theme::paragraph(
                ui,
                &trlf(
                    "Version {version} is available. You have version {current}.",
                    &[("version", &release.version), ("current", &current())],
                ),
                14.5,
                p.text,
            );
            ui.add_space(4.0);
            if !release.notes.trim().is_empty() {
                ui.label(theme::semibold(tr("What's new"), 15.0).color(p.accent));
                egui::ScrollArea::vertical().id_salt("update-notes").max_height(230.0).auto_shrink([false, true]).show(
                    ui,
                    |ui| notes_ui(ui, p, &release.notes),
                );
            }
            ui.hyperlink_to(icon_label(icon::LIST_BULLETS, "Open the release page"), &release.page);
            ui.add_space(8.0);

            // Downloaded and checked: nothing happens until the user says so. Installing
            // closes the app, which ends a scan that is running.
            if let Some((file, sha256)) = self.ready.clone() {
                theme::paragraph(
                    ui,
                    trl("The update is downloaded. Restart now to install it, or choose Update later and restart when it suits you."),
                    14.5,
                    p.text,
                );
                if busy {
                    ui.add_space(4.0);
                    theme::paragraph(ui, trl("Finish or stop the current scan or save first."), 14.5, p.warning);
                }
                if let Some(e) = &self.error {
                    ui.add_space(4.0);
                    theme::paragraph(ui, e, 14.5, p.danger);
                }
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::primary_button(ui, p, &format!("  {}  ", tr("Restart now")), !busy).clicked() {
                        self.error = None;
                        match install(&file, &sha256) {
                            Ok(()) => self.restart = true,
                            Err(e) => {
                                self.error = Some(trf(
                                    "The update could not be installed: {error}",
                                    &[("error", &format!("{e:#}"))],
                                ))
                            }
                        }
                    }
                    if theme::secondary_button(ui, tr("Update later")).clicked() {
                        self.dialog = false;
                    }
                });
                return;
            }

            if let Some(job) = &self.download {
                let state = job.progress.snapshot();
                let bar = match state.fraction() {
                    Some(f) => egui::ProgressBar::new(f).show_percentage(),
                    None => egui::ProgressBar::new(0.0).animate(true),
                };
                ui.add(bar);
                ui.label(
                    RichText::new(trf(
                        "Downloading… {done} of {total}",
                        &[("done", &format_size(state.done)), ("total", &format_size(state.total))],
                    ))
                    .color(p.weak),
                );
                ui.add_space(8.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        job.stop();
                    }
                });
                return;
            }

            if !release.installable() {
                theme::paragraph(
                    ui,
                    trl("This copy of the app was not installed with an installer, so it is updated by downloading the new version."),
                    14.5,
                    p.weak,
                );
            } else {
                theme::paragraph(ui, trl("The app will close, install the update and start again."), 14.5, p.weak);
                if cfg!(target_os = "macos") {
                    theme::paragraph(ui, trl("macOS will ask for Full Disk Access again after the update."), 14.5, p.weak);
                }
            }
            if busy {
                ui.add_space(4.0);
                theme::paragraph(ui, trl("Finish or stop the current scan or save first."), 14.5, p.warning);
            }
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                theme::paragraph(ui, e, 14.5, p.danger);
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if release.installable() {
                    if theme::primary_button(ui, p, &format!("  {}  ", tr("Update now")), !busy).clicked() {
                        self.error = None;
                        let r = release.clone();
                        self.download = Some(Job::spawn(ui.ctx(), move |progress, cancel| download(&r, progress, cancel)));
                    }
                } else if theme::primary_button(ui, p, &format!("  {}  ", tr("Open download page")), true).clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(&release.page));
                    self.dialog = false;
                }
                if theme::secondary_button(ui, tr("Later")).clicked() {
                    self.dialog = false;
                }
            });
        });
        // Closing from outside is "Later", except while downloading.
        if modal.should_close() && self.download.is_none() {
            self.dialog = false;
        }
    }
}

/// A line of release notes.
#[derive(Debug, PartialEq)]
enum Note {
    Heading(String),
    Bullet(String),
    Text(String),
    Gap,
}

/// Release notes (Markdown as GitHub shows it) as lines to draw: headings,
/// bullets and text, without the marks (`**`, backticks, links).
fn parse_notes(text: &str) -> Vec<Note> {
    let clean = |s: &str| {
        let mut out = s.replace("**", "").replace('`', "");
        // [text](address) → text
        while let (Some(a), Some(b)) = (out.find("]("), out.find('[')) {
            let Some(end) = out[a..].find(')') else { break };
            if b >= a {
                break;
            }
            let label = out[b + 1..a].to_string();
            out.replace_range(b..a + end + 1, &label);
        }
        out.trim().to_string()
    };
    let mut notes = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            if !matches!(notes.last(), Some(Note::Gap) | None) {
                notes.push(Note::Gap);
            }
        } else if let Some(h) = line.trim_start_matches('#').strip_prefix(' ').filter(|_| line.starts_with('#')) {
            // The version heading repeats what the dialog says.
            let h = clean(h);
            if !h.chars().next().is_some_and(|c| c.is_ascii_digit() || c == 'v') {
                notes.push(Note::Heading(h));
            }
        } else if let Some(b) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            notes.push(Note::Bullet(clean(b)));
        } else if let Some(Note::Bullet(last)) = notes.last_mut() {
            // A bullet written over several lines is one bullet.
            last.push(' ');
            last.push_str(&clean(line));
        } else {
            notes.push(Note::Text(clean(line)));
        }
    }
    notes
}

fn notes_ui(ui: &mut Ui, p: &Palette, text: &str) {
    for note in parse_notes(text) {
        match note {
            Note::Gap => ui.add_space(4.0),
            Note::Heading(h) => {
                ui.add_space(2.0);
                ui.label(theme::semibold(h, 14.5).color(p.text));
            }
            Note::Bullet(b) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("•").color(p.accent));
                    ui.add(egui::Label::new(RichText::new(b).color(p.text).size(13.5)).wrap());
                });
            }
            Note::Text(t) => {
                ui.add(egui::Label::new(RichText::new(t).color(p.weak).size(13.5)).wrap());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(is_newer("v0.3.10", "0.3.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("v0.3.1", "0.3.1"));
        assert!(!is_newer("0.3.0", "0.3.1"));
        assert!(!is_newer("garbage", "0.3.1"));
        assert_eq!(parse_version("v1.2.3-beta.1"), Some([1, 2, 3]));
    }

    #[test]
    fn too_many_requests_are_told_from_other_refusals() {
        // GitHub's answer with no requests left: 403 and the time the count starts again.
        assert_eq!(rate_limited(403, Some("0"), None, Some("1793360218"), 1000), Some(Some(1793360218)));
        // 429, or a Retry-After: wait that many seconds from now.
        assert_eq!(rate_limited(429, None, Some("90"), None, 1000), Some(Some(1090)));
        assert_eq!(rate_limited(403, Some("12"), Some("60"), None, 1000), Some(Some(1060)));
        // Too many, but no time said.
        assert_eq!(rate_limited(429, None, None, None, 1000), Some(None));
        // Another 403 (blocked, no right to it) or other errors are not about the count.
        assert_eq!(rate_limited(403, Some("12"), None, Some("1793360218"), 1000), None);
        assert_eq!(rate_limited(403, None, None, None, 1000), None);
        assert_eq!(rate_limited(404, Some("0"), None, None, 1000), None);
        assert_eq!(rate_limited(500, None, Some("5"), None, 1000), None);
    }

    #[test]
    fn installers_match_the_release_files() {
        let names: Vec<String> =
            [Kind::Setup, Kind::Bundle(PathBuf::new()), Kind::AppImage(PathBuf::new()), Kind::Deb, Kind::Rpm]
                .iter()
                .filter_map(asset_name)
                .collect();
        let workflow = include_str!("../../.github/workflows/release.yml");
        for name in names {
            // The workflow names them wdfr-<label>(-setup).<ext>.
            let label = name.trim_start_matches("wdfr-").split(['.']).next().unwrap().trim_end_matches("-setup");
            assert!(workflow.contains(&format!("label: {label} ")), "{name}");
        }
        assert_eq!(asset_name(&Kind::Portable), None);
    }
}
