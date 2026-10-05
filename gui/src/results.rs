//! The results screen: a searchable, sortable table of everything found,
//! with selection, previews and the "Recover" action.

use std::sync::Arc;

use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Ui, Vec2};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;
use wdfr::carve::Category;
use wdfr::fs::Condition;
use wdfr::recover::{self, Found, Item, Session};
use wdfr::units::format_size;

use crate::jobs::category_slot;
use crate::preview::{Preview, Previewer};
use crate::theme::{self, Palette};

/// Identifies a found file: an index into `Found::fs` or `Found::carved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowRef {
    Fs(usize),
    Carved(usize),
}

impl RowRef {
    pub fn item(self, found: &Found) -> Item<'_> {
        match self {
            RowRef::Fs(i) => Item::Fs(&found.fs[i]),
            RowRef::Carved(i) => Item::Carved(&found.carved[i]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Good,
    Partial(u8),
    Overwritten,
    /// Found by the deep search (content-based).
    Found,
}

pub struct Row {
    pub r: RowRef,
    pub name: String,
    pub name_lower: String,
    pub folder: String,
    pub ext: String,
    pub category: Option<Category>,
    pub size: u64,
    pub modified: Option<chrono::NaiveDateTime>,
    pub status: Status,
    pub note: Option<String>,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    All,
    Named,
    Deep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortCol {
    Name,
    Size,
    Modified,
    Status,
    Folder,
}

pub enum Action {
    None,
    Recover,
    NewScan,
    DeepScan,
}

pub struct Results {
    pub session: Arc<Session>,
    pub found: Arc<Found>,
    pub source_name: String,
    pub rows: Vec<Row>,
    pub selected: Vec<bool>,
    pub dest: String,
    pub dest_error: Option<String>,
    dest_checked: Option<String>,
    view: Vec<usize>,
    dirty: bool,
    query: String,
    /// Category slot (see `category_slot`), or None for all.
    cat: Option<usize>,
    origin: Origin,
    show_overwritten: bool,
    sort: (SortCol, bool),
    focus: Option<usize>,
    counts: [usize; 7],
}

pub fn category_label(slot: usize) -> &'static str {
    ["Images", "Videos", "Audio", "Documents", "Archives", "Databases", "Other"][slot.min(6)]
}

pub fn category_icon(c: Option<Category>) -> &'static str {
    match c {
        Some(Category::Image) => icon::IMAGE,
        Some(Category::Video) => icon::FILM_STRIP,
        Some(Category::Audio) => icon::MUSIC_NOTES,
        Some(Category::Document) => icon::FILE_TEXT,
        Some(Category::Archive) => icon::FILE_ZIP,
        Some(Category::Database) => icon::DATABASE,
        None => icon::FILE,
    }
}

fn status_of(c: Condition) -> Status {
    match c {
        Condition::Recoverable => Status::Good,
        Condition::Partial(p) => Status::Partial(p),
        Condition::Overwritten => Status::Overwritten,
    }
}

fn status_pill(ui: &mut Ui, p: &Palette, s: Status) {
    match s {
        Status::Good => theme::pill(ui, p, "Recoverable", p.success),
        Status::Partial(n) => theme::pill(ui, p, &format!("Partial · {n}%"), p.warning),
        Status::Overwritten => theme::pill(ui, p, "Overwritten", p.danger),
        Status::Found => theme::pill(ui, p, "Found by content", p.deep),
    };
}

fn status_rank(s: Status) -> u8 {
    match s {
        Status::Good => 0,
        Status::Found => 1,
        Status::Partial(_) => 2,
        Status::Overwritten => 3,
    }
}

impl Results {
    pub fn new(session: Arc<Session>, found: Found, source_name: String, show_overwritten: bool, dest: String) -> Self {
        let found = Arc::new(found);
        let mut rows = Vec::with_capacity(found.fs.len() + found.carved.len());
        for (i, f) in found.fs.iter().enumerate() {
            let file = &f.file;
            let name = file.name().to_string();
            let folder = file.path.rsplit_once('/').map_or(String::new(), |(d, _)| d.to_string());
            let p = &session.partitions[f.partition];
            rows.push(Row {
                r: RowRef::Fs(i),
                name_lower: name.to_lowercase(),
                ext: name.rsplit_once('.').map_or(String::new(), |(_, e)| e.to_ascii_lowercase()),
                category: recover::file_category(file),
                folder,
                name,
                size: file.size,
                modified: file.modified,
                status: status_of(file.condition),
                note: file.note.clone(),
                offset: file.data_ranges().first().map(|r| p.start + r.start),
            });
        }
        for (i, c) in found.carved.iter().enumerate() {
            let name = recover::carved_name(c);
            rows.push(Row {
                r: RowRef::Carved(i),
                name_lower: name.to_lowercase(),
                name,
                folder: String::new(),
                ext: c.ext.to_string(),
                category: Some(c.category),
                size: c.len,
                modified: None,
                status: Status::Found,
                note: Some(format!("{} structure", c.format.to_uppercase())),
                offset: Some(c.offset),
            });
        }
        let selected = rows.iter().map(|r| r.status != Status::Overwritten).collect();
        let mut res = Self {
            session,
            found,
            source_name,
            rows,
            selected,
            dest,
            dest_error: None,
            dest_checked: None,
            view: Vec::new(),
            dirty: true,
            query: String::new(),
            cat: None,
            origin: Origin::All,
            show_overwritten,
            sort: (SortCol::Status, true),
            focus: None,
            counts: [0; 7],
        };
        res.recompute();
        res
    }

