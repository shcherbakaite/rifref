#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod item;
mod loader;
mod pdf;
mod scene;

fn main() -> eframe::Result {
    let startup_file = std::env::args_os().nth(1).map(std::path::PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("RifRef")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([200.0, 150.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "RifRef",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, startup_file)))),
    )
}
