//! The results screen: a searchable, sortable table of everything found,
//! with selection, previews and the "Recover" action.

use std::sync::Arc;

use chrono::Datelike;

use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Ui, Vec2};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;
use wdfr::carve::Category;
use wdfr::fs::Condition;
use wdfr::recover::{self, Found, Session};
use wdfr::units::format_size;
use wdfr::verify::Verdict;

use crate::i18n::{icon_label, tr, trf, trl, trn, visual};
use crate::jobs::category_slot;
use crate::preview::{Preview, Previewer};
use crate::theme::{self, Palette};
use crate::verifier::Verifier;

/// Identifies a found file: an index into `Found::fs` or `Found::carved`.
pub type RowRef = recover::ItemRef;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Good,
    Partial(u8),
    Overwritten,
    /// The drive erased the data itself (SSD TRIM).
    Erased,
    /// Found by the deep search (content-based).
    Found,
}

impl Status {
    /// Files that will not open: listed and selected only on request
    /// ("Show overwritten files").
    pub fn hidden_by_default(self) -> bool {
        matches!(self, Status::Overwritten | Status::Erased)
    }
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
    /// A carved file named from its own metadata (date, camera, title).
    pub named_from_metadata: bool,
    /// The file this one is identical to ("folder/name"), if any.
    pub duplicate_of: Option<String>,
    /// Whether the file was checked to open (see `Verifier`).
    pub verdict: Option<Verdict>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    All,
    Named,
    Deep,
}

/// The date filter of the results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YearFilter {
    Any,
    Year(i32),
    Undated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortCol {
    Name,
    Size,
    Modified,
    Status,
    Folder,
}

/// Why the destination folder cannot be used.
#[derive(Debug, Clone, Copy)]
pub enum DestError {
    Missing,
    OnSource,
}

impl DestError {
    fn message(self) -> String {
        match self {
            DestError::Missing => icon_label(icon::WARNING, "Choose where to save the recovered files."),
            DestError::OnSource => icon_label(
                icon::WARNING,
                "This folder is on the drive you are recovering from. Choose a folder on another drive.",
            ),
        }
    }
}

pub enum Action {
    None,
    Recover,
    NewScan,
    DeepScan,
    /// Save these results to a file (see `wdfr::saved`).
    SaveScan,
}

pub struct Results {
    /// Showing what a still-running scan has found so far: recovering waits
    /// for the scan to finish.
    pub scanning: bool,
    /// Where these results were last saved ("Save scan…").
    pub saved_to: Option<std::path::PathBuf>,
    pub session: Arc<Session>,
    pub found: Arc<Found>,
    pub source_name: String,
    pub rows: Vec<Row>,
    pub selected: Vec<bool>,
    pub dest: String,
    pub dest_error: Option<DestError>,
    dest_checked: Option<String>,
    view: Vec<usize>,
    dirty: bool,
    query: String,
    /// Category slot (see `category_slot`), or None for all.
    cat: Option<usize>,
    origin: Origin,
    year: YearFilter,
    /// Years present in the results, newest first, and whether any file
    /// has no date: the choices of the date filter.
    years: Vec<i32>,
    has_undated: bool,
    show_overwritten: bool,
    /// Duplicates are hidden, and not recovered, unless this is off.
    hide_duplicates: bool,
    duplicates: usize,
    sort: (SortCol, bool),
    focus: Option<usize>,
    counts: [usize; 7],
    /// Checks the files in the background once the scan has finished.
    verifier: Option<Verifier>,
    verified: usize,
    damaged: usize,
    /// Only files that were checked and open.
    verified_only: bool,
    /// Row of each found file.
    index: std::collections::HashMap<RowRef, usize>,
    /// Save into a password-protected ZIP: the password, typed twice.
    encrypt: bool,
    password: String,
    password_again: String,
    /// The outcome of the last photo repair: (photo, message, success).
    repaired: Option<(RowRef, String, bool)>,
}

