//! Application state, navigation and the glue between screens and jobs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Align, Color32, CornerRadius, Frame, Layout, Margin, RichText, Sense, Stroke, Ui, Vec2};
use egui_phosphor::regular as icon;
use wdfr::filter::Filter;
use wdfr::recover::{self, Found, Method, SaveOptions, ScanOptions, Session, Summary};

use crate::elevate;
use crate::home::{self, Home};
use crate::i18n::{self, tr, trf, trl};
use crate::jobs::Job;
use crate::preview::Previewer;
use crate::results::{self, Results};
use crate::settings::{self, Settings};
use crate::theme::{self, Palette};
use crate::update::Updater;
use crate::views::{self, DoneAction, POWERED_BY};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Home,
    Scanning,
    Results,
    Saving,
    Done,
    Settings,
    About,
}

pub struct App {
    settings: Settings,
    applied: Option<(theme::Accent, f32, settings::ThemeChoice)>,
    language: Option<i18n::Lang>,
    page: Page,
    home: Home,
    scan: Option<(ScanJob, String)>,
    results: Option<Results>,
    save: Option<(Job<Summary>, PathBuf)>,
    done: Option<(Summary, PathBuf)>,
    previewer: Previewer,
    logo: egui::TextureHandle,
    error: Option<String>,
    restart: Option<elevate::Restart>,
    /// Whether the window has been checked against the screen size.
    fitted: bool,
    /// When the results of a running scan were last refreshed.
    live_refreshed: Option<std::time::Instant>,
    /// The "scan" job is reopening a saved scan.
    opening_saved: bool,
    updater: Updater,
    #[cfg(debug_assertions)]
    tour: Option<tour::Tour>,
}

const SETTINGS_KEY: &str = "settings";

/// The window size on first start, in points.
pub const DEFAULT_SIZE: [f32; 2] = [1240.0, 800.0];
/// The smallest window in which every screen still works, e.g. on a
/// 1024 × 768 screen or in a small virtual machine window.
pub const MIN_SIZE: [f32; 2] = [800.0, 540.0];

