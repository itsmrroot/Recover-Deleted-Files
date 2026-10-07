//! Progress, completion and About screens.

use std::path::Path;
use std::time::Duration;

use eframe::egui::{self, Align, Layout, RichText, Ui, Vec2};
use egui_phosphor::regular as icon;
use wdfr::carve::Category;
use wdfr::progress::Unit;
use wdfr::recover::Summary;
use wdfr::units::format_size;

use crate::i18n::{icon_label, tr, trf, trl, trn, visual};
use crate::jobs::{ProgressState, format_duration};
use crate::results::{category_color, category_icon, category_label};
use crate::theme::{self, Palette};

pub const POWERED_BY: &str = "Powered by Bashar Salmo";
pub const REPO: &str = "https://github.com/itsmrroot/Recover-Deleted-Files";

fn amount(unit: Unit, n: u64) -> String {
    match unit {
        Unit::Bytes => format_size(n),
        Unit::Items => n.to_string(),
    }
}

/// The current step of a scan or save. The library names its steps in
/// English: "Reading NTFS file system", "Deep search", "Recovering files".
fn task_label(task: &str) -> String {
    if let Some(fs) = task.strip_prefix("Reading ").and_then(|t| t.strip_suffix(" file system")) {
        return trf("Reading the {fs} file system", &[("fs", &fs)]);
    }
    match task {
        "Deep search" => tr("Deep search").into(),
        "Recovering files" => tr("Recovering files").into(),
        "Checking for duplicates" => tr("Checking for duplicates").into(),
        "Opening saved scan" => tr("Opening saved scan").into(),
        "Copying the drive" => tr("Copying the drive").into(),
        "Rebuilding lost partitions" => tr("Looking for lost partitions and old file tables").into(),
        "Rebuilding fragmented videos" => tr("Putting videos stored in pieces back together").into(),
        "Retrying damaged areas" => tr("Retrying damaged areas").into(),
        "Starting..." => tr("Starting…").into(),
        other => other.into(),
    }
}

/// What a progress screen is about.
pub struct ProgressInfo<'a> {
    pub title: &'a str,
    pub subtitle: &'a str,
    /// Show the found-files counters (scans, not saves).
    pub show_found: bool,
}

/// Live progress of a scan or a save. Returns true when Stop is clicked.
pub fn progress(
    ui: &mut Ui,
    p: &Palette,
    info: &ProgressInfo,
    st: &ProgressState,
    elapsed: Duration,
    stopping: bool,
) -> bool {
    let mut stop = false;
    let show_found = info.show_found;
    theme::page_title(ui, p, info.title, info.subtitle);
    theme::card(ui, p, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(22.0).color(p.accent));
            ui.label(theme::semibold(task_label(&st.task), 18.0).color(p.text));
        });
        ui.add_space(10.0);
        let bar = match st.fraction() {
            Some(f) => egui::ProgressBar::new(f).show_percentage(),
            None => egui::ProgressBar::new(0.0).animate(true),
        };
        ui.add(bar.desired_height(14.0).corner_radius(7).fill(p.accent));
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(&st.item).color(p.weak).size(12.5)).truncate());
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 46.0;
            let progress = if st.total > 0 {
                trf("{done} of {total}", &[("done", &amount(st.unit, st.done)), ("total", &amount(st.unit, st.total))])
            } else {
                amount(st.unit, st.done)
            };
            theme::stat(ui, p, tr("Progress"), &progress);
            let speed = match st.unit {
                Unit::Bytes if st.rate > 0.0 => format!("{}/s", format_size(st.rate as u64)),
                Unit::Items if st.rate > 0.0 => trf("{n} files/s", &[("n", &format!("{:.0}", st.rate))]),
                _ => "—".into(),
            };
            theme::stat(ui, p, tr("Speed"), &speed);
            theme::stat(ui, p, tr("Time left"), &st.eta().map_or_else(|| "—".into(), format_duration));
            theme::stat(ui, p, tr("Elapsed"), &format_duration(elapsed));
            if show_found {
                theme::stat(ui, p, tr("Files found"), &st.found.to_string());
            }
        });
        if show_found && st.found > 0 {
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                for slot in 0..7 {
                    let n = st.by_category[slot];
                    if n > 0 {
                        let c = Category::ALL.get(slot).copied();
                        theme::pill(
                            ui,
                            p,
                            &visual(&format!("{} {} {n}", category_icon(c), category_label(slot))),
                            category_color(p, slot),
                        );
                    }
                }
            });
        }
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            if stopping {
                ui.add(egui::Spinner::new().size(16.0));
                ui.label(RichText::new(tr("Stopping… everything found so far is kept.")).color(p.weak));
            } else if theme::danger_button(ui, p, &icon_label(icon::STOP, "Stop")).clicked() {
                stop = true;
            }
        });
    });
    if !st.warnings.is_empty() {
        ui.add_space(12.0);
        egui::CollapsingHeader::new(
            RichText::new(format!("{} {}", icon::WARNING, trn(st.warnings.len() as u64, "1 warning", "{n} warnings")))
                .color(p.warning),
        )
        .id_salt("warnings")
        .show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                for w in &st.warnings {
                    ui.label(RichText::new(w).color(p.weak).size(12.5));
                }
            });
        });
    }
    ui.add_space(12.0);
    theme::notice(
        ui,
        p,
        p.accent,
        icon::INFO,
        trl(
            "Please don't use the drive while this runs: new files saved on it can overwrite what you are trying to recover.",
        ),
    );
    stop
}

