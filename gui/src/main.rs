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
mod raid_dialog;
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

/// The built-in graphics chip rather than waking a separate one (laptops with
/// two often fail or start slowly), and on Windows DirectX 12 rather than
/// Vulkan, whose Intel and AMD drivers are the least reliable (Intel's crashed
/// the whole app at start, which no fallback can catch).
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup};
    use eframe::wgpu;
    let mut config = WgpuConfiguration::default();
    if let WgpuSetup::CreateNew(setup) = &mut config.wgpu_setup {
        setup.power_preference = wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::LowPower);
        if cfg!(windows) && wgpu::Backends::from_env().is_none() {
            setup.instance_descriptor.backends = wgpu::Backends::DX12;
        }
    }
    config
}

/// Where the app notes that this computer needs OpenGL.
fn glow_marker() -> Option<std::path::PathBuf> {
    Some(logfile::path()?.with_file_name("use-opengl"))
}

/// OpenGL instead of wgpu: after wgpu failed here, or when asked
/// (WDFR_RENDERER=glow or wgpu).
fn use_glow() -> bool {
    match std::env::var("WDFR_RENDERER").as_deref() {
        Ok("glow") => true,
        Ok("wgpu") => false,
        _ => glow_marker().is_some_and(|m| m.exists()),
    }
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
        wgpu_options: wgpu_options(),
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
    let glow = use_glow();
    let result = run(if glow { eframe::Renderer::Glow } else { eframe::Renderer::Wgpu });
    if let Err(e) = &result {
        log::error!("starting with {} failed: {e}", if glow { "OpenGL" } else { "wgpu" });
    }
    if result.is_err()
        && !glow
        && let Some(marker) = glow_marker()
    {
        // The graphics driver refused wgpu: start again with OpenGL, and keep
        // using it (a second window loop is not allowed in one process).
        let _ = std::fs::write(marker, "wgpu could not start\n");
        if let Ok(exe) = std::env::current_exe()
            && std::process::Command::new(exe).args(std::env::args_os().skip(1)).spawn().is_ok()
        {
            return Ok(());
        }
    }
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
