//! The Help page: short step-by-step guides for common situations.

use eframe::egui::{self, Align, Layout, RichText, Ui, collapsing_header::CollapsingState};
use egui_phosphor::regular as icon;

use crate::i18n::{tr, trl};
use crate::theme::{self, Palette};

/// A guide: icon, title and its steps (`trl`, logical order).
struct Guide {
    icon: &'static str,
    title: &'static str,
    steps: Vec<&'static str>,
}

fn guides() -> Vec<Guide> {
    let access = if cfg!(windows) {
        trl(
            "Windows asks for administrator rights when the app starts: click Yes. Without them only disk images can be read.",
        )
    } else if cfg!(target_os = "macos") {
        trl("macOS: click \"Allow access to drives\" on the start screen and enter your password.")
    } else {
        trl("Linux: click \"Restart with administrator rights\" on the start screen and enter your password.")
    };
    let mut access_steps = vec![access];
    if cfg!(target_os = "macos") {
        access_steps.push(trl(
            "macOS also needs Full Disk Access: click \"Open Full Disk Access settings\", turn on Deleted Files Recovery and click Refresh. After an update, turn it on again.",
        ));
    }
    access_steps.push(trl("Drives are only ever read. Nothing is written to the drive you recover from."));

    vec![
        Guide {
            icon: icon::ROCKET_LAUNCH,
            title: tr("Getting started"),
            steps: vec![
                trl("Choose the drive the files were deleted from, or open a disk image."),
                trl("Choose what you are looking for, for example Images, or Everything."),
                trl("Choose how deep to search. Recommended works in most cases."),
                trl("Click \"Start scan\". The files found so far can be viewed while it runs."),
                trl("Tick the files you want, choose a folder on another drive and click \"Recover\"."),
            ],
        },
        Guide { icon: icon::SHIELD_CHECK, title: tr("Giving the app access to drives"), steps: access_steps },
        Guide {
            icon: icon::USB,
            title: tr("Memory cards and USB sticks"),
            steps: vec![
                trl("Stop using the card or stick right away, and do not format it."),
                trl("Connect it to the computer. If it does not appear, click Refresh."),
                trl("Use Recommended. If the card was formatted, use \"Formatted drive\"."),
                trl(
                    "Long videos from cameras and phones are often stored in pieces: the deep search puts them back together.",
                ),
            ],
        },
        Guide {
            icon: icon::WRENCH,
            title: tr("Formatted or damaged drives"),
            steps: vec![
                trl("Choose \"Formatted drive\". It finds files by their content, even without a file system."),
                trl(
                    "It also finds deleted partitions, and the old file table of a quick-formatted NTFS drive: those files come back with their names and folders.",
                ),
                trl(
                    "Other files have no original name: they are named by type, or from their own data, such as the date a photo was taken.",
                ),
                trl(
                    "If the drive is very slow or makes noises, select it and click \"Copy to an image…\" first: the drive is read only once, damaged areas last. Then scan the copy.",
                ),
            ],
        },
        Guide {
            icon: icon::HARD_DRIVE,
            title: tr("Mac and Linux drives"),
            steps: vec![
                trl(
                    "Drives from a Mac (APFS, Mac OS Extended) and from Linux (ext2, ext3, ext4) are read with their names and folders, like Windows drives.",
                ),
                trl(
                    "Deleted files are found in the older copies these file systems keep: earlier checkpoints of APFS, old catalog entries of Mac OS Extended, and the journal of ext3 and ext4.",
                ),
                trl(
                    "A damaged APFS drive that the Mac no longer opens is rebuilt from what is left of its file tables.",
                ),
                trl("Drives encrypted with FileVault cannot be read; the deep search finds nothing on them either."),
            ],
        },
        Guide {
            icon: icon::LOCK_KEY,
            title: tr("BitLocker drives"),
            steps: vec![
                trl(
                    "A drive locked with BitLocker shows \"BitLocker · locked\". Select it, enter its 48-digit recovery key or its password and click Unlock.",
                ),
                trl(
                    "The recovery key is in your Microsoft account (aka.ms/myrecoverykey), on a printout or on a USB stick.",
                ),
                trl(
                    "Windows reads a BitLocker drive it has already unlocked like any other drive. A locked one may only appear with \"Show whole disks\" turned on in Settings.",
                ),
                trl("The drive is read decrypted; nothing on it is changed, and the key is not stored."),
            ],
        },
        Guide {
            icon: icon::CLOCK_COUNTER_CLOCKWISE,
            title: tr("Older copies of your files"),
            steps: vec![
                trl(
                    "Windows: files kept in the Previous Versions copies (System Restore points) are found too, even when their space on the drive has been reused.",
                ),
                trl("Mac: deleted files are also found in APFS snapshots, such as the ones Time Machine makes."),
                trl("These files are marked with the date of the copy they come from."),
            ],
        },
        Guide {
            icon: icon::LIST_MAGNIFYING_GLASS,
            title: tr("Finding your files in the results"),
            steps: vec![
                trl("Use the search box and the type and date filters to narrow the list."),
                trl("Click a file to preview it before recovering it."),
                trl(
                    "Files marked \"Verified\" were checked and are complete, so they should open. \"May be damaged\" means a part is missing or broken.",
                ),
                trl(
                    "The status shows how likely a file is to open. Files marked \"Overwritten\" or \"Erased by the drive\" usually cannot be recovered.",
                ),
                trl("Exact duplicates are hidden, so every file is recovered only once."),
                trl(
                    "To look in one folder only, type it in \"Only in this folder\" before the scan: for example Users/Ann/Pictures.",
                ),
                trl(
                    "A damaged JPEG photo can often be repaired: select it and click \"Repair this photo…\". If its beginning is gone, a good photo from the same camera is needed.",
                ),
            ],
        },
        Guide {
            icon: icon::LOCK,
            title: tr("Keeping recovered files private"),
            steps: vec![
                trl(
                    "Tick \"Protect with a password\" before clicking Recover: everything is saved into one ZIP file encrypted with AES-256, and nothing is written unencrypted.",
                ),
                trl(
                    "It opens with the password in 7-Zip, WinRAR or Keka. Keep the password safe: without it the files cannot be opened.",
                ),
            ],
        },
        Guide {
            icon: icon::CLOCK,
            title: tr("Long scans"),
            steps: vec![
                trl("On the results page, click \"Save scan…\" to keep the results of a long scan."),
                trl(
                    "Later, click \"Open saved scan…\" on the start screen to continue without scanning again. The same drive must be connected.",
                ),
            ],
        },
        Guide {
            icon: icon::QUESTION,
            title: tr("Why are my files not found?"),
            steps: vec![
                trl("SSDs and phones erase deleted files automatically (TRIM), so they are often gone for good."),
                trl("Files that new data has been written over cannot come back."),
                trl(
                    "Search deeper: Recommended first, then \"Formatted drive\", then turn on \"Byte-level deep search\" in Settings.",
                ),
            ],
        },
    ]
}