/// The name of a category slot, in logical order (compose, then `visual`).
pub fn category_label(slot: usize) -> &'static str {
    match slot {
        0 => trl("Images"),
        1 => trl("Videos"),
        2 => trl("Audio"),
        3 => trl("Documents"),
        4 => trl("Archives"),
        5 => trl("Databases"),
        _ => trl("Other"),
    }
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
        Condition::Erased => Status::Erased,
    }
}

fn status_pill(ui: &mut Ui, p: &Palette, s: Status, verdict: Option<Verdict>) {
    // A file that will not open is not checked: its status says it all.
    if !s.hidden_by_default() {
        match verdict {
            Some(Verdict::Verified) => {
                theme::pill(ui, p, &icon_label(icon::SEAL_CHECK, "Verified"), p.success);
                return;
            }
            Some(Verdict::Damaged) => {
                theme::pill(ui, p, tr("May be damaged"), p.warning);
                return;
            }
            _ => {}
        }
    }
    match s {
        Status::Good => theme::pill(ui, p, tr("Recoverable"), p.success),
        Status::Partial(n) => theme::pill(ui, p, &trf("Partial · {n}%", &[("n", &n)]), p.warning),
        Status::Overwritten => theme::pill(ui, p, tr("Overwritten"), p.danger),
        Status::Erased => theme::pill(ui, p, tr("Erased by the drive"), p.danger),
        Status::Found => theme::pill(ui, p, tr("Found by content"), p.deep),
    };
}

