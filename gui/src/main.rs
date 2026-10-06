//! Desktop app for Windows Deleted Files Recovery — Powered by Bashar Salmo.

// No console window behind the app in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod elevate;
mod home;
mod i18n;
mod jobs;
mod preview;
mod results;
mod settings;
mod theme;
mod translations;
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
    #[allow(unused_mut)]
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Windows Deleted Files Recovery")
        .with_app_id("wdfr")
        .with_inner_size(app::DEFAULT_SIZE)
        .with_min_inner_size(app::MIN_SIZE)
        .with_icon(icon());
    // Development aid: the screenshot tour can use a given window size ("820x560").
    #[allow(unused_mut)]
    let mut persist_window = true;
    #[cfg(debug_assertions)]
    if let Some((w, h)) = std::env::var("WDFR_TOUR_SIZE").ok().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
    }) {
        viewport = viewport.with_inner_size([w, h]).with_min_inner_size([w, h]);
        persist_window = false;
    }
    let options = eframe::NativeOptions {
        viewport,
        persist_window,
        renderer: eframe::Renderer::Wgpu,
        // Opens in the middle of the screen; see `App::fit_to_screen` for small screens.
        centered: true,
        ..Default::default()
    };
    eframe::run_native("Windows Deleted Files Recovery", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
