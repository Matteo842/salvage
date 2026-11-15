#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod lang;

mod app;
mod extract;
mod git;
mod gitsetup;
mod headless;
mod icon;
mod model;
mod paint;
mod releases;
mod term;
mod theme;
mod widgets;

use eframe::egui;

fn main() -> eframe::Result {
    if std::env::args().any(|a| a == "--sync") {
        headless::run();
        return Ok(());
    }
    // Packaging aid: the Linux tarball ships this PNG for its .desktop entry.
    if let Some(path) = std::env::args().skip_while(|a| a != "--write-icon").nth(1) {
        let n = 256;
        if let Err(e) = image::save_buffer(&path, &icon::icon_rgba(n), n as u32, n as u32, image::ColorType::Rgba8) {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Salvage")
            // Matches salvage.desktop, so Linux docks show the right icon.
            .with_app_id("salvage")
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([1000.0, 640.0])
            .with_decorations(false)
            .with_icon(paint::app_icon()),
        centered: true,
        ..Default::default()
    };
    eframe::run_native("Salvage", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