fn status_rank(s: Status, verdict: Option<Verdict>) -> u8 {
    match (s, verdict) {
        (Status::Overwritten, _) => 5,
        (Status::Erased, _) => 6,
        (_, Some(Verdict::Verified)) => 0,
        (_, Some(Verdict::Damaged)) => 4,
        (Status::Good, _) => 1,
        (Status::Found, _) => 2,
        (Status::Partial(_), _) => 3,
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
            let start = session.partition(f.partition).map_or(0, |p| p.start);
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
                offset: file.data_ranges().first().map(|r| start + r.start),
                named_from_metadata: false,
                duplicate_of: None,
                verdict: None,
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
                modified: c.date,
                status: Status::Found,
                note: Some(trf("{format} structure", &[("format", &c.format.to_uppercase())])),
                offset: Some(c.offset),
                named_from_metadata: c.title.is_some(),
                duplicate_of: None,
                verdict: None,
            });
        }
        // Name each duplicate's original.
        let index: std::collections::HashMap<RowRef, usize> = rows.iter().enumerate().map(|(i, r)| (r.r, i)).collect();
        for i in 0..rows.len() {
            if let Some(orig) = found.duplicates.get(&rows[i].r).and_then(|o| index.get(o)) {
                let o = &rows[*orig];
                rows[i].duplicate_of =
                    Some(if o.folder.is_empty() { o.name.clone() } else { format!("{}/{}", o.folder, o.name) });
            }
        }
        let duplicates = rows.iter().filter(|r| r.duplicate_of.is_some()).count();
        let selected = rows.iter().map(|r| Self::included_with(r, false, true)).collect();
        let mut res = Self {
            session,
            found,
            source_name,
            rows,
            selected,
            dest,
            dest_error: None,
            dest_checked: None,
            scanning: false,
            saved_to: None,
            view: Vec::new(),
            dirty: true,
            query: String::new(),
            cat: None,
            origin: Origin::All,
            year: YearFilter::Any,
            years: Vec::new(),
            has_undated: false,
            show_overwritten,
            hide_duplicates: true,
            duplicates,
            sort: (SortCol::Status, true),
            focus: None,
            counts: [0; 7],
            verifier: None,
            verified: 0,
            damaged: 0,
            verified_only: false,
            index: std::collections::HashMap::new(),
            encrypt: false,
            password: String::new(),
            password_again: String::new(),
            repaired: None,
        };
        res.index = res.rows.iter().enumerate().map(|(i, r)| (r.r, i)).collect();
        let mut years: Vec<i32> = res.rows.iter().filter_map(|r| r.modified.map(|m| m.year())).collect();
        years.sort_unstable_by(|a, b| b.cmp(a));
        years.dedup();
        res.years = years;
        res.has_undated = res.rows.iter().any(|r| r.modified.is_none());
        res.recompute();
        res
    }

    /// Replaces the results with newer ones of the same scan (files found
    /// since, or the final results), keeping what the user chose: selection,
    /// search, filters, sorting, the file being previewed and the folder.
    pub fn refresh(&mut self, found: Found) {
        let mut new = Results::new(
            self.session.clone(),
            found,
            self.source_name.clone(),
            self.show_overwritten,
            self.dest.clone(),
        );
        let chosen: std::collections::HashMap<RowRef, bool> =
            self.rows.iter().zip(&self.selected).map(|(r, s)| (r.r, *s)).collect();
        for (r, s) in new.rows.iter().zip(new.selected.iter_mut()) {
            if let Some(&c) = chosen.get(&r.r) {
                *s = c;
            }
        }
        new.scanning = self.scanning;
        new.query = std::mem::take(&mut self.query);
        new.cat = self.cat;
        new.origin = self.origin;
        new.year = self.year;
        new.hide_duplicates = self.hide_duplicates;
        new.sort = self.sort;
        new.focus = self.focus;
        new.verified_only = self.verified_only;
        new.encrypt = self.encrypt;
        new.password = std::mem::take(&mut self.password);
        new.password_again = std::mem::take(&mut self.password_again);
        new.dirty = true;
        *self = new;
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
        self.rows.iter().zip(&self.selected).filter(|(r, s)| **s && self.included(r)).map(|(r, _)| r.r).collect()
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
            .filter(|(r, s)| **s && self.included(r))
            .fold((0, 0), |(n, b), (r, _)| (n + 1, b + r.size))
    }

    /// Whether a row is listed and, when selected, recovered: files that
    /// will not open and duplicates only on request.
    fn included_with(r: &Row, show_overwritten: bool, hide_duplicates: bool) -> bool {
        (show_overwritten || !r.status.hidden_by_default()) && !(hide_duplicates && r.duplicate_of.is_some())
    }

    fn included(&self, r: &Row) -> bool {
        Self::included_with(r, self.show_overwritten, self.hide_duplicates)
    }

    /// Files listed (before search and filters), as counted in the header
    /// and the sidebar.
    pub fn listed(&self) -> usize {
        self.rows.iter().filter(|r| self.included(r)).count()
    }

    fn eligible(&self, r: &Row) -> bool {
        self.included(r)
            && (!self.verified_only || r.verdict == Some(Verdict::Verified))
            && match self.origin {
                Origin::All => true,
                Origin::Named => matches!(r.r, RowRef::Fs(_)),
                Origin::Deep => matches!(r.r, RowRef::Carved(_)),
            }
    }

    fn recompute(&mut self) {
        self.count_verdicts();
        let q = self.query.trim().to_lowercase();
        let mut counts = [0usize; 7];
        let mut view = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            let year_ok = match self.year {
                YearFilter::Any => true,
                YearFilter::Year(y) => r.modified.is_some_and(|m| m.year() == y),
                YearFilter::Undated => r.modified.is_none(),
            };
            if !self.eligible(r)
                || !year_ok
                || !(q.is_empty() || r.name_lower.contains(&q) || r.folder.to_lowercase().contains(&q))
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
                SortCol::Status => {
                    status_rank(x.status, x.verdict).cmp(&status_rank(y.status, y.verdict)).then(y.size.cmp(&x.size))
                }
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
            Some(DestError::Missing)
        } else if allow_same_volume {
            None
        } else {
            wdfr::output::ensure_not_on_source(&self.session.path, std::path::Path::new(self.dest.trim()))
                .err()
                .map(|_| DestError::OnSource)
        };
    }

    /// Starts checking the files once the scan has finished, and takes in
    /// the verdicts reached so far. The order of the list is left alone, so
    /// that it does not move under the mouse.
    fn poll_verifier(&mut self, ctx: &egui::Context) {
        if self.verifier.is_none() && !self.scanning {
            // Files that will not open need no check.
            let refs = self.rows.iter().filter(|r| !r.status.hidden_by_default()).map(|r| r.r).collect();
            self.verifier = Some(Verifier::start(ctx, self.session.clone(), self.found.clone(), refs));
        }
        let Some(v) = &self.verifier else { return };
        let new = v.take_new();
        if new.is_empty() {
            return;
        }
        for (r, verdict) in new {
            if let Some(&i) = self.index.get(&r) {
                self.rows[i].verdict = Some(verdict);
            }
        }
        self.count_verdicts();
        if self.verified_only {
            self.dirty = true;
        }
    }

    /// Counts the verdicts of the files listed (not of hidden duplicates).
    fn count_verdicts(&mut self) {
        let (mut ok, mut bad) = (0, 0);
        for r in self.rows.iter().filter(|r| self.included(r)) {
            match r.verdict {
                Some(Verdict::Verified) => ok += 1,
                Some(Verdict::Damaged) => bad += 1,
                _ => {}
            }
        }
        (self.verified, self.damaged) = (ok, bad);
    }

    /// For the screenshot tour: every file has been checked.
    #[cfg(debug_assertions)]
    pub fn verified_all(&self) -> bool {
        self.verifier.as_ref().is_some_and(Verifier::finished)
    }

    pub fn page(&mut self, ui: &mut Ui, p: &Palette, previewer: &mut Previewer, allow_same_volume: bool) -> Action {
        self.poll_verifier(ui.ctx());
        if self.dirty {
            self.recompute();
        }
        self.check_dest(allow_same_volume);
        let mut action = Action::None;

        // Header
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                let total = self.listed();
                ui.label(theme::semibold(trn(total as u64, "1 file found", "{n} files found"), 26.0).color(p.text));
                let name: &dyn std::fmt::Display = &self.source_name;
                let sub = if self.found.cancelled {
                    trf("on {name} · scan was stopped early", &[("name", name)])
                } else {
                    trf("on {name}", &[("name", name)])
                };
                ui.label(RichText::new(sub).color(p.weak).size(15.0));
                if let Some(v) = &self.verifier {
                    let (done, total) = v.progress();
                    if !v.finished() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            let text = trf(
                                "Checking that files open… {done} of {total}",
                                &[("done", &done), ("total", &total)],
                            );
                            ui.label(RichText::new(text).color(p.weak).size(13.0));
                        });
                    } else if self.verified + self.damaged > 0 {
                        let text = trf(
                            "{verified} verified · {damaged} may be damaged",
                            &[("verified", &self.verified), ("damaged", &self.damaged)],
                        );
                        let color = if self.verified > 0 { p.success } else { p.warning };
                        ui.label(RichText::new(format!("{}  {text}", icon::SEAL_CHECK)).color(color).size(13.0));
                    }
                }
                if let Some(saved) = &self.saved_to {
                    ui.label(RichText::new(icon_label(icon::CHECK_CIRCLE, "Scan saved")).color(p.success).size(13.0))
                        .on_hover_text(saved.display().to_string());
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::secondary_button(ui, &icon_label(icon::ARROW_COUNTER_CLOCKWISE, "New scan")).clicked() {
                    action = Action::NewScan;
                }
                // Saving a scan that is still running would save half of it.
                if !self.scanning
                    && theme::secondary_button(ui, &icon_label(icon::FLOPPY_DISK, "Save scan…"))
                        .on_hover_text(tr("Save these results to open them later without scanning again."))
                        .clicked()
                {
                    action = Action::SaveScan;
                }
            });
        });
        ui.add_space(10.0);

        if self.scanning {
            theme::notice(
                ui,
                p,
                p.accent,
                icon::HOURGLASS_MEDIUM,
                trl(
                    "The scan is still running. You can look at the files found so far; Recover becomes available when it has finished.",
                ),
            );
            ui.add_space(8.0);
        }
        if self.found.erased_by_drive {
            theme::notice(
                ui,
                p,
                p.warning,
                icon::WARNING,
                trl(
                    "This drive erases deleted files by itself (SSD TRIM), so most of them contain only zeros and cannot be recovered. Files found by their content are not affected.",
                ),
            );
            ui.add_space(8.0);
        }

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
        // The preview gets narrower, then hides, so that the table stays usable
        // in small windows.
        let preview_w = match ui.available_width() {
            w if w >= 940.0 => Some(300.0),
            w if w >= 640.0 => Some(240.0),
            _ => None,
        };
        if let Some(w) = preview_w {
            egui::Panel::right("results-preview")
                .exact_size(w)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 0, top: 0, bottom: 0 }))
                .show(ui, |ui| self.preview_panel(ui, p, previewer));
        }
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
                ui.label(theme::semibold(tr("No deleted files were found"), 20.0).color(p.text));
                theme::paragraph(
                    ui,
                    trl("If the drive was formatted, or the files were deleted a while ago, try a deep search: it looks for files by their content."),
                    14.5,
                    p.weak,
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    let w = 380.0;
                    ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                    if theme::primary_button(ui, p, &icon_label(icon::MAGNIFYING_GLASS, "Run a deep search"), true).clicked() {
                        *action = Action::DeepScan;
                    }
                    if theme::secondary_button(ui, tr("Choose another drive")).clicked() {
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
                .hint_text(icon_label(icon::MAGNIFYING_GLASS, "Search by name or folder"))
                .desired_width(260.0)
                .margin(Vec2::new(10.0, 7.0));
            if ui.add(search).changed() {
                self.dirty = true;
            }
            let before = self.origin;
            egui::ComboBox::from_id_salt("origin")
                .selected_text(match self.origin {
                    Origin::All => tr("All results"),
                    Origin::Named => tr("With original names"),
                    Origin::Deep => tr("Found by content"),
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.origin, Origin::All, tr("All results"));
                    ui.selectable_value(&mut self.origin, Origin::Named, tr("With original names"));
                    ui.selectable_value(&mut self.origin, Origin::Deep, tr("Found by content"));
                });
            if self.origin != before {
                self.dirty = true;
            }
            if self.duplicates > 0 {
                let label = trf("Hide duplicates ({n})", &[("n", &self.duplicates)]);
                if ui
                    .checkbox(&mut self.hide_duplicates, label)
                    .on_hover_text(tr("Files identical to another one, byte for byte."))
                    .changed()
                {
                    self.dirty = true;
                }
            }
            if self.verified > 0 {
                let label = trf("Only verified files ({n})", &[("n", &self.verified)]);
                if ui
                    .checkbox(&mut self.verified_only, label)
                    .on_hover_text(tr("Files that were checked and are complete."))
                    .changed()
                {
                    self.dirty = true;
                }
            }
            // Date filter: only offered when the results have dates.
            if !self.years.is_empty() {
                let before = self.year;
                let label = |y: YearFilter| match y {
                    YearFilter::Any => tr("Any date").to_string(),
                    YearFilter::Year(y) => y.to_string(),
                    YearFilter::Undated => tr("No date").to_string(),
                };
                egui::ComboBox::from_id_salt("year").selected_text(label(self.year)).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.year, YearFilter::Any, label(YearFilter::Any));
                    for y in self.years.clone() {
                        ui.selectable_value(&mut self.year, YearFilter::Year(y), label(YearFilter::Year(y)));
                    }
                    if self.has_undated {
                        ui.selectable_value(&mut self.year, YearFilter::Undated, label(YearFilter::Undated));
                    }
                });
                if self.year != before {
                    self.dirty = true;
                }
            }
            ui.add_space(6.0);
            let _ = p;
            let all: usize = self.counts.iter().sum();
            if ui.selectable_label(self.cat.is_none(), trf("All  {n}", &[("n", &all)])).clicked() {
                self.cat = None;
                self.dirty = true;
            }
            for slot in 0..7 {
                let n = self.counts[slot];
                if n == 0 && self.cat != Some(slot) {
                    continue;
                }
                let c = Category::ALL.get(slot).copied();
                let text = visual(&format!("{} {}  {n}", category_icon(c), category_label(slot))).into_owned();
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

        // The location and date columns only appear when there is room for them.
        let show_location = ui.available_width() > 860.0;
        let show_modified = ui.available_width() > 560.0;
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(30.0))
            .column(Column::remainder().at_least(150.0).clip(true))
            .column(Column::exact(84.0));
        if show_modified {
            table = table.column(Column::exact(128.0));
        }
        table = table.column(Column::exact(146.0));
        if show_location {
            table = table.column(Column::exact(200.0).clip(true));
        }
        table
            .header(30.0, |mut h| {
                h.col(|ui| {
                    let mut all = !view.is_empty() && view.iter().all(|&i| selected[i]);
                    if ui.checkbox(&mut all, "").on_hover_text(tr("Select all shown")).changed() {
                        for &i in view {
                            selected[i] = all;
                        }
                    }
                });
                h.col(|ui| header(ui, tr("Name"), SortCol::Name, &mut sort_click));
                h.col(|ui| header(ui, tr("Size"), SortCol::Size, &mut sort_click));
                if show_modified {
                    h.col(|ui| header(ui, tr("Modified"), SortCol::Modified, &mut sort_click));
                }
                h.col(|ui| header(ui, tr("Status"), SortCol::Status, &mut sort_click));
                if show_location {
                    h.col(|ui| header(ui, tr("Location"), SortCol::Folder, &mut sort_click));
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
                    if show_modified {
                        row.col(|ui| {
                            let t = r
                                .modified
                                .map(|m| m.format("%Y-%m-%d %H:%M").to_string())
                                .unwrap_or_else(|| "—".into());
                            ui.label(RichText::new(t).color(p.weak));
                        });
                    }
                    row.col(|ui| status_pill(ui, p, r.status, r.verdict));
                    if show_location {
                        row.col(|ui| {
                            let t = if matches!(r.r, RowRef::Carved(_)) {
                                tr("Deep search").to_string()
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
                    ui.label(RichText::new(tr("Click a file to preview it")).color(p.weak));
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
            status_pill(ui, p, row.status, row.verdict);
            ui.add_space(6.0);
            let explain = match row.status {
                Status::Good => trl("The space this file used has not been reused. It should open normally."),
                Status::Partial(_) => trl("Part of this file's space was reused by other files. It may be damaged."),
                Status::Overwritten => {
                    trl("Other files have been written over this one. It will most likely not open.")
                }
                Status::Erased => trl(
                    "The drive has erased this file's data itself (SSD TRIM). Only zeros are left, so it cannot be recovered.",
                ),
                Status::Found if row.named_from_metadata => trl(
                    "Found by its content in free space. Its name comes from information inside the file (date, camera, title).",
                ),
                Status::Found => trl("Found by its content in free space. The original name is not known."),
            };
            theme::paragraph(ui, explain, 12.5, p.weak);
            let checked = match row.verdict {
                _ if row.status.hidden_by_default() => None,
                Some(Verdict::Verified) => Some(trl("Checked: the file is complete, so it should open.")),
                Some(Verdict::Damaged) => {
                    Some(trl("Checked: the file is incomplete or broken. It may not open, or only in part."))
                }
                _ => None,
            };
            if let Some(text) = checked {
                ui.add_space(4.0);
                theme::paragraph(ui, text, 12.5, p.weak);
            }
            ui.add_space(8.0);
            egui::Grid::new("details").striped(false).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                let mut kv = |k: &str, v: String| {
                    ui.label(RichText::new(k).color(p.weak).size(12.5));
                    ui.add(egui::Label::new(RichText::new(v).color(p.text).size(12.5)).truncate());
                    ui.end_row();
                };
                kv(tr("Size"), format_size(row.size));
                if let Some([w, h]) = dims {
                    kv(tr("Dimensions"), format!("{w} × {h}"));
                }
                kv(
                    tr("Modified"),
                    row.modified
                        .map(|m| m.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| tr("Unknown").into()),
                );
                kv(tr("Location"), if row.folder.is_empty() { "—".into() } else { row.folder.clone() });
                if let Some(o) = row.offset {
                    kv(tr("Disk offset"), format!("{o:#x}"));
                }
                if let Some(n) = &row.note {
                    kv(tr("Notes"), n.clone());
                }
                if let Some(d) = &row.duplicate_of {
                    kv(tr("Duplicate of"), d.clone());
                }
            });
            // Damaged photos can often be repaired.
            let row = &self.rows[i];
            let damaged = row.verdict == Some(Verdict::Damaged) || matches!(row.status, Status::Partial(_));
            if matches!(row.ext.as_str(), "jpg" | "jpeg") && damaged {
                ui.add_space(8.0);
                if ui
                    .button(icon_label(icon::BANDAIDS, "Repair this photo…"))
                    .on_hover_text(tr("Closes a photo whose end is missing, or gives it the header of a good photo taken with the same camera."))
                    .clicked()
                {
                    match self.repair(r) {
                        Some(Ok(())) => self.repaired = Some((r, String::new(), true)),
                        Some(Err(e)) => self.repaired = Some((r, e, false)),
                        None => {} // cancelled
                    }
                }
                if let Some((at, msg, ok)) = &self.repaired
                    && *at == r
                {
                    let text = if *ok { tr("The repaired photo was saved.") } else { msg.as_str() };
                    ui.label(RichText::new(text).color(if *ok { p.success } else { p.danger }).size(12.5));
                }
            }
            ui.add_space(8.0);
            let sel = self.selected[i];
            let label = if sel {
                icon_label(icon::CHECK_SQUARE, "Selected")
            } else {
                icon_label(icon::SQUARE, "Select this file")
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
            // In narrow windows the destination moves to a second row, and the
            // Recover button too if it does not fit next to the selection.
            let one_row = ui.available_width() >= 880.0;
            let mut recover_in_first_row = false;
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(theme::semibold(trf("{n} selected", &[("n", &n)]), 16.0).color(p.text));
                    ui.label(RichText::new(format_size(bytes)).color(p.weak));
                });
                ui.add_space(6.0);
                if ui.small_button(tr("Select all")).clicked() {
                    for (r, s) in self.rows.iter().zip(self.selected.iter_mut()) {
                        *s = Self::included_with(r, self.show_overwritten, self.hide_duplicates);
                    }
                }
                if ui.small_button(tr("Select none")).clicked() {
                    self.selected.iter_mut().for_each(|s| *s = false);
                }
                if one_row {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        go = self.recover_button(ui, p, n);
                        self.destination(ui, p);
                    });
                } else if ui.available_width() >= recover_button_width(ui, n) {
                    recover_in_first_row = true;
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| go = self.recover_button(ui, p, n));
                }
            });
            if !one_row {
                ui.add_space(8.0);
                ui.horizontal(|ui| self.destination_row(ui, p));
                if !recover_in_first_row {
                    ui.add_space(4.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| go = self.recover_button(ui, p, n));
                }
            }
            if let Some(e) = &self.dest_error {
                ui.add_space(4.0);
                ui.label(RichText::new(e.message()).color(p.danger));
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.encrypt, icon_label(icon::LOCK, "Protect with a password"))
                    .on_hover_text(tr("Saves everything into one ZIP file encrypted with AES-256, instead of a folder. It opens with the password in 7-Zip, WinRAR or Keka."));
                if self.encrypt {
                    ui.add(password_field(&mut self.password, tr("Password")));
                    ui.add(password_field(&mut self.password_again, tr("Repeat the password")));
                    if !self.password_again.is_empty() && self.password != self.password_again {
                        ui.label(RichText::new(tr("The passwords are not the same.")).color(p.danger));
                    }
                }
            });
        });
        go
    }

    /// Repairs the photo `r` and saves the result where the user chooses.
    /// `None` if the user cancelled.
    fn repair(&self, r: RowRef) -> Option<Result<(), String>> {
        let item = r.item(&self.found);
        let Ok(Some(data)) = recover::read_item(&self.session, item, 256 << 20) else {
            return Some(Err(trl("The photo could not be read.").into()));
        };
        let mut fixed = wdfr::repair::repair_jpeg(&data, None);
        if fixed.is_none() && !data.starts_with(&[0xFF, 0xD8]) {
            // The header is gone: one from the same camera is needed.
            let path = rfd::FileDialog::new()
                .set_title(tr("Choose a good photo taken with the same camera"))
                .add_filter("JPEG", &["jpg", "jpeg", "JPG", "JPEG"])
                .pick_file()?;
            let Ok(reference) = std::fs::read(&path) else {
                return Some(Err(trl("The photo could not be read.").into()));
            };
            fixed = wdfr::repair::repair_jpeg(&data, Some(&reference));
        }
        let Some(fixed) = fixed.filter(|f| image::load_from_memory(&f.bytes).is_ok()) else {
            return Some(Err(trl("This photo could not be repaired.").into()));
        };
        let name = item.name();
        let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
        let out = rfd::FileDialog::new()
            .set_title(tr("Save the repaired photo"))
            .set_file_name(format!("{stem} (repaired).jpg"))
            .save_file()?;
        Some(
            std::fs::write(&out, &fixed.bytes)
                .map_err(|e| format!("{}: {e}", trl("The repaired photo could not be saved."))),
        )
    }

    /// The password to protect the saved files with, if one was chosen and
    /// typed the same twice.
    pub fn password(&self) -> Option<String> {
        (self.encrypt && !self.password.is_empty() && self.password == self.password_again)
            .then(|| self.password.clone())
    }

    /// Returns true when clicked (see `recover_button_width`).
    fn recover_button(&self, ui: &mut Ui, p: &Palette, n: usize) -> bool {
        let ok = n > 0 && self.dest_error.is_none() && !self.scanning && (!self.encrypt || self.password().is_some());
        let text = format!("{}  {}", icon::DOWNLOAD_SIMPLE, trn(n as u64, "Recover 1 file", "Recover {n} files"));
        theme::primary_button(ui, p, &text, ok).clicked()
    }

    /// "Save to [folder] Browse", laid out right to left (next to the
    /// Recover button in wide windows).
    fn destination(&mut self, ui: &mut Ui, p: &Palette) {
        if ui.button(icon_label(icon::FOLDER_OPEN, "Browse")).clicked()
            && let Some(dir) = rfd::FileDialog::new().pick_folder()
        {
            self.dest = dir.display().to_string();
        }
        // Leave room for the label, whose length depends on the language.
        let label = RichText::new(tr("Save to")).color(p.weak);
        let label_w = egui::WidgetText::from(label.clone())
            .into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body)
            .size()
            .x;
        let spacing = ui.spacing().item_spacing.x;
        ui.add(
            egui::TextEdit::singleline(&mut self.dest)
                .desired_width((ui.available_width() - label_w - 3.0 * spacing - 20.0).clamp(100.0, 420.0))
                .margin(Vec2::new(10.0, 7.0)),
        );
        ui.label(label);
    }

    /// "Save to [folder……] Browse" across the whole width (narrow windows).
    fn destination_row(&mut self, ui: &mut Ui, p: &Palette) {
        ui.label(RichText::new(tr("Save to")).color(p.weak));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button(icon_label(icon::FOLDER_OPEN, "Browse")).clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                self.dest = dir.display().to_string();
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.dest)
                    .desired_width(ui.available_width())
                    .margin(Vec2::new(10.0, 7.0)),
            );
        });
    }
}

fn password_field<'a>(text: &'a mut String, hint: &str) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(text).password(true).hint_text(hint.to_string()).desired_width(170.0)
}

/// The width of the Recover button, which depends on the language.
fn recover_button_width(ui: &Ui, n: usize) -> f32 {
    let text = format!("{}  {}", icon::DOWNLOAD_SIMPLE, trn(n as u64, "Recover 1 file", "Recover {n} files"));
    let galley = egui::WidgetText::from(theme::semibold(text, 15.0)).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Button,
    );
    galley.size().x + 2.0 * ui.spacing().button_padding.x + ui.spacing().item_spacing.x + 8.0
}

/// Text colour for the found-by-category counters.
pub fn category_color(p: &Palette, slot: usize) -> Color32 {
    [p.accent, p.deep, p.success, p.warning, p.weak, p.weak, p.weak][slot.min(6)]
}