pub fn page(ui: &mut Ui, p: &Palette) {
    theme::page_title(ui, p, tr("Help"), tr("Step-by-step guides for the most common situations."));
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        for (n, guide) in guides().into_iter().enumerate() {
            theme::card(ui, p, |ui| {
                ui.set_width(ui.available_width());
                let id = ui.make_persistent_id(("help", n));
                // The first guide starts open.
                let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, n == 0);
                let header = ui.horizontal(|ui| {
                    let arrow = if state.is_open() { icon::CARET_DOWN } else { icon::CARET_RIGHT };
                    ui.label(RichText::new(guide.icon).size(20.0).color(p.accent));
                    ui.label(theme::semibold(guide.title, 16.0).color(p.text));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(arrow).color(p.weak));
                    });
                });
                if header.response.interact(egui::Sense::click()).clicked() {
                    state.toggle(ui);
                }
                state.show_body_unindented(ui, |ui| {
                    ui.add_space(8.0);
                    for (i, step) in guide.steps.iter().enumerate() {
                        ui.horizontal_top(|ui| {
                            let number = RichText::new(format!("{}", i + 1)).strong().color(p.accent);
                            egui::Frame::new()
                                .fill(p.tint(p.accent))
                                .corner_radius(10)
                                .inner_margin(egui::Margin::symmetric(6, 1))
                                .show(ui, |ui| {
                                    // The same width for every number, so the steps line up.
                                    ui.set_width(12.0);
                                    ui.vertical_centered(|ui| ui.label(number));
                                });
                            theme::paragraph(ui, step, 14.5, p.text);
                        });
                        ui.add_space(4.0);
                    }
                });
                state.store(ui.ctx());
            });
            ui.add_space(12.0);
        }
    });
}
