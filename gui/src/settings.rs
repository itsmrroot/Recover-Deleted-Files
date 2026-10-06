//! User settings, persisted between runs, and the Settings page.

use eframe::egui::{self, RichText, Ui};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};
use wdfr::carve::Category;
use wdfr::recover::{Layout, Method};

use crate::i18n::{Language, icon_label, tr, trl};
use crate::theme::{self, Accent, Palette};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    System,
    Light,
    Dark,
    /// Near-black with a deep blue top bar.
    Midnight,
}

impl ThemeChoice {
    pub fn preference(self) -> egui::ThemePreference {
        match self {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::Dark | ThemeChoice::Midnight => egui::ThemePreference::Dark,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // Appearance
    pub language: Language,
    pub theme: ThemeChoice,
    pub accent: Accent,
    pub ui_scale: f32,
    // Scanning
    pub method: Method,
    /// Empty = everything.
    pub categories: Vec<Category>,
    pub min_size_kb: u64,
    pub carve_all_space: bool,
    pub byte_level: bool,
    /// 0 = no limit.
    pub max_carve_mb: u64,
    pub show_whole_disks: bool,
    // Results
    pub show_overwritten: bool,
    // Saving
    /// Empty = a new folder on the Desktop.
    pub destination: String,
    pub layout: Layout,
    pub restore_dates: bool,
    pub write_report: bool,
    pub open_folder_when_done: bool,
    pub allow_same_volume: bool,
    // Updates
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: Language::System,
            theme: ThemeChoice::System,
            accent: Accent::Blue,
            ui_scale: 1.0,
            method: Method::All,
            categories: Vec::new(),
            min_size_kb: 0,
            carve_all_space: false,
            byte_level: false,
            max_carve_mb: 0,
            show_whole_disks: false,
            show_overwritten: false,
            destination: String::new(),
            layout: Layout::Original,
            restore_dates: true,
            write_report: true,
            open_folder_when_done: true,
            allow_same_volume: false,
            check_updates: true,
        }
    }
}

pub fn method_label(m: Method) -> &'static str {
    match m {
        Method::Fs => tr("Quick"),
        Method::All => tr("Recommended"),
        Method::Carve => tr("Deep (formatted drives)"),
    }
}

/// A setting: `help` is in logical order (`trl`).
fn row(ui: &mut Ui, p: &Palette, title: &str, help: &str, control: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 300.0).max(260.0));
            ui.label(RichText::new(title).color(p.text).size(14.5));
            if !help.is_empty() {
                theme::paragraph(ui, help, 12.5, p.weak);
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
    ui.add_space(6.0);
    ui.separator();
    ui.add_space(6.0);
}

fn toggle(ui: &mut Ui, on: &mut bool) {
    let label = if *on { tr("On") } else { tr("Off") };
    ui.checkbox(on, label);
}

