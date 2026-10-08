//! The start screen: choose a drive, what to look for, and how deep.

use std::sync::Arc;

use eframe::egui::{self, Align, Layout, RichText, Ui, Vec2};
use egui_phosphor::regular as icon;
use wdfr::carve::Category;
use wdfr::devices::{self, Device, DeviceKind};
use wdfr::recover::Method;
use wdfr::source::{DiskSource, Source, SubSource};
use wdfr::units::format_size;

use crate::elevate;
use crate::i18n::{icon_label, tr, trf, trl};
use crate::jobs::Job;
use crate::results::{category_icon, category_label};
use crate::settings::Settings;
use crate::theme::{self, Palette};

pub struct DriveInfo {
    pub device: Device,
    /// "NTFS", "exFAT", "2 partitions: NTFS, FAT32", ...
    pub fs: String,
}

/// `os_fs` is the file system the OS reports, used when ours cannot read it
/// (e.g. APFS on a Mac, which only the deep search can scan).
fn describe(path: &str, os_fs: Option<&str>) -> String {
    let Ok(d) = DiskSource::open(path) else { return String::new() };
    let disk: Source = Arc::new(d);
    let parts = wdfr::partition::discover(&disk);
    let unknown = || match os_fs {
        Some(fs) => trf("{fs} · deep search only", &[("fs", &fs)]),
        None => tr("Unknown").to_string(),
    };
    let fs: Vec<String> = parts
        .iter()
        .map(|p| match p.fs {
            Some(f) => f.to_string(),
            None if p.locked(&disk) => tr("BitLocker · locked").to_string(),
            None => unknown(),
        })
        .collect();
    match fs.len() {
        0 => String::new(),
        1 => fs[0].clone(),
        n => trf("{n} partitions: {list}", &[("n", &n), ("list", &fs.join(", "))]),
    }
}

/// Where the partitions of `path` that BitLocker keeps locked are.
fn locked_partitions(path: &str) -> Vec<(u64, u64)> {
    let Ok(d) = DiskSource::open(path) else { return Vec::new() };
    let disk: Source = Arc::new(d);
    let parts = wdfr::partition::discover(&disk);
    parts.iter().filter(|p| p.locked(&disk)).map(|p| (p.start, p.len)).collect()
}

/// Tries a recovery key or password on the locked partitions of `path`.
/// True if it opened one (it is then used whenever the drive is read).
fn unlock(path: &str, key: &str) -> anyhow::Result<bool> {
    let disk: Source = Arc::new(DiskSource::open(path)?);
    let mut opened = false;
    for (start, len) in locked_partitions(path) {
        let raw: Source = Arc::new(SubSource::new(disk.clone(), start, len));
        match wdfr::bitlocker::add_key(raw, key) {
            Ok(()) => opened = true,
            Err(e) => log::info!("BitLocker at {start:#x}: {e:#}"),
        }
    }
    Ok(opened)
}

pub fn discover() -> Vec<DriveInfo> {
    devices::list()
        .into_iter()
        .map(|device| {
            let fs = if device.size.is_some() { describe(&device.path, device.fs.as_deref()) } else { String::new() };
            DriveInfo { device, fs }
        })
        .collect()
}

pub enum Action {
    None,
    Scan,
    /// Restart the app with administrator rights (macOS, Linux).
    Elevate,
    /// Open the results of an earlier scan.
    OpenScan(std::path::PathBuf),
    /// Copy the selected drive into this image file.
    CopyToImage(std::path::PathBuf),
}

pub struct Home {
    pub drives: Option<Vec<DriveInfo>>,
    drives_job: Option<Job<Vec<DriveInfo>>>,
    pub selected: Option<String>,
    pub images: Vec<String>,
    pub categories: Vec<Category>,
    pub method: Method,
    /// Only files in this folder (empty: everywhere).
    pub folder: String,
    /// A restart with administrator rights is waiting for the password.
    pub restarting: bool,
    elevate: bool,
    open_scan: Option<std::path::PathBuf>,
    /// The source the BitLocker state below is for, and its locked partitions.
    bitlocker: Option<(String, Vec<(u64, u64)>)>,
    bitlocker_key: String,
    unlock_job: Option<Job<bool>>,
    /// The last key tried did not open the drive.
    unlock_failed: bool,
    /// The source a key has just opened.
    unlocked: Option<String>,
}