    pub fn set_show_overwritten(&mut self, show: bool) {
        if self.show_overwritten != show {
            self.show_overwritten = show;
            self.dirty = true;
        }
    }

    /// Selected rows that are currently eligible (overwritten ones are only
    /// recovered when they are shown).
    pub fn selected_refs(&self) -> Vec<RowRef> {
        self.rows
            .iter()
            .zip(&self.selected)
            .filter(|(r, s)| **s && (self.show_overwritten || r.status != Status::Overwritten))
            .map(|(r, _)| r.r)
            .collect()
    }

    #[cfg(debug_assertions)]
    pub fn has_focus(&self) -> bool {
        self.focus.is_some()
    }

    /// Focuses the first previewable photo (used by the screenshot tour).
    #[cfg(debug_assertions)]
    pub fn focus_first_image(&mut self) {
        self.focus = self.view.iter().copied().find(|&i| matches!(self.rows[i].ext.as_str(), "jpg" | "png"));
    }

    fn selection(&self) -> (usize, u64) {
        self.rows
            .iter()
            .zip(&self.selected)
            .filter(|(r, s)| **s && (self.show_overwritten || r.status != Status::Overwritten))
            .fold((0, 0), |(n, b), (r, _)| (n + 1, b + r.size))
    }

    fn eligible(&self, r: &Row) -> bool {
        (self.show_overwritten || r.status != Status::Overwritten)
            && match self.origin {
                Origin::All => true,
                Origin::Named => matches!(r.r, RowRef::Fs(_)),
                Origin::Deep => matches!(r.r, RowRef::Carved(_)),
            }
    }

    fn recompute(&mut self) {
        let q = self.query.trim().to_lowercase();
        let mut counts = [0usize; 7];
        let mut view = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            if !self.eligible(r) || !(q.is_empty() || r.name_lower.contains(&q) || r.folder.to_lowercase().contains(&q))
            {
                continue;
            }
            let slot = category_slot(r.category);
            counts[slot] += 1;
            if self.cat.is_none_or(|c| c == slot) {
                view.push(i);
            }
        }
        let rows = &self.rows;
        let (col, asc) = self.sort;
        view.sort_by(|&a, &b| {
            let (x, y) = (&rows[a], &rows[b]);
            let o = match col {
                SortCol::Name => x.name_lower.cmp(&y.name_lower),
                SortCol::Size => x.size.cmp(&y.size),
                SortCol::Modified => x.modified.cmp(&y.modified),
                SortCol::Status => status_rank(x.status).cmp(&status_rank(y.status)).then(y.size.cmp(&x.size)),
                SortCol::Folder => x.folder.cmp(&y.folder).then(x.name_lower.cmp(&y.name_lower)),
            };
            if asc { o } else { o.reverse() }
        });
        self.view = view;
        self.counts = counts;
        self.dirty = false;
    }

    fn check_dest(&mut self, allow_same_volume: bool) {
        if self.dest_checked.as_deref() == Some(self.dest.as_str()) {
            return;
        }
        self.dest_checked = Some(self.dest.clone());
        self.dest_error = if self.dest.trim().is_empty() {
            Some("Choose where to save the recovered files.".into())
        } else if allow_same_volume {
            None
        } else {
            wdfr::output::ensure_not_on_source(&self.session.path, std::path::Path::new(self.dest.trim())).err().map(
                |_| "This folder is on the drive you are recovering from. Choose a folder on another drive.".into(),
            )
        };
    }