/// A running scan: produces the opened source and what was found on it.
type ScanJob = Job<(Arc<Session>, Found)>;

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // After a restart with administrator rights, continue with the
        // settings of the instance that asked for it.
        let handed_over = std::env::var(elevate::SETTINGS_ENV).ok().and_then(|j| serde_json::from_str(&j).ok());
        let settings: Settings =
            handed_over.or_else(|| cc.storage.and_then(|s| eframe::get_value(s, SETTINGS_KEY))).unwrap_or_default();
        elevate::announce_ready();
        let lang = i18n::set_language(settings.language);
        theme::install_fonts(&cc.egui_ctx, lang);
        let logo = {
            let img = image::load_from_memory(include_bytes!("../assets/icon.png")).map(|i| i.to_rgba8());
            let color = match img {
                Ok(i) => {
                    egui::ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw())
                }
                Err(_) => egui::ColorImage::filled([1, 1], Color32::TRANSPARENT),
            };
            cc.egui_ctx.load_texture("logo", color, egui::TextureOptions::LINEAR)
        };
        #[cfg(debug_assertions)]
        let tour = tour::Tour::from_env();
        #[cfg(debug_assertions)]
        let touring = tour.is_some();
        #[cfg(not(debug_assertions))]
        let touring = false;
        let mut updater = Updater::default();
        if settings.check_updates && !touring {
            updater.check(&cc.egui_ctx, false);
        }
        Self {
            home: Home::new(&cc.egui_ctx, &settings),
            settings,
            applied: None,
            language: Some(lang),
            page: Page::Home,
            scan: None,
            results: None,
            save: None,
            done: None,
            previewer: Previewer::default(),
            logo,
            error: None,
            restart: None,
            fitted: false,
            live_refreshed: None,
            opening_saved: false,
            updater,
            #[cfg(debug_assertions)]
            tour,
        }
    }

    fn apply_settings(&mut self, ctx: &egui::Context) {
        let want = (self.settings.accent, self.settings.ui_scale, self.settings.theme);
        if self.applied != Some(want) {
            let midnight = self.settings.theme == settings::ThemeChoice::Midnight;
            theme::apply_style(ctx, self.settings.accent, midnight);
            ctx.set_theme(self.settings.theme.preference());
            ctx.set_zoom_factor(self.settings.ui_scale);
            self.applied = Some(want);
        }
        let lang = i18n::set_language(self.settings.language);
        if self.language != Some(lang) {
            theme::install_fonts(ctx, lang);
            // Drive descriptions ("2 partitions: …") are made in the language
            // of the moment.
            if self.language.is_some() {
                self.home.refresh(ctx);
            }
            self.language = Some(lang);
        }
        if let Some(r) = &mut self.results {
            r.set_show_overwritten(self.settings.show_overwritten);
        }
    }

    fn busy(&self) -> bool {
        self.scan.is_some() || self.save.is_some()
    }

    // ------------------------------------------------------------------
    // Jobs

    fn start_scan(&mut self, ctx: &egui::Context, method_override: Option<Method>) {
        let Some(source) = self.home.selected.clone() else { return };
        let s = &self.settings;
        let method = method_override.unwrap_or(self.home.method);
        let filter = match Filter::new(&[], &self.home.categories, None, s.min_size_kb * 1024, None) {
            Ok(f) => f,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                return;
            }
        };
        let opts = ScanOptions {
            method,
            partition: None,
            filter,
            carve_all_space: s.carve_all_space,
            step: if s.byte_level { 1 } else { 512 },
            max_carve_size: (s.max_carve_mb > 0).then_some(s.max_carve_mb << 20),
        };
        let name = self.home.source_name();
        let job = Job::spawn(ctx, move |progress, cancel| {
            let session = Arc::new(Session::open(&source)?);
            progress.set_session(session.clone());
            let found = recover::scan(&session, &opts, progress, cancel)?;
            Ok((session, found))
        });
        self.previewer.clear();
        self.results = None;
        self.opening_saved = false;
        self.scan = Some((job, name));
        self.page = Page::Scanning;
    }

    fn start_save(&mut self, ctx: &egui::Context) {
        let Some(res) = &self.results else { return };
        let refs = res.selected_refs();
        let out = PathBuf::from(res.dest.trim());
        let (session, found) = (res.session.clone(), res.found.clone());
        let s = &self.settings;
        let opts = SaveOptions {
            out: out.clone(),
            layout: s.layout,
            restore_dates: s.restore_dates,
            write_report: s.write_report,
            allow_same_volume: s.allow_same_volume,
        };
        let job = Job::spawn(ctx, move |progress, cancel| {
            let items: Vec<_> = refs.iter().map(|r| r.item(&found)).collect();
            recover::save(&session, &items, &opts, progress, cancel)
        });
        self.save = Some((job, out));
        self.page = Page::Saving;
    }

    /// Shrinks and centres the window when it does not fit the screen: the
    /// first-start size (or one saved on a bigger screen) is taller than,
    /// for example, a 1366 × 768 laptop screen or a small VM window.
    fn fit_to_screen(&mut self, ctx: &egui::Context) {
        if self.fitted {
            return;
        }
        let (monitor, inner) = ctx.input(|i| (i.viewport().monitor_size, i.viewport().inner_rect));
        let (Some(monitor), Some(inner)) = (monitor, inner) else { return };
        self.fitted = true;
        // Leave room for the task bar or menu bar and the title bar.
        let max = egui::vec2(monitor.x * 0.94, monitor.y * 0.88);
        let size = inner.size();
        if size.x <= max.x && size.y <= max.y {
            return;
        }
        let min = egui::vec2(MIN_SIZE[0], MIN_SIZE[1]).min(max);
        let fitted = size.min(max).max(min);
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(min));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(fitted));
        let corner = ((monitor - fitted) / 2.0).max(egui::Vec2::ZERO);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(corner.to_pos2()));
    }

    fn start_restart(&mut self) {
        let mut settings = self.settings.clone();
        settings.method = self.home.method;
        settings.categories = self.home.categories.clone();
        let json = serde_json::to_string(&settings).unwrap_or_default();
        match elevate::restart(&json) {
            Ok(r) => {
                self.restart = Some(r);
                self.home.restarting = true;
            }
            Err(e) => {
                let msg = if cfg!(target_os = "macos") {
                    trl("Access to drives could not be requested.")
                } else {
                    trl("The app could not be restarted with administrator rights.")
                };
                self.error = Some(format!("{msg}\n\n{e}"))
            }
        }
    }

    fn poll_restart(&mut self, ctx: &egui::Context) {
        let Some(r) = &mut self.restart else { return };
        match r.poll() {
            elevate::Status::Waiting => ctx.request_repaint_after(Duration::from_millis(200)),
            elevate::Status::Granted => {
                self.restart = None;
                self.home.restarting = false;
                self.home.refresh(ctx);
            }
            elevate::Status::Started => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            elevate::Status::Failed => {
                self.restart = None;
                self.home.restarting = false;
            }
        }
    }

    /// Writes the current results to a file chosen by the user.
    fn save_scan(&mut self) {
        let Some(res) = &mut self.results else { return };
        let stem: String =
            res.source_name.chars().map(|c| if c.is_alphanumeric() || c == ' ' { c } else { '_' }).collect();
        let name =
            format!("{} {}.{}", stem.trim(), chrono::Local::now().format("%Y-%m-%d %H%M"), wdfr::saved::EXTENSION);
        let Some(path) = rfd::FileDialog::new()
            .add_filter(trl("Saved scans"), &[wdfr::saved::EXTENSION])
            .set_file_name(name)
            .save_file()
        else {
            return;
        };
        let path = if path.extension().is_some_and(|e| e == wdfr::saved::EXTENSION) {
            path
        } else {
            path.with_extension(wdfr::saved::EXTENSION)
        };
        match wdfr::saved::save(&path, &res.session, &res.found) {
            Ok(()) => res.saved_to = Some(path),
            Err(e) => self.error = Some(format!("{}\n\n{e:#}", trl("The scan could not be saved."))),
        }
    }

    /// Reopens results saved earlier: the drive or image they belong to is
    /// opened and checked, then the results are shown without scanning.
    fn open_saved_scan(&mut self, ctx: &egui::Context, path: PathBuf) {
        let name = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let job = Job::spawn(ctx, move |progress, _cancel| {
            wdfr::progress::Progress::begin(progress, "Opening saved scan", 0, wdfr::progress::Unit::Items);
            let saved = wdfr::saved::load(&path)?;
            let session = Arc::new(Session::open(&saved.source)?);
            let found = saved.into_found(&session)?;
            Ok((session, found))
        });
        self.previewer.clear();
        self.results = None;
        self.opening_saved = true;
        self.scan = Some((job, name));
        self.page = Page::Scanning;
    }

    /// Opens the results of the running scan (refreshed as it finds more).
    fn show_live_results(&mut self) {
        let Some((job, name)) = &self.scan else { return };
        let Some((session, found)) = job.progress.live_snapshot() else { return };
        if self.results.as_ref().is_none_or(|r| !r.scanning) {
            let dest = self.suggest_destination(&session.path);
            let mut res =
                Results::new(session, found, name.clone(), self.settings.show_overwritten, dest.display().to_string());
            res.scanning = true;
            self.results = Some(res);
        }
        self.live_refreshed = Some(std::time::Instant::now());
        self.page = Page::Results;
    }

    /// Adds what the running scan found since the last refresh.
    fn refresh_live_results(&mut self) {
        let (Some((job, _)), Some(res)) = (&self.scan, &mut self.results) else { return };
        if !res.scanning || self.live_refreshed.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            return;
        }
        self.live_refreshed = Some(std::time::Instant::now());
        if job.progress.live_count() != res.rows.len()
            && let Some((_, found)) = job.progress.live_snapshot()
        {
            res.refresh(found);
        }
    }

    fn poll_jobs(&mut self) {
        self.home.poll();
        if let Some((job, name)) = &self.scan
            && let Some(result) = job.poll()
        {
            let name = name.clone();
            self.scan = None;
            match result {
                Ok((session, found)) => {
                    // Results shown while scanning become the final ones.
                    match &mut self.results {
                        Some(res) if res.scanning => {
                            res.scanning = false;
                            res.refresh(found);
                        }
                        _ => {
                            // A reopened scan is named after its drive or image.
                            let name = if self.opening_saved {
                                Path::new(&session.path)
                                    .file_name()
                                    .map_or(session.path.clone(), |n| n.to_string_lossy().into_owned())
                            } else {
                                name
                            };
                            let dest = self.suggest_destination(&session.path);
                            self.results = Some(Results::new(
                                session,
                                found,
                                name,
                                self.settings.show_overwritten,
                                dest.display().to_string(),
                            ));
                        }
                    }
                    self.page = Page::Results;
                }
                Err(e) => {
                    let what = if self.opening_saved {
                        trl("The saved scan could not be opened.")
                    } else {
                        trl("The scan could not be completed.")
                    };
                    self.error = Some(format!("{what}\n\n{e:#}"));
                    self.page = Page::Home;
                }
            }
        }
        if let Some((job, out)) = &self.save
            && let Some(result) = job.poll()
        {
            let out = out.clone();
            self.save = None;
            match result {
                Ok(sum) => {
                    // When running as root, the files belong to the real user.
                    elevate::give_back(&out);
                    if self.settings.open_folder_when_done && !sum.cancelled && sum.fs_files + sum.carved_files > 0 {
                        open_path(&out);
                    }
                    self.done = Some((sum, out));
                    self.page = Page::Done;
                }
                Err(e) => {
                    self.error = Some(format!("{}\n\n{e:#}", trl("The files could not be saved.")));
                    self.page = Page::Results;
                }
            }
        }
    }

    /// The configured destination, or a new folder on the Desktop — or on
    /// another drive if the Desktop is on the drive being recovered.
    fn suggest_destination(&self, source: &str) -> PathBuf {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H%M").to_string();
        let folder = format!("Recovered Files {stamp}");
        if !self.settings.destination.trim().is_empty() {
            return PathBuf::from(self.settings.destination.trim()).join(folder);
        }
        let home = elevate::user_home()
            .or_else(|| std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from));
        let desktop = home.map(|h| if h.join("Desktop").is_dir() { h.join("Desktop") } else { h }).unwrap_or_default();
        let preferred = desktop.join(&folder);
        if self.settings.allow_same_volume || wdfr::output::ensure_not_on_source(source, &preferred).is_ok() {
            return preferred;
        }
        // Try the other drive letters.
        for d in self.home.drives.iter().flatten() {
            if let Some(letter) = d.device.path.strip_prefix(r"\\.\").and_then(|s| s.strip_suffix(':')) {
                let cand = PathBuf::from(format!(r"{letter}:\")).join(&folder);
                if d.device.size.is_some() && wdfr::output::ensure_not_on_source(source, &cand).is_ok() {
                    return cand;
                }
            }
        }
        preferred
    }

    // ------------------------------------------------------------------
    // Layout

    /// The top bar of the Midnight theme: name on the left, updates on the right.
    fn header(&mut self, ui: &mut Ui, p: &Palette) {
        let white = Color32::from_rgb(245, 247, 252);
        ui.horizontal_centered(|ui| {
            ui.add(egui::Image::new(&self.logo).fit_to_exact_size(Vec2::splat(30.0)));
            ui.add_space(4.0);
            ui.label(theme::semibold(tr("Deleted Files"), 17.0).color(white));
            ui.label(RichText::new(tr("Recovery")).color(white.gamma_multiply(0.75)).size(17.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.updater.available().is_some() {
                    self.updater.button(ui, p);
                } else {
                    ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).color(white.gamma_multiply(0.7)));
                }
            });
        });
    }

    fn sidebar(&mut self, ui: &mut Ui, p: &Palette) {
        if p.header.is_none() {
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                ui.add(egui::Image::new(&self.logo).fit_to_exact_size(Vec2::splat(40.0)));
                ui.vertical(|ui| {
                    ui.add_space(2.0);
                    ui.label(theme::semibold(tr("Deleted Files"), 16.0).color(p.text));
                    ui.label(RichText::new(tr("Recovery")).color(p.weak).size(13.0));
                });
            });
        }
        ui.add_space(22.0);

        let in_flow = matches!(self.page, Page::Home | Page::Scanning | Page::Saving | Page::Done);
        let flow_target = if self.scan.is_some() {
            Page::Scanning
        } else if self.save.is_some() {
            Page::Saving
        } else if self.done.is_some() && self.page == Page::Done {
            Page::Done
        } else {
            Page::Home
        };
        let results_badge = self.results.as_ref().map(Results::listed);
        let items: [(&str, &str, Page, bool, Option<String>); 4] = [
            (icon::MAGNIFYING_GLASS, tr("Recover"), flow_target, in_flow, self.busy().then(|| "●".to_string())),
            (
                icon::LIST_CHECKS,
                tr("Results"),
                Page::Results,
                self.page == Page::Results,
                results_badge.map(|n| n.to_string()),
            ),
            (icon::GEAR_SIX, tr("Settings"), Page::Settings, self.page == Page::Settings, None),
            (icon::INFO, tr("About"), Page::About, self.page == Page::About, None),
        ];
        for (glyph, label, target, active, badge) in items {
            let enabled = target != Page::Results || self.results.is_some();
            if nav_item(ui, p, glyph, label, active, enabled, badge.as_deref()).clicked() {
                if target == Page::Home && self.page == Page::Home {
                    continue;
                }
                self.page = target;
            }
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).color(p.weak).size(12.0));
            });
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(RichText::new(icon::SPARKLE).color(p.accent).size(13.0));
                ui.label(theme::semibold(POWERED_BY, 12.5).color(p.text));
            });
            if p.header.is_none() && self.updater.available().is_some() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    self.updater.button(ui, p);
                });
            }
            ui.add_space(4.0);
            ui.separator();
        });
    }

    fn content(&mut self, ui: &mut Ui, p: &Palette) {
        let ctx = ui.ctx().clone();
        match self.page {
            Page::Home => match self.home.page(ui, p, &self.settings) {
                home::Action::Scan => self.start_scan(&ctx, None),
                home::Action::Elevate => self.start_restart(),
                home::Action::OpenScan(path) => self.open_saved_scan(&ctx, path),
                home::Action::None => {}
            },
            Page::Scanning => {
                if let Some((job, name)) = &self.scan {
                    let st = job.progress.snapshot();
                    let subtitle = trf("Looking for deleted files on {name}", &[("name", name)]);
                    let info = views::ProgressInfo { title: tr("Scanning…"), subtitle: &subtitle, show_found: true };
                    let stop = views::progress(ui, p, &info, &st, job.started.elapsed(), job.stopping());
                    if stop {
                        job.stop();
                    }
                    let n = job.progress.live_count();
                    let mut show_live = false;
                    if n > 0 {
                        ui.add_space(12.0);
                        let label = trf("Show files found so far ({n})", &[("n", &n)]);
                        show_live = theme::secondary_button(ui, &format!("{}  {label}", icon::EYE)).clicked();
                    }
                    if show_live {
                        self.show_live_results();
                    }
                } else {
                    self.page = Page::Home;
                }
            }
            Page::Results => match &mut self.results {
                Some(res) => match res.page(ui, p, &mut self.previewer, self.settings.allow_same_volume) {
                    results::Action::Recover => self.start_save(&ctx),
                    results::Action::NewScan => {
                        // Leaving results of a running scan stops it.
                        if let Some((job, _)) = self.scan.take() {
                            job.stop();
                            self.results = None;
                        }
                        self.page = Page::Home;
                    }
                    results::Action::DeepScan => self.start_scan(&ctx, Some(Method::Carve)),
                    results::Action::SaveScan => self.save_scan(),
                    results::Action::None => {}
                },
                None => self.page = Page::Home,
            },
            Page::Saving => {
                if let Some((job, out)) = &self.save {
                    let st = job.progress.snapshot();
                    let subtitle = trf("Saving to {folder}", &[("folder", &out.display())]);
                    let info =
                        views::ProgressInfo {
                            title: tr("Recovering files…"), subtitle: &subtitle, show_found: false
                        };
                    let stop = views::progress(ui, p, &info, &st, job.started.elapsed(), job.stopping());
                    if stop {
                        job.stop();
                    }
                } else {
                    self.page = Page::Results;
                }
            }
            Page::Done => {
                if let Some((sum, out)) = &self.done {
                    match views::done(ui, p, sum, out) {
                        DoneAction::OpenFolder => open_path(out),
                        DoneAction::OpenReport => {
                            if let Some(r) = &sum.report {
                                open_path(r);
                            }
                        }
                        DoneAction::BackToResults => self.page = Page::Results,
                        DoneAction::NewScan => {
                            self.done = None;
                            self.page = Page::Home;
                        }
                        DoneAction::None => {}
                    }
                } else {
                    self.page = Page::Home;
                }
            }
            Page::Settings => {
                settings::page(ui, p, &mut self.settings);
            }
            Page::About => views::about(ui, p, &self.logo, &mut self.updater),
        }
    }

    fn error_modal(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(msg) = self.error.clone() else { return };
        let modal = egui::Modal::new(egui::Id::new("error")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::WARNING_CIRCLE).size(26.0).color(p.danger));
                ui.label(theme::semibold(tr("Something went wrong"), 18.0).color(p.text));
            });
            ui.add_space(8.0);
            theme::paragraph(ui, &msg, 14.5, p.text);
            if msg.contains("denied") || msg.contains("Administrator") || msg.contains("sudo") {
                ui.add_space(8.0);
                let hint = if cfg!(windows) {
                    trl("Reading a drive needs administrator rights: close the app, right-click it and choose \"Run as administrator\".")
                } else if cfg!(target_os = "macos") {
                    trl("Reading a drive needs administrator rights: click \"Allow access to drives\" on the start screen.")
                } else {
                    trl("Reading a drive needs administrator rights: click \"Restart with administrator rights\" on the start screen, or start the app with sudo.")
                };
                theme::paragraph(ui, hint, 14.5, p.weak);
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| theme::primary_button(ui, p, &format!("  {}  ", tr("OK")), true).clicked())
                .inner
        });
        if modal.inner || modal.should_close() {
            self.error = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.fit_to_screen(&ctx);
        self.apply_settings(&ctx);
        self.poll_jobs();
        self.refresh_live_results();
        self.poll_restart(&ctx);
        if self.previewer.poll(&ctx) {
            ctx.request_repaint();
        }

        // Disk images can be dropped onto the window.
        let dropped = ctx
            .input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).find(|p| !p.as_os_str().is_empty()));
        if let Some(path) = dropped
            && !self.busy()
        {
            self.home.add_image(path.display().to_string());
            self.page = Page::Home;
        }

        if self.updater.poll(&ctx) {
            // The installer takes over and starts the new version.
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        let midnight = self.settings.theme == settings::ThemeChoice::Midnight;
        let p = Palette::new(ui.visuals().dark_mode, midnight, self.settings.accent);
        if let Some(fill) = p.header {
            egui::Panel::top("header")
                .exact_size(56.0)
                .resizable(false)
                .show_separator_line(false)
                .frame(Frame::new().fill(fill).inner_margin(Margin::symmetric(18, 0)))
                .show(ui, |ui| self.header(ui, &p));
        }
        egui::Panel::left("sidebar")
            .exact_size(236.0)
            .resizable(false)
            .frame(
                Frame::new().fill(p.sidebar).stroke(Stroke::new(1.0, p.border)).inner_margin(Margin::symmetric(12, 0)),
            )
            .show(ui, |ui| self.sidebar(ui, &p));
        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 30, right: 30, top: 26, bottom: 22 }))
            .show(ui, |ui| self.content(ui, &p));
        self.error_modal(&ctx, &p);
        let busy = self.busy();
        self.updater.dialog(&ctx, &p, busy);

        #[cfg(debug_assertions)]
        self.run_tour(&ctx);

        if self.busy() || self.previewer.loading() || self.home.drives.is_none() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // The screenshot tour changes settings that are not the user's.
        #[cfg(debug_assertions)]
        if self.tour.is_some() {
            return;
        }
        self.settings.method = self.home.method;
        self.settings.categories = self.home.categories.clone();
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }
}