impl Home {
    pub fn new(ctx: &egui::Context, s: &Settings) -> Self {
        let mut h = Self {
            drives: None,
            drives_job: None,
            selected: None,
            images: Vec::new(),
            categories: s.categories.clone(),
            method: s.method,
            folder: String::new(),
            restarting: false,
            elevate: false,
            open_scan: None,
            bitlocker: None,
            bitlocker_key: String::new(),
            unlock_job: None,
            unlock_failed: false,
            unlocked: None,
        };
        h.refresh(ctx);
        h
    }

    pub fn refresh(&mut self, ctx: &egui::Context) {
        self.drives_job = Some(Job::spawn(ctx, |_, _| Ok(discover())));
    }

    pub fn poll(&mut self) {
        if let Some(r) = self.drives_job.as_ref().and_then(Job::poll) {
            self.drives = Some(r.unwrap_or_default());
            self.drives_job = None;
        }
    }

    pub fn add_image(&mut self, path: String) {
        if !self.images.contains(&path) {
            self.images.push(path.clone());
        }
        self.selected = Some(path);
    }

    /// Human name of the selected source.
    pub fn source_name(&self) -> String {
        let Some(sel) = &self.selected else { return String::new() };
        self.drives
            .iter()
            .flatten()
            .find(|d| &d.device.path == sel)
            .map(|d| d.device.display_name())
            .unwrap_or_else(|| file_name(sel))
    }

