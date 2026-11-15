#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod lang;

mod app;
mod git;
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
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Salvage")
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([1000.0, 640.0])
            .with_decorations(false)
            .with_icon(paint::app_icon()),
        centered: true,
        ..Default::default()
    };
    eframe::run_native("Salvage", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
