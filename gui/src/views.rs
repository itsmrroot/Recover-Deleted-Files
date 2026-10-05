//! Progress, completion and About screens.

use std::path::Path;
use std::time::Duration;

use eframe::egui::{self, Align, Layout, RichText, Ui, Vec2};
use egui_phosphor::regular as icon;
use wdfr::carve::Category;
use wdfr::progress::Unit;
use wdfr::recover::Summary;
use wdfr::units::format_size;

use crate::jobs::{ProgressState, format_duration};
use crate::results::{category_color, category_icon, category_label};
use crate::theme::{self, Palette};

pub const POWERED_BY: &str = "Powered by Bashar Salmo";
pub const REPO: &str = "https://github.com/itsmrroot/Windows-Deleted-Files-Recovery";

fn amount(unit: Unit, n: u64) -> String {
    match unit {
        Unit::Bytes => format_size(n),
        Unit::Items => n.to_string(),
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
            ui.label(theme::semibold(&st.task, 18.0).color(p.text));
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
                format!("{} of {}", amount(st.unit, st.done), amount(st.unit, st.total))
            } else {
                amount(st.unit, st.done)
            };
            theme::stat(ui, p, "Progress", &progress);
            let speed = match st.unit {
                Unit::Bytes if st.rate > 0.0 => format!("{}/s", format_size(st.rate as u64)),
                Unit::Items if st.rate > 0.0 => format!("{:.0} /s", st.rate),
                _ => "—".into(),
            };
            theme::stat(ui, p, "Speed", &speed);
            theme::stat(ui, p, "Time left", &st.eta().map_or_else(|| "—".into(), format_duration));
            theme::stat(ui, p, "Elapsed", &format_duration(elapsed));
            if show_found {
                theme::stat(ui, p, "Files found", &st.found.to_string());
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
                            &format!("{} {} {n}", category_icon(c), category_label(slot)),
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
                ui.label(RichText::new("Stopping… everything found so far is kept.").color(p.weak));
            } else if theme::danger_button(ui, p, &format!("{} Stop", icon::STOP)).clicked() {
                stop = true;
            }
        });
    });
    if !st.warnings.is_empty() {
        ui.add_space(12.0);
        egui::CollapsingHeader::new(
            RichText::new(format!("{} {} warnings", icon::WARNING, st.warnings.len())).color(p.warning),
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
        "Please don't use the drive while this runs: new files saved on it can overwrite what you are trying to recover.",
    );
    stop
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
                (icon::WARNING_CIRCLE, p.warning, "Recovery stopped")
            } else if files == 0 && sum.failures > 0 {
                (icon::X_CIRCLE, p.danger, "Nothing could be recovered")
            } else {
                (icon::CHECK_CIRCLE, p.success, "Recovery complete")
            };
            ui.label(RichText::new(glyph).size(64.0).color(color));
            ui.label(theme::semibold(title, 26.0).color(p.text));
            ui.label(
                RichText::new(format!("{files} files · {} saved to", format_size(bytes))).color(p.weak).size(15.0),
            );
            ui.label(RichText::new(out.display().to_string()).color(p.text).size(14.0));
            ui.add_space(18.0);
        });
        ui.columns(4, |cols| {
            for (col, (label, value)) in cols.iter_mut().zip([
                ("With original names", sum.fs_files.to_string()),
                ("Found by content", sum.carved_files.to_string()),
                ("Could not be saved", sum.failures.to_string()),
                (
                    "Unreadable data",
                    if sum.unreadable_bytes > 0 { format_size(sum.unreadable_bytes) } else { "None".into() },
                ),
            ]) {
                col.vertical_centered(|ui| {
                    ui.label(RichText::new(label).size(12.5).color(p.weak));
                    ui.label(theme::semibold(value, 20.0).color(p.text));
                });
            }
        });
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            let total_w = 640.0;
            ui.add_space(((ui.available_width() - total_w) / 2.0).max(0.0));
            if theme::primary_button(ui, p, &format!("{}  Open folder", icon::FOLDER_OPEN), true).clicked() {
                action = DoneAction::OpenFolder;
            }
            if sum.report.is_some() && theme::secondary_button(ui, &format!("{} Open report", icon::FILE_CSV)).clicked()
            {
                action = DoneAction::OpenReport;
            }
            if theme::secondary_button(ui, &format!("{} Back to results", icon::ARROW_LEFT)).clicked() {
                action = DoneAction::BackToResults;
            }
            if theme::secondary_button(ui, &format!("{} New scan", icon::ARROW_COUNTER_CLOCKWISE)).clicked() {
                action = DoneAction::NewScan;
            }
        });
        ui.add_space(12.0);
    });
    if sum.failures > 0 {
        ui.add_space(12.0);
        theme::notice(
            ui,
            p,
            p.warning,
            icon::WARNING,
            "Some files could not be written. Check that the destination drive has enough free space and that you can write to it.",
        );
    }
    action
}

pub fn about(ui: &mut Ui, p: &Palette, logo: &egui::TextureHandle) {
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::splat(88.0)));
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.add_space(6.0);
                    ui.label(theme::semibold("Windows Deleted Files Recovery", 24.0).color(p.text));
                    ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(p.weak));
                    ui.add_space(4.0);
                    theme::pill(ui, p, POWERED_BY, p.accent);
                });
            });
            ui.add_space(12.0);
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Recovers deleted photos, videos, music, documents and more from NTFS, FAT32 and exFAT drives, \
                         USB sticks, memory cards and disk images. Drives are only ever read, never written.",
                    )
                    .color(p.text),
                )
                .wrap(),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.hyperlink_to(format!("{} Project page", icon::GITHUB_LOGO), REPO);
                ui.add_space(12.0);
                ui.hyperlink_to(format!("{} Latest version", icon::DOWNLOAD_SIMPLE), format!("{REPO}/releases/latest"));
                ui.add_space(12.0);
                ui.hyperlink_to(format!("{} MIT License", icon::SCALES), format!("{REPO}/blob/main/LICENSE"));
            });
        });
        ui.add_space(14.0);
        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, p, icon::LIFEBUOY, "Tips for a successful recovery");
            ui.add_space(4.0);
            for tip in [
                "Stop using the drive right away. Every new file can overwrite deleted ones.",
                "Never save recovered files to the same drive you are recovering from.",
                "Memory cards, USB sticks and hard drives usually recover well.",
                "SSDs often erase deleted files automatically (TRIM), so recovery may be impossible.",
                "If the drive makes noises or is very slow, copy it to a disk image first and scan the image.",
                "No luck with Quick or Recommended? A deep search finds files even after formatting.",
            ] {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::CHECK).color(p.success));
                    ui.add(egui::Label::new(RichText::new(tip).color(p.text)).wrap());
                });
            }
        });
        ui.add_space(14.0);
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            ui.label(RichText::new(format!("© 2026 Bashar Salmo · {POWERED_BY}")).color(p.weak).size(12.5));
        });
    });
}