fn nav_item(
    ui: &mut Ui,
    p: &Palette,
    glyph: &str,
    label: &str,
    active: bool,
    enabled: bool,
    badge: Option<&str>,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
    let resp = if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let hovered = enabled && resp.hovered();
        if active {
            painter.rect_filled(rect, CornerRadius::same(10), p.tint(p.accent));
            painter.rect_filled(
                egui::Rect::from_min_size(rect.min + Vec2::new(0.0, 10.0), Vec2::new(3.0, rect.height() - 20.0)),
                CornerRadius::same(2),
                p.accent,
            );
        } else if hovered {
            painter.rect_filled(rect, CornerRadius::same(10), p.card_alt);
        }
        let color = if !enabled {
            p.weak.gamma_multiply(0.5)
        } else if active {
            p.text
        } else {
            p.weak
        };
        let icon_color = if active { p.accent } else { color };
        let y = rect.center().y;
        painter.text(
            egui::pos2(rect.left() + 16.0, y),
            egui::Align2::LEFT_CENTER,
            glyph,
            egui::FontId::proportional(19.0),
            icon_color,
        );
        painter.text(
            egui::pos2(rect.left() + 46.0, y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::new(15.0, egui::FontFamily::Name(theme::SEMIBOLD.into())),
            color,
        );
        if let Some(b) = badge {
            let c = if b == "●" { p.accent } else { p.weak };
            painter.text(
                egui::pos2(rect.right() - 12.0, y),
                egui::Align2::RIGHT_CENTER,
                b,
                egui::FontId::proportional(12.5),
                c,
            );
        }
    }
    ui.add_space(2.0);
    resp
}