    pub fn page(&mut self, ui: &mut Ui, p: &Palette, s: &Settings) -> Action {
        let mut action = Action::None;
        theme::page_title(
            ui,
            p,
            tr("Recover deleted files"),
            tr("Choose the drive the files were deleted from. Nothing is written to it."),
        );
        // The start button stays visible however long the drive list is.
        egui::Panel::bottom("start-bar")
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 0, top: 14, bottom: 0 }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let ready = self.selected.is_some();
                    let label = icon_label(icon::MAGNIFYING_GLASS, "Start scan");
                    if theme::primary_button(ui, p, &label, ready).clicked() {
                        action = Action::Scan;
                    }
                    // Only drives can be copied; an image already is a copy.
                    let drive = self.selected.as_ref().filter(|sel| !self.images.contains(sel));
                    if drive.is_some()
                        && theme::secondary_button(ui, &icon_label(icon::COPY, "Copy to an image…"))
                            .on_hover_text(tr(
                                "For failing drives: reads the drive once, damaged areas last, into an image file. Then scan the image instead of the drive.",
                            ))
                            .clicked()
                    {
                        let name = format!("{}.img", crate::app::file_safe(&self.source_name()));
                        if let Some(out) = rfd::FileDialog::new()
                            .set_title(tr("Save the copy of the drive"))
                            .set_file_name(name)
                            .add_filter(trl("Disk images"), &["img"])
                            .save_file()
                        {
                            action = Action::CopyToImage(out);
                        }
                    }
                    ui.add_space(8.0);
                    let hint = if ready {
                        trf("Ready to scan {name}", &[("name", &self.source_name())])
                    } else {
                        tr("Select a drive or disk image first").into()
                    };
                    ui.label(RichText::new(hint).color(p.weak));
                });
            });
        if std::mem::take(&mut self.elevate) {
            action = Action::Elevate;
        }
        if let Some(path) = self.open_scan.take() {
            action = Action::OpenScan(path);
        }
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            self.drives_card(ui, p, s);
            ui.add_space(14.0);
            self.types_card(ui, p);
            ui.add_space(14.0);
            self.mode_card(ui, p);
            ui.add_space(8.0);
        });
        action
    }

    fn drives_card(&mut self, ui: &mut Ui, p: &Palette, s: &Settings) {
        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::section_title(ui, p, icon::HARD_DRIVES, tr("1. Choose a drive"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(icon_label(icon::ARROWS_CLOCKWISE, "Refresh")).clicked() {
                        self.refresh(ui.ctx());
                    }
                    if ui.button(icon_label(icon::CLOCK_COUNTER_CLOCKWISE, "Open saved scan…")).clicked()
                        && let Some(f) =
                            rfd::FileDialog::new().add_filter(trl("Saved scans"), &[wdfr::saved::EXTENSION]).pick_file()
                    {
                        self.open_scan = Some(f);
                    }
                    if ui.button(icon_label(icon::FILE_PLUS, "Open disk image…")).clicked()
                        && let Some(f) = rfd::FileDialog::new()
                            .add_filter(trl("Disk images"), &["img", "dd", "raw", "bin", "iso", "dmg", "vhd", "001"])
                            .add_filter(trl("All files"), &["*"])
                            .pick_file()
                    {
                        self.add_image(f.display().to_string());
                    }
                });
            });
            ui.add_space(8.0);
            let Some(drives) = &self.drives else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(tr("Looking for drives…")).color(p.weak));
                });
                return;
            };
            let shown: Vec<&DriveInfo> =
                drives.iter().filter(|d| s.show_whole_disks || d.device.kind != DeviceKind::Disk).collect();
            // With administrator rights, macOS still refuses to read drives
            // until the app has Full Disk Access.
            let needs_full_disk_access =
                cfg!(target_os = "macos") && (elevate::is_root() || elevate::has_drive_access());
            // A disk image or card the user mounted can be readable while the
            // drives are not, so on macOS and Linux access is offered as soon
            // as one drive is locked. (On Windows the app always runs as
            // administrator.)
            let unreadable = |d: &&DriveInfo| d.device.size.is_none();
            let locked = if cfg!(windows) { shown.iter().all(unreadable) } else { shown.iter().any(unreadable) };
            if needs_full_disk_access && locked {
                theme::notice(
                    ui,
                    p,
                    p.warning,
                    icon::WARNING,
                    trl(
                        "macOS also needs Full Disk Access to read drives: turn on Deleted Files Recovery in System Settings → Privacy & Security → Full Disk Access, then click Refresh.",
                    ),
                );
                ui.add_space(8.0);
                if theme::primary_button(ui, p, &icon_label(icon::GEAR_SIX, "Open Full Disk Access settings"), true)
                    .clicked()
                {
                    elevate::open_full_disk_access_settings();
                }
                ui.add_space(8.0);
            } else if locked {
                let msg = if cfg!(windows) {
                    trl(
                        "No drive could be opened. Close the app, right-click it and choose \"Run as administrator\". Disk images work without it.",
                    )
                } else {
                    trl("Reading drives needs administrator rights. Disk images work without them.")
                };
                theme::notice(ui, p, p.warning, icon::WARNING, msg);
                ui.add_space(8.0);
                if elevate::supported() && !elevate::is_root() && !elevate::has_drive_access() {
                    ui.horizontal(|ui| {
                        let label = if cfg!(target_os = "macos") {
                            icon_label(icon::SHIELD_CHECK, "Allow access to drives")
                        } else {
                            icon_label(icon::SHIELD_CHECK, "Restart with administrator rights")
                        };
                        if theme::primary_button(ui, p, &label, !self.restarting).clicked() {
                            self.elevate = true;
                        }
                        if self.restarting {
                            ui.spinner();
                            ui.label(RichText::new(tr("Waiting for the password…")).color(p.weak));
                        }
                    });
                    ui.add_space(8.0);
                }
            }
            let mut clicked = None;
            // (path, icon, title, detail, readable): opened images first, then drives.
            let mut cards: Vec<(String, &str, String, String, bool)> = Vec::new();
            for img in &self.images {
                let size = std::fs::metadata(img).map(|m| format_size(m.len())).unwrap_or_default();
                cards.push((
                    img.clone(),
                    icon::FILE_DASHED,
                    file_name(img),
                    trf("Disk image · {size}", &[("size", &size)]),
                    true,
                ));
            }
            for d in &shown {
                let dev = &d.device;
                let ok = dev.size.is_some();
                let glyph = match dev.kind {
                    DeviceKind::Removable => icon::USB,
                    DeviceKind::Disk => icon::HARD_DRIVES,
                    DeviceKind::Volume => icon::HARD_DRIVE,
                };
                let detail = match dev.size {
                    Some(sz) if d.fs.is_empty() => format_size(sz),
                    Some(sz) => format!("{} · {}", format_size(sz), d.fs),
                    None => tr("Needs administrator rights").into(),
                };
                cards.push((dev.path.clone(), glyph, dev.display_name(), detail, ok));
            }
            const CARD_W: f32 = 236.0;
            let gap = 10.0;
            let per_row = (((ui.available_width() + gap) / (CARD_W + 30.0 + gap)).floor() as usize).max(1);
            for row in cards.chunks(per_row) {
                // Top-aligned: cards with Arabic text are slightly taller.
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (path, glyph, title, detail, ok) in row {
                        let sel = self.selected.as_deref() == Some(path.as_str());
                        if drive_card(ui, p, egui::Id::new(("source", path)), sel, *ok, glyph, title, detail).clicked()
                        {
                            clicked = Some(path.clone());
                        }
                    }
                });
                ui.add_space(gap - ui.spacing().item_spacing.y);
            }
            if let Some(c) = clicked {
                self.selected = Some(c);
            }
            self.bitlocker_box(ui, p);
            ui.add_space(6.0);
            ui.label(
                RichText::new(icon_label(icon::INFO, "Tip: you can also drag a disk image file onto this window."))
                    .color(p.weak)
                    .size(12.5),
            );
        });
    }

    /// Tries `key` on the selected drive, in the background.
    pub fn try_unlock(&mut self, ctx: &egui::Context, key: String) {
        let Some(path) = self.selected.clone() else { return };
        self.bitlocker_key = key.clone();
        self.unlock_failed = false;
        self.unlock_job = Some(Job::spawn(ctx, move |_, _| unlock(&path, &key)));
    }

    /// Asks for the recovery key when the selected drive is locked with
    /// BitLocker.
    fn bitlocker_box(&mut self, ui: &mut Ui, p: &Palette) {
        let Some(sel) = self.selected.clone() else { return };
        if self.bitlocker.as_ref().is_none_or(|(path, _)| *path != sel) {
            self.bitlocker = Some((sel.clone(), locked_partitions(&sel)));
            self.bitlocker_key.clear();
            self.unlock_failed = false;
        }
        if let Some(r) = self.unlock_job.as_ref().and_then(Job::poll) {
            self.unlock_job = None;
            if matches!(r, Ok(true)) {
                self.unlocked = Some(sel.clone());
                self.bitlocker = Some((sel.clone(), locked_partitions(&sel)));
                self.bitlocker_key.clear();
                self.refresh(ui.ctx());
            } else {
                self.unlock_failed = true;
            }
        }
        if self.bitlocker.as_ref().is_none_or(|(_, locked)| locked.is_empty()) {
            if self.unlocked.as_ref() == Some(&sel) {
                ui.add_space(4.0);
                theme::notice(
                    ui,
                    p,
                    p.success,
                    icon::LOCK_KEY_OPEN,
                    trl("Unlocked. The drive is read decrypted; nothing on it is changed."),
                );
                ui.add_space(4.0);
            }
            return;
        }
        ui.add_space(4.0);
        theme::notice(
            ui,
            p,
            p.warning,
            icon::LOCK,
            trl(
                "This drive is locked with BitLocker. Enter its 48-digit recovery key or its password to find its deleted files. The recovery key is in your Microsoft account (aka.ms/myrecoverykey), on a printout or on a USB stick.",
            ),
        );
        ui.add_space(8.0);
        let busy = self.unlock_job.is_some();
        ui.horizontal(|ui| {
            let field = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut self.bitlocker_key)
                    .hint_text(tr("Recovery key or password"))
                    .desired_width(420.0),
            );
            if field.changed() {
                self.unlock_failed = false;
            }
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let ready = !busy && !self.bitlocker_key.trim().is_empty();
            if (theme::primary_button(ui, p, &icon_label(icon::LOCK_KEY_OPEN, "Unlock"), ready).clicked() || enter)
                && ready
            {
                let key = self.bitlocker_key.trim().to_string();
                self.try_unlock(ui.ctx(), key);
            }
            if busy {
                ui.spinner();
                ui.label(RichText::new(tr("Unlocking…")).color(p.weak));
            }
        });
        if self.unlock_failed && !busy {
            ui.add_space(4.0);
            ui.label(RichText::new(tr("This recovery key or password does not open the drive.")).color(p.danger));
        }
        ui.add_space(4.0);
    }

    fn types_card(&mut self, ui: &mut Ui, p: &Palette) {
        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, p, icon::FUNNEL, tr("2. What are you looking for?"));
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                let everything = self.categories.is_empty();
                if chip(ui, p, everything, &icon_label(icon::SQUARES_FOUR, "Everything")).clicked() {
                    self.categories.clear();
                }
                for (slot, c) in Category::ALL.iter().enumerate() {
                    let on = self.categories.contains(c);
                    if chip(
                        ui,
                        p,
                        on,
                        &crate::i18n::visual(&format!("{} {}", category_icon(Some(*c)), category_label(slot))),
                    )
                    .clicked()
                    {
                        if on {
                            self.categories.retain(|x| x != c);
                        } else {
                            self.categories.push(*c);
                        }
                    }
                }
            });
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(icon_label(icon::FOLDER, "Only in this folder:")).color(p.weak));
                ui.add(
                    egui::TextEdit::singleline(&mut self.folder)
                        .hint_text(tr("everywhere (for example Users/Ann/Pictures)"))
                        .desired_width(320.0),
                )
                .on_hover_text(tr(
                    "Faster to look through: only files that were in this folder, or below it, are listed.",
                ));
            });
        });
    }

    fn mode_card(&mut self, ui: &mut Ui, p: &Palette) {
        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, p, icon::GAUGE, tr("3. How deep should I search?"));
            ui.add_space(8.0);
            let modes = [
                (
                    Method::Fs,
                    icon::LIGHTNING,
                    tr("Quick"),
                    trl("Deleted files that still have their names and folders. Takes seconds to minutes."),
                ),
                (
                    Method::All,
                    icon::SPARKLE,
                    tr("Recommended"),
                    trl("Names and folders first, then a deep search of free space for everything else."),
                ),
                (
                    Method::Carve,
                    icon::MAGNIFYING_GLASS,
                    tr("Formatted drive"),
                    trl("For formatted or corrupted drives. Finds files by their content only."),
                ),
            ];
            ui.columns(3, |cols| {
                for (col, (m, glyph, title, text)) in cols.iter_mut().zip(modes) {
                    let sel = self.method == m;
                    let r = theme::selectable_card(col, p, egui::Id::new(("mode", title)), sel, true, |ui| {
                        ui.set_width(ui.available_width());
                        ui.set_min_height(92.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(glyph).size(20.0).color(p.accent));
                            ui.label(theme::semibold(title, 15.5).color(p.text));
                            if m == Method::All {
                                theme::pill(ui, p, tr("Best"), p.success);
                            }
                        });
                        theme::paragraph(ui, text, 12.5, p.weak);
                    });
                    if r.clicked() {
                        self.method = m;
                    }
                }
            });
        });
    }
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

