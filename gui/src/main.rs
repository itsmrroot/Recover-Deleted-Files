//! Desktop app for Deleted Files Recovery — Powered by Bashar Salmo.

// No console window behind the app in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod elevate;
mod help;
mod home;
mod i18n;
mod jobs;
mod logfile;
mod menu;
mod preview;
mod results;
mod settings;
mod theme;
mod translations;
mod update;
mod verifier;
mod views;

use eframe::egui;

fn icon() -> egui::IconData {
    image::load_from_memory(include_bytes!("../assets/icon.png"))
        .map(|i| {
            let i = i.to_rgba8();
            egui::IconData { width: i.width(), height: i.height(), rgba: i.into_raw() }
        })
        .unwrap_or_default()
}

fn main() -> eframe::Result {
    // Started as the macOS drive helper: no window at all.
    if let Some(code) = elevate::run_drive_helper() {
        std::process::exit(code);
    }
    let log = logfile::init();
    // macOS would add "Show Tab Bar" to the View menu: one window is enough.
    #[cfg(target_os = "macos")]
    if let Some(mtm) = objc2::MainThreadMarker::new() {
        objc2_app_kit::NSWindow::setAllowsAutomaticWindowTabbing(false, mtm);
    }
    #[allow(unused_mut)]
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Deleted Files Recovery")
        .with_app_id(logfile::APP_ID)
        .with_inner_size(app::DEFAULT_SIZE)
        .with_min_inner_size(app::MIN_SIZE)
        // Windows and Linux: the whole screen, as most programs there open.
        .with_maximized(app::OPEN_MAXIMIZED)
        .with_icon(icon());
    // Development aid: the screenshot tour can use a given window size ("820x560").
    #[allow(unused_mut)]
    let mut persist_window = true;
    #[cfg(debug_assertions)]
    if let Some((w, h)) = std::env::var("WDFR_TOUR_SIZE").ok().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
    }) {
        viewport = viewport.with_inner_size([w, h]).with_min_inner_size([w, h]).with_maximized(false);
        persist_window = false;
    }
    let options = |renderer| eframe::NativeOptions {
        viewport: viewport.clone(),
        persist_window,
        renderer,
        // Opens in the middle of the screen; see `App::fit_to_screen` for small screens.
        centered: true,
        ..Default::default()
    };
    // The name settings are stored under: kept from the app's first name so
    // that nobody loses their settings.
    let run = |renderer| {
        eframe::run_native(
            "Windows Deleted Files Recovery",
            options(renderer),
            Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
        )
    };
    // Some graphics drivers (older Intel ones on Windows in particular)
    // refuse the modern renderer now and then: OpenGL then still works.
    #[allow(unused_mut)]
    let mut first = eframe::Renderer::Wgpu;
    // Development aid: try the fallback renderer.
    #[cfg(debug_assertions)]
    if std::env::var_os("WDFR_GLOW").is_some() {
        first = eframe::Renderer::Glow;
    }
    let result = run(first).or_else(|e| {
        log::error!("starting with wgpu failed: {e}; trying OpenGL");
        run(eframe::Renderer::Glow)
    });
    if let Err(e) = &result {
        log::error!("the window could not be opened: {e}");
        log::logger().flush();
        let mut text = format!("Deleted Files Recovery could not open its window:\n\n{e}");
        if let Some(path) = &log {
            text.push_str(&format!("\n\nDetails are in {}", path.display()));
        }
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Deleted Files Recovery")
            .set_description(text)
            .show();
    }
    result
}