pub fn open_path(path: &Path) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(path).spawn();
}

/// Development aid (debug builds only): with `WDFR_TOUR_DIR` and
/// `WDFR_TOUR_IMAGE` set, the app walks through every screen using the
/// given disk image and saves a screenshot of each, then exits.
/// `WDFR_TOUR_LANG` (`en`, `de`, `ar`, `es`, `fr`, `ru`, `zh`, `tr`) picks the
/// interface language.
#[cfg(debug_assertions)]
mod tour {
    use std::path::PathBuf;

    pub struct Tour {
        pub dir: PathBuf,
        pub image: String,
        pub step: usize,
        pub frames: u32,
        pub waiting_for_shot: Option<&'static str>,
        pub language: Option<crate::i18n::Language>,
    }

    impl Tour {
        pub fn from_env() -> Option<Self> {
            let dir = PathBuf::from(std::env::var_os("WDFR_TOUR_DIR")?);
            let image = std::env::var("WDFR_TOUR_IMAGE").ok()?;
            std::fs::create_dir_all(&dir).ok()?;
            let language = match std::env::var("WDFR_TOUR_LANG").as_deref() {
                Ok("en") => Some(crate::i18n::Language::English),
                Ok("de") => Some(crate::i18n::Language::German),
                Ok("ar") => Some(crate::i18n::Language::Arabic),
                Ok("es") => Some(crate::i18n::Language::Spanish),
                Ok("fr") => Some(crate::i18n::Language::French),
                Ok("ru") => Some(crate::i18n::Language::Russian),
                Ok("zh") => Some(crate::i18n::Language::Chinese),
                Ok("tr") => Some(crate::i18n::Language::Turkish),
                _ => None,
            };
            Some(Self { dir, image, step: 0, frames: 0, waiting_for_shot: None, language })
        }
    }
}

