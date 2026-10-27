//! "Combine drives (RAID)…": the drives of a NAS or a RAID, read as one.
//! Linux software RAID (Synology, QNAP, Linux) is found automatically from
//! the drives; a hardware RAID is described by hand.

use eframe::egui::{self, Align, Layout, RichText, Ui};
use egui_phosphor::regular as icon;
use wdfr::raid::{Geometry, Spec};

use crate::i18n::{icon_label, tr, trf, trl};
use crate::theme::{self, Palette};

/// A disk of a hand-described RAID: a source, or a missing one.
#[derive(Clone, PartialEq)]
enum Slot {
    Disk(String, String),
    Missing,
}

pub struct RaidDialog {
    /// In order: for a hand-described RAID the order matters.
    slots: Vec<Slot>,
    auto: bool,
    level: i32,
    chunk_kib: u64,
    layout: wdfr::raid::Layout,
}

const LEVELS: [(i32, &str); 6] =
    [(0, "RAID 0"), (1, "RAID 1"), (5, "RAID 5"), (6, "RAID 6"), (10, "RAID 10"), (-1, "JBOD")];
const CHUNKS: [u64; 7] = [4, 16, 32, 64, 128, 256, 512];

pub enum Outcome {
    Open,
    Cancelled,
    /// The `raid:` source to scan.
    Combined(String),
}

impl Default for RaidDialog {
    fn default() -> Self {
        Self { slots: Vec::new(), auto: true, level: 5, chunk_kib: 64, layout: wdfr::raid::Layout::LeftSymmetric }
    }
}

fn layout_label(l: wdfr::raid::Layout) -> &'static str {
    use wdfr::raid::Layout::*;
    match l {
        LeftSymmetric => trl("Left symmetric (most common)"),
        LeftAsymmetric => trl("Left asymmetric"),
        RightSymmetric => trl("Right symmetric"),
        RightAsymmetric => trl("Right asymmetric"),
        Parity0 => trl("Parity on the last drive"),
    }
}

impl RaidDialog {
    #[cfg(debug_assertions)]
    pub fn tour(&mut self, images: &[String], manual: bool) {
        for i in images {
            let name = std::path::Path::new(i).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            self.slots.push(Slot::Disk(i.clone(), name));
        }
        if manual {
            self.auto = false;
            self.slots.insert(1, Slot::Missing);
        }
    }

    /// The `raid:` path for what is chosen, if enough is.
    fn spec(&self) -> Option<String> {
        let disks: Vec<Option<String>> = self
            .slots
            .iter()
            .map(|s| match s {
                Slot::Disk(p, _) => Some(p.clone()),
                Slot::Missing => None,
            })
            .collect();
        let present: Vec<String> = disks.iter().flatten().cloned().collect();
        if present.is_empty() || (!self.auto && disks.len() < 2) {
            return None;
        }
        let spec = if self.auto {
            Spec::Auto(present)
        } else if self.level == -1 {
            Spec::Span(present)
        } else {
            let near = if self.level == 10 { 2 } else { 0 };
            Spec::Manual {
                geometry: Geometry { level: self.level, chunk: self.chunk_kib * 1024, layout: self.layout, near },
                disks,
            }
        };
        Some(spec.path())
    }