#[allow(clippy::too_many_arguments)]
fn drive_card(
    ui: &mut Ui,
    p: &Palette,
    id: egui::Id,
    selected: bool,
    enabled: bool,
    glyph: &str,
    title: &str,
    detail: &str,
) -> egui::Response {
    theme::selectable_card(ui, p, id, selected, enabled, |ui| {
        ui.set_width(236.0);
        ui.set_height(48.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(glyph).size(30.0).color(if enabled { p.accent } else { p.weak }));
            ui.vertical(|ui| {
                ui.add(egui::Label::new(theme::semibold(title, 15.0).color(p.text)).truncate());
                ui.add(
                    egui::Label::new(RichText::new(detail).color(if enabled { p.weak } else { p.warning }).size(12.5))
                        .truncate(),
                );
            });
        });
    })
}

fn chip(ui: &mut Ui, p: &Palette, on: bool, text: &str) -> egui::Response {
    let (fill, stroke, color) = if on {
        (p.tint(p.accent), egui::Stroke::new(1.5, p.accent), p.text)
    } else {
        (p.card_alt, egui::Stroke::new(1.0, p.border), p.weak)
    };
    ui.add(
        egui::Button::new(RichText::new(text).color(color).size(14.0))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(255)
            .min_size(Vec2::new(0.0, 34.0)),
    )
}