#[cfg(debug_assertions)]
impl App {
    fn run_tour(&mut self, ctx: &egui::Context) {
        let Some(t) = self.tour.as_mut() else { return };
        ctx.request_repaint();
        // A screenshot we asked for has arrived: save it and move on.
        if let Some(name) = t.waiting_for_shot {
            let shot = ctx.input(|i| {
                i.raw.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(img) = shot {
                let path = t.dir.join(format!("{name}.png"));
                let _ = image::save_buffer(
                    &path,
                    img.as_raw(),
                    img.width() as u32,
                    img.height() as u32,
                    image::ColorType::Rgba8,
                );
                t.waiting_for_shot = None;
                t.step += 1;
                t.frames = 0;
            }
            return;
        }
        t.frames += 1;
        // Keep the window at the requested size on every screen.
        if t.frames == 1
            && let Some((w, h)) = std::env::var("WDFR_TOUR_SIZE").ok().and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
            })
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
        }
        let (step, frames, image, dir) = (t.step, t.frames, t.image.clone(), t.dir.clone());
        let shoot = |name: &'static str, ctx: &egui::Context, t: &mut tour::Tour| {
            t.waiting_for_shot = Some(name);
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        };
        // WDFR_TOUR_SAVED: save the results, reopen them, show them.
        if std::env::var_os("WDFR_TOUR_SAVED").is_some() {
            match step {
                0 if frames == 1 => self.home.add_image(image),
                0 if frames == 40 => self.start_scan(ctx, None),
                0 if self.page == Page::Results && self.scan.is_none() => {
                    let file = dir.join("tour.wdfrscan");
                    if let Some(res) = &self.results {
                        wdfr::saved::save(&file, &res.session, &res.found).expect("saving the scan");
                    }
                    self.open_saved_scan(ctx, file);
                    let t = self.tour.as_mut().unwrap();
                    (t.step, t.frames) = (1, 0);
                }
                1 if self.page == Page::Results && self.scan.is_none() && frames > 20 => {
                    shoot("reopened", ctx, self.tour.as_mut().unwrap())
                }
                2 => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                _ => {}
            }
            return;
        }
        // WDFR_TOUR_UPDATE: find an update and install it, as "Update now" does.
        if std::env::var_os("WDFR_TOUR_UPDATE").is_some() {
            match step {
                0 if frames == 1 => self.updater.check(ctx, true),
                0 if self.updater.available().is_some() => {
                    self.updater.update_now(ctx);
                    self.tour.as_mut().unwrap().step = 1;
                }
                1 if frames == 30 => shoot("update-downloading", ctx, self.tour.as_mut().unwrap()),
                _ => {}
            }
            return;
        }
        // WDFR_TOUR_MIDNIGHT: the Midnight theme with an update available
        // (run with WDFR_PRETEND_VERSION set to an older version).
        if std::env::var_os("WDFR_TOUR_MIDNIGHT").is_some() {
            match step {
                0 if frames == 1 => {
                    self.settings.theme = settings::ThemeChoice::Midnight;
                    self.home.add_image(image);
                    self.updater.check(ctx, false);
                }
                0 if frames > 40 && self.home.drives.is_some() && self.updater.available().is_some() => {
                    shoot("midnight-home", ctx, self.tour.as_mut().unwrap())
                }
                1 if frames == 1 => self.start_scan(ctx, None),
                1 if self.page == Page::Results && self.scan.is_none() && frames > 30 => {
                    shoot("midnight-results", ctx, self.tour.as_mut().unwrap())
                }
                2 if frames == 1 => self.page = Page::Settings,
                2 if frames > 10 => shoot("midnight-settings", ctx, self.tour.as_mut().unwrap()),
                3 if frames == 1 => {
                    self.page = Page::About;
                    self.updater.show_dialog();
                }
                3 if frames > 10 => shoot("midnight-update", ctx, self.tour.as_mut().unwrap()),
                4 if frames == 1 => self.settings.theme = settings::ThemeChoice::Dark,
                4 if frames > 10 => shoot("dark-sidebar-update", ctx, self.tour.as_mut().unwrap()),
                5 => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                _ => {}
            }
            return;
        }
        // WDFR_TOUR_LIVE: only the results of a scan that is still running.
        if std::env::var_os("WDFR_TOUR_LIVE").is_some() {
            match step {
                0 if frames == 1 => self.home.add_image(image),
                0 if frames == 40 => {
                    // Byte-level search is slow enough to look at results mid-scan.
                    self.settings.byte_level = true;
                    self.start_scan(ctx, Some(Method::Carve));
                }
                0 if self.scan.as_ref().is_some_and(|(j, _)| j.progress.live_count() > 0) => {
                    self.show_live_results();
                    self.tour.as_mut().unwrap().step = 1;
                    self.tour.as_mut().unwrap().frames = 0;
                }
                1 if frames > 30 => shoot("live-results", ctx, self.tour.as_mut().unwrap()),
                2 if self.scan.is_none() && frames > 5 => shoot("live-final", ctx, self.tour.as_mut().unwrap()),
                3 => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                _ => {}
            }
            return;
        }
        match step {
            0 if frames == 1 => {
                if let Some(l) = self.tour.as_ref().and_then(|t| t.language) {
                    self.settings.language = l;
                }
                // A file manager window would cover the app and stop it drawing.
                self.settings.open_folder_when_done = false;
                self.home.add_image(image);
            }
            0 if frames > 40 && self.home.drives.is_some() => shoot("1-home", ctx, self.tour.as_mut().unwrap()),
            1 if frames == 1 => self.start_scan(ctx, None),
            1 if frames == 12 => shoot("2-scanning", ctx, self.tour.as_mut().unwrap()),
            2 if self.page == Page::Results => {
                if let Some(r) = &mut self.results
                    && !r.has_focus()
                {
                    r.focus_first_image();
                    self.tour.as_mut().unwrap().frames = 1;
                }
                if frames > 60 && !self.previewer.loading() {
                    shoot("3-results", ctx, self.tour.as_mut().unwrap());
                }
            }
            3 if frames == 1 => self.page = Page::Settings,
            3 if frames > 10 => shoot("4-settings", ctx, self.tour.as_mut().unwrap()),
            4 if frames == 1 => self.page = Page::About,
            4 if frames > 10 => shoot("5-about", ctx, self.tour.as_mut().unwrap()),
            5 if frames == 1 => {
                self.settings.theme = settings::ThemeChoice::Light;
                self.page = Page::Results;
            }
            5 if frames > 20 => shoot("6-results-light", ctx, self.tour.as_mut().unwrap()),
            6 if frames == 1 => {
                self.settings.theme = settings::ThemeChoice::Dark;
                if let Some(r) = &mut self.results {
                    r.dest = dir.join("recovered").display().to_string();
                }
            }
            6 if frames == 5 => self.start_save(ctx),
            6 if self.page == Page::Done && frames > 10 => shoot("7-done", ctx, self.tour.as_mut().unwrap()),
            7 => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }
    }
}