    /// `sources`: (path, name) of every drive and image that can be used.
    pub fn show(&mut self, ctx: &egui::Context, p: &Palette, sources: &[(String, String)]) -> Outcome {
        let mut outcome = Outcome::Open;
        let modal = egui::Modal::new(egui::Id::new("raid")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::HARD_DRIVES).size(26.0).color(p.accent));
                ui.label(theme::semibold(tr("Combine drives (RAID)"), 18.0).color(p.text));
            });
            ui.add_space(6.0);
            theme::paragraph(
                ui,
                trl("For the drives of a NAS (Synology, QNAP) or of a RAID: choose all of its drives, or copies of them. They are read as one, and nothing is written to them."),
                14.0,
                p.weak,
            );
            ui.add_space(10.0);
            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                for (path, name) in sources {
                    let picked = self.slots.iter().any(|s| matches!(s, Slot::Disk(q, _) if q == path));
                    let mut on = picked;
                    if ui.checkbox(&mut on, name).changed() {
                        if on {
                            self.slots.push(Slot::Disk(path.clone(), name.clone()));
                        } else {
                            self.slots.retain(|s| !matches!(s, Slot::Disk(q, _) if q == path));
                        }
                    }
                }
                if sources.is_empty() {
                    ui.label(RichText::new(tr("No drives can be read: open disk images, or give the app access to drives.")).color(p.weak));
                }
            });
            ui.add_space(10.0);
            ui.radio_value(&mut self.auto, true, tr("Find the RAID automatically (Linux, Synology, QNAP)"));
            ui.radio_value(&mut self.auto, false, tr("Describe it (hardware RAID)"));
            if !self.auto {
                ui.add_space(6.0);
                self.manual(ui, p);
            }
            ui.add_space(12.0);
            let spec = self.spec();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::primary_button(ui, p, &icon_label(icon::CHECK, "Combine"), spec.is_some()).clicked()
                    && let Some(s) = spec
                {
                    outcome = Outcome::Combined(s);
                }
                if theme::secondary_button(ui, tr("Cancel")).clicked() {
                    outcome = Outcome::Cancelled;
                }
            });
        });
        if modal.should_close() && matches!(outcome, Outcome::Open) {
            outcome = Outcome::Cancelled;
        }
        outcome
    }

    fn manual(&mut self, ui: &mut Ui, p: &Palette) {
        egui::Grid::new("raid-geometry").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Level"));
            egui::ComboBox::from_id_salt("raid-level")
                .selected_text(LEVELS.iter().find(|l| l.0 == self.level).map_or("", |l| l.1))
                .show_ui(ui, |ui| {
                    for (l, name) in LEVELS {
                        ui.selectable_value(&mut self.level, l, name);
                    }
                });
            ui.end_row();
            if self.level != 1 && self.level != -1 {
                ui.label(tr("Stripe size"));
                egui::ComboBox::from_id_salt("raid-chunk").selected_text(format!("{} KiB", self.chunk_kib)).show_ui(
                    ui,
                    |ui| {
                        for c in CHUNKS {
                            ui.selectable_value(&mut self.chunk_kib, c, format!("{c} KiB"));
                        }
                    },
                );
                ui.end_row();
            }
            if self.level == 5 || self.level == 6 {
                use wdfr::raid::Layout::*;
                ui.label(tr("Parity layout"));
                egui::ComboBox::from_id_salt("raid-layout").selected_text(layout_label(self.layout)).show_ui(
                    ui,
                    |ui| {
                        for l in [LeftSymmetric, LeftAsymmetric, RightSymmetric, RightAsymmetric, Parity0] {
                            ui.selectable_value(&mut self.layout, l, layout_label(l));
                        }
                    },
                );
                ui.end_row();
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new(tr("Order of the drives in the RAID:")).color(p.weak));
        let mut swap = None;
        let mut remove = None;
        let n = self.slots.len();
        for (i, s) in self.slots.iter().enumerate() {
            ui.horizontal(|ui| {
                let name = match s {
                    Slot::Disk(_, name) => name.clone(),
                    Slot::Missing => tr("(missing drive)").to_string(),
                };
                ui.label(format!("{}.", i + 1));
                if ui.add_enabled(i > 0, egui::Button::new(icon::ARROW_UP).small()).clicked() {
                    swap = Some(i - 1);
                }
                if ui.add_enabled(i + 1 < n, egui::Button::new(icon::ARROW_DOWN).small()).clicked() {
                    swap = Some(i);
                }
                if *s == Slot::Missing && ui.button(icon::X).clicked() {
                    remove = Some(i);
                }
                ui.label(name);
            });
        }
        if let Some(i) = swap {
            self.slots.swap(i, i + 1);
        }
        if let Some(i) = remove {
            self.slots.remove(i);
        }
        if ui.button(icon_label(icon::PLUS, "Add a missing drive")).clicked() {
            self.slots.push(Slot::Missing);
        }
        if self.level == 0 && self.slots.contains(&Slot::Missing) {
            ui.label(RichText::new(tr("A RAID 0 cannot be read with a drive missing.")).color(p.danger));
        }
    }
}

/// The name of a `raid:` source: "RAID (3 drives)".
pub fn name(path: &str) -> Option<String> {
    let spec = Spec::parse(path.strip_prefix("raid:")?).ok()?;
    let n = match &spec {
        Spec::Auto(d) | Spec::Span(d) => d.len(),
        Spec::Manual { disks, .. } => disks.len(),
    };
    Some(trf("RAID ({n} drives)", &[("n", &n)]))
}
