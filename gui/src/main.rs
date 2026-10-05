//! Desktop app for Windows Deleted Files Recovery — Powered by Bashar Salmo.

// No console window behind the app in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod home;
mod jobs;
mod preview;
mod results;
mod settings;
mod theme;
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
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Windows Deleted Files Recovery")
            .with_app_id("wdfr")
            .with_inner_size([1240.0, 800.0])
            .with_min_inner_size([980.0, 640.0])
            .with_icon(icon()),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native("Windows Deleted Files Recovery", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