    pub fn page(&mut self, ui: &mut Ui, p: &Palette, previewer: &mut Previewer, allow_same_volume: bool) -> Action {
        if self.dirty {
            self.recompute();
        }
        self.check_dest(allow_same_volume);
        let mut action = Action::None;

        // Header
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                let total =
                    self.rows.iter().filter(|r| self.show_overwritten || r.status != Status::Overwritten).count();
                ui.label(theme::semibold(format!("{total} files found"), 26.0).color(p.text));
                let mut sub = format!("on {}", self.source_name);
                if self.found.cancelled {
                    sub.push_str(" · scan was stopped early");
                }
                ui.label(RichText::new(sub).color(p.weak).size(15.0));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::secondary_button(ui, &format!("{} New scan", icon::ARROW_COUNTER_CLOCKWISE)).clicked() {
                    action = Action::NewScan;
                }
            });
        });
        ui.add_space(10.0);

        if self.rows.is_empty() {
            self.empty_state(ui, p, &mut action);
            return action;
        }

        self.toolbar(ui, p);
        ui.add_space(8.0);

        // Action bar at the bottom, preview on the right, table in the rest.
        egui::Panel::bottom("results-actions")
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 0, top: 12, bottom: 0 }))
            .show(ui, |ui| {
                if self.action_bar(ui, p) {
                    action = Action::Recover;
                }
            });
        egui::Panel::right("results-preview")
            .exact_size(300.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| self.preview_panel(ui, p, previewer));
        egui::CentralPanel::no_frame().show(ui, |ui| {
            theme::card(ui, p, |ui| {
                ui.set_min_size(ui.available_size());
                self.table(ui, p);
            });
        });
        action
    }

    fn empty_state(&self, ui: &mut Ui, p: &Palette, action: &mut Action) {
        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.label(RichText::new(icon::MAGNIFYING_GLASS).size(54.0).color(p.weak));
                ui.label(theme::semibold("No deleted files were found", 20.0).color(p.text));
                ui.label(
                    RichText::new(
                        "If the drive was formatted, or the files were deleted a while ago, try a deep search: it looks for files by their content.",
                    )
                    .color(p.weak),
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    let w = 380.0;
                    ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                    if theme::primary_button(ui, p, &format!("{} Run a deep search", icon::MAGNIFYING_GLASS), true).clicked() {
                        *action = Action::DeepScan;
                    }
                    if theme::secondary_button(ui, "Choose another drive").clicked() {
                        *action = Action::NewScan;
                    }
                });
                ui.add_space(30.0);
            });
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, p: &Palette) {
        ui.horizontal_wrapped(|ui| {
            let search = egui::TextEdit::singleline(&mut self.query)
                .hint_text(format!("{}  Search by name or folder", icon::MAGNIFYING_GLASS))
                .desired_width(260.0)
                .margin(Vec2::new(10.0, 7.0));
            if ui.add(search).changed() {
                self.dirty = true;
            }
            let before = self.origin;
            egui::ComboBox::from_id_salt("origin")
                .selected_text(match self.origin {
                    Origin::All => "All results",
                    Origin::Named => "With original names",
                    Origin::Deep => "Found by content",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.origin, Origin::All, "All results");
                    ui.selectable_value(&mut self.origin, Origin::Named, "With original names");
                    ui.selectable_value(&mut self.origin, Origin::Deep, "Found by content");
                });
            if self.origin != before {
                self.dirty = true;
            }
            ui.add_space(6.0);
            let _ = p;
            let all: usize = self.counts.iter().sum();
            if ui.selectable_label(self.cat.is_none(), format!("All  {all}")).clicked() {
                self.cat = None;
                self.dirty = true;
            }
            for slot in 0..7 {
                let n = self.counts[slot];
                if n == 0 && self.cat != Some(slot) {
                    continue;
                }
                let c = Category::ALL.get(slot).copied();
                let text = format!("{} {}  {n}", category_icon(c), category_label(slot));
                if ui.selectable_label(self.cat == Some(slot), text).clicked() {
                    self.cat = if self.cat == Some(slot) { None } else { Some(slot) };
                    self.dirty = true;
                }
            }
        });
    }

    fn table(&mut self, ui: &mut Ui, p: &Palette) {
        let rows = &self.rows;
        let view = &self.view;
        let selected = &mut self.selected;
        let mut focus = self.focus;
        let mut sort_click: Option<SortCol> = None;
        let sort = self.sort;

        let header = |ui: &mut Ui, label: &str, col: SortCol, sort_click: &mut Option<SortCol>| {
            let arrow = if sort.0 == col { if sort.1 { icon::CARET_UP } else { icon::CARET_DOWN } } else { "" };
            let text = RichText::new(format!("{label} {arrow}")).strong().color(p.weak).size(13.0);
            if ui
                .add(egui::Label::new(text).sense(Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                *sort_click = Some(col);
            }
        };

        // The location column only appears when there is room for it.
        let show_location = ui.available_width() > 860.0;
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(30.0))
            .column(Column::remainder().at_least(150.0).clip(true))
            .column(Column::exact(84.0))
            .column(Column::exact(128.0))
            .column(Column::exact(146.0));
        if show_location {
            table = table.column(Column::exact(200.0).clip(true));
        }
        table
            .header(30.0, |mut h| {
                h.col(|ui| {
                    let mut all = !view.is_empty() && view.iter().all(|&i| selected[i]);
                    if ui.checkbox(&mut all, "").on_hover_text("Select all shown").changed() {
                        for &i in view {
                            selected[i] = all;
                        }
                    }
                });
                h.col(|ui| header(ui, "Name", SortCol::Name, &mut sort_click));
                h.col(|ui| header(ui, "Size", SortCol::Size, &mut sort_click));
                h.col(|ui| header(ui, "Modified", SortCol::Modified, &mut sort_click));
                h.col(|ui| header(ui, "Status", SortCol::Status, &mut sort_click));
                if show_location {
                    h.col(|ui| header(ui, "Location", SortCol::Folder, &mut sort_click));
                }
            })
            .body(|body| {
                body.rows(30.0, view.len(), |mut row| {
                    let i = view[row.index()];
                    let r = &rows[i];
                    row.set_selected(focus == Some(i));
                    row.col(|ui| {
                        ui.checkbox(&mut selected[i], "");
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(category_icon(r.category)).size(16.0).color(p.accent));
                        ui.label(RichText::new(&r.name).color(p.text)).on_hover_text(if r.folder.is_empty() {
                            r.name.clone()
                        } else {
                            format!("{}/{}", r.folder, r.name)
                        });
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(format_size(r.size)).color(p.weak));
                    });
                    row.col(|ui| {
                        let t =
                            r.modified.map(|m| m.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "—".into());
                        ui.label(RichText::new(t).color(p.weak));
                    });
                    row.col(|ui| status_pill(ui, p, r.status));
                    if show_location {
                        row.col(|ui| {
                            let t = if matches!(r.r, RowRef::Carved(_)) {
                                "Deep search".to_string()
                            } else if r.folder.is_empty() {
                                "/".to_string()
                            } else {
                                r.folder.clone()
                            };
                            ui.label(RichText::new(t).color(p.weak));
                        });
                    }
                    let resp = row.response();
                    if resp.clicked() {
                        focus = Some(i);
                    }
                    if resp.double_clicked() {
                        selected[i] = !selected[i];
                    }
                });
            });

        self.focus = focus;
        if let Some(col) = sort_click {
            self.sort = if self.sort.0 == col { (col, !self.sort.1) } else { (col, col != SortCol::Size) };
            self.dirty = true;
        }
    }

    fn preview_panel(&mut self, ui: &mut Ui, p: &Palette, previewer: &mut Previewer) {
        theme::card(ui, p, |ui| {
            ui.set_min_size(ui.available_size());
            let Some(i) = self.focus else {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    ui.label(RichText::new(icon::EYE).size(40.0).color(p.weak));
                    ui.label(RichText::new("Click a file to preview it").color(p.weak));
                });
                return;
            };
            let (r, ext) = (self.rows[i].r, self.rows[i].ext.clone());
            let preview_h = 230.0;
            let mut dims = None;
            egui::Frame::new().fill(p.card_alt).corner_radius(10).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_height(preview_h);
                ui.centered_and_justified(|ui| match previewer.get(&self.session, &self.found, r, &ext) {
                    Preview::Loading => {
                        ui.spinner();
                    }
                    Preview::Image { texture, size } => {
                        dims = Some(*size);
                        ui.add(
                            egui::Image::new(texture)
                                .max_size(Vec2::new(ui.available_width() - 8.0, preview_h - 8.0))
                                .corner_radius(6),
                        );
                    }
                    Preview::Text(t) => {
                        egui::ScrollArea::vertical().id_salt("text-preview").show(ui, |ui| {
                            ui.label(RichText::new(t.as_str()).monospace().size(11.5).color(p.text));
                        });
                    }
                    Preview::Unavailable(why) => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(50.0);
                            ui.label(RichText::new(category_icon(self.rows[i].category)).size(46.0).color(p.accent));
                            ui.label(RichText::new(why.as_str()).color(p.weak).size(12.5));
                        });
                    }
                });
            });
            ui.add_space(10.0);
            let row = &self.rows[i];
            ui.add(egui::Label::new(theme::semibold(&row.name, 15.5).color(p.text)).wrap());
            ui.add_space(4.0);
            status_pill(ui, p, row.status);
            ui.add_space(6.0);
            let explain = match row.status {
                Status::Good => "The space this file used has not been reused. It should open normally.",
                Status::Partial(_) => "Part of this file's space was reused by other files. It may be damaged.",
                Status::Overwritten => "Other files have been written over this one. It will most likely not open.",
                Status::Found => "Found by its content in free space. The original name is not known.",
            };
            ui.add(egui::Label::new(RichText::new(explain).color(p.weak).size(12.5)).wrap());
            ui.add_space(8.0);
            egui::Grid::new("details").striped(false).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                let mut kv = |k: &str, v: String| {
                    ui.label(RichText::new(k).color(p.weak).size(12.5));
                    ui.add(egui::Label::new(RichText::new(v).color(p.text).size(12.5)).truncate());
                    ui.end_row();
                };
                kv("Size", format_size(row.size));
                if let Some([w, h]) = dims {
                    kv("Dimensions", format!("{w} × {h}"));
                }
                kv(
                    "Modified",
                    row.modified.map(|m| m.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_else(|| "Unknown".into()),
                );
                kv("Location", if row.folder.is_empty() { "—".into() } else { row.folder.clone() });
                if let Some(o) = row.offset {
                    kv("Disk offset", format!("{o:#x}"));
                }
                if let Some(n) = &row.note {
                    kv("Notes", n.clone());
                }
            });
            ui.add_space(8.0);
            let sel = self.selected[i];
            let label = if sel {
                format!("{} Selected", icon::CHECK_SQUARE)
            } else {
                format!("{} Select this file", icon::SQUARE)
            };
            if theme::secondary_button(ui, &label).clicked() {
                self.selected[i] = !sel;
            }
        });
    }

    /// Returns true when "Recover" was clicked.
    fn action_bar(&mut self, ui: &mut Ui, p: &Palette) -> bool {
        let (n, bytes) = self.selection();
        let mut go = false;
        theme::card(ui, p, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(theme::semibold(format!("{n} selected"), 16.0).color(p.text));
                    ui.label(RichText::new(format_size(bytes)).color(p.weak));
                });
                ui.add_space(6.0);
                if ui.small_button("Select all").clicked() {
                    for (r, s) in self.rows.iter().zip(self.selected.iter_mut()) {
                        *s = r.status != Status::Overwritten || self.show_overwritten;
                    }
                }
                if ui.small_button("Select none").clicked() {
                    self.selected.iter_mut().for_each(|s| *s = false);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let ok = n > 0 && self.dest_error.is_none();
                    if theme::primary_button(ui, p, &format!("{}  Recover {n} files", icon::DOWNLOAD_SIMPLE), ok)
                        .clicked()
                    {
                        go = true;
                    }
                    if ui.button(format!("{} Browse", icon::FOLDER_OPEN)).clicked()
                        && let Some(dir) = rfd::FileDialog::new().pick_folder()
                    {
                        self.dest = dir.display().to_string();
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.dest)
                            .desired_width((ui.available_width() - 120.0).clamp(200.0, 420.0))
                            .margin(Vec2::new(10.0, 7.0)),
                    );
                    ui.label(RichText::new("Save to").color(p.weak));
                });
            });
            if let Some(e) = &self.dest_error {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{} {e}", icon::WARNING)).color(p.danger));
            }
        });
        go
    }
}

/// Text colour for the found-by-category counters.
pub fn category_color(p: &Palette, slot: usize) -> Color32 {
    [p.accent, p.deep, p.success, p.warning, p.weak, p.weak, p.weak][slot.min(6)]
}