/// Draws the Settings page. Returns true if anything changed.
pub fn page(ui: &mut Ui, p: &Palette, s: &mut Settings) -> bool {
    let before = s.clone();
    theme::page_title(ui, p, tr("Settings"), tr("Saved automatically and used for every new scan."));

    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, p, icon::PALETTE, tr("Appearance"));
            ui.add_space(6.0);
            row(ui, p, tr("Language"), "", |ui| {
                egui::ComboBox::from_id_salt("language")
                    .selected_text(s.language.label())
                    // Tall enough to show every language without scrolling.
                    .height(420.0).show_ui(ui, |ui| {
                    for l in Language::ALL {
                        ui.selectable_value(&mut s.language, l, l.label());
                    }
                });
            });
            row(ui, p, tr("Theme"), trl("Follow the system, or always light or dark."), |ui| {
                for (choice, label) in [
                    (ThemeChoice::Midnight, tr("Midnight")),
                    (ThemeChoice::Dark, tr("Dark")),
                    (ThemeChoice::Light, tr("Light")),
                    (ThemeChoice::System, tr("System")),
                ] {
                    ui.selectable_value(&mut s.theme, choice, label);
                }
            });
            row(ui, p, tr("Accent colour"), "", |ui| {
                for a in Accent::ALL.iter().rev() {
                    let selected = s.accent == *a;
                    let text = RichText::new(if selected { icon::CHECK_CIRCLE } else { icon::CIRCLE }).size(22.0).color(a.color());
                    if ui.add(egui::Button::new(text).frame(false)).on_hover_text(a.name()).clicked() {
                        s.accent = *a;
                    }
                }
            });
            row(ui, p, tr("Interface size"), trl("Make everything larger or smaller."), |ui| {
                let mut pct = (s.ui_scale * 100.0).round() as i32;
                if ui.add(egui::Slider::new(&mut pct, 80..=150).step_by(10.0).suffix(" %")).changed() {
                    s.ui_scale = pct as f32 / 100.0;
                }
            });
        });
        ui.add_space(14.0);

        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, p, icon::MAGNIFYING_GLASS, tr("Scanning"));
            ui.add_space(6.0);
            row(ui, p, tr("Default search mode"), trl("Used when you start a new scan."), |ui| {
                egui::ComboBox::from_id_salt("method").selected_text(method_label(s.method)).show_ui(ui, |ui| {
                    for m in [Method::All, Method::Fs, Method::Carve] {
                        ui.selectable_value(&mut s.method, m, method_label(m));
                    }
                });
            });
            row(ui, p, tr("Skip files smaller than"), trl("Tiny files are mostly icons and thumbnails. 0 keeps everything."), |ui| {
                ui.add(egui::DragValue::new(&mut s.min_size_kb).range(0..=1_000_000).suffix(" KB"));
            });
            row(
                ui,
                p,
                tr("Search the whole drive"),
                trl("Deep search normally scans only free space, where deleted files live. Turn on to also scan space used by existing files."),
                |ui| toggle(ui, &mut s.carve_all_space),
            );
            row(
                ui,
                p,
                tr("Byte-level deep search"),
                trl("Also finds files hidden inside other data (e.g. photos inside documents). Much slower."),
                |ui| toggle(ui, &mut s.byte_level),
            );
            row(ui, p, tr("Largest file to look for"), trl("Limits deep-search results. 0 means no limit."), |ui| {
                ui.add(egui::DragValue::new(&mut s.max_carve_mb).range(0..=1_048_576).suffix(" MB"));
            });
            row(ui, p, tr("Show whole disks"), trl("List physical disks in addition to drive letters (advanced)."), |ui| {
                toggle(ui, &mut s.show_whole_disks)
            });
        });
        ui.add_space(14.0);

        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, p, icon::FLOPPY_DISK, tr("Saving recovered files"));
            ui.add_space(6.0);
            row(ui, p, tr("Default destination"), trl("Leave empty to create a new folder on your Desktop."), |ui| {
                if ui.button(icon_label(icon::FOLDER_OPEN, "Browse")).clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    s.destination = dir.display().to_string();
                }
                ui.add(egui::TextEdit::singleline(&mut s.destination).hint_text(tr("Desktop (automatic)")).desired_width(220.0));
            });
            row(ui, p, tr("Folder layout"), trl("Keep the original folders, or sort files into Images, Videos, ..."), |ui| {
                ui.selectable_value(&mut s.layout, Layout::ByType, tr("By file type"));
                ui.selectable_value(&mut s.layout, Layout::Original, tr("Original folders"));
            });
            row(ui, p, tr("Restore original dates"), trl("Give recovered files their original modification time."), |ui| {
                toggle(ui, &mut s.restore_dates)
            });
            row(ui, p, tr("Create a report"), trl("Write report.csv listing every recovered file."), |ui| toggle(ui, &mut s.write_report));
            row(ui, p, tr("Open the folder when done"), "", |ui| toggle(ui, &mut s.open_folder_when_done));
            row(ui, p, tr("Show overwritten files"), trl("List files whose content has been replaced by other data. They rarely open."), |ui| {
                toggle(ui, &mut s.show_overwritten)
            });
        });
        ui.add_space(14.0);

        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, p, icon::SHIELD_WARNING, tr("Safety"));
            ui.add_space(6.0);
            row(
                ui,
                p,
                tr("Allow saving to the drive being recovered"),
                trl("Not recommended: every file written there can overwrite the deleted files you are trying to save."),
                |ui| {
                    let (label, color) = if s.allow_same_volume { (tr("Allowed"), p.danger) } else { (tr("Off"), p.text) };
                    ui.checkbox(&mut s.allow_same_volume, RichText::new(label).color(color));
                },
            );
        });
        ui.add_space(14.0);

        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, p, icon::ARROW_CIRCLE_UP, tr("Updates"));
            ui.add_space(6.0);
            row(
                ui,
                p,
                tr("Check for updates at start"),
                trl("Asks GitHub for the latest version when the app starts. Nothing about you or your files is sent."),
                |ui| toggle(ui, &mut s.check_updates),
            );
        });
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if theme::secondary_button(ui, &icon_label(icon::ARROW_COUNTER_CLOCKWISE, "Reset to defaults")).clicked() {
                // The language is kept: resetting should not switch it under the user.
                *s = Settings { language: s.language, ..Settings::default() };
            }
        });
        ui.add_space(20.0);
    });
    *s != before
}