pub enum ImageAction {
    None,
    ScanCopy,
    Back,
}

/// After copying a drive into an image file.
pub fn image_done(ui: &mut Ui, p: &Palette, st: &wdfr::imaging::Stats, out: &Path) -> ImageAction {
    let mut action = ImageAction::None;
    theme::card(ui, p, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (glyph, color, title) = if st.cancelled {
                (icon::PAUSE_CIRCLE, p.warning, tr("The copy was stopped"))
            } else {
                (icon::CHECK_CIRCLE, p.success, tr("The drive was copied"))
            };
            ui.label(RichText::new(glyph).size(30.0).color(color));
            ui.label(theme::semibold(title, 22.0).color(p.text));
        });
        ui.add_space(10.0);
        let copied = trf(
            "Copied {copied} of {size} into {file}.",
            &[("copied", &format_size(st.copied)), ("size", &format_size(st.size)), ("file", &out.display())],
        );
        theme::paragraph(ui, &copied, 14.5, p.text);
        if st.unreadable > 0 {
            let bad = trf(
                "{bad} could not be read; those parts are zeros in the copy.",
                &[("bad", &format_size(st.unreadable))],
            );
            theme::paragraph(ui, &bad, 14.5, p.warning);
        }
        if st.cancelled {
            theme::paragraph(
                ui,
                trl("To continue where it stopped, copy the drive again into the same file."),
                14.5,
                p.weak,
            );
        } else {
            theme::paragraph(
                ui,
                trl("Scan the copy now: the drive is not needed any more, so it is not worn out further."),
                14.5,
                p.weak,
            );
        }
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if theme::primary_button(ui, p, &icon_label(icon::MAGNIFYING_GLASS, "Scan the copy"), true).clicked() {
                action = ImageAction::ScanCopy;
            }
            if theme::secondary_button(ui, &icon_label(icon::HOUSE, "Back to the start screen")).clicked() {
                action = ImageAction::Back;
            }
        });
    });
    action
}

pub enum DoneAction {
    None,
    OpenFolder,
    OpenReport,
    BackToResults,
    NewScan,
}

