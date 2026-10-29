#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod db;
mod icons;
mod index;
mod scanner;
mod updater;
mod win_utils;

use std::sync::Arc;

use app::BibliothequeApp;
use egui::{IconData, ViewportBuilder};

fn load_embedded_icon() -> Option<Arc<IconData>> {
    const ICON_BYTES: &[u8] = include_bytes!("../bibliotheque.png");
    let image = image::load_from_memory(ICON_BYTES).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(Arc::new(IconData {
        rgba: image.into_raw(),
        width,
        height,
    }))
}

fn main() -> eframe::Result {
    let mut viewport = ViewportBuilder::default()
        .with_title("Bibliotheque d'applications")
        .with_inner_size([1120.0, 690.0])
        .with_min_inner_size([760.0, 460.0]);

    if let Some(icon) = load_embedded_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Bibliotheque d'applications",
        options,
        Box::new(|cc| Ok(Box::new(BibliothequeApp::new(cc)))),
    )
}