pub fn done(ui: &mut Ui, p: &Palette, sum: &Summary, out: &Path) -> DoneAction {
    let mut action = DoneAction::None;
    let files = sum.fs_files + sum.carved_files;
    let bytes = sum.fs_bytes + sum.carved_bytes;
    ui.add_space(20.0);
    theme::card(ui, p, |ui| {
        ui.set_width(ui.available_width());
        ui.vertical_centered(|ui| {
            ui.add_space(16.0);
            let (glyph, color, title) = if sum.cancelled {
                (icon::WARNING_CIRCLE, p.warning, tr("Recovery stopped"))
            } else if files == 0 && sum.failures > 0 {
                (icon::X_CIRCLE, p.danger, tr("Nothing could be recovered"))
            } else {
                (icon::CHECK_CIRCLE, p.success, tr("Recovery complete"))
            };
            ui.label(RichText::new(glyph).size(64.0).color(color));
            ui.label(theme::semibold(title, 26.0).color(p.text));
            ui.label(
                RichText::new(if files == 1 {
                    trf("1 file · {size} saved to", &[("size", &format_size(bytes))])
                } else {
                    trf("{n} files · {size} saved to", &[("n", &files), ("size", &format_size(bytes))])
                })
                .color(p.weak)
                .size(15.0),
            );
            ui.label(RichText::new(out.display().to_string()).color(p.text).size(14.0));
            ui.add_space(18.0);
        });
        ui.columns(4, |cols| {
            for (col, (label, value)) in cols.iter_mut().zip([
                (tr("With original names"), sum.fs_files.to_string()),
                (tr("Found by content"), sum.carved_files.to_string()),
                (tr("Could not be saved"), sum.failures.to_string()),
                (
                    tr("Unreadable data"),
                    if sum.unreadable_bytes > 0 { format_size(sum.unreadable_bytes) } else { tr("None").into() },
                ),
            ]) {
                col.vertical_centered(|ui| {
                    ui.label(RichText::new(label).size(12.5).color(p.weak));
                    ui.label(theme::semibold(value, 20.0).color(p.text));
                });
            }
        });
        ui.add_space(18.0);
        // Centred when the buttons fit on one line (their width is measured
        // in the previous frame), otherwise wrapped onto more lines.
        let width_id = ui.id().with("done-buttons-width");
        let width = ui.ctx().data(|d| d.get_temp::<f32>(width_id)).unwrap_or(640.0);
        let fits = width <= ui.available_width();
        let row = |ui: &mut Ui, add: &mut dyn FnMut(&mut Ui)| {
            if fits {
                ui.horizontal(|ui| {
                    ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                    add(ui)
                })
                .response
            } else {
                ui.horizontal_wrapped(|ui| add(ui)).response
            }
        };
        let mut used = 0.0;
        row(ui, &mut |ui: &mut Ui| {
            let start = ui.cursor().min.x;
            if theme::primary_button(ui, p, &icon_label(icon::FOLDER_OPEN, "Open folder"), true).clicked() {
                action = DoneAction::OpenFolder;
            }
            if sum.report.is_some() && theme::secondary_button(ui, &icon_label(icon::FILE_CSV, "Open report")).clicked()
            {
                action = DoneAction::OpenReport;
            }
            if theme::secondary_button(ui, &icon_label(icon::ARROW_LEFT, "Back to results")).clicked() {
                action = DoneAction::BackToResults;
            }
            if theme::secondary_button(ui, &icon_label(icon::ARROW_COUNTER_CLOCKWISE, "New scan")).clicked() {
                action = DoneAction::NewScan;
            }
            used = ui.min_rect().right() - start;
        });
        if fits {
            ui.ctx().data_mut(|d| d.insert_temp(width_id, used));
        }
        ui.add_space(12.0);
    });
    if sum.failures > 0 {
        ui.add_space(12.0);
        theme::notice(
            ui,
            p,
            p.warning,
            icon::WARNING,
            trl(
                "Some files could not be written. Check that the destination drive has enough free space and that you can write to it.",
            ),
        );
    }
    action
}

pub fn about(ui: &mut Ui, p: &Palette, logo: &egui::TextureHandle, updater: &mut crate::update::Updater) {
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::splat(88.0)));
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.add_space(6.0);
                    ui.label(theme::semibold("Deleted Files Recovery", 24.0).color(p.text));
                    ui.label(RichText::new(trf("Version {version}", &[("version", &env!("CARGO_PKG_VERSION"))])).color(p.weak));
                    ui.add_space(4.0);
                    theme::pill(ui, p, POWERED_BY, p.accent);
                });
            });
            ui.add_space(10.0);
            updater.status(ui, p);
            ui.add_space(12.0);
            theme::paragraph(
                ui,
                trl("Recovers deleted photos, videos, music, documents and more from NTFS, FAT32 and exFAT drives, USB sticks, memory cards and disk images. Drives are only ever read, never written."),
                14.5,
                p.text,
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.hyperlink_to(icon_label(icon::GITHUB_LOGO, "Project page"), REPO);
                ui.add_space(12.0);
                ui.hyperlink_to(icon_label(icon::DOWNLOAD_SIMPLE, "Latest version"), format!("{REPO}/releases/latest"));
                ui.add_space(12.0);
                ui.hyperlink_to(icon_label(icon::SCALES, "MIT License"), format!("{REPO}/blob/main/LICENSE"));
            });
        });
        ui.add_space(14.0);
        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, p, icon::LIFEBUOY, tr("Tips for a successful recovery"));
            ui.add_space(4.0);
            for tip in [
                trl("Stop using the drive right away. Every new file can overwrite deleted ones."),
                trl("Never save recovered files to the same drive you are recovering from."),
                trl("Memory cards, USB sticks and hard drives usually recover well."),
                trl("SSDs often erase deleted files automatically (TRIM), so recovery may be impossible."),
                trl("If the drive makes noises or is very slow, copy it to a disk image first and scan the image."),
                trl("No luck with Quick or Recommended? A deep search finds files even after formatting."),
            ] {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::CHECK).color(p.success));
                    theme::paragraph(ui, tip, 14.5, p.text);
                });
            }
        });
        ui.add_space(14.0);
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            ui.label(RichText::new(format!("© 2026 Bashar Salmo · {POWERED_BY}")).color(p.weak).size(12.5));
        });
    });
}
